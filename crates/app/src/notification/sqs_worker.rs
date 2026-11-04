//! 푸시 Worker의 SQS 수신·완료 처리. 원본 `SqsPushService`와 `SqsPollingScheduler`의 호출부.
//!
//! - 한 번 수신에 메시지 하나를 20초 롱 폴링·60초 숨김 시간으로 받는다. 수신이 실패하면 로그를 남기고 1초 쉰 뒤 다음 폴링을 한다.
//! - 메시지를 처리하는 동안 40초마다 숨김 시간을 60초로 연장한다(전용 단일 스레드 하나가 모든 메시지의 연장을 맡는다).
//!   처리가 끝나는 순간 연장을 멈추고, 진행 중인 연장 요청이 끝나기를 기다린 뒤 삭제 또는 재시도 지연을 적용한다.
//! - 해석할 수 없는 본문(JSON `null` 포함)은 발송하지 않고 삭제한다. 처리 결과가 재시도가 아니면 삭제하고, 재시도면
//!   `Retry-After + 1`초(사건의 원래 기한 안에서)만큼 다음 전달을 늦춘다. 예상하지 못한 오류는 삭제하지 않아 SQS가 다시 전달한다
//!   (최대 수신 횟수 뒤 DLQ).
//! - 로그에는 `sqsMessageId`·`receiveCount`를 MDC로 붙인다. 숨김 연장 스레드는 MDC를 물려받지 않으므로 필드로 직접 남긴다.
//! - SQS 호출은 수신이 25초, 삭제·숨김 변경이 5초 안에 끝나야 하고 재시도하지 않는다.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chalkak_aws::{ReceiveMessageInput, SqsClient, SqsError};
use chalkak_core::log::Logger;
use chalkak_core::scheduler::ShutdownSignal;
use chalkak_core::{Clock, mdc};
use jiff::Timestamp;
use serde_json::Value;

use super::push_message::{RawPushMessage, parse_push_message};
use super::push_worker::PushProcessor;
use crate::common::Elapsed;

pub(super) const SERVICE_LOGGER: &str =
    "com.chalkak.backend.notification.infrastructure.infra.SqsPushService";
const TYPE_NOTIFICATION: &str = "notification";
const RECEIVE_COUNT_ATTRIBUTE: &str = "ApproximateReceiveCount";
const SENT_TIMESTAMP_ATTRIBUTE: &str = "SentTimestamp";
/// 수신 한 번이 기다리는 시간(`waitTimeSeconds`).
const LONG_POLL_SECONDS: u32 = 20;
/// 수신한 메시지가 다른 소비자에게 숨는 시간(`visibilityTimeout`).
const VISIBILITY_SECONDS: u32 = 60;
/// 처리 중 숨김 시간을 연장하는 주기.
pub const VISIBILITY_EXTENSION_INTERVAL: Duration = Duration::from_secs(40);
/// 수신 오류 뒤 쉬는 시간.
const POLL_FAILURE_WAIT: Duration = Duration::from_secs(1);
/// 수신 호출의 제한 시간(`apiCallAttemptTimeout` 25초).
pub const RECEIVE_TIMEOUT: Duration = Duration::from_secs(25);
/// 삭제·숨김 변경 호출의 제한 시간(요청별 `overrideConfiguration` 5초).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// 수신한 메시지 하나(`software.amazon.awssdk.services.sqs.model.Message`의 쓰는 부분).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueMessage {
    pub message_id: Option<String>,
    pub receipt_handle: Option<String>,
    pub body: String,
    /// `ApproximateReceiveCount` 시스템 속성
    pub receive_count: Option<String>,
    /// `SentTimestamp` 시스템 속성(밀리초 문자열). 없어도 처리에는 영향이 없다.
    pub sent_timestamp: Option<String>,
}

/// 큐 호출 실패. `exception`은 로그의 `errorCode`로 남기는 예외 이름이다(메시지는 요청 내용이 섞일 수 있어 담지 않는다).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueFailure {
    pub exception: String,
}

impl QueueFailure {
    pub fn new(exception: &str) -> Self {
        Self {
            exception: exception.to_owned(),
        }
    }
}

