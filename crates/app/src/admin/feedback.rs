//! 관리자 피드백 조회(원본 `AdminFeedbackController`, `AdminFeedbackQueryService`,
//! `AdminFeedbackQueryRepositoryImpl`): `GET /api/v1/admin/feedbacks`.

use chalkak_db::DbError;
use chalkak_http::Request;
use jiff::Timestamp;
use postgres::GenericClient;
use serde::Serialize;
use uuid::Uuid;

use crate::admin::binding::{QueryParams, page_params};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, limit_offset, resolve_current_admin,
    validate_pagination,
};
use crate::admin::user::repository::{AdminUserStatus, UserStatus};
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminFeedbackSort {
    CreatedAtDesc,
    CreatedAtAsc,
}

impl AdminFeedbackSort {
    pub fn from_query(value: &str) -> Option<AdminFeedbackSort> {
        match value {
            "createdAtDesc" => Some(AdminFeedbackSort::CreatedAtDesc),
            "createdAtAsc" => Some(AdminFeedbackSort::CreatedAtAsc),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FeedbackSummary {
    pub feedback_id: Uuid,
    pub content: String,
    pub created_at: Timestamp,
    pub author_id: Uuid,
    pub author_email: Option<String>,
    pub author_status: AdminUserStatus,
    pub author_app_version: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AdminFeedbackRepository;

impl AdminFeedbackRepository {
    pub fn find_feedbacks(
        &self,
        db: &mut impl GenericClient,
        sort: AdminFeedbackSort,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<FeedbackSummary>, DbError> {
        let direction = match sort {
            AdminFeedbackSort::CreatedAtDesc => "DESC",
            AdminFeedbackSort::CreatedAtAsc => "ASC",
        };
        let (limit, offset) = limit_offset(page, page_size);
        let sql = format!(
            "SELECT f.id, f.content, f.created_at, u.id AS author_id, u.email, \
             u.status::text AS status, u.app_version, u.deleted_at \
             FROM feedbacks f JOIN users u ON u.id = f.user_id \
             ORDER BY f.created_at {direction}, f.id {direction} LIMIT $1 OFFSET $2"
        );
        let rows = db.query(&sql, &[&limit, &offset])?;
        let items = rows
            .iter()
            .map(|row| {
                let status: String = row.try_get("status")?;
                let status = UserStatus::parse(&status)
                    .ok_or_else(|| DbError::Config(format!("알 수 없는 회원 상태: {status}")))?;
                let deleted_at: Option<Timestamp> = row.try_get("deleted_at")?;
                Ok(FeedbackSummary {
                    feedback_id: row.try_get("id")?,
                    content: row.try_get("content")?,
                    created_at: row.try_get("created_at")?,
                    author_id: row.try_get("author_id")?,
                    author_email: row.try_get("email")?,
                    author_status: AdminUserStatus::from_user(status, deleted_at),
                    author_app_version: row.try_get("app_version")?,
                })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        Ok(AdminPage::from_overfetched(items, page, page_size))
    }
}

#[derive(Clone)]
pub struct AdminFeedbackQueryService {
    deps: AdminDeps,
}

impl AdminFeedbackQueryService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminFeedbackQueryService { deps }
    }

    pub fn get_feedbacks(
        &self,
        sort: Option<AdminFeedbackSort>,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<FeedbackSummary>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let mut conn = self.deps.pool.get()?;
        Ok(AdminFeedbackRepository.find_feedbacks(
            &mut *conn,
            sort.unwrap_or(AdminFeedbackSort::CreatedAtDesc),
            page,
            page_size,
        )?)
    }
}

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let deps = deps.clone();
    let service = AdminFeedbackQueryService::new(deps.clone());
    routes.get("/api/v1/admin/feedbacks", move |req| {
        resolve_current_admin(&deps)?;
        get_feedbacks(&service, req)
    })
}

fn get_feedbacks(service: &AdminFeedbackQueryService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let sort = q.sort("sort", AdminFeedbackSort::from_query)?;
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_feedbacks(sort, page, page_size)?;
    Ok(ok_json(&FeedbackListResponse {
        current_page: result.current_page,
        page_size: result.page_size,
        has_next: result.has_next,
        feedbacks: result
            .items
            .into_iter()
            .map(|f| FeedbackResponse {
                feedback_id: f.feedback_id.hyphenated().to_string(),
                content: f.content,
                created_at: f.created_at,
                author: AuthorResponse {
                    user_id: f.author_id.hyphenated().to_string(),
                    email: f.author_email,
                    status: f.author_status.as_str(),
                    app_version: f.author_app_version,
                },
            })
            .collect(),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    feedbacks: Vec<FeedbackResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackResponse {
    feedback_id: String,
    content: String,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    author: AuthorResponse,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorResponse {
    user_id: String,
    email: Option<String>,
    status: &'static str,
    app_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testing::{deps, insert_user};
    use crate::testing::TestDb;

    // AdminFeedbackQueryServiceTest
    #[test]
    fn 정렬_값은_정확히_일치해야_한다() {
        assert_eq!(
            AdminFeedbackSort::from_query("createdAtAsc"),
            Some(AdminFeedbackSort::CreatedAtAsc)
        );
        assert_eq!(AdminFeedbackSort::from_query("CREATED_AT_ASC"), None);
        assert_eq!(AdminFeedbackSort::from_query(" createdAtAsc"), None);
    }

    #[test]
    fn 페이지_조건이_틀리면_조회_조건_오류다() {
        let Some(db) = TestDb::create() else { return };
        let service = AdminFeedbackQueryService::new(deps(&db));
        for (page, size) in [(0, 20), (1, 0), (1, 101)] {
            let e = service.get_feedbacks(None, page, size).unwrap_err();
            assert_eq!(e.message(), INVALID_QUERY_MESSAGE);
        }
    }

    // AdminFeedbackQueryRepositoryTest
    #[test]
    fn 작성자_상태와_함께_정렬해_읽는다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let active = insert_user(&mut client, "a@test.chalkak", "ACTIVE");
        let banned = insert_user(&mut client, "b@test.chalkak", "BANNED");
        let withdrawn = insert_user(&mut client, "c@test.chalkak", "ACTIVE");
        client
            .execute(
                "UPDATE users SET deleted_at = now() WHERE id = $1",
                &[&withdrawn],
            )
            .unwrap();
        for (user, content, at) in [
            (active, "첫째", "2026-10-01T00:00:00Z"),
            (banned, "둘째", "2026-10-02T00:00:00Z"),
            (withdrawn, "셋째", "2026-10-03T00:00:00Z"),
        ] {
            client
                .execute(
                    "INSERT INTO feedbacks (user_id, content, created_at) VALUES ($1, $2, $3::text::timestamptz)",
                    &[&user, &content, &at],
                )
                .unwrap();
        }
        let service = AdminFeedbackQueryService::new(deps(&db));
        let desc = service.get_feedbacks(None, 1, 20).unwrap();
        let contents: Vec<_> = desc.items.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, ["셋째", "둘째", "첫째"]);
        assert_eq!(desc.items[0].author_status, AdminUserStatus::Withdrawn);
        assert_eq!(desc.items[1].author_status, AdminUserStatus::Banned);
        assert_eq!(desc.items[2].author_status, AdminUserStatus::Active);
        assert!(!desc.has_next);
        let asc = service
            .get_feedbacks(Some(AdminFeedbackSort::CreatedAtAsc), 1, 2)
            .unwrap();
        assert_eq!(asc.items[0].content, "첫째");
        assert!(asc.has_next);
        let second_page = service
            .get_feedbacks(Some(AdminFeedbackSort::CreatedAtAsc), 2, 2)
            .unwrap();
        assert_eq!(second_page.items.len(), 1);
        assert_eq!(second_page.current_page, 2);
    }
}
