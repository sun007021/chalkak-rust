//! 알림 저장소·서비스·정리·검수 알림 생성의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `NotificationInboxServiceTest`, `NotificationCleanupSchedulerTest`(본문), `AdminPostCommandModerationTest`·
//! `AdminPostNotificationTransactionTest`의 알림 부분을 옮겼다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_app::notification::cleanup::delete_expired_notifications;
use chalkak_app::notification::service::NotificationInboxService;
use chalkak_app::notification::{ModerationDecision, NotificationError, create_for_moderation};
use chalkak_app::photo::url::{ImageUrlProvider, InvalidStorageKey};
use chalkak_app::testing::TestDb;
use chalkak_core::FixedClock;
use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::Client;
use uuid::Uuid;

const ADMIN_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572f1";
const USER_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572a1";
const OTHER_USER_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572a2";
const TOPIC_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572b1";
const PHOTO_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572c1";
const POST_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572d1";
const EVENT_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572e1";
const SECOND_EVENT_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572e2";
const NOTIFICATION_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572f2";
const SECOND_NOTIFICATION_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6572f3";
const CREATED_AT: &str = "2026-09-29T09:00:00Z";
const ORIGINAL_KEY: &str = "chalkak/posts/inbox/original.webp";
const THUMBNAIL_KEY: &str = "chalkak/posts/inbox/thumbnail.webp";

fn id(s: &str) -> Uuid {
    s.parse().unwrap()
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// 원본 테스트의 `@MockitoBean ImageUrlProvider`: 키에 접두사를 붙인다.
struct FakeImageUrls;

impl ImageUrlProvider for FakeImageUrls {
    fn get_url(&self, storage_key: Option<&str>) -> Result<Option<String>, InvalidStorageKey> {
        Ok(storage_key.map(|k| format!("https://cdn.test/{k}")))
    }
}

struct Fixture {
    db: TestDb,
    clock: Arc<FixedClock>,
    service: NotificationInboxService,
}

/// `NotificationInboxServiceTest.insertFixtures`
fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let mut c = db.client();
    c.batch_execute(&format!(
        "INSERT INTO admins (id, username, password) VALUES ('{ADMIN_ID}', 'inbox-test-admin', 'test-password')"
    ))
    .unwrap();
    insert_user(&mut c, USER_ID, "inbox-test@example.com");
    insert_user(&mut c, OTHER_USER_ID, "inbox-other@example.com");
    c.batch_execute(&format!(
        "INSERT INTO topics (id, title, topic_date, starts_at, ends_at) VALUES \
         ('{TOPIC_ID}', '알림함 조회', '2026-09-29', '2026-09-29T00:00:00Z', '2026-09-30T00:00:00Z');\
         INSERT INTO photos (id, original_storage_key, thumbnail_storage_key) VALUES \
         ('{PHOTO_ID}', '{ORIGINAL_KEY}', '{THUMBNAIL_KEY}');\
         INSERT INTO posts (id, user_id, topic_id, photo_id, title, moderation_status) VALUES \
         ('{POST_ID}', '{USER_ID}', '{TOPIC_ID}', '{PHOTO_ID}', '검수 대상', 'REJECTED')"
    ))
    .unwrap();
    insert_event(&mut c, EVENT_ID, CREATED_AT);
    insert_notification(&mut c, NOTIFICATION_ID, USER_ID, EVENT_ID, CREATED_AT);
    drop(c);
    let clock = Arc::new(FixedClock::new(ts("2026-09-29T10:00:00Z")));
    let service = NotificationInboxService::new(db.pool(2), clock.clone(), Arc::new(FakeImageUrls));
    Some(Fixture { db, clock, service })
}

fn insert_user(c: &mut Client, user_id: &str, email: &str) {
    c.batch_execute(&format!(
        "INSERT INTO users (id, email, signature_original_storage_key) VALUES \
         ('{user_id}', '{email}', 'chalkak/signatures/inbox/{user_id}/original.webp')"
    ))
    .unwrap();
}

fn insert_event(c: &mut Client, event_id: &str, occurred_at: &str) {
    c.batch_execute(&format!(
        "INSERT INTO admin_audit_logs (id, actor_admin_id, action, target_type, target_id, reason, \
         before_state, after_state, occurred_at, request_id) VALUES \
         ('{event_id}', '{ADMIN_ID}', 'POST_REJECTED', 'POST', '{POST_ID}', '사진 품질', \
         '{{\"status\":\"PENDING\"}}', '{{\"status\":\"REJECTED\"}}', '{occurred_at}', gen_random_uuid())"
    ))
    .unwrap();
}

