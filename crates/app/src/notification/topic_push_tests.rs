//! 주제 푸시 테스트. 원본 `TopicPushServiceTest`에 대응한다. 시계는 고정 시계이고 SQS는 가짜 발행기(또는 가짜 전송)다.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본의 "발행 중 트랜잭션이 없다" 확인은 옮기지 않았다. 이 서비스는 처음부터 트랜잭션을 열지 않는다.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use chalkak_aws::{
    AwsError, Credentials, CredentialsProvider, SqsClient, SqsEndpoint, SqsHttpResponse,
    SqsTransport,
};
use chalkak_core::FixedClock;
use chalkak_db::Pool;
use jiff::Timestamp;
use postgres::Client;
use serde_json::{Map, Value};

use super::*;
use crate::notification::push_test_support::{
    FakePublisher, Lines, failure, insert_user, logger, plus, ts,
};
use crate::notification::sqs_push::{PushMessage, SqsPushMessagePublisher};
use crate::testing::TestDb;

const NOW: &str = "2026-10-12T00:00:00Z";

fn user(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

struct Fixture {
    db: TestDb,
    pool: Pool,
    clock: Arc<FixedClock>,
    lines: Lines,
    publisher: Arc<FakePublisher>,
    topic: Uuid,
}

fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let pool = db.pool(4);
    let mut client = db.client();
    insert_user(&mut client, user(1));
    let topic = Uuid::new_v4();
    insert_topic(&mut client, topic, "2026-10-12");
    insert_schedule(
        &mut client,
        Uuid::new_v4(),
        "09:00:00",
        plus(ts(NOW), -3600),
    );
    drop(client);
    Some(Fixture {
        db,
        pool,
        clock: Arc::new(FixedClock::new(ts(NOW))),
        lines: Lines::default(),
        publisher: FakePublisher::accepting(),
        topic,
    })
}

impl Fixture {
    fn service(&self) -> TopicPushService {
        self.service_with(self.publisher.clone())
    }

    fn service_with(&self, publisher: Arc<dyn PushMessagePublisher>) -> TopicPushService {
        TopicPushService::new(
            self.pool.clone(),
            self.clock.clone(),
            publisher,
            logger(&self.lines, &self.clock),
        )
    }

    fn client(&self) -> Client {
        self.db.client()
    }

    fn messages(&self) -> Vec<PushMessage> {
        self.publisher.calls()
    }

    fn summary(&self) -> Map<String, Value> {
        self.lines
            .at_stage("topic_publication_summary")
            .into_iter()
            .next()
            .expect("주제 발행 집계 로그가 없습니다.")
    }
}

fn insert_topic(client: &mut Client, id: Uuid, date: &str) {
    // 참여 기간은 KST 자정부터 24시간이다.
    client
        .execute(
            "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) \
             VALUES ($1, '빛과 그림자', $2::text::date, \
             ($2::text::date)::timestamp AT TIME ZONE 'Asia/Seoul', \
             ($2::text::date)::timestamp AT TIME ZONE 'Asia/Seoul' + interval '1 day')",
            &[&id, &date],
        )
        .unwrap();
}

fn insert_schedule(client: &mut Client, id: Uuid, send_time: &str, updated_at: Timestamp) {
    client
        .execute(
            "INSERT INTO topic_push_schedules (id, send_time, created_at, updated_at) \
             VALUES ($1, $2::text::time, $3, $3)",
            &[&id, &send_time, &updated_at],
        )
        .unwrap();
}

fn insert_participation(client: &mut Client, user: Uuid, topic: Uuid, status: &str) {
    let photo = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO photos (id, original_storage_key, thumbnail_storage_key) \
             VALUES ($1, 'topic/original', 'topic/thumbnail')",
            &[&photo],
        )
        .unwrap();
    client
        .execute(
            "INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status) \
             VALUES ($1, $2, $3, $4, $5::text::moderation_status)",
            &[&Uuid::new_v4(), &user, &topic, &photo, &status],
        )
        .unwrap();
}

fn count(summary: &Map<String, Value>, key: &str) -> i64 {
    summary[key].as_i64().unwrap()
}

