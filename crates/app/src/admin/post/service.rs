//! 원본 `AdminPostQueryService`·`AdminPostCommandService`.

use jiff::Timestamp;
use jiff::civil::Date;
use uuid::Uuid;

use super::repository::{
    AdminPostRepository, AdminPostSort, ModerationStatus, PostDetailRow, PostQueryCriteria,
    PostSummaryRow,
};
use crate::admin::audit::domain::{
    AdminAction, AdminTargetType, AuditSnapshot, instant_value, text_value, uuid_value,
};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{code_points, has_text, java_trim};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, business, in_transaction, not_found,
    state_changed, validate_pagination,
};
use crate::notification::{ModerationDecision, create_for_moderation};
use crate::web::ApiError;
use crate::web::binding::is_java_whitespace;
use chalkak_core::log::fields;

const MAX_REASON_LENGTH: usize = 500;
const INVALID_MODERATION_REQUEST_MESSAGE: &str = "게시물 검수 요청이 올바르지 않습니다.";
const INVALID_DELETION_REQUEST_MESSAGE: &str = "게시물 삭제 요청이 올바르지 않습니다.";
const NOT_FOUND_MESSAGE: &str = "게시물을 찾을 수 없습니다.";

#[derive(Clone, Debug)]
pub struct PostListItem {
    pub row: PostSummaryRow,
    pub original_image_url: Option<String>,
    pub thumbnail_image_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PostDetail {
    pub row: PostDetailRow,
    pub original_image_url: Option<String>,
    pub thumbnail_image_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PostModerationResult {
    pub post_id: Uuid,
    pub moderation_status: ModerationStatus,
    pub moderated_by: Uuid,
    pub moderated_at: Timestamp,
    pub rejection_reason: Option<String>,
}

#[derive(Clone)]
pub struct AdminPostQueryService {
    deps: AdminDeps,
}

impl AdminPostQueryService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminPostQueryService { deps }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn get_posts(
        &self,
        status: Option<ModerationStatus>,
        topic_id: Option<Uuid>,
        topic_date: Option<Date>,
        user_id: Option<Uuid>,
        created_at_from: Option<Timestamp>,
        created_at_to: Option<Timestamp>,
        sort: Option<AdminPostSort>,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<PostListItem>, ApiError> {
        if status == Some(ModerationStatus::Validating) {
            return Err(business("게시물 조회 상태가 올바르지 않습니다."));
        }
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        if let (Some(from), Some(to)) = (created_at_from, created_at_to)
            && from > to
        {
            return Err(business("등록 시각 범위가 올바르지 않습니다."));
        }
        let criteria = PostQueryCriteria {
            status,
            topic_id,
            topic_date,
            user_id,
            created_at_from,
            created_at_to,
            sort: sort.unwrap_or(AdminPostSort::CreatedAtDesc),
        };
        let mut conn = self.deps.pool.get()?;
        let result = AdminPostRepository.find_posts(&mut conn, &criteria, page, page_size)?;
        let urls = &self.deps.image_urls;
        let mut items = Vec::with_capacity(result.items.len());
        for row in result.items {
            items.push(PostListItem {
                original_image_url: urls.get_url(Some(&row.original_storage_key))?,
                thumbnail_image_url: urls.get_url(row.thumbnail_storage_key.as_deref())?,
                row,
            });
        }
        Ok(AdminPage {
            items,
            current_page: result.current_page,
            page_size: result.page_size,
            has_next: result.has_next,
        })
    }

    pub fn get_post(&self, post_id: Uuid) -> Result<PostDetail, ApiError> {
        let mut conn = self.deps.pool.get()?;
        let row = AdminPostRepository
            .find_post_by_id(&mut conn, post_id)?
            .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
        let urls = &self.deps.image_urls;
        Ok(PostDetail {
            original_image_url: urls.get_url(Some(&row.original_storage_key))?,
            thumbnail_image_url: urls.get_url(row.thumbnail_storage_key.as_deref())?,
            row,
        })
    }
}

#[derive(Clone)]
pub struct AdminPostCommandService {
    deps: AdminDeps,
}

impl AdminPostCommandService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminPostCommandService { deps }
    }

