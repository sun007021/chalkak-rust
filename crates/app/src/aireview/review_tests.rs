//! `AiReview` 전이 시험. 기대값은 원본 `AiReviewTest`에서 가져왔다.

use jiff::Timestamp;
use uuid::Uuid;

use super::domain::{AiReviewResult, Criterion, Feedback};
use super::review::{AiReview, FailureReason, Invocation, ReviewError, Status};

const MAX_CLAIMS: i32 = 3;

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x019a_0000_0000_7000_8000_0000_0000_0000 + n)
}

fn requested_at() -> Timestamp {
    ts("2026-10-20T05:00:00Z")
}
fn now() -> Timestamp {
    ts("2026-10-20T05:01:00Z")
}
fn lease_end() -> Timestamp {
    ts("2026-10-20T05:03:00Z")
}
fn retry_at() -> Timestamp {
    ts("2026-10-20T05:01:40Z")
}
fn again_at() -> Timestamp {
    ts("2026-10-20T06:00:00Z")
}

fn invocation() -> Invocation {
    Invocation {
        input_tokens: Some(1200),
        cached_input_tokens: Some(0),
        output_tokens: Some(300),
        reasoning_tokens: Some(64),
        latency_ms: Some(21_000),
        openai_response_id: Some("resp_abc".to_owned()),
    }
}

fn result() -> AiReviewResult {
    AiReviewResult {
        overall_score: 7.5,
        criteria: vec![Criterion {
            key: "COMPOSITION".to_owned(),
            label: "구도".to_owned(),
            score: 7.5,
        }],
        distinctive_perspective: true,
        feedback: Feedback {
            summary: "구도가 좋습니다.".to_owned(),
            strength: "빛이 좋습니다.".to_owned(),
            weakness: "수평이 아쉽습니다.".to_owned(),
            suggestion: "낮게 찍어 보세요.".to_owned(),
        },
    }
}

const OUTPUT: &str = "{\"compositionScore\":7.46,\"feedback\":\"구도가 좋습니다.\"}";

fn queued() -> AiReview {
    AiReview::request(id(1), id(1), id(1), id(1), requested_at())
}

fn claim(review: &mut AiReview, at: Timestamp) -> bool {
    review
        .claim(at, at + std::time::Duration::from_secs(120), MAX_CLAIMS)
        .unwrap()
}

fn processing() -> AiReview {
    let mut review = queued();
    review.claim(now(), lease_end(), MAX_CLAIMS).unwrap();
    review
}

fn complete(review: &mut AiReview, attempt: i32) -> bool {
    review.complete(attempt, result(), OUTPUT.to_owned(), invocation(), now())
}

fn fail_or_retry(review: &mut AiReview, attempt: i32, code: &str, retryable: bool) -> bool {
    review.fail_or_retry(
        attempt,
        code,
        retryable,
        invocation(),
        retry_at(),
        MAX_CLAIMS,
        now(),
    )
}

fn retry_waiting() -> AiReview {
    let mut review = processing();
    fail_or_retry(&mut review, 1, "TIMEOUT", true);
    review
}

fn failed() -> AiReview {
    let mut review = processing();
    fail_or_retry(&mut review, 1, "INVALID_OUTPUT", false);
    review
}

fn failed_after_requests(requests: i32) -> AiReview {
    let mut review = failed();
    for _ in 2..=requests {
        review.request_again(id(1), again_at()).unwrap();
        let attempt = review.attempt + 1;
        claim(&mut review, again_at());
        fail_or_retry(&mut review, attempt, "INVALID_OUTPUT", false);
    }
    review
}

// AiReviewTest#request_newReview_startsQueued
#[test]
fn request_starts_queued() {
    let review = queued();
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.attempt, 0);
    assert_eq!(review.round_attempt, 0);
    assert_eq!(review.request_count, 1);
    assert_eq!(review.last_requested_at, requested_at());
    assert!(review.next_attempt_at.is_none());
    assert!(review.result.is_none());
    assert!(review.completed_at.is_none());
}

// AiReviewTest#reviewResult_roundTrip_keepsFixedFieldNames
#[test]
fn review_result_round_trip_keeps_fixed_field_names() {
    let json = serde_json::to_string(&result()).unwrap();
    assert!(json.starts_with("{\"overallScore\":7.5,\"criteria\":[{\"key\":\"COMPOSITION\""));
    let back: AiReviewResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back, result());
}

