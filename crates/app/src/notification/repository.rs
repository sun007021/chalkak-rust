//! 알림 저장소. 원본 `NotificationRepositoryImpl`과 `NotificationJpaRepository`의 JPQL을 손으로 쓴 SQL로 옮겼다.
//!
//! 알림함 조회는 모두 게시물이 삭제되지 않은(`posts.deleted_at IS NULL`) 알림만 대상으로 한다. 게시물이 삭제돼도 알림 행은
//! 남기고(보관 기간이 지나면 정리 쿼리가 지운다) 화면에서만 가린다. 연결(또는 트랜잭션)은 호출 측이 넘긴다.

use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::Row;
use uuid::Uuid;

use super::domain::{Notification, NotificationSourceType, NotificationType, PublishStatus};

const NOTIFICATION_COLUMNS: &str = "n.id, n.user_id, n.source_type, n.source_id, n.event_key, n.type, \
     n.title, n.body, n.payload ->> 'rejectionReason' AS rejection_reason, n.read_at, n.created_at, \
     n.sqs_publish_status, n.next_attempt_at, n.sqs_published_at";

/// 알림함에 보이는 알림의 조건(`findSummariesByUserId` 등의 JPQL). 대상이 게시물이면 삭제되지 않은 게시물이어야 하고, 공지면
/// 삭제되지 않은 공지여야 한다. 대상이 없거나 주제면 항상 보인다. `p`는 게시물, `nt`는 공지를 `source_id`로 잇는다.
const VISIBLE_SOURCE: &str = "(n.source_type IS NULL OR n.source_type <> 'POST' \
     OR (p.id IS NOT NULL AND p.deleted_at IS NULL)) \
     AND (n.source_type IS NULL OR n.source_type <> 'NOTICE' \
     OR (nt.id IS NOT NULL AND nt.deleted_at IS NULL))";

/// 위 조건이 쓰는 조인.
const SOURCE_JOINS: &str = "LEFT JOIN posts p ON n.source_type = 'POST' AND p.id = n.source_id \
     LEFT JOIN photos photo ON photo.id = p.photo_id \
     LEFT JOIN notices nt ON n.source_type = 'NOTICE' AND nt.id = n.source_id";

/// `markRead`/`markAllRead`가 같은 조건을 EXISTS로 거는 문장 조각(UPDATE는 조인을 쓰지 않는다).
const VISIBLE_SOURCE_EXISTS: &str = "(n.source_type IS NULL OR n.source_type <> 'POST' \
     OR EXISTS (SELECT 1 FROM posts p WHERE p.id = n.source_id AND p.deleted_at IS NULL)) \
     AND (n.source_type IS NULL OR n.source_type <> 'NOTICE' \
     OR EXISTS (SELECT 1 FROM notices nt WHERE nt.id = n.source_id AND nt.deleted_at IS NULL))";

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
    /// 게시물 알림이 아니면 사진이 없다.
    pub original_storage_key: Option<String>,
}

/// 목록 한 쪽(`NotificationSlice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSlice {
    pub notifications: Vec<NotificationSummary>,
    pub has_next: bool,
}

/// 발행 대기 현황(`PublicationBacklog`): `PENDING` 건수와 그중 가장 오래된 생성 시각.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationBacklog {
    pub pending_count: i64,
    pub oldest_created_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NotificationRepository;

fn from_row(row: &Row) -> Result<Notification, DbError> {
    let kind: String = row.try_get("type")?;
    let source_type: Option<String> = row.try_get("source_type")?;
    let publish_status: String = row.try_get("sqs_publish_status")?;
    Ok(Notification {
        id: Some(row.try_get("id")?),
        user_id: row.try_get("user_id")?,
        source_type: source_type
            .map(|t| {
                NotificationSourceType::parse(&t)
                    .ok_or_else(|| DbError::Config(format!("알 수 없는 알림 대상 종류: {t}")))
            })
            .transpose()?,
        source_id: row.try_get("source_id")?,
        event_key: row.try_get("event_key")?,
        // 종류는 열거형 이름이라 알 수 없는 값은 원본도 읽다가 실패한다.
        kind: NotificationType::parse(&kind)
            .ok_or_else(|| DbError::Config(format!("알 수 없는 알림 종류: {kind}")))?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        rejection_reason: row.try_get("rejection_reason")?,
        read_at: row.try_get("read_at")?,
        created_at: row.try_get("created_at")?,
        publish_status: PublishStatus::parse(&publish_status)
            .ok_or_else(|| DbError::Config(format!("알 수 없는 발행 상태: {publish_status}")))?,
        next_attempt_at: row.try_get("next_attempt_at")?,
        published_at: row.try_get("sqs_published_at")?,
    })
}

