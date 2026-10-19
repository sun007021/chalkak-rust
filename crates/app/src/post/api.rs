//! 원본 `PostController`(`/api/v1/posts`)와 요청·응답 DTO.
//!
//! 요청 처리 순서는 Spring MVC와 같다. 인자 해석(쿼리 바인딩, 본문 읽기와 검증) -> 메서드 보안(`@RequiresUsableUser`,
//! `@RequiresExistingUser`) -> 컨트롤러 본문(경로 변수 `CanonicalUuidParser`) -> 서비스.

use chalkak_http::{Method, Request};
use serde::Serialize;

use super::command::{
    PostCommandService, PostCreationResult, PostImageUploadResult, PostUpdateResult,
};
use super::domain::PostTitle;
use super::query::{
    PostCalendarResult, PostDetail, PostListResult, PostQueryService, PostSort, TodayPostStatus,
};
use super::request::{jackson_uuid, parse_canonical_uuid, spring_integer};
use super::support::UserGuard;
use crate::AppContext;
use crate::security::login_user;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, binding, context, created_json, ok_json};

// ---------------------------------------------------------------------------------------------
// 응답
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostImageUploadResponse {
    upload_id: String,
    upload_url: String,
    expires_in_seconds: u64,
    content_type: String,
    max_bytes: u64,
}