// publishDueTopics_normalRecipients_publishesWithoutInboxOrTransaction
#[test]
fn 오늘_주제_참여_회원을_제외하고_검수_설정과_무관하게_사용자별_메시지를_직접_발행한다() {
    let Some(f) = fixture() else { return };
    let mut c = f.client();
    for n in 2..=6 {
        insert_user(&mut c, user(n));
    }
    c.execute(
        "UPDATE users SET moderation_push_enabled = false WHERE id = $1",
        &[&user(2)],
    )
    .unwrap();
    c.execute(
        "UPDATE users SET status = 'BANNED' WHERE id = $1",
        &[&user(3)],
    )
    .unwrap();
    c.execute(
        "UPDATE users SET deleted_at = $1 WHERE id = $2",
        &[&ts(NOW), &user(4)],
    )
    .unwrap();
    c.execute(
        "UPDATE users SET topic_push_enabled = false WHERE id = $1",
        &[&user(5)],
    )
    .unwrap();
    insert_participation(&mut c, user(6), f.topic, "APPROVED");

    f.service().publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(
        messages.iter().map(|m| m.user_id).collect::<Vec<_>>(),
        vec![user(1), user(2)]
    );
    assert!(messages.iter().all(|m| m.event_id == messages[0].event_id));
    for message in &messages {
        assert_eq!(message.notification_id, None);
        assert_eq!(message.kind, NotificationType::Topic);
        assert_eq!(message.source_type, Some(NotificationSourceType::Topic));
        assert_eq!(message.source_id, Some(f.topic));
        assert_eq!(message.occurred_at, ts(NOW));
        assert_eq!(message.expires_at, plus(ts(NOW), 1800));
    }
    let notifications: i64 = c
        .query_one("SELECT COUNT(*) FROM notifications", &[])
        .unwrap()
        .get(0);
    assert_eq!(notifications, 0);
    let summary = f.summary();
    assert_eq!(count(&summary, "targetCount"), 2);
    assert_eq!(count(&summary, "acceptedCount"), 2);
    assert!(f.lines.at_stage("topic_publication_result").is_empty());
    assert_eq!(f.lines.at_stage("topic_publication_summary").len(), 1);
}

// publishDueTopics_allUsersParticipating_skipsPublication
#[test]
fn 모든_회원이_오늘_주제에_참여했다면_sqs_발행_없이_빈_대상_집계를_남긴다() {
    let Some(f) = fixture() else { return };
    insert_participation(&mut f.client(), user(1), f.topic, "PENDING");

    f.service().publish_due_topics().unwrap();

    assert!(f.messages().is_empty());
    let summary = f.summary();
    assert_eq!(count(&summary, "targetCount"), 0);
    assert_eq!(count(&summary, "acceptedCount"), 0);
}

// 반려된 글은 참여로 보지 않는다(`excludedStatus`).
#[test]
fn 반려된_글만_있는_회원은_다시_수신_대상이다() {
    let Some(f) = fixture() else { return };
    insert_participation(&mut f.client(), user(1), f.topic, "REJECTED");

    f.service().publish_due_topics().unwrap();

    assert_eq!(f.messages().len(), 1);
}

// publishDueTopics_missingActiveTopic_skipsOccurrence
#[test]
fn 오늘_주제가_없거나_삭제되면_해당_회차는_발행을_생략한다() {
    for state in ["missing", "deleted"] {
        let Some(f) = fixture() else { return };
        let mut c = f.client();
        if state == "missing" {
            c.execute("DELETE FROM topics", &[]).unwrap();
        } else {
            c.execute("UPDATE topics SET deleted_at = $1", &[&ts(NOW)])
                .unwrap();
        }
        let service = f.service();

        service.publish_due_topics().unwrap();
        service.publish_due_topics().unwrap();

        assert!(f.messages().is_empty(), "{state}");
        let skipped = f.lines.at_stage("topic_execution_skipped");
        assert_eq!(skipped.len(), 1, "{state}: 같은 회차는 한 번만 시도한다");
        assert_eq!(skipped[0]["reason"], "NO_TOPIC_OR_SCHEDULE_CHANGED");
    }
}

// publishDueTopics_noRecipients_recordsEmptySummary
#[test]
fn 수신_대상이_없으면_빈_회차를_기록하고_sqs를_호출하지_않는다() {
    let Some(f) = fixture() else { return };
    f.client()
        .execute("UPDATE users SET topic_push_enabled = false", &[])
        .unwrap();

    f.service().publish_due_topics().unwrap();

    assert!(f.messages().is_empty());
    let summary = f.summary();
    assert_eq!(count(&summary, "targetCount"), 0);
    assert_eq!(count(&summary, "acceptedCount"), 0);
}

