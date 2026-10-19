//! Relay 테스트. 원본 `NotificationRelayServiceTest`에 대응한다. 시계는 고정 시계이고 SQS는 가짜 발행기다.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::mpsc;
use std::time::Duration;

use chalkak_core::FixedClock;
use chalkak_db::Pool;
use jiff::Timestamp;
use postgres::Client;

use super::*;
use crate::notification::push_test_support::{
    FakePublisher, Lines, failure, insert_user, logger, plus, ts,
};
use crate::notification::sqs_push::PushPublicationResult;
use crate::testing::TestDb;

const NOW: &str = "2026-10-06T00:00:00Z";

struct Fixture {
    db: TestDb,
    pool: Pool,
    publisher: Arc<FakePublisher>,
    lines: Lines,
    user: Uuid,
}

fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let pool = db.pool(4);
    let user = Uuid::new_v4();
    insert_user(&mut db.client(), user);
    Some(Fixture {
        db,
        pool,
        publisher: FakePublisher::accepting(),
        lines: Lines::default(),
        user,
    })
}

impl Fixture {
    fn relay(&self, now: Timestamp) -> NotificationRelayService {
        let clock = Arc::new(FixedClock::new(now));
        NotificationRelayService::new(
            self.pool.clone(),
            clock.clone(),
            self.publisher.clone(),
            logger(&self.lines, &clock),
        )
    }

    fn run(&self, now: Timestamp) {
        self.relay(now).publish_pending_notifications().unwrap();
    }

    fn client(&self) -> Client {
        self.db.client()
    }

    fn save(&self, occurred_at: Timestamp, push_enabled: bool) -> Uuid {
        let mut notification = Notification::approved(
            self.user,
            Uuid::new_v4(),
            Uuid::new_v4(),
            occurred_at,
            push_enabled,
        );
        NotificationRepository
            .save(&mut self.client(), &mut notification)
            .unwrap();
        notification.id.unwrap()
    }

    fn status(&self, id: Uuid) -> String {
        self.column(id, "sqs_publish_status")
    }

    fn column(&self, id: Uuid, column: &str) -> String {
        self.client()
            .query_one(
                &format!("SELECT {column}::text FROM notifications WHERE id = $1"),
                &[&id],
            )
            .unwrap()
            .get::<_, Option<String>>(0)
            .unwrap_or_default()
    }

    fn time(&self, id: Uuid, column: &str) -> Option<Timestamp> {
        self.client()
            .query_one(
                &format!("SELECT {column} FROM notifications WHERE id = $1"),
                &[&id],
            )
            .unwrap()
            .get(0)
    }

    fn committed_ids(&self) -> Vec<String> {
        self.lines
            .at_stage("relay_transaction_committed")
            .iter()
            .map(|e| e["notificationId"].as_str().unwrap().to_owned())
            .collect()
    }
}

// publishPendingNotifications_committedPending_publishesAndPersistsAcceptance
#[test]
fn 커밋된_발행_대기는_식별_정보만_전달하고_sqs_수락_상태를_저장한다() {
    let Some(f) = fixture() else { return };
    let id = f.save(ts(NOW), true);

    f.run(plus(ts(NOW), 1));

    assert_eq!(f.status(id), "PUBLISHED");
    assert_eq!(f.time(id, "next_attempt_at"), None);
    assert_eq!(f.time(id, "sqs_published_at"), Some(plus(ts(NOW), 1)));
    let calls = f.publisher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].notification_id, Some(id));
    assert_eq!(calls[0].user_id, f.user);
    assert_eq!(calls[0].expires_at, plus(ts(NOW), 1800));
    assert_eq!(calls[0].occurred_at, ts(NOW));
    assert_eq!(f.committed_ids(), vec![id.to_string()]);
}

// publishPendingNotifications_transientFailure_retriesAtOneMinute
#[test]
fn 일시_실패와_수락_불명은_1분_뒤_다시_시도하고_그_전에는_발행하지_않는다() {
    for unknown in [false, true] {
        let Some(f) = fixture() else { return };
        let id = f.save(ts(NOW), true);
        f.publisher.set_fallback(if unknown {
            PushPublicationResult::unknown("InternalError")
        } else {
            PushPublicationResult::retry("RequestThrottled")
        });

        f.run(ts(NOW));

        assert_eq!(f.status(id), "PENDING");
        assert_eq!(f.time(id, "next_attempt_at"), Some(plus(ts(NOW), 60)));
        assert_eq!(f.time(id, "sqs_published_at"), None);
        f.publisher.clear_calls();
        f.run(plus(ts(NOW), 59));
        assert!(f.publisher.calls().is_empty());
        f.publisher
            .set_fallback(PushPublicationResult::accepted("retry-accepted"));
        f.run(plus(ts(NOW), 60));
        assert_eq!(f.status(id), "PUBLISHED");
    }
}