/// Worker가 쓰는 큐 연산. 운영은 [`SqsWorkerQueue`]다.
pub trait WorkerQueue: Send + Sync {
    /// 메시지 한 건을 수신한다(20초 롱 폴링, 60초 숨김). 없으면 `None`.
    fn receive(&self) -> Result<Option<QueueMessage>, QueueFailure>;
    /// 삭제(5초 제한)
    fn delete(&self, receipt_handle: &str) -> Result<(), QueueFailure>;
    /// 숨김 시간 변경(5초 제한)
    fn change_visibility(&self, receipt_handle: &str, seconds: u32) -> Result<(), QueueFailure>;
}

/// SQS 클라이언트로 구현한 큐.
pub struct SqsWorkerQueue {
    receive_client: SqsClient,
    request_client: SqsClient,
    queue_url: String,
}

impl SqsWorkerQueue {
    /// `client`는 호출 제한 시간만 다른 복사본 두 개로 쓴다.
    pub fn new(client: &SqsClient, queue_url: &str) -> Self {
        Self {
            receive_client: client.with_timeout(RECEIVE_TIMEOUT),
            request_client: client.with_timeout(REQUEST_TIMEOUT),
            queue_url: queue_url.to_owned(),
        }
    }
}

/// AWS SDK 예외의 단순 이름. 시간 초과는 `ApiCall(Attempt)TimeoutException`, 응답이 없으면 `SdkClientException`,
/// 서비스 오류는 오류 코드에 맞는 예외(없으면 `SqsException`)다.
fn failure_of(error: &SqsError, attempt_timeout: bool) -> QueueFailure {
    match error {
        SqsError::Client(message) if message.contains("시간 초과") => {
            QueueFailure::new(if attempt_timeout {
                "ApiCallAttemptTimeoutException"
            } else {
                "ApiCallTimeoutException"
            })
        }
        SqsError::Client(_) => QueueFailure::new("SdkClientException"),
        SqsError::Service(service) => {
            let code = service.code.as_deref().unwrap_or("");
            let name = [
                ("NonExistentQueue", "QueueDoesNotExistException"),
                ("QueueDoesNotExist", "QueueDoesNotExistException"),
                ("ReceiptHandleIsInvalid", "ReceiptHandleIsInvalidException"),
                ("MessageNotInflight", "MessageNotInflightException"),
                ("RequestThrottled", "RequestThrottledException"),
                ("OverLimit", "OverLimitException"),
                ("InvalidAddress", "InvalidAddressException"),
            ]
            .iter()
            .find(|(fragment, _)| code.contains(fragment))
            .map_or("SqsException", |(_, name)| name);
            QueueFailure::new(name)
        }
    }
}

impl WorkerQueue for SqsWorkerQueue {
    fn receive(&self) -> Result<Option<QueueMessage>, QueueFailure> {
        let input = ReceiveMessageInput {
            max_number_of_messages: Some(1),
            visibility_timeout: Some(VISIBILITY_SECONDS),
            wait_time_seconds: Some(LONG_POLL_SECONDS),
            message_system_attribute_names: vec![
                RECEIVE_COUNT_ATTRIBUTE.to_owned(),
                SENT_TIMESTAMP_ATTRIBUTE.to_owned(),
            ],
        };
        let messages = self
            .receive_client
            .receive_message(&self.queue_url, &input)
            .map_err(|e| failure_of(&e, true))?;
        Ok(messages.into_iter().next().map(|m| QueueMessage {
            receive_count: m.attribute(RECEIVE_COUNT_ATTRIBUTE).map(str::to_owned),
            sent_timestamp: m.attribute(SENT_TIMESTAMP_ATTRIBUTE).map(str::to_owned),
            message_id: m.message_id,
            receipt_handle: m.receipt_handle,
            body: m.body,
        }))
    }

    fn delete(&self, receipt_handle: &str) -> Result<(), QueueFailure> {
        self.request_client
            .delete_message(&self.queue_url, receipt_handle)
            .map_err(|e| failure_of(&e, false))
    }

    fn change_visibility(&self, receipt_handle: &str, seconds: u32) -> Result<(), QueueFailure> {
        self.request_client
            .change_message_visibility(&self.queue_url, receipt_handle, seconds)
            .map_err(|e| failure_of(&e, false))
    }
}

// ---- 숨김 연장 실행기 ----

/// 예약한 작업의 취소 손잡이.
pub trait HeartbeatHandle: Send + Sync {
    /// 이후 실행을 취소한다(이미 실행 중인 작업은 끝까지 간다: `cancel(false)`).
    fn cancel(&self);
}

