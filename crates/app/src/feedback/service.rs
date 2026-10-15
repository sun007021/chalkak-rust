//! 원본 `FeedbackService`/`FeedbackSubmissionResult`.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_db::Pool;
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::Feedback;
use super::repository::FeedbackRepository;
use crate::web::ApiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedbackSubmissionResult {
    pub feedback_id: Uuid,
    pub created_at: Timestamp,
}

#[derive(Clone)]
pub struct FeedbackService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    repository: FeedbackRepository,
}

impl FeedbackService {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>) -> Self {
        FeedbackService {
            pool,
            clock,
            repository: FeedbackRepository,
        }
    }

    pub fn submit(
        &self,
        user_id: Uuid,
        content: Option<&str>,
    ) -> Result<FeedbackSubmissionResult, ApiError> {
        let mut feedback = Feedback::create(user_id, content)?;
        let mut conn = self.pool.get()?;
        self.repository
            .save(&mut conn, &mut feedback, self.clock.now())?;
        Ok(FeedbackSubmissionResult {
            feedback_id: feedback.id.expect("저장된 피드백에는 식별자가 있다"),
            created_at: feedback.created_at.expect("저장된 피드백에는 시각이 있다"),
        })
    }
}