fn insert_notification(
    c: &mut Client,
    notification_id: &str,
    user_id: &str,
    event_id: &str,
    created_at: &str,
) {
    c.batch_execute(&format!(
        "INSERT INTO notifications (id, user_id, post_id, event_key, type, title, body, rejection_reason, created_at) \
         VALUES ('{notification_id}', '{user_id}', '{POST_ID}', '{event_id}', 'POST_REJECTED', \
         '게시물이 반려되었습니다.', '반려 사유를 확인해 주세요.', '사진 품질', '{created_at}')"
    ))
    .unwrap();
}

/// `insertSecondNotification`
fn insert_second_notification(db: &TestDb, recipient: &str) {
    let mut c = db.client();
    insert_event(&mut c, SECOND_EVENT_ID, "2026-09-29T09:00:01Z");
    insert_notification(
        &mut c,
        SECOND_NOTIFICATION_ID,
        recipient,
        SECOND_EVENT_ID,
        "2026-09-29T09:00:01Z",
    );
}

fn read_at(db: &TestDb, notification_id: &str) -> Option<Timestamp> {
    db.client()
        .query_one(
            "SELECT read_at FROM notifications WHERE id = $1",
            &[&id(notification_id)],
        )
        .unwrap()
        .get(0)
}

fn count(db: &TestDb, sql: &str) -> i64 {
    db.client().query_one(sql, &[]).unwrap().get(0)
}

// NotificationInboxServiceTest#getNotifications_returnsOwnNotificationsWithThumbnail
#[test]
fn get_notifications_returns_own_notifications_with_thumbnail() {
    let Some(f) = fixture() else { return };
    insert_second_notification(&f.db, OTHER_USER_ID);

    let result = f.service.get_notifications(id(USER_ID), 1, 20).unwrap();

    assert_eq!(result.notifications.len(), 1);
    let first = &result.notifications[0];
    assert_eq!(first.id, id(NOTIFICATION_ID));
    assert_eq!(
        first.thumbnail_image_url.as_deref(),
        Some("https://cdn.test/chalkak/posts/inbox/thumbnail.webp")
    );
    assert_eq!(first.read_at, None);
    assert_eq!(first.created_at, ts(CREATED_AT));
    assert!(!result.has_next);
    assert_eq!((result.current_page, result.page_size), (1, 20));
}

// NotificationInboxServiceTest#getNotifications_ordersNewestFirstAndReturnsHasNext
#[test]
fn get_notifications_orders_newest_first_and_reports_has_next() {
    let Some(f) = fixture() else { return };
    insert_second_notification(&f.db, USER_ID);

    let first = f.service.get_notifications(id(USER_ID), 1, 1).unwrap();
    let second = f.service.get_notifications(id(USER_ID), 2, 1).unwrap();

    let ids = |r: &chalkak_app::notification::service::NotificationListResult| {
        r.notifications.iter().map(|n| n.id).collect::<Vec<_>>()
    };
    assert_eq!(ids(&first), [id(SECOND_NOTIFICATION_ID)]);
    assert!(first.has_next);
    assert_eq!(ids(&second), [id(NOTIFICATION_ID)]);
    assert!(!second.has_next);
}

#[test]
fn get_notifications_page_beyond_the_end_is_empty_and_huge_offset_is_a_server_error() {
    let Some(f) = fixture() else { return };
    let past = f.service.get_notifications(id(USER_ID), 5, 20).unwrap();
    assert!(past.notifications.is_empty());
    assert!(!past.has_next);
    // Spring Data JPA: 오프셋이 int 범위를 넘으면 InvalidDataAccessApiUsageException -> 500
    let err = f
        .service
        .get_notifications(id(USER_ID), i32::MAX, 100)
        .unwrap_err();
    assert_eq!(err.status(), 500);
}

