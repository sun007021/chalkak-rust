//! 푸시 Worker SQS 수신·완료 처리 테스트. 원본 `SqsPushServiceTest`(21개)에 대응한다.
//!
//! 원본은 로컬 HTTP 서버(JDK `HttpServer`)와 실제 SQS 클라이언트로 요청을 확인한다. 여기서는 같은 지점(HTTP 전송)에 가짜
//! 전송을 끼워 `X-Amz-Target`과 JSON 본문을 확인한다. 숨김 연장 실행기와 업무 서비스는 Mockito 대신 직접 만든 가짜를 쓴다.
//! 동시 처리는 실제 스레드로, 진행 중인 연장 요청의 대기는 열고 닫을 수 있는 문(`Gate`)으로 재현한다.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

use chalkak_aws::sqs::md5_hex;
use chalkak_aws::{
    AwsError, Credentials, CredentialsProvider, HttpRequest, SqsClient, SqsEndpoint,
    SqsHttpResponse, SqsTransport,
};
use chalkak_core::scheduler::ShutdownSignal;
use chalkak_core::{Clock, FixedClock, mdc};
use serde_json::Value;
use uuid::Uuid;

use super::domain::{NotificationSourceType, NotificationType};
use super::push_message::RawPushMessage;
use super::push_sender::PushProcessingResult;
use super::push_test_support::{Lines, logger, ts};
use super::push_worker::PushProcessor;
use super::sqs_push::PushMessage;
use super::sqs_worker::{
    HeartbeatHandle, HeartbeatScheduler, QueueMessage, RECEIVE_TIMEOUT, REQUEST_TIMEOUT,
    SqsPushService, SqsWorkerQueue,
};

const NOW: &str = "2026-10-07T00:00:00Z";
const QUEUE: &str = "http://127.0.0.1:1/000000000000/test";
const WORKER_LOGGER: &str = "com.chalkak.backend.notification.service.PushWorkerService";

// ---------------------------------------------------------------------------------------------
// 가짜
// ---------------------------------------------------------------------------------------------

/// 열고 닫을 수 있는 문: 시작을 알리고, 열릴 때까지 기다린다.
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    fn started(&self) {
        self.state.lock().unwrap().0 = true;
        self.changed.notify_all();
    }

    fn wait_started(&self) -> bool {
        let guard = self.state.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(5), |s| !s.0)
            .unwrap();
        guard.0
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }

    fn wait_released(&self) {
        let guard = self.state.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(15), |s| !s.1)
            .unwrap();
    }
}

/// 로컬 SQS 서버 대용 전송. 요청 순서와 본문을 기록한다.
#[derive(Default)]
struct Wire {
    requests: Mutex<Vec<(String, Value, Duration)>>,
    receive_response: Mutex<String>,
    fail_receive: AtomicBool,
    fail_visibility: AtomicBool,
    fail_delete: AtomicBool,
    /// 숨김 변경 응답을 문이 열릴 때까지 붙잡는다.
    hold_visibility: Mutex<Option<Arc<Gate>>>,
    /// 이 수신 핸들의 숨김 변경은 응답이 없어 요청 제한 시간에 걸린다.
    stall_receipt: Mutex<Option<String>>,
    stalled: Gate,
}

impl Wire {
    fn operations(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(op, _, _)| op.clone())
            .collect()
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body, _)| body.clone())
            .collect()
    }

    fn timeouts(&self) -> Vec<Duration> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, timeout)| *timeout)
            .collect()
    }
}

fn http(status: u16, body: &str) -> Result<SqsHttpResponse, AwsError> {
    Ok(SqsHttpResponse {
        status,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    })
}

impl SqsTransport for Wire {
    fn send(&self, request: &HttpRequest, timeout: Duration) -> Result<SqsHttpResponse, AwsError> {
        let operation = request.header("x-amz-target").unwrap().to_owned();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        self.requests
            .lock()
            .unwrap()
            .push((operation.clone(), body.clone(), timeout));
        let receipt = body["ReceiptHandle"].as_str().map(str::to_owned);
        if operation.ends_with("ChangeMessageVisibility") {
            let gate = self.hold_visibility.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.started();
                gate.wait_released();
            }
            if receipt.is_some() && receipt == *self.stall_receipt.lock().unwrap() {
                // 서버가 응답하지 않아 요청 제한 시간에 걸린다(원본은 SDK가 5초 뒤 `ApiCallTimeoutException`).
                self.stalled.started();
                return Err(AwsError::Transport("요청 시간 초과".to_owned()));
            }
        }
        let internal_error = || http(500, "{\"__type\":\"InternalError\"}");
        if operation.ends_with("ReceiveMessage") {
            if self.fail_receive.load(Ordering::SeqCst) {
                return internal_error();
            }
            return http(200, &self.receive_response.lock().unwrap().clone());
        }
        if operation.ends_with("DeleteMessage") && self.fail_delete.load(Ordering::SeqCst) {
            return internal_error();
        }
        if operation.ends_with("ChangeMessageVisibility")
            && self.fail_visibility.load(Ordering::SeqCst)
        {
            return internal_error();
        }
        http(200, "{}")
    }
}

