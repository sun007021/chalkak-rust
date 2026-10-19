//! 푸시 Worker 업무 처리(`PushWorkerService`)의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PushWorkerServiceTest`(승인 알림), `PushWorkerTopicTest`(주제), `PushWorkerNoticeTest`(공지)를 옮겼다.
//! 원본의 Mockito `DevicePushSender`는 순서 목록을 쓰고 마지막 값을 반복하는 가짜 발송기로 대신한다.
//! 원본은 `PushMessage` 레코드를 그대로 넘기지만 Rust의 Worker는 SQS 본문을 읽은 `RawPushMessage`를 받으므로, 같은 값을 담아 넘긴다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalkak_app::common::transact;
use chalkak_app::notification::domain::{Notification, NotificationSourceType, NotificationType};
use chalkak_app::notification::notice::{Notice, NoticeRepository};
use chalkak_app::notification::push_device::FcmToken;
use chalkak_app::notification::push_message::RawPushMessage;
use chalkak_app::notification::push_sender::{
    DevicePushRequest, DevicePushResult, DevicePushSender, PushProcessingResult, SenderFailure,
};
use chalkak_app::notification::push_worker::PushWorkerService;
use chalkak_app::notification::repository::NotificationRepository;
use chalkak_app::notification::sqs_push::PushMessage;
use chalkak_app::testing::TestDb;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, FixedClock};
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};
use postgres::{Client, NoTls};
use serde_json::{Map, Value};
use uuid::Uuid;

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn plus(at: Timestamp, seconds: i64) -> Timestamp {
    at.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

/// 로그 sink.
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
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn events(&self) -> Vec<Map<String, Value>> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|value| value.as_object().unwrap().clone())
            .collect()
    }

    fn at_stage(&self, stage: &str) -> Vec<Map<String, Value>> {
        self.events()
            .into_iter()
            .filter(|event| event.get("stage").and_then(Value::as_str) == Some(stage))
            .collect()
    }
}

type Responder =
    Box<dyn FnMut(&DevicePushRequest) -> Result<DevicePushResult, SenderFailure> + Send>;

/// Mockito `DevicePushSender` 대용: 받은 요청을 기억하고, 정해 둔 결과를 순서대로(마지막 값은 반복) 돌려준다.
struct FakeSender {
    responder: Mutex<Responder>,
    requests: Mutex<Vec<DevicePushRequest>>,
}

