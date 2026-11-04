//! AI 리뷰 저장소(원본 `AiReviewRepositoryImpl`, `AiReviewJpaRepository`). 연결(또는 트랜잭션)은 호출 측이 넘긴다.
//!
//! JPA는 변경 감지로 바뀐 열만 갱신하지만 여기서는 가변 열 전체를 한 문장으로 갱신한다. 한 행을 잠근 트랜잭션 안에서 읽고
//! 바꾸므로 결과는 같다.

use chalkak_db::{Db, DbError};
use jiff::Timestamp;
use postgres::Row;
use uuid::Uuid;

use super::domain::AiReviewResult;
use super::review::{AiReview, Invocation, Status};

const INSERT_IF_ABSENT_SQL: &str = "INSERT INTO ai_reviews (post_id, user_id, prompt_version_id, \
     status, attempt, request_id, requested_at, last_requested_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (post_id) DO NOTHING";

const FIND_BY_POST_SQL: &str = "SELECT id, post_id, user_id, prompt_version_id, status, attempt, \
     round_attempt, request_count, next_attempt_at, lease_expires_at, result::text AS result, \
     output::text AS output, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens, \
     latency_ms, openai_response_id, error_code, request_id, requested_at, last_requested_at, \
     started_at, completed_at FROM ai_reviews WHERE post_id = $1";

const FIND_BY_POST_FOR_UPDATE_SQL: &str = "SELECT id, post_id, user_id, prompt_version_id, status, \
     attempt, round_attempt, request_count, next_attempt_at, lease_expires_at, result::text AS result, \
     output::text AS output, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens, \
     latency_ms, openai_response_id, error_code, request_id, requested_at, last_requested_at, \
     started_at, completed_at FROM ai_reviews WHERE post_id = $1 FOR UPDATE";

const FIND_BY_ID_FOR_UPDATE_SQL: &str = "SELECT id, post_id, user_id, prompt_version_id, status, \
     attempt, round_attempt, request_count, next_attempt_at, lease_expires_at, result::text AS result, \
     output::text AS output, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens, \
     latency_ms, openai_response_id, error_code, request_id, requested_at, last_requested_at, \
     started_at, completed_at FROM ai_reviews WHERE id = $1 FOR UPDATE";

/// 다른 트랜잭션이 잠근 행은 건너뛰어 워커끼리 같은 행을 가져가지 않는다. 기다린 시간은 다음 시도 시각이 있으면 그 시각,
/// 없으면 이번 회차의 요청 시각부터 센다.
const FIND_CLAIMABLE_SQL: &str = "SELECT id, post_id, user_id, prompt_version_id, status, attempt, \
     round_attempt, request_count, next_attempt_at, lease_expires_at, result::text AS result, \
     output::text AS output, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens, \
     latency_ms, openai_response_id, error_code, request_id, requested_at, last_requested_at, \
     started_at, completed_at FROM ai_reviews \
     WHERE (status = 'QUEUED' AND (next_attempt_at IS NULL OR next_attempt_at <= $1)) \
        OR (status = 'PROCESSING' AND lease_expires_at < $1) \
     ORDER BY COALESCE(next_attempt_at, last_requested_at), id LIMIT 1 FOR UPDATE SKIP LOCKED";

const UPDATE_SQL: &str = "UPDATE ai_reviews SET prompt_version_id = $2, status = $3, attempt = $4, \
     round_attempt = $5, request_count = $6, next_attempt_at = $7, lease_expires_at = $8, \
     result = $9::text::jsonb, output = $10::text::jsonb, input_tokens = $11, cached_input_tokens = $12, \
     output_tokens = $13, reasoning_tokens = $14, latency_ms = $15, openai_response_id = $16, \
     error_code = $17, last_requested_at = $18, started_at = $19, completed_at = $20 WHERE id = $1";

/// DB가 마이크로초까지 저장하므로 저장값과 메모리 값이 같도록 맞춘다.
fn micros(at: Timestamp) -> Timestamp {
    Timestamp::from_microsecond(at.as_microsecond()).unwrap_or(at)
}

