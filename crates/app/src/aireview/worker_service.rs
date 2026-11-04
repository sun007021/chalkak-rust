//! 대기열의 AI 리뷰 한 건을 가져가 처리한다(원본 `AiReviewWorkerService`, `AiReviewWorkerLog`, `AbandonReason`). 행을 잠가 가져가는 짧은
//! 트랜잭션과 결과를 반영하는 짧은 트랜잭션 사이에서 OpenAI를 호출하므로 호출하는 동안 DB 연결과 행 잠금을 잡지 않는다.
//!
//! 가져간 행은 임대 시간 안에 끝내야 한다. 워커가 죽거나 종료되면 행은 처리 중으로 남고 임대가 끝난 뒤 다시 가져간다. 그래서
//! 종료·DB 오류처럼 호출 결과를 알 수 없는 상황에서는 실패를 쓰지 않고 행을 그대로 둔다. 반복해서 워커를 멈추게 하는 행은 가져간
//! 횟수 상한에서 실패로 끝난다. 일시적인 생성 실패는 행을 대기로 되돌리고 백오프 뒤에 다시 가져간다. 임대 회수도 같은 회차의 가져간
//! 횟수에 포함하므로 한 번의 사용자 요청이 처리되는 시도는 상한을 넘지 않는다.
//!
//! 로그 이벤트 이름과 필드 키는 CloudWatch 메트릭 필터와 경보가 매칭한다. 바꾸면 경보가 조용히 세지 않게 되므로 운영 문서와 필터를
//! 함께 고친다.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use chalkak_core::Clock;
use chalkak_core::log::{EventBuilder, Logger, fields};
use chalkak_db::{Db, Pool};
use jiff::Timestamp;
use uuid::Uuid;

use super::backoff::{AiReviewRetryBackoff, RetryDelay};
use super::domain::AiPromptVersion;
use super::generator::{
    AiReviewGeneration, AiReviewGenerationError, AiReviewGenerationRequest, AiReviewGenerator,
    Attempt,
};
use super::post_query::find_visible_post;
use super::post_service::{AI_REVIEW_LOGGER, TYPE_AI_REVIEW};
use super::prompt_repository::PromptVersionRepository;
use super::review::{AiReview, FailureReason, Invocation, Status};
use super::review_repository::AiReviewRepository;
use crate::notification::ai_review::{AiReviewOutcome, create_for_ai_review};
use crate::photo::url::ImageUrlProvider;
use crate::post::support::in_transaction;
use crate::web::ApiError;

/// 처리를 끝내지 못한 채 행을 처리 중으로 남기는 사유. 로그의 `reason` 값으로만 쓰며 DB에는 저장하지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonReason {
    /// 종료로 처리를 멈춘 경우
    Shutdown,
    /// 처리 도중 예상 못 한 예외로 멈춘 경우
    UnexpectedWhileProcessing,
    /// 결과를 반영하는 DB 단계가 실패한 경우
    DbError,
    /// 호출 전 준비 단계(게시물 조회·요청 생성)가 실패한 경우
    PrepareFailed,
}

impl AbandonReason {
    pub const fn name(self) -> &'static str {
        match self {
            AbandonReason::Shutdown => "SHUTDOWN",
            AbandonReason::UnexpectedWhileProcessing => "UNEXPECTED_WHILE_PROCESSING",
            AbandonReason::DbError => "DB_ERROR",
            AbandonReason::PrepareFailed => "PREPARE_FAILED",
        }
    }
}

/// 워커가 가져간 행의 식별 정보와 요청 시점에 고정된 프롬프트 버전. 시도 번호는 반영할 때 늦은 워커를 가려내는 기준이고, 시도
/// 번호가 같으면 회차 시도 횟수도 같다.
#[derive(Clone, Debug)]
pub struct Claim {
    pub review_id: Uuid,
    pub attempt: i32,
    pub round_attempt: i32,
    pub post_id: Uuid,
    pub user_id: Uuid,
    pub version: AiPromptVersion,
}

impl Claim {
    fn of(review: &AiReview, version: &AiPromptVersion) -> Claim {
        Claim {
            review_id: review.id.unwrap_or_default(),
            attempt: review.attempt,
            round_attempt: review.round_attempt,
            post_id: review.post_id,
            user_id: review.user_id,
            version: version.clone(),
        }
    }
}

