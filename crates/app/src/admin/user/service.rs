//! 원본 `AdminUserQueryService`·`AdminUserStatusService`.

use uuid::Uuid;

use super::repository::{
    AdminUserRepository, AdminUserSort, AdminUserStatus, PostCounts, UserDetailRow,
    UserQueryCriteria, UserStatus, UserSummary,
};
use crate::admin::audit::domain::{AdminAction, AdminTargetType, AuditSnapshot, text_value};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{code_points, has_text, java_trim};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, business, in_transaction, not_found,
    state_changed, validate_pagination,
};
use crate::web::ApiError;

const MAX_EMAIL_FILTER_LENGTH: usize = 320;
const MAX_REASON_LENGTH: usize = 500;
const INVALID_STATUS_REQUEST_MESSAGE: &str = "사용자 상태 변경 요청이 올바르지 않습니다.";

#[derive(Clone, Debug)]
pub struct UserListItem {
    pub summary: UserSummary,
    pub status: AdminUserStatus,
}

#[derive(Clone, Debug)]
pub struct UserSignature {
    pub original_image_url: Option<String>,
    pub thumbnail_image_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AdminUserDetail {
    pub summary: UserSummary,
    pub status: AdminUserStatus,
    pub signature: UserSignature,
    pub post_counts: PostCounts,
}

#[derive(Clone, Debug)]
pub struct UserStatusResult {
    pub user_id: Uuid,
    pub status: UserStatus,
}

#[derive(Clone)]
pub struct AdminUserQueryService {
    deps: AdminDeps,
}

impl AdminUserQueryService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminUserQueryService { deps }
    }

    pub fn get_users(
        &self,
        status: Option<AdminUserStatus>,
        email: Option<&str>,
        sort: Option<AdminUserSort>,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<UserListItem>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let email = normalize_email(email)?;
        let criteria = UserQueryCriteria {
            status,
            email,
            sort: sort.unwrap_or(AdminUserSort::CreatedAtDesc),
        };
        let mut conn = self.deps.pool.get()?;
        let result = AdminUserRepository.find_users(&mut conn, &criteria, page, page_size)?;
        Ok(result.map(|summary| UserListItem {
            status: AdminUserStatus::from_user(summary.user_status, summary.deleted_at),
            summary,
        }))
    }

    pub fn get_user(&self, user_id: Uuid) -> Result<AdminUserDetail, ApiError> {
        let mut conn = self.deps.pool.get()?;
        let row = AdminUserRepository
            .find_user_by_id(&mut conn, user_id)?
            .ok_or_else(|| not_found("사용자를 찾을 수 없습니다."))?;
        detail_of(row, &self.deps)
    }
}

fn detail_of(row: UserDetailRow, deps: &AdminDeps) -> Result<AdminUserDetail, ApiError> {
    let summary = row.summary;
    let status = AdminUserStatus::from_user(summary.user_status, summary.deleted_at);
    let signature = if status == AdminUserStatus::Withdrawn {
        UserSignature {
            original_image_url: None,
            thumbnail_image_url: None,
        }
    } else {
        let urls = &deps.image_urls;
        UserSignature {
            original_image_url: urls.get_url(Some(&row.signature_original_storage_key))?,
            thumbnail_image_url: urls.get_url(row.signature_thumbnail_storage_key.as_deref())?,
        }
    };
    Ok(AdminUserDetail {
        post_counts: summary.post_counts,
        summary,
        status,
        signature,
    })
}

/// `normalizeEmail`: 공백뿐이면 조건 없음, 아니면 앞뒤 공백을 떼고 소문자로 바꿔 길이를 본다.
fn normalize_email(email: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(email) = email.filter(|e| has_text(e)) else {
        return Ok(None);
    };
    let normalized = java_trim(email).to_lowercase();
    if normalized.encode_utf16().count() > MAX_EMAIL_FILTER_LENGTH {
        return Err(business("이메일 검색어가 올바르지 않습니다."));
    }
    Ok(Some(normalized))
}

#[derive(Clone)]
pub struct AdminUserStatusService {
    deps: AdminDeps,
}

impl AdminUserStatusService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminUserStatusService { deps }
    }

    pub fn update_status(
        &self,
        user_id: Uuid,
        admin_id: Uuid,
        status: UserStatus,
        reason: &str,
    ) -> Result<UserStatusResult, ApiError> {
        let reason = validate_and_normalize(reason)?;
        let mut conn = self.deps.pool.get()?;
        let now = self.deps.now();
        in_transaction(&mut conn, |tx| {
            let repo = AdminUserRepository;
            let user = repo
                .find_by_id_for_update(tx, user_id)?
                .ok_or_else(|| not_found("사용자를 찾을 수 없습니다."))?;
            let before = status_snapshot(user.status)?;
            change_status(&user, status)?;
            let after = status_snapshot(status)?;
            repo.update_status(tx, user.id, status, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: match status {
                        UserStatus::Banned => AdminAction::UserBanned,
                        UserStatus::Active => AdminAction::UserUnbanned,
                    },
                    target_type: AdminTargetType::User,
                    target_id: user.id,
                    reason: Some(reason.clone()),
                    before_state: before,
                    after_state: after,
                },
                now,
            )?;
            Ok(UserStatusResult {
                user_id: user.id,
                status,
            })
        })
    }
}

