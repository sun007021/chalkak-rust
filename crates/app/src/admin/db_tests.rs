//! 감사 로그·저장소·동시성처럼 여러 하위 모듈에 걸치는 DB 테스트.
//!
//! 원본의 `AdminAuditLog*Test`, `AdminRepositoryTest`, `Admin*ConcurrencyTest`, `AdminPostNotificationTransactionTest`
//! 가 확인하던 것을 실제 DB로 확인한다. 원본은 Spring 트랜잭션 전파와 JPA 슬라이스를 쓰지만 여기서는 트랜잭션을
//! 함수 인자로 명시하므로, 같은 보장(업무 변경과 감사 로그가 함께 커밋·롤백)을 결과로 확인한다.

use std::sync::Barrier;
use std::thread;

use jiff::Timestamp;
use uuid::Uuid;

use super::audit::domain::{AdminAction, AdminTargetType, AuditSnapshot, text_value};
use super::audit::repository::AuditLogSort;
use super::audit::service::{
    AuditLogCommand, AuditLogQuery, AuditLogQueryService, create_audit_log,
};
use super::auth::domain::Admin;
use super::auth::repository::AdminRepository;
use super::post::repository::ModerationStatus;
use super::post::service::AdminPostCommandService;
use super::support::{AdminDeps, in_transaction};
use super::testing::{deps, deps_at, insert_admin, insert_post, insert_topic, insert_user};
use super::topic::{AdminTopicCommandService, TopicMutation};
use super::topic::{AdminTopicQueryService, AdminTopicSort};
use super::user::repository::UserStatus;
use super::user::service::AdminUserStatusService;
use crate::testing::TestDb;
use crate::topic::domain::TopicPhase;

fn ban_command(actor: Uuid, user: Uuid) -> AuditLogCommand {
    AuditLogCommand {
        actor_admin_id: actor,
        action: AdminAction::UserBanned,
        target_type: AdminTargetType::User,
        target_id: user,
        reason: Some("스팸".to_owned()),
        before_state: AuditSnapshot::of(vec![("status", text_value("ACTIVE"))]).unwrap(),
        after_state: AuditSnapshot::of(vec![("status", text_value("BANNED"))]).unwrap(),
    }
}

fn ts(value: &str) -> Timestamp {
    value.parse().unwrap()
}

fn save_log(deps: &AdminDeps, actor: Uuid, user: Uuid, at: &str) {
    let mut conn = deps.pool.get().unwrap();
    in_transaction(&mut conn, |tx| {
        create_audit_log(tx, ban_command(actor, user), ts(at)).map(|_| ())
    })
    .unwrap();
}

fn audit_count(db: &TestDb) -> i64 {
    db.client()
        .query_one("SELECT count(*) FROM admin_audit_logs", &[])
        .unwrap()
        .get(0)
}

// AdminAuditLogRepositoryTest: save_validAuditLog_persistsPostgresTypes, save_sameRequestId_persistsMultipleAuditLogs
#[test]
fn 감사_로그는_유형과_jsonb를_저장하고_한_요청에서_여러_건을_남긴다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    save_log(&deps, admin, user, "2026-10-12T01:00:00Z");
    save_log(&deps, admin, user, "2026-10-12T01:00:00Z");
    let row = db
        .client()
        .query_one(
            "SELECT action::text, target_type::text, before_state::text, after_state::text, reason \
             FROM admin_audit_logs ORDER BY id LIMIT 1",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "USER_BANNED");
    assert_eq!(row.get::<_, String>(1), "USER");
    assert!(row.get::<_, String>(2).contains("ACTIVE"));
    assert!(row.get::<_, String>(3).contains("BANNED"));
    assert_eq!(row.get::<_, String>(4), "스팸");
    assert_eq!(audit_count(&db), 2);
}