/// 실패 한 건을 반영하는 데 필요한 값. 워커 자체 사유와 예상 못 한 예외는 재시도하지 않고 호출 결과도 없다.
struct Failure {
    error_code: String,
    retryable: bool,
    invocation: Invocation,
    retry_after: Option<Duration>,
}

impl Failure {
    fn internal(reason: FailureReason) -> Failure {
        Failure {
            error_code: reason.name().to_owned(),
            retryable: false,
            invocation: Invocation::default(),
            retry_after: None,
        }
    }
}

/// 가져가기 트랜잭션 한 번의 결과. 정상 행을 가져갔으면 `claim`이 있고, 이 트랜잭션에서 소진으로 끝낸 행은 `exhausted`에 담는다.
struct ClaimRound {
    claim: Option<Claim>,
    exhausted: Vec<(AiReview, Claim)>,
}

pub struct AiReviewWorkerService {
    pool: Pool,
    reviews: AiReviewRepository,
    prompts: PromptVersionRepository,
    generator: Arc<dyn AiReviewGenerator>,
    image_urls: Arc<dyn ImageUrlProvider>,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
    lease_duration: Duration,
    max_claims: i32,
    backoff: AiReviewRetryBackoff,
}

impl AiReviewWorkerService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Pool,
        generator: Arc<dyn AiReviewGenerator>,
        image_urls: Arc<dyn ImageUrlProvider>,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
        lease_duration: Duration,
        max_claims: i32,
        backoff: AiReviewRetryBackoff,
    ) -> AiReviewWorkerService {
        AiReviewWorkerService {
            pool,
            reviews: AiReviewRepository,
            prompts: PromptVersionRepository,
            generator,
            image_urls,
            clock,
            logger,
            lease_duration,
            max_claims,
            backoff,
        }
    }

    /// 가장 오래 기다린 처리 대상을 하나 가져간다. 가져간 횟수 상한을 넘은 행은 처리하지 않고 실패로 끝낸 뒤 다음 행을 찾는다.
    ///
    /// 소진 행을 정리한 가져가기 트랜잭션은 정상 행을 가져가지 않고 커밋한다. 소진 행의 종료 로그와 결과 알림은 그 커밋 뒤에 남기고,
    /// 새 트랜잭션에서 다시 가져간다. 그래서 정상 행의 임대는 알림 저장이 끝난 뒤 시작해 알림이 느려도 줄어들지 않는다. 알림 저장이
    /// 실패해도 행은 실패로 확정된 채 두고 다음 행을 가져간다. 가져가기 자체가 실패하면 오류를 그대로 돌려준다.
    pub fn claim_next(&self) -> Result<Option<Claim>, ApiError> {
        loop {
            let round = in_transaction(&self.pool, |tx| self.claim_round(tx))?;
            for (review, claim) in &round.exhausted {
                self.log_finished(review, claim);
                self.notify_result(review, claim);
            }
            if round.claim.is_some() || round.exhausted.is_empty() {
                return Ok(round.claim);
            }
        }
    }

    /// 한 트랜잭션 안에서 앞쪽 소진 행을 실패로 확정하고, 소진 행이 없을 때만 정상 행을 가져간다. 소진 행을 정리했다면 정상 행은
    /// 건드리지 않고 끝내 잠금을 풀며, 가져가는 일은 다음 트랜잭션에 맡긴다.
    fn claim_round(&self, tx: &mut impl Db) -> Result<ClaimRound, ApiError> {
        let now = self.clock.now();
        let mut exhausted: Vec<(AiReview, Claim)> = Vec::new();
        let mut found = self.reviews.find_claimable_for_update(tx, now)?;
        while let Some(mut review) = found {
            if !review.is_claim_limit_reached(self.max_claims) && !exhausted.is_empty() {
                return Ok(ClaimRound {
                    claim: None,
                    exhausted,
                });
            }
            let version = self
                .prompts
                .find_by_id(tx, review.prompt_version_id)?
                .ok_or_else(|| {
                    ApiError::Internal("프롬프트 버전을 찾을 수 없습니다.".to_owned())
                })?;
            let lease_end = now
                .checked_add(self.lease_duration)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let claimed = review
                .claim(now, lease_end, self.max_claims)
                .map_err(|e| e.into_api_error())?;
            // 다음 조회가 같은 행을 다시 돌려주지 않도록 바로 저장한다(JPA 자동 flush에 해당).
            self.reviews.save(tx, &review)?;
            if claimed {
                return Ok(ClaimRound {
                    claim: Some(Claim::of(&review, &version)),
                    exhausted,
                });
            }
            let claim = Claim::of(&review, &version);
            exhausted.push((review, claim));
            found = self.reviews.find_claimable_for_update(tx, now)?;
        }
        Ok(ClaimRound {
            claim: None,
            exhausted,
        })
    }

    /// 가져간 행 하나를 끝까지 처리한다. 예상 가능한 실패는 모두 행의 상태로 기록하며, 결과를 알 수 없는 경우에는 행을 처리 중으로
    /// 둔다. `stopping`은 애플리케이션 종료가 시작됐는지다.
    pub fn process(&self, claim: &Claim, stopping: &dyn Fn() -> bool) {
        let version = &claim.version;
        // 사진과 주제를 함께 읽어 오므로 트랜잭션이 없어도 OpenAI를 호출하는 동안 DB 연결을 잡지 않는다.
        let post = match self
            .pool
            .get()
            .map_err(ApiError::from)
            .and_then(|mut conn| {
                find_visible_post(&mut conn, claim.post_id).map_err(ApiError::from)
            }) {
            Ok(post) => post,
            Err(error) => return self.abandon(claim, AbandonReason::PrepareFailed, Some(&error)),
        };
        let Some(post) = post else {
            return self.apply_failure(claim, Failure::internal(FailureReason::PostUnavailable));
        };
        if !version.is_runnable_with(|model| self.generator.supports_model(model)) {
            return self.apply_failure(
                claim,
                Failure::internal(FailureReason::PromptVersionUnavailable),
            );
        }
        let request = version.to_draft().map_err(ApiError::App).and_then(|draft| {
            AiReviewGenerationRequest::for_post(
                self.image_urls.as_ref(),
                draft,
                &post.original_storage_key,
                &post.topic_title,
            )
            .map_err(ApiError::Internal)
        });
        let request = match request {
            Ok(request) => request,
            Err(error) => return self.abandon(claim, AbandonReason::PrepareFailed, Some(&error)),
        };

        let generation = catch_unwind(AssertUnwindSafe(|| self.generator.generate(&request)));
        match generation {
            Ok(Ok(generation)) => self.apply_completion(claim, generation),
            Ok(Err(error)) => {
                let failure = failure_of(&error);
                self.handle_generation_failure(claim, stopping, failure);
            }
            Err(_) => self.handle_generation_failure(
                claim,
                stopping,
                Failure::internal(FailureReason::UnexpectedError),
            ),
        }
    }

    /// 처리를 끝내지 못한 채 행을 처리 중으로 남긴다. 임대가 끝나면 다른 워커가 다시 가져간다.
    pub fn abandon(&self, claim: &Claim, reason: AbandonReason, error: Option<&ApiError>) {
        let mut event = self
            .claim_event(
                self.logger.warn(AI_REVIEW_LOGGER),
                "review_abandoned",
                claim,
            )
            .field("reason", reason.name());
        if let Some(error) = error {
            event = event.field(fields::ERROR_CODE, error.exception_name());
        }
        event.log("AI 리뷰 처리 중단, 임대 만료 뒤 다시 가져감");
    }

    /// 폴링 자체가 실패했다.
    pub fn log_poll_failed(&self, error: &ApiError) {
        self.logger
            .error(AI_REVIEW_LOGGER)
            .field(fields::TYPE, TYPE_AI_REVIEW)
            .field(fields::EVENT, "worker_poll_failed")
            .field(fields::ERROR_CODE, error.exception_name())
            .log("AI 리뷰 워커 폴링 실패");
    }

    fn handle_generation_failure(
        &self,
        claim: &Claim,
        stopping: &dyn Fn() -> bool,
        failure: Failure,
    ) {
        // 종료로 끊긴 호출은 일반 실패처럼 올라오므로 어떤 처리보다 먼저 확인한다.
        if stopping() {
            return self.abandon(claim, AbandonReason::Shutdown, None);
        }
        self.apply_failure(claim, failure);
    }

    fn apply_completion(&self, claim: &Claim, generation: AiReviewGeneration) {
        let invocation = invocation_of(Some(&generation.attempt));
        let now = self.clock.now();
        let attempt = claim.attempt;
        self.apply(claim, RetryDelay::NONE, move |review| {
            review.complete(
                attempt,
                generation.result,
                generation.raw_output,
                invocation,
                now,
            )
        });
    }

    fn apply_failure(&self, claim: &Claim, failure: Failure) {
        let now = self.clock.now();
        let retry_delay = self
            .backoff
            .delay_after(claim.round_attempt, failure.retry_after);
        let next_attempt_at = now.checked_add(retry_delay.delay).unwrap_or(Timestamp::MAX);
        let (attempt, max_claims) = (claim.attempt, self.max_claims);
        self.apply(claim, retry_delay, move |review| {
            review.fail_or_retry(
                attempt,
                &failure.error_code,
                failure.retryable,
                failure.invocation,
                next_attempt_at,
                max_claims,
                now,
            )
        });
    }

    /// `retry_delay`는 재시도 대기로 전환될 때 로그에 남길 대기 시간이다. 완료로 끝나는 전이에서는 쓰지 않는다.
    fn apply(
        &self,
        claim: &Claim,
        retry_delay: RetryDelay,
        transition: impl FnOnce(&mut AiReview) -> bool,
    ) {
        let applied = in_transaction(&self.pool, |tx| {
            let mut review = self
                .reviews
                .find_by_id_for_update(tx, claim.review_id)?
                .ok_or_else(|| ApiError::Internal("AI 리뷰를 찾을 수 없습니다.".to_owned()))?;
            if !transition(&mut review) {
                return Ok(None);
            }
            self.reviews.save(tx, &review)?;
            Ok(Some(review))
        });
        let applied = match applied {
            Ok(applied) => applied,
            Err(error) => return self.abandon(claim, AbandonReason::DbError, Some(&error)),
        };
        let Some(applied) = applied else {
            self.claim_event(
                self.logger.warn(AI_REVIEW_LOGGER),
                "review_stale_update",
                claim,
            )
            .log("AI 리뷰 늦은 반영 무시");
            return;
        };
        if applied.is_retry_scheduled() {
            self.log_retry_scheduled(&applied, claim, retry_delay);
            return;
        }
        self.log_finished(&applied, claim);
        self.notify_result(&applied, claim);
    }

    /// 리뷰 상태가 커밋된 뒤 별도 트랜잭션에서 결과 알림을 만든다. 재시도 대기처럼 끝나지 않았거나 알릴 필요 없는 상태는 건너뛴다.
    /// 알림 저장이 실패해도 확정한 리뷰 결과는 그대로 두고 로그만 남긴다.
    fn notify_result(&self, review: &AiReview, claim: &Claim) {
        if !review.is_result_notifiable() {
            return;
        }
        let outcome = if review.is_completed() {
            AiReviewOutcome::Completed
        } else if review.can_request_again() {
            AiReviewOutcome::FailedRetryable
        } else {
            AiReviewOutcome::FailedFinal
        };
        let occurred_at = review.completed_at.unwrap_or_else(|| self.clock.now());
        let result = in_transaction(&self.pool, |tx| {
            create_for_ai_review(
                tx,
                review.user_id,
                review.post_id,
                claim.review_id,
                review.request_count,
                outcome,
                occurred_at,
            )
            .map_err(ApiError::from)
        });
        if let Err(error) = result {
            self.claim_event(
                self.logger.error(AI_REVIEW_LOGGER),
                "result_notification_failed",
                claim,
            )
            .field(fields::ERROR_CODE, error.exception_name())
            .log("AI 리뷰 결과 알림 저장 실패, 결과는 확정됨");
        }
    }

    // ---- 로그 ----

    /// 모든 이벤트가 공통으로 남기는 식별 필드를 붙인다.
    fn claim_event<'a>(
        &self,
        builder: EventBuilder<'a>,
        name: &str,
        claim: &Claim,
    ) -> EventBuilder<'a> {
        builder
            .field(fields::TYPE, TYPE_AI_REVIEW)
            .field(fields::EVENT, name)
            .field("aiReviewId", claim.review_id.hyphenated().to_string())
            .field(fields::POST_ID, claim.post_id.hyphenated().to_string())
            .field(fields::USER_ID, claim.user_id.hyphenated().to_string())
            .field("attempt", i64::from(claim.attempt))
    }

    fn log_finished(&self, review: &AiReview, claim: &Claim) {
        let completed = review.status == Status::Completed;
        let builder = if completed {
            self.logger.info(AI_REVIEW_LOGGER)
        } else {
            self.logger.warn(AI_REVIEW_LOGGER)
        };
        let name = if completed {
            "review_completed"
        } else {
            "review_failed"
        };
        let started = review.started_at.unwrap_or(review.last_requested_at);
        let ended = review.completed_at.unwrap_or(started);
        let event = self
            .claim_event(builder, name, claim)
            .field("promptVersion", i64::from(claim.version.version))
            .field("model", claim.version.model.as_str())
            .field(
                "queueWaitMs",
                millis_between(review.last_requested_at, started),
            )
            .field("totalMs", millis_between(review.last_requested_at, ended));
        with_optional_fields(event, review).log(if completed {
            "AI 리뷰 완료"
        } else {
            "AI 리뷰 실패"
        });
    }

    /// 자동 재시도를 예약했다. DB에는 마지막 시도의 값만 남으므로 재시도 비용을 추적하려고 시도마다 사용량을 남긴다.
    fn log_retry_scheduled(&self, review: &AiReview, claim: &Claim, delay: RetryDelay) {
        let claim = Claim::of(review, &claim.version);
        let mut event = self
            .claim_event(
                self.logger.warn(AI_REVIEW_LOGGER),
                "review_retry_scheduled",
                &claim,
            )
            .field("roundAttempt", i64::from(review.round_attempt))
            .field("requestCount", i64::from(review.request_count))
            .field("promptVersion", i64::from(claim.version.version))
            .field("model", claim.version.model.as_str())
            .field("retryInMs", delay.delay.as_millis() as i64)
            .field("retryAfterUsed", delay.retry_after_used);
        event = with_optional_fields(event, review);
        if let Some(id) = &review.invocation.openai_response_id {
            event = event.field("openaiResponseId", id.as_str());
        }
        event.log("AI 리뷰 자동 재시도 예약");
    }
}