// publishPendingNotifications_permanentFailure_stopsPublication
#[test]
fn 권한_설정_등_영구_오류는_failed로_저장해_반복_발행하지_않는다() {
    let Some(f) = fixture() else { return };
    let id = f.save(ts(NOW), true);
    f.publisher
        .set_fallback(PushPublicationResult::failed("AccessDenied"));

    f.run(ts(NOW));

    assert_eq!(f.status(id), "FAILED");
    assert_eq!(f.time(id, "next_attempt_at"), None);
    assert_eq!(f.time(id, "sqs_published_at"), None);
    f.publisher.clear_calls();
    f.run(plus(ts(NOW), 1));
    assert!(f.publisher.calls().is_empty());
}

// publishPendingNotifications_deadlineBoundary_enforcesOriginalDeadline
#[test]
fn 기한_전에는_발행하고_30분_정각부터는_큐에_보내지_않는다() {
    for seconds in [1799, 1800, 1801] {
        let Some(f) = fixture() else { return };
        let id = f.save(ts(NOW), true);

        f.run(plus(ts(NOW), seconds));

        if seconds < 1800 {
            assert_eq!(f.status(id), "PUBLISHED", "{seconds}");
            continue;
        }
        assert_eq!(f.status(id), "EXPIRED", "{seconds}");
        assert_eq!(f.time(id, "next_attempt_at"), None);
        assert!(f.publisher.calls().is_empty());
        // 만료 처리도 로그를 남긴다(오류 코드 없이 INFO).
        let relay = f.lines.at_stage("relay");
        assert_eq!(relay.len(), 1);
        assert_eq!(relay[0]["result"], "EXPIRED");
        assert_eq!(relay[0]["publishStatus"], "EXPIRED");
        assert_eq!(relay[0]["level"], "INFO");
    }
}

// publishPendingNotifications_ineligibleStates_doesNotPublish
#[test]
fn 발행_불필요_발행_완료_재시도_미도래는_조회에서_제외한다() {
    let Some(f) = fixture() else { return };
    let off = f.save(ts(NOW), false);
    let published = f.save(ts(NOW), true);
    let future = f.save(ts(NOW), true);
    let failed = f.save(ts(NOW), true);
    let expired = f.save(ts(NOW), true);
    let mut c = f.client();
    c.execute(
        "UPDATE notifications SET sqs_publish_status = 'PUBLISHED', next_attempt_at = NULL, \
         sqs_published_at = $1 WHERE id = $2",
        &[&ts(NOW), &published],
    )
    .unwrap();
    c.execute(
        "UPDATE notifications SET next_attempt_at = $1 WHERE id = $2",
        &[&plus(ts(NOW), 1), &future],
    )
    .unwrap();
    c.execute(
        "UPDATE notifications SET sqs_publish_status = 'FAILED', next_attempt_at = NULL WHERE id = $1",
        &[&failed],
    )
    .unwrap();
    c.execute(
        "UPDATE notifications SET sqs_publish_status = 'EXPIRED', next_attempt_at = NULL WHERE id = $1",
        &[&expired],
    )
    .unwrap();

    f.run(ts(NOW));

    assert!(f.publisher.calls().is_empty());
    assert_eq!(f.status(off), "NOT_REQUIRED");
    assert_eq!(f.status(future), "PENDING");
}