impl NotificationRepository {
    /// `save`: INSERT 하고 DB가 만든 `id`를 채운다.
    pub fn save(&self, db: &mut impl Db, notification: &mut Notification) -> Result<(), DbError> {
        // 반려 사유는 `NotificationPayload` 레코드(`{"rejectionReason": ...}`)로 저장한다.
        let payload = notification
            .rejection_reason
            .as_ref()
            .map(|reason| serde_json::json!({ "rejectionReason": reason }).to_string());
        let row = db.query_one(
            "INSERT INTO notifications \
             (user_id, source_type, source_id, event_key, type, title, body, payload, read_at, \
              created_at, sqs_publish_status, next_attempt_at, sqs_published_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::jsonb, $9, $10, $11, $12, $13) RETURNING id",
            &[
                &notification.user_id,
                &notification.source_type.map(NotificationSourceType::as_str),
                &notification.source_id,
                &notification.event_key,
                &notification.kind.as_str(),
                &notification.title,
                &notification.body,
                &payload,
                &notification.read_at,
                &notification.created_at,
                &notification.publish_status.as_str(),
                &notification.next_attempt_at,
                &notification.published_at,
            ],
        )?;
        notification.id = Some(row.try_get("id")?);
        Ok(())
    }

    /// `findByUserId`: 최신순(`created_at DESC, id DESC`)으로 `offset`부터 `page_size`건. 한 건을 더 읽어 다음 쪽 여부를 안다
    /// (Spring Data `Slice`). `created_from` 이전에 생긴 알림은 보이지 않는다.
    pub fn find_by_user_id(
        &self,
        db: &mut impl Db,
        user_id: Uuid,
        offset: i64,
        page_size: i64,
        created_from: Timestamp,
    ) -> Result<NotificationSlice, DbError> {
        let sql = format!(
            "SELECT {NOTIFICATION_COLUMNS}, photo.thumbnail_storage_key \
             FROM notifications n {SOURCE_JOINS} \
             WHERE n.user_id = $1 AND n.created_at >= $4 AND {VISIBLE_SOURCE} \
             ORDER BY n.created_at DESC, n.id DESC \
             LIMIT $2 OFFSET $3"
        );
        let rows = db.query(&sql, &[&user_id, &(page_size + 1), &offset, &created_from])?;
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
        db: &mut impl Db,
        notification_id: Uuid,
        user_id: Uuid,
        created_from: Timestamp,
    ) -> Result<Option<NotificationDetail>, DbError> {
        let sql = format!(
            "SELECT {NOTIFICATION_COLUMNS}, photo.original_storage_key \
             FROM notifications n {SOURCE_JOINS} \
             WHERE n.id = $1 AND n.user_id = $2 AND n.created_at >= $3 AND {VISIBLE_SOURCE}"
        );
        db.query_opt(&sql, &[&notification_id, &user_id, &created_from])?
            .as_ref()
            .map(|row| {
                Ok(NotificationDetail {
                    notification: from_row(row)?,
                    original_storage_key: row.try_get("original_storage_key")?,
                })
            })
            .transpose()
    }