fn millis_between(from: Timestamp, to: Timestamp) -> i64 {
    to.as_millisecond() - from.as_millisecond()
}

/// 값이 없는 필드는 남기지 않는다. 순서가 곧 로그 필드 순서다.
fn with_optional_fields<'a>(mut event: EventBuilder<'a>, review: &AiReview) -> EventBuilder<'a> {
    let invocation = &review.invocation;
    if let Some(v) = invocation.latency_ms {
        event = event.field("latencyMs", v);
    }
    for (key, value) in [
        ("inputTokens", invocation.input_tokens),
        ("cachedInputTokens", invocation.cached_input_tokens),
        ("outputTokens", invocation.output_tokens),
        ("reasoningTokens", invocation.reasoning_tokens),
    ] {
        if let Some(v) = value {
            event = event.field(key, i64::from(v));
        }
    }
    if let Some(code) = &review.error_code {
        event = event.field(fields::ERROR_CODE, code.as_str());
    }
    event
}

fn invocation_of(attempt: Option<&Attempt>) -> Invocation {
    let Some(attempt) = attempt else {
        return Invocation::default();
    };
    Invocation {
        input_tokens: attempt.usage.input_tokens,
        cached_input_tokens: attempt.usage.cached_input_tokens,
        output_tokens: attempt.usage.output_tokens,
        reasoning_tokens: attempt.usage.reasoning_tokens,
        latency_ms: Some(attempt.latency.as_millis() as i64),
        openai_response_id: attempt.response_id.clone(),
    }
}

fn failure_of(error: &AiReviewGenerationError) -> Failure {
    Failure {
        error_code: error.failure_type.as_str().to_owned(),
        retryable: error.failure_type.is_retryable(),
        invocation: invocation_of(error.attempt.as_ref()),
        retry_after: error.retry_after,
    }
}