/// 고정 주기 작업 실행기(`ScheduledExecutorService.scheduleAtFixedRate`).
pub trait HeartbeatScheduler: Send + Sync {
    fn schedule_at_fixed_rate(
        &self,
        initial_delay: Duration,
        period: Duration,
        task: Arc<dyn Fn() + Send + Sync>,
    ) -> Box<dyn HeartbeatHandle>;
}

struct ScheduledTask {
    next: Instant,
    period: Duration,
    cancelled: Arc<AtomicBool>,
    task: Arc<dyn Fn() + Send + Sync>,
}

struct ExecutorState {
    tasks: BTreeMap<u64, ScheduledTask>,
    closed: bool,
}

struct ExecutorShared {
    state: Mutex<ExecutorState>,
    wake: Condvar,
    next_id: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 전용 단일 스레드 실행기(`Executors.newSingleThreadScheduledExecutor`). 모든 숨김 연장이 이 스레드 하나에서 차례로 돈다.
/// 한 요청이 늦으면 뒤 작업이 기다리므로 호출에는 5초 제한이 있다.
pub struct ThreadHeartbeatScheduler {
    shared: Arc<ExecutorShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ThreadHeartbeatScheduler {
    pub fn start() -> std::io::Result<Self> {
        let shared = Arc::new(ExecutorShared {
            state: Mutex::new(ExecutorState {
                tasks: BTreeMap::new(),
                closed: false,
            }),
            wake: Condvar::new(),
            next_id: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("notification-visibility-extension".to_owned())
            .spawn(move || run_executor(&worker))?;
        Ok(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// 실행기를 멈추고 스레드가 끝나기를 기다린다(`shutdownNow`). 실행 중인 작업은 끝까지 간다.
    pub fn shutdown(&self) {
        {
            let mut state = lock(&self.shared.state);
            state.closed = true;
            state.tasks.clear();
        }
        self.shared.wake.notify_all();
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ThreadHeartbeatScheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ThreadHandle {
    id: u64,
    cancelled: Arc<AtomicBool>,
    shared: Arc<ExecutorShared>,
}

impl HeartbeatHandle for ThreadHandle {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        lock(&self.shared.state).tasks.remove(&self.id);
        self.shared.wake.notify_all();
    }
}

impl HeartbeatScheduler for ThreadHeartbeatScheduler {
    fn schedule_at_fixed_rate(
        &self,
        initial_delay: Duration,
        period: Duration,
        task: Arc<dyn Fn() + Send + Sync>,
    ) -> Box<dyn HeartbeatHandle> {
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock(&self.shared.state);
            if !state.closed {
                state.tasks.insert(
                    id,
                    ScheduledTask {
                        next: Instant::now() + initial_delay,
                        period,
                        cancelled: Arc::clone(&cancelled),
                        task,
                    },
                );
            }
        }
        self.shared.wake.notify_all();
        Box::new(ThreadHandle {
            id,
            cancelled,
            shared: Arc::clone(&self.shared),
        })
    }
}

fn run_executor(shared: &ExecutorShared) {
    let mut state = lock(&shared.state);
    loop {
        if state.closed {
            return;
        }
        let due = state
            .tasks
            .iter()
            .min_by_key(|(_, t)| t.next)
            .map(|(id, t)| (*id, t.next));
        let Some((id, next)) = due else {
            state = shared
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if next > now {
            state = shared
                .wake
                .wait_timeout(state, next - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        let Some(task) = state.tasks.get_mut(&id) else {
            continue;
        };
        // 고정 주기: 다음 실행 시각은 이번 예정 시각에서 주기만큼 뒤다(늦으면 따라잡는다).
        task.next += task.period;
        let run = Arc::clone(&task.task);
        let cancelled = Arc::clone(&task.cancelled);
        drop(state);
        if !cancelled.load(Ordering::SeqCst) {
            // 작업이 패닉하면 그 작업의 이후 실행만 멈춘다(`ScheduledThreadPoolExecutor`가 예외 난 주기 작업을 멈추는 것과 같다).
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run()));
            if outcome.is_err() {
                lock(&shared.state).tasks.remove(&id);
            }
        }
        state = lock(&shared.state);
    }
}

// ---- 서비스 ----

/// `SqsPushService`
pub struct SqsPushService {
    queue: Arc<dyn WorkerQueue>,
    processor: Arc<dyn PushProcessor>,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
    scheduler: Arc<dyn HeartbeatScheduler>,
    shutdown: ShutdownSignal,
}

impl SqsPushService {
    pub fn new(
        queue: Arc<dyn WorkerQueue>,
        processor: Arc<dyn PushProcessor>,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
        scheduler: Arc<dyn HeartbeatScheduler>,
        shutdown: ShutdownSignal,
    ) -> Self {
        Self {
            queue,
            processor,
            clock,
            logger,
            scheduler,
            shutdown,
        }
    }

    /// `poll`: 메시지를 한 건 받아 처리한다. 수신이 실패하면 로그를 남기고 1초 쉰다(종료 신호가 오면 바로 깬다).
    /// 수신에 걸린 시간을 재서 빈 응답·수신 로그·실패 로그에 싣는다.
    pub fn poll(&self) {
        let elapsed = Elapsed::start();
        match self.queue.receive() {
            Ok(Some(message)) => self.process_received(&message, Some(elapsed.to_millis())),
            Ok(None) => {
                self.logger
                    .debug(SERVICE_LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", "worker_sqs_receive")
                    .field("received", false)
                    .field("sqsMessageId", Value::Null)
                    .field("receiveCount", Value::Null)
                    .field("sqsReceiveDurationMs", elapsed.to_millis())
                    .log("SQS 빈 수신 응답");
            }
            Err(failure) => {
                self.logger
                    .error(SERVICE_LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", "worker_poll_failed")
                    .field("phase", "RECEIVE")
                    .field("sqsReceiveDurationMs", elapsed.to_millis())
                    .field("errorCode", failure.exception.as_str())
                    .log("푸시 작업 수신 오류");
                self.shutdown.wait_timeout(POLL_FAILURE_WAIT);
            }
        }
    }

    /// `process(Message)`: 수신 시간을 모르는 채로 처리한다.
    pub fn process(&self, message: &QueueMessage) {
        self.process_received(message, None);
    }

    /// `process(Message, Long)`: 수신한 메시지 하나를 끝까지 처리한다. 어떤 실패도 밖으로 내지 않는다. 처리 시도가 끝나면
    /// 결과(`outcome`)·큐에서 기다린 시간·처리 시간을 한 줄로 남긴다.
    pub fn process_received(&self, message: &QueueMessage, sqs_receive_duration_ms: Option<u64>) {
        let elapsed = Elapsed::start();
        let received_at = self.clock.now();
        let _context = mdc::enter(&[
            ("sqsMessageId", message.message_id.as_deref()),
            ("receiveCount", message.receive_count.as_deref()),
        ]);
        let extension = VisibilityExtension::start(self, message);
        let processed = self.process_message(message, &extension, sqs_receive_duration_ms);
        // 자원 해제는 오류 로그보다 먼저다(`try-with-resources`).
        extension.close();
        let outcome = match processed {
            Ok(outcome) => outcome,
            Err(failure) => {
                // 삭제하지 않는다. SQS가 다시 전달하거나 설정된 최대 수신 횟수 이후 DLQ로 이동한다.
                self.log_processing_failed(
                    &failure.exception,
                    sqs_receive_duration_ms,
                    failure.received_logged,
                );
                "FAILED"
            }
        };
        self.log_attempt_completed(message, received_at, elapsed, outcome);
    }

    fn process_message(
        &self,
        message: &QueueMessage,
        extension: &VisibilityExtension,
        sqs_receive_duration_ms: Option<u64>,
    ) -> Result<&'static str, ProcessingFailure> {
        let after_received = |exception: String| ProcessingFailure {
            exception,
            received_logged: true,
        };
        let push = match parse_push_message(&message.body) {
            Ok(push) => push,
            Err(exception) => {
                self.log_failure("worker_invalid_message", exception);
                None
            }
        };
        self.log_received(push.as_ref(), sqs_receive_duration_ms);
        let Some(push) = push else {
            extension
                .finish_attempt(|| self.delete(message))
                .map_err(after_received)?;
            return Ok("INVALID_MESSAGE_DELETED");
        };
        let result = self.processor.process(&push).map_err(after_received)?;
        extension
            .finish_attempt(|| {
                if !result.retryable {
                    return self.delete(message);
                }
                self.postpone_retry(message, &push, result.retry_after)?;
                self.logger
                    .info(SERVICE_LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", "worker_retry_pending")
                    .field("eventId", push.event_id.map(|id| id.to_string()))
                    .field("userId", push.user_id.map(|id| id.to_string()))
                    .field(
                        "notificationId",
                        push.notification_id.map(|id| id.to_string()),
                    )
                    .field("retryAfterSeconds", result.retry_after.as_secs())
                    .log("푸시 작업 재전달 대기");
                Ok(())
            })
            .map_err(after_received)?;
        Ok(if result.retryable {
            "RETRY_PENDING"
        } else {
            "DELETED"
        })
    }

    /// `postponeRetry`: `Retry-After + 1`초, 사건의 원래 기한을 넘기지 않게 숨김 시간을 늘린다. 지연이 없으면 그대로 둔다.
    fn postpone_retry(
        &self,
        message: &QueueMessage,
        push: &RawPushMessage,
        retry_after: Duration,
    ) -> Result<(), String> {
        if retry_after.is_zero() {
            return Ok(());
        }
        let expires_at = push
            .expires_at
            .ok_or_else(|| "NullPointerException".to_owned())?;
        let remaining = seconds_until(self.clock.now(), expires_at);
        let seconds = remaining.min(retry_after.as_secs() as i64 + 1).max(0);
        self.queue
            .change_visibility(receipt(message)?, seconds as u32)
            .map_err(|f| f.exception)
    }

    fn delete(&self, message: &QueueMessage) -> Result<(), String> {
        let elapsed = Elapsed::start();
        let deleted = receipt(message).and_then(|handle| {
            self.queue
                .delete(handle)
                .map_err(|failure| failure.exception)
        });
        if let Err(exception) = deleted {
            // 기존 `worker_processing_failed`만 ERROR로 남겨 오류 지표를 중복 집계하지 않는다.
            self.logger
                .info(SERVICE_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "worker_sqs_delete_failed")
                .field("sqsDeleteDurationMs", elapsed.to_millis())
                .log("SQS 삭제 요청 실패 시간");
            return Err(exception);
        }
        self.logger
            .info(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_message_deleted")
            .field("sqsDeleteDurationMs", elapsed.to_millis())
            .log("SQS 푸시 작업 완료");
        Ok(())
    }

    fn log_failure(&self, stage: &str, exception: &str) {
        self.logger
            .error(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", stage)
            .field("errorCode", exception)
            .log("푸시 작업 처리 오류");
    }

    fn log_processing_failed(
        &self,
        exception: &str,
        sqs_receive_duration_ms: Option<u64>,
        received_logged: bool,
    ) {
        let mut event = self
            .logger
            .error(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_processing_failed")
            .field("errorCode", exception);
        if !received_logged && let Some(millis) = sqs_receive_duration_ms {
            event = event.field("sqsReceiveDurationMs", millis);
        }
        event.log("푸시 작업 처리 오류");
    }

    fn log_received(&self, push: Option<&RawPushMessage>, sqs_receive_duration_ms: Option<u64>) {
        let mut event = self
            .logger
            .info(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_received")
            .field("received", true)
            .field("messageParsed", push.is_some());
        if let Some(millis) = sqs_receive_duration_ms {
            event = event.field("sqsReceiveDurationMs", millis);
        }
        if let Some(push) = push {
            event = event
                .field("eventId", push.event_id.map(|id| id.to_string()))
                .field(
                    "notificationId",
                    push.notification_id.map(|id| id.to_string()),
                )
                .field("userId", push.user_id.map(|id| id.to_string()))
                .field("notificationType", push.kind.map(|k| k.as_str()))
                .field("sourceId", push.source_id.map(|id| id.to_string()));
        }
        event.log("푸시 작업 수신");
    }

    fn log_attempt_completed(
        &self,
        message: &QueueMessage,
        received_at: Timestamp,
        elapsed: Elapsed,
        outcome: &str,
    ) {
        self.logger
            .info(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_attempt_completed")
            .field("outcome", outcome)
            .field("queueAgeMs", find_queue_age_millis(message, received_at))
            .field("processingDurationMs", elapsed.to_millis())
            .log("푸시 처리 시도 종료");
    }
}

/// 처리 도중 실패. `received_logged`는 `worker_received`를 이미 남겼는지다(수신 시간은 둘 중 한 곳에만 싣는다).
struct ProcessingFailure {
    exception: String,
    received_logged: bool,
}

/// `findQueueAgeMillis`: 큐에 보낸 시각(`SentTimestamp`)부터 받은 시각까지. 값이 없거나 숫자가 아니거나 음수이거나 미래이면 `None`이다.
fn find_queue_age_millis(message: &QueueMessage, received_at: Timestamp) -> Option<i64> {
    let sent_at = message.sent_timestamp.as_deref()?.parse::<i64>().ok()?;
    let received_millis = received_at.as_millisecond();
    if sent_at < 0 || sent_at > received_millis {
        return None;
    }
    Some(received_millis - sent_at)
}

fn receipt(message: &QueueMessage) -> Result<&str, String> {
    message
        .receipt_handle
        .as_deref()
        .ok_or_else(|| "SdkClientException".to_owned())
}

/// `Duration.between(from, to).toSeconds()`: 내림한 초(음수면 더 작은 쪽).
fn seconds_until(from: Timestamp, to: Timestamp) -> i64 {
    let nanos = to.as_nanosecond() - from.as_nanosecond();
    nanos.div_euclid(1_000_000_000) as i64
}

/// 처리 중인 메시지 하나의 숨김 연장(`VisibilityExtension`).
struct VisibilityExtension {
    inner: Arc<ExtensionInner>,
}

struct ExtensionInner {
    queue: Arc<dyn WorkerQueue>,
    logger: Arc<Logger>,
    message_id: Option<String>,
    receipt_handle: Option<String>,
    receive_count: Option<String>,
    state: Mutex<ExtensionState>,
}

struct ExtensionState {
    stopped: bool,
    handle: Option<Box<dyn HeartbeatHandle>>,
}

impl VisibilityExtension {
    fn start(service: &SqsPushService, message: &QueueMessage) -> Self {
        let inner = Arc::new(ExtensionInner {
            queue: Arc::clone(&service.queue),
            logger: Arc::clone(&service.logger),
            message_id: message.message_id.clone(),
            receipt_handle: message.receipt_handle.clone(),
            receive_count: message.receive_count.clone(),
            state: Mutex::new(ExtensionState {
                stopped: false,
                handle: None,
            }),
        });
        let task = {
            let inner = Arc::clone(&inner);
            Arc::new(move || inner.extend()) as Arc<dyn Fn() + Send + Sync>
        };
        let handle = service.scheduler.schedule_at_fixed_rate(
            VISIBILITY_EXTENSION_INTERVAL,
            VISIBILITY_EXTENSION_INTERVAL,
            task,
        );
        lock(&inner.state).handle = Some(handle);
        Self { inner }
    }

    /// 연장을 멈춘 뒤 `completion`을 실행한다. 진행 중인 연장 요청이 있으면 끝나기를 기다린다(같은 잠금).
    fn finish_attempt(
        &self,
        completion: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = lock(&self.inner.state);
        state.stopped = true;
        if let Some(handle) = &state.handle {
            handle.cancel();
        }
        completion()
    }

    fn close(&self) {
        let mut state = lock(&self.inner.state);
        state.stopped = true;
        if let Some(handle) = &state.handle {
            handle.cancel();
        }
    }
}

impl ExtensionInner {
    /// 주기 작업 본문. 멈춘 뒤 깨어난 실행은 아무것도 하지 않는다.
    fn extend(&self) {
        let state = lock(&self.state);
        if state.stopped {
            return;
        }
        let outcome = match self.receipt_handle.as_deref() {
            Some(receipt) => self
                .queue
                .change_visibility(receipt, VISIBILITY_SECONDS)
                .map_err(|f| f.exception),
            None => Err("SdkClientException".to_owned()),
        };
        if let Err(exception) = outcome {
            // 연장 전용 스레드는 처리 스레드의 MDC를 물려받지 않는다.
            self.logger
                .error(SERVICE_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "worker_visibility_failed")
                .field("sqsMessageId", self.message_id.as_deref())
                .field("receiveCount", self.receive_count.as_deref())
                .field("errorCode", exception)
                .log("푸시 숨김 시간 연장 실패");
        }
        drop(state);
    }
}
