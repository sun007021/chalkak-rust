//! 푸시 Worker 폴링 작업 연결 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PushWorkerConfigurationTest`의 스케줄러 동작 5개를 옮겼다. Spring 컨텍스트 대신 `jobs::start`가 같은 구실을 한다:
//! 원본의 "컨텍스트 초기화 후 폴링 시작"은 "`start` 호출 뒤 폴링 시작", "공용 스케줄러에 폴링이 막히지 않음"은
//! "다른 주기 작업(Relay)이 폴링 두 개가 모두 대기하는 동안에도 실행됨"에 해당한다.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use chalkak_app::jobs::{self, Job, JobDeps, JobsSettings};
use chalkak_app::notification::domain::Notification;
use chalkak_app::notification::push_message::RawPushMessage;
use chalkak_app::notification::push_sender::PushProcessingResult;
use chalkak_app::notification::push_worker::PushProcessor;
use chalkak_app::notification::push_worker_config::{FcmSettings, PushWorker, build_push_worker};
use chalkak_app::notification::repository::NotificationRepository;
use chalkak_app::notification::sqs_push::{
    PublisherFailure, PushMessage, PushMessagePublisher, PushPublicationResult, SqsPushSettings,
};
use chalkak_app::notification::sqs_worker::{
    HeartbeatHandle, HeartbeatScheduler, QueueFailure, QueueMessage, SqsPushService, WorkerQueue,
};
use chalkak_app::testing::TestDb;
use chalkak_core::config::Env;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, FixedClock, ShutdownSignal};
use chalkak_db::Pool;
use jiff::Timestamp;
use uuid::Uuid;

const NOW: &str = "2026-10-07T00:00:00Z";
const QUEUE_URL: &str = "https://sqs.ap-northeast-2.amazonaws.com/000000000000/test";

/// 문이 열릴 때까지 `receive`를 붙잡는 큐. 호출 횟수와 호출한 스레드를 기록한다.
#[derive(Default)]
struct BlockingQueue {
    receives: AtomicUsize,
    threads: Mutex<HashSet<ThreadId>>,
    released: Mutex<bool>,
    changed: Condvar,
}

impl BlockingQueue {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn receives(&self) -> usize {
        self.receives.load(Ordering::SeqCst)
    }
}

impl WorkerQueue for BlockingQueue {
    fn receive(&self) -> Result<Option<QueueMessage>, QueueFailure> {
        self.receives.fetch_add(1, Ordering::SeqCst);
        self.threads
            .lock()
            .unwrap()
            .insert(std::thread::current().id());
        let guard = self.released.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(10), |released| !*released)
            .unwrap();
        Ok(None)
    }

    fn delete(&self, _: &str) -> Result<(), QueueFailure> {
        Ok(())
    }

    fn change_visibility(&self, _: &str, _: u32) -> Result<(), QueueFailure> {
        Ok(())
    }
}

struct Processor;

impl PushProcessor for Processor {
    fn process(&self, _: &RawPushMessage) -> Result<PushProcessingResult, String> {
        Ok(PushProcessingResult::completed())
    }
}

struct NoHeartbeat;

struct NoHandle;

impl HeartbeatHandle for NoHandle {
    fn cancel(&self) {}
}

impl HeartbeatScheduler for NoHeartbeat {
    fn schedule_at_fixed_rate(
        &self,
        _: Duration,
        _: Duration,
        _: Arc<dyn Fn() + Send + Sync>,
    ) -> Box<dyn HeartbeatHandle> {
        Box::new(NoHandle)
    }
}

/// Relay가 부르는 발행기: 부른 횟수와 스레드를 기록한다.
#[derive(Default)]
struct RecordingPublisher {
    calls: AtomicUsize,
    threads: Mutex<HashSet<ThreadId>>,
}

impl PushMessagePublisher for RecordingPublisher {
    fn publish(&self, _: &PushMessage) -> Result<PushPublicationResult, PublisherFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.threads
            .lock()
            .unwrap()
            .insert(std::thread::current().id());
        Ok(PushPublicationResult::accepted("sqs-message-id"))
    }
}

struct Fx {
    db: TestDb,
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
    queue: Arc<BlockingQueue>,
    publisher: Arc<RecordingPublisher>,
    shutdown: ShutdownSignal,
}