// AdminAuditLogRepositoryTest: save_unknownActorAdminId_throwsDataIntegrityViolationException,
// AdminAuditLogServiceTest: createAuditLog_auditPersistenceFailure_rollsBackBusinessChange
#[test]
fn 작업자가_없으면_감사_로그가_실패하고_업무_변경도_롤백한다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    let mut conn = deps.pool.get().unwrap();
    let result = in_transaction(&mut conn, |tx| {
        tx.execute(
            "UPDATE users SET status = 'BANNED'::user_status WHERE id = $1",
            &[&user],
        )
        .map_err(chalkak_db::DbError::from)?;
        create_audit_log(
            tx,
            ban_command(Uuid::new_v4(), user),
            ts("2026-10-12T01:00:00Z"),
        )
        .map(|_| ())
    });
    assert!(result.is_err());
    let status: String = db
        .client()
        .query_one("SELECT status::text FROM users WHERE id = $1", &[&user])
        .unwrap()
        .get(0);
    assert_eq!(status, "ACTIVE");
    assert_eq!(audit_count(&db), 0);
}

// AdminAuditLogServiceTest: createAuditLog_businessFailure_rollsBackChangeAndAuditLog
#[test]
fn 업무_실패면_감사_로그도_함께_사라진다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    let mut conn = deps.pool.get().unwrap();
    let result: Result<(), _> = in_transaction(&mut conn, |tx| {
        create_audit_log(tx, ban_command(admin, user), ts("2026-10-12T01:00:00Z"))?;
        Err(super::support::business("업무 실패"))
    });
    assert!(result.is_err());
    assert_eq!(audit_count(&db), 0);
}

// AdminAuditLogQueryRepositoryTest: deleteActor_withAuditHistory_isRejectedToPreserveActorIdentity
#[test]
fn 감사_로그가_있는_관리자는_삭제할_수_없다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    save_log(&deps, admin, user, "2026-10-12T01:00:00Z");
    let result = db
        .client()
        .execute("DELETE FROM admins WHERE id = $1", &[&admin]);
    assert!(result.is_err());
}

// AdminAuditLogQueryRepositoryTest: findAuditLogs_combinedFiltersAndInclusiveBoundary_returnsMatchingDeletedTargetHistory,
// findAuditLogs_sameOccurredAt_usesIdForStablePagination; AdminAuditLogQueryServiceTest: getAuditLogs_allFilters_returnsReadOnlyPage
#[test]
fn 감사_로그_조회는_경계를_포함하고_같은_시각이면_id로_안정적으로_나눈다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let other = insert_admin(&mut db.client(), "reviewer");
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    save_log(&deps, admin, user, "2026-10-12T01:00:00Z");
    save_log(&deps, admin, user, "2026-10-12T01:00:00Z");
    save_log(&deps, admin, user, "2026-10-12T02:00:00Z");
    save_log(&deps, other, user, "2026-10-12T03:00:00Z");
    let service = AuditLogQueryService::new(deps.clone());
    let query = AuditLogQuery {
        admin_id: Some(admin),
        action: Some(AdminAction::UserBanned),
        target_type: Some(AdminTargetType::User),
        target_id: Some(user),
        occurred_from: Some(ts("2026-10-12T01:00:00Z")),
        occurred_to: Some(ts("2026-10-12T02:00:00Z")),
        sort: AuditLogSort::OccurredAtAsc,
    };
    let all = service.get_audit_logs(&query, 1, 10).unwrap();
    assert_eq!(all.items.len(), 3);
    assert!(!all.has_next);
    let first = service.get_audit_logs(&query, 1, 2).unwrap();
    let second = service.get_audit_logs(&query, 2, 2).unwrap();
    assert!(first.has_next);
    let mut paged: Vec<Uuid> = first.items.iter().map(|l| l.audit_log_id).collect();
    paged.extend(second.items.iter().map(|l| l.audit_log_id));
    let ordered: Vec<Uuid> = all.items.iter().map(|l| l.audit_log_id).collect();
    assert_eq!(paged, ordered);
    assert_eq!(all.items[0].actor_username, "operator");
}

