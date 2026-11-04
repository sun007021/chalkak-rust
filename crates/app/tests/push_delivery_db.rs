//! 푸시 발송 기록(`PushDeliveryRepository`, `PushDeliveryService`, `PushDeliveryAttempt`)의 DB 통합 테스트.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PushDeliveryServiceTest`, `PushDeliveryRepositoryImplTest`를 옮기고, 같은 키·다른 키·기록 생성의 동시 실행 시험을
//! 더했다. 원본의 `Propagation.NEVER`(상위 트랜잭션 거절)는 Rust에 암묵적 트랜잭션이 없어 옮기지 않는다.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use chalkak_app::notification::domain::{NotificationSourceType, NotificationType};
use chalkak_app::notification::push_delivery::{
    DeviceDeliveryStatus, PushDeliveryError, PushDeliveryRepository, PushDeliveryService,
    PushDeliveryStatus, RETRY_DELAY,
};
use chalkak_app::notification::push_sender::{DevicePushResult, PushProcessingResult};
use chalkak_app::notification::sqs_push::PushMessage;
use chalkak_app::testing::TestDb;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, FixedClock};
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};
use uuid::Uuid;

const NOW: &str = "2026-10-26T00:00:00Z";

fn now() -> Timestamp {
    NOW.parse().unwrap()
}

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Lines {
    fn at_stage(&self, stage: &str) -> Vec<Map<String, Value>> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .filter(|event| event.get("stage").and_then(Value::as_str) == Some(stage))
            .collect()
    }
}

/// 스레드 사이에 공유하는 부분(`TestDb`는 공유하지 못한다).
struct Shared {
    pool: Pool,
    service: PushDeliveryService,
    lines: Lines,
    clock: Arc<dyn Clock>,
    user: Uuid,
    message: PushMessage,
}

struct Fx {
    db: TestDb,
    shared: Arc<Shared>,
}

impl std::ops::Deref for Fx {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.shared
    }
}

fn insert_user(db: &TestDb, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    db.client()
        .execute(
            "INSERT INTO users (id, email, signature_original_storage_key) VALUES ($1, $2, $3)",
            &[&id, &email, &format!("delivery/{id}")],
        )
        .unwrap();
    id
}

fn message(event_id: Uuid, user_id: Uuid) -> PushMessage {
    PushMessage {
        event_id,
        notification_id: None,
        user_id,
        kind: NotificationType::Topic,
        source_type: Some(NotificationSourceType::Topic),
        source_id: Some(Uuid::new_v4()),
        occurred_at: now(),
        expires_at: now().checked_add(SignedDuration::from_secs(1800)).unwrap(),
    }
}

impl Fx {
    fn new() -> Option<Fx> {
        let db = TestDb::create()?;
        let pool = db.pool(8);
        let user = insert_user(&db, "delivery-test@example.com");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(now()));
        let lines = Lines::default();
        let logger = Arc::new(Logger::new(
            Box::new(lines.clone()),
            Level::Info,
            clock.clone(),
        ));
        let service = PushDeliveryService::new(pool.clone(), logger);
        Some(Fx {
            db,
            shared: Arc::new(Shared {
                message: message(Uuid::new_v4(), user),
                pool,
                service,
                lines,
                clock,
                user,
            }),
        })
    }
}

impl Shared {
    /// 기록 하나에 기기 결과를 직접 쌓는다(원본의 `repository.saveDeviceResult`).
    fn current(&self) -> chalkak_app::notification::push_delivery::PushDelivery {
        self.find(self.message.event_id, self.user).unwrap()
    }

    fn find(
        &self,
        event_id: Uuid,
        user_id: Uuid,
    ) -> Option<chalkak_app::notification::push_delivery::PushDelivery> {
        let mut conn = self.pool.get().unwrap();
        PushDeliveryRepository
            .find_by_event_id_and_user_id(&mut conn, event_id, user_id)
            .unwrap()
    }

    /// 기기 하나를 수락 처리하는 전형적인 한 번의 처리.
    fn send_accepted(&self, message: &PushMessage, device: Uuid) -> PushProcessingResult {
        self.service
            .process(message, |attempt| {
                let result =
                    attempt.send(device, &*self.clock, || Ok(DevicePushResult::accepted()))?;
                Ok(PushProcessingResult {
                    retryable: result.is_retryable(),
                    retry_after: result.retry_after,
                })
            })
            .unwrap()
    }
}