// publishPendingNotifications_uncommittedNotification_doesNotPublish
#[test]
fn 업무_트랜잭션에서_아직_커밋하지_않은_알림은_외부에_발행하지_않는다() {
    let Some(f) = fixture() else { return };
    let mut writer = f.client();
    let mut tx = writer.transaction().unwrap();
    let mut notification =
        Notification::approved(f.user, Uuid::new_v4(), Uuid::new_v4(), ts(NOW), true);
    NotificationRepository
        .save(&mut tx, &mut notification)
        .unwrap();

    f.run(ts(NOW));

    tx.rollback().unwrap();
    assert!(f.publisher.calls().is_empty());
    let count: i64 = f
        .client()
        .query_one("SELECT COUNT(*) FROM notifications", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

// publishPendingNotifications_databaseFailure_isolatesRollbackAndReusesEvent
#[test]
fn 한_작업의_db_반영_실패는_다른_작업을_롤백하지_않고_다음_실행에서_동일_사건을_재발행한다() {
    let Some(f) = fixture() else { return };
    let failed_id = f.save(ts(NOW), true);
    let success_id = f.save(plus(ts(NOW), 1), true);
    let mut c = f.client();
    c.batch_execute(&format!(
        "CREATE FUNCTION relay_reject_update() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF OLD.id = '{failed_id}'::uuid THEN RAISE EXCEPTION 'relay persistence failure'; END IF; \
         RETURN NEW; END $$; \
         CREATE TRIGGER relay_reject_update BEFORE UPDATE ON notifications \
         FOR EACH ROW EXECUTE FUNCTION relay_reject_update()"
    ))
    .unwrap();

    f.run(plus(ts(NOW), 1));

    assert_eq!(f.status(failed_id), "PENDING");
    assert_eq!(f.time(failed_id, "sqs_published_at"), None);
    assert_eq!(f.status(success_id), "PUBLISHED");
    assert_eq!(f.committed_ids(), vec![success_id.to_string()]);
    let errors = f.lines.at_stage("relay_transaction_failed");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["notificationId"], failed_id.to_string());
    assert_eq!(errors[0]["level"], "ERROR");
    assert!(errors[0].get("errorCode").is_some());
    c.batch_execute("DROP TRIGGER relay_reject_update ON notifications")
        .unwrap();
    f.run(plus(ts(NOW), 2));
    let calls = f.publisher.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls.first().unwrap().event_id,
        calls.last().unwrap().event_id
    );
    assert_eq!(f.status(failed_id), "PUBLISHED");
}

#[test]
fn 발행기가_결과를_만들지_못하면_롤백하고_예외_이름을_오류_코드로_남긴다() {
    let Some(f) = fixture() else { return };
    let id = f.save(ts(NOW), true);
    f.publisher
        .queue(vec![Err(failure("IllegalStateException"))]);

    f.run(ts(NOW));

    assert_eq!(f.status(id), "PENDING");
    assert_eq!(f.time(id, "next_attempt_at"), Some(ts(NOW)));
    let errors = f.lines.at_stage("relay_transaction_failed");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["errorCode"], "IllegalStateException");
    assert!(f.committed_ids().is_empty());
}

// publishPendingNotifications_concurrentRelays_publishesOnlyOnce
#[test]
fn 같은_알림을_동시에_조회해도_잠긴_행은_기다리지_않고_건너뛴다() {
    let Some(f) = fixture() else { return };
    let id = f.save(ts(NOW), true);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let started_tx = std::sync::Mutex::new(started_tx);
    let release_rx = std::sync::Mutex::new(release_rx);
    f.publisher.on_publish(move |_| {
        let _ = started_tx.lock().unwrap().send(());
        // 첫 Relay가 SQS 응답을 기다리는 동안 행 잠금을 쥐고 있게 한다.
        assert!(
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .is_ok(),
            "test publication latch timed out"
        );
    });

    let first = {
        let relay = f.relay(ts(NOW));
        std::thread::spawn(move || relay.publish_pending_notifications().unwrap())
    };
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // 둘째 Relay는 잠긴 행을 건너뛰어 기다리지 않고 끝난다.
    let second = {
        let relay = f.relay(ts(NOW));
        std::thread::spawn(move || relay.publish_pending_notifications().unwrap())
    };
    let started = std::time::Instant::now();
    while !second.is_finished() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "둘째 Relay가 잠금을 기다렸다"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    second.join().unwrap();
    assert!(f.committed_ids().is_empty());
    release_tx.send(()).unwrap();
    first.join().unwrap();

    assert_eq!(f.status(id), "PUBLISHED");
    assert_eq!(f.publisher.calls().len(), 1);
    assert_eq!(f.committed_ids(), vec![id.to_string()]);
}