// NotificationInboxServiceTest#getNotification_rejectedNotification_returnsOriginalAndReasonForOwnerOnly
#[test]
fn get_notification_returns_original_and_reason_for_owner_only() {
    let Some(f) = fixture() else { return };

    let detail = f
        .service
        .get_notification(id(USER_ID), id(NOTIFICATION_ID))
        .unwrap();

    assert_eq!(
        detail.original_image_url.as_deref(),
        Some("https://cdn.test/chalkak/posts/inbox/original.webp")
    );
    assert_eq!(detail.rejection_reason.as_deref(), Some("사진 품질"));
    assert_eq!(detail.title, "게시물이 반려되었습니다.");
    assert_eq!(
        read_at(&f.db, NOTIFICATION_ID),
        None,
        "조회로 읽음 처리되지 않는다"
    );
    let err = f
        .service
        .get_notification(id(OTHER_USER_ID), id(NOTIFICATION_ID))
        .unwrap_err();
    assert_eq!(err.status(), 404);
    assert_eq!(err.message(), "알림을 찾을 수 없습니다.");
}

// NotificationInboxServiceTest#hasUnreadNotification_returnsOwnUnreadStatus
#[test]
fn has_unread_notification_is_per_user() {
    let Some(f) = fixture() else { return };
    assert!(f.service.has_unread_notification(id(USER_ID)).unwrap());
    assert!(
        !f.service
            .has_unread_notification(id(OTHER_USER_ID))
            .unwrap()
    );
}

// NotificationInboxServiceTest#getNotifications_authorDeletedApprovedPost_hidesNotificationButKeepsRecord
#[test]
fn author_deleted_approved_post_hides_notification_but_keeps_record() {
    let Some(f) = fixture() else { return };
    f.db.client()
        .batch_execute(&format!(
            "UPDATE posts SET moderation_status = 'APPROVED', deleted_at = CURRENT_TIMESTAMP WHERE id = '{POST_ID}'; \
             UPDATE notifications SET type = 'POST_APPROVED', title = '게시물이 승인되었습니다.', \
             body = '내 사진이 피드에 공개되었습니다.', rejection_reason = NULL WHERE id = '{NOTIFICATION_ID}'"
        ))
        .unwrap();

    assert!(
        f.service
            .get_notifications(id(USER_ID), 1, 20)
            .unwrap()
            .notifications
            .is_empty()
    );
    assert_eq!(
        f.service
            .get_notification(id(USER_ID), id(NOTIFICATION_ID))
            .unwrap_err()
            .status(),
        404
    );
    assert!(!f.service.has_unread_notification(id(USER_ID)).unwrap());
    assert_eq!(
        f.service
            .mark_read(id(USER_ID), id(NOTIFICATION_ID))
            .unwrap_err()
            .status(),
        404
    );
    assert_eq!(count(&f.db, "SELECT COUNT(*) FROM notifications"), 1);
}

// NotificationInboxServiceTest#getNotifications_adminDeletedRejectedPost_hidesNotification
#[test]
fn admin_deleted_rejected_post_hides_notification_and_mark_all_read_skips_it() {
    let Some(f) = fixture() else { return };
    f.db.client()
        .batch_execute(&format!(
            "UPDATE posts SET deleted_at = CURRENT_TIMESTAMP WHERE id = '{POST_ID}'"
        ))
        .unwrap();

    assert!(
        f.service
            .get_notifications(id(USER_ID), 1, 20)
            .unwrap()
            .notifications
            .is_empty()
    );
    assert_eq!(
        f.service
            .get_notification(id(USER_ID), id(NOTIFICATION_ID))
            .unwrap_err()
            .status(),
        404
    );
    assert!(!f.service.has_unread_notification(id(USER_ID)).unwrap());
    f.service.mark_all_read(id(USER_ID)).unwrap();
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM notifications WHERE read_at IS NULL"
        ),
        1
    );
}

// NotificationInboxServiceTest#markRead_isOwnerOnlyAndIdempotent
#[test]
fn mark_read_is_owner_only_and_keeps_the_first_read_time() {
    let Some(f) = fixture() else { return };
    assert_eq!(
        f.service
            .mark_read(id(OTHER_USER_ID), id(NOTIFICATION_ID))
            .unwrap_err()
            .status(),
        404
    );

    f.service
        .mark_read(id(USER_ID), id(NOTIFICATION_ID))
        .unwrap();
    let first = read_at(&f.db, NOTIFICATION_ID);
    f.clock.advance(Duration::from_secs(60));
    f.service
        .mark_read(id(USER_ID), id(NOTIFICATION_ID))
        .unwrap();
    let second = read_at(&f.db, NOTIFICATION_ID);

    assert_eq!(first, Some(ts("2026-09-29T10:00:00Z")));
    assert_eq!(second, first);
    assert!(!f.service.has_unread_notification(id(USER_ID)).unwrap());
}