fn from_row(row: &Row) -> Result<AiReview, DbError> {
    let status_name: String = row.try_get("status")?;
    let status = Status::parse(&status_name)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 AI 리뷰 상태: {status_name}")))?;
    let result: Option<String> = row.try_get("result")?;
    let result = result
        .map(|text| serde_json::from_str::<AiReviewResult>(&text))
        .transpose()
        .map_err(|e| DbError::Config(format!("저장된 AI 리뷰 결과가 올바르지 않습니다: {e}")))?;
    Ok(AiReview {
        id: Some(row.try_get("id")?),
        post_id: row.try_get("post_id")?,
        user_id: row.try_get("user_id")?,
        prompt_version_id: row.try_get("prompt_version_id")?,
        status,
        attempt: row.try_get("attempt")?,
        round_attempt: row.try_get("round_attempt")?,
        request_count: row.try_get("request_count")?,
        next_attempt_at: row.try_get("next_attempt_at")?,
        lease_expires_at: row.try_get("lease_expires_at")?,
        result,
        output: row.try_get("output")?,
        invocation: Invocation {
            input_tokens: row.try_get("input_tokens")?,
            cached_input_tokens: row.try_get("cached_input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            reasoning_tokens: row.try_get("reasoning_tokens")?,
            latency_ms: row.try_get("latency_ms")?,
            openai_response_id: row.try_get("openai_response_id")?,
        },
        error_code: row.try_get("error_code")?,
        request_id: row.try_get("request_id")?,
        requested_at: row.try_get("requested_at")?,
        last_requested_at: row.try_get("last_requested_at")?,
        started_at: row.try_get("started_at")?,
        completed_at: row.try_get("completed_at")?,
    })
}

fn one(
    db: &mut impl Db,
    sql: &str,
    param: &(dyn postgres::types::ToSql + Sync),
) -> Result<Option<AiReview>, DbError> {
    db.query_opt(sql, &[param])?
        .as_ref()
        .map(from_row)
        .transpose()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AiReviewRepository;

impl AiReviewRepository {
    /// `insertIfAbsent`: 게시물에 리뷰가 없을 때만 대기 상태로 넣는다. 이미 있으면 아무것도 바꾸지 않고, 동시에 요청해도 한
    /// 행만 만들어진다. 새로 만들었으면 1, 이미 있었으면 0.
    pub fn insert_if_absent(&self, db: &mut impl Db, review: &AiReview) -> Result<u64, DbError> {
        Ok(db.execute(
            INSERT_IF_ABSENT_SQL,
            &[
                &review.post_id,
                &review.user_id,
                &review.prompt_version_id,
                &review.status.as_str(),
                &review.attempt,
                &review.request_id,
                &micros(review.requested_at),
                &micros(review.last_requested_at),
            ],
        )?)
    }

    pub fn find_by_post_id(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
    ) -> Result<Option<AiReview>, DbError> {
        one(db, FIND_BY_POST_SQL, &post_id)
    }

    /// 실패한 리뷰를 다시 요청하는 동시 요청이 한 건만 반영되도록 트랜잭션의 첫 리뷰 읽기로 쓴다.
    pub fn find_by_post_id_for_update(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
    ) -> Result<Option<AiReview>, DbError> {
        one(db, FIND_BY_POST_FOR_UPDATE_SQL, &post_id)
    }

    /// 가져갈 수 있는 가장 오래 기다린 행 하나를 잠가서 읽는다. 호출한 트랜잭션 안에서 바로 상태를 바꿔야 한다.
    pub fn find_claimable_for_update(
        &self,
        db: &mut impl Db,
        now: Timestamp,
    ) -> Result<Option<AiReview>, DbError> {
        one(db, FIND_CLAIMABLE_SQL, &micros(now))
    }

    pub fn find_by_id_for_update(
        &self,
        db: &mut impl Db,
        id: Uuid,
    ) -> Result<Option<AiReview>, DbError> {
        one(db, FIND_BY_ID_FOR_UPDATE_SQL, &id)
    }

    /// 잠가서 읽은 행의 바뀐 값을 저장한다(JPA 변경 감지에 해당).
    pub fn save(&self, db: &mut impl Db, review: &AiReview) -> Result<(), DbError> {
        let id = review.id.ok_or_else(|| {
            DbError::Config("저장되지 않은 AI 리뷰는 갱신할 수 없습니다".to_owned())
        })?;
        let result = review
            .result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| DbError::Config(format!("AI 리뷰 결과를 직렬화할 수 없습니다: {e}")))?;
        let invocation = &review.invocation;
        db.execute(
            UPDATE_SQL,
            &[
                &id,
                &review.prompt_version_id,
                &review.status.as_str(),
                &review.attempt,
                &review.round_attempt,
                &review.request_count,
                &review.next_attempt_at.map(micros),
                &review.lease_expires_at.map(micros),
                &result,
                &review.output,
                &invocation.input_tokens,
                &invocation.cached_input_tokens,
                &invocation.output_tokens,
                &invocation.reasoning_tokens,
                &invocation.latency_ms,
                &invocation.openai_response_id,
                &review.error_code,
                &micros(review.last_requested_at),
                &review.started_at.map(micros),
                &review.completed_at.map(micros),
            ],
        )?;
        Ok(())
    }
}
