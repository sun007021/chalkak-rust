//! 원본 `AdminPostController`: `/api/v1/admin/posts`.

use chalkak_http::{Method, Request};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::repository::{AdminPostSort, ModerationStatus};
use super::service::{
    AdminPostCommandService, AdminPostQueryService, PostDetail, PostListItem, PostModerationResult,
};
use crate::admin::binding::{
    QueryParams, body_enum, code_points, has_text, java_trim, page_params, validation,
};
use crate::admin::support::{AdminDeps, AdminPage, no_content, path_uuid, resolve_current_admin};
use crate::admin::user::api::serialize_optional_instant;
use crate::web::binding::read_json_object;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let query = AdminPostQueryService::new(deps.clone());
    let command = AdminPostCommandService::new(deps.clone());
    let (list_deps, detail_deps, moderation_deps, delete_deps) =
        (deps.clone(), deps.clone(), deps.clone(), deps.clone());
    let (detail_query, moderation_command) = (query.clone(), command.clone());
    routes
        .get("/api/v1/admin/posts", move |req| {
            resolve_current_admin(&list_deps)?;
            get_posts(&query, req)
        })
        .get("/api/v1/admin/posts/{postId}", move |req| {
            resolve_current_admin(&detail_deps)?;
            let detail = detail_query.get_post(path_uuid(req, "postId")?)?;
            Ok(ok_json(&PostDetailResponse::from(detail)))
        })
        .route(
            Method::Put,
            "/api/v1/admin/posts/{postId}/moderation",
            move |req| {
                let admin_id = resolve_current_admin(&moderation_deps)?;
                let (status, reason) = read_moderation(req)?;
                let post_id = path_uuid(req, "postId")?;
                let result =
                    moderation_command.moderate(post_id, admin_id, status, reason.as_deref())?;
                Ok(ok_json(&ModerationResponse::from(result)))
            },
        )
        .route(Method::Delete, "/api/v1/admin/posts/{postId}", move |req| {
            let admin_id = resolve_current_admin(&delete_deps)?;
            let reason = read_deletion(req)?;
            let post_id = path_uuid(req, "postId")?;
            command.delete_post(post_id, admin_id, &reason)?;
            Ok(no_content())
        })
}

fn get_posts(service: &AdminPostQueryService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let status = q.enumeration("status", ModerationStatus::parse_list_status)?;
    let topic_id = q.uuid("topicId")?;
    let topic_date = q.date("topicDate")?;
    let user_id = q.uuid("userId")?;
    let created_at_from = q.instant("createdAtFrom")?;
    let created_at_to = q.instant("createdAtTo")?;
    let sort = q.sort("sort", AdminPostSort::from_query)?;
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_posts(
        status,
        topic_id,
        topic_date,
        user_id,
        created_at_from,
        created_at_to,
        sort,
        page,
        page_size,
    )?;
    Ok(ok_json(&PostListResponse::from(result)))
}

/// `AdminPostModerationRequest`: `@NotNull status`와 결정 규칙(`@AssertTrue`).
fn read_moderation(req: &Request) -> Result<(ModerationStatus, Option<String>), ApiError> {
    let body = read_json_object(req)?;
    let status = body_enum(&body, "status", &ModerationStatus::ALL)?;
    let reason = body.string("rejectionReason")?;
    let Some(status) = status else {
        return Err(validation("검수 상태가 필요합니다."));
    };
    let valid = match status {
        ModerationStatus::Approved => reason.is_none(),
        ModerationStatus::Rejected => reason
            .as_deref()
            .is_some_and(|r| has_text(r) && code_points(java_trim(r)) <= 500),
        _ => false,
    };
    if !valid {
        return Err(validation(
            "승인 요청에는 거절 사유를 입력할 수 없고, 거절 요청에는 500자 이하의 사유가 필요합니다.",
        ));
    }
    Ok((status, reason))
}

/// `AdminPostDeletionRequest`: `@NotBlank`와 500자 제한(`@AssertTrue`).
fn read_deletion(req: &Request) -> Result<String, ApiError> {
    let body = read_json_object(req)?;
    let reason = match body.string("reason")? {
        Some(r) if has_text(&r) => r,
        _ => return Err(validation("삭제 사유가 필요합니다.")),
    };
    if code_points(&reason) > 500 {
        return Err(validation("삭제 사유는 500자 이하여야 합니다."));
    }
    Ok(reason)
}