// AiReviewTest#claim_queued_startsProcessing
#[test]
fn claim_queued_starts_processing() {
    let mut review = queued();
    assert!(claim(&mut review, now()));
    assert_eq!(review.status, Status::Processing);
    assert_eq!(review.attempt, 1);
    assert_eq!(review.round_attempt, 1);
    assert_eq!(review.lease_expires_at, Some(ts("2026-10-20T05:03:00Z")));
    assert_eq!(review.started_at, Some(now()));
}

// AiReviewTest#claim_expiredLease_reclaimsAndKeepsStartedAt
#[test]
fn claim_expired_lease_reclaims_and_keeps_started_at() {
    let mut review = processing();
    let later = lease_end() + std::time::Duration::from_secs(1);
    assert!(claim(&mut review, later));
    assert_eq!(review.attempt, 2);
    assert_eq!(review.round_attempt, 2);
    assert_eq!(
        review.lease_expires_at,
        Some(later + std::time::Duration::from_secs(120))
    );
    assert_eq!(review.started_at, Some(now()));
}

// AiReviewTest#claim_notClaimable_throws
#[test]
fn claim_not_claimable_is_illegal_state() {
    let live = processing();
    let mut done = processing();
    complete(&mut done, 1);
    let one = std::time::Duration::from_secs(1);
    for (review, at) in [
        (&mut live.clone(), lease_end() - one),
        (&mut live.clone(), lease_end()),
        (&mut done, now()),
    ] {
        let err = review
            .claim(at, at + one, MAX_CLAIMS)
            .expect_err("가져갈 수 없다");
        assert!(matches!(err, ReviewError::IllegalState(_)));
    }
    assert_eq!(live.status, Status::Processing);
}

// AiReviewTest#claim_overMaxClaims_failsWithLeaseExhausted
#[test]
fn claim_over_max_claims_fails_with_lease_exhausted() {
    let mut review = queued();
    let mut at = now();
    for _ in 1..=MAX_CLAIMS {
        claim(&mut review, at);
        at += std::time::Duration::from_secs(121);
    }
    assert!(!claim(&mut review, at));
    assert_eq!(review.status, Status::Failed);
    assert_eq!(
        review.error_code.as_deref(),
        Some(FailureReason::LeaseExhausted.name())
    );
    assert_eq!(review.attempt, MAX_CLAIMS);
    assert_eq!(review.round_attempt, MAX_CLAIMS);
    assert!(review.lease_expires_at.is_none());
    assert_eq!(review.completed_at, Some(at));
}

// AiReviewTest#isClaimLimitReached_aroundMaxClaims_reachedOnlyAfterMaxClaims
#[test]
fn claim_limit_is_reached_only_after_max_claims() {
    let mut review = queued();
    let mut at = now();
    for _ in 1..MAX_CLAIMS {
        claim(&mut review, at);
        at += std::time::Duration::from_secs(121);
    }
    assert!(!review.is_claim_limit_reached(MAX_CLAIMS));
    claim(&mut review, at);
    assert!(review.is_claim_limit_reached(MAX_CLAIMS));
}

// AiReviewTest#claim_exhaustedAfterRetry_clearsPreviousInvocation
#[test]
fn claim_exhausted_after_retry_clears_previous_invocation() {
    let mut review = retry_waiting();
    claim(&mut review, retry_at());
    claim(
        &mut review,
        retry_at() + std::time::Duration::from_secs(121),
    );
    assert_eq!(review.invocation, invocation());
    assert!(!claim(
        &mut review,
        retry_at() + std::time::Duration::from_secs(242)
    ));
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.invocation, Invocation::default());
}

// AiReviewTest#claim_retryWaiting_claimableOnlyFromNextAttemptAt
#[test]
fn claim_retry_waiting_only_from_next_attempt_at() {
    let mut review = retry_waiting();
    let before = retry_at() - std::time::Duration::from_secs(1);
    assert!(review.claim(before, before, MAX_CLAIMS).is_err());
    assert_eq!(review.status, Status::Queued);
    assert!(claim(&mut review, retry_at()));
    assert_eq!(review.status, Status::Processing);
    assert_eq!(review.attempt, 2);
    assert_eq!(review.round_attempt, 2);
    assert!(review.next_attempt_at.is_none());
    assert_eq!(review.started_at, Some(now()));
}