fn validate_and_normalize(reason: &str) -> Result<String, ApiError> {
    if !has_text(reason) {
        return Err(business(INVALID_STATUS_REQUEST_MESSAGE));
    }
    let normalized = java_trim(reason);
    if code_points(normalized) > MAX_REASON_LENGTH {
        return Err(business(INVALID_STATUS_REQUEST_MESSAGE));
    }
    Ok(normalized.to_owned())
}

/// `User.ban()` / `User.unban()`
fn change_status(user: &super::repository::UserRow, to: UserStatus) -> Result<(), ApiError> {
    if user.deleted_at.is_some() {
        return Err(business("탈퇴한 회원의 상태를 변경할 수 없습니다."));
    }
    match (to, user.status) {
        (UserStatus::Banned, UserStatus::Banned) => Err(state_changed("이미 차단된 회원입니다.")),
        (UserStatus::Active, UserStatus::Active) => {
            Err(state_changed("이미 활성 상태인 회원입니다."))
        }
        _ => Ok(()),
    }
}

fn status_snapshot(status: UserStatus) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(vec![(
        "status",
        text_value(status.as_str()),
    )])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::support::AdminDeps;
    use crate::testing::TestDb;
    use jiff::Timestamp;

    #[test]
    fn 이메일_검색어는_공백이면_조건이_없다() {
        assert_eq!(normalize_email(None).unwrap(), None);
        assert_eq!(normalize_email(Some("   ")).unwrap(), None);
        assert_eq!(
            normalize_email(Some("  Alice@Test.Chalkak "))
                .unwrap()
                .as_deref(),
            Some("alice@test.chalkak")
        );
    }

    // AdminUserQueryServiceTest
    #[test]
    fn 이메일_검색어는_320자까지다() {
        let long = "a".repeat(321);
        let e = normalize_email(Some(&long)).unwrap_err();
        assert_eq!(e.message(), "이메일 검색어가 올바르지 않습니다.");
        assert!(normalize_email(Some(&"a".repeat(320))).is_ok());
    }

    // User#ban / #unban (UserTest)
    #[test]
    fn 상태_전이_규칙() {
        use super::super::repository::UserRow;
        let row = |status, deleted: bool| UserRow {
            id: Uuid::nil(),
            status,
            deleted_at: deleted.then_some(Timestamp::UNIX_EPOCH),
        };
        assert!(change_status(&row(UserStatus::Active, false), UserStatus::Banned).is_ok());
        assert!(change_status(&row(UserStatus::Banned, false), UserStatus::Active).is_ok());
        let e = change_status(&row(UserStatus::Banned, false), UserStatus::Banned).unwrap_err();
        assert_eq!(e.message(), "이미 차단된 회원입니다.");
        assert_eq!(e.code().as_str(), "RESOURCE_STATE_CHANGED");
        let e = change_status(&row(UserStatus::Active, false), UserStatus::Active).unwrap_err();
        assert_eq!(e.message(), "이미 활성 상태인 회원입니다.");
        let e = change_status(&row(UserStatus::Active, true), UserStatus::Banned).unwrap_err();
        assert_eq!(e.message(), "탈퇴한 회원의 상태를 변경할 수 없습니다.");
        assert_eq!(e.code().as_str(), "BUSINESS_ERROR");
    }

    // AdminUserStatusServiceTest
    #[test]
    fn 사유는_공백만이면_거절하고_앞뒤_공백을_뗀다() {
        let e = validate_and_normalize("   ").unwrap_err();
        assert_eq!(e.message(), INVALID_STATUS_REQUEST_MESSAGE);
        assert_eq!(validate_and_normalize("  사유 ").unwrap(), "사유");
        assert!(validate_and_normalize(&"가".repeat(500)).is_ok());
        assert!(validate_and_normalize(&"가".repeat(501)).is_err());
    }

    fn deps(db: &TestDb) -> AdminDeps {
        crate::admin::testing::deps(db)
    }

    // AdminUserStatusServiceTest: 같은 트랜잭션의 감사 로그와 갱신 시각
    #[test]
    fn 차단하면_상태와_감사_로그를_함께_남긴다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = crate::admin::testing::insert_admin(&mut client, "operator");
        let user = crate::admin::testing::insert_user(&mut client, "a@test.chalkak", "ACTIVE");
        let service = AdminUserStatusService::new(deps(&db));
        let result = service
            .update_status(user, admin, UserStatus::Banned, " 반복 위반 ")
            .unwrap();
        assert_eq!(result.status, UserStatus::Banned);
        let status: String = client
            .query_one("SELECT status::text FROM users WHERE id = $1", &[&user])
            .unwrap()
            .get(0);
        assert_eq!(status, "BANNED");
        let row = client
            .query_one(
                "SELECT action::text, reason, before_state::text, after_state::text \
                 FROM admin_audit_logs WHERE target_id = $1",
                &[&user],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "USER_BANNED");
        assert_eq!(row.get::<_, String>(1), "반복 위반");
        assert_eq!(row.get::<_, String>(2), r#"{"status": "ACTIVE"}"#);
        assert_eq!(row.get::<_, String>(3), r#"{"status": "BANNED"}"#);

        let e = service
            .update_status(user, admin, UserStatus::Banned, "again")
            .unwrap_err();
        assert_eq!(e.message(), "이미 차단된 회원입니다.");
        let logs: i64 = client
            .query_one("SELECT COUNT(*) FROM admin_audit_logs", &[])
            .unwrap()
            .get(0);
        assert_eq!(logs, 1, "실패한 요청은 감사 로그를 남기지 않는다");

        let e = service
            .update_status(Uuid::new_v4(), admin, UserStatus::Banned, "x")
            .unwrap_err();
        assert_eq!(e.status(), 404);
    }

    // 민감 정보가 든 사유는 상태 변경까지 롤백한다.
    #[test]
    fn 민감한_사유면_상태도_바뀌지_않는다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = crate::admin::testing::insert_admin(&mut client, "operator");
        let user = crate::admin::testing::insert_user(&mut client, "a@test.chalkak", "ACTIVE");
        let service = AdminUserStatusService::new(deps(&db));
        let e = service
            .update_status(user, admin, UserStatus::Banned, "Bearer abc.def")
            .unwrap_err();
        assert_eq!(
            e.message(),
            "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
        );
        let status: String = client
            .query_one("SELECT status::text FROM users WHERE id = $1", &[&user])
            .unwrap()
            .get(0);
        assert_eq!(status, "ACTIVE");
    }

    // AdminUserQueryRepositoryTest
    #[test]
    fn 목록은_상태와_이메일로_거르고_게시물_수를_센다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let active =
            crate::admin::testing::insert_user(&mut client, "Alice@Test.Chalkak", "ACTIVE");
        let banned = crate::admin::testing::insert_user(&mut client, "bob@test.chalkak", "BANNED");
        let withdrawn =
            crate::admin::testing::insert_user(&mut client, "carol@test.chalkak", "ACTIVE");
        client
            .execute(
                "UPDATE users SET deleted_at = now() WHERE id = $1",
                &[&withdrawn],
            )
            .unwrap();
        let service = AdminUserQueryService::new(deps(&db));

        let ids = |status, email: Option<&str>, sort| {
            service
                .get_users(status, email, sort, 1, 20)
                .unwrap()
                .items
                .into_iter()
                .map(|u| u.summary.user_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Some(AdminUserStatus::Active), None, None), vec![active]);
        assert_eq!(ids(Some(AdminUserStatus::Banned), None, None), vec![banned]);
        assert_eq!(
            ids(Some(AdminUserStatus::Withdrawn), None, None),
            vec![withdrawn]
        );
        assert_eq!(ids(None, None, None).len(), 3);
        assert_eq!(ids(None, Some(" ALICE@"), None), vec![active]);
        let asc = ids(None, None, Some(AdminUserSort::CreatedAtAsc));
        let mut desc = ids(None, None, Some(AdminUserSort::CreatedAtDesc));
        desc.reverse();
        assert_eq!(asc, desc);

        let page = service.get_users(None, None, None, 1, 2).unwrap();
        assert!(page.has_next);
        assert_eq!(page.items.len(), 2);

        let e = service.get_users(None, None, None, 0, 20).unwrap_err();
        assert_eq!(e.message(), INVALID_QUERY_MESSAGE);
    }

    #[test]
    fn 상세는_탈퇴한_회원의_서명을_숨긴다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let active = crate::admin::testing::insert_user(&mut client, "a@test.chalkak", "ACTIVE");
        let withdrawn = crate::admin::testing::insert_user(&mut client, "b@test.chalkak", "ACTIVE");
        client
            .execute(
                "UPDATE users SET deleted_at = now() WHERE id = $1",
                &[&withdrawn],
            )
            .unwrap();
        let service = AdminUserQueryService::new(deps(&db));
        let detail = service.get_user(active).unwrap();
        assert!(detail.signature.original_image_url.is_some());
        let detail = service.get_user(withdrawn).unwrap();
        assert_eq!(detail.status, AdminUserStatus::Withdrawn);
        assert!(detail.signature.original_image_url.is_none());
        assert!(detail.signature.thumbnail_image_url.is_none());
        let e = service.get_user(Uuid::new_v4()).unwrap_err();
        assert_eq!(e.message(), "사용자를 찾을 수 없습니다.");
    }
}
