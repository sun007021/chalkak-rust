//! 게시물 도메인. 원본 `com.chalkak.backend.post` 패키지.
//!
//! - [`api`]: `/api/v1/posts` 컨트롤러와 요청·응답 DTO. [`callback`]: `/internal/v1/post-image-processing`.
//! - [`command`]·[`query`]: 상태를 바꾸는 흐름과 읽기 전용 조회. [`repository`]·[`upload_repository`]: SQL.
//! - [`storage`]: 스토리지 키 규칙과 presigned URL 발급(S3 호출은 [`storage::ObjectStore`] 뒤).
//! - [`listener`]: 검수 대기 알림 연결점. 커밋 뒤에 호출하며 구현은 서버 조립이 주입한다.

pub mod api;
pub mod callback;
pub mod command;
pub mod domain;
pub mod listener;
pub mod policy;
pub mod query;
pub mod repository;
pub mod request;
pub mod settings;
pub mod storage;
pub mod support;
pub mod upload_repository;

use std::sync::Arc;

use crate::AppContext;
use crate::photo::url::CloudFrontUrlProvider;
pub use callback::ProcessingCallbackAuthenticator;
pub use command::PostCommandService;
pub use listener::{NoopPostModerationPendingListener, PostModerationPendingListener};
pub use policy::PostProcessingPolicy;
pub use query::PostQueryService;
pub use settings::{ImageSettings, PostSettings};
pub use storage::{ObjectStore, PostImageStorage};

/// 게시물 흐름이 서버 조립에서 받는 것: 설정, S3 호출, 검수 대기 구독자.
#[derive(Clone)]
pub struct PostWiring {
    pub settings: PostSettings,
    pub object_store: Arc<dyn ObjectStore>,
    pub listener: Arc<dyn PostModerationPendingListener>,
}

impl PostWiring {
    pub fn new(
        settings: PostSettings,
        object_store: Arc<dyn ObjectStore>,
        listener: Arc<dyn PostModerationPendingListener>,
    ) -> Self {
        PostWiring {
            settings,
            object_store,
            listener,
        }
    }

    fn policy(&self) -> PostProcessingPolicy {
        PostProcessingPolicy::new(self.settings.image_processing_timeout)
            .expect("설정 단계에서 0보다 큰 기간임을 확인했다")
    }

    pub fn command_service(&self, ctx: &AppContext) -> PostCommandService {
        PostCommandService::new(
            ctx.pool.clone(),
            ctx.clock.clone(),
            ctx.logger.clone(),
            PostImageStorage::new(self.object_store.clone(), self.settings.image.clone()),
            self.policy(),
            self.listener.clone(),
        )
    }

    pub fn query_service(&self, ctx: &AppContext) -> PostQueryService {
        PostQueryService::new(
            ctx.pool.clone(),
            ctx.clock.clone(),
            CloudFrontUrlProvider::new(
                &self.settings.cloudfront_base_url,
                &self.settings.cloudfront_origin_path,
            ),
            self.policy(),
        )
    }

    pub fn authenticator(&self, ctx: &AppContext) -> ProcessingCallbackAuthenticator {
        ProcessingCallbackAuthenticator::new(
            &self.settings.callback_secret,
            &self.settings.image.environment,
            ctx.clock.clone(),
        )
        .expect("설정 단계에서 비밀키와 환경 이름을 확인했다")
    }
}
