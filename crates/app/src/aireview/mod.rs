//! AI 리뷰(원본 `aireview` 패키지): 프롬프트 버전, OpenAI 호출, 게시물 AI 리뷰.
//!
//! - [`domain`]: 프롬프트 유형·옵션·초안·버전, 모델 출력과 서비스 결과.
//! - [`generator`]: 리뷰 생성 포트와 실패 분류. [`openai`]가 Responses API 어댑터다.
//! - [`prompt_repository`]: 프롬프트 버전 저장소.

pub mod domain;
pub mod generator;
pub mod openai;
pub mod prompt_repository;

use std::sync::Arc;

pub use generator::AiReviewGenerator;

/// 서버 조립이 넘겨 주는 AI 리뷰 구성 요소.
#[derive(Clone)]
pub struct AiReviewWiring {
    pub generator: Arc<dyn AiReviewGenerator>,
}
