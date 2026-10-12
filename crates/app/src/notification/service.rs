//! 원본 `NotificationInboxService`와 결과 타입(`NotificationListResult`, `NotificationDetailResult`).

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Pool;
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::NotificationType;
use super::image_url::ImageUrlProvider;
use super::repository::NotificationRepository;
use crate::web::ApiError;

const NOT_FOUND_MESSAGE: &str = "알림을 찾을 수 없습니다.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationListResult {
    pub current_page: i32,
    pub page_size: i32,
    pub has_next: bool,
    pub notifications: Vec<NotificationSummaryResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSummaryResult {
    pub id: Uuid,
    pub kind: NotificationType,
    pub title: String,
    pub body: String,
    pub thumbnail_image_url: Option<String>,
    pub read_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationDetailResult {
    pub id: Uuid,
    pub kind: NotificationType,
    pub title: String,
    pub body: String,
    pub original_image_url: Option<String>,
    pub rejection_reason: Option<String>,
    pub read_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

#[derive(Clone)]
pub struct NotificationInboxService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    images: Arc<dyn ImageUrlProvider>,
    repository: NotificationRepository,
}

impl NotificationInboxService {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>, images: Arc<dyn ImageUrlProvider>) -> Self {
        NotificationInboxService {
            pool,
            clock,
            images,
            repository: NotificationRepository,
        }
    }

    /// `getNotifications`. `page`는 1부터, 두 값 모두 컨트롤러가 범위를 이미 확인했다.
    pub fn get_notifications(
        &self,
        user_id: Uuid,
        page: i32,
        page_size: i32,
    ) -> Result<NotificationListResult, ApiError> {
        // Spring Data JPA는 오프셋을 int로 줄여 쓰고, 넘으면 InvalidDataAccessApiUsageException(500)이다.
        let offset = i64::from(page - 1) * i64::from(page_size);
        if offset > i64::from(i32::MAX) {
            return Err(ApiError::Internal(
                "Page request offset exceeds Integer.MAX_VALUE".to_owned(),
            ));
        }
        let mut conn = self.pool.get()?;
        let slice =
            self.repository
                .find_by_user_id(&mut *conn, user_id, offset, i64::from(page_size))?;
        let mut notifications = Vec::with_capacity(slice.notifications.len());
        for summary in slice.notifications {
            let n = summary.notification;
            notifications.push(NotificationSummaryResult {
                id: n.id.expect("저장된 알림에는 식별자가 있다"),
                kind: n.kind,
                title: n.title,
                body: n.body,
                thumbnail_image_url: self.images.url(summary.thumbnail_storage_key.as_deref())?,
                read_at: n.read_at,
                created_at: n.created_at,
            });
        }
        Ok(NotificationListResult {
            current_page: page,
            page_size,
            has_next: slice.has_next,
            notifications,
        })
    }

    /// `getNotification`: 본인 알림만, 게시물이 삭제되지 않았을 때만. 읽음 처리는 하지 않는다.
    pub fn get_notification(
        &self,
        user_id: Uuid,
        notification_id: Uuid,
    ) -> Result<NotificationDetailResult, ApiError> {
        let mut conn = self.pool.get()?;
        let detail = self
            .repository
            .find_detail_by_id_and_user_id(&mut *conn, notification_id, user_id)?
            .ok_or_else(not_found)?;
        let n = detail.notification;
        Ok(NotificationDetailResult {
            id: n.id.expect("저장된 알림에는 식별자가 있다"),
            kind: n.kind,
            title: n.title,
            body: n.body,
            original_image_url: self.images.url(Some(&detail.original_storage_key))?,
            rejection_reason: n.rejection_reason,
            read_at: n.read_at,
            created_at: n.created_at,
        })
    }

    /// `hasUnreadNotification`
    pub fn has_unread_notification(&self, user_id: Uuid) -> Result<bool, ApiError> {
        let mut conn = self.pool.get()?;
        Ok(self
            .repository
            .exists_unread_by_user_id(&mut *conn, user_id)?)
    }

    /// `markRead`: 반복 호출해도 처음 읽은 시각을 유지한다. 없거나 남의 알림이면 404.
    pub fn mark_read(&self, user_id: Uuid, notification_id: Uuid) -> Result<(), ApiError> {
        let mut conn = self.pool.get()?;
        let updated =
            self.repository
                .mark_read(&mut *conn, notification_id, user_id, self.clock.now())?;
        if updated == 0 {
            return Err(not_found());
        }
        Ok(())
    }

    /// `markAllRead`
    pub fn mark_all_read(&self, user_id: Uuid) -> Result<(), ApiError> {
        let mut conn = self.pool.get()?;
        self.repository
            .mark_all_read(&mut *conn, user_id, self.clock.now())?;
        Ok(())
    }
}

fn not_found() -> ApiError {
    ApiError::App(AppError::not_found(
        ErrorCode::BUSINESS_ERROR,
        NOT_FOUND_MESSAGE,
    ))
}
