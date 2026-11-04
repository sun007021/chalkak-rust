//! 원본 `PostAiReviewController`(`POST·GET /api/v1/posts/{postId}/ai-review`)와 `PostAiReviewResponse`.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::Pool;
use chalkak_http::{Method, Request};
use jiff::Timestamp;
use serde::Serialize;

use super::AiReviewGenerator;
use super::domain::AiReviewResult;
use super::post_service::{ApiStatus, PostAiReviewService, ReviewResult};
use crate::AppContext;
use crate::post::request::parse_canonical_uuid;
use crate::post::support::UserGuard;
use crate::web::json::serialize_instant;
use crate::web::{ApiResult, Routes, json_response, ok_json};

/// 필드 선언 순서가 JSON 출력 순서다. `requestedAt`, `completedAt`, `result`는 값이 없으면 null로 나간다.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostAiReviewResponse {
    status: ApiStatus,
    #[serde(serialize_with = "serialize_optional_instant")]
    requested_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_optional_instant")]
    completed_at: Option<Timestamp>,
    result: Option<AiReviewResult>,
    can_request_again: bool,
}

fn serialize_optional_instant<S: serde::Serializer>(
    value: &Option<Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(at) => serialize_instant(at, serializer),
        None => serializer.serialize_none(),
    }
}

impl From<&ReviewResult> for PostAiReviewResponse {
    fn from(r: &ReviewResult) -> Self {
        PostAiReviewResponse {
            status: r.status,
            requested_at: r.requested_at,
            completed_at: r.completed_at,
            result: r.result.clone(),
            can_request_again: r.can_request_again,
        }
    }
}

pub fn register(routes: Routes, ctx: &AppContext, generator: Arc<dyn AiReviewGenerator>) -> Routes {
    register_parts(
        routes,
        ctx.pool.clone(),
        Arc::clone(&ctx.clock),
        Arc::clone(&ctx.logger),
        generator,
    )
}

/// 필요한 조각만 받아 등록한다(시험이 쓴다).
pub fn register_parts(
    routes: Routes,
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
    generator: Arc<dyn AiReviewGenerator>,
) -> Routes {
    let guard = UserGuard::new(pool.clone());
    let service = PostAiReviewService::new(pool, generator, clock, logger);
    let (post_guard, post_service) = (guard.clone(), service.clone());
    routes
        .route(
            Method::Post,
            "/api/v1/posts/{postId}/ai-review",
            move |req| request_ai_review(&post_guard, &post_service, req),
        )
        .route(
            Method::Get,
            "/api/v1/posts/{postId}/ai-review",
            move |req| get_ai_review(&guard, &service, req),
        )
}

/// `@RequiresUsableUser`. 진행 중이면 202, 그 밖에는 200.
fn request_ai_review(guard: &UserGuard, service: &PostAiReviewService, req: &Request) -> ApiResult {
    let user = guard.validate_usable()?;
    let post_id = parse_canonical_uuid(req.param("postId").unwrap_or_default())?;
    let result = service.request(user.user_id, post_id)?;
    let response = PostAiReviewResponse::from(&result);
    if result.is_in_progress() {
        return Ok(json_response(202, crate::web::json::to_json(&response)));
    }
    Ok(ok_json(&response))
}

/// `@RequiresExistingUser`
fn get_ai_review(guard: &UserGuard, service: &PostAiReviewService, req: &Request) -> ApiResult {
    let user = guard.validate_existing()?;
    let post_id = parse_canonical_uuid(req.param("postId").unwrap_or_default())?;
    let result = service.get(user.user_id, post_id)?;
    Ok(ok_json(&PostAiReviewResponse::from(&result)))
}