    /// `existsVisibleUnreadByUserId`
    pub fn exists_unread_by_user_id(
        &self,
        db: &mut impl Db,
        user_id: Uuid,
        created_from: Timestamp,
    ) -> Result<bool, DbError> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM notifications n {SOURCE_JOINS} \
             WHERE n.user_id = $1 AND n.read_at IS NULL AND n.created_at >= $2 AND {VISIBLE_SOURCE})"
        );
        let row = db.query_one(&sql, &[&user_id, &created_from])?;
        Ok(row.try_get(0)?)
    }

    /// `markRead`: 이미 읽은 알림은 처음 읽은 시각을 유지한다. 영향받은 행 수를 돌려준다.
    pub fn mark_read(
        &self,
        db: &mut impl Db,
        notification_id: Uuid,
        user_id: Uuid,
        read_at: Timestamp,
        created_from: Timestamp,
    ) -> Result<u64, DbError> {
        let sql = format!(
            "UPDATE notifications n SET read_at = coalesce(n.read_at, $3) \
             WHERE n.id = $1 AND n.user_id = $2 AND n.created_at >= $4 AND {VISIBLE_SOURCE_EXISTS}"
        );
        Ok(db.execute(&sql, &[&notification_id, &user_id, &read_at, &created_from])?)
    }

    /// `markAllRead`
    pub fn mark_all_read(
        &self,
        db: &mut impl Db,
        user_id: Uuid,
        read_at: Timestamp,
        created_from: Timestamp,
    ) -> Result<u64, DbError> {
        let sql = format!(
            "UPDATE notifications n SET read_at = $2 \
             WHERE n.user_id = $1 AND n.read_at IS NULL AND n.created_at >= $3 \
             AND {VISIBLE_SOURCE_EXISTS}"
        );
        Ok(db.execute(&sql, &[&user_id, &read_at, &created_from])?)
    }

    /// `deleteExpiredForActiveUsers`: 탈퇴하지 않은 회원의 알림 중 `created_before`보다 먼저 만들어진 것.
    pub fn delete_expired_for_active_users(
        &self,
        db: &mut impl Db,
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
        db: &mut impl Db,
        withdrawn_before: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM notifications n \
             WHERE n.user_id IN (SELECT u.id FROM users u WHERE u.deleted_at < $1)",
            &[&withdrawn_before],
        )?)
    }

    /// `getPublicationBacklog`. 호출 측이 제한 시간을 건 트랜잭션 안에서 부른다.
    pub fn get_publication_backlog(&self, db: &mut impl Db) -> Result<PublicationBacklog, DbError> {
        let row = db.query_one(
            "SELECT COUNT(*), MIN(created_at) FROM notifications WHERE sqs_publish_status = 'PENDING'",
            &[],
        )?;
        Ok(PublicationBacklog {
            pending_count: row.try_get(0)?,
            oldest_created_at: row.try_get(1)?,
        })
    }

    /// `findTopicPushRecipientIds`: 탈퇴하지 않은 ACTIVE 회원 중 주제 푸시를 받고 그 주제에 반려되지 않은 글이 없는 회원
    /// (참여한 회원은 제외). 회원 ID 순.
    pub fn find_topic_push_recipient_ids(
        &self,
        db: &mut impl Db,
        topic_id: Uuid,
    ) -> Result<Vec<Uuid>, DbError> {
        let rows = db.query(
            "SELECT u.id FROM users u \
             WHERE u.deleted_at IS NULL AND u.status = 'ACTIVE'::user_status AND u.topic_push_enabled = true \
             AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id AND p.topic_id = $1 \
                 AND p.deleted_at IS NULL AND p.moderation_status <> 'REJECTED') \
             ORDER BY u.id",
            &[&topic_id],
        )?;
        rows.iter()
            .map(|row| row.try_get::<_, Uuid>(0).map_err(DbError::from))
            .collect()
    }

    /// `findDuePublicationIds`: 지금 발행할 차례인 알림을 다음 시도 시각 순으로 `limit`건.
    pub fn find_due_publication_ids(
        &self,
        db: &mut impl Db,
        now: Timestamp,
        limit: i64,
    ) -> Result<Vec<Uuid>, DbError> {
        let rows = db.query(
            "SELECT id FROM notifications WHERE sqs_publish_status = 'PENDING' AND next_attempt_at <= $1 \
             ORDER BY next_attempt_at, id LIMIT $2",
            &[&now, &limit],
        )?;
        rows.iter()
            .map(|row| row.try_get::<_, Uuid>(0).map_err(DbError::from))
            .collect()
    }

    /// `findPendingPublicationForUpdate`: 발행할 차례인 알림 하나를 잠그고 읽는다. 다른 트랜잭션이 잠근 행은 기다리지 않고
    /// 건너뛴다(`FOR UPDATE SKIP LOCKED`).
    pub fn find_pending_publication_for_update(
        &self,
        db: &mut impl Db,
        notification_id: Uuid,
        now: Timestamp,
    ) -> Result<Option<Notification>, DbError> {
        let sql = format!(
            "SELECT {NOTIFICATION_COLUMNS} FROM notifications n \
             WHERE n.id = $1 AND n.sqs_publish_status = 'PENDING' AND n.next_attempt_at <= $2 \
             FOR UPDATE OF n SKIP LOCKED"
        );
        db.query_opt(&sql, &[&notification_id, &now])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// 발행 결과 저장(JPA 더티 체킹이 하던 UPDATE): 상태, 다음 시도 시각, 발행 시각.
    pub fn save_publication(
        &self,
        db: &mut impl Db,
        notification: &Notification,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE notifications SET sqs_publish_status = $2, next_attempt_at = $3, sqs_published_at = $4 \
             WHERE id = $1",
            &[
                &notification.id,
                &notification.publish_status.as_str(),
                &notification.next_attempt_at,
                &notification.published_at,
            ],
        )?;
        Ok(())
    }
}