impl FakeSender {
    fn accepting() -> Arc<Self> {
        Arc::new(FakeSender {
            responder: Mutex::new(Box::new(|_| Ok(DevicePushResult::accepted()))),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// `willReturn(a, b, ...)`: 순서대로 돌려주고 다 쓰면 마지막 값을 반복한다.
    fn queue(&self, results: Vec<Result<DevicePushResult, SenderFailure>>) {
        let mut index = 0;
        self.respond(move |_| {
            let result = results[index.min(results.len() - 1)].clone();
            index += 1;
            result
        });
    }

    /// `willAnswer`
    fn respond(
        &self,
        f: impl FnMut(&DevicePushRequest) -> Result<DevicePushResult, SenderFailure> + Send + 'static,
    ) {
        *self.responder.lock().unwrap() = Box::new(f);
    }

    fn requests(&self) -> Vec<DevicePushRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl DevicePushSender for FakeSender {
    fn send(&self, request: &DevicePushRequest) -> Result<DevicePushResult, SenderFailure> {
        self.requests.lock().unwrap().push(request.clone());
        (self.responder.lock().unwrap())(request)
    }
}

fn ok(result: DevicePushResult) -> Result<DevicePushResult, SenderFailure> {
    Ok(result)
}

fn boom() -> Result<DevicePushResult, SenderFailure> {
    Err(SenderFailure::new("IllegalStateException"))
}

fn raw(m: &PushMessage) -> RawPushMessage {
    RawPushMessage {
        event_id: Some(m.event_id),
        notification_id: m.notification_id,
        user_id: Some(m.user_id),
        kind: Some(m.kind),
        source_type: m.source_type,
        source_id: m.source_id,
        occurred_at: Some(m.occurred_at),
        expires_at: Some(m.expires_at),
    }
}

struct Fx {
    db: TestDb,
    pool: Pool,
    sender: Arc<FakeSender>,
    lines: Lines,
    user: Uuid,
    now: Timestamp,
    message: PushMessage,
}

impl Fx {
    fn base(now: &str) -> Option<Fx> {
        let db = TestDb::create()?;
        let pool = db.pool(4);
        let user = Uuid::new_v4();
        let mut c = db.client();
        c.execute(
            "INSERT INTO users (id, email, status, signature_original_storage_key) \
             VALUES ($1, 'worker@example.com', 'ACTIVE', 'worker/signature')",
            &[&user],
        )
        .unwrap();
        drop(c);
        let now = ts(now);
        let message = PushMessage {
            event_id: Uuid::new_v4(),
            notification_id: None,
            user_id: user,
            kind: NotificationType::Topic,
            source_type: Some(NotificationSourceType::Topic),
            source_id: None,
            occurred_at: now,
            expires_at: plus(now, 1800),
        };
        Some(Fx {
            db,
            pool,
            sender: FakeSender::accepting(),
            lines: Lines::default(),
            user,
            now,
            message,
        })
    }

    /// `worker(now)`: 시각을 고정한 업무 서비스.
    fn worker(&self, now: Timestamp) -> PushWorkerService {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(now));
        let logger = Arc::new(Logger::new(
            Box::new(self.lines.clone()),
            Level::Info,
            clock.clone(),
        ));
        PushWorkerService::new(self.pool.clone(), clock, self.sender.clone(), logger)
    }

    fn process_at(&self, now: Timestamp, message: &PushMessage) -> PushProcessingResult {
        self.worker(now).process_message(&raw(message)).unwrap()
    }

    fn process(&self, message: &PushMessage) -> PushProcessingResult {
        self.process_at(self.now, message)
    }

    /// 로그인 세션(리프레시 토큰)과 기기를 하나 만든다.
    fn add_device(&self, token: &str) -> Uuid {
        add_device(&self.db, self.user, token, self.now)
    }

    fn device_ids(&self) -> Vec<Uuid> {
        self.db
            .client()
            .query(
                "SELECT id FROM push_devices WHERE user_id = $1 ORDER BY registered_at, id",
                &[&self.user],
            )
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    fn execute(&self, sql: &str) {
        self.db.client().batch_execute(sql).unwrap();
    }

    fn execute_at(&self, sql: &str, at: Timestamp) {
        self.db.client().execute(sql, &[&at]).unwrap();
    }

    fn skipped_reasons(&self) -> Vec<String> {
        self.lines
            .at_stage("worker_skipped")
            .iter()
            .map(|log| log["reason"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn add_device(db: &TestDb, user: Uuid, token: &str, now: Timestamp) -> Uuid {
    let session = Uuid::new_v4();
    let mut c = db.client();
    c.execute(
        "INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
        &[
            &user,
            &session,
            &FcmToken::new(&Uuid::new_v4().to_string()).unwrap().hash(),
            &plus(now, 3600),
            &plus(now, 7200),
        ],
    )
    .unwrap();
    c.query_one(
        "INSERT INTO push_devices (user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $5) RETURNING id",
        &[
            &user,
            &session,
            &token,
            &FcmToken::new(token).unwrap().hash(),
            &plus(now, 1),
        ],
    )
    .unwrap()
    .get(0)
}

// ---------------------------------------------------------------------------------------------
// PushWorkerServiceTest (승인·반려 알림)
// ---------------------------------------------------------------------------------------------

struct Approved {
    fx: Fx,
    post: Uuid,
    device: Uuid,
}

fn approved() -> Option<Approved> {
    let mut fx = Fx::base("2026-10-07T00:00:00Z")?;
    let (topic, photo, post) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut c = fx.db.client();
    c.execute(
        "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) \
         VALUES ($1, '주제', CURRENT_DATE, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP + INTERVAL '1 hour')",
        &[&topic],
    )
    .unwrap();
    c.execute(
        "INSERT INTO photos (id, original_storage_key) VALUES ($1, 'worker/original')",
        &[&photo],
    )
    .unwrap();
    c.execute(
        "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status) \
         VALUES ($1, $2, $3, $4, 'APPROVED')",
        &[&post, &fx.user, &topic, &photo],
    )
    .unwrap();
    drop(c);
    let (user, now) = (fx.user, fx.now);
    let notification = transact(&fx.pool, |tx| {
        let mut notification = Notification::approved(user, post, Uuid::new_v4(), now, true);
        NotificationRepository.save(tx, &mut notification)?;
        Ok(notification)
    })
    .unwrap();
    fx.message = PushMessage {
        event_id: notification.event_key,
        notification_id: notification.id,
        user_id: user,
        kind: notification.kind,
        source_type: notification.source_type,
        source_id: Some(post),
        occurred_at: now,
        expires_at: plus(now, 1800),
    };
    let device = fx.add_device("worker-token");
    Some(Approved { fx, post, device })
}

#[test]
fn 현재_등록_기기로_보내며_최초_sqs_상태_반영이_실패한_대기_알림도_처리한다() {
    let Some(a) = approved() else { return };
    let result = a.fx.process(&a.fx.message);
    assert!(!result.retryable);
    assert_eq!(a.fx.sender.calls(), 1);
}

#[test]
fn 발생_후_30분_직전만_발송하고_정각_이후에는_완료한다() {
    for seconds in [1799, 1800, 1801] {
        let Some(a) = approved() else { return };
        let result = a.fx.process_at(plus(a.fx.now, seconds), &a.fx.message);
        assert!(!result.retryable, "{seconds}");
        assert_eq!(
            a.fx.sender.calls(),
            usize::from(seconds < 1800),
            "{seconds}"
        );
    }
}

#[test]
fn 발송_직전_읽음_삭제_탈퇴_설정_로그인_기기_상태를_확인하여_발송을_생략한다() {
    let cases: [(&str, &str, &str); 9] = [
        (
            "read",
            "UPDATE notifications SET read_at = $1",
            "ALREADY_READ",
        ),
        (
            "postDeleted",
            "UPDATE posts SET deleted_at = $1",
            "NOTIFICATION_UNAVAILABLE",
        ),
        (
            "withdrawn",
            "UPDATE users SET deleted_at = $1",
            "LOGIN_UNUSABLE",
        ),
        (
            "disabled",
            "UPDATE users SET moderation_push_enabled = false WHERE $1::timestamptz IS NOT NULL",
            "PUSH_DISABLED",
        ),
        (
            "revoked",
            "UPDATE user_refresh_tokens SET revoked_at = $1",
            "LOGIN_UNUSABLE",
        ),
        (
            "expired",
            "UPDATE user_refresh_tokens SET expires_at = $1",
            "LOGIN_UNUSABLE",
        ),
        (
            "missingDevice",
            "DELETE FROM push_devices WHERE $1::timestamptz IS NOT NULL",
            "NO_DEVICES",
        ),
        (
            "missingNotification",
            "DELETE FROM notifications WHERE $1::timestamptz IS NOT NULL",
            "NOTIFICATION_UNAVAILABLE",
        ),
        (
            "missingPost",
            "DELETE FROM posts WHERE $1::timestamptz IS NOT NULL",
            "NOTIFICATION_UNAVAILABLE",
        ),
    ];
    for (state, sql, reason) in cases {
        let Some(a) = approved() else { return };
        a.fx.execute_at(sql, a.fx.now);
        let result = a.fx.process(&a.fx.message);
        assert!(!result.retryable, "{state}");
        assert_eq!(a.fx.sender.calls(), 0, "{state}");
        assert_eq!(a.fx.skipped_reasons(), vec![reason.to_owned()], "{state}");
    }
}

#[test]
fn 정지_회원에게도_검수_결과_푸시를_보낸다() {
    let Some(a) = approved() else { return };
    a.fx.execute("UPDATE users SET status = 'BANNED'");
    a.fx.process(&a.fx.message);
    assert_eq!(a.fx.sender.calls(), 1);
}

#[test]
fn 모든_기기가_일시_실패하면_재전달을_요청하며_가장_긴_retry_after를_유지한다() {
    let Some(a) = approved() else { return };
    a.fx.add_device("second-token");
    a.fx.sender.queue(vec![
        ok(DevicePushResult::retryable_after(
            "UNAVAILABLE",
            Duration::from_secs(60),
        )),
        ok(DevicePushResult::retryable_after(
            "QUOTA",
            Duration::from_secs(120),
        )),
    ]);
    let result = a.fx.process(&a.fx.message);
    assert!(result.retryable);
    assert_eq!(result.retry_after, Duration::from_secs(120));
}

#[test]
fn 기기_하나라도_수락되면_나머지_일시_실패를_재시도하지_않는다() {
    let Some(a) = approved() else { return };
    a.fx.add_device("second-token");
    a.fx.sender.queue(vec![
        ok(DevicePushResult::retryable_after(
            "UNAVAILABLE",
            Duration::from_secs(60),
        )),
        ok(DevicePushResult::accepted()),
    ]);
    assert!(!a.fx.process(&a.fx.message).retryable);
    assert_eq!(a.fx.sender.calls(), 2);
    let summary = a.fx.lines.at_stage("worker_summary");
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["targetCount"], 2);
    assert_eq!(summary[0]["acceptedCount"], 1);
    assert_eq!(summary[0]["retryableCount"], 1);
    assert_eq!(summary[0]["retryable"], false);
}

#[test]
fn 첫_기기_수락_후_다음_기기의_예외가_발생해도_사용자_작업을_완료한다() {
    let Some(a) = approved() else { return };
    a.fx.add_device("second-token");
    a.fx.sender
        .queue(vec![ok(DevicePushResult::accepted()), boom()]);
    assert_eq!(
        a.fx.process(&a.fx.message),
        PushProcessingResult::completed()
    );
    assert_eq!(a.fx.sender.calls(), 2);
}

#[test]
fn 첫_기기의_예외가_발생해도_다음_기기를_발송하고_수락되면_완료한다() {
    let Some(a) = approved() else { return };
    a.fx.add_device("second-token");
    a.fx.sender
        .queue(vec![boom(), ok(DevicePushResult::accepted())]);
    assert_eq!(
        a.fx.process(&a.fx.message),
        PushProcessingResult::completed()
    );
    assert_eq!(a.fx.sender.calls(), 2);
}

#[test]
fn 수락한_기기_없이_예상하지_못한_예외가_발생하면_재전달을_요청한다() {
    let Some(a) = approved() else { return };
    a.fx.sender.queue(vec![boom()]);
    let result = a.fx.process(&a.fx.message);
    assert!(result.retryable);
    assert_eq!(a.fx.device_ids(), vec![a.device]);
}

/// 기기 삭제를 거부하는 트리거(원본 `rejectDeviceDeletion`).
fn reject_device_deletion(fx: &Fx) {
    fx.execute(
        "CREATE FUNCTION worker_reject_device_delete() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'device cleanup failure'; END $$; \
         CREATE TRIGGER worker_reject_device_delete BEFORE DELETE ON push_devices \
         FOR EACH ROW EXECUTE FUNCTION worker_reject_device_delete()",
    );
}

#[test]
fn 첫_기기_수락_후_다음_기기의_무효_토큰_삭제가_실패해도_완료한다() {
    let Some(a) = approved() else { return };
    let other = a.fx.add_device("second-token");
    reject_device_deletion(&a.fx);
    a.fx.sender.queue(vec![
        ok(DevicePushResult::accepted()),
        ok(DevicePushResult::invalid_token("UNREGISTERED")),
    ]);
    assert_eq!(
        a.fx.process(&a.fx.message),
        PushProcessingResult::completed()
    );
    let mut ids = a.fx.device_ids();
    ids.sort();
    let mut expected = vec![a.device, other];
    expected.sort();
    assert_eq!(ids, expected);
    assert_eq!(a.fx.sender.calls(), 2);
}

#[test]
fn 무효_토큰_삭제_실패는_재발송_사유가_아니며_기기_행은_롤백되어_남는다() {
    let Some(a) = approved() else { return };
    reject_device_deletion(&a.fx);
    a.fx.sender
        .queue(vec![ok(DevicePushResult::invalid_token("UNREGISTERED"))]);
    assert_eq!(
        a.fx.process(&a.fx.message),
        PushProcessingResult::completed()
    );
    assert_eq!(a.fx.device_ids(), vec![a.device]);
}

#[test]
fn 등록_해제_토큰의_해당_기기만_제거한다() {
    let Some(a) = approved() else { return };
    let other = a.fx.add_device("other-token");
    a.fx.sender.respond(|request| {
        if request.token == "worker-token" {
            ok(DevicePushResult::invalid_token("UNREGISTERED"))
        } else {
            ok(DevicePushResult::accepted())
        }
    });
    a.fx.process(&a.fx.message);
    assert_eq!(a.fx.device_ids(), vec![other]);
}

#[test]
fn 실패_응답_전에_갱신된_토큰은_이전_무효_토큰_제거로_삭제하지_않는다() {
    let Some(a) = approved() else { return };
    let (cfg, device) = (a.fx.db.config.pg.clone(), a.device);
    a.fx.sender.respond(move |_| {
        let mut c = cfg.connect(NoTls).unwrap();
        c.execute(
            "UPDATE push_devices SET fcm_token = 'updated-token', fcm_token_hash = $1 WHERE id = $2",
            &[&FcmToken::new("updated-token").unwrap().hash(), &device],
        )
        .unwrap();
        ok(DevicePushResult::invalid_token("UNREGISTERED"))
    });
    a.fx.process(&a.fx.message);
    let token: String =
        a.fx.db
            .client()
            .query_one(
                "SELECT fcm_token FROM push_devices WHERE id = $1",
                &[&a.device],
            )
            .unwrap()
            .get(0);
    assert_eq!(token, "updated-token");
}

#[test]
fn 메시지_프로젝트_인증_영구_오류는_기기와_토큰을_보존하고_작업을_완료한다() {
    for code in [
        "INVALID_ARGUMENT",
        "SENDER_ID_MISMATCH",
        "THIRD_PARTY_AUTH_ERROR",
    ] {
        let Some(a) = approved() else { return };
        a.fx.sender
            .queue(vec![ok(DevicePushResult::permanent_failure(code))]);
        assert_eq!(
            a.fx.process(&a.fx.message),
            PushProcessingResult::completed()
        );
        assert_eq!(a.fx.device_ids(), vec![a.device]);
        let row =
            a.fx.db
                .client()
                .query_one(
                    "SELECT fcm_token, fcm_token_hash FROM push_devices WHERE id = $1",
                    &[&a.device],
                )
                .unwrap();
        assert_eq!(row.get::<_, String>(0), "worker-token");
        assert_eq!(
            row.get::<_, String>(1),
            FcmToken::new("worker-token").unwrap().hash()
        );
    }
}

#[test]
fn 다른_사건이나_임의로_연장한_기한의_메시지는_발송하지_않는다() {
    let Some(a) = approved() else { return };
    let invalid = PushMessage {
        event_id: Uuid::new_v4(),
        expires_at: plus(a.fx.now, 3600),
        ..a.fx.message.clone()
    };
    assert!(!a.fx.process(&invalid).retryable);
    assert_eq!(a.fx.sender.calls(), 0);
}

#[test]
fn 발송_전용_조회에서도_원래_사건_시각보다_앞서거나_뒤선_메시지는_발송하지_않는다() {
    for seconds in [-1, 1] {
        let Some(a) = approved() else { return };
        let invalid = PushMessage {
            occurred_at: plus(a.fx.now, seconds),
            ..a.fx.message.clone()
        };
        assert_eq!(a.fx.process(&invalid), PushProcessingResult::completed());
        assert_eq!(a.fx.sender.calls(), 0, "{seconds}");
    }
}

#[test]
fn 기기_처리_결과를_종류별_집계하고_식별_정보만_로그에_남긴다() {
    for status in [
        "ACCEPTED",
        "RETRYABLE",
        "PERMANENT_FAILURE",
        "INVALID_TOKEN",
        "SKIPPED",
    ] {
        let Some(a) = approved() else { return };
        let result = match status {
            "RETRYABLE" => DevicePushResult::retryable("UNAVAILABLE"),
            "PERMANENT_FAILURE" => DevicePushResult::permanent_failure("SENDER_ID_MISMATCH"),
            "INVALID_TOKEN" => DevicePushResult::invalid_token("UNREGISTERED"),
            "SKIPPED" => DevicePushResult::skipped("PUSH_EXPIRED"),
            _ => DevicePushResult::accepted(),
        };
        a.fx.sender.queue(vec![ok(result)]);
        a.fx.process(&a.fx.message);

        let summaries = a.fx.lines.at_stage("worker_summary");
        let summary = &summaries[0];
        let m = &a.fx.message;
        assert_eq!(summary["eventId"], m.event_id.to_string());
        assert_eq!(
            summary["notificationId"],
            m.notification_id.unwrap().to_string()
        );
        assert_eq!(summary["userId"], a.fx.user.to_string());
        assert_eq!(summary["targetCount"], 1);
        let expected = |name: &str| i64::from(status == name);
        assert_eq!(summary["acceptedCount"], expected("ACCEPTED"), "{status}");
        assert_eq!(summary["retryableCount"], expected("RETRYABLE"), "{status}");
        assert_eq!(
            summary["permanentFailureCount"],
            expected("PERMANENT_FAILURE"),
            "{status}"
        );
        assert_eq!(
            summary["invalidTokenCount"],
            expected("INVALID_TOKEN"),
            "{status}"
        );
        assert_eq!(summary["skippedCount"], expected("SKIPPED"), "{status}");
        assert_eq!(
            a.fx.lines.at_stage("fcm_result").len(),
            usize::from(status != "SKIPPED"),
            "{status}"
        );
        let request = a.fx.sender.requests().remove(0);
        let logged = a.fx.lines.text();
        for secret in [
            request.token.as_str(),
            request.title.as_str(),
            request.body.as_str(),
            "worker@example.com",
            "worker/original",
        ] {
            assert!(!logged.contains(secret), "{status}: {secret}");
        }
        let _ = a.post;
    }
}

// ---------------------------------------------------------------------------------------------
// PushWorkerTopicTest (주제)
// ---------------------------------------------------------------------------------------------

struct Topic {
    fx: Fx,
    topic: Uuid,
}

fn insert_topic(
    c: &mut Client,
    id: Uuid,
    title: &str,
    date: &str,
    starts: Timestamp,
    ends: Timestamp,
) {
    c.execute(
        "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) VALUES ($1, $2, $3::text::date, $4, $5)",
        &[&id, &title, &date, &starts, &ends],
    )
    .unwrap();
}

fn topic_message(fx: &Fx, topic: Uuid, at: Timestamp) -> PushMessage {
    PushMessage {
        event_id: Uuid::new_v4(),
        notification_id: None,
        user_id: fx.user,
        kind: NotificationType::Topic,
        source_type: Some(NotificationSourceType::Topic),
        source_id: Some(topic),
        occurred_at: at,
        expires_at: plus(at, 1800),
    }
}

fn topic_fx() -> Option<Topic> {
    let mut fx = Fx::base("2026-10-12T00:00:00Z")?;
    let topic = Uuid::new_v4();
    // NOW(UTC 2026-10-12 00:00)는 KST로도 10월 12일이다.
    insert_topic(
        &mut fx.db.client(),
        topic,
        "빛과 그림자",
        "2026-10-12",
        plus(fx.now, -3600),
        plus(fx.now, 3600),
    );
    fx.add_device("topic-token");
    fx.message = topic_message(&fx, topic, fx.now);
    Some(Topic { fx, topic })
}

fn insert_post(db: &TestDb, now: Timestamp, user: Uuid, topic: Uuid, status: &str, deleted: bool) {
    let photo = Uuid::new_v4();
    let mut c = db.client();
    c.execute(
        "INSERT INTO photos (id, original_storage_key) VALUES ($1, $2)",
        &[&photo, &format!("topic/photo/{photo}")],
    )
    .unwrap();
    let deleted_at = deleted.then(|| plus(now, 30));
    c.execute(
        "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status, deleted_at, created_at) \
         VALUES ($1, $2, $3, $4, $5::text::moderation_status, $6, $7)",
        &[&Uuid::new_v4(), &user, &topic, &photo, &status, &deleted_at, &plus(now, 10)],
    )
    .unwrap();
}

#[test]
fn 주제_오늘_주제에_제출하지_않은_회원에게_검수_설정과_관계없이_주제를_보내고_알림함에_저장하지_않는다()
 {
    let Some(t) = topic_fx() else { return };
    t.fx.execute("UPDATE users SET moderation_push_enabled = false");
    let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    let requests = t.fx.sender.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].message, t.fx.message);
    assert_eq!(requests[0].title, "오늘의 주제");
    assert_eq!(requests[0].body, "빛과 그림자");
    assert_eq!(requests[0].token, "topic-token");
    assert_eq!(requests[0].expires_at, plus(t.fx.now, 1800));
    let count: i64 =
        t.fx.db
            .client()
            .query_one("SELECT COUNT(*) FROM notifications", &[])
            .unwrap()
            .get(0);
    assert_eq!(count, 0);
}

#[test]
fn 큐_등록_후_오늘_주제에_참여하면_발송을_생략하고_반려_삭제만_있으면_발송한다() {
    let cases = [
        ("VALIDATING", false, false),
        ("PENDING", false, false),
        ("APPROVED", false, false),
        ("REJECTED", false, true),
        ("VALIDATING", true, true),
        ("PENDING", true, true),
        ("APPROVED", true, true),
        ("REJECTED", true, true),
    ];
    for (status, deleted, sendable) in cases {
        let Some(t) = topic_fx() else { return };
        insert_post(&t.fx.db, t.fx.now, t.fx.user, t.topic, status, deleted);
        let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
        assert_eq!(result, PushProcessingResult::completed());
        assert_eq!(
            t.fx.sender.calls(),
            usize::from(sendable),
            "{status} deleted={deleted}"
        );
    }
}

#[test]
fn 주제_반려_후_재제출한_승인_대기_게시물이_있으면_재시도_없이_완료한다() {
    let Some(t) = topic_fx() else { return };
    insert_post(&t.fx.db, t.fx.now, t.fx.user, t.topic, "REJECTED", false);
    insert_post(&t.fx.db, t.fx.now, t.fx.user, t.topic, "PENDING", false);
    let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    assert_eq!(t.fx.sender.calls(), 0);
}

#[test]
fn 다른_주제나_다른_회원의_제출은_현재_회원의_주제_발송을_막지_않는다() {
    for state in ["otherTopic", "otherUser"] {
        let Some(t) = topic_fx() else { return };
        let (mut author, mut topic) = (t.fx.user, t.topic);
        let mut c = t.fx.db.client();
        if state == "otherTopic" {
            topic = Uuid::new_v4();
            insert_topic(
                &mut c,
                topic,
                "다른 주제",
                "2026-10-11",
                plus(t.fx.now, -86400),
                plus(t.fx.now, -3600),
            );
        } else {
            author = Uuid::new_v4();
            c.execute(
                "INSERT INTO users (id, email, status, signature_original_storage_key) \
                 VALUES ($1, $2, 'ACTIVE', $3)",
                &[
                    &author,
                    &format!("{author}@topic-worker.test"),
                    &format!("signature/{author}"),
                ],
            )
            .unwrap();
        }
        drop(c);
        insert_post(&t.fx.db, t.fx.now, author, topic, "APPROVED", false);
        let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
        assert_eq!(result, PushProcessingResult::completed());
        assert_eq!(t.fx.sender.calls(), 1, "{state}");
    }
}

#[test]
fn 일시_실패_후_제출한_회원은_같은_메시지를_다시_받아도_fcm_재발송_없이_완료한다() {
    let Some(t) = topic_fx() else { return };
    t.fx.sender
        .queue(vec![ok(DevicePushResult::retryable("UNAVAILABLE"))]);
    let service = t.fx.worker(plus(t.fx.now, 60));
    let first = service.process_message(&raw(&t.fx.message)).unwrap();
    assert!(first.retryable);
    insert_post(&t.fx.db, t.fx.now, t.fx.user, t.topic, "PENDING", false);
    let second = service.process_message(&raw(&t.fx.message)).unwrap();
    assert_eq!(second, PushProcessingResult::completed());
    assert_eq!(t.fx.sender.calls(), 1);
}

#[test]
fn 첫_기기_처리_중_제출하면_다음_기기_요청_직전에_재확인해_추가_발송을_생략한다() {
    let Some(t) = topic_fx() else { return };
    t.fx.add_device("second-topic-token");
    let (db_cfg, now, user, topic) = (t.fx.db.config.pg.clone(), t.fx.now, t.fx.user, t.topic);
    t.fx.sender.respond(move |_| {
        let photo = Uuid::new_v4();
        let mut c = db_cfg.connect(NoTls).unwrap();
        c.execute(
            "INSERT INTO photos (id, original_storage_key) VALUES ($1, $2)",
            &[&photo, &format!("topic/photo/{photo}")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status, created_at) \
             VALUES ($1, $2, $3, $4, 'PENDING', $5)",
            &[&Uuid::new_v4(), &user, &topic, &photo, &plus(now, 10)],
        )
        .unwrap();
        ok(DevicePushResult::accepted())
    });
    let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    assert_eq!(t.fx.sender.calls(), 1);
}

#[test]
fn 주제_푸시의_최신_회원_수신_설정_오늘_주제_로그인_기기_조건이_바뀌면_생략한다() {
    let states = [
        "banned",
        "withdrawn",
        "disabled",
        "revoked",
        "expired",
        "missingDevice",
        "missingTopic",
        "deletedTopic",
        "differentTopic",
    ];
    for state in states {
        let Some(t) = topic_fx() else { return };
        let mut message = t.fx.message.clone();
        let at = t.fx.now;
        match state {
            "banned" => t.fx.execute("UPDATE users SET status = 'BANNED'"),
            "withdrawn" => t.fx.execute_at("UPDATE users SET deleted_at = $1", at),
            "disabled" => t.fx.execute("UPDATE users SET topic_push_enabled = false"),
            "revoked" => {
                t.fx.execute_at("UPDATE user_refresh_tokens SET revoked_at = $1", at)
            }
            "expired" => {
                t.fx.execute_at("UPDATE user_refresh_tokens SET expires_at = $1", at)
            }
            "missingDevice" => t.fx.execute("DELETE FROM push_devices"),
            "missingTopic" => t.fx.execute("DELETE FROM topics"),
            "deletedTopic" => t.fx.execute_at("UPDATE topics SET deleted_at = $1", at),
            _ => message.source_id = Some(Uuid::new_v4()),
        }
        let result = t.fx.process_at(plus(t.fx.now, 60), &message);
        assert_eq!(result, PushProcessingResult::completed(), "{state}");
        assert_eq!(t.fx.sender.calls(), 0, "{state}");
    }
}

#[test]
fn 주제도_예정_시각부터_30분_직전만_발송하고_경계부터는_완료한다() {
    for seconds in [1799, 1800, 1801] {
        let Some(t) = topic_fx() else { return };
        let result = t.fx.process_at(plus(t.fx.now, seconds), &t.fx.message);
        assert_eq!(result, PushProcessingResult::completed(), "{seconds}");
        assert_eq!(
            t.fx.sender.calls(),
            usize::from(seconds < 1800),
            "{seconds}"
        );
    }
}

#[test]
fn kst_날짜가_바뀌면_기한이_남아도_전날_주제_메시지를_보내지_않는다() {
    let Some(mut t) = topic_fx() else { return };
    let scheduled = ts("2026-10-11T14:55:00Z");
    t.fx.execute("UPDATE topics SET topic_date = '2026-10-11'");
    t.fx.message = topic_message(&t.fx, t.topic, scheduled);
    let result = t.fx.process_at(ts("2026-10-11T15:00:00Z"), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    assert_eq!(t.fx.sender.calls(), 0);
}

#[test]
fn 알림함_id_게시물_대상_연장_기한_미래_시각_누락_식별자가_있는_주제_메시지는_보내지_않는다() {
    for field in [
        "notificationId",
        "sourceType",
        "deadline",
        "future",
        "eventId",
    ] {
        let Some(t) = topic_fx() else { return };
        let mut invalid = raw(&t.fx.message);
        match field {
            "notificationId" => invalid.notification_id = Some(Uuid::new_v4()),
            "sourceType" => invalid.source_type = Some(NotificationSourceType::Post),
            "deadline" => invalid.expires_at = Some(plus(t.fx.now, 3600)),
            "future" => {
                invalid.occurred_at = Some(plus(t.fx.now, 60));
                invalid.expires_at = Some(plus(t.fx.now, 1860));
            }
            _ => invalid.event_id = None,
        }
        let result = t.fx.worker(t.fx.now).process_message(&invalid).unwrap();
        assert_eq!(result, PushProcessingResult::completed(), "{field}");
        assert_eq!(t.fx.sender.calls(), 0, "{field}");
    }
}

#[test]
fn 재전달_때는_당시의_기기를_다시_조회하고_새_등록_기기로_발송한다() {
    let Some(t) = topic_fx() else { return };
    t.fx.sender.queue(vec![
        ok(DevicePushResult::retryable("UNAVAILABLE")),
        ok(DevicePushResult::accepted()),
    ]);
    assert!(t.fx.process_at(plus(t.fx.now, 60), &t.fx.message).retryable);
    t.fx.execute("DELETE FROM push_devices");
    t.fx.add_device("new-topic-token");
    let result = t.fx.process_at(plus(t.fx.now, 120), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    let tokens: Vec<String> =
        t.fx.sender
            .requests()
            .into_iter()
            .map(|r| r.token)
            .collect();
    assert_eq!(tokens, vec!["topic-token", "new-topic-token"]);
}

#[test]
fn 주제도_기기_하나가_수락하면_다른_기기의_실패를_재시도하지_않는다() {
    let Some(t) = topic_fx() else { return };
    t.fx.add_device("second-topic-token");
    t.fx.sender.queue(vec![
        ok(DevicePushResult::accepted()),
        ok(DevicePushResult::retryable("UNAVAILABLE")),
    ]);
    let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    assert_eq!(t.fx.sender.calls(), 2);
}

#[test]
fn 주제의_모든_기기가_일시_실패하면_가장_긴_지연으로_sqs_재전달을_요청한다() {
    let Some(t) = topic_fx() else { return };
    t.fx.add_device("second-topic-token");
    t.fx.sender.queue(vec![
        ok(DevicePushResult::retryable_after(
            "UNAVAILABLE",
            Duration::from_secs(60),
        )),
        ok(DevicePushResult::retryable_after(
            "QUOTA_EXCEEDED",
            Duration::from_secs(120),
        )),
    ]);
    let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
    assert!(result.retryable);
    assert_eq!(result.retry_after, Duration::from_secs(120));
}

#[test]
fn 주제의_영구_실패는_완료하고_무효_토큰일_때만_기기_행을_지운다() {
    let Some(t) = topic_fx() else { return };
    t.fx.add_device("second-topic-token");
    t.fx.sender.queue(vec![
        ok(DevicePushResult::permanent_failure("INVALID_ARGUMENT")),
        ok(DevicePushResult::invalid_token("UNREGISTERED")),
    ]);
    let result = t.fx.process_at(plus(t.fx.now, 60), &t.fx.message);
    assert_eq!(result, PushProcessingResult::completed());
    assert_eq!(t.fx.device_ids().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// PushWorkerNoticeTest (공지)
// ---------------------------------------------------------------------------------------------

const NOTICE_TITLE: &str = "서비스 점검 안내";
const NOTICE_BODY: &str = "10월 20일 02:00부터 점검합니다.";

fn notice_fx() -> Option<Fx> {
    let mut fx = Fx::base("2026-10-14T00:00:00Z")?;
    let (now, user) = (fx.now, fx.user);
    // 실제 배포 경로로 공지 알림을 만들고 Relay가 발행하는 것과 같은 메시지를 구성한다.
    fx.message = transact(&fx.pool, |tx| {
        let mut notice = Notice::create(NOTICE_TITLE, NOTICE_BODY, now, now).unwrap();
        NoticeRepository.save_and_flush(tx, &mut notice, now)?;
        NoticeRepository.create_notice_notifications(tx, &notice, now)?;
        let notification_id: Uuid = tx
            .query_one("SELECT id FROM notifications WHERE user_id = $1", &[&user])
            .map_err(chalkak_db::DbError::from)?
            .get(0);
        let notification = NotificationRepository
            .find_for_push_by_id_and_user_id(tx, notification_id, user)?
            .expect("방금 배포한 공지 알림");
        Ok(PushMessage {
            event_id: notification.event_key,
            notification_id: notification.id,
            user_id: user,
            kind: notification.kind,
            source_type: notification.source_type,
            source_id: notification.source_id,
            occurred_at: notification.created_at,
            expires_at: notification.push_expires_at(),
        })
    })
    .unwrap();
    fx.add_device("notice-token");
    Some(fx)
}

fn assert_skipped(fx: &Fx, result: PushProcessingResult, reason: &str) {
    assert!(!result.retryable);
    assert_eq!(fx.sender.calls(), 0);
    assert_eq!(fx.skipped_reasons(), vec![reason.to_owned()]);
}

#[test]
fn 배포된_공지_알림을_공지_제목_본문으로_현재_기기에_보낸다() {
    let Some(fx) = notice_fx() else { return };
    let result = fx.process(&fx.message);
    assert!(!result.retryable);
    let requests = fx.sender.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].title, NOTICE_TITLE);
    assert_eq!(requests[0].body, NOTICE_BODY);
    assert_eq!(requests[0].message.kind, NotificationType::Notice);
    assert_eq!(
        requests[0].message.source_type,
        Some(NotificationSourceType::Notice)
    );
}

#[test]
fn 검수_결과_푸시를_끈_회원에게도_공지를_보낸다() {
    let Some(fx) = notice_fx() else { return };
    fx.execute("UPDATE users SET moderation_push_enabled = false");
    fx.process(&fx.message);
    assert_eq!(fx.sender.calls(), 1);
}

#[test]
fn 정지_회원에게도_공지를_보낸다() {
    let Some(fx) = notice_fx() else { return };
    fx.execute("UPDATE users SET status = 'BANNED'");
    fx.process(&fx.message);
    assert_eq!(fx.sender.calls(), 1);
}

#[test]
fn 이미_읽은_공지_알림은_보내지_않는다() {
    let Some(fx) = notice_fx() else { return };
    fx.execute_at("UPDATE notifications SET read_at = $1", fx.now);
    let result = fx.process(&fx.message);
    assert_skipped(&fx, result, "ALREADY_READ");
}

#[test]
fn 공지_배포_뒤_탈퇴한_회원에게는_보내지_않는다() {
    let Some(fx) = notice_fx() else { return };
    fx.execute_at("UPDATE users SET deleted_at = $1", fx.now);
    let result = fx.process(&fx.message);
    assert_skipped(&fx, result, "LOGIN_UNUSABLE");
}

#[test]
fn 배포_후_30분_직전까지만_보내고_정각_이후에는_보내지_않는다() {
    for seconds in [1799, 1800, 1801] {
        let Some(fx) = notice_fx() else { return };
        let result = fx.process_at(plus(fx.now, seconds), &fx.message);
        if seconds < 1800 {
            assert_eq!(fx.sender.calls(), 1, "{seconds}");
        } else {
            assert_skipped(&fx, result, "EXPIRED");
        }
    }
}

#[test]
fn 알림_id가_없거나_대상_종류가_공지가_아닌_공지_메시지는_보내지_않는다() {
    let Some(fx) = notice_fx() else { return };
    let mut without_id = raw(&fx.message);
    without_id.notification_id = None;
    let mut post_source = raw(&fx.message);
    post_source.source_type = Some(NotificationSourceType::Post);
    fx.worker(fx.now).process_message(&without_id).unwrap();
    fx.worker(fx.now).process_message(&post_source).unwrap();
    assert_eq!(fx.sender.calls(), 0);
    assert_eq!(
        fx.skipped_reasons(),
        vec!["INVALID_MESSAGE", "INVALID_MESSAGE"]
    );
}

#[test]
fn 다른_사건으로_조작한_공지_메시지는_보내지_않는다() {
    let Some(fx) = notice_fx() else { return };
    let tampered = PushMessage {
        event_id: Uuid::new_v4(),
        ..fx.message.clone()
    };
    let result = fx.process(&tampered);
    assert_skipped(&fx, result, "MESSAGE_MISMATCH");
}

#[test]
fn 배포_후_삭제된_공지의_알림은_보내지_않는다() {
    let Some(fx) = notice_fx() else { return };
    fx.execute_at("UPDATE notices SET deleted_at = $1", fx.now);
    let result = fx.process(&fx.message);
    assert_skipped(&fx, result, "NOTIFICATION_UNAVAILABLE");
}