fn id(uuid: Uuid) -> String {
    uuid.hyphenated().to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorResponse {
    user_id: String,
    email: Option<String>,
    status: &'static str,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    posts: Vec<PostItemResponse>,
}

impl From<AdminPage<PostListItem>> for PostListResponse {
    fn from(page: AdminPage<PostListItem>) -> Self {
        PostListResponse {
            current_page: page.current_page,
            page_size: page.page_size,
            has_next: page.has_next,
            posts: page.items.into_iter().map(PostItemResponse::from).collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListTopicResponse {
    topic_id: String,
    title: String,
    topic_date: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListPhotoResponse {
    photo_id: String,
    original_image_url: Option<String>,
    thumbnail_image_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostItemResponse {
    post_id: String,
    title: Option<String>,
    moderation_status: &'static str,
    author: AuthorResponse,
    topic: ListTopicResponse,
    photo: ListPhotoResponse,
    like_count: i64,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    moderated_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

impl From<PostListItem> for PostItemResponse {
    fn from(item: PostListItem) -> Self {
        let r = item.row;
        PostItemResponse {
            post_id: id(r.post_id),
            title: r.title,
            moderation_status: r.moderation_status.as_str(),
            author: AuthorResponse {
                user_id: id(r.author_id),
                email: r.author_email,
                status: r.author_status.as_str(),
                deleted_at: r.author_deleted_at,
            },
            topic: ListTopicResponse {
                topic_id: id(r.topic_id),
                title: r.topic_title,
                topic_date: r.topic_date.to_string(),
            },
            photo: ListPhotoResponse {
                photo_id: id(r.photo_id),
                original_image_url: item.original_image_url,
                thumbnail_image_url: item.thumbnail_image_url,
            },
            like_count: r.like_count,
            created_at: r.created_at,
            moderated_at: r.moderated_at,
            deleted_at: r.deleted_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetailTopicResponse {
    topic_id: String,
    title: String,
    topic_date: String,
    #[serde(serialize_with = "serialize_instant")]
    starts_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    ends_at: Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

/// `PhotoMetadataResponse`: `width`·`height`·`byteSize`만, 숫자가 아니면 빼고 나간다(`NON_NULL`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetadataResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_size: Option<i64>,
}

/// `Number.intValue()`: 정수는 하위 32비트, 소수는 0 방향으로 잘라 int 범위로 포화한다.
fn int_value(value: Option<&Value>) -> Option<i32> {
    let n = value?.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i as i32);
    }
    if let Some(u) = n.as_u64() {
        return Some(u as i32);
    }
    n.as_f64().map(|f| f as i32)
}

/// `Number.longValue()`
fn long_value(value: Option<&Value>) -> Option<i64> {
    let n = value?.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    if let Some(u) = n.as_u64() {
        return Some(u as i64);
    }
    n.as_f64().map(|f| f as i64)
}

impl MetadataResponse {
    fn from_value(metadata: &Value) -> Self {
        let get = |key: &str| metadata.as_object().and_then(|m| m.get(key));
        MetadataResponse {
            width: int_value(get("width")),
            height: int_value(get("height")),
            byte_size: long_value(get("byteSize")),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetailPhotoResponse {
    photo_id: String,
    original_image_url: Option<String>,
    thumbnail_image_url: Option<String>,
    metadata: MetadataResponse,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageUploadResponse {
    upload_id: String,
    status: String,
    rejection_reason: Option<String>,
    #[serde(serialize_with = "serialize_optional_instant")]
    created_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_optional_instant")]
    updated_at: Option<Timestamp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostDetailResponse {
    post_id: String,
    title: Option<String>,
    moderation_status: &'static str,
    author: AuthorResponse,
    topic: DetailTopicResponse,
    photo: DetailPhotoResponse,
    image_upload: Option<ImageUploadResponse>,
    like_count: i64,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    moderated_at: Option<Timestamp>,
    moderated_by: Option<String>,
    rejection_reason: Option<String>,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

impl From<PostDetail> for PostDetailResponse {
    fn from(detail: PostDetail) -> Self {
        let r = detail.row;
        PostDetailResponse {
            post_id: id(r.post_id),
            title: r.title,
            moderation_status: r.moderation_status.as_str(),
            author: AuthorResponse {
                user_id: id(r.author_id),
                email: r.author_email,
                status: r.author_status.as_str(),
                deleted_at: r.author_deleted_at,
            },
            topic: DetailTopicResponse {
                topic_id: id(r.topic_id),
                title: r.topic_title,
                topic_date: r.topic_date.to_string(),
                starts_at: r.topic_starts_at,
                ends_at: r.topic_ends_at,
                deleted_at: r.topic_deleted_at,
            },
            photo: DetailPhotoResponse {
                photo_id: id(r.photo_id),
                original_image_url: detail.original_image_url,
                thumbnail_image_url: detail.thumbnail_image_url,
                metadata: MetadataResponse::from_value(&r.photo_metadata),
                created_at: r.photo_created_at,
                updated_at: r.photo_updated_at,
                deleted_at: r.photo_deleted_at,
            },
            image_upload: r.upload_id.map(|upload_id| ImageUploadResponse {
                upload_id: id(upload_id),
                status: r.upload_status.unwrap_or_default(),
                rejection_reason: r.upload_rejection_reason,
                created_at: r.upload_created_at,
                updated_at: r.upload_updated_at,
            }),
            like_count: r.like_count,
            created_at: r.created_at,
            updated_at: r.updated_at,
            moderated_at: r.moderated_at,
            moderated_by: r.moderated_by.map(id),
            rejection_reason: r.rejection_reason,
            deleted_at: r.deleted_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModerationResponse {
    post_id: String,
    moderation_status: &'static str,
    moderated_by: String,
    #[serde(serialize_with = "serialize_instant")]
    moderated_at: Timestamp,
    rejection_reason: Option<String>,
}

impl From<PostModerationResult> for ModerationResponse {
    fn from(r: PostModerationResult) -> Self {
        ModerationResponse {
            post_id: id(r.post_id),
            moderation_status: r.moderation_status.as_str(),
            moderated_by: id(r.moderated_by),
            moderated_at: r.moderated_at,
            rejection_reason: r.rejection_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::json::to_json;
    use serde_json::json;

    fn request(method: Method, body: Value) -> Request {
        let mut r = Request::new(method, "/");
        r.headers.append("Content-Type", "application/json");
        r.body = body.to_string().into_bytes();
        r
    }

    fn message<T: std::fmt::Debug>(r: Result<T, ApiError>) -> String {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => panic!("검증 오류가 아니다: {other:?}"),
        }
    }

    // AdminPostModerationRequestTest
    #[test]
    fn 검수_요청_검증() {
        let decision = "승인 요청에는 거절 사유를 입력할 수 없고, 거절 요청에는 500자 이하의 사유가 필요합니다.";
        let m = |body: Value| message(read_moderation(&request(Method::Put, body)));
        assert_eq!(m(json!({})), "검수 상태가 필요합니다.");
        assert_eq!(m(json!({"status": "PENDING"})), decision);
        assert_eq!(m(json!({"status": "VALIDATING"})), decision);
        assert_eq!(
            m(json!({"status": "APPROVED", "rejectionReason": "x"})),
            decision
        );
        assert_eq!(
            m(json!({"status": "APPROVED", "rejectionReason": ""})),
            decision
        );
        assert_eq!(m(json!({"status": "REJECTED"})), decision);
        assert_eq!(
            m(json!({"status": "REJECTED", "rejectionReason": " "})),
            decision
        );
        assert_eq!(
            m(json!({"status": "REJECTED", "rejectionReason": "가".repeat(501)})),
            decision
        );
        let (s, r) = read_moderation(&request(Method::Put, json!({"status": "APPROVED"}))).unwrap();
        assert_eq!((s, r), (ModerationStatus::Approved, None));
        let (s, r) = read_moderation(&request(
            Method::Put,
            json!({"status": "REJECTED", "rejectionReason": "가".repeat(500)}),
        ))
        .unwrap();
        assert_eq!(s, ModerationStatus::Rejected);
        assert!(r.is_some());
        // 서수 3은 REJECTED
        assert!(
            read_moderation(&request(
                Method::Put,
                json!({"status": 3, "rejectionReason": "x"})
            ))
            .is_ok()
        );
        assert!(matches!(
            read_moderation(&request(Method::Put, json!({"status": "approved"}))),
            Err(ApiError::UnreadableBody)
        ));
    }

    // AdminPostDeletionRequestTest
    #[test]
    fn 삭제_요청_검증() {
        let m = |body: Value| message(read_deletion(&request(Method::Delete, body)));
        assert_eq!(m(json!({})), "삭제 사유가 필요합니다.");
        assert_eq!(m(json!({"reason": "  "})), "삭제 사유가 필요합니다.");
        assert_eq!(
            m(json!({"reason": "가".repeat(501)})),
            "삭제 사유는 500자 이하여야 합니다."
        );
        // 이모지는 코드 포인트 하나로 센다.
        assert!(
            read_deletion(&request(
                Method::Delete,
                json!({"reason": "😀".repeat(500)})
            ))
            .is_ok()
        );
        assert_eq!(
            read_deletion(&request(Method::Delete, json!({"reason": "사유"}))).unwrap(),
            "사유"
        );
    }

    // PhotoMetadataResponse
    #[test]
    fn 사진_메타데이터는_숫자만_안전_키만_내보낸다() {
        let m = |v: Value| to_json(&MetadataResponse::from_value(&v));
        assert_eq!(
            m(json!({"width": 4000, "height": 3000, "byteSize": 2048, "camera": "x"})),
            r#"{"width":4000,"height":3000,"byteSize":2048}"#
        );
        assert_eq!(
            m(json!({"width": 1000.9, "height": "tall"})),
            r#"{"width":1000}"#
        );
        assert_eq!(m(json!({})), "{}");
        assert_eq!(m(json!({"width": 4294967297_i64})), r#"{"width":1}"#);
        assert_eq!(
            m(json!({"byteSize": 1e30})),
            r#"{"byteSize":9223372036854775807}"#
        );
    }
}