fn fx() -> Option<Fx> {
    let db = TestDb::create()?;
    let pool = db.pool(4);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let logger = Arc::new(Logger::new(
        Box::new(std::io::sink()),
        Level::Info,
        clock.clone(),
    ));
    let now: Timestamp = NOW.parse().unwrap();
    let (user, topic, photo, post) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let mut c = db.client();
    c.execute(
        "INSERT INTO users (id, email, status, signature_original_storage_key) \
         VALUES ($1, 'poll@example.com', 'ACTIVE', 'poll/signature')",
        &[&user],
    )
    .unwrap();
    c.execute(
        "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) \
         VALUES ($1, '주제', '2026-10-07', $2::timestamptz, $2::timestamptz + INTERVAL '1 hour')",
        &[&topic, &now],
    )
    .unwrap();
    c.execute(
        "INSERT INTO photos (id, original_storage_key) VALUES ($1, 'poll/original')",
        &[&photo],
    )
    .unwrap();
    c.execute(
        "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status) \
         VALUES ($1, $2, $3, $4, 'APPROVED')",
        &[&post, &user, &topic, &photo],
    )
    .unwrap();
    drop(c);
    // 발행 대기 알림 하나: Relay 작업이 도는지 보는 표지다.
    chalkak_app::common::transact(&pool, |tx| {
        let mut notification = Notification::approved(user, post, Uuid::new_v4(), now, true);
        NotificationRepository.save(tx, &mut notification)?;
        Ok(())
    })
    .unwrap();
    Some(Fx {
        db,
        pool,
        clock,
        logger,
        queue: Arc::new(BlockingQueue::default()),
        publisher: Arc::new(RecordingPublisher::default()),
        shutdown: ShutdownSignal::new(),
    })
}

fn settings(extra: &[(&str, &str)]) -> JobsSettings {
    let mut pairs = vec![
        ("NOTIFICATION_RELAY_ENABLED", "true"),
        ("NOTIFICATION_WORKER_ENABLED", "true"),
        ("NOTIFICATION_SQS_QUEUE_URL", QUEUE_URL),
        ("NOTIFICATION_FCM_PROJECT_ID", "test"),
        ("NOTIFICATION_FCM_CREDENTIALS_PATH", "test.json"),
        // 정리 작업은 이 시험과 상관없으니 끈다.
        ("CHALKAK_AUTH_REFRESHTOKEN_CLEANUP_ENABLED", "false"),
        ("CHALKAK_AUTH_CONSUMEDSIGNUPTOKEN_CLEANUP_ENABLED", "false"),
        ("CHALKAK_NOTIFICATION_CLEANUP_ENABLED", "false"),
        (
            "CHALKAK_AUTH_PENDINGAPPLEAUTHORIZATION_CLEANUP_ENABLED",
            "false",
        ),
    ];
    pairs.extend_from_slice(extra);
    JobsSettings::from_env(&Env::from_pairs(pairs), false, false).unwrap()
}

impl Fx {
    fn worker(&self) -> Arc<PushWorker> {
        Arc::new(PushWorker {
            service: Arc::new(SqsPushService::new(
                self.queue.clone(),
                Arc::new(Processor),
                self.clock.clone(),
                self.logger.clone(),
                Arc::new(NoHeartbeat),
                self.shutdown.clone(),
            )),
        })
    }

