//! 리마인더 푸시의 Worker 업무 처리 DB 통합 테스트(원본 `PushWorkerReminderTest`). `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::{Arc, Mutex};

use chalkak_app::notification::domain::{NotificationSourceType, NotificationType};
use chalkak_app::notification::push_device::FcmToken;
use chalkak_app::notification::push_message::RawPushMessage;
use chalkak_app::notification::push_sender::{
    DevicePushRequest, DevicePushResult, DevicePushSender, DevicePushTiming, SenderFailure,
};
use chalkak_app::notification::push_worker::PushWorkerService;
use chalkak_app::testing::TestDb;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, FixedClock};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};
use uuid::Uuid;

const TITLE: &str = "오늘이 얼마 남지 않았어요";
const BODY: &str = "사진을 자랑해 주세요!";

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn plus(at: Timestamp, seconds: i64) -> Timestamp {
    at.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

/// KST 2026-10-22 21:00
fn occurred_at() -> Timestamp {
    ts("2026-10-22T12:00:00Z")
}

fn now() -> Timestamp {
    plus(occurred_at(), 60)
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
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter_map(|value| value.as_object().cloned())
            .filter(|event| event.get("stage").and_then(Value::as_str) == Some(stage))
            .collect()
    }
}

#[derive(Default)]
struct FakeSender {
    requests: Mutex<Vec<DevicePushRequest>>,
}

impl DevicePushSender for FakeSender {
    fn send(
        &self,
        request: &DevicePushRequest,
        _timing: &mut DevicePushTiming,
    ) -> Result<DevicePushResult, SenderFailure> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(DevicePushResult::accepted())
    }
}

struct Fx {
    db: TestDb,
    sender: Arc<FakeSender>,
    lines: Lines,
    user: Uuid,
}

fn fx() -> Option<Fx> {
    let db = TestDb::create()?;
    let user = Uuid::new_v4();
    let mut c = db.client();
    c.execute(
        "INSERT INTO users (id, email, status, signature_original_storage_key) \
         VALUES ($1, 'reminder-worker@example.com', 'ACTIVE', 'reminder/signature')",
        &[&user],
    )
    .unwrap();
    let session = Uuid::new_v4();
    c.execute(
        "INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
        &[
            &user,
            &session,
            &FcmToken::new(&Uuid::new_v4().to_string()).unwrap().hash(),
            &plus(now(), 3600),
            &plus(now(), 7200),
        ],
    )
    .unwrap();
    c.execute(
        "INSERT INTO push_devices (user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at) \
         VALUES ($1, $2, 'reminder-token', $3, $4, $4)",
        &[
            &user,
            &session,
            &FcmToken::new("reminder-token").unwrap().hash(),
            &plus(occurred_at(), -60),
        ],
    )
    .unwrap();
    drop(c);
    Some(Fx {
        db,
        sender: Arc::new(FakeSender::default()),
        lines: Lines::default(),
        user,
    })
}

