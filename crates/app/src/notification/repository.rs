//! 알림 저장소. 원본 `NotificationRepositoryImpl`과 `NotificationJpaRepository`의 JPQL을 손으로 쓴 SQL로 옮겼다.
//!
//! 알림함 조회는 모두 게시물이 삭제되지 않은(`posts.deleted_at IS NULL`) 알림만 대상으로 한다. 게시물이 삭제돼도 알림 행은
//! 남기고(보관 기간이 지나면 정리 쿼리가 지운다) 화면에서만 가린다. 연결(또는 트랜잭션)은 호출 측이 넘긴다.

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::{GenericClient, Row};
use uuid::Uuid;

use super::domain::{Notification, NotificationType};

const NOTIFICATION_COLUMNS: &str = "n.id, n.user_id, n.post_id, n.event_key, n.type, n.title, n.body, \
     n.rejection_reason, n.read_at, n.created_at";

/// 목록의 한 줄: 알림과 사진 썸네일의 스토리지 키(`NotificationSummary`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSummary {
    pub notification: Notification,
    pub thumbnail_storage_key: Option<String>,
}

/// 상세: 알림과 사진 원본의 스토리지 키(`NotificationDetail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationDetail {
    pub notification: Notification,
    pub original_storage_key: String,
}

/// 목록 한 쪽(`NotificationSlice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSlice {
    pub notifications: Vec<NotificationSummary>,
    pub has_next: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NotificationRepository;

fn from_row(row: &Row) -> Result<Notification, DbError> {
    let kind: String = row.try_get("type")?;
    Ok(Notification {
        id: Some(row.try_get("id")?),
        user_id: row.try_get("user_id")?,
        post_id: row.try_get("post_id")?,
        event_key: row.try_get("event_key")?,
        // `ck_notifications_type` 제약이 두 값만 허용한다.
        kind: NotificationType::parse(&kind)
            .ok_or_else(|| DbError::Config(format!("알 수 없는 알림 종류: {kind}")))?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        rejection_reason: row.try_get("rejection_reason")?,
        read_at: row.try_get("read_at")?,
        created_at: row.try_get("created_at")?,
    })
}

impl NotificationRepository {
    /// `save`: INSERT 하고 DB가 만든 `id`를 채운다.
    pub fn save(
        &self,
        db: &mut impl GenericClient,
        notification: &mut Notification,
    ) -> Result<(), DbError> {
        let row = db.query_one(
            "INSERT INTO notifications \
             (user_id, post_id, event_key, type, title, body, rejection_reason, read_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
            &[
                &notification.user_id,
                &notification.post_id,
                &notification.event_key,
                &notification.kind.as_str(),
                &notification.title,
                &notification.body,
                &notification.rejection_reason,
                &notification.read_at,
                &notification.created_at,
            ],
        )?;
        notification.id = Some(row.try_get("id")?);
        Ok(())
    }

    /// `findByUserId`: 최신순(`created_at DESC, id DESC`)으로 `offset`부터 `page_size`건. 한 건을 더 읽어 다음 쪽 여부를 안다
    /// (Spring Data `Slice`).
    pub fn find_by_user_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
        offset: i64,
        page_size: i64,
    ) -> Result<NotificationSlice, DbError> {
        let sql = format!(
            "SELECT {NOTIFICATION_COLUMNS}, photo.thumbnail_storage_key \
             FROM notifications n \
             JOIN posts p ON p.id = n.post_id \
             JOIN photos photo ON photo.id = p.photo_id \
             WHERE n.user_id = $1 AND p.deleted_at IS NULL \
             ORDER BY n.created_at DESC, n.id DESC \
             LIMIT $2 OFFSET $3"
        );
        let rows = db.query(&sql, &[&user_id, &(page_size + 1), &offset])?;
        let has_next = rows.len() as i64 > page_size;
        let notifications = rows
            .iter()
            .take(page_size as usize)
            .map(|row| {
                Ok(NotificationSummary {
                    notification: from_row(row)?,
                    thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
                })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        Ok(NotificationSlice {
            notifications,
            has_next,
        })
    }

    /// `findDetailByIdAndUserId`
    pub fn find_detail_by_id_and_user_id(
        &self,
        db: &mut impl GenericClient,
        notification_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<NotificationDetail>, DbError> {
        let sql = format!(
            "SELECT {NOTIFICATION_COLUMNS}, photo.original_storage_key \
             FROM notifications n \
             JOIN posts p ON p.id = n.post_id \
             JOIN photos photo ON photo.id = p.photo_id \
             WHERE n.id = $1 AND n.user_id = $2 AND p.deleted_at IS NULL"
        );
        db.query_opt(&sql, &[&notification_id, &user_id])?
            .as_ref()
            .map(|row| {
                Ok(NotificationDetail {
                    notification: from_row(row)?,
                    original_storage_key: row.try_get("original_storage_key")?,
                })
            })
            .transpose()
    }

    /// `existsUnreadByUserId`
    pub fn exists_unread_by_user_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
    ) -> Result<bool, DbError> {
        let row = db.query_one(
            "SELECT EXISTS (SELECT 1 FROM notifications n \
             JOIN posts p ON p.id = n.post_id \
             WHERE n.user_id = $1 AND n.read_at IS NULL AND p.deleted_at IS NULL)",
            &[&user_id],
        )?;
        Ok(row.try_get(0)?)
    }

    /// `markRead`: 이미 읽은 알림은 처음 읽은 시각을 유지한다. 영향받은 행 수를 돌려준다.
    pub fn mark_read(
        &self,
        db: &mut impl GenericClient,
        notification_id: Uuid,
        user_id: Uuid,
        read_at: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "UPDATE notifications n SET read_at = coalesce(n.read_at, $3) \
             WHERE n.id = $1 AND n.user_id = $2 \
             AND EXISTS (SELECT 1 FROM posts p WHERE p.id = n.post_id AND p.deleted_at IS NULL)",
            &[&notification_id, &user_id, &read_at],
        )?)
    }

    /// `markAllRead`
    pub fn mark_all_read(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
        read_at: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "UPDATE notifications n SET read_at = $2 \
             WHERE n.user_id = $1 AND n.read_at IS NULL \
             AND EXISTS (SELECT 1 FROM posts p WHERE p.id = n.post_id AND p.deleted_at IS NULL)",
            &[&user_id, &read_at],
        )?)
    }

    /// `deleteExpiredForActiveUsers`: 탈퇴하지 않은 회원의 알림 중 `created_before`보다 먼저 만들어진 것.
    pub fn delete_expired_for_active_users(
        &self,
        db: &mut impl GenericClient,
        created_before: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM notifications n WHERE n.created_at < $1 \
             AND n.user_id IN (SELECT u.id FROM users u WHERE u.deleted_at IS NULL)",
            &[&created_before],
        )?)
    }

    /// `deleteExpiredForWithdrawnUsers`: `withdrawn_before`보다 먼저 탈퇴한 회원의 알림 전부.
    pub fn delete_expired_for_withdrawn_users(
        &self,
        db: &mut impl GenericClient,
        withdrawn_before: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM notifications n \
             WHERE n.user_id IN (SELECT u.id FROM users u WHERE u.deleted_at < $1)",
            &[&withdrawn_before],
        )?)
    }
}