// AiReviewTest#complete_currentAttempt_storesResultAndReleasesLease
#[test]
fn complete_current_attempt_stores_result_and_releases_lease() {
    let mut review = processing();
    assert!(complete(&mut review, 1));
    assert_eq!(review.status, Status::Completed);
    assert_eq!(review.result, Some(result()));
    assert_eq!(review.output.as_deref(), Some(OUTPUT));
    assert_eq!(review.invocation, invocation());
    assert!(review.error_code.is_none());
    assert!(review.lease_expires_at.is_none());
    assert_eq!(review.completed_at, Some(now()));
}

// AiReviewTest#failOrRetry_retryableWithBudget_requeuesWithNextAttemptAt
#[test]
fn retryable_failure_with_budget_requeues() {
    let mut review = processing();
    assert!(fail_or_retry(&mut review, 1, "TIMEOUT", true));
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.next_attempt_at, Some(retry_at()));
    assert_eq!(review.error_code.as_deref(), Some("TIMEOUT"));
    assert_eq!(review.invocation, invocation());
    assert!(review.lease_expires_at.is_none());
    assert!(review.completed_at.is_none());
    assert_eq!(review.attempt, 1);
    assert_eq!(review.round_attempt, 1);
    assert_eq!(review.started_at, Some(now()));
    assert!(review.is_retry_scheduled());
}

// AiReviewTest#failOrRetry_retryableWithoutBudget_failsWithLastReason
#[test]
fn retryable_failure_without_budget_fails_with_last_reason() {
    let mut review = retry_waiting();
    claim(&mut review, retry_at());
    claim(
        &mut review,
        retry_at() + std::time::Duration::from_secs(121),
    );
    assert!(fail_or_retry(&mut review, 3, "RATE_LIMITED", true));
    assert_eq!(review.round_attempt, MAX_CLAIMS);
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.error_code.as_deref(), Some("RATE_LIMITED"));
    assert!(review.next_attempt_at.is_none());
    assert!(review.lease_expires_at.is_none());
    assert_eq!(review.completed_at, Some(now()));
}

// AiReviewTest#failOrRetry_notRetryable_failsImmediately
#[test]
fn non_retryable_failure_fails_immediately_and_keeps_usage() {
    let mut review = processing();
    assert!(fail_or_retry(&mut review, 1, "INVALID_OUTPUT", false));
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.error_code.as_deref(), Some("INVALID_OUTPUT"));
    assert_eq!(review.invocation, invocation());
    assert!(review.result.is_none());
    assert!(review.output.is_none());
    assert!(review.next_attempt_at.is_none());
    assert!(review.lease_expires_at.is_none());
    assert_eq!(review.completed_at, Some(now()));
}

// AiReviewTest#failOrRetry_withoutInvocation_leavesUsageEmpty
#[test]
fn failure_without_invocation_leaves_usage_empty() {
    let mut review = processing();
    review.fail_or_retry(
        1,
        FailureReason::PostUnavailable.name(),
        false,
        Invocation::default(),
        retry_at(),
        MAX_CLAIMS,
        now(),
    );
    assert_eq!(review.invocation, Invocation::default());
}

// AiReviewTest#completeAndFailOrRetry_staleOrTerminal_changesNothing
#[test]
fn stale_or_terminal_reviews_ignore_updates() {
    let mut reclaimed = processing();
    claim(
        &mut reclaimed,
        lease_end() + std::time::Duration::from_secs(1),
    );
    let mut finished = processing();
    complete(&mut finished, 1);

    assert!(!complete(&mut reclaimed, 1));
    assert!(!fail_or_retry(&mut reclaimed, 1, "TIMEOUT", true));
    assert_eq!(reclaimed.status, Status::Processing);
    assert_eq!(reclaimed.attempt, 2);
    assert!(reclaimed.error_code.is_none());
    assert!(reclaimed.next_attempt_at.is_none());
    assert!(reclaimed.completed_at.is_none());

    assert!(!complete(&mut finished, 1));
    assert!(!fail_or_retry(&mut finished, 1, "TIMEOUT", true));
    assert_eq!(finished.status, Status::Completed);
    assert!(finished.error_code.is_none());
}

// AiReviewTest#requestAgain_failed_requeuesWithFreshRound / keepsAttemptAndFirstRequest
#[test]
fn request_again_requeues_with_fresh_round() {
    let mut review = failed();
    review.request_again(id(2), again_at()).unwrap();
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.request_count, 2);
    assert_eq!(review.last_requested_at, again_at());
    assert_eq!(review.round_attempt, 0);
    assert_eq!(review.prompt_version_id, id(2));
    assert!(review.started_at.is_none());
    assert!(review.completed_at.is_none());
    assert!(review.error_code.is_none());
    assert!(review.next_attempt_at.is_none());
    assert!(review.lease_expires_at.is_none());
    assert_eq!(review.invocation, Invocation::default());
    assert_eq!(review.attempt, 1);
    assert_eq!(review.request_id, id(1));
    assert_eq!(review.requested_at, requested_at());
}

