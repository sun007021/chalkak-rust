//! 게시물 한 건의 AI 리뷰(원본 `AiReview`). 게시물당 하나이며 사용자가 요청할 때 고른 프롬프트 버전을 그 요청이 끝날 때까지 쓴다.
//! 일시적 실패는 같은 행을 대기로 되돌려 자동으로 다시 시도하고, 실패한 리뷰는 사용자가 정해진 횟수까지 다시 요청할 수 있다.
//! 다시 요청하면 같은 행을 대기로 되돌리고 그때의 최신 버전으로 다시 고정한다.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::AiReviewResult;
use crate::web::ApiError;

const MAX_REQUEST_COUNT: i32 = 3;

/// 서버 안에서 쓰는 처리 단계.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Queued,
    Processing,
    Completed,
    Failed,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "QUEUED",
            Status::Processing => "PROCESSING",
            Status::Completed => "COMPLETED",
            Status::Failed => "FAILED",
        }
    }

    pub fn parse(name: &str) -> Option<Status> {
        [
            Status::Queued,
            Status::Processing,
            Status::Completed,
            Status::Failed,
        ]
        .into_iter()
        .find(|s| s.as_str() == name)
    }
}

/// 워커가 직접 정하는 실패 사유. 생성기 실패는 생성기의 실패 유형 이름을 그대로 오류 코드로 저장한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureReason {
    PostUnavailable,
    PromptVersionUnavailable,
    LeaseExhausted,
    UnexpectedError,
}

impl FailureReason {
    pub const fn name(self) -> &'static str {
        match self {
            FailureReason::PostUnavailable => "POST_UNAVAILABLE",
            FailureReason::PromptVersionUnavailable => "PROMPT_VERSION_UNAVAILABLE",
            FailureReason::LeaseExhausted => "LEASE_EXHAUSTED",
            FailureReason::UnexpectedError => "UNEXPECTED_ERROR",
        }
    }

    /// 저장된 errorCode가 이 사유인지. errorCode 컬럼은 호출 실패 종류까지 담는 문자열이라 이름으로 비교한다.
    pub fn matches(self, error_code: Option<&str>) -> bool {
        error_code == Some(self.name())
    }
}

/// OpenAI 호출에서 받은 사용량·지연시간·응답 ID. 응답 본문을 받지 못했거나 OpenAI가 주지 않은 값은 `None`이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invocation {
    pub input_tokens: Option<i32>,
    pub cached_input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub reasoning_tokens: Option<i32>,
    pub latency_ms: Option<i64>,
    pub openai_response_id: Option<String>,
}

/// 도메인 규칙 위반. 상태가 맞지 않는 호출은 프로그램 오류(`IllegalState`)이고, 요청 횟수 소진은 사용자에게 알린다.
#[derive(Debug)]
pub enum ReviewError {
    IllegalState(String),
    RequestsExhausted,
}

