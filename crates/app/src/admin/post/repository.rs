//! 원본 `AdminPostQueryRepositoryImpl`·`AdminPostJpaRepository`와 검수·삭제가 쓰는 `PostRepository`·`PostLikeRepository`
//! 조각. posts·photos·post_likes·post_image_uploads를 직접 읽고 쓴다.

use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::Timestamp;
use jiff::civil::Date;
use serde_json::Value;
use uuid::Uuid;

use crate::admin::support::{AdminPage, SqlFilter, limit_offset};
use crate::admin::user::repository::UserStatus;

/// 게시물 검수 상태(`ModerationStatus`). 선언 순서가 JSON 서수다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModerationStatus {
    Validating,
    Pending,
    Approved,
    Rejected,
}

impl ModerationStatus {
    pub const ALL: [(&'static str, ModerationStatus); 4] = [
        ("VALIDATING", ModerationStatus::Validating),
        ("PENDING", ModerationStatus::Pending),
        ("APPROVED", ModerationStatus::Approved),
        ("REJECTED", ModerationStatus::Rejected),
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ModerationStatus::Validating => "VALIDATING",
            ModerationStatus::Pending => "PENDING",
            ModerationStatus::Approved => "APPROVED",
            ModerationStatus::Rejected => "REJECTED",
        }
    }

