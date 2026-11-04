//! 리마인더 푸시 발행 테스트. 원본 `ReminderPushServiceTest`에 대응한다. 시계는 고정 시계이고 SQS는 가짜 발행기다.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chalkak_core::FixedClock;
use postgres::Client;
use serde_json::{Map, Value};

use super::*;
use crate::notification::push_test_support::{FakePublisher, Lines, insert_user, logger, plus, ts};
use crate::notification::sqs_push::PushMessage;
use crate::testing::TestDb;

const SCHEDULED_AT: &str = "2026-10-22T12:00:00Z";
const OPEN_ENDS_AT: &str = "2026-10-22T15:00:00Z";
const CLOSED_ENDS_AT: &str = "2026-10-22T11:00:00Z";

fn user(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

struct Fixture {
    db: TestDb,
    pool: Pool,
    clock: Arc<FixedClock>,
    lines: Lines,
    publisher: Arc<FakePublisher>,
}

fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let pool = db.pool(4);
    insert_user(&mut db.client(), user(1));
    Some(Fixture {
        db,
        pool,
        clock: Arc::new(FixedClock::new(plus(ts(SCHEDULED_AT), 10))),
        lines: Lines::default(),
        publisher: FakePublisher::accepting(),
    })
}

impl Fixture {
    fn service(&self) -> ReminderPushService {
        ReminderPushService::new(
            self.pool.clone(),
            self.clock.clone(),
            self.publisher.clone(),
            logger(&self.lines, &self.clock),
        )
    }

    fn client(&self) -> Client {
        self.db.client()
    }

    fn messages(&self) -> Vec<PushMessage> {
        self.publisher.calls()
    }

    fn at_stage(&self, stage: &str) -> Vec<Map<String, Value>> {
        self.lines.at_stage(stage)
    }

    fn insert_reminder(&self, repeat: &str, target: &str) -> Uuid {
        self.client()
            .query_one(
                "INSERT INTO reminders (title, body, repeat, target, scheduled_at, created_at, updated_at) \
                 VALUES ('오늘이 얼마 남지 않았어요', '사진을 자랑해 주세요!', $1, $2, $3, \
                 '2026-10-21T00:00:00Z', '2026-10-21T00:00:00Z') RETURNING id",
                &[&repeat, &target, &ts(SCHEDULED_AT)],
            )
            .unwrap()
            .get(0)
    }

    /// 회차 날짜(KST 2026-10-22)의 주제. 시작은 KST 자정이고 마감 시각만 바꿔 열림·마감 상태를 만든다.
    fn insert_topic(&self, ends_at: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.client()
            .execute(
                "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) \
                 VALUES ($1, '빛과 그림자', '2026-10-22', $2, $3)",
                &[&id, &ts("2026-10-21T15:00:00Z"), &ts(ends_at)],
            )
            .unwrap();
        id
    }

    fn insert_approved_post(&self, user: Uuid, topic: Uuid) {
        let photo = Uuid::new_v4();
        let mut client = self.client();
        client
            .execute(
                "INSERT INTO photos (id, original_storage_key, thumbnail_storage_key) \
                 VALUES ($1, 'original/p', 'thumbnail/p')",
                &[&photo],
            )
            .unwrap();
        client
            .execute(
                "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status) \
                 VALUES ($1, $2, $3, $4, 'APPROVED')",
                &[&Uuid::new_v4(), &user, &topic, &photo],
            )
            .unwrap();
    }
}

fn user_ids(messages: &[PushMessage]) -> Vec<Uuid> {
    messages.iter().map(|m| m.user_id).collect()
}

#[test]
fn 전체_대상은_정상이면서_주제_알림을_켠_회원에게만_회차_값으로_발행하고_집계를_남긴다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    f.client()
        .execute(
            "UPDATE users SET topic_push_enabled = false WHERE id = $1",
            &[&user(2)],
        )
        .unwrap();
    insert_user(&mut f.client(), user(3));
    f.client()
        .execute(
            "UPDATE users SET status = 'BANNED' WHERE id = $1",
            &[&user(3)],
        )
        .unwrap();
    insert_user(&mut f.client(), user(4));
    f.client()
        .execute(
            "UPDATE users SET deleted_at = $2 WHERE id = $1",
            &[&user(4), &ts(SCHEDULED_AT)],
        )
        .unwrap();
    let reminder_id = f.insert_reminder("ONCE", "ALL");

    f.service().publish_due_reminders().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    assert_eq!(message.user_id, user(1));
    assert_eq!(message.notification_id, None);
    assert_eq!(message.kind, NotificationType::Reminder);
    assert_eq!(message.source_type, Some(NotificationSourceType::Reminder));
    assert_eq!(message.source_id, Some(reminder_id));
    assert_eq!(message.occurred_at, ts(SCHEDULED_AT));
    assert_eq!(message.expires_at, plus(ts(SCHEDULED_AT), 1800));
    let summaries = f.at_stage("reminder_publication_summary");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["reminderId"], reminder_id.to_string());
    assert_eq!(summaries[0]["eventId"], message.event_id.to_string());
    assert_eq!(summaries[0]["targetCount"], 1);
    assert_eq!(summaries[0]["acceptedCount"], 1);
    assert_eq!(summaries[0]["message"], "리마인더 SQS 발행 집계");
}