// NotificationInboxServiceTest#markAllRead_marksEveryOwnUnreadNotification
#[test]
fn mark_all_read_marks_every_own_unread_notification() {
    let Some(f) = fixture() else { return };
    insert_second_notification(&f.db, USER_ID);
    // 이미 읽은 알림의 시각은 바뀌지 않는다.
    f.service
        .mark_read(id(USER_ID), id(NOTIFICATION_ID))
        .unwrap();
    f.clock.advance(Duration::from_secs(60));

    f.service.mark_all_read(id(USER_ID)).unwrap();

    assert_eq!(
        count(
            &f.db,
            &format!(
                "SELECT COUNT(*) FROM notifications WHERE user_id = '{USER_ID}' AND read_at IS NULL"
            )
        ),
        0
    );
    assert_eq!(
        read_at(&f.db, NOTIFICATION_ID),
        Some(ts("2026-09-29T10:00:00Z"))
    );
    assert_eq!(
        read_at(&f.db, SECOND_NOTIFICATION_ID),
        Some(ts("2026-09-29T10:01:00Z"))
    );
}

// ---------------------------------------------------------------------------------------------
// NotificationCleanupScheduler.deleteExpiredNotifications
// ---------------------------------------------------------------------------------------------

const NOW: &str = "2026-09-29T00:00:00Z";
const NORMAL_THRESHOLD: &str = "2026-07-31T00:00:00Z"; // NOW - 60일
const WITHDRAWN_THRESHOLD: &str = "2026-08-30T00:00:00Z"; // NOW - 30일

struct Cleanup {
    db: TestDb,
}

fn cleanup_fixture() -> Option<Cleanup> {
    let f = fixture()?;
    // 정리 시험은 기본 알림을 쓰지 않는다. 사용자와 게시물만 필요하다.
    f.db.client()
        .batch_execute("DELETE FROM notifications")
        .unwrap();
    Some(Cleanup { db: f.db })
}

fn shift(base: &str, seconds: i64) -> String {
    ts(base)
        .checked_add(jiff::SignedDuration::from_secs(seconds))
        .unwrap()
        .to_string()
}

/// 알림 하나와 그 근거인 감사 로그를 만든다.
fn insert_cleanup_notification(db: &TestDb, user: &str, created_at: &str) -> Uuid {
    let event_id = Uuid::new_v4();
    let notification_id = Uuid::new_v4();
    let mut c = db.client();
    c.execute(
        "INSERT INTO admin_audit_logs (id, actor_admin_id, action, target_type, target_id, \
         before_state, after_state, occurred_at, request_id) VALUES \
         ($1, $2, 'POST_APPROVED', 'POST', $3, '{\"status\":\"PENDING\"}', '{\"status\":\"APPROVED\"}', $4, gen_random_uuid())",
        &[&event_id, &id(ADMIN_ID), &id(POST_ID), &ts(created_at)],
    )
    .unwrap();
    c.execute(
        "INSERT INTO notifications (id, user_id, post_id, event_key, type, title, body, created_at) \
         VALUES ($1, $2, $3, $4, 'POST_APPROVED', '승인', '피드에 공개', $5)",
        &[&notification_id, &id(user), &id(POST_ID), &event_id, &ts(created_at)],
    )
    .unwrap();
    notification_id
}

fn exists(db: &TestDb, notification_id: Uuid) -> bool {
    db.client()
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM notifications WHERE id = $1)",
            &[&notification_id],
        )
        .unwrap()
        .get(0)
}

fn run_cleanup(db: &TestDb) {
    let mut client = db.client();
    let mut tx = client.transaction().unwrap();
    delete_expired_notifications(&mut tx, ts(NOW)).unwrap();
    tx.commit().unwrap();
}