impl From<PostImageUploadResult> for PostImageUploadResponse {
    fn from(r: PostImageUploadResult) -> Self {
        PostImageUploadResponse {
            upload_id: r.upload_id.to_string(),
            upload_url: r.upload_url,
            expires_in_seconds: r.expires_in_seconds,
            content_type: r.content_type,
            max_bytes: r.max_bytes,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostCreateResponse {
    post_id: String,
    moderation_status: &'static str,
}

impl From<PostCreationResult> for PostCreateResponse {
    fn from(r: PostCreationResult) -> Self {
        PostCreateResponse {
            post_id: r.post_id.to_string(),
            moderation_status: r.moderation_status.as_str(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostUpdateResponse {
    post_id: String,
    title: Option<String>,
}

impl From<PostUpdateResult> for PostUpdateResponse {
    fn from(r: PostUpdateResult) -> Self {
        PostUpdateResponse {
            post_id: r.post_id.to_string(),
            title: r.title,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TopicResponse {
    id: String,
    title: String,
    topic_date: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostDetailResponse {
    id: String,
    topic: TopicResponse,
    original_image_url: Option<String>,
    thumbnail_image_url: Option<String>,
    signature_original_image_url: Option<String>,
    title: Option<String>,
    like_count: i64,
    is_liked: bool,
}

impl From<PostDetail> for PostDetailResponse {
    fn from(d: PostDetail) -> Self {
        PostDetailResponse {
            id: d.id.to_string(),
            topic: TopicResponse {
                id: d.topic.id.to_string(),
                title: d.topic.title,
                topic_date: d.topic.topic_date.to_string(),
            },
            original_image_url: d.original_image_url,
            thumbnail_image_url: d.thumbnail_image_url,
            signature_original_image_url: d.signature_original_image_url,
            title: d.title,
            like_count: d.like_count,
            is_liked: d.is_liked,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TodayPostStatusResponse {
    topic_date: String,
    is_posted: bool,
    post_id: Option<String>,
    moderation_status: Option<&'static str>,
}

impl From<TodayPostStatus> for TodayPostStatusResponse {
    fn from(s: TodayPostStatus) -> Self {
        TodayPostStatusResponse {
            topic_date: s.topic_date.to_string(),
            is_posted: s.is_posted,
            post_id: s.post_id.map(|id| id.to_string()),
            moderation_status: s.moderation_status.map(|m| m.as_str()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalendarPostResponse {
    topic_date: String,
    post_id: String,
    thumbnail_image_url: Option<String>,
    status: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostCalendarResponse {
    year: i32,
    month: i32,
    posts: Vec<CalendarPostResponse>,
}

impl From<PostCalendarResult> for PostCalendarResponse {
    fn from(r: PostCalendarResult) -> Self {
        PostCalendarResponse {
            year: r.year,
            month: r.month,
            posts: r
                .posts
                .into_iter()
                .map(|p| CalendarPostResponse {
                    topic_date: p.topic_date.to_string(),
                    post_id: p.post_id.to_string(),
                    thumbnail_image_url: p.thumbnail_image_url,
                    status: p.status.as_str(),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct CalendarMonthResponse {
    year: i32,
    month: i32,
}

#[derive(Serialize)]
struct PostCalendarMonthsResponse {
    months: Vec<CalendarMonthResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostListItemResponse {
    id: String,
    original_image_url: Option<String>,
    thumbnail_image_url: Option<String>,
    signature_original_image_url: Option<String>,
    signature_thumbnail_image_url: Option<String>,
    title: Option<String>,
    #[serde(serialize_with = "serialize_instant")]
    submitted_at: jiff::Timestamp,
    like_count: i64,
    is_liked: bool,
    is_mine: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    random_seed: Option<String>,
    posts: Vec<PostListItemResponse>,
}

impl From<PostListResult> for PostListResponse {
    fn from(r: PostListResult) -> Self {
        PostListResponse {
            current_page: r.current_page,
            page_size: r.page_size,
            has_next: r.has_next,
            random_seed: r.random_seed,
            posts: r
                .posts
                .into_iter()
                .map(|p| PostListItemResponse {
                    id: p.id.to_string(),
                    original_image_url: p.original_image_url,
                    thumbnail_image_url: p.thumbnail_image_url,
                    signature_original_image_url: p.signature_original_image_url,
                    signature_thumbnail_image_url: p.signature_thumbnail_image_url,
                    title: p.title,
                    submitted_at: p.submitted_at,
                    like_count: p.like_count,
                    is_liked: p.is_liked,
                    is_mine: p.is_mine,
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 요청
// ---------------------------------------------------------------------------------------------

const TITLE_TOO_LONG: &str = "제목은 10자 이하여야 합니다.";
const LIST_CONDITION_INVALID: &str = "조회 조건이 올바르지 않습니다.";
const CALENDAR_INVALID: &str = "조회 연월이 올바르지 않습니다.";

/// `PostCreateRequest`
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PostCreateRequest {
    pub topic_id: uuid::Uuid,
    pub photo_upload_id: uuid::Uuid,
    pub title: Option<String>,
}

/// 본문 읽기 -> `@Valid`. 제약 위반이 여럿이면 어느 메시지가 나갈지는 검증기 내부 순서에 달려 있어,
/// 원본을 직접 확인한 순서(주제, 사진 업로드, 제목)를 따른다.
pub(crate) fn read_create_request(req: &Request) -> Result<PostCreateRequest, ApiError> {
    let body = binding::read_json_object(req)?;
    let topic_id = jackson_uuid(body.value("topicId"))?;
    let photo_upload_id = jackson_uuid(body.value("photoUploadId"))?;
    let title = body.string("title")?;
    let Some(topic_id) = topic_id else {
        return Err(ApiError::Validation(
            "주제 정보가 올바르지 않습니다.".to_owned(),
        ));
    };
    let Some(photo_upload_id) = photo_upload_id else {
        return Err(ApiError::Validation(
            "사진 업로드 정보가 올바르지 않습니다.".to_owned(),
        ));
    };
    if !PostTitle::is_valid(title.as_deref()) {
        return Err(ApiError::Validation(TITLE_TOO_LONG.to_owned()));
    }
    Ok(PostCreateRequest {
        topic_id,
        photo_upload_id,
        title,
    })
}

/// `PostUpdateRequest`: `title`은 값이 `null`이어도 키는 있어야 한다(`@JsonProperty(required = true)`).
pub(crate) fn read_update_request(req: &Request) -> Result<Option<String>, ApiError> {
    let body = binding::read_json_object(req)?;
    if body.value("title").is_none() {
        return Err(ApiError::UnreadableBody);
    }
    let title = body.string("title")?;
    if !PostTitle::is_valid(title.as_deref()) {
        return Err(ApiError::Validation(TITLE_TOO_LONG.to_owned()));
    }
    Ok(title)
}

/// `PostListRequest`(바인딩 후 검증을 거친 값).
#[derive(Debug, PartialEq)]
pub(crate) struct PostListRequest {
    pub topic_date: binding::LocalDate,
    pub sort: PostSort,
    pub random_seed: Option<String>,
    pub page: i32,
    pub page_size: i32,
}

/// `PostSortConverter`와 Spring의 열거형 대체 변환. 변환기는 소문자 이름만 받는다. 실패하면 Spring은 문자열을 잘라
/// 열거형 상수 이름과 비교해 보고(`RECENT`), 빈 문자열이면 값 없음으로 본다.
struct NotASort;

fn bind_sort(text: &str) -> Result<Option<PostSort>, NotASort> {
    if let Ok(sort) = PostSort::from_name(text) {
        return Ok(Some(sort));
    }
    match text.trim() {
        "" => Ok(None),
        "RECENT" => Ok(Some(PostSort::Recent)),
        "POPULAR" => Ok(Some(PostSort::Popular)),
        "RANDOM" => Ok(Some(PostSort::Random)),
        _ => Err(NotASort),
    }
}

/// `@Valid @ModelAttribute PostListRequest`: 바인딩 실패(생성자 인자 순서대로 첫 실패)가 검증보다 먼저 나간다.
pub(crate) fn bind_list_request(req: &Request) -> Result<PostListRequest, ApiError> {
    let mismatch = |name: &str| ApiError::ArgumentTypeMismatch(name.to_owned());

    let topic_date = match req.query_param("topicDate") {
        Some(text) if !text.chars().all(binding::is_java_whitespace) => {
            Some(binding::parse_iso_date(text).ok_or_else(|| mismatch("topicDate"))?)
        }
        _ => None,
    };
    let sort = match req.query_param("sort") {
        Some(text) => bind_sort(text).map_err(|_| mismatch("sort"))?,
        None => None,
    };
    let random_seed = req.query_param("randomSeed").map(str::to_owned);
    let page = match req.query_param("page") {
        Some(text) => spring_integer(text).map_err(|_| mismatch("page"))?,
        None => None,
    };
    let page_size = match req.query_param("pageSize") {
        Some(text) => spring_integer(text).map_err(|_| mismatch("pageSize"))?,
        None => None,
    };

    let sort = sort.unwrap_or(PostSort::Recent);
    let page = page.unwrap_or(1);
    let page_size = page_size.unwrap_or(20);

    let invalid = || ApiError::Validation(LIST_CONDITION_INVALID.to_owned());
    let topic_date = topic_date.ok_or_else(invalid)?;
    if let Some(seed) = &random_seed
        && (!(1..=64).contains(&seed.len())
            || !seed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    {
        return Err(invalid());
    }
    if page < 1 || !(1..=100).contains(&page_size) {
        return Err(invalid());
    }
    Ok(PostListRequest {
        topic_date,
        sort,
        random_seed,
        page,
        page_size,
    })
}

/// `@Valid @ModelAttribute PostCalendarRequest`
pub(crate) fn bind_calendar_request(req: &Request) -> Result<(i32, i32), ApiError> {
    let mismatch = |name: &str| ApiError::ArgumentTypeMismatch(name.to_owned());
    let year = match req.query_param("year") {
        Some(text) => spring_integer(text).map_err(|_| mismatch("year"))?,
        None => None,
    };
    let month = match req.query_param("month") {
        Some(text) => spring_integer(text).map_err(|_| mismatch("month"))?,
        None => None,
    };
    let invalid = || ApiError::Validation(CALENDAR_INVALID.to_owned());
    match (year, month) {
        (Some(year), Some(month)) if (1..=9999).contains(&year) && (1..=12).contains(&month) => {
            Ok((year, month))
        }
        _ => Err(invalid()),
    }
}

// ---------------------------------------------------------------------------------------------
// 라우트
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let guard = UserGuard::new(ctx.pool.clone());
    let command = ctx.post.command_service(ctx);
    let query = ctx.post.query_service(ctx);

    let upload = (guard.clone(), command.clone());
    let create = (guard.clone(), command.clone());
    let update = (guard.clone(), command.clone());
    let delete = (guard.clone(), command.clone());
    let today = (guard.clone(), query.clone());
    let calendar = (guard.clone(), query.clone());
    let months = (guard.clone(), query.clone());
    let detail = (guard, query.clone());

    routes
        .route(Method::Post, "/api/v1/posts/uploads", move |req| {
            create_post_image_upload(&upload.0, &upload.1, req)
        })
        .route(Method::Post, "/api/v1/posts", move |req| {
            create_post(&create.0, &create.1, req)
        })
        .route(Method::Put, "/api/v1/posts/{postId}", move |req| {
            update_post(&update.0, &update.1, req)
        })
        .route(Method::Delete, "/api/v1/posts/{postId}", move |req| {
            delete_post(&delete.0, &delete.1, req)
        })
        .route(Method::Get, "/api/v1/posts", move |req| {
            get_posts(&query, req)
        })
        .route(Method::Get, "/api/v1/posts/today", move |req| {
            get_today(&today.0, &today.1, req)
        })
        .route(Method::Get, "/api/v1/posts/calendar", move |req| {
            get_calendar(&calendar.0, &calendar.1, req)
        })
        .route(Method::Get, "/api/v1/posts/calendar/months", move |req| {
            get_calendar_months(&months.0, &months.1, req)
        })
        .route(Method::Get, "/api/v1/posts/{postId}", move |req| {
            get_post(&detail.0, &detail.1, req)
        })
}

fn path_post_id(req: &Request) -> Result<uuid::Uuid, ApiError> {
    parse_canonical_uuid(req.param("postId").unwrap_or_default())
}

fn create_post_image_upload(
    guard: &UserGuard,
    command: &PostCommandService,
    _req: &Request,
) -> ApiResult {
    let user = guard.validate_usable()?;
    let result = command.create_post_image_upload(user.user_id)?;
    Ok(ok_json(&PostImageUploadResponse::from(result)))
}

fn create_post(guard: &UserGuard, command: &PostCommandService, req: &Request) -> ApiResult {
    let request = read_create_request(req)?;
    let user = guard.validate_usable()?;
    let result = command.create_post(
        user.user_id,
        request.topic_id,
        request.photo_upload_id,
        request.title.as_deref(),
    )?;
    Ok(created_json(&PostCreateResponse::from(result)))
}

fn update_post(guard: &UserGuard, command: &PostCommandService, req: &Request) -> ApiResult {
    let title = read_update_request(req)?;
    let user = guard.validate_usable()?;
    let post_id = path_post_id(req)?;
    let result = command.update_post(user.user_id, post_id, title.as_deref())?;
    Ok(ok_json(&PostUpdateResponse::from(result)))
}

fn delete_post(guard: &UserGuard, command: &PostCommandService, req: &Request) -> ApiResult {
    let user = guard.validate_existing()?;
    let post_id = path_post_id(req)?;
    command.delete_post(user.user_id, post_id)?;
    Ok(chalkak_http::Response::new(204))
}

fn get_posts(query: &PostQueryService, req: &Request) -> ApiResult {
    let request = bind_list_request(req)?;
    // 이 경로만 회원 상태 판정을 붙이지 않는다. 비로그인 조회를 허용하는 경로라서, 로그인 상태의 회원 판정은
    // 서비스가 식별자가 있을 때만 한다.
    let user_id = login_user().ok().map(|user| user.user_id);
    let result = query.get_posts(
        request.topic_date,
        request.sort,
        request.random_seed.as_deref(),
        request.page,
        request.page_size,
        user_id,
    )?;
    // 접근 로그가 남길 조회 조건. 컨트롤러가 검증과 응답 변환을 마친 값만 쓴다.
    context::set_feed_query(
        match request.sort {
            PostSort::Recent => "recent",
            PostSort::Popular => "popular",
            PostSort::Random => "random",
        },
        i64::from(result.current_page),
        i64::from(result.page_size),
    );
    Ok(ok_json(&PostListResponse::from(result)))
}

fn get_today(guard: &UserGuard, query: &PostQueryService, _req: &Request) -> ApiResult {
    let user = guard.validate_usable()?;
    let status = query.get_my_today_post_status(user.user_id)?;
    Ok(ok_json(&TodayPostStatusResponse::from(status)))
}

fn get_calendar(guard: &UserGuard, query: &PostQueryService, req: &Request) -> ApiResult {
    let (year, month) = bind_calendar_request(req)?;
    let user = guard.validate_existing()?;
    let result = query.get_my_post_calendar(user.user_id, year, month)?;
    Ok(ok_json(&PostCalendarResponse::from(result)))
}

fn get_calendar_months(guard: &UserGuard, query: &PostQueryService, _req: &Request) -> ApiResult {
    let user = guard.validate_existing()?;
    let months = query.get_my_post_calendar_months(user.user_id)?;
    Ok(ok_json(&PostCalendarMonthsResponse {
        months: months
            .into_iter()
            .map(|(year, month)| CalendarMonthResponse { year, month })
            .collect(),
    }))
}

fn get_post(guard: &UserGuard, query: &PostQueryService, req: &Request) -> ApiResult {
    let user = guard.validate_existing()?;
    let post_id = path_post_id(req)?;
    let detail = query.get_post(post_id, user.user_id)?;
    Ok(ok_json(&PostDetailResponse::from(detail)))
}
