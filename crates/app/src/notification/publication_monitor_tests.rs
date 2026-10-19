//! 발행 대기 모니터 테스트. 원본 `NotificationPublicationMonitorTest`에 대응한다.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use chalkak_core::FixedClock;
use jiff::Timestamp;
use postgres::Client;
use uuid::Uuid;

use super::*;
use crate::notification::domain::{Notification, PublishStatus};
use crate::notification::push_test_support::{Lines, insert_user, logger, plus, ts};
use crate::testing::TestDb;

const NOW: &str = "2026-10-14T00:00:00Z";

struct Fixture {
    db: TestDb,
    monitor: NotificationPublicationMonitor,
    lines: Lines,
    user: Uuid,
}

fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let user = Uuid::new_v4();
    insert_user(&mut db.client(), user);
    let clock = Arc::new(FixedClock::new(ts(NOW)));
    let lines = Lines::default();
    let monitor =
        NotificationPublicationMonitor::new(db.pool(2), clock.clone(), logger(&lines, &clock));
    Some(Fixture {
        db,
        monitor,
        lines,
        user,
    })
}

impl Fixture {
    fn client(&self) -> Client {
        self.db.client()
    }

    fn save(&self, notification: &mut Notification) {
        NotificationRepository
            .save(&mut self.client(), notification)
            .unwrap();
    }

    fn snapshot(&self) -> serde_json::Map<String, serde_json::Value> {
        let events = self.lines.events();
        assert_eq!(events.len(), 1);
        events.into_iter().next().unwrap()
    }
}

// logBacklog_emptyPending_logsZero
#[test]
fn 발행_대기가_없으면_건수와_나이가_0인_정상_관측을_남긴다() {
    let Some(f) = fixture() else { return };

    f.monitor.log_backlog();

    let log = f.snapshot();
    assert_eq!(log["stage"], "publication_snapshot");
    assert_eq!(log["type"], "notification");
    assert_eq!(log["level"], "INFO");
    assert_eq!(log["message"], "알림 발행 대기 상태");
    assert_eq!(log["pendingCount"], 0);
    assert_eq!(log["oldestPendingAgeSeconds"], 0);
}

// logBacklog_mixedStates_countsOnlyPendingIncludingExpired
#[test]
fn pending만_집계하며_재시도_대기와_기한이_지난_미처리_행도_포함한다() {
    let Some(f) = fixture() else { return };
    let now: Timestamp = ts(NOW);
    for status in [
        PublishStatus::NotRequired,
        PublishStatus::Pending,
        PublishStatus::Published,
        PublishStatus::Expired,
        PublishStatus::Failed,
    ] {
        let created = plus(
            now,
            if status == PublishStatus::Pending {
                -2500
            } else {
                -3000
            },
        );
        let mut n = Notification::approved(
            f.user,
            Uuid::new_v4(),
            Uuid::new_v4(),
            created,
            status != PublishStatus::NotRequired,
        );
        match status {
            PublishStatus::Published => n.mark_published(now).unwrap(),
            PublishStatus::Failed => n.fail_publication().unwrap(),
            PublishStatus::Expired => n.expire_publication().unwrap(),
            _ => {}
        }
        f.save(&mut n);
    }
    let mut retrying =
        Notification::approved(f.user, Uuid::new_v4(), Uuid::new_v4(), plus(now, -20), true);
    retrying.retry_publication(now).unwrap();
    f.save(&mut retrying);

    f.monitor.log_backlog();

    let log = f.snapshot();
    assert_eq!(log["pendingCount"], 2);
    assert_eq!(log["oldestPendingAgeSeconds"], 2500);
    let backlog = NotificationRepository
        .get_publication_backlog(&mut f.client())
        .unwrap();
    assert_eq!(backlog.oldest_created_at, Some(plus(now, -2500)));
}

// logBacklog_creationBoundary_logsNonNegativeAge
#[test]
fn 생성_시각_직전_정각_직후에도_대기_나이가_음수가_되지_않는다() {
    for age in [-1i64, 0, 1] {
        let Some(f) = fixture() else { return };
        let mut n = Notification::approved(
            f.user,
            Uuid::new_v4(),
            Uuid::new_v4(),
            plus(ts(NOW), -age),
            true,
        );
        f.save(&mut n);

        f.monitor.log_backlog();

        let log = f.snapshot();
        assert_eq!(log["pendingCount"], 1);
        assert_eq!(log["oldestPendingAgeSeconds"], age.max(0), "{age}");
    }
}

// logBacklog_queryFailure_logsErrorWithoutHealthySnapshot
#[test]
fn 조회_실패는_정상_0건으로_기록하지_않고_예외_원문_없이_오류를_기록한다() {
    let Some(f) = fixture() else { return };
    f.client()
        .batch_execute("DROP TABLE notifications CASCADE")
        .unwrap();

    f.monitor.log_backlog();

    let log = f.snapshot();
    assert_eq!(log["stage"], "publication_snapshot_failed");
    assert_eq!(log["level"], "ERROR");
    assert!(log["errorCode"].is_string());
    assert!(log.get("pendingCount").is_none());
    assert!(log.get("oldestPendingAgeSeconds").is_none());
    assert!(!log["message"].as_str().unwrap().contains("notifications"));
    assert!(log.get("stack_trace").is_none());
}
