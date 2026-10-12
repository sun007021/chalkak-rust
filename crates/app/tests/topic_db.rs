//! 주제 저장소·서비스의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `TopicRepositoryTest`, `TopicRepositoryImplTest`, `TopicRepositoryImplIntegrationTest`, `TopicServiceTest`를 옮겼다.

use std::sync::Arc;

use chalkak_app::testing::TestDb;
use chalkak_app::topic::domain::{ParticipationPeriod, Topic, TopicPhase};
use chalkak_app::topic::repository::{SaveError, TopicRepository};
use chalkak_app::topic::service::TopicService;
use chalkak_app::web::binding::LocalDate;
use chalkak_core::FixedClock;
use jiff::Timestamp;
use jiff::civil::date;
use uuid::Uuid;

const TOPIC_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6570b2";
const TITLE: &str = "오늘 가장 기억에 남은 순간";

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn id(s: &str) -> Uuid {
    s.parse().unwrap()
}

/// TopicRepositoryTest.setUp
fn insert_topic(db: &TestDb) {
    db.client()
        .batch_execute(&format!(
            "INSERT INTO topics (id, title, topic_date, starts_at, ends_at, created_at, updated_at) \
             VALUES ('{TOPIC_ID}', '{TITLE}', '2026-08-12', '2026-08-11T15:00:00Z', '2026-08-12T15:00:00Z', \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        ))
        .unwrap();
}

// TopicRepositoryTest#findActiveByTopicDate_existingTopic_returnsTopic
#[test]
fn find_active_by_topic_date_existing_topic_returns_topic() {
    let Some(db) = TestDb::create() else { return };
    insert_topic(&db);
    let mut client = db.client();

    let topic = TopicRepository
        .find_active_by_topic_date(&mut client, date(2026, 8, 12))
        .unwrap()
        .expect("주제가 있다");

    assert_eq!(topic.id, Some(id(TOPIC_ID)));
    assert_eq!(topic.title, TITLE);
    assert_eq!(topic.topic_date, date(2026, 8, 12));
    assert_eq!(topic.phase_at(ts("2026-08-12T03:00:00Z")), TopicPhase::Open);
}

// TopicRepositoryTest#findActiveByTopicDate_deletedTopic_returnsEmpty
#[test]
fn find_active_by_topic_date_deleted_topic_returns_empty() {
    let Some(db) = TestDb::create() else { return };
    insert_topic(&db);
    let mut client = db.client();
    client
        .execute(
            "UPDATE topics SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1",
            &[&id(TOPIC_ID)],
        )
        .unwrap();

    let result = TopicRepository
        .find_active_by_topic_date(&mut client, date(2026, 8, 12))
        .unwrap();

    assert!(result.is_none());
}

// TopicRepositoryTest#findActiveByTopicDate_noTopicOnDate_returnsEmpty
#[test]
fn find_active_by_topic_date_no_topic_on_date_returns_empty() {
    let Some(db) = TestDb::create() else { return };
    insert_topic(&db);
    let mut client = db.client();

    let result = TopicRepository
        .find_active_by_topic_date(&mut client, date(2026, 8, 13))
        .unwrap();

    assert!(result.is_none());
}

fn new_topic(title: &str, topic_date: jiff::civil::Date, starts: &str, ends: &str) -> Topic {
    Topic::create(
        title,
        topic_date,
        ParticipationPeriod::new(ts(starts), ts(ends)).unwrap(),
        ts("2026-08-28T03:00:00Z"),
    )
    .unwrap()
}

// TopicRepositoryImplIntegrationTest#saveAndFlush_overlappingPeriodInDatabase_throwsBusinessException
#[test]
fn save_and_flush_overlapping_period_in_database_throws_business_exception() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    client
        .batch_execute(
            "INSERT INTO topics (id, title, topic_date, starts_at, ends_at, created_at, updated_at) \
             VALUES ('0198fd20-0000-7000-8000-000000000011', '기존 주제', DATE '2026-08-30', \
             TIMESTAMPTZ '2026-08-29 15:00:00+00', TIMESTAMPTZ '2026-08-30 15:00:00+00', \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        )
        .unwrap();
    let mut overlapping = new_topic(
        "겹치는 주제",
        date(2026, 8, 29),
        "2026-08-29T14:00:00Z",
        "2026-08-29T16:00:00Z",
    );

    let err = TopicRepository
        .save_and_flush(&mut client, &mut overlapping, ts("2026-08-28T03:00:00Z"))
        .unwrap_err();

    let SaveError::Business(error) = err else {
        panic!("업무 오류여야 한다: {err:?}");
    };
    assert_eq!(error.code.as_str(), "BUSINESS_ERROR");
    assert_eq!(error.message, "다른 주제의 참여 기간과 겹칩니다.");
}

// TopicRepositoryImplTest#saveAndFlush_topicDateUniqueViolation_throwsBusinessException
// (원본은 Mockito로 제약 이름만 흉내 냈다. 여기서는 실제 DB의 유니크 인덱스를 위반시킨다.)
#[test]
fn save_and_flush_topic_date_unique_violation_throws_business_exception() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let mut first = new_topic(
        "첫 주제",
        date(2026, 8, 30),
        "2026-08-29T15:00:00Z",
        "2026-08-30T15:00:00Z",
    );
    TopicRepository
        .save_and_flush(&mut client, &mut first, ts("2026-08-28T03:00:00Z"))
        .unwrap();
    let mut same_date = new_topic(
        "같은 날짜",
        date(2026, 8, 30),
        "2026-08-29T15:00:00Z",
        "2026-08-30T15:00:00Z",
    );
    // 기간이 겹치면 제외 제약 위반이 되므로, 첫 주제의 기간을 옮겨 날짜 유니크 인덱스만 위반하게 만든다.
    client
        .execute(
            "UPDATE topics SET starts_at = '2026-08-20T00:00:00Z', ends_at = '2026-08-21T00:00:00Z'",
            &[],
        )
        .unwrap();

    let err = TopicRepository
        .save_and_flush(&mut client, &mut same_date, ts("2026-08-28T03:00:00Z"))
        .unwrap_err();

    let SaveError::Business(error) = err else {
        panic!("업무 오류여야 한다: {err:?}");
    };
    assert_eq!(error.message, "해당 날짜의 주제가 이미 존재합니다.");
}

// TopicRepositoryImplTest#saveAndFlush_unknownIntegrityViolation_propagatesOriginalException
#[test]
fn save_and_flush_unknown_integrity_violation_propagates_original_exception() {
    let Some(db) = TestDb::create() else { return };
    insert_topic(&db);
    let mut client = db.client();
    // 제약 이름이 없는 DB 오류(제목이 컬럼 길이 255자를 넘음)는 업무 오류로 바꾸지 않고 그대로 전파한다.
    let mut topic = new_topic(
        "제목",
        date(2026, 8, 30),
        "2026-08-29T15:00:00Z",
        "2026-08-30T15:00:00Z",
    );
    topic.title = "가".repeat(300);

    let err = TopicRepository
        .save_and_flush(&mut client, &mut topic, ts("2026-08-28T03:00:00Z"))
        .unwrap_err();

    assert!(matches!(err, SaveError::Db(_)), "{err:?}");
}

fn service(db: &TestDb, now: &str) -> TopicService {
    TopicService::new(db.pool(2), Arc::new(FixedClock::new(ts(now))))
}

/// TopicServiceTest.insertTopic: 한국 날짜의 하루가 참여 기간이다.
fn insert_today_topic(db: &TestDb, topic_date: &str, starts: &str, ends: &str) {
    db.client()
        .batch_execute(&format!(
            "INSERT INTO topics (id, title, topic_date, starts_at, ends_at, created_at, updated_at) \
             VALUES ('{TOPIC_ID}', '{TITLE}', '{topic_date}', '{starts}', '{ends}', \
             CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)"
        ))
        .unwrap();
}

// TopicServiceTest#getTopic_openedTopic_returnsTopicDetail
#[test]
fn get_topic_opened_topic_returns_topic_detail() {
    let Some(db) = TestDb::create() else { return };
    insert_today_topic(
        &db,
        "2026-10-12",
        "2026-10-11T15:00:00Z",
        "2026-10-12T15:00:00Z",
    );
    let service = service(&db, "2026-10-12T03:00:00Z");

    let detail = service
        .get_topic(LocalDate::Valid(date(2026, 10, 12)))
        .unwrap();

    assert_eq!(detail.id, id(TOPIC_ID));
    assert_eq!(detail.title, TITLE);
    assert_eq!(detail.topic_date, date(2026, 10, 12));
    assert_eq!(detail.phase, TopicPhase::Open);
}

// TopicServiceTest#getTopic_futureDate_throwsBusinessException
#[test]
fn get_topic_future_date_throws_business_exception() {
    let Some(db) = TestDb::create() else { return };
    insert_today_topic(
        &db,
        "2026-10-12",
        "2026-10-11T15:00:00Z",
        "2026-10-12T15:00:00Z",
    );
    let service = service(&db, "2026-10-12T03:00:00Z");

    let err = service
        .get_topic(LocalDate::Valid(date(2026, 10, 13)))
        .unwrap_err();

    assert_eq!(err.status(), 400);
    assert_eq!(err.code().as_str(), "BUSINESS_ERROR");
    assert_eq!(err.message(), "미래의 주제는 조회할 수 없습니다.");
}

// TopicServiceTest#getTopic_noTopicOnDate_throwsNotFoundException
#[test]
fn get_topic_no_topic_on_date_throws_not_found_exception() {
    let Some(db) = TestDb::create() else { return };
    insert_today_topic(
        &db,
        "2026-10-12",
        "2026-10-11T15:00:00Z",
        "2026-10-12T15:00:00Z",
    );
    let service = service(&db, "2026-10-12T03:00:00Z");

    let err = service
        .get_topic(LocalDate::Valid(date(2026, 10, 11)))
        .unwrap_err();

    assert_eq!(err.status(), 404);
    assert_eq!(err.code().as_str(), "BUSINESS_ERROR");
    assert_eq!(err.message(), "주제를 찾을 수 없습니다.");
}