impl ReviewError {
    pub fn into_api_error(self) -> ApiError {
        match self {
            ReviewError::RequestsExhausted => ApiError::App(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "더 이상 AI 리뷰를 다시 요청할 수 없습니다.",
            )),
            ReviewError::IllegalState(message) => ApiError::Internal(message),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AiReview {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub post_id: Uuid,
    pub user_id: Uuid,
    /// 이번 회차에 쓰는 프롬프트 버전. 요청하거나 다시 요청할 때 고정하며 회차가 끝날 때까지 바뀌지 않는다.
    pub prompt_version_id: Uuid,
    pub status: Status,
    /// 워커가 가져갈 때마다 늘고 줄지 않는다. 늦게 끝난 워커의 반영을 막는 기준이다.
    pub attempt: i32,
    /// 이번 회차(사용자 요청 1회) 안에서 가져간 횟수. 재시도와 임대 회수를 모두 센다.
    pub round_attempt: i32,
    /// 사용자 요청 횟수(처음 요청 포함). 상한은 3번이다.
    pub request_count: i32,
    pub next_attempt_at: Option<Timestamp>,
    pub lease_expires_at: Option<Timestamp>,
    pub result: Option<AiReviewResult>,
    /// 모델이 돌려준 output_text를 검증·재해석하지 않고 담는다.
    pub output: Option<String>,
    pub invocation: Invocation,
    pub error_code: Option<String>,
    pub request_id: Uuid,
    pub requested_at: Timestamp,
    /// 이번 회차의 사용자 요청 시각. 처음 요청 시각은 `requested_at`에 남는다.
    pub last_requested_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
}

impl AiReview {
    pub fn request(
        post_id: Uuid,
        user_id: Uuid,
        prompt_version_id: Uuid,
        request_id: Uuid,
        requested_at: Timestamp,
    ) -> AiReview {
        AiReview {
            id: None,
            post_id,
            user_id,
            prompt_version_id,
            status: Status::Queued,
            attempt: 0,
            round_attempt: 0,
            request_count: 1,
            next_attempt_at: None,
            lease_expires_at: None,
            result: None,
            output: None,
            invocation: Invocation::default(),
            error_code: None,
            request_id,
            requested_at,
            last_requested_at: requested_at,
            started_at: None,
            completed_at: None,
        }
    }

    /// 이번 회차에서 한 번 더 가져가면 가져간 횟수 상한을 넘는지. 넘으면 [`AiReview::claim`]이 처리하지 않고 실패로 끝낸다.
    pub fn is_claim_limit_reached(&self, max_claims: i32) -> bool {
        self.round_attempt + 1 > max_claims
    }

    /// 워커가 이 행을 가져간다. 대기 중(다음 시도 시각이 지난 것)이거나 임대가 끝난 처리 중 행만 가져갈 수 있으며 가져갈 때마다 시도
    /// 번호와 회차 시도 횟수가 늘고 줄지 않는다. 회차 시도 횟수가 상한을 넘게 되면 처리하지 않고 실패로 끝낸다(시도 번호와 회차
    /// 시도 횟수는 늘리지 않는다). 마지막 시도는 결과를 모르는 중단이므로 이전 시도의 사용량과 응답 ID는 비운다.
    ///
    /// 처리해야 하면 `true`, 상한을 넘어 실패로 끝냈으면 `false`.
    pub fn claim(
        &mut self,
        now: Timestamp,
        lease_expires_at: Timestamp,
        max_claims: i32,
    ) -> Result<bool, ReviewError> {
        if !self.is_claimable_at(now) {
            return Err(ReviewError::IllegalState(format!(
                "가져갈 수 없는 AI 리뷰입니다. 상태: {}",
                self.status.as_str()
            )));
        }
        if self.is_claim_limit_reached(max_claims) {
            self.invocation = Invocation::default();
            self.finish(
                Status::Failed,
                Some(FailureReason::LeaseExhausted.name()),
                now,
            );
            return Ok(false);
        }
        self.attempt += 1;
        self.round_attempt += 1;
        self.next_attempt_at = None;
        self.status = Status::Processing;
        self.lease_expires_at = Some(lease_expires_at);
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        Ok(true)
    }

    /// 시도 번호가 현재와 같고 처리 중일 때만 결과를 반영한다. 임대가 끝나 다른 워커가 다시 가져간 뒤 늦게 끝난 워커의 결과는 버린다.
    ///
    /// 반영했으면 `true`, 늦은 워커라 반영하지 않았으면 `false`.
    pub fn complete(
        &mut self,
        attempt: i32,
        result: AiReviewResult,
        output: String,
        invocation: Invocation,
        now: Timestamp,
    ) -> bool {
        if !self.is_processing_attempt(attempt) {
            return false;
        }
        self.result = Some(result);
        self.output = Some(output);
        self.invocation = invocation;
        self.finish(Status::Completed, None, now);
        true
    }

    /// 시도 번호가 현재와 같고 처리 중일 때만 실패를 반영한다. 재시도할 수 있는 실패이고 이번 회차의 시도가 남았으면 대기로 되돌려
    /// 다음 시도 시각을 정한다. 이때 오류 코드와 사용량은 마지막 시도의 값이며 완료 시각은 비워 둔다. 그 밖에는 실패로 끝낸다.
    #[allow(clippy::too_many_arguments)]
    pub fn fail_or_retry(
        &mut self,
        attempt: i32,
        error_code: &str,
        retryable: bool,
        invocation: Invocation,
        next_attempt_at: Timestamp,
        max_claims: i32,
        now: Timestamp,
    ) -> bool {
        if !self.is_processing_attempt(attempt) {
            return false;
        }
        self.invocation = invocation;
        if retryable && self.round_attempt < max_claims {
            self.status = Status::Queued;
            self.error_code = Some(error_code.to_owned());
            self.next_attempt_at = Some(next_attempt_at);
            self.lease_expires_at = None;
            return true;
        }
        self.finish(Status::Failed, Some(error_code), now);
        true
    }

    /// 실패한 리뷰를 사용자가 다시 요청해 대기로 되돌린다. 요청 횟수가 늘고 회차 시도 횟수는 0으로 돌아가며 프롬프트 버전을 다시
    /// 고정한다. 이전 회차의 시작·완료 시각, 오류 코드, 다음 시도 시각, 사용량·지연·응답 ID는 회차가 섞이지 않게 비운다. 시도 번호는
    /// 늦은 워커를 막는 기준이라 그대로 두고, 최초 요청 ID와 시각도 바꾸지 않는다.
    pub fn request_again(
        &mut self,
        prompt_version_id: Uuid,
        now: Timestamp,
    ) -> Result<(), ReviewError> {
        if self.status != Status::Failed {
            return Err(ReviewError::IllegalState(format!(
                "실패한 AI 리뷰만 다시 요청할 수 있습니다. 상태: {}",
                self.status.as_str()
            )));
        }
        self.validate_requests_left()?;
        self.prompt_version_id = prompt_version_id;
        self.request_count += 1;
        self.last_requested_at = now;
        self.round_attempt = 0;
        self.status = Status::Queued;
        self.started_at = None;
        self.completed_at = None;
        self.error_code = None;
        self.next_attempt_at = None;
        self.lease_expires_at = None;
        self.invocation = Invocation::default();
        Ok(())
    }

    /// 실패한 리뷰이고 사용자 요청 횟수가 상한에 닿지 않았으면 다시 요청할 수 있다.
    pub fn can_request_again(&self) -> bool {
        self.status == Status::Failed && self.request_count < MAX_REQUEST_COUNT
    }

    /// 사용자 요청 횟수가 상한에 닿았으면 사용자에게 알린다.
    pub fn validate_requests_left(&self) -> Result<(), ReviewError> {
        if self.request_count >= MAX_REQUEST_COUNT {
            return Err(ReviewError::RequestsExhausted);
        }
        Ok(())
    }

    /// 사용자에게 결과를 알릴 최종 상태인지. 완료와 실패가 대상이고, 게시물을 쓸 수 없어 실패한 경우는 사용자가 할 수 있는 일이
    /// 없어 제외한다. 재시도 대기와 처리 중은 아직 끝나지 않았으므로 대상이 아니다.
    pub fn is_result_notifiable(&self) -> bool {
        if self.is_completed() {
            return true;
        }
        self.is_failed() && !FailureReason::PostUnavailable.matches(self.error_code.as_deref())
    }

    pub fn is_completed(&self) -> bool {
        self.status == Status::Completed
    }

    pub fn is_failed(&self) -> bool {
        self.status == Status::Failed
    }

    /// 실패한 시도를 자동으로 다시 시도하려고 대기 중인지. 앱에는 이 상태도 처리 중으로 보인다.
    pub fn is_retry_scheduled(&self) -> bool {
        self.status == Status::Queued && self.next_attempt_at.is_some()
    }

    fn is_claimable_at(&self, now: Timestamp) -> bool {
        match self.status {
            Status::Queued => self.next_attempt_at.is_none_or(|at| at <= now),
            Status::Processing => self.lease_expires_at.is_some_and(|at| at < now),
            _ => false,
        }
    }

    fn is_processing_attempt(&self, attempt: i32) -> bool {
        self.status == Status::Processing && self.attempt == attempt
    }

    fn finish(&mut self, terminal: Status, error_code: Option<&str>, now: Timestamp) {
        self.status = terminal;
        self.error_code = error_code.map(str::to_owned);
        self.lease_expires_at = None;
        self.next_attempt_at = None;
        self.completed_at = Some(now);
    }
}