    pub fn parse(name: &str) -> Option<ModerationStatus> {
        ModerationStatus::ALL
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| *s)
    }

    /// `AdminPostListStatus`: 목록 필터로는 VALIDATING을 받지 않는다.
    pub fn parse_list_status(name: &str) -> Option<ModerationStatus> {
        ModerationStatus::parse(name).filter(|s| *s != ModerationStatus::Validating)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminPostSort {
    CreatedAtDesc,
    CreatedAtAsc,
}

impl AdminPostSort {
    pub fn from_query(value: &str) -> Option<AdminPostSort> {
        match value {
            "createdAtDesc" => Some(AdminPostSort::CreatedAtDesc),
            "createdAtAsc" => Some(AdminPostSort::CreatedAtAsc),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PostQueryCriteria {
    pub status: Option<ModerationStatus>,
    pub topic_id: Option<Uuid>,
    pub topic_date: Option<Date>,
    pub user_id: Option<Uuid>,
    pub created_at_from: Option<Timestamp>,
    pub created_at_to: Option<Timestamp>,
    pub sort: AdminPostSort,
}

#[derive(Clone, Debug)]
pub struct PostSummaryRow {
    pub post_id: Uuid,
    pub title: Option<String>,
    pub moderation_status: ModerationStatus,
    pub topic_id: Uuid,
    pub topic_title: String,
    pub topic_date: Date,
    pub author_id: Uuid,
    pub author_email: Option<String>,
    pub author_status: UserStatus,
    pub author_deleted_at: Option<Timestamp>,
    pub photo_id: Uuid,
    pub original_storage_key: String,
    pub thumbnail_storage_key: Option<String>,
    pub like_count: i64,
    pub created_at: Timestamp,
    pub moderated_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}

#[derive(Clone, Debug)]
pub struct PostDetailRow {
    pub post_id: Uuid,
    pub title: Option<String>,
    pub moderation_status: ModerationStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub moderated_at: Option<Timestamp>,
    pub moderated_by: Option<Uuid>,
    pub rejection_reason: Option<String>,
    pub deleted_at: Option<Timestamp>,
    pub author_id: Uuid,
    pub author_email: Option<String>,
    pub author_status: UserStatus,
    pub author_deleted_at: Option<Timestamp>,
    pub topic_id: Uuid,
    pub topic_title: String,
    pub topic_date: Date,
    pub topic_starts_at: Timestamp,
    pub topic_ends_at: Timestamp,
    pub topic_deleted_at: Option<Timestamp>,
    pub photo_id: Uuid,
    pub original_storage_key: String,
    pub thumbnail_storage_key: Option<String>,
    pub photo_metadata: Value,
    pub photo_created_at: Timestamp,
    pub photo_updated_at: Timestamp,
    pub photo_deleted_at: Option<Timestamp>,
    pub upload_id: Option<Uuid>,
    pub upload_status: Option<String>,
    pub upload_rejection_reason: Option<String>,
    pub upload_created_at: Option<Timestamp>,
    pub upload_updated_at: Option<Timestamp>,
    pub like_count: i64,
}

/// 검수·삭제 대상 게시물 행.
#[derive(Clone, Debug)]
pub struct PostForUpdate {
    pub id: Uuid,
    pub author_id: Uuid,
    pub moderation_status: ModerationStatus,
    pub moderated_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
    pub photo_id: Uuid,
    pub photo_deleted_at: Option<Timestamp>,
}

fn status_of(name: &str) -> Result<ModerationStatus, DbError> {
    ModerationStatus::parse(name)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 검수 상태: {name}")))
}

fn user_status_of(name: &str) -> Result<UserStatus, DbError> {
    UserStatus::parse(name).ok_or_else(|| DbError::Config(format!("알 수 없는 회원 상태: {name}")))
}

const LIKE_COUNT: &str = "(SELECT COUNT(*) FROM post_likes pl WHERE pl.post_id = p.id)";

#[derive(Clone, Copy, Debug, Default)]
pub struct AdminPostRepository;

impl AdminPostRepository {
    pub fn find_posts(
        &self,
        db: &mut impl Db,
        criteria: &PostQueryCriteria,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<PostSummaryRow>, DbError> {
        let mut filter = SqlFilter::new();
        filter.add_fixed("p.moderation_status <> 'VALIDATING'");
        filter.add_opt(
            "p.moderation_status = ?::text::moderation_status",
            criteria.status.map(|s| s.as_str().to_owned()),
        );
        filter.add_opt("t.id = ?", criteria.topic_id);
        filter.add_opt("t.topic_date = ?", criteria.topic_date);
        filter.add_opt("u.id = ?", criteria.user_id);
        filter.add_opt("p.created_at >= ?", criteria.created_at_from);
        filter.add_opt("p.created_at <= ?", criteria.created_at_to);
        let direction = match criteria.sort {
            AdminPostSort::CreatedAtDesc => "DESC",
            AdminPostSort::CreatedAtAsc => "ASC",
        };
        let (limit, offset) = limit_offset(page, page_size);
        let sql = format!(
            "SELECT p.id, p.title, p.moderation_status::text AS moderation_status, \
             t.id AS topic_id, t.title AS topic_title, t.topic_date, \
             u.id AS author_id, u.email, u.status::text AS author_status, u.deleted_at AS author_deleted_at, \
             ph.id AS photo_id, ph.original_storage_key, ph.thumbnail_storage_key, \
             {LIKE_COUNT} AS like_count, p.created_at, p.moderated_at, p.deleted_at \
             FROM posts p JOIN topics t ON t.id = p.topic_id JOIN users u ON u.id = p.user_id \
             JOIN photos ph ON ph.id = p.photo_id{} \
             ORDER BY p.created_at {direction}, p.id {direction} LIMIT ${} OFFSET ${}",
            filter.where_clause(),
            filter.next_placeholder(),
            filter.next_placeholder() + 1,
        );
        let params = filter.params_with(&[&limit, &offset]);
        let rows = db.query(&sql, &params)?;
        let items = rows
            .iter()
            .map(|row| {
                let status: String = row.try_get("moderation_status")?;
                let author_status: String = row.try_get("author_status")?;
                Ok(PostSummaryRow {
                    post_id: row.try_get("id")?,
                    title: row.try_get("title")?,
                    moderation_status: status_of(&status)?,
                    topic_id: row.try_get("topic_id")?,
                    topic_title: row.try_get("topic_title")?,
                    topic_date: row.try_get("topic_date")?,
                    author_id: row.try_get("author_id")?,
                    author_email: row.try_get("email")?,
                    author_status: user_status_of(&author_status)?,
                    author_deleted_at: row.try_get("author_deleted_at")?,
                    photo_id: row.try_get("photo_id")?,
                    original_storage_key: row.try_get("original_storage_key")?,
                    thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
                    like_count: row.try_get("like_count")?,
                    created_at: row.try_get("created_at")?,
                    moderated_at: row.try_get("moderated_at")?,
                    deleted_at: row.try_get("deleted_at")?,
                })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        Ok(AdminPage::from_overfetched(items, page, page_size))
    }

    pub fn find_post_by_id(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
    ) -> Result<Option<PostDetailRow>, DbError> {
        let sql = format!(
            "SELECT p.id, p.title, p.moderation_status::text AS moderation_status, \
             p.created_at, p.updated_at, p.moderated_at, \
             (SELECT a.actor_admin_id FROM admin_audit_logs a WHERE a.target_type = 'POST' AND a.target_id = p.id \
                AND a.action IN ('POST_APPROVED', 'POST_REJECTED') ORDER BY a.occurred_at, a.id LIMIT 1) AS moderated_by, \
             (SELECT a.reason FROM admin_audit_logs a WHERE a.target_type = 'POST' AND a.target_id = p.id \
                AND a.action IN ('POST_APPROVED', 'POST_REJECTED') ORDER BY a.occurred_at, a.id LIMIT 1) AS rejection_reason, \
             p.deleted_at, \
             u.id AS author_id, u.email, u.status::text AS author_status, u.deleted_at AS author_deleted_at, \
             t.id AS topic_id, t.title AS topic_title, t.topic_date, t.starts_at, t.ends_at, t.deleted_at AS topic_deleted_at, \
             ph.id AS photo_id, ph.original_storage_key, ph.thumbnail_storage_key, ph.metadata::text AS metadata, \
             ph.created_at AS photo_created_at, ph.updated_at AS photo_updated_at, ph.deleted_at AS photo_deleted_at, \
             up.id AS upload_id, up.status::text AS upload_status, up.rejection_reason AS upload_rejection_reason, \
             up.created_at AS upload_created_at, up.updated_at AS upload_updated_at, \
             {LIKE_COUNT} AS like_count \
             FROM posts p JOIN topics t ON t.id = p.topic_id JOIN users u ON u.id = p.user_id \
             JOIN photos ph ON ph.id = p.photo_id \
             LEFT JOIN post_image_uploads up ON up.id = p.post_image_upload_id \
             WHERE p.id = $1 AND p.moderation_status <> 'VALIDATING' LIMIT 1"
        );
        let Some(row) = db.query_opt(&sql, &[&post_id])? else {
            return Ok(None);
        };
        let status: String = row.try_get("moderation_status")?;
        let author_status: String = row.try_get("author_status")?;
        let metadata: String = row.try_get("metadata")?;
        Ok(Some(PostDetailRow {
            post_id: row.try_get("id")?,
            title: row.try_get("title")?,
            moderation_status: status_of(&status)?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            moderated_at: row.try_get("moderated_at")?,
            moderated_by: row.try_get("moderated_by")?,
            rejection_reason: row.try_get("rejection_reason")?,
            deleted_at: row.try_get("deleted_at")?,
            author_id: row.try_get("author_id")?,
            author_email: row.try_get("email")?,
            author_status: user_status_of(&author_status)?,
            author_deleted_at: row.try_get("author_deleted_at")?,
            topic_id: row.try_get("topic_id")?,
            topic_title: row.try_get("topic_title")?,
            topic_date: row.try_get("topic_date")?,
            topic_starts_at: row.try_get("starts_at")?,
            topic_ends_at: row.try_get("ends_at")?,
            topic_deleted_at: row.try_get("topic_deleted_at")?,
            photo_id: row.try_get("photo_id")?,
            original_storage_key: row.try_get("original_storage_key")?,
            thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
            photo_metadata: serde_json::from_str(&metadata)
                .map_err(|e| DbError::Config(e.to_string()))?,
            photo_created_at: row.try_get("photo_created_at")?,
            photo_updated_at: row.try_get("photo_updated_at")?,
            photo_deleted_at: row.try_get("photo_deleted_at")?,
            upload_id: row.try_get("upload_id")?,
            upload_status: row.try_get("upload_status")?,
            upload_rejection_reason: row.try_get("upload_rejection_reason")?,
            upload_created_at: row.try_get("upload_created_at")?,
            upload_updated_at: row.try_get("upload_updated_at")?,
            like_count: row.try_get("like_count")?,
        }))
    }

    /// `findActiveByIdForUpdate` / `findByIdForUpdate`. 삭제된 게시물은 `include_deleted`일 때만 돌려준다.
    pub fn find_for_update(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
        include_deleted: bool,
    ) -> Result<Option<PostForUpdate>, DbError> {
        let deleted_filter = if include_deleted {
            ""
        } else {
            " AND p.deleted_at IS NULL"
        };
        let sql = format!(
            "SELECT p.id, p.user_id, p.moderation_status::text AS moderation_status, p.moderated_at, \
             p.created_at, p.deleted_at, ph.id AS photo_id, ph.deleted_at AS photo_deleted_at \
             FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = $1{deleted_filter} \
             FOR NO KEY UPDATE OF p"
        );
        let Some(row) = db.query_opt(&sql, &[&post_id])? else {
            return Ok(None);
        };
        let status: String = row.try_get("moderation_status")?;
        Ok(Some(PostForUpdate {
            id: row.try_get("id")?,
            author_id: row.try_get("user_id")?,
            moderation_status: status_of(&status)?,
            moderated_at: row.try_get("moderated_at")?,
            created_at: row.try_get("created_at")?,
            deleted_at: row.try_get("deleted_at")?,
            photo_id: row.try_get("photo_id")?,
            photo_deleted_at: row.try_get("photo_deleted_at")?,
        }))
    }

    /// 검수 결정을 저장한다(`@UpdateTimestamp`로 `updated_at`도 갱신).
    pub fn save_moderation(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
        status: ModerationStatus,
        moderated_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE posts SET moderation_status = $2::text::moderation_status, moderated_at = $3, updated_at = $4 \
             WHERE id = $1",
            &[&post_id, &status.as_str(), &moderated_at, &now],
        )?;
        Ok(())
    }

    /// 관리자 삭제: 게시물과 사진을 같은 시각으로 삭제 표시한다.
    pub fn save_deletion(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
        photo_id: Uuid,
        deleted_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE posts SET deleted_at = $2, updated_at = $3 WHERE id = $1",
            &[&post_id, &deleted_at, &now],
        )?;
        self.mark_photo_deleted(db, photo_id, deleted_at, now)
    }

    /// `Photo.delete`: 이미 삭제된 사진은 건드리지 않는다.
    pub fn mark_photo_deleted(
        &self,
        db: &mut impl Db,
        photo_id: Uuid,
        deleted_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE photos SET deleted_at = $2, updated_at = $3 WHERE id = $1 AND deleted_at IS NULL",
            &[&photo_id, &deleted_at, &now],
        )?;
        Ok(())
    }

    /// `PostLikeRepository.deleteByPostId`
    pub fn delete_likes(&self, db: &mut impl Db, post_id: Uuid) -> Result<u64, DbError> {
        Ok(db.execute("DELETE FROM post_likes WHERE post_id = $1", &[&post_id])?)
    }
}
