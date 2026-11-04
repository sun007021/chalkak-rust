//! 작성자가 본인 게시물의 AI 리뷰를 요청하고 조회한다(원본 `PostAiReviewService`). 요청은 대기열에 한 행을 넣을 뿐이며 OpenAI는
//! 호출하지 않는다. 작성자가 아니거나 공개 조건을 만족하지 않는 게시물은 리뷰 존재 여부를 숨기려고 게시물이 없는 것과 같게 404로
//! 응답한다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::{Logger, fields};
use chalkak_db::{Db, Pool};
use jiff::Timestamp;
use uuid::Uuid;

use super::AiReviewGenerator;
use super::domain::{AiPromptType, AiPromptVersion, AiReviewResult};
use super::post_query::find_visible_post;
use super::prompt_repository::PromptVersionRepository;
use super::review::{AiReview, Status};
use super::review_repository::AiReviewRepository;
use crate::post::support::in_transaction;
use crate::web::ApiError;

pub const AI_REVIEW_LOGGER: &str = "chalkak.aireview";
/// `LogFields.TYPE_AI_REVIEW`
pub const TYPE_AI_REVIEW: &str = "ai_review";

/// 앱에 노출하는 상태. 대기와 처리 중은 앱이 구분할 필요가 없어 하나로 합친다. `None`은 아직 요청하지 않은 상태다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApiStatus {
    None,
    Processing,
    Completed,
    Failed,
}

impl ApiStatus {
    pub fn from_status(status: Status) -> ApiStatus {
        match status {
            Status::Completed => ApiStatus::Completed,
            Status::Failed => ApiStatus::Failed,
            Status::Queued | Status::Processing => ApiStatus::Processing,
        }
    }
}

/// 요청·조회 결과. 결과는 완료일 때만 있다. 요청한 적이 없으면 시각과 결과 없이 요청 전 상태(`None`)다.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewResult {
    pub status: ApiStatus,
    pub requested_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub result: Option<AiReviewResult>,
    pub can_request_again: bool,
}

impl ReviewResult {
    fn from_review(review: &AiReview) -> ReviewResult {
        ReviewResult {
            status: ApiStatus::from_status(review.status),
            requested_at: Some(review.requested_at),
            completed_at: review.completed_at,
            result: review.result.clone(),
            can_request_again: review.can_request_again(),
        }
    }

    fn none() -> ReviewResult {
        ReviewResult {
            status: ApiStatus::None,
            requested_at: None,
            completed_at: None,
            result: None,
            can_request_again: false,
        }
    }

    pub fn is_in_progress(&self) -> bool {
        self.status == ApiStatus::Processing
    }
}

#[derive(Clone)]
pub struct PostAiReviewService {
    pool: Pool,
    reviews: AiReviewRepository,
    prompts: PromptVersionRepository,
    generator: Arc<dyn AiReviewGenerator>,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
}

impl PostAiReviewService {
    pub fn new(
        pool: Pool,
        generator: Arc<dyn AiReviewGenerator>,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
    ) -> Self {
        PostAiReviewService {
            pool,
            reviews: AiReviewRepository,
            prompts: PromptVersionRepository,
            generator,
            clock,
            logger,
        }
    }

    /// `request`
    pub fn request(&self, user_id: Uuid, post_id: Uuid) -> Result<ReviewResult, ApiError> {
        in_transaction(&self.pool, |tx| {
            validate_authored_visible_post(tx, user_id, post_id)?;
            // 동시 재요청이 한 번만 반영되도록 트랜잭션의 첫 리뷰 읽기를 잠금 읽기로 한다.
            let Some(mut review) = self.reviews.find_by_post_id_for_update(tx, post_id)? else {
                return self.create_review(tx, user_id, post_id);
            };
            if !review.is_failed() {
                return Ok(ReviewResult::from_review(&review));
            }
            self.request_again(tx, &mut review)
        })
    }