// publishDueTopics_multipleTimesAndDays_distinguishesOccurrences
#[test]
fn 여러_시각을_회차별로_한_번_실행하고_다음_날_다시_실행한다() {
    let Some(f) = fixture() else { return };
    insert_schedule(
        &mut f.client(),
        Uuid::new_v4(),
        "09:10:00",
        plus(ts(NOW), -3600),
    );
    let service = f.service();
    f.clock.set(plus(ts(NOW), 600));

    service.publish_due_topics().unwrap();
    service.publish_due_topics().unwrap();
    insert_topic(&mut f.client(), Uuid::new_v4(), "2026-10-13");
    f.clock.set(plus(ts(NOW), 86400 + 600));
    service.publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages.iter().map(|m| m.occurred_at).collect::<Vec<_>>(),
        vec![
            ts(NOW),
            plus(ts(NOW), 600),
            plus(ts(NOW), 86400),
            plus(ts(NOW), 87000)
        ]
    );
    let mut events: Vec<_> = messages.iter().map(|m| m.event_id).collect();
    events.sort();
    events.dedup();
    assert_eq!(events.len(), 4);
    assert_eq!(messages[0].expires_at, plus(ts(NOW), 1800));
}

// publishDueTopics_delayedExecution_observesOriginalDeadline
#[test]
fn 늦게_실행해도_원래_예정_시각의_기한을_유지하고_30분_정각부터_생략한다() {
    for seconds in [1799, 1800, 1801] {
        let Some(f) = fixture() else { return };
        f.clock.set(plus(ts(NOW), seconds));

        f.service().publish_due_topics().unwrap();

        if seconds >= 1800 {
            assert!(f.messages().is_empty(), "{seconds}");
            continue;
        }
        let messages = f.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].expires_at, plus(ts(NOW), 1800));
    }
}

// publishDueTopics_pastSettingChange_appliesNextDay
#[test]
fn 이미_지난_시각으로_생성_수정한_설정은_소급_실행하지_않고_다음_날_실행한다() {
    for change in ["created", "updated"] {
        let Some(f) = fixture() else { return };
        let mut c = f.client();
        if change == "created" {
            c.execute("DELETE FROM topic_push_schedules", &[]).unwrap();
            insert_schedule(&mut c, Uuid::new_v4(), "09:00:00", plus(ts(NOW), 1));
        } else {
            c.execute(
                "UPDATE topic_push_schedules SET updated_at = $1",
                &[&plus(ts(NOW), 1)],
            )
            .unwrap();
        }
        let service = f.service();
        f.clock.set(plus(ts(NOW), 300));

        service.publish_due_topics().unwrap();

        assert!(f.messages().is_empty(), "{change}");
        let tomorrow = Uuid::new_v4();
        insert_topic(&mut c, tomorrow, "2026-10-13");
        f.clock.set(plus(ts(NOW), 86400));
        service.publish_due_topics().unwrap();
        let messages = f.messages();
        assert_eq!(messages.len(), 1, "{change}");
        assert_eq!(messages[0].source_id, Some(tomorrow));
        assert_eq!(messages[0].occurred_at, plus(ts(NOW), 86400));
    }
}

// publishDueTopics_futureTimeUpdate_appliesUpcomingTime
#[test]
fn 앞으로_도래하는_시각으로_수정하면_당일_새_시각부터_실행한다() {
    let Some(f) = fixture() else { return };
    f.client()
        .execute(
            "UPDATE topic_push_schedules SET send_time = '09:10:00', updated_at = $1",
            &[&plus(ts(NOW), 300)],
        )
        .unwrap();
    let service = f.service();
    f.clock.set(plus(ts(NOW), 300));

    service.publish_due_topics().unwrap();
    assert!(f.messages().is_empty());
    f.clock.set(plus(ts(NOW), 600));
    service.publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].occurred_at, plus(ts(NOW), 600));
}

// publishDueTopics_deletedSetting_preservesPublishedMessageOnly
#[test]
fn 삭제된_설정은_실행하지_않고_이미_발행한_메시지의_기한은_바꾸지_않는다() {
    let Some(f) = fixture() else { return };
    let service = f.service();

    service.publish_due_topics().unwrap();
    f.client()
        .execute("DELETE FROM topic_push_schedules", &[])
        .unwrap();
    service.publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].expires_at, plus(ts(NOW), 1800));
}