fn unreachable_action() -> Result<DevicePushResult, String> {
    panic!("이 경로에서는 FCM을 호출하면 안 된다");
}

fn retry_later() -> PushProcessingResult {
    PushProcessingResult {
        retryable: true,
        retry_after: RETRY_DELAY,
    }
}

// PushDeliveryServiceTest#process_acceptedAndRetryableDevices_keepsPendingDelivery
#[test]
fn 수락한_기기가_있어도_다른_기기의_재처리가_필요하면_미완료_이력을_유지한다() {
    let Some(fx) = Fx::new() else { return };
    let (accepted, other) = (Uuid::new_v4(), Uuid::new_v4());

    let result = fx
        .service
        .process(&fx.message, |attempt| {
            attempt.send(accepted, &*fx.clock, || Ok(DevicePushResult::accepted()))?;
            attempt.send(other, &*fx.clock, || {
                Ok(DevicePushResult::retryable("UNAVAILABLE"))
            })?;
            Ok(PushProcessingResult {
                retryable: true,
                retry_after: Duration::from_secs(120),
            })
        })
        .unwrap();

    assert!(result.retryable);
    assert_eq!(result.retry_after, Duration::from_secs(120));
    let current = fx.current();
    assert_eq!(current.status, PushDeliveryStatus::Pending);
    assert!(current.device_results[&accepted].is_accepted());
    assert_eq!(
        current.device_results[&other].status,
        DeviceDeliveryStatus::Retryable
    );
}

// PushDeliveryServiceTest#process_sendFails_recordsUnknownAndUnlocks
#[test]
fn sdk_예외는_결과_불명으로_저장하고_락을_해제해_다음_재처리를_허용한다() {
    let Some(fx) = Fx::new() else { return };
    let device = Uuid::new_v4();

    let failed = fx
        .service
        .process(&fx.message, |attempt| {
            let sent = attempt.send(device, &*fx.clock, || {
                Err("IllegalStateException".to_owned())
            })?;
            Ok(PushProcessingResult {
                retryable: sent.is_retryable(),
                retry_after: sent.retry_after,
            })
        })
        .unwrap();

    assert!(failed.retryable);
    let record = &fx.current().device_results[&device];
    assert_eq!(record.status, DeviceDeliveryStatus::Unknown);
    assert_eq!(
        record.last_error_code.as_deref(),
        Some("IllegalStateException")
    );

    assert!(!fx.send_accepted(&fx.message, device).retryable);
    let current = fx.current();
    assert!(current.is_completed());
    assert_eq!(current.device_results[&device].attempt_count, 2);
    // 결과 불명 기록을 다시 처리했음을 조사 로그로 남긴다.
    let previous = fx.lines.at_stage("worker_delivery_previous_unknown");
    assert_eq!(previous.len(), 1);
    assert_eq!(previous[0]["deviceId"], device.to_string());
    assert_eq!(previous[0]["level"], "WARN");
}

// PushDeliveryServiceTest#process_actionFailsAfterAcceptance_preservesResultAndUnlocks
#[test]
fn 수락_결과_이후_오류가_나도_수락_기록은_남고_락은_해제된다() {
    let Some(fx) = Fx::new() else { return };
    let device = Uuid::new_v4();

    let error = fx
        .service
        .process(&fx.message, |attempt| {
            attempt.send(device, &*fx.clock, || Ok(DevicePushResult::accepted()))?;
            Err(PushDeliveryError::Api(
                chalkak_app::web::ApiError::Internal("processing failed after result".to_owned()),
            ))
        })
        .unwrap_err();

    assert!(matches!(error, PushDeliveryError::Api(_)));
    let current = fx.current();
    assert_eq!(current.status, PushDeliveryStatus::Pending);
    assert!(current.device_results[&device].is_accepted());

    let retried = fx
        .service
        .process(&fx.message, |attempt| {
            // 수락한 기기는 다시 발송하지 않는다.
            let skipped = attempt.send(device, &*fx.clock, unreachable_action)?;
            assert_eq!(
                skipped,
                DevicePushResult::skipped("DELIVERY_ALREADY_PROCESSED")
            );
            Ok(PushProcessingResult::completed())
        })
        .unwrap();
    assert!(!retried.retryable);
    assert!(fx.current().is_completed());
}

