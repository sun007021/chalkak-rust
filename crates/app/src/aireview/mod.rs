//! AI 리뷰(원본 `aireview` 패키지): 프롬프트 버전, OpenAI 호출, 게시물 AI 리뷰.
//!
//! - [`domain`]: 프롬프트 유형·옵션·초안·버전, 모델 출력과 서비스 결과.
//! - [`generator`]: 리뷰 생성 포트와 실패 분류. [`openai`]가 Responses API 어댑터다.
//! - [`prompt_repository`]: 프롬프트 버전 저장소. [`post_query`]: 리뷰 대상(공개) 게시물 조회.

pub mod domain;
pub mod generator;
pub mod openai;
pub mod post_query;
pub mod prompt_repository;

use std::sync::Arc;

pub use generator::AiReviewGenerator;

/// 서버 조립이 넘겨 주는 AI 리뷰 구성 요소.
#[derive(Clone)]
pub struct AiReviewWiring {
    pub generator: Arc<dyn AiReviewGenerator>,
}

/// 시험용 대역. 통합 시험이 `AppContext`를 조립할 때 쓴다.
#[doc(hidden)]
pub mod testing {
    use std::collections::VecDeque;
    use std::sync::{Mutex, PoisonError};

    use super::generator::{
        AiReviewGeneration, AiReviewGenerationError, AiReviewGenerationRequest, AiReviewGenerator,
        FailureType,
    };

    /// 정해 둔 결과를 차례로 돌려주고 받은 요청을 기록한다. 결과가 떨어지면 `BAD_REQUEST`로 실패한다.
    pub struct StubGenerator {
        models: Vec<String>,
        outcomes: Mutex<VecDeque<Result<AiReviewGeneration, AiReviewGenerationError>>>,
        requests: Mutex<Vec<AiReviewGenerationRequest>>,
    }

    impl StubGenerator {
        pub fn new(
            models: &[&str],
            outcomes: Vec<Result<AiReviewGeneration, AiReviewGenerationError>>,
        ) -> StubGenerator {
            StubGenerator {
                models: models.iter().map(|m| (*m).to_owned()).collect(),
                outcomes: Mutex::new(outcomes.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// 호출되면 실패하는 기본 대역(허용 모델은 운영 기본값과 같다).
        pub fn idle() -> StubGenerator {
            StubGenerator::new(&["gpt-6.1-sol", "gpt-6-luna"], Vec::new())
        }

        pub fn requests(&self) -> Vec<AiReviewGenerationRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl AiReviewGenerator for StubGenerator {
        fn generate(
            &self,
            request: &AiReviewGenerationRequest,
        ) -> Result<AiReviewGeneration, AiReviewGenerationError> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            self.outcomes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| Err(AiReviewGenerationError::of(FailureType::BadRequest)))
        }

        fn supports_model(&self, model: &str) -> bool {
            self.models.iter().any(|m| m == model)
        }
    }
}