struct StaticCredentials;

impl CredentialsProvider for StaticCredentials {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        Ok(Credentials {
            access_key_id: "test".into(),
            secret_access_key: "test".into(),
            session_token: None,
        })
    }
}

type Process = dyn Fn(&RawPushMessage) -> Result<PushProcessingResult, String> + Send + Sync;

/// `PushWorkerService` 대용: 받은 메시지를 기억하고 정해 둔 동작을 한다.
struct FakeProcessor {
    behavior: Mutex<Arc<Process>>,
    received: Mutex<Vec<RawPushMessage>>,
}

impl FakeProcessor {
    fn set(
        &self,
        behavior: impl Fn(&RawPushMessage) -> Result<PushProcessingResult, String>
        + Send
        + Sync
        + 'static,
    ) {
        *self.behavior.lock().unwrap() = Arc::new(behavior);
    }

    fn received(&self) -> Vec<RawPushMessage> {
        self.received.lock().unwrap().clone()
    }
}

impl PushProcessor for FakeProcessor {
    fn process(&self, message: &RawPushMessage) -> Result<PushProcessingResult, String> {
        self.received.lock().unwrap().push(message.clone());
        let behavior = self.behavior.lock().unwrap().clone();
        behavior(message)
    }
}

struct Scheduled {
    /// 예약한 스레드의 `sqsMessageId`(원본 `captureExtensions`는 MDC로 메시지를 구분한다).
    message_id: Option<String>,
    task: Arc<dyn Fn() + Send + Sync>,
    cancelled: Arc<AtomicBool>,
}

/// 주기 실행을 하지 않고 작업만 붙잡아 두는 실행기(원본의 Mockito `ScheduledExecutorService`).
#[derive(Default)]
struct ManualScheduler {
    scheduled: Mutex<Vec<Scheduled>>,
}

struct ManualHandle(Arc<AtomicBool>);

impl HeartbeatHandle for ManualHandle {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl HeartbeatScheduler for ManualScheduler {
    fn schedule_at_fixed_rate(
        &self,
        initial_delay: Duration,
        period: Duration,
        task: Arc<dyn Fn() + Send + Sync>,
    ) -> Box<dyn HeartbeatHandle> {
        assert_eq!(initial_delay, Duration::from_secs(40));
        assert_eq!(period, Duration::from_secs(40));
        let cancelled = Arc::new(AtomicBool::new(false));
        self.scheduled.lock().unwrap().push(Scheduled {
            message_id: mdc::get("sqsMessageId"),
            task,
            cancelled: Arc::clone(&cancelled),
        });
        Box::new(ManualHandle(cancelled))
    }
}

impl ManualScheduler {
    /// `heartbeatAction.get()`: 가장 마지막에 예약한 작업.
    fn last(&self) -> Arc<dyn Fn() + Send + Sync> {
        Arc::clone(
            &self
                .scheduled
                .lock()
                .unwrap()
                .last()
                .expect("예약된 작업")
                .task,
        )
    }

    fn for_message(&self, message_id: &str) -> Arc<dyn Fn() + Send + Sync> {
        let scheduled = self.scheduled.lock().unwrap();
        let found = scheduled
            .iter()
            .find(|s| s.message_id.as_deref() == Some(message_id))
            .expect("예약된 작업");
        Arc::clone(&found.task)
    }

    fn cancelled(&self) -> Vec<bool> {
        self.scheduled
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.cancelled.load(Ordering::SeqCst))
            .collect()
    }
}

struct Fx {
    wire: Arc<Wire>,
    lines: Lines,
    scheduler: Arc<ManualScheduler>,
    processor: Arc<FakeProcessor>,
    worker: Arc<SqsPushService>,
    shutdown: ShutdownSignal,
    push: PushMessage,
    message: QueueMessage,
}