#[test]
fn 같은_회차는_매_분_다시_확인해도_한_번만_발행한다() {
    let Some(f) = fixture() else { return };
    f.insert_reminder("DAILY", "ALL");
    let service = f.service();
    service.publish_due_reminders().unwrap();
    f.clock.set(plus(ts(SCHEDULED_AT), 70));
    service.publish_due_reminders().unwrap();
    assert_eq!(f.messages().len(), 1);
}

#[test]
fn 서비스를_다시_만들어_발행해도_같은_회차는_같은_사건_id를_사용한다() {
    for repeat in ["ONCE", "DAILY"] {
        let Some(f) = fixture() else { return };
        f.insert_reminder(repeat, "ALL");
        f.service().publish_due_reminders().unwrap();
        f.clock.set(plus(ts(SCHEDULED_AT), 70));
        f.service().publish_due_reminders().unwrap();
        let messages = f.messages();
        assert_eq!(messages.len(), 2, "{repeat}");
        assert_eq!(messages[1], messages[0], "{repeat}");
    }
}

#[test]
fn 같은_매일_반복도_다음_날_회차는_다른_사건_id를_사용한다() {
    let Some(f) = fixture() else { return };
    f.insert_reminder("DAILY", "ALL");
    let service = f.service();
    service.publish_due_reminders().unwrap();
    f.clock.set(plus(ts(SCHEDULED_AT), 86_410));
    service.publish_due_reminders().unwrap();
    let messages = f.messages();
    assert_eq!(messages.len(), 2);
    assert_ne!(messages[0].event_id, messages[1].event_id);
    assert_eq!(messages[1].source_id, messages[0].source_id);
    assert_eq!(messages[1].occurred_at, plus(ts(SCHEDULED_AT), 86_400));
    assert_eq!(messages[1].expires_at, plus(ts(SCHEDULED_AT), 88_200));
}

#[test]
fn 다른_요청이_행을_잠그고_있으면_이번_확인은_건너뛰고_잠금이_풀린_뒤_다음_확인에_발행한다() {
    let Some(f) = fixture() else { return };
    let reminder_id = f.insert_reminder("ONCE", "ALL");
    let service = f.service();
    let mut holder = f.client();
    let mut tx = holder.transaction().unwrap();
    tx.query_one(
        "SELECT id FROM reminders WHERE id = $1 FOR UPDATE",
        &[&reminder_id],
    )
    .unwrap();

    service.publish_due_reminders().unwrap();
    tx.commit().unwrap();

    assert!(f.messages().is_empty());
    f.clock.set(plus(ts(SCHEDULED_AT), 70));
    service.publish_due_reminders().unwrap();
    assert_eq!(f.messages().len(), 1);
    let skipped = f.at_stage("reminder_execution_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["reason"], "REMINDER_LOCKED_OR_DELETED");
}

#[test]
fn 목록을_읽은_뒤_발행_전에_삭제되거나_일정이_바뀐_리마인더는_발행하지_않는다() {
    let Some(f) = fixture() else { return };
    let deleted = f.insert_reminder("ONCE", "ALL");
    let rescheduled = f.insert_reminder("ONCE", "ALL");
    let service = f.service();
    let listed = service.find_active_reminders().unwrap();
    f.client()
        .execute(
            "UPDATE reminders SET deleted_at = $2 WHERE id = $1",
            &[&deleted, &ts(SCHEDULED_AT)],
        )
        .unwrap();
    f.client()
        .execute(
            "UPDATE reminders SET scheduled_at = $2 WHERE id = $1",
            &[&rescheduled, &plus(ts(SCHEDULED_AT), 3600)],
        )
        .unwrap();

    service.publish_listed(&listed);

    assert!(f.messages().is_empty());
    let mut reasons: Vec<String> = f
        .at_stage("reminder_execution_skipped")
        .iter()
        .map(|log| log["reason"].as_str().unwrap().to_owned())
        .collect();
    reasons.sort();
    assert_eq!(reasons, ["REMINDER_LOCKED_OR_DELETED", "SCHEDULE_CHANGED"]);
}

#[test]
fn 오늘_주제_참여자_대상은_올린_회원에게만_발행한다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    let topic = f.insert_topic(OPEN_ENDS_AT);
    f.insert_approved_post(user(1), topic);
    f.insert_reminder("ONCE", "TOPIC_PARTICIPANTS");
    f.service().publish_due_reminders().unwrap();
    assert_eq!(user_ids(&f.messages()), [user(1)]);
}

