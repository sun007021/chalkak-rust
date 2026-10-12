//! 원본 `AdminUserController`: `/api/v1/admin/users`.

use chalkak_http::Request;
use serde::Serialize;

use super::repository::{AdminUserSort, AdminUserStatus, PostCounts, UserStatus};
use super::service::{
    AdminUserDetail, AdminUserQueryService, AdminUserStatusService, UserListItem,
};
use crate::admin::binding::{
    MUST_NOT_BE_BLANK, MUST_NOT_BE_NULL, QueryParams, body_enum, has_text, page_params,
    size_message, utf16_len, validation,
};
use crate::admin::support::{AdminDeps, AdminPage, path_uuid, resolve_current_admin};
use crate::web::binding::read_json_object;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let query = AdminUserQueryService::new(deps.clone());
    let status = AdminUserStatusService::new(deps.clone());
    let list_deps = deps.clone();
    let detail_deps = deps.clone();
    let update_deps = deps.clone();
    let detail_query = query.clone();
    routes
        .get("/api/v1/admin/users", move |req| {
            resolve_current_admin(&list_deps)?;
            get_users(&query, req)
        })
        .get("/api/v1/admin/users/{userId}", move |req| {
            resolve_current_admin(&detail_deps)?;
            let detail = detail_query.get_user(path_uuid(req, "userId")?)?;
            Ok(ok_json(&UserDetailResponse::from(detail)))
        })
        .route(
            chalkak_http::Method::Patch,
            "/api/v1/admin/users/{userId}/status",
            move |req| {
                let admin_id = resolve_current_admin(&update_deps)?;
                let (new_status, reason) = read_status_update(req)?;
                let user_id = path_uuid(req, "userId")?;
                let result = status.update_status(user_id, admin_id, new_status, &reason)?;
                Ok(ok_json(&UserStatusResponse {
                    user_id: result.user_id.hyphenated().to_string(),
                    status: result.status.as_str(),
                }))
            },
        )
}

fn get_users(service: &AdminUserQueryService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let status = q.enumeration("status", AdminUserStatus::parse)?;
    let email = q.string("email");
    let sort = q.sort("sort", AdminUserSort::from_query)?;
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    if email.as_deref().is_some_and(|e| utf16_len(e) > 320) {
        return Err(validation("이메일 검색어가 올바르지 않습니다."));
    }
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_users(
        Some(status.unwrap_or(AdminUserStatus::Active)),
        email.as_deref(),
        sort,
        page,
        page_size,
    )?;
    Ok(ok_json(&UserListResponse::from(result)))
}

/// `AdminUserStatusUpdateRequest`: `@NotNull status`, `@NotBlank @Size(max = 500) reason`.
fn read_status_update(req: &Request) -> Result<(UserStatus, String), ApiError> {
    let body = read_json_object(req)?;
    let status = body_enum(
        &body,
        "status",
        &[
            ("ACTIVE", UserStatus::Active),
            ("BANNED", UserStatus::Banned),
        ],
    )?;
    let reason = body.string("reason")?;
    let Some(status) = status else {
        return Err(validation(MUST_NOT_BE_NULL));
    };
    let reason = match reason {
        Some(r) if has_text(&r) => r,
        _ => return Err(validation(MUST_NOT_BE_BLANK)),
    };
    if utf16_len(&reason) > 500 {
        return Err(validation(&size_message(500)));
    }
    Ok((status, reason))
}

#[derive(Serialize)]
struct PostCountsResponse {
    pending: i64,
    approved: i64,
    rejected: i64,
}

impl From<PostCounts> for PostCountsResponse {
    fn from(c: PostCounts) -> Self {
        PostCountsResponse {
            pending: c.pending,
            approved: c.approved,
            rejected: c.rejected,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    users: Vec<UserItemResponse>,
}

impl From<AdminPage<UserListItem>> for UserListResponse {
    fn from(page: AdminPage<UserListItem>) -> Self {
        UserListResponse {
            current_page: page.current_page,
            page_size: page.page_size,
            has_next: page.has_next,
            users: page.items.into_iter().map(UserItemResponse::from).collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserItemResponse {
    user_id: String,
    email: Option<String>,
    status: &'static str,
    app_version: Option<String>,
    social_provider: Option<String>,
    post_counts: PostCountsResponse,
    #[serde(serialize_with = "serialize_instant")]
    created_at: jiff::Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: jiff::Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<jiff::Timestamp>,
}

impl From<UserListItem> for UserItemResponse {
    fn from(item: UserListItem) -> Self {
        let s = item.summary;
        UserItemResponse {
            user_id: s.user_id.hyphenated().to_string(),
            email: s.email,
            status: item.status.as_str(),
            app_version: s.app_version,
            social_provider: s.social_provider,
            post_counts: s.post_counts.into(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            deleted_at: s.deleted_at,
        }
    }
}

pub fn serialize_optional_instant<S: serde::Serializer>(
    value: &Option<jiff::Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(at) => serialize_instant(at, serializer),
        None => serializer.serialize_none(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignatureResponse {
    original_image_url: Option<String>,
    thumbnail_image_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserDetailResponse {
    user_id: String,
    email: Option<String>,
    status: &'static str,
    app_version: Option<String>,
    social_provider: Option<String>,
    signature: SignatureResponse,
    post_counts: PostCountsResponse,
    #[serde(serialize_with = "serialize_instant")]
    created_at: jiff::Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: jiff::Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<jiff::Timestamp>,
}

impl From<AdminUserDetail> for UserDetailResponse {
    fn from(d: AdminUserDetail) -> Self {
        let s = d.summary;
        UserDetailResponse {
            user_id: s.user_id.hyphenated().to_string(),
            email: s.email,
            status: d.status.as_str(),
            app_version: s.app_version,
            social_provider: s.social_provider,
            signature: SignatureResponse {
                original_image_url: d.signature.original_image_url,
                thumbnail_image_url: d.signature.thumbnail_image_url,
            },
            post_counts: d.post_counts.into(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            deleted_at: s.deleted_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserStatusResponse {
    user_id: String,
    status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(body: serde_json::Value) -> Request {
        let mut r = Request::new(chalkak_http::Method::Patch, "/");
        r.headers.append("Content-Type", "application/json");
        r.body = body.to_string().into_bytes();
        r
    }

    fn message(result: Result<(UserStatus, String), ApiError>) -> String {
        match result {
            Err(ApiError::Validation(m)) => m,
            other => panic!("검증 오류가 아니다: {other:?}"),
        }
    }

    // AdminUserStatusUpdateRequest
    #[test]
    fn 상태_변경_요청_검증() {
        assert_eq!(
            message(read_status_update(&request(json!({"reason": "x"})))),
            "must not be null"
        );
        assert_eq!(
            message(read_status_update(&request(
                json!({"status": "BANNED", "reason": " "})
            ))),
            "must not be blank"
        );
        assert_eq!(
            message(read_status_update(&request(
                json!({"status": "BANNED", "reason": "가".repeat(501)})
            ))),
            "size must be between 0 and 500"
        );
        let (status, reason) =
            read_status_update(&request(json!({"status": "BANNED", "reason": "사유"}))).unwrap();
        assert_eq!(status, UserStatus::Banned);
        assert_eq!(reason, "사유");
        // 서수로도 읽는다(Jackson).
        let (status, _) =
            read_status_update(&request(json!({"status": 0, "reason": "사유"}))).unwrap();
        assert_eq!(status, UserStatus::Active);
        assert!(matches!(
            read_status_update(&request(json!({"status": "banned", "reason": "x"}))),
            Err(ApiError::UnreadableBody)
        ));
    }
}
