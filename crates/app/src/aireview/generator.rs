//! 프롬프트 초안으로 사진 리뷰를 생성하는 포트(원본 `AiReviewGenerator`, `AiReviewGeneration`,
//! `AiReviewGenerationException`, `AiReviewGenerationRequest`, `AiReviewGenerationRequestFactory`).

use std::fmt;
use std::time::Duration;

use super::domain::{AiPromptDraft, AiReviewResult, PhotoReviewOutput, is_blank};
use crate::photo::url::{ImageUrlProvider, InvalidStorageKey};

/// 생성 요청. 이미지 주소와 주제 제목이 비어 있으면 만들 수 없다(원본은 `IllegalArgumentException`).
#[derive(Clone, Debug)]
pub struct AiReviewGenerationRequest {
    pub draft: AiPromptDraft,
    pub image_url: String,
    pub topic_title: String,
}

impl AiReviewGenerationRequest {
    pub fn new(
        draft: AiPromptDraft,
        image_url: &str,
        topic_title: &str,
    ) -> Result<AiReviewGenerationRequest, String> {
        if is_blank(image_url) {
            return Err("이미지 주소가 필요합니다.".to_owned());
        }
        if is_blank(topic_title) {
            return Err("주제 제목이 필요합니다.".to_owned());
        }
        Ok(AiReviewGenerationRequest {
            draft,
            image_url: image_url.to_owned(),
            topic_title: topic_title.to_owned(),
        })
    }

    /// 프롬프트 초안과 게시물(원본 사진 키·주제 제목)로 조립한다. 관리자 미리보기와 워커가 같은 방식으로 쓴다.
    pub fn for_post(
        urls: &dyn ImageUrlProvider,
        draft: AiPromptDraft,
        original_storage_key: &str,
        topic_title: &str,
    ) -> Result<AiReviewGenerationRequest, String> {
        let url = urls
            .get_url(Some(original_storage_key))
            .map_err(|InvalidStorageKey(message)| message)?
            .unwrap_or_default();
        AiReviewGenerationRequest::new(draft, &url, topic_title)
    }
}

/// OpenAI가 보고한 토큰 사용량. `None`이면 주지 않은 값이고 0은 실제로 0이라고 보고한 값이다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<i32>,
    pub cached_input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub reasoning_tokens: Option<i32>,
}

/// 응답 본문을 받은 호출의 기록. 성공과 실패 모두 같은 형태로 남긴다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub usage: Usage,
    pub latency: Duration,
    pub response_id: Option<String>,
}

/// 생성 성공 결과. `raw_output`은 모델이 돌려준 output_text 그대로다.
#[derive(Clone, Debug, PartialEq)]
pub struct AiReviewGeneration {
    pub result: AiReviewResult,
    pub output: PhotoReviewOutput,
    pub raw_output: String,
    pub attempt: Attempt,
}

/// 생성 실패 유형. 같은 요청을 다시 보내면 성공할 수 있는 실패만 재시도 대상이다. 결제·사용 한도 소진은 기다려도 풀리지 않아
/// 재시도하지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureType {
    Timeout,
    RateLimited,
    QuotaExceeded,
    UpstreamError,
    BadRequest,
    Refused,
    Incomplete,
    InvalidOutput,
}

impl FailureType {
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureType::Timeout => "TIMEOUT",
            FailureType::RateLimited => "RATE_LIMITED",
            FailureType::QuotaExceeded => "QUOTA_EXCEEDED",
            FailureType::UpstreamError => "UPSTREAM_ERROR",
            FailureType::BadRequest => "BAD_REQUEST",
            FailureType::Refused => "REFUSED",
            FailureType::Incomplete => "INCOMPLETE",
            FailureType::InvalidOutput => "INVALID_OUTPUT",
        }
    }

    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            FailureType::Timeout | FailureType::RateLimited | FailureType::UpstreamError
        )
    }
}

/// AI 리뷰 생성 실패. 메시지에는 실패 유형만 담아 API 키·프롬프트·응답 본문이 새지 않게 한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiReviewGenerationError {
    pub failure_type: FailureType,
    /// 응답 본문을 받은 뒤 실패했을 때만 있다. HTTP 오류나 시간 초과는 없다.
    pub attempt: Option<Attempt>,
    /// 외부 서비스가 알려 준 최소 대기 시간.
    pub retry_after: Option<Duration>,
}

impl AiReviewGenerationError {
    pub fn of(failure_type: FailureType) -> Self {
        AiReviewGenerationError {
            failure_type,
            attempt: None,
            retry_after: None,
        }
    }

    pub fn with_attempt(failure_type: FailureType, attempt: Attempt) -> Self {
        AiReviewGenerationError {
            failure_type,
            attempt: Some(attempt),
            retry_after: None,
        }
    }

    pub fn with_retry_after(failure_type: FailureType, retry_after: Duration) -> Self {
        AiReviewGenerationError {
            failure_type,
            attempt: None,
            retry_after: Some(retry_after),
        }
    }
}

impl fmt::Display for AiReviewGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AI 리뷰 생성에 실패했습니다. 유형: {}",
            self.failure_type.as_str()
        )
    }
}

impl std::error::Error for AiReviewGenerationError {}

/// 프롬프트 초안으로 사진 리뷰를 생성하는 포트.
pub trait AiReviewGenerator: Send + Sync {
    fn generate(
        &self,
        request: &AiReviewGenerationRequest,
    ) -> Result<AiReviewGeneration, AiReviewGenerationError>;

    /// 이 생성기가 호출을 허용한 모델인지. 허용되지 않은 모델은 호출 전에 거절하는 데 쓴다.
    fn supports_model(&self, model: &str) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aireview::domain::{AiPromptParameters, AiPromptType};

    fn draft() -> AiPromptDraft {
        AiPromptDraft::new(
            AiPromptType::PhotoReview,
            "s",
            "{{topicTitle}}",
            "m",
            AiPromptParameters::new(None, None, None, None).unwrap(),
        )
        .unwrap()
    }

    // AiReviewGenerationRequestTest
    #[test]
    fn 이미지_주소와_주제_제목이_비면_만들_수_없다() {
        assert!(AiReviewGenerationRequest::new(draft(), "https://x/y.jpg", "노을").is_ok());
        assert_eq!(
            AiReviewGenerationRequest::new(draft(), " ", "노을").unwrap_err(),
            "이미지 주소가 필요합니다."
        );
        assert_eq!(
            AiReviewGenerationRequest::new(draft(), "https://x/y.jpg", "").unwrap_err(),
            "주제 제목이 필요합니다."
        );
    }

    // AiReviewGenerationExceptionTest
    #[test]
    fn 재시도할_수_있는_실패는_시간_초과_한도_상위_오류뿐이다() {
        let retryable: Vec<&str> = [
            FailureType::Timeout,
            FailureType::RateLimited,
            FailureType::QuotaExceeded,
            FailureType::UpstreamError,
            FailureType::BadRequest,
            FailureType::Refused,
            FailureType::Incomplete,
            FailureType::InvalidOutput,
        ]
        .into_iter()
        .filter(|t| t.is_retryable())
        .map(FailureType::as_str)
        .collect();
        assert_eq!(retryable, ["TIMEOUT", "RATE_LIMITED", "UPSTREAM_ERROR"]);
        assert_eq!(
            AiReviewGenerationError::of(FailureType::Refused).to_string(),
            "AI 리뷰 생성에 실패했습니다. 유형: REFUSED"
        );
    }
}
