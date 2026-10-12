//! 원본 `NotificationController`(`/api/v1/notifications/**`)와 요청·응답 DTO.

use std::sync::Arc;

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_http::{Method, Request, Response};
use serde::Serialize;
use uuid::Uuid;

use super::domain::NotificationType;
use super::service::{
    NotificationDetailResult, NotificationInboxService, NotificationListResult,
    NotificationSummaryResult,
};
use crate::AppContext;
use crate::auth::UsableUserPolicy;
use crate::common::java_uuid_from_string;
use crate::web::binding::is_java_whitespace;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

const DEFAULT_PAGE: i32 = 1;
const DEFAULT_PAGE_SIZE: i32 = 20;
const MAX_PAGE_SIZE: i32 = 100;
const INVALID_QUERY_MESSAGE: &str = "조회 조건이 올바르지 않습니다.";

// ---------------------------------------------------------------------------------------------
// 응답 DTO
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    notifications: Vec<NotificationSummaryResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationSummaryResponse {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    title: String,
    body: String,
    thumbnail_image_url: Option<String>,
    #[serde(serialize_with = "serialize_opt_instant")]
    read_at: Option<jiff::Timestamp>,
    #[serde(serialize_with = "serialize_instant")]
    created_at: jiff::Timestamp,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationDetailResponse {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    title: String,
    body: String,
    original_image_url: Option<String>,
    rejection_reason: Option<String>,
    #[serde(serialize_with = "serialize_opt_instant")]
    read_at: Option<jiff::Timestamp>,
    #[serde(serialize_with = "serialize_instant")]
    created_at: jiff::Timestamp,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationUnreadStatusResponse {
    has_unread: bool,
}

fn serialize_opt_instant<S: serde::Serializer>(
    ts: &Option<jiff::Timestamp>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match ts {
        Some(ts) => serialize_instant(ts, s),
        None => s.serialize_none(),
    }
}

fn type_name(kind: NotificationType) -> &'static str {
    kind.as_str()
}

impl From<NotificationSummaryResult> for NotificationSummaryResponse {
    fn from(s: NotificationSummaryResult) -> Self {
        NotificationSummaryResponse {
            id: s.id.to_string(),
            kind: type_name(s.kind),
            title: s.title,
            body: s.body,
            thumbnail_image_url: s.thumbnail_image_url,
            read_at: s.read_at,
            created_at: s.created_at,
        }
    }
}

impl From<NotificationListResult> for NotificationListResponse {
    fn from(r: NotificationListResult) -> Self {
        NotificationListResponse {
            current_page: r.current_page,
            page_size: r.page_size,
            has_next: r.has_next,
            notifications: r.notifications.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<NotificationDetailResult> for NotificationDetailResponse {
    fn from(r: NotificationDetailResult) -> Self {
        NotificationDetailResponse {
            id: r.id.to_string(),
            kind: type_name(r.kind),
            title: r.title,
            body: r.body,
            original_image_url: r.original_image_url,
            rejection_reason: r.rejection_reason,
            read_at: r.read_at,
            created_at: r.created_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 라우트
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let service = Arc::new(NotificationInboxService::new(
        ctx.pool.clone(),
        ctx.clock.clone(),
        ctx.image_urls.clone(),
    ));
    let policy = Arc::new(UsableUserPolicy::new(ctx.pool.clone()));

    let (s, p) = (service.clone(), policy.clone());
    let routes = routes.get("/api/v1/notifications", move |req| {
        get_notifications(&s, &p, req)
    });
    let (s, p) = (service.clone(), policy.clone());
    let routes = routes.get("/api/v1/notifications/unread-status", move |_req| {
        get_unread_status(&s, &p)
    });
    let (s, p) = (service.clone(), policy.clone());
    let routes = routes.get("/api/v1/notifications/{notificationId}", move |req| {
        get_notification(&s, &p, req)
    });
    let (s, p) = (service.clone(), policy.clone());
    let routes = routes.route(
        Method::Patch,
        "/api/v1/notifications/{notificationId}/read",
        move |req| mark_read(&s, &p, req),
    );
    routes.route(
        Method::Patch,
        "/api/v1/notifications/read-all",
        move |_req| mark_all_read(&service, &policy),
    )
}

fn get_notifications(
    service: &NotificationInboxService,
    policy: &UsableUserPolicy,
    req: &Request,
) -> ApiResult {
    // 인자 해석(@Valid @ModelAttribute) -> 메서드 보안(@RequiresExistingUser) -> 서비스 순서다.
    let (page, page_size) = parse_list_request(req)?;
    let user = policy.validate_existing()?;
    let result = service.get_notifications(user.user_id, page, page_size)?;
    Ok(ok_json(&NotificationListResponse::from(result)))
}

fn get_notification(
    service: &NotificationInboxService,
    policy: &UsableUserPolicy,
    req: &Request,
) -> ApiResult {
    let user = policy.validate_existing()?;
    let id = parse_canonical_uuid(req.param("notificationId").unwrap_or_default())?;
    let result = service.get_notification(user.user_id, id)?;
    Ok(ok_json(&NotificationDetailResponse::from(result)))
}

fn get_unread_status(service: &NotificationInboxService, policy: &UsableUserPolicy) -> ApiResult {
    let user = policy.validate_existing()?;
    let has_unread = service.has_unread_notification(user.user_id)?;
    Ok(ok_json(&NotificationUnreadStatusResponse { has_unread }))
}

fn mark_read(
    service: &NotificationInboxService,
    policy: &UsableUserPolicy,
    req: &Request,
) -> ApiResult {
    let user = policy.validate_existing()?;
    let id = parse_canonical_uuid(req.param("notificationId").unwrap_or_default())?;
    service.mark_read(user.user_id, id)?;
    Ok(Response::new(204))
}

fn mark_all_read(service: &NotificationInboxService, policy: &UsableUserPolicy) -> ApiResult {
    let user = policy.validate_existing()?;
    service.mark_all_read(user.user_id)?;
    Ok(Response::new(204))
}

// ---------------------------------------------------------------------------------------------
// 요청 해석
// ---------------------------------------------------------------------------------------------

/// `CanonicalUuidParser.parse`: `UUID.fromString`이 받고 다시 문자열로 만든 값이 입력과 (대소문자 무시) 같아야 한다.
pub fn parse_canonical_uuid(value: &str) -> Result<Uuid, ApiError> {
    java_uuid_from_string(value)
        .filter(|uuid| uuid.hyphenated().to_string().eq_ignore_ascii_case(value))
        .ok_or_else(|| {
            ApiError::App(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "ID 형식이 올바르지 않습니다.",
            ))
        })
}

/// `NotificationListRequest`: `page`(기본 1, 1 이상)와 `pageSize`(기본 20, 1~100).
fn parse_list_request(req: &Request) -> Result<(i32, i32), ApiError> {
    let page = bind_integer(req, "page")?;
    let page_size = bind_integer(req, "pageSize")?;
    let page = page.unwrap_or(DEFAULT_PAGE);
    let page_size = page_size.unwrap_or(DEFAULT_PAGE_SIZE);
    if page < 1 || !(1..=MAX_PAGE_SIZE).contains(&page_size) {
        return Err(ApiError::Validation(INVALID_QUERY_MESSAGE.to_owned()));
    }
    Ok((page, page_size))
}

/// `Integer` 쿼리 값 바인딩. 없거나 빈 문자열이면 `None`, 같은 이름이 여러 번이면 첫 값을 쓴다.
fn bind_integer(req: &Request, name: &str) -> Result<Option<i32>, ApiError> {
    match req.query_param(name) {
        None | Some("") => Ok(None),
        Some(text) => parse_spring_integer(text)
            .map(Some)
            .ok_or_else(|| ApiError::ArgumentTypeMismatch(name.to_owned())),
    }
}

/// Spring `NumberUtils.parseNumber(text, Integer.class)`: 공백을 모두 지우고, `0x`·`#` 16진수면 `Integer.decode`,
/// 아니면 `Integer.valueOf`로 읽는다.
fn parse_spring_integer(text: &str) -> Option<i32> {
    let trimmed: String = text.chars().filter(|c| !is_java_whitespace(*c)).collect();
    let (negative, unsigned) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.as_str()),
    };
    let hex = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
        .or_else(|| unsigned.strip_prefix('#'));
    if let Some(digits) = hex {
        return parse_digits(digits, 16, negative);
    }
    let digits = if negative {
        unsigned
    } else {
        unsigned.strip_prefix('+').unwrap_or(unsigned)
    };
    parse_digits(digits, 10, negative)
}

/// 부호 없는 숫자열(`radix` 진법)을 `i32` 범위 안에서 읽는다. 빈 문자열과 숫자가 아닌 문자는 실패.
fn parse_digits(digits: &str, radix: u32, negative: bool) -> Option<i32> {
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for c in digits.chars() {
        value = value * i64::from(radix) + i64::from(c.to_digit(radix)?);
        if value > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &[(&str, &str)]) -> Request {
        let mut r = Request::new(Method::Get, "/api/v1/notifications");
        r.query = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        r
    }

    fn list(query: &[(&str, &str)]) -> Result<(i32, i32), ApiError> {
        parse_list_request(&request(query))
    }

    // NotificationListRequest: 생략하면 page=1, pageSize=20
    #[test]
    fn 목록_조건을_생략하면_기본값이다() {
        assert_eq!(list(&[]).unwrap(), (1, 20));
        assert_eq!(list(&[("page", ""), ("pageSize", "")]).unwrap(), (1, 20));
    }

    // NotificationControllerTest#getNotifications_invalidPageSize_returnsBadRequest
    #[test]
    fn 페이지_크기가_범위를_벗어나면_400이다() {
        for size in ["0", "101", "-1"] {
            let err = list(&[("pageSize", size)]).unwrap_err();
            assert_eq!(err.status(), 400, "{size}");
            assert_eq!(err.message(), "조회 조건이 올바르지 않습니다.");
        }
        assert_eq!(list(&[("pageSize", "100")]).unwrap(), (1, 100));
        assert_eq!(list(&[("pageSize", "1")]).unwrap(), (1, 1));
    }

    #[test]
    fn 페이지가_1_미만이면_400이다() {
        for page in ["0", "-3"] {
            let err = list(&[("page", page)]).unwrap_err();
            assert_eq!(err.message(), "조회 조건이 올바르지 않습니다.");
        }
        assert_eq!(list(&[("page", "7"), ("pageSize", "5")]).unwrap(), (7, 5));
    }

    #[test]
    fn 숫자가_아니면_이름이_붙은_형식_오류다() {
        let err = list(&[("page", "abc")]).unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(err.message(), "page: 요청 값의 형식이 올바르지 않습니다.");
        let err = list(&[("pageSize", "1.5")]).unwrap_err();
        assert_eq!(
            err.message(),
            "pageSize: 요청 값의 형식이 올바르지 않습니다."
        );
        assert!(list(&[("page", "2147483648")]).is_err());
    }

    #[test]
    fn 같은_이름이_여러_번이면_첫_값을_쓴다() {
        assert_eq!(list(&[("page", "2"), ("page", "x")]).unwrap().0, 2);
    }

    // Spring NumberUtils.parseNumber
    #[test]
    fn 정수는_스프링처럼_읽는다() {
        assert_eq!(parse_spring_integer("12"), Some(12));
        assert_eq!(parse_spring_integer("+12"), Some(12));
        assert_eq!(parse_spring_integer("-12"), Some(-12));
        assert_eq!(
            parse_spring_integer(" 1 2 "),
            Some(12),
            "공백은 모두 지운다"
        );
        assert_eq!(parse_spring_integer("007"), Some(7), "8진수가 아니다");
        assert_eq!(parse_spring_integer("0x1F"), Some(31));
        assert_eq!(parse_spring_integer("#1f"), Some(31));
        assert_eq!(parse_spring_integer("-0x10"), Some(-16));
        assert_eq!(parse_spring_integer("+0x10"), None);
        assert_eq!(parse_spring_integer("2147483647"), Some(i32::MAX));
        assert_eq!(parse_spring_integer("-2147483648"), Some(i32::MIN));
        assert_eq!(parse_spring_integer("2147483648"), None);
        assert_eq!(parse_spring_integer("0xFFFFFFFF"), None);
        for bad in ["", " ", "-", "+", "0x", "--1", "1e3", "٣x"] {
            assert_eq!(parse_spring_integer(bad), None, "{bad:?}");
        }
    }

    // CanonicalUuidParser
    #[test]
    fn 식별자는_정규_형식만_받는다() {
        let id = "0198f6c1-62ba-7d30-8b12-0f733b6572f2";
        assert_eq!(parse_canonical_uuid(id).unwrap().to_string(), id);
        assert!(parse_canonical_uuid(&id.to_uppercase()).is_ok());
        for bad in [
            "",
            "abc",
            "1-2-3-4-5",
            "0198f6c1-62ba-7d30-8b12-0f733b6572f2 ",
            "+1-2-3-4-5",
        ] {
            let err = parse_canonical_uuid(bad).unwrap_err();
            assert_eq!(err.status(), 400, "{bad:?}");
            assert_eq!(err.message(), "ID 형식이 올바르지 않습니다.");
        }
    }

    // NotificationListResponse/NotificationDetailResponse: null도 키를 남긴다
    #[test]
    fn 응답은_null_필드도_남기고_순서를_지킨다() {
        let created: jiff::Timestamp = "2026-09-29T09:00:00Z".parse().unwrap();
        let summary = NotificationSummaryResponse {
            id: "i".to_owned(),
            kind: "POST_APPROVED",
            title: "t".to_owned(),
            body: "b".to_owned(),
            thumbnail_image_url: None,
            read_at: None,
            created_at: created,
        };
        assert_eq!(
            crate::web::json::to_json(&summary),
            r#"{"id":"i","type":"POST_APPROVED","title":"t","body":"b","thumbnailImageUrl":null,"readAt":null,"createdAt":"2026-09-29T09:00:00Z"}"#
        );
        let detail = NotificationDetailResponse {
            id: "i".to_owned(),
            kind: "POST_REJECTED",
            title: "t".to_owned(),
            body: "b".to_owned(),
            original_image_url: Some("u".to_owned()),
            rejection_reason: Some("r".to_owned()),
            read_at: Some(created),
            created_at: created,
        };
        assert_eq!(
            crate::web::json::to_json(&detail),
            r#"{"id":"i","type":"POST_REJECTED","title":"t","body":"b","originalImageUrl":"u","rejectionReason":"r","readAt":"2026-09-29T09:00:00Z","createdAt":"2026-09-29T09:00:00Z"}"#
        );
        assert_eq!(
            crate::web::json::to_json(&NotificationUnreadStatusResponse { has_unread: true }),
            r#"{"hasUnread":true}"#
        );
    }
}