// AdminAuditLogQueryServiceTest: getAuditLogs_reversedPeriod_throwsBusinessException, getAuditLogs_excessiveOffset_throwsBusinessException
#[test]
fn 감사_로그_조회의_기간과_오프셋이_틀리면_거절한다() {
    let Some(db) = TestDb::create() else { return };
    let service = AuditLogQueryService::new(deps(&db));
    let mut query = AuditLogQuery {
        admin_id: None,
        action: None,
        target_type: None,
        target_id: None,
        occurred_from: Some(ts("2026-10-12T02:00:00Z")),
        occurred_to: Some(ts("2026-10-12T01:00:00Z")),
        sort: AuditLogSort::OccurredAtDesc,
    };
    assert!(service.get_audit_logs(&query, 1, 10).is_err());
    query.occurred_from = None;
    query.occurred_to = None;
    assert!(service.get_audit_logs(&query, i32::MAX, 100).is_err());
    assert!(service.get_audit_logs(&query, 0, 10).is_err());
}

// AdminRepositoryTest
#[test]
fn 관리자는_사용자명으로_찾고_중복_저장은_실패한다() {
    let Some(db) = TestDb::create() else { return };
    let hash = "$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6";
    let mut client = db.client();
    let mut admin = Admin::create("operator", hash).unwrap();
    AdminRepository.save(&mut client, &mut admin).unwrap();
    let found = AdminRepository
        .find_by_username(&mut client, "operator")
        .unwrap()
        .unwrap();
    assert_eq!(found.id, admin.id);
    assert_eq!(found.password_hash, hash);
    let mut duplicate = Admin::create("operator", hash).unwrap();
    assert!(AdminRepository.save(&mut client, &mut duplicate).is_err());
}

fn topic_request(day: &str, starts: &str, ends: &str) -> TopicMutation {
    TopicMutation {
        title: "주제".to_owned(),
        topic_date: day.parse().unwrap(),
        starts_at: ts(starts),
        ends_at: ts(ends),
    }
}

// AdminPostModerationConcurrencyTest: moderate_auditActorForeignKeyFailure_rollsBackPostTransition,
// AdminPostNotificationTransactionTest: moderate_rolledBackTransaction_removesNotificationAndAudit
#[test]
fn 검수_중_감사_로그가_실패하면_게시물_상태와_알림이_남지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let author = insert_user(&mut db.client(), "a@example.com", "ACTIVE");
    let topic = insert_topic(
        &mut db.client(),
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let post = insert_post(&mut db.client(), author, topic, "PENDING");
    let result = AdminPostCommandService::new(deps).moderate(
        post,
        Uuid::new_v4(),
        ModerationStatus::Approved,
        None,
    );
    assert!(result.is_err());
    let status: String = db
        .client()
        .query_one(
            "SELECT moderation_status::text FROM posts WHERE id = $1",
            &[&post],
        )
        .unwrap()
        .get(0);
    assert_eq!(status, "PENDING");
    let notifications: i64 = db
        .client()
        .query_one("SELECT count(*) FROM notifications", &[])
        .unwrap()
        .get(0);
    assert_eq!(notifications, 0);
}

// AdminPostCommandDeletionTest: deletePost_auditActorForeignKeyFailure_rollsBackEveryChange
#[test]
fn 삭제_중_감사_로그가_실패하면_게시물과_사진이_그대로다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let author = insert_user(&mut db.client(), "a@example.com", "ACTIVE");
    let topic = insert_topic(
        &mut db.client(),
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let post = insert_post(&mut db.client(), author, topic, "PENDING");
    assert!(
        AdminPostCommandService::new(deps)
            .delete_post(post, Uuid::new_v4(), "사유")
            .is_err()
    );
    let row = db
        .client()
        .query_one(
            "SELECT p.deleted_at IS NULL, ph.deleted_at IS NULL FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = $1",
            &[&post],
        )
        .unwrap();
    assert!(row.get::<_, bool>(0) && row.get::<_, bool>(1));
}

// AdminUserStatusConcurrencyTest: updateStatus_auditActorForeignKeyFailure_rollsBackUserTransition
#[test]
fn 차단_중_감사_로그가_실패하면_사용자_상태가_그대로다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    let result = AdminUserStatusService::new(deps).update_status(
        user,
        Uuid::new_v4(),
        UserStatus::Banned,
        "스팸",
    );
    assert!(result.is_err());
    let status: String = db
        .client()
        .query_one("SELECT status::text FROM users WHERE id = $1", &[&user])
        .unwrap()
        .get(0);
    assert_eq!(status, "ACTIVE");
}