impl Fx {
    fn worker(&self) -> PushWorkerService {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(now()));
        let logger = Arc::new(Logger::new(
            Box::new(self.lines.clone()),
            Level::Info,
            clock.clone(),
        ));
        PushWorkerService::new(self.db.pool(4), clock, self.sender.clone(), logger)
    }

    fn message(&self, reminder: Uuid) -> RawPushMessage {
        RawPushMessage {
            event_id: Some(Uuid::new_v4()),
            notification_id: None,
            user_id: Some(self.user),
            kind: Some(NotificationType::Reminder),
            source_type: Some(NotificationSourceType::Reminder),
            source_id: Some(reminder),
            occurred_at: Some(occurred_at()),
            expires_at: Some(plus(occurred_at(), 1800)),
        }
    }

    fn insert_reminder(&self, target: &str) -> Uuid {
        self.db
            .client()
            .query_one(
                "INSERT INTO reminders (title, body, repeat, target, scheduled_at, created_at, updated_at) \
                 VALUES ($1, $2, 'ONCE', $3, $4, '2026-10-21T00:00:00Z', '2026-10-21T00:00:00Z') RETURNING id",
                &[&TITLE, &BODY, &target, &occurred_at()],
            )
            .unwrap()
            .get(0)
    }

    /// 회차 날짜(KST 2026-10-22)의 주제.
    fn insert_topic(&self) -> Uuid {
        let id = Uuid::new_v4();
        self.db
            .client()
            .execute(
                "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) \
                 VALUES ($1, '빛과 그림자', '2026-10-22', $2, $3)",
                &[
                    &id,
                    &ts("2026-10-21T15:00:00Z"),
                    &ts("2026-10-22T15:00:00Z"),
                ],
            )
            .unwrap();
        id
    }

    fn insert_post(&self, topic: Uuid, status: &str, deleted: bool) {
        let photo = Uuid::new_v4();
        let mut c = self.db.client();
        c.execute(
            "INSERT INTO photos (id, original_storage_key) VALUES ($1, $2)",
            &[&photo, &format!("reminder/photo/{photo}")],
        )
        .unwrap();
        let deleted_at = deleted.then(|| plus(occurred_at(), 30));
        c.execute(
            "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status, deleted_at, created_at) \
             VALUES ($1, $2, $3, $4, $5::text::moderation_status, $6, $7)",
            &[
                &Uuid::new_v4(),
                &self.user,
                &topic,
                &photo,
                &status,
                &deleted_at,
                &plus(occurred_at(), 10),
            ],
        )
        .unwrap();
    }

    fn sent(&self) -> Vec<DevicePushRequest> {
        self.sender.requests.lock().unwrap().clone()
    }

    fn assert_skipped(&self, message: &RawPushMessage, reason: &str) {
        let before = self.sent().len();
        let result = self.worker().process_message(message).unwrap();
        assert!(!result.retryable);
        assert_eq!(self.sent().len(), before, "{reason}");
        let skipped = self.lines.at_stage("worker_skipped");
        assert_eq!(
            skipped.last().map(|log| log["reason"].clone()),
            Some(Value::String(reason.to_owned())),
            "{reason}"
        );
    }
}