// NotificationCleanupSchedulerTest#deleteExpiredNotifications_activeUser_deletesOnlyOlderThanSixtyDays
#[test]
fn cleanup_active_user_deletes_only_older_than_sixty_days() {
    let Some(c) = cleanup_fixture() else { return };
    let expired = insert_cleanup_notification(&c.db, USER_ID, &shift(NORMAL_THRESHOLD, -1));
    let on_boundary = insert_cleanup_notification(&c.db, USER_ID, NORMAL_THRESHOLD);
    let recent = insert_cleanup_notification(&c.db, USER_ID, &shift(NORMAL_THRESHOLD, 1));

    run_cleanup(&c.db);

    assert!(!exists(&c.db, expired));
    assert!(exists(&c.db, on_boundary));
    assert!(exists(&c.db, recent));
}

// NotificationCleanupSchedulerTest#deleteExpiredNotifications_oldWithdrawal_deletesRecentNotification
#[test]
fn cleanup_old_withdrawal_deletes_recent_notification() {
    let Some(c) = cleanup_fixture() else { return };
    c.db.client()
        .execute(
            "UPDATE users SET deleted_at = $1 WHERE id = $2",
            &[&ts(&shift(WITHDRAWN_THRESHOLD, -1)), &id(OTHER_USER_ID)],
        )
        .unwrap();
    let notification = insert_cleanup_notification(&c.db, OTHER_USER_ID, &shift(NOW, -86_400));

    run_cleanup(&c.db);

    assert!(!exists(&c.db, notification));
}

// NotificationCleanupSchedulerTest#deleteExpiredNotifications_recentWithdrawal_keepsOlderNotification
#[test]
fn cleanup_recent_withdrawal_keeps_older_notification() {
    let Some(c) = cleanup_fixture() else { return };
    c.db.client()
        .execute(
            "UPDATE users SET deleted_at = $1 WHERE id = $2",
            &[&ts(WITHDRAWN_THRESHOLD), &id(OTHER_USER_ID)],
        )
        .unwrap();
    let notification =
        insert_cleanup_notification(&c.db, OTHER_USER_ID, &shift(NORMAL_THRESHOLD, -1));

    run_cleanup(&c.db);

    assert!(exists(&c.db, notification));
}

// ---------------------------------------------------------------------------------------------
// UserNotificationService.createForModeration
// ---------------------------------------------------------------------------------------------

struct Moderation {
    db: TestDb,
}

fn moderation_fixture() -> Option<Moderation> {
    let f = fixture()?;
    f.db.client()
        .batch_execute("DELETE FROM notifications")
        .unwrap();
    Some(Moderation { db: f.db })
}

fn row_of(db: &TestDb, event_id: &str) -> postgres::Row {
    db.client()
        .query_one(
            "SELECT user_id, post_id, type, title, body, rejection_reason, read_at, created_at \
             FROM notifications WHERE event_key = $1",
            &[&id(event_id)],
        )
        .unwrap()
}

// AdminPostCommandModerationTest#moderate_approvedRequest_createsNotification
#[test]
fn create_for_moderation_approved_saves_the_inbox_item() {
    let Some(m) = moderation_fixture() else {
        return;
    };
    let mut client = m.db.client();
    let mut tx = client.transaction().unwrap();

    create_for_moderation(
        &mut tx,
        id(USER_ID),
        id(POST_ID),
        id(EVENT_ID),
        ModerationDecision::Approved,
        None,
        ts("2026-09-29T09:30:00.123456Z"),
    )
    .unwrap();
    tx.commit().unwrap();

    let row = row_of(&m.db, EVENT_ID);
    assert_eq!(row.get::<_, Uuid>("user_id"), id(USER_ID));
    assert_eq!(row.get::<_, Uuid>("post_id"), id(POST_ID));
    assert_eq!(row.get::<_, String>("type"), "POST_APPROVED");
    assert_eq!(row.get::<_, String>("title"), "게시물이 승인되었습니다.");
    assert_eq!(
        row.get::<_, String>("body"),
        "내 사진이 피드에 공개되었습니다."
    );
    assert_eq!(row.get::<_, Option<String>>("rejection_reason"), None);
    assert_eq!(row.get::<_, Option<Timestamp>>("read_at"), None);
    assert_eq!(
        row.get::<_, Timestamp>("created_at"),
        ts("2026-09-29T09:30:00.123456Z")
    );
}