// AdminTopicConcurrencyTest: createTopic_auditActorForeignKeyFailure_rollsBackTopic
#[test]
fn 주제_생성_중_감사_로그가_실패하면_주제도_없다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let request = topic_request("2026-10-14", "2026-10-13T15:00:00Z", "2026-10-14T15:00:00Z");
    assert!(
        AdminTopicCommandService::new(deps)
            .create_topic(Uuid::new_v4(), &request)
            .is_err()
    );
    let topics: i64 = db
        .client()
        .query_one("SELECT count(*) FROM topics", &[])
        .unwrap()
        .get(0);
    assert_eq!(topics, 0);
}

fn race<T: Send>(a: impl FnOnce() -> T + Send, b: impl FnOnce() -> T + Send) -> (T, T) {
    let barrier = Barrier::new(2);
    thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            a()
        });
        let second = scope.spawn(|| {
            barrier.wait();
            b()
        });
        (first.join().unwrap(), second.join().unwrap())
    })
}

// AdminUserStatusConcurrencyTest: updateStatus_concurrentBan_changesExactlyOnce
#[test]
fn 같은_사용자를_동시에_차단하면_한_번만_바뀐다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let user = insert_user(&mut db.client(), "u@example.com", "ACTIVE");
    let service = AdminUserStatusService::new(deps);
    let (a, b) = race(
        || service.update_status(user, admin, UserStatus::Banned, "스팸"),
        || service.update_status(user, admin, UserStatus::Banned, "스팸"),
    );
    assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    assert_eq!(audit_count(&db), 1);
}

// AdminPostModerationConcurrencyTest: moderate_concurrentApproveAndReject_decidesExactlyOnce
#[test]
fn 승인과_거절이_동시에_오면_하나만_확정한다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let reviewer = insert_admin(&mut db.client(), "reviewer");
    let author = insert_user(&mut db.client(), "a@example.com", "ACTIVE");
    let topic = insert_topic(
        &mut db.client(),
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let post = insert_post(&mut db.client(), author, topic, "PENDING");
    let service = AdminPostCommandService::new(deps);
    let (a, b) = race(
        || service.moderate(post, admin, ModerationStatus::Approved, None),
        || service.moderate(post, reviewer, ModerationStatus::Rejected, Some("부적절")),
    );
    assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    assert_eq!(audit_count(&db), 1);
}

// AdminPostModerationDeletionConcurrencyTest: moderateAndDelete_concurrentSamePendingPost_deletesConsistently
#[test]
fn 승인과_삭제가_겹쳐도_교착_없이_삭제_상태로_수렴한다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let reviewer = insert_admin(&mut db.client(), "reviewer");
    let author = insert_user(&mut db.client(), "a@example.com", "ACTIVE");
    let topic = insert_topic(
        &mut db.client(),
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let post = insert_post(&mut db.client(), author, topic, "PENDING");
    let service = AdminPostCommandService::new(deps);
    let (_, deleted) = race(
        || {
            service
                .moderate(post, admin, ModerationStatus::Approved, None)
                .map(|_| ())
        },
        || service.delete_post(post, reviewer, "삭제"),
    );
    assert!(deleted.is_ok());
    let row = db
        .client()
        .query_one(
            "SELECT deleted_at IS NOT NULL FROM posts WHERE id = $1",
            &[&post],
        )
        .unwrap();
    assert!(row.get::<_, bool>(0));
}

// AdminTopicConcurrencyTest: createTopic_sameDateConcurrently_createsExactlyOnce
#[test]
fn 같은_날짜의_주제를_동시에_만들면_하나만_저장한다() {
    let Some(db) = TestDb::create() else { return };
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let service = AdminTopicCommandService::new(deps);
    let request = topic_request("2026-10-14", "2026-10-13T15:00:00Z", "2026-10-14T15:00:00Z");
    let (a, b) = race(
        || service.create_topic(admin, &request).map(|_| ()),
        || service.create_topic(admin, &request).map(|_| ()),
    );
    assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1);
    let topics: i64 = db
        .client()
        .query_one("SELECT count(*) FROM topics", &[])
        .unwrap()
        .get(0);
    assert_eq!(topics, 1);
}