#[test]
fn 오늘_주제_미참여자_대상은_아직_올리지_않은_회원에게만_발행한다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    let topic = f.insert_topic(OPEN_ENDS_AT);
    f.insert_approved_post(user(1), topic);
    f.insert_reminder("ONCE", "TOPIC_NON_PARTICIPANTS");
    f.service().publish_due_reminders().unwrap();
    assert_eq!(user_ids(&f.messages()), [user(2)]);
}

#[test]
fn 주제가_마감된_뒤에도_미참여자와_참여자_대상은_발행한다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    let topic = f.insert_topic(CLOSED_ENDS_AT);
    f.insert_approved_post(user(1), topic);
    f.insert_reminder("ONCE", "TOPIC_NON_PARTICIPANTS");
    f.insert_reminder("ONCE", "TOPIC_PARTICIPANTS");
    f.service().publish_due_reminders().unwrap();
    let mut recipients = user_ids(&f.messages());
    recipients.sort();
    assert_eq!(recipients, [user(1), user(2)]);
}

#[test]
fn 오늘_주제가_없으면_주제_대상은_발행하지_않고_한_번만_생략을_남긴다() {
    for target in ["TOPIC_PARTICIPANTS", "TOPIC_NON_PARTICIPANTS"] {
        let Some(f) = fixture() else { return };
        f.insert_reminder("ONCE", target);
        let service = f.service();
        service.publish_due_reminders().unwrap();
        f.clock.set(plus(ts(SCHEDULED_AT), 70));
        service.publish_due_reminders().unwrap();
        assert!(f.messages().is_empty());
        let skipped = f.at_stage("reminder_execution_skipped");
        assert_eq!(skipped.len(), 1, "{target}");
        assert_eq!(skipped[0]["reason"], "NO_TOPIC");
    }
}

#[test]
fn 자정을_넘겨_발행한_일회성_회차도_회차_날짜의_주제로_대상을_고른다() {
    let Some(f) = fixture() else { return };
    let scheduled_at = ts("2026-10-22T14:59:30Z");
    let topic = f.insert_topic(OPEN_ENDS_AT);
    f.insert_approved_post(user(1), topic);
    let reminder = f.insert_reminder("ONCE", "TOPIC_PARTICIPANTS");
    f.client()
        .execute(
            "UPDATE reminders SET scheduled_at = $2 WHERE id = $1",
            &[&reminder, &scheduled_at],
        )
        .unwrap();
    f.clock.set(ts("2026-10-22T15:00:00Z"));
    f.service().publish_due_reminders().unwrap();
    let messages = f.messages();
    assert_eq!(user_ids(&messages), [user(1)]);
    assert_eq!(messages[0].occurred_at, scheduled_at);
}

#[test]
fn 예정_시각부터_30분이_지난_회차는_발행하지_않는다() {
    let Some(f) = fixture() else { return };
    f.insert_reminder("ONCE", "ALL");
    f.clock.set(plus(ts(SCHEDULED_AT), 1800));
    f.service().publish_due_reminders().unwrap();
    assert!(f.messages().is_empty());
}

#[test]
fn 삭제된_리마인더의_시도_기록을_지워_같은_id가_다시_나타나도_회차를_새로_판단한다() {
    let Some(f) = fixture() else { return };
    let reminder = f.insert_reminder("DAILY", "ALL");
    let service = f.service();
    service.publish_due_reminders().unwrap();
    f.client()
        .execute(
            "UPDATE reminders SET deleted_at = $2 WHERE id = $1",
            &[&reminder, &ts(SCHEDULED_AT)],
        )
        .unwrap();
    f.clock.set(plus(ts(SCHEDULED_AT), 70));
    service.publish_due_reminders().unwrap();

    f.client()
        .execute(
            "UPDATE reminders SET deleted_at = NULL WHERE id = $1",
            &[&reminder],
        )
        .unwrap();
    f.clock.set(plus(ts(SCHEDULED_AT), 130));
    service.publish_due_reminders().unwrap();

    assert_eq!(f.messages().len(), 2);
}

#[test]
fn 발행_중_패닉해도_같은_인스턴스는_같은_회차를_다시_발행하지_않는다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    f.insert_reminder("ONCE", "ALL");
    let fired = Arc::new(AtomicBool::new(false));
    let flag = fired.clone();
    f.publisher.on_publish(move |_| {
        assert!(flag.swap(true, Ordering::SeqCst), "simulated interruption");
    });
    let service = f.service();
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service.publish_due_reminders()
    }));
    assert!(interrupted.is_err());
    f.publisher.on_publish(|_| {});
    f.clock.set(plus(ts(SCHEDULED_AT), 70));
    service.publish_due_reminders().unwrap();
    assert_eq!(f.messages().len(), 1);
    let _ = Duration::ZERO;
}