// 승인 알림에는 사유를 넘겨도 저장하지 않는다(Notification.approved는 사유를 받지 않는다).
#[test]
fn create_for_moderation_approved_ignores_the_reason() {
    let Some(m) = moderation_fixture() else {
        return;
    };
    let mut client = m.db.client();
    let mut tx = client.transaction().unwrap();
    create_for_moderation(
        &mut tx,
        id(USER_ID),
        id(POST_ID),
        id(EVENT_ID),
        ModerationDecision::Approved,
        Some("무시됨"),
        ts(CREATED_AT),
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        row_of(&m.db, EVENT_ID).get::<_, Option<String>>("rejection_reason"),
        None
    );
}

// AdminPostCommandModerationTest#moderate_rejectedRequest_createsNotificationWithReason
#[test]
fn create_for_moderation_rejected_keeps_the_reason() {
    let Some(m) = moderation_fixture() else {
        return;
    };
    let mut client = m.db.client();
    let mut tx = client.transaction().unwrap();
    create_for_moderation(
        &mut tx,
        id(USER_ID),
        id(POST_ID),
        id(EVENT_ID),
        ModerationDecision::Rejected,
        Some("사진 품질"),
        ts(CREATED_AT),
    )
    .unwrap();
    tx.commit().unwrap();

    let row = row_of(&m.db, EVENT_ID);
    assert_eq!(row.get::<_, String>("type"), "POST_REJECTED");
    assert_eq!(row.get::<_, String>("title"), "게시물이 반려되었습니다.");
    assert_eq!(row.get::<_, String>("body"), "반려 사유를 확인해 주세요.");
    assert_eq!(
        row.get::<_, Option<String>>("rejection_reason").as_deref(),
        Some("사진 품질")
    );
}

#[test]
fn create_for_moderation_rejected_without_a_reason_is_a_business_error() {
    let Some(m) = moderation_fixture() else {
        return;
    };
    let mut client = m.db.client();
    for reason in [None, Some(""), Some("  ")] {
        let mut tx = client.transaction().unwrap();
        let err = create_for_moderation(
            &mut tx,
            id(USER_ID),
            id(POST_ID),
            id(EVENT_ID),
            ModerationDecision::Rejected,
            reason,
            ts(CREATED_AT),
        )
        .unwrap_err();
        let NotificationError::Business(e) = err else {
            panic!("업무 오류여야 한다");
        };
        assert_eq!(e.message, "반려 알림에는 사유가 필요합니다.");
        tx.rollback().unwrap();
    }
    assert_eq!(count(&m.db, "SELECT COUNT(*) FROM notifications"), 0);
}

// AdminPostCommandModerationTest#moderate_duplicateEventKey_rejectsDuplicateNotification
#[test]
fn create_for_moderation_duplicate_event_key_is_rejected_by_the_database() {
    let Some(m) = moderation_fixture() else {
        return;
    };
    let save = |db: &TestDb| {
        let mut client = db.client();
        let mut tx = client.transaction().unwrap();
        let result = create_for_moderation(
            &mut tx,
            id(USER_ID),
            id(POST_ID),
            id(EVENT_ID),
            ModerationDecision::Approved,
            None,
            ts(CREATED_AT),
        );
        if result.is_ok() {
            tx.commit().unwrap();
        }
        result
    };
    save(&m.db).unwrap();

    let err = save(&m.db).unwrap_err();

    let NotificationError::Db(DbError::Postgres(e)) = err else {
        panic!("DB 오류여야 한다");
    };
    assert_eq!(
        e.as_db_error().and_then(|d| d.constraint()),
        Some("ux_notifications_user_event")
    );
    assert_eq!(count(&m.db, "SELECT COUNT(*) FROM notifications"), 1);
}

// AdminPostNotificationTransactionTest#moderate_rolledBackTransaction_removesNotificationAndAudit
#[test]
fn create_for_moderation_is_rolled_back_with_the_transaction() {
    let Some(m) = moderation_fixture() else {
        return;
    };
    let mut client = m.db.client();
    let mut tx = client.transaction().unwrap();
    create_for_moderation(
        &mut tx,
        id(USER_ID),
        id(POST_ID),
        id(EVENT_ID),
        ModerationDecision::Rejected,
        Some("사진 품질"),
        ts(CREATED_AT),
    )
    .unwrap();
    tx.rollback().unwrap();

    assert_eq!(count(&m.db, "SELECT COUNT(*) FROM notifications"), 0);
}