    fn deps(&self, worker: Option<Arc<PushWorker>>) -> JobDeps {
        JobDeps {
            pool: self.pool.clone(),
            clock: self.clock.clone(),
            logger: self.logger.clone(),
            apple: None,
            publisher: Some(self.publisher.clone()),
            analytics: None,
            worker,
        }
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "시간 초과: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// PushWorkerConfigurationTest#configuration_twoPollsBlocked_keepsCommonSchedulerFreeAndClosesExecutors
#[test]
fn 두_폴링이_동시에_대기해도_다른_주기_작업이_실행되고_종료하면_스레드가_끝난다() {
    let Some(fx) = fx() else { return };
    let started = jobs::start(&fx.deps(Some(fx.worker())), &settings(&[]), &fx.shutdown).unwrap();
    assert!(started.started().contains(&Job::NotificationWorkerFirst));
    assert!(started.started().contains(&Job::NotificationWorkerSecond));

    wait_until("폴링 둘이 모두 대기", || fx.queue.receives() == 2);
    wait_until("Relay 실행", || {
        fx.publisher.calls.load(Ordering::SeqCst) >= 1
    });

    let polls = fx.queue.threads.lock().unwrap().clone();
    let relay = fx.publisher.threads.lock().unwrap().clone();
    assert_eq!(polls.len(), 2);
    assert!(polls.is_disjoint(&relay));

    fx.shutdown.trigger();
    fx.queue.release();
    started.join();
}

// PushWorkerConfigurationTest#configuration_schedulingDisabled_doesNotRegisterPolling
#[test]
fn 공용_스케줄링을_끄면_worker의_폴링_작업도_시작하지_않는다() {
    let Some(fx) = fx() else { return };
    let settings = settings(&[("CHALKAK_SCHEDULING_ENABLED", "false")]);
    assert!(settings.planned().is_empty());
    // 폴링 작업 자체는 켜져 있지만(원본에서 빈은 남는다) 스케줄링이 꺼져 있어 도는 것이 없다.
    assert!(settings.job_enabled(Job::NotificationWorkerFirst));

    let started = jobs::start(&fx.deps(Some(fx.worker())), &settings, &fx.shutdown).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert!(started.started().is_empty());
    assert_eq!(fx.queue.receives(), 0);
    assert_eq!(fx.publisher.calls.load(Ordering::SeqCst), 0);
    fx.shutdown.trigger();
    started.join();
}

// PushWorkerConfigurationTest#configuration_initializationFails_doesNotPoll
#[test]
fn 초기화가_실패하면_sqs_폴링을_시작하지_않는다() {
    let Some(fx) = fx() else { return };
    // Worker 조립에 실패한 상황: 서비스 계정 파일을 읽을 수 없으면 `build_push_worker`가 오류를 낸다.
    let sqs = SqsPushSettings::from_env(&Env::from_pairs([
        ("NOTIFICATION_WORKER_ENABLED", "true"),
        ("NOTIFICATION_SQS_QUEUE_URL", QUEUE_URL),
    ]))
    .unwrap();
    let fcm = FcmSettings {
        project_id: "test".to_owned(),
        credentials_path: "/nonexistent/service-account.json".to_owned(),
        endpoint_url: "https://fcm.googleapis.com".to_owned(),
    };
    let built = build_push_worker(&sqs, &fcm, &fx.pool, &fx.clock, &fx.logger, &fx.shutdown);
    assert!(built.is_err());

    // Worker가 없으면 `start`는 어떤 작업도 시작하지 않고 실패한다(Relay도 돌지 않는다).
    let result = jobs::start(&fx.deps(None), &settings(&[]), &fx.shutdown);
    assert!(result.is_err());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fx.queue.receives(), 0);
    assert_eq!(fx.publisher.calls.load(Ordering::SeqCst), 0);
}

// PushWorkerConfigurationTest#configuration_initializationCompletes_startsPollingAfterInitialization
#[test]
fn 시작_전에는_폴링하지_않고_시작_뒤_두_worker가_수신한다() {
    let Some(fx) = fx() else { return };
    let deps = fx.deps(Some(fx.worker()));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fx.queue.receives(), 0);

    let started = jobs::start(&deps, &settings(&[]), &fx.shutdown).unwrap();
    wait_until("폴링 둘이 시작", || fx.queue.receives() == 2);

    fx.shutdown.trigger();
    fx.queue.release();
    started.join();
}

// PushWorkerConfigurationTest#configuration_contextClosing_doesNotPollAgain
#[test]
fn 종료_신호_뒤_진행_중인_수신이_끝나도_다음_폴링을_시작하지_않는다() {
    let Some(fx) = fx() else { return };
    let started = jobs::start(&fx.deps(Some(fx.worker())), &settings(&[]), &fx.shutdown).unwrap();
    wait_until("폴링 둘이 대기", || fx.queue.receives() == 2);

    fx.shutdown.trigger();
    fx.queue.release();
    started.join();

    assert_eq!(fx.queue.receives(), 2);
    let _ = &fx.db;
}