// publishDueTopics_scheduleChangesBeforeExecution_skipsStaleSnapshot
#[test]
fn 설정_목록을_읽은_뒤에도_회차_실행_전에_삭제_수정된_설정을_다시_확인한다() {
    for change in ["deleted", "updated"] {
        let Some(f) = fixture() else { return };
        let later = Uuid::new_v4();
        insert_schedule(&mut f.client(), later, "09:05:00", plus(ts(NOW), -3600));
        f.clock.set(plus(ts(NOW), 300));
        let pool = f.pool.clone();
        let clock = f.clock.clone();
        let change = change.to_owned();
        f.publisher.on_publish(move |_| {
            // 첫 설정(09:00)을 발행하는 도중 둘째 설정(09:05)이 지워지거나 바뀐다.
            let mut conn = pool.get().unwrap();
            if change == "deleted" {
                conn.execute("DELETE FROM topic_push_schedules WHERE id = $1", &[&later])
                    .unwrap();
            } else {
                conn.execute(
                    "UPDATE topic_push_schedules SET send_time = '10:00:00', updated_at = $2 WHERE id = $1",
                    &[&later, &clock.now()],
                )
                .unwrap();
            }
        });

        f.service().publish_due_topics().unwrap();

        let messages = f.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].occurred_at, ts(NOW));
    }
}

// publishDueTopics_kstMidnight_usesKstDate
#[test]
fn utc_날짜가_달라도_kst_오늘_주제와_자정_설정을_사용한다() {
    let Some(f) = fixture() else { return };
    let midnight_topic = Uuid::new_v4();
    insert_topic(&mut f.client(), midnight_topic, "2026-10-13");
    let midnight = ts("2026-10-12T15:00:00Z");
    insert_schedule(
        &mut f.client(),
        Uuid::new_v4(),
        "00:00:00",
        plus(midnight, -60),
    );
    f.clock.set(midnight);

    f.service().publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].source_id, Some(midnight_topic));
    assert_eq!(messages[0].occurred_at, midnight);
}

// publishDueTopics_mixedResults_continuesAndRecordsWithoutRepublishing
#[test]
fn 한_회원의_실패가_다른_회원을_중단시키지_않고_실패_결과_불명을_구분하며_재발행하지_않는다() {
    let Some(f) = fixture() else { return };
    for n in 2..=4 {
        insert_user(&mut f.client(), user(n));
    }
    f.publisher.queue(vec![
        Ok(PushPublicationResult::accepted("sqs-message")),
        Ok(PushPublicationResult::retry("RequestThrottled")),
        Ok(PushPublicationResult::unknown("SQS_CLIENT_ERROR")),
        Err(failure("IllegalStateException")),
    ]);
    let service = f.service();

    service.publish_due_topics().unwrap();
    service.publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 4);
    let summary = f.summary();
    assert_eq!(count(&summary, "targetCount"), 4);
    assert_eq!(count(&summary, "acceptedCount"), 1);
    assert_eq!(count(&summary, "failedCount"), 1);
    assert_eq!(count(&summary, "unknownCount"), 2);
    assert_eq!(count(&summary, "expiredCount"), 0);
    let failures = f.lines.at_stage("topic_publication_result");
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["userId"], user(2).to_string());
    assert_eq!(failures[0]["result"], "FAILED");
    assert_eq!(failures[0]["errorCode"], "RequestThrottled");
    assert_eq!(failures[1]["userId"], user(3).to_string());
    assert_eq!(failures[1]["result"], "UNKNOWN");
    assert_eq!(failures[1]["errorCode"], "SQS_CLIENT_ERROR");
    for log in &failures {
        assert_eq!(log["eventId"], messages[0].event_id.to_string());
        assert_eq!(log["sourceId"], f.topic.to_string());
        assert!(log.get("messageId").is_none());
    }
    let unknown = f.lines.at_stage("topic_publication_unknown");
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0]["userId"], user(4).to_string());
    assert_eq!(unknown[0]["errorCode"], "IllegalStateException");
    assert_eq!(unknown[0]["eventId"], messages[0].event_id.to_string());
    assert_eq!(unknown[0]["sourceId"], f.topic.to_string());
    assert_eq!(unknown[0]["result"], "UNKNOWN");
    assert_eq!(f.lines.at_stage("topic_publication_summary").len(), 1);
    assert_eq!(
        f.lines.levels_of("주제 푸시 SQS 발행 결과"),
        ["WARN", "WARN"]
    );
    assert_eq!(f.lines.levels_of("주제 푸시 SQS 발행 집계"), ["INFO"]);
    assert_eq!(
        f.lines.levels_of("주제 푸시 SQS 발행 결과 확인 실패"),
        ["ERROR"]
    );
}