// PushDeliveryServiceTest#process_startRecordFails_rollsBackPreparationAndUnlocks
#[test]
fn 시작_기록_실패는_신규_이력_생성을_롤백하고_기존_기기의_수락_기록은_보존한다() {
    for existing_acceptance in [false, true] {
        let Some(fx) = Fx::new() else { return };
        let target = Uuid::new_v4();
        let previous_device = Uuid::new_v4();
        if existing_acceptance {
            fx.send_accepted(&fx.message, previous_device);
            // 완료 처리된 행은 새 처리가 막으므로, 미완료 상태의 수락 기록으로 되돌려 놓는다.
            fx.db
                .client()
                .batch_execute("UPDATE push_deliveries SET status = 'PENDING', completed_at = NULL")
                .unwrap();
        }
        fx.db
            .client()
            .batch_execute(
                "ALTER TABLE push_deliveries ADD CONSTRAINT ck_test_push_delivery_start \
                 CHECK (NOT jsonb_path_exists(device_results, '$.* ? (@.status == \"SENDING\")'))",
            )
            .unwrap();

        let failed = fx
            .service
            .process(&fx.message, |attempt| {
                attempt.send(target, &*fx.clock, unreachable_action)?;
                Ok(PushProcessingResult::completed())
            })
            .unwrap();

        assert_eq!(failed, retry_later(), "{existing_acceptance}");
        if existing_acceptance {
            let current = fx.current();
            assert_eq!(current.status, PushDeliveryStatus::Pending);
            assert_eq!(current.device_results.len(), 1);
            assert!(current.device_results[&previous_device].is_accepted());
        } else {
            assert!(fx.find(fx.message.event_id, fx.user).is_none());
        }
        assert_eq!(fx.lines.at_stage("worker_delivery_record_failed").len(), 1);
        assert_eq!(
            fx.lines
                .at_stage("worker_delivery_record_unavailable")
                .len(),
            1
        );
        fx.db
            .client()
            .batch_execute(
                "ALTER TABLE push_deliveries DROP CONSTRAINT ck_test_push_delivery_start",
            )
            .unwrap();

        // 롤백과 락 해제 후 같은 사건을 다시 처리한다.
        let retried = fx
            .service
            .process(&fx.message, |attempt| {
                attempt.send(target, &*fx.clock, || Ok(DevicePushResult::accepted()))?;
                Ok(PushProcessingResult::completed())
            })
            .unwrap();
        assert!(!retried.retryable);
        let current = fx.current();
        assert!(current.is_completed());
        assert_eq!(current.device_results[&target].attempt_count, 1);
    }
}

// PushDeliveryServiceTest#process_preparationCommitFails_rollsBackBeforeSendAndUnlocks
#[test]
fn 준비_sql이_성공해도_커밋이_실패하면_이력을_롤백하고_fcm_호출_없이_락을_해제한다() {
    let Some(fx) = Fx::new() else { return };
    let device = Uuid::new_v4();
    fx.db
        .client()
        .batch_execute(
            "CREATE FUNCTION delivery_test_reject_commit() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'preparation commit failed'; END $$; \
             CREATE CONSTRAINT TRIGGER delivery_test_reject_commit \
             AFTER UPDATE ON push_deliveries DEFERRABLE INITIALLY DEFERRED \
             FOR EACH ROW EXECUTE FUNCTION delivery_test_reject_commit()",
        )
        .unwrap();

    let failed = fx
        .service
        .process(&fx.message, |attempt| {
            attempt.send(device, &*fx.clock, unreachable_action)?;
            Ok(PushProcessingResult::completed())
        })
        .unwrap();

    assert_eq!(failed, retry_later());
    assert!(fx.find(fx.message.event_id, fx.user).is_none());
    fx.db
        .client()
        .batch_execute(
            "DROP TRIGGER delivery_test_reject_commit ON push_deliveries; \
             DROP FUNCTION delivery_test_reject_commit()",
        )
        .unwrap();
    assert!(!fx.send_accepted(&fx.message, device).retryable);
    assert!(fx.current().is_completed());
}