// AiReviewTest#requestAgain_notFailed_throws
#[test]
fn request_again_requires_failed_status() {
    let mut done = processing();
    complete(&mut done, 1);
    for mut review in [queued(), processing(), done] {
        let before = review.status;
        let err = review.request_again(id(2), again_at()).unwrap_err();
        assert!(matches!(err, ReviewError::IllegalState(_)));
        assert_eq!(review.status, before);
        assert_eq!(review.request_count, 1);
    }
}

// AiReviewTest#requestAgain_limitReached_throwsBusinessException
#[test]
fn request_again_at_limit_is_business_error_and_changes_nothing() {
    let mut review = failed_after_requests(3);
    let err = review.request_again(id(2), again_at()).unwrap_err();
    assert!(matches!(err, ReviewError::RequestsExhausted));
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.request_count, 3);
    assert_eq!(review.prompt_version_id, id(1));
    assert!(review.completed_at.is_some());
}

// AiReviewTest#validateRequestsLeft_byRequestCount
#[test]
fn validate_requests_left_by_request_count() {
    assert!(failed_after_requests(2).validate_requests_left().is_ok());
    assert!(matches!(
        failed_after_requests(3).validate_requests_left(),
        Err(ReviewError::RequestsExhausted)
    ));
}

// AiReviewTest#failureReasonMatches_byErrorCodeName
#[test]
fn failure_reason_matches_only_its_own_name() {
    assert!(FailureReason::PostUnavailable.matches(Some("POST_UNAVAILABLE")));
    assert!(!FailureReason::PostUnavailable.matches(Some("LEASE_EXHAUSTED")));
    assert!(!FailureReason::PostUnavailable.matches(Some("INVALID_OUTPUT")));
    assert!(!FailureReason::PostUnavailable.matches(None));
}

// AiReviewTest#isResultNotifiable_byStateAndErrorCode
#[test]
fn result_notifiable_excludes_post_unavailable_and_unfinished() {
    let mut post_unavailable = processing();
    fail_or_retry(
        &mut post_unavailable,
        1,
        FailureReason::PostUnavailable.name(),
        false,
    );
    let mut lease_exhausted = processing();
    lease_exhausted
        .claim(
            lease_end() + std::time::Duration::from_secs(1),
            lease_end() + std::time::Duration::from_secs(120),
            1,
        )
        .unwrap();
    let mut completed = processing();
    complete(&mut completed, 1);

    assert!(completed.is_result_notifiable());
    assert!(failed().is_result_notifiable());
    assert_eq!(
        lease_exhausted.error_code.as_deref(),
        Some(FailureReason::LeaseExhausted.name())
    );
    assert!(lease_exhausted.is_result_notifiable());
    assert!(!post_unavailable.is_result_notifiable());
    assert!(!retry_waiting().is_result_notifiable());
    assert!(!queued().is_result_notifiable());
    assert!(!processing().is_result_notifiable());
}

// AiReviewTest#canRequestAgain_byStatusAndRequestCount
#[test]
fn can_request_again_by_status_and_request_count() {
    let mut done = processing();
    complete(&mut done, 1);
    assert!(!queued().can_request_again());
    assert!(!processing().can_request_again());
    assert!(!done.can_request_again());
    assert!(failed_after_requests(1).can_request_again());
    assert!(failed_after_requests(2).can_request_again());
    assert!(!failed_after_requests(3).can_request_again());
}

// AiReviewTest#completeAndFailOrRetry_afterRequestAgain_ignoresPreviousRoundAttempt
#[test]
fn updates_from_previous_round_are_ignored_after_request_again() {
    let mut review = failed();
    review.request_again(id(2), again_at()).unwrap();
    assert!(!complete(&mut review, 1));
    assert!(!fail_or_retry(&mut review, 1, "TIMEOUT", true));
    assert_eq!(review.status, Status::Queued);
    claim(&mut review, again_at());
    assert_eq!(review.attempt, 2);
    assert!(!complete(&mut review, 1));
    assert!(!fail_or_retry(&mut review, 1, "TIMEOUT", true));
    assert_eq!(review.status, Status::Processing);
    assert!(review.result.is_none());
}