// findDuePublicationIds_orderAndLimit_returnsEarliestDueRows
#[test]
fn 대기_작업은_다음_시도_시각_순으로_정해진_크기만_조회한다() {
    let Some(f) = fixture() else { return };
    let later = f.save(plus(ts(NOW), 1), true);
    let earlier = f.save(ts(NOW), true);
    f.save(plus(ts(NOW), 2), true);

    let ids = NotificationRepository
        .find_due_publication_ids(&mut f.client(), plus(ts(NOW), 2), 2)
        .unwrap();

    assert_eq!(ids, vec![earlier, later]);
}

#[test]
fn 한_번에_100건까지만_처리한다() {
    let Some(f) = fixture() else { return };
    for i in 0..101 {
        f.save(plus(ts(NOW), i % 3), true);
    }

    f.run(plus(ts(NOW), 3));

    assert_eq!(f.publisher.calls().len(), 100);
    let pending: i64 = f
        .client()
        .query_one(
            "SELECT COUNT(*) FROM notifications WHERE sqs_publish_status = 'PENDING'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(pending, 1);
}

// publishPendingNotifications_retryableResult_distinguishesAcceptance
#[test]
fn 발행_거절과_수락_불명을_구분하고_db_재시도_상태와_식별자를_함께_기록한다() {
    for (unknown, result) in [(false, "SQS_FAILED"), (true, "SQS_UNKNOWN")] {
        let Some(f) = fixture() else { return };
        let id = f.save(ts(NOW), true);
        f.publisher.set_fallback(if unknown {
            PushPublicationResult::unknown("SQS_CLIENT_ERROR")
        } else {
            PushPublicationResult::retry("RequestThrottled")
        });

        f.run(ts(NOW));

        assert_eq!(f.status(id), "PENDING");
        let logs: Vec<_> = f
            .lines
            .at_stage("relay")
            .into_iter()
            .filter(|e| e["result"] == result)
            .collect();
        assert_eq!(logs.len(), 1, "{result}");
        let log = &logs[0];
        assert_eq!(log["notificationId"], id.to_string());
        assert_eq!(log["userId"], f.user.to_string());
        assert_eq!(log["publishStatus"], "PENDING");
        assert_eq!(log["nextAttemptAt"], "2026-10-06T00:01:00Z");
        assert!(log.get("eventId").is_some());
        assert_eq!(log["level"], "WARN");
    }
}

/// 운영 문서가 정한 로그 필드(`notification-sqs-setup.md`)와 순서를 지킨다.
#[test]
fn 수락_로그는_문서의_필드를_순서대로_담는다() {
    let Some(f) = fixture() else { return };
    let id = f.save(ts(NOW), true);

    f.run(plus(ts(NOW), 1));

    let relay = f.lines.at_stage("relay");
    assert_eq!(relay.len(), 2);
    assert_eq!(relay[0]["result"], "REQUESTED");
    let accepted = &relay[1];
    assert_eq!(accepted["result"], "SQS_ACCEPTED");
    assert_eq!(accepted["level"], "INFO");
    assert_eq!(accepted["sqsMessageId"], "sqs-message-id");
    assert_eq!(accepted["publishStatus"], "PUBLISHED");
    assert_eq!(accepted["notificationType"], "POST_APPROVED");
    assert_eq!(accepted["occurredAt"], NOW);
    assert_eq!(accepted["expiresAt"], "2026-10-06T00:30:00Z");
    assert_eq!(accepted["message"], "알림 SQS 발행 처리");
    assert_eq!(accepted["type"], "notification");
    assert_eq!(accepted["notificationId"], id.to_string());
    assert!(accepted["errorCode"].is_null());
    assert!(accepted["nextAttemptAt"].is_null());
    // 문서에 적힌 순서대로 키가 나온다(JSON 줄에서 위치를 비교한다).
    let line = f
        .lines
        .raw()
        .into_iter()
        .find(|l| l.contains("\"result\":\"SQS_ACCEPTED\""))
        .unwrap();
    let mut last = 0;
    for key in [
        "type",
        "stage",
        "eventId",
        "notificationId",
        "userId",
        "notificationType",
        "sourceId",
        "occurredAt",
        "expiresAt",
        "publishStatus",
        "sqsMessageId",
        "result",
        "errorCode",
        "nextAttemptAt",
    ] {
        let at = line
            .find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("{key}"));
        assert!(at > last, "{key} 위치가 앞선다");
        last = at;
    }
}