// AdminTopicCommandServiceTest: createTopic_adjacentPeriod_savesTopic, createTopic_overlappingDeletedTopic_savesTopic
#[test]
fn 기간이_맞닿거나_삭제된_주제와_겹치는_새_주제는_만들_수_있다() {
    let Some(db) = TestDb::create() else { return };
    let admin = insert_admin(&mut db.client(), "operator");
    let service = AdminTopicCommandService::new(deps(&db));
    service
        .create_topic(
            admin,
            &topic_request("2026-10-14", "2026-10-13T15:00:00Z", "2026-10-14T15:00:00Z"),
        )
        .unwrap();
    let adjacent = topic_request("2026-10-15", "2026-10-14T15:00:00Z", "2026-10-15T15:00:00Z");
    let created = service.create_topic(admin, &adjacent).unwrap();
    service
        .delete_topic(created.topic_id, admin, "취소")
        .unwrap();
    assert!(service.create_topic(admin, &adjacent).is_ok());
}

// AdminTopicCommandServiceTest: insertTopic_overlappingPeriod_violatesDatabaseConstraint
#[test]
fn 날짜가_달라도_기간이_겹치면_db가_저장하지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    insert_topic(
        &mut client,
        "첫째",
        "2026-10-14",
        "2026-10-13T15:00:00Z",
        "2026-10-14T15:00:00Z",
    );
    let result = client.execute(
        "INSERT INTO topics (title, topic_date, starts_at, ends_at) \
         VALUES ('둘째', '2026-10-15', '2026-10-14T00:00:00Z', '2026-10-15T15:00:00Z')",
        &[],
    );
    assert!(result.is_err());
}

// AdminTopicCommandServiceTest: updateTopic_overlappingPeriod_throwsBusinessException,
// updateTopic_openTopicWithOverlappingPeriod_throwsStateChanged, updateTopic_openTopic_throwsStateChanged
#[test]
fn 수정한_기간이_다른_주제와_겹치면_거절하고_참여_중이면_상태_오류가_먼저다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "operator");
    let service = AdminTopicCommandService::new(deps(&db));
    let first = service
        .create_topic(
            admin,
            &topic_request("2026-10-14", "2026-10-13T15:00:00Z", "2026-10-14T15:00:00Z"),
        )
        .unwrap();
    service
        .create_topic(
            admin,
            &topic_request("2026-10-16", "2026-10-15T15:00:00Z", "2026-10-16T15:00:00Z"),
        )
        .unwrap();
    let stretched = topic_request("2026-10-14", "2026-10-13T15:00:00Z", "2026-10-15T20:00:00Z");
    let e = service
        .update_topic(first.topic_id, admin, &stretched)
        .unwrap_err();
    assert_eq!(e.message(), "다른 주제의 참여 기간과 겹칩니다.");
    let open = insert_topic(
        &mut client,
        "열림",
        "2026-10-12",
        "2026-10-11T15:00:00Z",
        "2026-10-12T15:00:00Z",
    );
    let overlapping = topic_request("2026-10-12", "2026-10-11T15:00:00Z", "2026-10-13T20:00:00Z");
    let e = service.update_topic(open, admin, &overlapping).unwrap_err();
    assert_eq!(e.code().as_str(), "RESOURCE_STATE_CHANGED");
}

fn topic_fixture(db: &TestDb) {
    let mut client = db.client();
    insert_topic(
        &mut client,
        "가",
        "2026-10-10",
        "2026-10-09T15:00:00Z",
        "2026-10-10T15:00:00Z",
    );
    insert_topic(
        &mut client,
        "나",
        "2026-10-11",
        "2026-10-10T15:00:00Z",
        "2026-10-11T15:00:00Z",
    );
    insert_topic(
        &mut client,
        "다",
        "2026-10-12",
        "2026-10-11T15:00:00Z",
        "2026-10-12T15:00:00Z",
    );
}