    /// `get`
    pub fn get(&self, user_id: Uuid, post_id: Uuid) -> Result<ReviewResult, ApiError> {
        let mut conn = self.pool.get()?;
        validate_authored_visible_post(&mut conn, user_id, post_id)?;
        Ok(self
            .reviews
            .find_by_post_id(&mut conn, post_id)?
            .map(|review| ReviewResult::from_review(&review))
            .unwrap_or_else(ReviewResult::none))
    }

    fn create_review(
        &self,
        tx: &mut impl Db,
        user_id: Uuid,
        post_id: Uuid,
    ) -> Result<ReviewResult, ApiError> {
        let version = self.usable_latest_version(tx)?;
        let requested = AiReview::request(
            post_id,
            user_id,
            version.id.ok_or_else(missing_version_id)?,
            chalkak_core::request_id::resolve_current(),
            self.clock.now(),
        );
        let inserted = self.reviews.insert_if_absent(tx, &requested)?;
        let review = self
            .reviews
            .find_by_post_id(tx, post_id)?
            .ok_or_else(|| ApiError::Internal("AI 리뷰를 찾을 수 없습니다.".to_owned()))?;
        if inserted == 1 {
            self.log_requested(&review, &version, true);
        }
        Ok(ReviewResult::from_review(&review))
    }

    fn request_again(
        &self,
        tx: &mut impl Db,
        review: &mut AiReview,
    ) -> Result<ReviewResult, ApiError> {
        review
            .validate_requests_left()
            .map_err(|e| e.into_api_error())?;
        let version = self.usable_latest_version(tx)?;
        review
            .request_again(version.id.ok_or_else(missing_version_id)?, self.clock.now())
            .map_err(|e| e.into_api_error())?;
        self.reviews.save(tx, review)?;
        self.log_requested(review, &version, false);
        Ok(ReviewResult::from_review(review))
    }

    /// 가장 큰 버전만 쓴다. 현재 서버가 해석할 수 없는 버전이면 이전 버전으로 내려가지 않고 기능이 꺼진 것으로 본다.
    fn usable_latest_version(&self, tx: &mut impl Db) -> Result<AiPromptVersion, ApiError> {
        self.prompts
            .find_latest_by_type(tx, AiPromptType::PhotoReview)?
            .filter(|version| {
                version.is_runnable_with(|model| self.generator.supports_model(model))
            })
            .ok_or_else(|| {
                ApiError::App(AppError::business(
                    ErrorCode::BUSINESS_ERROR,
                    "지금은 AI 리뷰를 요청할 수 없습니다.",
                ))
            })
    }

    /// 이벤트 이름과 필드 키는 CloudWatch 메트릭 필터가 매칭한다. 바꾸면 운영 문서와 필터를 함께 고친다.
    fn log_requested(&self, review: &AiReview, version: &AiPromptVersion, created: bool) {
        self.logger
            .info(AI_REVIEW_LOGGER)
            .field(fields::TYPE, TYPE_AI_REVIEW)
            .field(fields::EVENT, "review_requested")
            .field("aiReviewId", uuid_text(review.id))
            .field(fields::POST_ID, review.post_id.hyphenated().to_string())
            .field(fields::USER_ID, review.user_id.hyphenated().to_string())
            .field("promptVersion", i64::from(version.version))
            .field("model", version.model.as_str())
            .field("requestCount", i64::from(review.request_count))
            .field("created", created)
            .log("AI 리뷰 요청");
    }
}

fn uuid_text(id: Option<Uuid>) -> String {
    id.map(|id| id.hyphenated().to_string()).unwrap_or_default()
}

fn missing_version_id() -> ApiError {
    ApiError::Internal("프롬프트 버전 식별자가 없습니다.".to_owned())
}

/// 작성자가 아니거나 공개 조건을 만족하지 않는 게시물은 게시물이 없는 것과 같게 404다.
fn validate_authored_visible_post(
    db: &mut impl Db,
    user_id: Uuid,
    post_id: Uuid,
) -> Result<(), ApiError> {
    match find_visible_post(db, post_id)? {
        Some(post) if post.author_id == user_id => Ok(()),
        _ => Err(ApiError::App(AppError::not_found(
            ErrorCode::BUSINESS_ERROR,
            "게시물을 찾을 수 없습니다.",
        ))),
    }
}