/// 요청 수를 세고 정해 둔 오류 응답을 주는 전송.
struct ErrorTransport {
    status: u16,
    code: &'static str,
    calls: AtomicUsize,
}

impl SqsTransport for ErrorTransport {
    fn send(
        &self,
        _request: &chalkak_aws::HttpRequest,
        _timeout: std::time::Duration,
    ) -> Result<SqsHttpResponse, AwsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(SqsHttpResponse {
            status: self.status,
            headers: vec![],
            body: format!("{{\"__type\":\"{}\"}}", self.code).into_bytes(),
        })
    }
}

struct TestCredentials;
impl CredentialsProvider for TestCredentials {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        Ok(Credentials {
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            session_token: None,
        })
    }
}

// publishDueTopics_sqsError_classifiesOutcomeWithoutRepublishing
#[test]
fn sqs_서버_오류는_수락_불명으로_명시적_거절은_실패로_집계하고_재발행하지_않는다() {
    // (상태, 코드, 불명, 실패)
    for (status, code, unknown, failed) in [
        (503, "InternalError", 1, 0),
        (429, "TooManyRequests", 0, 1),
        (403, "AccessDenied", 0, 1),
    ] {
        let Some(f) = fixture() else { return };
        let transport = Arc::new(ErrorTransport {
            status,
            code,
            calls: AtomicUsize::new(0),
        });
        let client = SqsClient::new(
            "ap-northeast-2",
            SqsEndpoint::custom("http://127.0.0.1:1").unwrap(),
            Arc::new(TestCredentials),
            transport.clone(),
            Arc::new(|| ts(NOW)),
        );
        let service = f.service_with(Arc::new(SqsPushMessagePublisher::new(
            client,
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/test",
        )));

        service.publish_due_topics().unwrap();
        service.publish_due_topics().unwrap();

        let summary = f.summary();
        assert_eq!(count(&summary, "acceptedCount"), 0, "{status}");
        assert_eq!(count(&summary, "unknownCount"), unknown, "{status}");
        assert_eq!(count(&summary, "failedCount"), failed, "{status}");
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1, "{status}");
    }
}

// publishDueTopics_deadlineDuringPublication_skipsRemainingRecipients
#[test]
fn 회차_발행_중_기한에_도달하면_남은_회원은_생략하고_다시_발행하지_않는다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    insert_user(&mut f.client(), user(3));
    let clock = f.clock.clone();
    f.publisher
        .on_publish(move |_| clock.set(plus(ts(NOW), 1800)));
    // 시계를 기한으로 옮기는 시각은 마지막 발행 뒤이므로 다시 조회해도 같은 회차가 아니다.
    let service = f.service();

    service.publish_due_topics().unwrap();
    service.publish_due_topics().unwrap();

    assert_eq!(f.messages().len(), 1);
    let summary = f.summary();
    assert_eq!(count(&summary, "acceptedCount"), 1);
    assert_eq!(count(&summary, "expiredCount"), 2);
}

// publishDueTopics_interruptedOccurrence_preservesBestEffortLimit
#[test]
fn 실행_중단_후_같은_인스턴스는_재발행하지_않고_재시작하면_중복_가능성이_남는다() {
    let Some(f) = fixture() else { return };
    insert_user(&mut f.client(), user(2));
    insert_user(&mut f.client(), user(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    f.publisher.on_publish(move |_| {
        // 두 번째 호출에서 프로세스가 끊긴 것처럼 패닉한다.
        assert!(
            counter.fetch_add(1, Ordering::SeqCst) != 1,
            "simulated process interruption"
        );
    });
    let service = f.service();

    let interrupted = catch_unwind(AssertUnwindSafe(|| service.publish_due_topics()));
    assert!(interrupted.is_err());
    service.publish_due_topics().unwrap();
    assert_eq!(f.messages().len(), 2);
    f.publisher.on_publish(|_| {});
    f.service().publish_due_topics().unwrap();

    let messages = f.messages();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[2].user_id, messages[0].user_id);
    assert_eq!(messages[2].occurred_at, messages[0].occurred_at);
    assert_ne!(messages[2].event_id, messages[0].event_id);
}