    /// `moderate`
    pub fn moderate(
        &self,
        post_id: Uuid,
        admin_id: Uuid,
        status: ModerationStatus,
        rejection_reason: Option<&str>,
    ) -> Result<PostModerationResult, ApiError> {
        let reason = validate_and_normalize_moderation(status, rejection_reason)?;
        let repo = AdminPostRepository;
        let mut conn = self.deps.pool.get()?;
        let moderated_at = self.deps.now();
        in_transaction(&mut conn, |tx| {
            let post = repo
                .find_for_update(tx, post_id, false)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            if post.moderation_status != ModerationStatus::Pending {
                return Err(state_changed("대기 중인 게시물만 검수할 수 있습니다."));
            }
            let before = AuditSnapshot::of(vec![
                (
                    "moderationStatus",
                    text_value(post.moderation_status.as_str()),
                ),
                ("moderatedAt", moderated_at_value(post.moderated_at)),
            ])?;
            repo.save_moderation(tx, post.id, status, moderated_at, moderated_at)?;
            self.log_moderated(post.id, status, post.created_at, moderated_at);
            let after = AuditSnapshot::of(vec![
                ("moderationStatus", text_value(status.as_str())),
                ("moderatedAt", instant_value(moderated_at)),
                ("moderatedBy", uuid_value(admin_id)),
            ])?;
            let log = create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: match status {
                        ModerationStatus::Approved => AdminAction::PostApproved,
                        _ => AdminAction::PostRejected,
                    },
                    target_type: AdminTargetType::Post,
                    target_id: post.id,
                    reason: reason.clone(),
                    before_state: before,
                    after_state: after,
                },
                moderated_at,
            )?;
            let event_key = log
                .id
                .ok_or_else(|| ApiError::Internal("감사 로그 식별자가 비어 있다".to_owned()))?;
            create_for_moderation(
                tx,
                post.author_id,
                post.id,
                event_key,
                match status {
                    ModerationStatus::Approved => ModerationDecision::Approved,
                    _ => ModerationDecision::Rejected,
                },
                reason.as_deref(),
                moderated_at,
            )?;
            Ok(PostModerationResult {
                post_id: post.id,
                moderation_status: status,
                moderated_by: admin_id,
                moderated_at,
                rejection_reason: reason.clone(),
            })
        })
    }

    fn log_moderated(
        &self,
        post_id: Uuid,
        status: ModerationStatus,
        created_at: Timestamp,
        moderated_at: Timestamp,
    ) {
        let wait_seconds = moderated_at.duration_since(created_at).as_secs();
        self.deps
            .logger
            .info("chalkak.moderation")
            .field(fields::TYPE, fields::TYPE_MODERATION)
            .field(fields::EVENT, status.as_str().to_lowercase())
            .field(fields::POST_ID, post_id.hyphenated().to_string())
            .field(fields::WAIT_SECONDS, wait_seconds)
            .log("검수 결정");
    }

    /// `deletePost`
    pub fn delete_post(&self, post_id: Uuid, admin_id: Uuid, reason: &str) -> Result<(), ApiError> {
        let reason = validate_and_normalize_deletion(reason)?;
        let repo = AdminPostRepository;
        let mut conn = self.deps.pool.get()?;
        let requested_at = self.deps.now();
        in_transaction(&mut conn, |tx| {
            let post = repo
                .find_for_update(tx, post_id, true)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            if let Some(deleted_at) = post.deleted_at {
                // 이미 삭제된 게시물: 사진 삭제 표시만 맞추고 성공으로 끝낸다.
                repo.mark_photo_deleted(tx, post.photo_id, deleted_at, requested_at)?;
                return Ok(());
            }
            if post.moderation_status == ModerationStatus::Validating {
                return Err(business("이미지 처리 중인 게시물은 삭제할 수 없습니다."));
            }
            let before = deletion_state(post.moderation_status, None)?;
            repo.save_deletion(tx, post.id, post.photo_id, requested_at, requested_at)?;
            repo.delete_likes(tx, post.id)?;
            let after = deletion_state(post.moderation_status, Some(requested_at))?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::PostDeleted,
                    target_type: AdminTargetType::Post,
                    target_id: post.id,
                    reason: Some(reason.clone()),
                    before_state: before,
                    after_state: after,
                },
                requested_at,
            )?;
            Ok(())
        })
    }
}

fn moderated_at_value(at: Option<Timestamp>) -> serde_json::Value {
    at.map_or(serde_json::Value::Null, instant_value)
}

fn deletion_state(
    status: ModerationStatus,
    deleted_at: Option<Timestamp>,
) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(vec![
        ("moderationStatus", text_value(status.as_str())),
        ("deletedAt", moderated_at_value(deleted_at)),
    ])?)
}