#[test]
fn 전체_대상_리마인더는_리마인더의_제목_본문으로_현재_기기에_보내고_알림함에_저장하지_않는다() {
    let Some(fx) = fx() else { return };
    let message = fx.message(fx.insert_reminder("ALL"));
    let result = fx.worker().process_message(&message).unwrap();
    assert!(!result.retryable);
    let sent = fx.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, TITLE);
    assert_eq!(sent[0].body, BODY);
    assert_eq!(sent[0].token, "reminder-token");
    assert_eq!(sent[0].expires_at, plus(occurred_at(), 1800));
    assert_eq!(sent[0].message.kind, NotificationType::Reminder);
    let count: i64 = fx
        .db
        .client()
        .query_one("SELECT COUNT(*) FROM notifications", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[test]
fn 발행_뒤_고친_문구는_발송_직전의_문구로_보낸다() {
    let Some(fx) = fx() else { return };
    let reminder = fx.insert_reminder("ALL");
    fx.db
        .client()
        .execute(
            "UPDATE reminders SET title = '고친 제목', body = '고친 본문' WHERE id = $1",
            &[&reminder],
        )
        .unwrap();
    fx.worker().process_message(&fx.message(reminder)).unwrap();
    let sent = fx.sent();
    assert_eq!(sent[0].title, "고친 제목");
    assert_eq!(sent[0].body, "고친 본문");
}

#[test]
fn 주제_알림_수신을_끈_회원과_정지_회원과_삭제된_리마인더는_보내지_않는다() {
    let Some(fx) = fx() else { return };
    let reminder = fx.insert_reminder("ALL");
    let message = fx.message(reminder);
    fx.db
        .client()
        .execute(
            "UPDATE reminders SET deleted_at = $2 WHERE id = $1",
            &[&reminder, &occurred_at()],
        )
        .unwrap();
    fx.assert_skipped(&message, "REMINDER_UNAVAILABLE");
    fx.db
        .client()
        .execute("UPDATE reminders SET deleted_at = NULL", &[])
        .unwrap();
    fx.db
        .client()
        .execute("UPDATE users SET topic_push_enabled = false", &[])
        .unwrap();
    fx.assert_skipped(&message, "PUSH_DISABLED");
    fx.db
        .client()
        .execute(
            "UPDATE users SET topic_push_enabled = true, status = 'BANNED'",
            &[],
        )
        .unwrap();
    fx.assert_skipped(&message, "USER_INACTIVE");
}

#[test]
fn 참여자_대상은_오늘_주제에_올린_회원에게_보내고_올리지_않은_회원은_생략한다() {
    let Some(fx) = fx() else { return };
    let topic = fx.insert_topic();
    let message = fx.message(fx.insert_reminder("TOPIC_PARTICIPANTS"));
    fx.assert_skipped(&message, "NOT_PARTICIPATED");
    fx.insert_post(topic, "APPROVED", false);
    fx.worker().process_message(&message).unwrap();
    assert_eq!(fx.sent().len(), 1);
}

#[test]
fn 미참여자_대상은_그_사이_올렸으면_생략하고_반려_삭제만_있으면_보낸다() {
    for (status, deleted, sendable) in [
        ("APPROVED", false, false),
        ("PENDING", false, false),
        ("VALIDATING", false, false),
        ("REJECTED", false, true),
        ("APPROVED", true, true),
    ] {
        let Some(fx) = fx() else { return };
        let topic = fx.insert_topic();
        fx.insert_post(topic, status, deleted);
        let message = fx.message(fx.insert_reminder("TOPIC_NON_PARTICIPANTS"));
        if sendable {
            fx.worker().process_message(&message).unwrap();
            assert_eq!(fx.sent().len(), 1, "{status} {deleted}");
        } else {
            fx.assert_skipped(&message, "ALREADY_PARTICIPATED");
        }
    }
}

#[test]
fn 주제_대상인데_회차_날짜의_주제가_없으면_보내지_않는다() {
    for target in ["TOPIC_PARTICIPANTS", "TOPIC_NON_PARTICIPANTS"] {
        let Some(fx) = fx() else { return };
        let message = fx.message(fx.insert_reminder(target));
        fx.assert_skipped(&message, "TOPIC_UNAVAILABLE");
    }
}

#[test]
fn 미참여자_대상은_오늘_주제의_참여_시간이_끝난_뒤에도_보낸다() {
    let Some(fx) = fx() else { return };
    let topic = fx.insert_topic();
    fx.db
        .client()
        .execute(
            "UPDATE topics SET ends_at = $2 WHERE id = $1",
            &[&topic, &plus(occurred_at(), -3600)],
        )
        .unwrap();
    let message = fx.message(fx.insert_reminder("TOPIC_NON_PARTICIPANTS"));
    fx.worker().process_message(&message).unwrap();
    assert_eq!(fx.sent().len(), 1);
}

#[test]
fn 알림_id_다른_대상_종류_30분이_아닌_기한_미래_예정_시각이_있는_메시지는_보내지_않는다() {
    for invalid in ["notificationId", "sourceType", "deadline", "future"] {
        let Some(fx) = fx() else { return };
        let mut message = fx.message(fx.insert_reminder("ALL"));
        match invalid {
            "notificationId" => message.notification_id = Some(Uuid::new_v4()),
            "sourceType" => message.source_type = Some(NotificationSourceType::Topic),
            "deadline" => message.expires_at = Some(plus(occurred_at(), 3600)),
            _ => {
                message.occurred_at = Some(plus(now(), 60));
                message.expires_at = Some(plus(now(), 60 + 1800));
            }
        }
        fx.assert_skipped(&message, "INVALID_MESSAGE");
    }
}