// AdminTopicQueryServiceTest: getTopic_phaseBoundaries_derivesFromClock,
// AdminTopicQueryRepositoryTest: findTopics_phaseAtBoundary_returnsMatchingTopic
#[test]
fn 시작_시각은_공개_중이고_종료_시각은_종료다() {
    let Some(db) = TestDb::create() else { return };
    let id = insert_topic(
        &mut db.client(),
        "경계",
        "2026-10-12",
        "2026-10-11T15:00:00Z",
        "2026-10-12T15:00:00Z",
    );
    let phase_at = |now: &str| {
        let service = AdminTopicQueryService::new(deps_at(&db, now));
        service.get_topic(id).unwrap().phase.as_str()
    };
    assert_eq!(phase_at("2026-10-11T14:59:59Z"), "BEFORE_OPEN");
    assert_eq!(phase_at("2026-10-11T15:00:00Z"), "OPEN");
    assert_eq!(phase_at("2026-10-12T15:00:00Z"), "CLOSED");
    let service = AdminTopicQueryService::new(deps_at(&db, "2026-10-12T15:00:00Z"));
    let closed = service
        .get_topics(
            Some(TopicPhase::Closed),
            None,
            None,
            AdminTopicSort::TopicDateDesc,
            1,
            10,
        )
        .unwrap();
    assert_eq!(closed.items.len(), 1);
}

// AdminTopicQueryRepositoryTest: findTopics_dateBounds_returnsInclusiveRange, findTopics_eachSort_returnsStablePages
#[test]
fn 주제_목록은_날짜_경계를_포함하고_네_정렬이_페이지를_안정적으로_나눈다() {
    let Some(db) = TestDb::create() else { return };
    topic_fixture(&db);
    let service = AdminTopicQueryService::new(deps(&db));
    let date = |s: &str| -> Option<jiff::civil::Date> { Some(s.parse().unwrap()) };
    let bounded = service
        .get_topics(
            None,
            date("2026-10-11"),
            date("2026-10-12"),
            AdminTopicSort::TopicDateAsc,
            1,
            10,
        )
        .unwrap();
    assert_eq!(bounded.items.len(), 2);
    let from_only = service
        .get_topics(
            None,
            date("2026-10-12"),
            None,
            AdminTopicSort::TopicDateAsc,
            1,
            10,
        )
        .unwrap();
    assert_eq!(from_only.items.len(), 1);
    let to_only = service
        .get_topics(
            None,
            None,
            date("2026-10-10"),
            AdminTopicSort::TopicDateAsc,
            1,
            10,
        )
        .unwrap();
    assert_eq!(to_only.items.len(), 1);
    for sort in [
        AdminTopicSort::TopicDateDesc,
        AdminTopicSort::TopicDateAsc,
        AdminTopicSort::CreatedAtDesc,
        AdminTopicSort::CreatedAtAsc,
    ] {
        let all = service.get_topics(None, None, None, sort, 1, 10).unwrap();
        let first = service.get_topics(None, None, None, sort, 1, 2).unwrap();
        let second = service.get_topics(None, None, None, sort, 2, 2).unwrap();
        assert!(first.has_next && !second.has_next);
        let paged: Vec<Uuid> = first
            .items
            .iter()
            .chain(second.items.iter())
            .map(|t| t.topic_id)
            .collect();
        let whole: Vec<Uuid> = all.items.iter().map(|t| t.topic_id).collect();
        assert_eq!(paged, whole);
    }
}

// AdminTopicQueryServiceTest: getTopics_offsetWithinIntegerRange_returnsEmptyPage
#[test]
fn 허용되는_마지막_페이지는_빈_목록이고_그_다음은_거절한다() {
    let Some(db) = TestDb::create() else { return };
    let service = AdminTopicQueryService::new(deps(&db));
    let last = service
        .get_topics(
            None,
            None,
            None,
            AdminTopicSort::TopicDateDesc,
            21_474_837,
            100,
        )
        .unwrap();
    assert!(last.items.is_empty());
    assert!(
        service
            .get_topics(
                None,
                None,
                None,
                AdminTopicSort::TopicDateDesc,
                21_474_838,
                100
            )
            .is_err()
    );
}

