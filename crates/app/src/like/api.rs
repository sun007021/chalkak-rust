//! 원본 `PostLikeController`(`PUT·DELETE /api/v1/posts/{postId}/likes`)와 `PostLikeResponse`.

use chalkak_http::{Method, Request};
use serde::Serialize;

use super::service::{PostLikeResult, PostLikeService};
use crate::AppContext;
use crate::post::request::parse_canonical_uuid;
use crate::post::support::UserGuard;
use crate::web::{ApiResult, Routes, ok_json};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostLikeResponse {
    post_id: String,
    like_count: i64,
    is_liked: bool,
}

impl From<PostLikeResult> for PostLikeResponse {
    fn from(r: PostLikeResult) -> Self {
        PostLikeResponse {
            post_id: r.post_id.to_string(),
            like_count: r.like_count,
            is_liked: r.is_liked,
        }
    }
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let guard = UserGuard::new(ctx.pool.clone());
    let service = PostLikeService::new(ctx.pool.clone());
    let (like_guard, like_service) = (guard.clone(), service.clone());
    routes
        .route(Method::Put, "/api/v1/posts/{postId}/likes", move |req| {
            like_post(&like_guard, &like_service, req)
        })
        .route(Method::Delete, "/api/v1/posts/{postId}/likes", move |req| {
            unlike_post(&guard, &service, req)
        })
}

/// `@RequiresUsableUser`: 정지 회원은 좋아요를 누를 수 없다.
fn like_post(guard: &UserGuard, service: &PostLikeService, req: &Request) -> ApiResult {
    let user = guard.validate_usable()?;
    let post_id = parse_canonical_uuid(req.param("postId").unwrap_or_default())?;
    let result = service.like_post(post_id, user.user_id)?;
    Ok(ok_json(&PostLikeResponse::from(result)))
}

/// `@RequiresExistingUser`: 정지 회원도 좋아요를 취소할 수 있다.
fn unlike_post(guard: &UserGuard, service: &PostLikeService, req: &Request) -> ApiResult {
    let user = guard.validate_existing()?;
    let post_id = parse_canonical_uuid(req.param("postId").unwrap_or_default())?;
    let result = service.unlike_post(post_id, user.user_id)?;
    Ok(ok_json(&PostLikeResponse::from(result)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    // PostLikeControllerTest: 응답 본문은 postId, likeCount, isLiked 순서다.
    #[test]
    fn like_response_serializes_in_record_order() {
        let post_id = Uuid::parse_str("0198f6c1-62ba-7d30-8b12-0f733b6570d4").unwrap();
        let response = ok_json(&PostLikeResponse::from(PostLikeResult {
            post_id,
            like_count: 3,
            is_liked: true,
        }));
        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8(response.body).unwrap(),
            r#"{"postId":"0198f6c1-62ba-7d30-8b12-0f733b6570d4","likeCount":3,"isLiked":true}"#
        );
    }
}