// PushDeliveryServiceTest#process_sendOutsideTransaction_preservesTransactionBoundaries
#[test]
fn 시작_기록은_fcm_호출_전에_커밋된다() {
    let Some(fx) = Fx::new() else { return };
    let device = Uuid::new_v4();

    let result = fx
        .service
        .process(&fx.message, |attempt| {
            let sent = attempt.send(device, &*fx.clock, || {
                // FCM 대기 중에 다른 연결에서도 SENDING 기록이 보인다.
                let seen = fx.current();
                assert_eq!(
                    seen.device_results[&device].status,
                    DeviceDeliveryStatus::Sending
                );
                Ok(DevicePushResult::accepted())
            })?;
            let seen = fx.current();
            assert_eq!(seen.status, PushDeliveryStatus::Pending);
            assert!(seen.device_results[&device].is_accepted());
            Ok(PushProcessingResult {
                retryable: sent.is_retryable(),
                retry_after: sent.retry_after,
            })
        })
        .unwrap();

    assert!(!result.retryable);
    assert!(fx.current().is_completed());
}

// PushDeliveryServiceTest#process_nestedSameKey_preservesRegistrationUntilOuterFinishes
#[test]
fn 중첩_진입이_거절돼도_바깥_처리의_키를_지우지_않고_종료_후_재처리를_허용한다() {
    let Some(fx) = Fx::new() else { return };

    let outer = fx
        .service
        .process(&fx.message, |_| {
            for _ in 0..2 {
                let nested = fx
                    .service
                    .process(&fx.message, |_| {
                        panic!("진행 중인 키의 중첩 처리에 진입하면 안 된다")
                    })
                    .unwrap();
                assert_eq!(nested, retry_later());
            }
            Ok(PushProcessingResult {
                retryable: true,
                retry_after: Duration::ZERO,
            })
        })
        .unwrap();

    assert!(outer.retryable);
    assert_eq!(fx.lines.at_stage("worker_delivery_lock_busy").len(), 2);
    assert!(!fx.send_accepted(&fx.message, Uuid::new_v4()).retryable);
    assert!(fx.current().is_completed());
}