// AdminAuditLogRepositoryTest: migration_adminAuditLogs_hasQueryIndexes
#[test]
fn 감사_로그_테이블에_조회_인덱스가_있다() {
    let Some(db) = TestDb::create() else { return };
    let names: Vec<String> = db
        .client()
        .query(
            "SELECT indexname FROM pg_indexes WHERE schemaname = current_schema() AND tablename = 'admin_audit_logs'",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    for expected in [
        "ix_admin_audit_logs_occurred_at_id",
        "ix_admin_audit_logs_actor_occurred_at_id",
        "ix_admin_audit_logs_action_occurred_at_id",
        "ix_admin_audit_logs_target_occurred_at_id",
        "ix_admin_audit_logs_request_id",
    ] {
        assert!(names.iter().any(|n| n == expected), "{expected}");
    }
}

#[derive(Clone, Default)]
struct SharedBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn logging_deps(db: &TestDb, buffer: &SharedBuffer) -> AdminDeps {
    let mut deps = deps(db);
    deps.logger = std::sync::Arc::new(chalkak_core::log::Logger::new(
        Box::new(buffer.clone()),
        chalkak_core::log::Level::Info,
        deps.clock.clone(),
    ));
    deps
}

// AdminPostModerationLogTest: moderate_approve_logsApprovedWithWaitSeconds, moderate_reject_logsRejected
#[test]
fn 검수하면_결정_로그를_한_건씩_남긴다() {
    let Some(db) = TestDb::create() else { return };
    let admin = insert_admin(&mut db.client(), "operator");
    let topic = insert_topic(
        &mut db.client(),
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let mut posts = Vec::new();
    for n in 0..2 {
        let author = insert_user(&mut db.client(), &format!("a{n}@example.com"), "ACTIVE");
        posts.push(insert_post(&mut db.client(), author, topic, "PENDING"));
    }
    db.client()
        .execute(
            "UPDATE posts SET created_at = '2026-10-12T00:59:30Z' WHERE id = $1",
            &[&posts[0]],
        )
        .unwrap();
    let buffer = SharedBuffer::default();
    let service = AdminPostCommandService::new(logging_deps(&db, &buffer));
    service
        .moderate(posts[0], admin, ModerationStatus::Approved, None)
        .unwrap();
    let approved = buffer.text();
    assert_eq!(approved.lines().count(), 1);
    assert!(approved.contains("\"event\":\"approved\""), "{approved}");
    assert!(approved.contains("\"waitSeconds\":30"), "{approved}");
    assert!(approved.contains("검수 결정"));
    service
        .moderate(posts[1], admin, ModerationStatus::Rejected, Some("부적절"))
        .unwrap();
    let all = buffer.text();
    assert_eq!(all.lines().count(), 2);
    assert!(all.contains("\"event\":\"rejected\""), "{all}");
}

// AdminPostCommandModerationTest: moderate_duplicateEventKey_rejectsDuplicateNotification
#[test]
fn 같은_검수_사건의_알림은_중복_저장할_수_없다() {
    let Some(db) = TestDb::create() else { return };
    let author = insert_user(&mut db.client(), "a@example.com", "ACTIVE");
    let topic = insert_topic(
        &mut db.client(),
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let post = insert_post(&mut db.client(), author, topic, "PENDING");
    let admin = insert_admin(&mut db.client(), "operator");
    save_log(&deps(&db), admin, author, "2026-10-12T01:00:00Z");
    let event_key: Uuid = db
        .client()
        .query_one("SELECT id FROM admin_audit_logs LIMIT 1", &[])
        .unwrap()
        .get(0);
    let mut client = db.client();
    let mut save = || {
        super::notification_adapter::create_for_moderation(
            &mut client,
            author,
            post,
            event_key,
            super::notification_adapter::ModerationDecision::Approved,
            None,
            ts("2026-10-12T01:00:00Z"),
        )
    };
    assert!(save().is_ok());
    assert!(save().is_err());
}
