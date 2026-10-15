//! 원본 `FeedbackRepositoryImpl.save`.

use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::Timestamp;

use super::domain::Feedback;

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackRepository;

impl FeedbackRepository {
    /// INSERT. `created_at`은 `@CreationTimestamp`처럼 앱 시각(`now`)으로 채우고, `id`는 DB 기본값(`uuidv7()`)이 만든다.
    pub fn save(
        &self,
        db: &mut impl Db,
        feedback: &mut Feedback,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let row = db.query_one(
            "INSERT INTO feedbacks (user_id, content, created_at) VALUES ($1, $2, $3) \
             RETURNING id, created_at",
            &[&feedback.user_id, &feedback.content, &now],
        )?;
        feedback.id = Some(row.try_get("id")?);
        feedback.created_at = Some(row.try_get("created_at")?);
        Ok(())
    }
}
