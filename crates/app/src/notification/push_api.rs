//! 원본 `NotificationSettingsController`(`/api/v1/notification-settings`), `PushDeviceController`
//! (`/api/v1/push-devices/current`)와 요청·응답 DTO.

use std::sync::Arc;

use chalkak_http::{Request, Response};
use serde::Serialize;
use serde_json::Value;

use super::push_device::PushDeviceService;
use super::settings::{NotificationSettingsResult, NotificationSettingsService};
use crate::AppContext;
use crate::auth::UsableUserPolicy;
use crate::web::binding::{self, JsonObject};
use crate::web::{ApiError, ApiResult, Routes, ok_json};

const UPDATE_REQUIRED: &str = "변경할 푸시 수신 설정을 하나 이상 입력해주세요.";
const TOKEN_REQUIRED: &str = "FCM 토큰이 필요합니다.";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotificationSettingsResponse {
    topic_push_enabled: bool,
    moderation_push_enabled: bool,
}

impl From<NotificationSettingsResult> for NotificationSettingsResponse {
    fn from(r: NotificationSettingsResult) -> Self {
        NotificationSettingsResponse {
            topic_push_enabled: r.topic_push_enabled,
            moderation_push_enabled: r.moderation_push_enabled,
        }
    }
}

/// `Boolean` 필드를 Jackson처럼 읽는다. 없거나 `null`이면 `None`, 불리언 리터럴과 `"true"`/`"false"` 문자열(대소문자
/// 무시), 숫자(0은 거짓, 그 밖은 참)를 받고 빈 문자열은 `None`이다. 그 밖의 모양(객체·배열 등)은 본문 오류(400).
fn read_boolean(body: &JsonObject, name: &str) -> Result<Option<bool>, ApiError> {
    match body.value(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(0) => Ok(Some(false)),
            Some(_) => Ok(Some(true)),
            None => Err(ApiError::UnreadableBody),
        },
        Some(Value::String(s)) => match s.as_str() {
            "" => Ok(None),
            t if t.eq_ignore_ascii_case("true") => Ok(Some(true)),
            t if t.eq_ignore_ascii_case("false") => Ok(Some(false)),
            _ => Err(ApiError::UnreadableBody),
        },
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let settings = NotificationSettingsService::new(ctx.pool.clone(), ctx.clock.clone());
    let devices = PushDeviceService::new(ctx.pool.clone(), ctx.clock.clone());
    let policy = Arc::new(UsableUserPolicy::new(ctx.pool.clone()));

    let (s, p) = (settings.clone(), policy.clone());
    let routes = routes.get("/api/v1/notification-settings", move |_req| {
        get_settings(&s, &p)
    });
    let p = policy.clone();
    let routes = routes.route(
        chalkak_http::Method::Patch,
        "/api/v1/notification-settings",
        move |req| update_settings(&settings, &p, req),
    );
    routes.put("/api/v1/push-devices/current", move |req| {
        register_current_device(&devices, &policy, req)
    })
}

// 인자 해석(@Valid @RequestBody) -> 메서드 보안(@RequiresExistingUser) -> 서비스 순서다.

fn get_settings(service: &NotificationSettingsService, policy: &UsableUserPolicy) -> ApiResult {
    let user = policy.validate_existing()?;
    let result = service.get_settings(user.user_id)?;
    Ok(ok_json(&NotificationSettingsResponse::from(result)))
}

fn update_settings(
    service: &NotificationSettingsService,
    policy: &UsableUserPolicy,
    req: &Request,
) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let topic = read_boolean(&body, "topicPushEnabled")?;
    let moderation = read_boolean(&body, "moderationPushEnabled")?;
    if topic.is_none() && moderation.is_none() {
        return Err(ApiError::Validation(UPDATE_REQUIRED.to_owned()));
    }
    let user = policy.validate_existing()?;
    service.update_settings(user.user_id, topic, moderation)?;
    Ok(Response::new(204))
}

fn register_current_device(
    service: &PushDeviceService,
    policy: &UsableUserPolicy,
    req: &Request,
) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let token = body.string("fcmToken")?;
    // `@NotBlank`는 `trim()`(공백 문자 이하 제어 문자 포함) 기준이다.
    let Some(token) = token.filter(|t| t.chars().any(|c| c > ' ')) else {
        return Err(ApiError::Validation(TOKEN_REQUIRED.to_owned()));
    };
    let user = policy.validate_existing()?;
    service.register(Some(user.user_id), user.session_id, &token)?;
    Ok(Response::new(204))
}