// PushDeliveryServiceTest#process_sameKeyBeforeRecordExists_rejectsConcurrentAction
#[test]
fn 같은_사건_회원은_db_행이_생기기_전에도_중복_진입을_막고_다음_수신으로_넘긴다() {
    let Some(fx) = Fx::new() else { return };
    let shared = Arc::clone(&fx.shared);
    let (entered_tx, entered_rx) = channel::<()>();
    let (finish_tx, finish_rx) = channel::<()>();
    let device = Uuid::new_v4();
    let first = {
        let fx = Arc::clone(&shared);
        std::thread::spawn(move || {
            fx.service
                .process(&fx.message, |attempt| {
                    entered_tx.send(()).unwrap();
                    finish_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    attempt.send(device, &*fx.clock, || Ok(DevicePushResult::accepted()))?;
                    Ok(PushProcessingResult::completed())
                })
                .unwrap()
        })
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(fx.find(fx.message.event_id, fx.user).is_none());

    let duplicate = fx
        .service
        .process(&fx.message, |_| {
            panic!("같은 키의 진행 중 작업에 진입하면 안 된다")
        })
        .unwrap();

    assert_eq!(duplicate, retry_later());
    finish_tx.send(()).unwrap();
    assert!(!first.join().unwrap().retryable);
    let done = fx
        .service
        .process(&fx.message, |_| panic!("완료 기록을 먼저 확인해야 한다"))
        .unwrap();
    assert!(!done.retryable);
    assert_eq!(fx.lines.at_stage("worker_delivery_skipped").len(), 1);
}

// PushDeliveryServiceTest#process_differentKey_runsWhileOtherIsSending
#[test]
fn 사건이나_회원_중_하나가_다르면_기존_작업이_fcm을_기다려도_병렬로_처리한다() {
    for different_user in [false, true] {
        let Some(fx) = Fx::new() else { return };
        let shared = Arc::clone(&fx.shared);
        let (mut other_user, mut other_event) = (fx.user, Uuid::new_v4());
        if different_user {
            other_user = insert_user(&fx.db, "other-delivery@example.com");
            other_event = fx.message.event_id;
        }
        let other_message = message(other_event, other_user);
        let (sending_tx, sending_rx) = channel::<()>();
        let (finish_tx, finish_rx) = channel::<()>();
        let first = {
            let fx = Arc::clone(&shared);
            std::thread::spawn(move || {
                fx.service
                    .process(&fx.message, |attempt| {
                        attempt.send(Uuid::new_v4(), &*fx.clock, || {
                            sending_tx.send(()).unwrap();
                            finish_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                            Ok(DevicePushResult::accepted())
                        })?;
                        Ok(PushProcessingResult::completed())
                    })
                    .unwrap()
            })
        };
        sending_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let other = {
            let fx = Arc::clone(&shared);
            let other_message = other_message.clone();
            std::thread::spawn(move || fx.send_accepted(&other_message, Uuid::new_v4()))
        }
        .join()
        .unwrap();

        assert!(!other.retryable, "{different_user}");
        assert!(!first.is_finished());
        assert!(
            fx.find(other_message.event_id, other_message.user_id)
                .unwrap()
                .is_completed()
        );
        finish_tx.send(()).unwrap();
        assert!(!first.join().unwrap().retryable);
    }
}

// PushDeliveryServiceTest#process_noDevice_createsNoRecordAndUnlocks
#[test]
fn 발송할_기기가_없으면_이력을_만들지_않고_같은_키의_다음_처리를_허용한다() {
    let Some(fx) = Fx::new() else { return };

    let result = fx
        .service
        .process(&fx.message, |_| Ok(PushProcessingResult::completed()))
        .unwrap();

    assert!(!result.retryable);
    assert!(fx.find(fx.message.event_id, fx.user).is_none());
    assert!(!fx.send_accepted(&fx.message, Uuid::new_v4()).retryable);
}

// PushDeliveryServiceTest#process_completedDelivery_skipsActionAndUnlocks
#[test]
fn 완료된_이력은_기기_처리에_진입하지_않고_락을_해제한다() {
    let Some(fx) = Fx::new() else { return };
    let mut conn = fx.pool.get().unwrap();
    let delivery = PushDeliveryRepository
        .create_if_absent(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    assert!(
        PushDeliveryRepository
            .complete(&mut conn, delivery.id)
            .unwrap()
    );
    drop(conn);

    for _ in 0..2 {
        let result = fx
            .service
            .process(&fx.message, |_| {
                panic!("완료된 사건은 다시 처리하면 안 된다")
            })
            .unwrap();
        assert!(!result.retryable);
    }
    assert!(!fx.service.locks().is_held(fx.message.event_id, fx.user));
}

// 동시성: 같은 키로 여러 스레드가 동시에 들어와도 FCM(action)은 한 번만 실행된다.
#[test]
fn 같은_사건을_동시에_받아도_락을_잡은_처리만_기기를_발송한다() {
    const THREADS: usize = 8;
    let Some(fx) = Fx::new() else { return };
    let shared = Arc::clone(&fx.shared);
    let device = Uuid::new_v4();
    let actions = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (fx, actions, start) = (
                Arc::clone(&shared),
                Arc::clone(&actions),
                Arc::clone(&start),
            );
            std::thread::spawn(move || {
                start.wait();
                fx.service
                    .process(&fx.message, |attempt| {
                        actions.fetch_add(1, Ordering::SeqCst);
                        attempt.send(device, &*fx.clock, || {
                            std::thread::sleep(Duration::from_millis(100));
                            Ok(DevicePushResult::accepted())
                        })?;
                        Ok(PushProcessingResult::completed())
                    })
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // 락을 잡은 처리는 완료하고, 늦게 온 처리는 1분 뒤 재처리로 넘기거나(진행 중) 완료 기록을 보고 끝낸다.
    assert!(results.iter().any(|r| !r.retryable));
    assert!(results.iter().all(|r| !r.retryable || *r == retry_later()));
    assert_eq!(actions.load(Ordering::SeqCst), 1);
    let current = fx.current();
    assert!(current.is_completed());
    // 같은 기기를 두 번 발송하지 않았다.
    assert_eq!(current.device_results[&device].attempt_count, 1);
}

// ---- PushDeliveryRepositoryImplTest ----

// PushDeliveryRepositoryImplTest#createIfAbsent_existingDelivery_preservesResultsAndCompletion
#[test]
fn 같은_사건_회원으로_다시_생성해도_기존_기기_결과와_완료_상태를_보존한다() {
    let Some(fx) = Fx::new() else { return };
    let mut conn = fx.pool.get().unwrap();
    let repository = PushDeliveryRepository;
    let device = Uuid::new_v4();
    let delivery = repository
        .create_if_absent(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    let accepted = delivery
        .start_device(device, now())
        .unwrap()
        .finish(DeviceDeliveryStatus::Accepted, None, now())
        .unwrap();
    assert!(
        repository
            .save_device_result(&mut conn, delivery.id, device, &accepted)
            .unwrap()
    );
    assert!(repository.complete(&mut conn, delivery.id).unwrap());

    let again = repository
        .create_if_absent(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();

    assert_eq!(again.id, delivery.id);
    assert!(again.is_completed());
    assert!(again.completed_at.is_some());
    assert_eq!(again.device_results[&device], accepted);
}

// PushDeliveryRepositoryImplTest#createIfAbsent_differentUser_createsSeparateDelivery
#[test]
fn 같은_사건도_수신_회원이_다르면_별도_발송_이력을_만든다() {
    let Some(fx) = Fx::new() else { return };
    let other = insert_user(&fx.db, "other@example.com");
    let mut conn = fx.pool.get().unwrap();

    let first = PushDeliveryRepository
        .create_if_absent(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    let second = PushDeliveryRepository
        .create_if_absent(&mut conn, fx.message.event_id, other)
        .unwrap()
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(first.event_id, second.event_id);
}

// PushDeliveryRepositoryImplTest#createIfAbsent_concurrentCreate_returnsSingleRow
#[test]
fn 동시에_같은_이력을_생성해도_유니크_제약으로_하나의_행을_반환한다() {
    const THREADS: usize = 8;
    let Some(fx) = Fx::new() else { return };
    let start = Arc::new(Barrier::new(THREADS));
    let event_id = fx.message.event_id;
    let user = fx.user;

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (pool, start) = (fx.pool.clone(), Arc::clone(&start));
            std::thread::spawn(move || {
                start.wait();
                let mut conn = pool.get().unwrap();
                PushDeliveryRepository
                    .create_if_absent(&mut conn, event_id, user)
                    .unwrap()
                    .unwrap()
                    .id
            })
        })
        .collect();
    let ids: Vec<Uuid> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert!(ids.iter().all(|id| *id == ids[0]));
    let count: i64 = fx
        .db
        .client()
        .query_one(
            "SELECT count(*) FROM push_deliveries WHERE event_id = $1 AND user_id = $2",
            &[&event_id, &user],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}

// PushDeliveryRepositoryImplTest#createIfAbsent_missingUser_createsNothing
#[test]
fn 존재하지_않는_회원의_이력은_생성하지_않는다() {
    let Some(fx) = Fx::new() else { return };
    let mut conn = fx.pool.get().unwrap();

    let created = PushDeliveryRepository
        .create_if_absent(&mut conn, Uuid::new_v4(), Uuid::new_v4())
        .unwrap();

    assert!(created.is_none());
}

// PushDeliveryRepositoryImplTest#saveDeviceResult_partialUpdate_preservesOtherDevicesAndCompletedRows
#[test]
fn 기기_json_부분_갱신은_다른_기기의_수락_기록을_보존하고_완료_행은_변경하지_않는다() {
    let Some(fx) = Fx::new() else { return };
    let mut conn = fx.pool.get().unwrap();
    let repository = PushDeliveryRepository;
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let delivery = repository
        .create_if_absent(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    let accepted = delivery
        .start_device(first, now())
        .unwrap()
        .finish(DeviceDeliveryStatus::Accepted, None, now())
        .unwrap();
    let sending = delivery.start_device(second, now()).unwrap();
    assert!(
        repository
            .save_device_result(&mut conn, delivery.id, first, &accepted)
            .unwrap()
    );
    assert!(
        repository
            .save_device_result(&mut conn, delivery.id, second, &sending)
            .unwrap()
    );
    let retryable = sending
        .finish(DeviceDeliveryStatus::Retryable, Some("UNAVAILABLE"), now())
        .unwrap();
    assert!(
        repository
            .save_device_result(&mut conn, delivery.id, second, &retryable)
            .unwrap()
    );

    let saved = repository
        .find_by_event_id_and_user_id(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    assert_eq!(saved.device_results[&first], accepted);
    assert_eq!(saved.device_results[&second], retryable);

    assert!(repository.complete(&mut conn, delivery.id).unwrap());
    // 완료된 행은 더 바꾸지 못한다.
    assert!(
        !repository
            .save_device_result(&mut conn, delivery.id, second, &sending)
            .unwrap()
    );
    assert!(!repository.complete(&mut conn, delivery.id).unwrap());
    let done = repository
        .find_by_event_id_and_user_id(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    assert_eq!(done.device_results[&second], retryable);
}

// PushDeliveryRepositoryImplTest#saveDeviceResult_missingRow_returnsFalse
#[test]
fn 기록이_사라지면_결과_저장과_완료_갱신은_실패를_반환한다() {
    let Some(fx) = Fx::new() else { return };
    let mut conn = fx.pool.get().unwrap();
    let delivery = PushDeliveryRepository
        .create_if_absent(&mut conn, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    let device = Uuid::new_v4();
    let started = delivery.start_device(device, now()).unwrap();
    fx.db
        .client()
        .execute("DELETE FROM push_deliveries WHERE id = $1", &[&delivery.id])
        .unwrap();

    assert!(
        !PushDeliveryRepository
            .save_device_result(&mut conn, delivery.id, device, &started)
            .unwrap()
    );
    assert!(
        !PushDeliveryRepository
            .complete(&mut conn, delivery.id)
            .unwrap()
    );
}

// PushDeliveryRepositoryImplTest#createIfAbsent_rollback_leavesNoRow
#[test]
fn 이력_생성_트랜잭션이_롤백되면_행이_남지_않는다() {
    let Some(fx) = Fx::new() else { return };
    let mut conn = fx.pool.get().unwrap();
    let mut tx = conn.begin().unwrap();
    PushDeliveryRepository
        .create_if_absent(&mut tx, fx.message.event_id, fx.user)
        .unwrap()
        .unwrap();
    tx.rollback().unwrap();

    assert!(fx.find(fx.message.event_id, fx.user).is_none());
}

// PushDeliveryRepositoryImplTest#schema_rejectsInvalidStatusAndJsonShape
#[test]
fn db는_허용하지_않는_상태와_완료_시각_json_형식의_불일치를_거절한다() {
    let Some(fx) = Fx::new() else { return };
    let mut client = fx.db.client();
    for sql in [
        "INSERT INTO push_deliveries (event_id, user_id, status) VALUES (gen_random_uuid(), $1, 'DONE')",
        "INSERT INTO push_deliveries (event_id, user_id, status) VALUES (gen_random_uuid(), $1, 'COMPLETED')",
        "INSERT INTO push_deliveries (event_id, user_id, completed_at) VALUES (gen_random_uuid(), $1, now())",
        "INSERT INTO push_deliveries (event_id, user_id, device_results) VALUES (gen_random_uuid(), $1, '[]'::jsonb)",
    ] {
        assert!(client.execute(sql, &[&fx.user]).is_err(), "{sql}");
    }
}

// 기록 JSON은 Jackson이 쓰는 모양이며 다시 읽을 수 있다.
#[test]
fn 저장한_기기_결과_json은_원본_모양이다() {
    let Some(fx) = Fx::new() else { return };
    let device = Uuid::new_v4();
    fx.send_accepted(&fx.message, device);

    let stored: String = fx
        .db
        .client()
        .query_one(
            "SELECT (device_results -> $1)::text FROM push_deliveries WHERE event_id = $2",
            &[&device.to_string(), &fx.message.event_id],
        )
        .unwrap()
        .get(0);
    let value: Value = serde_json::from_str(&stored).unwrap();

    assert_eq!(value["status"], "ACCEPTED");
    assert_eq!(value["attemptCount"], 1);
    assert_eq!(value["lastErrorCode"], Value::Null);
    assert_eq!(value["sendStartedAt"], NOW);
    assert_eq!(value["acceptedAt"], NOW);
}