fn push_message(kind: NotificationType, with_inbox_id: bool) -> PushMessage {
    let now = ts(NOW);
    PushMessage {
        event_id: Uuid::new_v4(),
        notification_id: with_inbox_id.then(Uuid::new_v4),
        user_id: Uuid::new_v4(),
        kind,
        source_type: Some(kind.source_type()),
        source_id: Some(Uuid::new_v4()),
        occurred_at: now,
        expires_at: super::push_test_support::plus(now, 1800),
    }
}

fn queue_message(id: &str, receipt: &str, body: &str, receive_count: Option<&str>) -> QueueMessage {
    QueueMessage {
        message_id: Some(id.to_owned()),
        receipt_handle: Some(receipt.to_owned()),
        body: body.to_owned(),
        receive_count: receive_count.map(str::to_owned),
    }
}

fn fx() -> Fx {
    let wire = Arc::new(Wire::default());
    *wire.receive_response.lock().unwrap() = "{}".to_owned();
    let client = SqsClient::new(
        "ap-northeast-2",
        SqsEndpoint::custom("http://127.0.0.1:1").unwrap(),
        Arc::new(StaticCredentials),
        wire.clone(),
        Arc::new(|| ts(NOW)),
    );
    let clock = Arc::new(FixedClock::new(ts(NOW)));
    let lines = Lines::default();
    let scheduler = Arc::new(ManualScheduler::default());
    let processor = Arc::new(FakeProcessor {
        behavior: Mutex::new(Arc::new(|_| Ok(PushProcessingResult::completed()))),
        received: Mutex::new(Vec::new()),
    });
    let shutdown = ShutdownSignal::new();
    let worker = Arc::new(SqsPushService::new(
        Arc::new(SqsWorkerQueue::new(&client, QUEUE)),
        processor.clone(),
        clock.clone() as Arc<dyn Clock>,
        logger(&lines, &clock),
        scheduler.clone(),
        shutdown.clone(),
    ));
    let push = push_message(NotificationType::PostApproved, true);
    let message = queue_message("sqs-id", "receipt-handle", &push.to_json(), Some("1"));
    Fx {
        wire,
        lines,
        scheduler,
        processor,
        worker,
        shutdown,
        push,
        message,
    }
}

/// 업무 서비스가 남기는 로그를 흉내 내는 로거(같은 스레드의 MDC를 쓴다).
fn log_from_worker(lines: &Lines, stage: &str, event_id: Option<Uuid>) {
    let clock = Arc::new(FixedClock::new(ts(NOW)));
    let logger = logger(lines, &clock);
    let mut event = logger.info(WORKER_LOGGER).field("stage", stage);
    if let Some(id) = event_id {
        event = event.field("eventId", id.to_string());
    }
    event.log("테스트 발송 결과");
}

fn second_message() -> (QueueMessage, PushMessage) {
    let push = push_message(NotificationType::PostRejected, true);
    let message = queue_message(
        "second-sqs-id",
        "second-receipt",
        &push.to_json(),
        Some("1"),
    );
    (message, push)
}

fn mdc_is_empty() -> bool {
    mdc::snapshot().is_empty()
}

