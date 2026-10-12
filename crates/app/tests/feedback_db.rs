//! 피드백 제출과 `UsableUserPolicy.validateExisting`의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본에는 이 두 곳의 전용 Java 테스트가 없어(`FeedbackServiceTest` 등은 별도 이슈 범위) 하네스 시나리오
//! `platform/request-body-errors`가 보장하는 동작을 DB 수준에서 확인한다.

use std::sync::Arc;

use chalkak_app::auth::UsableUserPolicy;
use chalkak_app::feedback::domain::Feedback;
use chalkak_app::feedback::repository::FeedbackRepository;
use chalkak_app::feedback::service::FeedbackService;
use chalkak_app::security::{self, AUTHORITY_USER, Principal};
use chalkak_app::testing::TestDb;
use chalkak_core::FixedClock;
use jiff::Timestamp;
use uuid::Uuid;

const USER_ID: &str = "01990000-0000-7000-8000-000000000001";

fn insert_user(db: &TestDb, id: &str, deleted: bool) {
    let deleted_at = if deleted { "CURRENT_TIMESTAMP" } else { "NULL" };
    db.client()
        .batch_execute(&format!(
            "INSERT INTO users (id, email, signature_original_storage_key, deleted_at) \
             VALUES ('{id}', 'u-{id}@test.chalkak', 'chalkak/signatures/{id}/original.png', {deleted_at})"
        ))
        .unwrap();
}

fn now() -> Timestamp {
    "2026-10-12T03:00:00.123456Z".parse().unwrap()
}

fn as_user(id: &str) -> security::PrincipalGuard {
    security::enter(Principal {
        subject: id.to_owned(),
        authorities: vec![AUTHORITY_USER.to_owned()],
    })
}

#[test]
fn submit_stores_stripped_content_with_app_clock_timestamp() {
    let Some(db) = TestDb::create() else { return };
    insert_user(&db, USER_ID, false);
    let service = FeedbackService::new(db.pool(2), Arc::new(FixedClock::new(now())));

    let result = service
        .submit(USER_ID.parse().unwrap(), Some("  사진이 멈춰요\n"))
        .unwrap();

    let row = db
        .client()
        .query_one(
            "SELECT user_id, content, created_at FROM feedbacks WHERE id = $1",
            &[&result.feedback_id],
        )
        .unwrap();
    assert_eq!(
        row.get::<_, Uuid>("user_id"),
        USER_ID.parse::<Uuid>().unwrap()
    );
    assert_eq!(row.get::<_, String>("content"), "사진이 멈춰요");
    assert_eq!(row.get::<_, Timestamp>("created_at"), now());
    assert_eq!(result.created_at, now());
}

#[test]
fn submit_rejects_blank_content_before_touching_the_database() {
    let Some(db) = TestDb::create() else { return };
    insert_user(&db, USER_ID, false);
    let service = FeedbackService::new(db.pool(2), Arc::new(FixedClock::new(now())));

    let err = service
        .submit(USER_ID.parse().unwrap(), Some("   "))
        .unwrap_err();

    assert_eq!(err.status(), 400);
    assert_eq!(err.message(), "피드백 내용이 올바르지 않습니다.");
    let count: i64 = db
        .client()
        .query_one("SELECT count(*) FROM feedbacks", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

// UsableUserPolicy#validateExisting: 없거나 탈퇴한 회원은 401, 있으면 통과(정지도 통과).
#[test]
fn validate_existing_requires_a_present_user() {
    let Some(db) = TestDb::create() else { return };
    let present = "01990000-0000-7000-8000-000000000001";
    let withdrawn = "01990000-0000-7000-8000-000000000002";
    let missing = "01990000-0000-7000-8000-000000000003";
    insert_user(&db, present, false);
    insert_user(&db, withdrawn, true);
    let policy = UsableUserPolicy::new(db.pool(2));

    {
        let _guard = as_user(present);
        assert_eq!(
            policy.validate_existing().unwrap().user_id,
            present.parse::<Uuid>().unwrap()
        );
    }
    for id in [withdrawn, missing] {
        let _guard = as_user(id);
        let err = policy.validate_existing().unwrap_err();
        assert_eq!(err.status(), 401, "{id}");
        assert_eq!(err.message(), "유효하지 않은 인증 정보입니다.");
    }
}

// FeedbackRepositoryTest#save_newFeedback_assignsIdentifierAndCreatedAt
#[test]
fn save_assigns_identifier_and_created_at() {
    let Some(db) = TestDb::create() else { return };
    insert_user(&db, USER_ID, false);
    let mut feedback =
        Feedback::create(USER_ID.parse().unwrap(), Some("사진 업로드가 느려요.")).unwrap();
    let mut client = db.client();

    FeedbackRepository
        .save(&mut client, &mut feedback, now())
        .unwrap();

    assert!(feedback.id.is_some());
    assert_eq!(feedback.created_at, Some(now()));
    let row = client
        .query_one(
            "SELECT user_id, content FROM feedbacks WHERE id = $1",
            &[&feedback.id],
        )
        .unwrap();
    assert_eq!(
        row.get::<_, Uuid>("user_id"),
        USER_ID.parse::<Uuid>().unwrap()
    );
    assert_eq!(row.get::<_, String>("content"), "사진 업로드가 느려요.");
}

// FeedbackRepositoryTest#save_repeatedFeedback_storesEverySubmission
#[test]
fn a_user_can_submit_feedback_repeatedly() {
    let Some(db) = TestDb::create() else { return };
    insert_user(&db, USER_ID, false);
    let service = FeedbackService::new(db.pool(2), Arc::new(FixedClock::new(now())));

    let first = service
        .submit(USER_ID.parse().unwrap(), Some("첫 번째 의견"))
        .unwrap();
    let second = service
        .submit(USER_ID.parse().unwrap(), Some("두 번째 의견"))
        .unwrap();

    assert_ne!(first.feedback_id, second.feedback_id);
    let count: i64 = db
        .client()
        .query_one(
            "SELECT COUNT(*) FROM feedbacks WHERE user_id = $1",
            &[&USER_ID.parse::<Uuid>().unwrap()],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 2);
}

// FeedbackSubmissionIntegrationTest#submitFeedback_maxLengthSupplementaryCharacters_areStoredIntact
#[test]
fn max_length_supplementary_characters_are_stored_intact() {
    let Some(db) = TestDb::create() else { return };
    insert_user(&db, USER_ID, false);
    let service = FeedbackService::new(db.pool(2), Arc::new(FixedClock::new(now())));
    let content = "😀".repeat(1000);

    let result = service
        .submit(USER_ID.parse().unwrap(), Some(&content))
        .unwrap();

    let stored: String = db
        .client()
        .query_one(
            "SELECT content FROM feedbacks WHERE id = $1",
            &[&result.feedback_id],
        )
        .unwrap()
        .get(0);
    assert_eq!(stored, content);
}

// SuspendedUserAccessTest#submitFeedback_suspendedUser_isNotForbidden:
// 정지(BANNED) 회원도 `validateExisting`을 통과해 피드백을 낼 수 있다.
#[test]
fn a_banned_user_still_passes_the_existing_user_check() {
    let Some(db) = TestDb::create() else { return };
    insert_user(&db, USER_ID, false);
    db.client()
        .batch_execute("UPDATE users SET status = 'BANNED'")
        .unwrap();
    let policy = UsableUserPolicy::new(db.pool(2));
    let _guard = as_user(USER_ID);
    assert!(policy.validate_existing().is_ok());
}
