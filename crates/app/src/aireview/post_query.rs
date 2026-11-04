//! 리뷰 대상 게시물 조회(원본 `PostJpaRepository.findVisibleById`의 AI 리뷰가 쓰는 열).
//!
//! 조건을 바꾸면 관리자 AI 리뷰 조회의 `visibleToAuthor`([`crate::admin::aireview`])도 함께 고친다.

use chalkak_db::{Db, DbError};
use uuid::Uuid;

/// 승인됐고 게시물·주제·사진·작성자가 모두 삭제되지 않은 게시물에서 AI 리뷰가 필요로 하는 값.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisiblePost {
    pub post_id: Uuid,
    pub author_id: Uuid,
    pub topic_title: String,
    pub original_storage_key: String,
}

const VISIBLE_SQL: &str = "SELECT p.id, p.user_id, t.title, ph.original_storage_key \
     FROM posts p \
     JOIN topics t ON t.id = p.topic_id \
     JOIN photos ph ON ph.id = p.photo_id \
     JOIN users u ON u.id = p.user_id \
     WHERE p.id = $1 AND p.moderation_status = 'APPROVED' \
       AND p.deleted_at IS NULL AND t.deleted_at IS NULL \
       AND ph.deleted_at IS NULL AND u.deleted_at IS NULL";

/// `findVisibleById`
pub fn find_visible_post(db: &mut impl Db, post_id: Uuid) -> Result<Option<VisiblePost>, DbError> {
    let row = db.query_opt(VISIBLE_SQL, &[&post_id])?;
    row.map(|row| {
        Ok(VisiblePost {
            post_id: row.try_get("id")?,
            author_id: row.try_get("user_id")?,
            topic_title: row.try_get("title")?,
            original_storage_key: row.try_get("original_storage_key")?,
        })
    })
    .transpose()
}