/// 두 스레드에서 동시에 처리하고 각 스레드에 남은 MDC를 돌려준다(원본 `processConcurrently`).
fn process_concurrently(fx: &Fx, second: &QueueMessage) -> Vec<Vec<(String, String)>> {
    let handles: Vec<_> = [fx.message.clone(), second.clone()]
        .into_iter()
        .map(|message| {
            let worker = Arc::clone(&fx.worker);
            std::thread::spawn(move || {
                worker.process(&message);
                mdc::snapshot()
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

fn both_processing(barrier: &Arc<Barrier>) {
    barrier.wait();
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "시간 초과: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn str_field<'a>(event: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    event[key].as_str().unwrap()
}

// ---------------------------------------------------------------------------------------------
// 테스트
// ---------------------------------------------------------------------------------------------

// SqsPushServiceTest#process_redeliveredMessage_correlatesSenderLogsAndRestoresContext
#[test]
fn 재수신마다_sqs_식별자와_횟수를_발송_로그에_연결하고_기존_로그_문맥을_복원한다() {
    let fx = fx();
    let _previous = mdc::enter(&[
        ("requestId", Some("previous-request")),
        ("sqsMessageId", Some("previous-message")),
    ]);
    let before = mdc::snapshot();
    let lines = fx.lines.clone();
    fx.processor.set(move |_| {
        log_from_worker(&lines, "fcm_result", None);
        Ok(PushProcessingResult::completed())
    });

    fx.worker.process(&fx.message);
    let mut second = fx.message.clone();
    second.receive_count = Some("2".to_owned());
    fx.worker.process(&second);

    let results = fx.lines.at_stage("fcm_result");
    assert_eq!(results.len(), 2);
    assert_eq!(str_field(&results[0], "sqsMessageId"), "sqs-id");
    assert_eq!(str_field(&results[0], "receiveCount"), "1");
    assert_eq!(str_field(&results[1], "sqsMessageId"), "sqs-id");
    assert_eq!(str_field(&results[1], "receiveCount"), "2");
    let received = fx.lines.at_stage("worker_received");
    assert_eq!(
        str_field(&received[0], "eventId"),
        fx.push.event_id.to_string()
    );
    assert_eq!(
        str_field(&received[0], "userId"),
        fx.push.user_id.to_string()
    );
    assert_eq!(mdc::snapshot(), before);
}

// SqsPushServiceTest#process_invalidAndFailedMessages_recordsSafeCorrelatedFailures
#[test]
fn 메시지_파싱_처리_오류도_큐_식별자로_조사하고_원문_수신_핸들_예외_내용을_기록하지_않는다() {
    let fx = fx();
    fx.processor
        .set(|_| Err("IllegalStateException".to_owned()));
    let mut invalid = fx.message.clone();
    invalid.body = "private-json-token-secret".to_owned();

    fx.worker.process(&invalid);
    fx.worker.process(&fx.message);

    for stage in ["worker_invalid_message", "worker_processing_failed"] {
        let logs = fx.lines.at_stage(stage);
        assert_eq!(logs.len(), 1, "{stage}");
        assert_eq!(str_field(&logs[0], "sqsMessageId"), "sqs-id");
        assert_eq!(str_field(&logs[0], "receiveCount"), "1");
    }
    assert!(mdc::get("sqsMessageId").is_none());
    assert!(mdc::get("receiveCount").is_none());
    let text = fx.lines.raw().join("\n");
    for secret in [
        "private-json-token-secret",
        "private-exception-secret",
        "receipt-handle",
    ] {
        assert!(!text.contains(secret), "{secret}");
    }
}

// SqsPushServiceTest#process_visibilityFailureOnAnotherThread_recordsMessageId
#[test]
fn 전용_스레드의_숨김_연장_오류도_원래_큐_메시지와_연결한다() {
    let fx = fx();
    fx.wire.fail_visibility.store(true, Ordering::SeqCst);
    let scheduler = fx.scheduler.clone();
    fx.processor.set(move |_| {
        let task = scheduler.last();
        std::thread::spawn(move || task()).join().unwrap();
        Ok(PushProcessingResult::completed())
    });

    fx.worker.process(&fx.message);

    let logs = fx.lines.at_stage("worker_visibility_failed");
    assert_eq!(logs.len(), 1);
    assert_eq!(str_field(&logs[0], "sqsMessageId"), "sqs-id");
    assert_eq!(str_field(&logs[0], "receiveCount"), "1");
}

// SqsPushServiceTest#process_concurrentCompletions_deletesEachReceipt
#[test]
fn 동시에_완료한_두_메시지는_각자의_수신_핸들로_삭제한다() {
    let fx = fx();
    let (second, _) = second_message();
    let barrier = Arc::new(Barrier::new(2));
    fx.processor.set(move |_| {
        both_processing(&barrier);
        Ok(PushProcessingResult::completed())
    });

    process_concurrently(&fx, &second);

    assert_eq!(
        fx.wire.operations(),
        vec!["AmazonSQS.DeleteMessage", "AmazonSQS.DeleteMessage"]
    );
    let mut receipts: Vec<String> = fx
        .wire
        .bodies()
        .iter()
        .map(|b| b["ReceiptHandle"].as_str().unwrap().to_owned())
        .collect();
    receipts.sort();
    assert_eq!(receipts, vec!["receipt-handle", "second-receipt"]);
}

// SqsPushServiceTest#process_oneConcurrentMessageFails_deletesOnlySuccessfulMessage
#[test]
fn 동시_처리_중_한_메시지가_실패해도_다른_메시지는_삭제한다() {
    let fx = fx();
    let (second, _) = second_message();
    let barrier = Arc::new(Barrier::new(2));
    let first_event = fx.push.event_id.to_string();
    fx.processor.set(move |raw| {
        both_processing(&barrier);
        if raw.event_id.map(|id| id.to_string()).as_deref() != Some(first_event.as_str()) {
            return Err("IllegalStateException".to_owned());
        }
        Ok(PushProcessingResult::completed())
    });

    process_concurrently(&fx, &second);

    assert_eq!(fx.wire.operations(), vec!["AmazonSQS.DeleteMessage"]);
    assert_eq!(fx.wire.bodies()[0]["ReceiptHandle"], "receipt-handle");
    let failed = fx.lines.at_stage("worker_processing_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(str_field(&failed[0], "sqsMessageId"), "second-sqs-id");
}

// SqsPushServiceTest#process_concurrentMessages_keepsSeparateLogContexts
#[test]
fn 동시_발송_로그는_각_메시지_식별자를_유지하고_처리_후_문맥을_비운다() {
    let fx = fx();
    let (second, second_push) = second_message();
    let barrier = Arc::new(Barrier::new(2));
    let lines = fx.lines.clone();
    fx.processor.set(move |raw| {
        both_processing(&barrier);
        log_from_worker(&lines, "fcm_result", raw.event_id);
        Ok(PushProcessingResult::completed())
    });

    let remaining = process_concurrently(&fx, &second);

    assert!(remaining.iter().all(Vec::is_empty));
    let results = fx.lines.at_stage("fcm_result");
    assert_eq!(results.len(), 2);
    let pairs: BTreeMap<String, String> = results
        .iter()
        .map(|log| {
            (
                str_field(log, "sqsMessageId").to_owned(),
                str_field(log, "eventId").to_owned(),
            )
        })
        .collect();
    assert_eq!(pairs["sqs-id"], fx.push.event_id.to_string());
    assert_eq!(pairs["second-sqs-id"], second_push.event_id.to_string());
}

// SqsPushServiceTest#process_oneMessageCompletes_extendsOnlyRemainingMessage
#[test]
fn 한_메시지가_완료되어도_아직_처리_중인_다른_메시지의_숨김_연장은_유지한다() {
    let fx = fx();
    let (second, _) = second_message();
    let barrier = Arc::new(Barrier::new(2));
    let release_second = Arc::new(Gate::default());
    let first_event = fx.push.event_id.to_string();
    let gate = Arc::clone(&release_second);
    fx.processor.set(move |raw| {
        both_processing(&barrier);
        if raw.event_id.map(|id| id.to_string()).as_deref() != Some(first_event.as_str()) {
            gate.wait_released();
        }
        Ok(PushProcessingResult::completed())
    });
    let first_done = Arc::new(AtomicBool::new(false));
    let second_done = Arc::new(AtomicBool::new(false));
    let spawn = |message: QueueMessage, done: Arc<AtomicBool>| {
        let worker = Arc::clone(&fx.worker);
        std::thread::spawn(move || {
            worker.process(&message);
            done.store(true, Ordering::SeqCst);
        })
    };
    let first_thread = spawn(fx.message.clone(), first_done.clone());
    let second_thread = spawn(second, second_done.clone());

    wait_for("첫 메시지 완료", || first_done.load(Ordering::SeqCst));
    (fx.scheduler.for_message("sqs-id"))();
    (fx.scheduler.for_message("second-sqs-id"))();

    assert!(!second_done.load(Ordering::SeqCst));
    assert_eq!(
        fx.wire.operations(),
        vec![
            "AmazonSQS.DeleteMessage",
            "AmazonSQS.ChangeMessageVisibility"
        ]
    );
    assert_eq!(fx.wire.bodies()[1]["ReceiptHandle"], "second-receipt");

    release_second.release();
    second_thread.join().unwrap();
    first_thread.join().unwrap();
    (fx.scheduler.for_message("sqs-id"))();
    (fx.scheduler.for_message("second-sqs-id"))();
    assert_eq!(
        fx.wire.operations(),
        vec![
            "AmazonSQS.DeleteMessage",
            "AmazonSQS.ChangeMessageVisibility",
            "AmazonSQS.DeleteMessage"
        ]
    );
}

// SqsPushServiceTest#process_visibilityRequestStalls_singleExecutorContinuesAfterTimeout
#[test]
fn 첫_숨김_연장_요청이_응답하지_않아도_타임아웃_후_같은_스레드가_다음_메시지를_연장한다() {
    let fx = fx();
    *fx.wire.stall_receipt.lock().unwrap() = Some("receipt-handle".to_owned());
    let (second, _) = second_message();
    let barrier = Arc::new(Barrier::new(2));
    let finish = Arc::new(Gate::default());
    let gate = Arc::clone(&finish);
    fx.processor.set(move |_| {
        both_processing(&barrier);
        gate.wait_released();
        Ok(PushProcessingResult::completed())
    });
    let spawn = |message: QueueMessage| {
        let worker = Arc::clone(&fx.worker);
        std::thread::spawn(move || worker.process(&message))
    };
    let first_thread = spawn(fx.message.clone());
    let second_thread = spawn(second);
    wait_for("두 메시지 예약", || {
        fx.scheduler.scheduled.lock().unwrap().len() == 2
    });

    // 40초 주기를 기다리지 않고 두 연장 작업을 같은 스레드에서 차례로 실행한다(전용 단일 스레드 실행기와 같다).
    let first_task = fx.scheduler.for_message("sqs-id");
    let second_task = fx.scheduler.for_message("second-sqs-id");
    let extension_thread = std::thread::spawn(move || {
        first_task();
        second_task();
    });
    extension_thread.join().unwrap();

    assert_eq!(
        fx.wire.operations(),
        vec![
            "AmazonSQS.ChangeMessageVisibility",
            "AmazonSQS.ChangeMessageVisibility"
        ]
    );
    // 두 요청 모두 5초 제한으로 나간다. 첫 요청이 제한에 걸려도 다음 메시지의 연장이 진행된다.
    assert_eq!(fx.wire.timeouts(), vec![REQUEST_TIMEOUT, REQUEST_TIMEOUT]);
    assert_eq!(fx.wire.bodies()[1]["ReceiptHandle"], "second-receipt");
    let failed = fx.lines.at_stage("worker_visibility_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(str_field(&failed[0], "sqsMessageId"), "sqs-id");
    assert!(str_field(&failed[0], "errorCode").contains("Timeout"));

    finish.release();
    first_thread.join().unwrap();
    second_thread.join().unwrap();
}

// SqsPushServiceTest#process_topicMessageWithoutInboxId_passesToWorkerAndDeletes
#[test]
fn 알림함_id가_없는_주제_json도_공통_worker에_전달하고_완료_결과이면_sqs에서_삭제한다() {
    let fx = fx();
    let topic = push_message(NotificationType::Topic, false);
    let message = queue_message("topic-sqs-id", "topic-receipt", &topic.to_json(), None);

    fx.worker.process(&message);

    let received = fx.processor.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].event_id, Some(topic.event_id));
    assert_eq!(received[0].notification_id, None);
    assert_eq!(received[0].kind, Some(NotificationType::Topic));
    assert_eq!(received[0].source_type, Some(NotificationSourceType::Topic));
    assert_eq!(fx.wire.operations(), vec!["AmazonSQS.DeleteMessage"]);
}

// SqsPushServiceTest#poll_emptyQueue_requestsLongPollingAndVisibility
#[test]
fn 수신은_20초_롱_폴링_60초_숨김_한_건으로_요청한다() {
    let fx = fx();

    fx.worker.poll();

    assert_eq!(fx.wire.operations(), vec!["AmazonSQS.ReceiveMessage"]);
    let body = &fx.wire.bodies()[0];
    assert_eq!(body["WaitTimeSeconds"], 20);
    assert_eq!(body["VisibilityTimeout"], 60);
    assert_eq!(body["MaxNumberOfMessages"], 1);
    assert_eq!(
        body["MessageSystemAttributeNames"][0],
        "ApproximateReceiveCount"
    );
    assert_eq!(fx.wire.timeouts(), vec![RECEIVE_TIMEOUT]);
}

// SqsPushServiceTest#poll_receiveFailure_waitsBeforeNextPoll
#[test]
fn 수신_오류가_나면_최소_1초_대기하고_다음_폴링을_계속할_수_있다() {
    let fx = fx();
    fx.wire.fail_receive.store(true, Ordering::SeqCst);

    let started = Instant::now();
    fx.worker.poll();
    let elapsed = started.elapsed();
    fx.wire.fail_receive.store(false, Ordering::SeqCst);
    fx.worker.poll();

    assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
    assert_eq!(
        fx.wire.operations(),
        vec!["AmazonSQS.ReceiveMessage", "AmazonSQS.ReceiveMessage"]
    );
    assert_eq!(fx.lines.at_stage("worker_poll_failed").len(), 1);
    assert!(fx.processor.received().is_empty());
}

// SqsPushServiceTest#poll_interruptedFailureWait_preservesInterrupt
// Java는 스레드 인터럽트가 대기를 깨우고 인터럽트 상태를 남긴다. Rust에는 인터럽트가 없어 종료 신호가 같은 역할을 한다:
// 종료 신호가 오면 오류 뒤 1초 대기를 바로 끝내고 업무 서비스를 부르지 않는다.
#[test]
fn 수신_오류_대기에_종료_신호가_들어오면_바로_깨어난다() {
    let fx = fx();
    fx.wire.fail_receive.store(true, Ordering::SeqCst);
    fx.shutdown.trigger();

    let started = Instant::now();
    fx.worker.poll();

    assert!(started.elapsed() < Duration::from_millis(900));
    assert!(fx.shutdown.is_triggered());
    assert!(fx.processor.received().is_empty());
}

// SqsPushServiceTest#poll_oneMessage_processesAndDeletesMessage
#[test]
fn 메시지를_한_건_받으면_처리_후_해당_메시지를_삭제한다() {
    let fx = fx();
    let body = fx.message.body.clone();
    *fx.wire.receive_response.lock().unwrap() = serde_json::json!({"Messages": [{
        "MessageId": "sqs-id",
        "ReceiptHandle": "receipt-handle",
        "MD5OfBody": md5_hex(body.as_bytes()),
        "Body": body,
        "Attributes": {"ApproximateReceiveCount": "1"},
    }]})
    .to_string();

    fx.worker.poll();

    assert_eq!(
        fx.wire.operations(),
        vec!["AmazonSQS.ReceiveMessage", "AmazonSQS.DeleteMessage"]
    );
    assert_eq!(fx.wire.bodies()[1]["ReceiptHandle"], "receipt-handle");
    assert_eq!(fx.processor.received().len(), 1);
}

// SqsPushServiceTest#process_completed_deletesAndCancelsHeartbeat
#[test]
fn 완료_결과는_수신_핸들로_메시지를_삭제하고_숨김_연장을_멈춘다() {
    let fx = fx();

    fx.worker.process(&fx.message);
    (fx.scheduler.last())();

    assert_eq!(fx.wire.operations(), vec!["AmazonSQS.DeleteMessage"]);
    assert_eq!(fx.wire.bodies()[0]["ReceiptHandle"], "receipt-handle");
    assert_eq!(fx.scheduler.cancelled(), vec![true]);
}

// SqsPushServiceTest#process_processingAndTransientFailure_extendsOnlyWhileProcessing
#[test]
fn 처리_중에는_숨김_시간을_연장하고_일시_실패_후에는_연장을_멈춘다() {
    let fx = fx();
    let scheduler = fx.scheduler.clone();
    fx.processor.set(move |_| {
        (scheduler.last())();
        Ok(PushProcessingResult {
            retryable: true,
            retry_after: Duration::ZERO,
        })
    });

    fx.worker.process(&fx.message);
    (fx.scheduler.last())();

    assert_eq!(
        fx.wire.operations(),
        vec!["AmazonSQS.ChangeMessageVisibility"]
    );
    assert_eq!(fx.wire.bodies()[0]["VisibilityTimeout"], 60);
}

// SqsPushServiceTest#process_completionDuringHeartbeat_waitsBeforeFinalSqsRequest
#[test]
fn 진행_중인_숨김_연장이_끝난_뒤_삭제_또는_재시도_지연을_적용한다() {
    for retryable in [false, true] {
        let fx = fx();
        let gate = Arc::new(Gate::default());
        *fx.wire.hold_visibility.lock().unwrap() = Some(gate.clone());
        let scheduler = fx.scheduler.clone();
        let extension: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::default();
        let (slot, started) = (extension.clone(), gate.clone());
        fx.processor.set(move |_| {
            let task = scheduler.last();
            *slot.lock().unwrap() = Some(std::thread::spawn(move || task()));
            assert!(started.wait_started());
            Ok(PushProcessingResult {
                retryable,
                retry_after: Duration::from_secs(120),
            })
        });
        let done = Arc::new(AtomicBool::new(false));
        let processing = {
            let (worker, message, done) = (fx.worker.clone(), fx.message.clone(), done.clone());
            std::thread::spawn(move || {
                worker.process(&message);
                done.store(true, Ordering::SeqCst);
            })
        };

        // 연장 요청이 응답하는 동안에는 삭제도 재시도 지연도 나가지 않는다.
        wait_for("연장 요청 시작", || fx.wire.operations().len() == 1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!done.load(Ordering::SeqCst), "{retryable}");
        assert_eq!(
            fx.wire.operations(),
            vec!["AmazonSQS.ChangeMessageVisibility"]
        );

        gate.release();
        processing.join().unwrap();
        extension.lock().unwrap().take().unwrap().join().unwrap();

        assert_eq!(fx.wire.bodies()[0]["VisibilityTimeout"], 60);
        if retryable {
            assert_eq!(
                fx.wire.operations(),
                vec![
                    "AmazonSQS.ChangeMessageVisibility",
                    "AmazonSQS.ChangeMessageVisibility"
                ]
            );
            assert_eq!(fx.wire.bodies()[1]["VisibilityTimeout"], 121);
        } else {
            assert_eq!(
                fx.wire.operations(),
                vec![
                    "AmazonSQS.ChangeMessageVisibility",
                    "AmazonSQS.DeleteMessage"
                ]
            );
        }
        (fx.scheduler.last())();
        assert_eq!(fx.wire.operations().len(), 2);
    }
}

// SqsPushServiceTest#process_retryAfter_changesVisibilityWithoutDeleting
#[test]
fn retry_after가_있으면_삭제_없이_다음_전달을_지연한다() {
    let fx = fx();
    fx.processor.set(|_| {
        Ok(PushProcessingResult {
            retryable: true,
            retry_after: Duration::from_secs(120),
        })
    });

    fx.worker.process(&fx.message);

    assert_eq!(
        fx.wire.operations(),
        vec!["AmazonSQS.ChangeMessageVisibility"]
    );
    assert_eq!(fx.wire.bodies()[0]["VisibilityTimeout"], 121);
    let pending = fx.lines.at_stage("worker_retry_pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(str_field(&pending[0], "sqsMessageId"), "sqs-id");
    assert_eq!(pending[0]["retryAfterSeconds"], 120);
}

// SqsPushServiceTest#process_delayBeyondDeadline_capsVisibilityAtDeadline
#[test]
fn 재시도_지연은_사건의_원래_기한을_넘기지_않는다() {
    let fx = fx();
    fx.processor.set(|_| {
        Ok(PushProcessingResult {
            retryable: true,
            retry_after: Duration::from_secs(3600),
        })
    });

    fx.worker.process(&fx.message);

    assert_eq!(fx.wire.bodies()[0]["VisibilityTimeout"], 1800);
}

// SqsPushServiceTest#process_unexpectedFailure_keepsMessage
#[test]
fn 예상하지_못한_처리_오류는_삭제하지_않아_sqs_재전달에_맡긴다() {
    let fx = fx();
    fx.processor
        .set(|_| Err("IllegalStateException".to_owned()));

    fx.worker.process(&fx.message);
    assert!(fx.wire.operations().is_empty());
    (fx.scheduler.last())();

    assert!(fx.wire.operations().is_empty());
    assert_eq!(fx.lines.at_stage("worker_processing_failed").len(), 1);
}

// SqsPushServiceTest#process_deleteFailure_leavesMessageForRedelivery
#[test]
fn 삭제_요청_실패를_삼켜도_다시_발송할_수_있음을_숨기지_않는다() {
    let fx = fx();
    fx.wire.fail_delete.store(true, Ordering::SeqCst);

    fx.worker.process(&fx.message);
    (fx.scheduler.last())();

    assert_eq!(fx.wire.operations(), vec!["AmazonSQS.DeleteMessage"]);
    let failed = fx.lines.at_stage("worker_processing_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(str_field(&failed[0], "errorCode"), "SqsException");
    assert!(fx.lines.at_stage("worker_message_deleted").is_empty());
}

// SqsPushServiceTest#process_invalidJson_completesWithoutSending
#[test]
fn 해석할_수_없는_json은_발송하지_않고_영구_오류로_완료한다() {
    let fx = fx();
    let mut invalid = fx.message.clone();
    invalid.body = "invalid".to_owned();

    fx.worker.process(&invalid);

    assert_eq!(fx.wire.operations(), vec!["AmazonSQS.DeleteMessage"]);
    assert!(fx.processor.received().is_empty());
    assert!(mdc_is_empty());
}

// PushWorkerConfigurationTest#configuration_twoPollsBlocked_keepsCommonSchedulerFreeAndClosesExecutors (실행기 종료 부분)
#[test]
fn 숨김_연장_실행기는_종료하면_스레드가_끝나고_예약한_작업을_더_실행하지_않는다() {
    use std::sync::atomic::AtomicUsize;

    use super::sqs_worker::ThreadHeartbeatScheduler;

    let scheduler = ThreadHeartbeatScheduler::start().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&runs);
    let _handle = scheduler.schedule_at_fixed_rate(
        Duration::from_millis(10),
        Duration::from_millis(10),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    );
    wait_for("주기 실행", || runs.load(Ordering::SeqCst) >= 2);

    scheduler.shutdown();
    let after_shutdown = runs.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(runs.load(Ordering::SeqCst), after_shutdown);

    // 종료한 뒤 예약한 작업은 실행하지 않는다.
    let counter = Arc::clone(&runs);
    let _late = scheduler.schedule_at_fixed_rate(
        Duration::from_millis(1),
        Duration::from_millis(1),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(runs.load(Ordering::SeqCst), after_shutdown);
}
