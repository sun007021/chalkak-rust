//! 원본 `FeedbackController`(`POST /api/v1/feedbacks`)와 요청·응답 DTO.

use chalkak_http::Request;
use serde::Serialize;

use super::domain::is_within_max_length;
use super::service::{FeedbackService, FeedbackSubmissionResult};
use crate::AppContext;
use crate::auth::UsableUserPolicy;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, binding, created_json};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackSubmissionResponse {
    feedback_id: String,
    #[serde(serialize_with = "serialize_instant")]
    created_at: jiff::Timestamp,
}

impl From<FeedbackSubmissionResult> for FeedbackSubmissionResponse {
    fn from(r: FeedbackSubmissionResult) -> Self {
        FeedbackSubmissionResponse {
            feedback_id: r.feedback_id.to_string(),
            created_at: r.created_at,
        }
    }
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let service = FeedbackService::new(ctx.pool.clone(), ctx.clock.clone());
    let policy = UsableUserPolicy::new(ctx.pool.clone());
    routes.post("/api/v1/feedbacks", move |req| {
        submit_feedback(&service, &policy, req)
    })
}

/// `FeedbackSubmissionRequest`의 `@NotBlank`와 `@AssertTrue`.
fn validate_content(content: Option<&str>) -> Result<(), ApiError> {
    match content {
        None => Err(ApiError::Validation("피드백 내용이 필요합니다.".to_owned())),
        Some(c) if c.chars().all(crate::web::binding::is_java_whitespace) => {
            Err(ApiError::Validation("피드백 내용이 필요합니다.".to_owned()))
        }
        Some(c) if !is_within_max_length(c) => Err(ApiError::Validation(
            "피드백 내용은 1000자 이하여야 합니다.".to_owned(),
        )),
        Some(_) => Ok(()),
    }
}

fn submit_feedback(
    service: &FeedbackService,
    policy: &UsableUserPolicy,
    req: &Request,
) -> ApiResult {
    // 인자 해석(본문 읽기와 검증) -> 메서드 보안(@RequiresExistingUser) -> 서비스 순서다.
    let body = binding::read_json_object(req)?;
    let content = body.string("content")?;
    validate_content(content.as_deref())?;
    let user = policy.validate_existing()?;
    let result = service.submit(user.user_id, content.as_deref())?;
    Ok(created_json(&FeedbackSubmissionResponse::from(result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // FeedbackSubmissionRequest: @NotBlank, @AssertTrue(isValidContentLength)
    #[test]
    fn 내용_검증은_원본_메시지를_쓴다() {
        let msg = |r: Result<(), ApiError>| r.unwrap_err().message();
        assert_eq!(msg(validate_content(None)), "피드백 내용이 필요합니다.");
        assert_eq!(
            msg(validate_content(Some(" \n"))),
            "피드백 내용이 필요합니다."
        );
        assert_eq!(
            msg(validate_content(Some(&"a".repeat(1001)))),
            "피드백 내용은 1000자 이하여야 합니다."
        );
        assert!(validate_content(Some(&"a".repeat(1000))).is_ok());
    }
}