/// `validateAndNormalizeModeration`: 승인에는 사유가 없어야 하고, 거절에는 500자 이하의 사유가 있어야 한다.
pub fn validate_and_normalize_moderation(
    status: ModerationStatus,
    rejection_reason: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let invalid = || business(INVALID_MODERATION_REQUEST_MESSAGE);
    match status {
        ModerationStatus::Approved => {
            if rejection_reason.is_some() {
                return Err(invalid());
            }
            Ok(None)
        }
        ModerationStatus::Rejected => {
            let Some(reason) = rejection_reason.filter(|r| has_text(r)) else {
                return Err(invalid());
            };
            let normalized = java_trim(reason);
            if code_points(normalized) > MAX_REASON_LENGTH {
                return Err(invalid());
            }
            Ok(Some(normalized.to_owned()))
        }
        _ => Err(invalid()),
    }
}

/// `validateAndNormalizeDeletion`: 비어 있지 않고 500자 이하, `String.strip()`으로 정리한다.
pub fn validate_and_normalize_deletion(reason: &str) -> Result<String, ApiError> {
    if !has_text(reason) || code_points(reason) > MAX_REASON_LENGTH {
        return Err(business(INVALID_DELETION_REQUEST_MESSAGE));
    }
    Ok(reason.trim_matches(is_java_whitespace).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testing::{deps, insert_admin, insert_post, insert_topic, insert_user};
    use crate::testing::TestDb;

    fn message<T: std::fmt::Debug>(r: Result<T, ApiError>) -> String {
        r.unwrap_err().message().to_owned()
    }

    // AdminPostCommandServiceTest: 요청 정규화
    #[test]
    fn 승인에는_사유가_없어야_하고_거절에는_사유가_있어야_한다() {
        assert_eq!(
            validate_and_normalize_moderation(ModerationStatus::Approved, None).unwrap(),
            None
        );
        for (status, reason) in [
            (ModerationStatus::Approved, Some("x")),
            (ModerationStatus::Rejected, None),
            (ModerationStatus::Rejected, Some("  ")),
            (ModerationStatus::Pending, None),
            (ModerationStatus::Validating, None),
        ] {
            assert_eq!(
                message(validate_and_normalize_moderation(status, reason)),
                INVALID_MODERATION_REQUEST_MESSAGE
            );
        }
        assert_eq!(
            validate_and_normalize_moderation(ModerationStatus::Rejected, Some("  정책 위반 "))
                .unwrap()
                .as_deref(),
            Some("정책 위반")
        );
        let long = "가".repeat(501);
        assert!(
            validate_and_normalize_moderation(ModerationStatus::Rejected, Some(&long)).is_err()
        );
    }

    #[test]
    fn 삭제_사유는_비어_있지_않고_500자_이하다() {
        assert_eq!(
            validate_and_normalize_deletion(" 사유\u{1f}").unwrap(),
            "사유"
        );
        assert!(validate_and_normalize_deletion("   ").is_err());
        assert!(validate_and_normalize_deletion(&"가".repeat(501)).is_err());
        assert!(validate_and_normalize_deletion(&"가".repeat(500)).is_ok());
    }

    struct Fixture {
        db: TestDb,
        admin: Uuid,
        author: Uuid,
        topic: Uuid,
    }

    fn fixture() -> Option<Fixture> {
        let db = TestDb::create()?;
        let mut client = db.client();
        let admin = insert_admin(&mut client, "operator");
        let author = insert_user(&mut client, "author@test.chalkak", "ACTIVE");
        let topic = insert_topic(
            &mut client,
            "주제",
            "2026-10-12",
            "2026-10-11T15:00:00Z",
            "2026-10-12T15:00:00Z",
        );
        drop(client);
        Some(Fixture {
            db,
            admin,
            author,
            topic,
        })
    }

    // 승인: 상태, 감사 로그, 알림이 한 트랜잭션에서 남는다.
    #[test]
    fn 승인하면_상태와_감사_로그와_알림이_함께_남는다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let post = insert_post(&mut client, f.author, f.topic, "PENDING");
        let service = AdminPostCommandService::new(deps(&f.db));
        let result = service
            .moderate(post, f.admin, ModerationStatus::Approved, None)
            .unwrap();
        assert_eq!(result.moderation_status, ModerationStatus::Approved);
        assert_eq!(result.rejection_reason, None);

        let row = client
            .query_one(
                "SELECT moderation_status::text, moderated_at IS NOT NULL FROM posts WHERE id = $1",
                &[&post],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "APPROVED");
        assert!(row.get::<_, bool>(1));

        let log = client
            .query_one(
                "SELECT id, action::text, before_state::text, after_state::text FROM admin_audit_logs WHERE target_id = $1",
                &[&post],
            )
            .unwrap();
        assert_eq!(log.get::<_, String>(1), "POST_APPROVED");
        assert_eq!(
            log.get::<_, String>(2),
            r#"{"moderatedAt": null, "moderationStatus": "PENDING"}"#
        );
        let after: serde_json::Value = serde_json::from_str(&log.get::<_, String>(3)).unwrap();
        assert_eq!(after["moderatedBy"], f.admin.to_string());

        let n = client
            .query_one(
                "SELECT user_id, event_key, type, title, rejection_reason FROM notifications WHERE post_id = $1",
                &[&post],
            )
            .unwrap();
        assert_eq!(n.get::<_, Uuid>(0), f.author);
        assert_eq!(n.get::<_, Uuid>(1), log.get::<_, Uuid>(0));
        assert_eq!(n.get::<_, String>(2), "POST_APPROVED");
        assert_eq!(n.get::<_, String>(3), "게시물이 승인되었습니다.");
        assert_eq!(n.get::<_, Option<String>>(4), None);
    }

    #[test]
    fn 거절하면_사유가_감사_로그와_알림에_남는다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let post = insert_post(&mut client, f.author, f.topic, "PENDING");
        let service = AdminPostCommandService::new(deps(&f.db));
        let result = service
            .moderate(
                post,
                f.admin,
                ModerationStatus::Rejected,
                Some(" 정책 위반 "),
            )
            .unwrap();
        assert_eq!(result.rejection_reason.as_deref(), Some("정책 위반"));
        let n = client
            .query_one(
                "SELECT type, title, body, rejection_reason FROM notifications WHERE post_id = $1",
                &[&post],
            )
            .unwrap();
        assert_eq!(n.get::<_, String>(0), "POST_REJECTED");
        assert_eq!(n.get::<_, String>(2), "반려 사유를 확인해 주세요.");
        assert_eq!(n.get::<_, String>(3), "정책 위반");
        let reason: String = client
            .query_one(
                "SELECT reason FROM admin_audit_logs WHERE target_id = $1",
                &[&post],
            )
            .unwrap()
            .get(0);
        assert_eq!(reason, "정책 위반");
    }

    #[test]
    fn 대기_중이_아니거나_없는_게시물은_검수할_수_없다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let approved = insert_post(&mut client, f.author, f.topic, "APPROVED");
        let service = AdminPostCommandService::new(deps(&f.db));
        let e = service
            .moderate(approved, f.admin, ModerationStatus::Approved, None)
            .unwrap_err();
        assert_eq!(e.message(), "대기 중인 게시물만 검수할 수 있습니다.");
        assert_eq!(e.code().as_str(), "RESOURCE_STATE_CHANGED");

        let other = insert_user(&mut client, "o@test.chalkak", "ACTIVE");
        let deleted = insert_post(&mut client, other, f.topic, "PENDING");
        client
            .execute(
                "UPDATE posts SET deleted_at = now() WHERE id = $1",
                &[&deleted],
            )
            .unwrap();
        let e = service
            .moderate(deleted, f.admin, ModerationStatus::Approved, None)
            .unwrap_err();
        assert_eq!(e.status(), 404);
        let e = service
            .moderate(Uuid::new_v4(), f.admin, ModerationStatus::Approved, None)
            .unwrap_err();
        assert_eq!(e.message(), NOT_FOUND_MESSAGE);
        let logs: i64 = client
            .query_one("SELECT COUNT(*) FROM admin_audit_logs", &[])
            .unwrap()
            .get(0);
        assert_eq!(logs, 0);
    }

    // 삭제
    #[test]
    fn 삭제하면_사진과_좋아요를_정리하고_감사_로그를_남긴다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let post = insert_post(&mut client, f.author, f.topic, "APPROVED");
        let liker = insert_user(&mut client, "l@test.chalkak", "ACTIVE");
        client
            .execute(
                "INSERT INTO post_likes (post_id, user_id) VALUES ($1, $2)",
                &[&post, &liker],
            )
            .unwrap();
        let service = AdminPostCommandService::new(deps(&f.db));
        service.delete_post(post, f.admin, " 정책 위반 ").unwrap();

        let row = client
            .query_one(
                "SELECT p.deleted_at IS NOT NULL, ph.deleted_at = p.deleted_at, \
                 (SELECT COUNT(*) FROM post_likes WHERE post_id = p.id) \
                 FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = $1",
                &[&post],
            )
            .unwrap();
        assert!(row.get::<_, bool>(0));
        assert!(row.get::<_, bool>(1));
        assert_eq!(row.get::<_, i64>(2), 0);
        let log = client
            .query_one(
                "SELECT action::text, reason, before_state::text FROM admin_audit_logs WHERE target_id = $1",
                &[&post],
            )
            .unwrap();
        assert_eq!(log.get::<_, String>(0), "POST_DELETED");
        assert_eq!(log.get::<_, String>(1), "정책 위반");
        assert_eq!(
            log.get::<_, String>(2),
            r#"{"deletedAt": null, "moderationStatus": "APPROVED"}"#
        );

        // 다시 삭제해도 성공이며 감사 로그는 늘지 않는다.
        service.delete_post(post, f.admin, "again").unwrap();
        let logs: i64 = client
            .query_one("SELECT COUNT(*) FROM admin_audit_logs", &[])
            .unwrap()
            .get(0);
        assert_eq!(logs, 1);
    }

    #[test]
    fn 이미지_처리_중인_게시물은_삭제할_수_없다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let post = insert_post(&mut client, f.author, f.topic, "VALIDATING");
        let service = AdminPostCommandService::new(deps(&f.db));
        let e = service.delete_post(post, f.admin, "x").unwrap_err();
        assert_eq!(e.message(), "이미지 처리 중인 게시물은 삭제할 수 없습니다.");
        let e = service
            .delete_post(Uuid::new_v4(), f.admin, "x")
            .unwrap_err();
        assert_eq!(e.status(), 404);
    }

    // AdminPostQueryRepositoryTest
    #[test]
    fn 목록은_검증_중인_게시물을_빼고_조건으로_거른다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let pending = insert_post(&mut client, f.author, f.topic, "PENDING");
        let other = insert_user(&mut client, "o@test.chalkak", "ACTIVE");
        let approved = insert_post(&mut client, other, f.topic, "APPROVED");
        let third = insert_user(&mut client, "t@test.chalkak", "ACTIVE");
        let _validating = insert_post(&mut client, third, f.topic, "VALIDATING");
        let service = AdminPostQueryService::new(deps(&f.db));
        let list = |status, user| {
            service
                .get_posts(status, None, None, user, None, None, None, 1, 20)
                .unwrap()
                .items
                .into_iter()
                .map(|i| i.row.post_id)
                .collect::<Vec<_>>()
        };
        let all = list(None, None);
        assert_eq!(all.len(), 2);
        assert!(all.contains(&pending) && all.contains(&approved));
        assert_eq!(list(Some(ModerationStatus::Approved), None), vec![approved]);
        assert_eq!(list(None, Some(f.author)), vec![pending]);

        let e = service
            .get_posts(
                Some(ModerationStatus::Validating),
                None,
                None,
                None,
                None,
                None,
                None,
                1,
                20,
            )
            .unwrap_err();
        assert_eq!(e.message(), "게시물 조회 상태가 올바르지 않습니다.");
        let e = service
            .get_posts(
                None,
                None,
                None,
                None,
                Some("2026-10-02T00:00:00Z".parse().unwrap()),
                Some("2026-10-01T00:00:00Z".parse().unwrap()),
                None,
                1,
                20,
            )
            .unwrap_err();
        assert_eq!(e.message(), "등록 시각 범위가 올바르지 않습니다.");

        let detail = service.get_post(pending).unwrap();
        assert_eq!(detail.row.moderation_status, ModerationStatus::Pending);
        assert!(
            detail
                .original_image_url
                .unwrap()
                .starts_with("https://cdn.test.chalkak/")
        );
        assert_eq!(detail.row.like_count, 0);
        let e = service.get_post(_validating).unwrap_err();
        assert_eq!(e.message(), NOT_FOUND_MESSAGE);
    }

    #[test]
    fn 상세는_검수한_관리자와_거절_사유를_감사_로그에서_읽는다() {
        let Some(f) = fixture() else { return };
        let mut client = f.db.client();
        let post = insert_post(&mut client, f.author, f.topic, "PENDING");
        let service_deps = deps(&f.db);
        AdminPostCommandService::new(service_deps.clone())
            .moderate(post, f.admin, ModerationStatus::Rejected, Some("정책 위반"))
            .unwrap();
        let detail = AdminPostQueryService::new(service_deps)
            .get_post(post)
            .unwrap();
        assert_eq!(detail.row.moderated_by, Some(f.admin));
        assert_eq!(detail.row.rejection_reason.as_deref(), Some("정책 위반"));
        assert!(detail.row.moderated_at.is_some());
    }
}
