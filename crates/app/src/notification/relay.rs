//! 알림 Relay(원본 `NotificationRelayService`). 커밋된 발행 대기 알림을 SQS로 발행하고 결과를 DB에 반영한다.
//! 언제 부를지는 [`crate::jobs`]가 정한다(`NotificationRelayScheduler`, 앞 실행이 끝난 뒤 1초).
//!
//! 한 번에 최대 100건을 읽고 알림마다 별도 트랜잭션에서 순서대로 처리한다. 트랜잭션은 알림 행을 잠근 채(다른 Relay가 잠근
//! 행은 건너뛴다) SQS를 호출하므로, SQS 수락 뒤 DB 반영이 실패하면 롤백되어 다음 실행에서 같은 사건을 다시 발행한다.

use std::cell::RefCell;
use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::Pool;
use uuid::Uuid;

use super::domain::Notification;
use super::repository::NotificationRepository;
use super::sqs_push::{PushMessage, PushMessagePublisher, PushPublicationResult};
use crate::common::transact;
use crate::web::ApiError;
use crate::web::json::format_instant;

const BATCH_SIZE: i64 = 100;

pub(super) const RELAY_LOGGER: &str =
    "com.chalkak.backend.notification.service.NotificationRelayService";
const TYPE_NOTIFICATION: &str = "notification";

#[derive(Clone)]
pub struct NotificationRelayService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    publisher: Arc<dyn PushMessagePublisher>,
    logger: Arc<Logger>,
}

impl NotificationRelayService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        publisher: Arc<dyn PushMessagePublisher>,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            pool,
            clock,
            publisher,
            logger,
        }
    }

    /// `publishPendingNotifications`. 대상 조회가 실패하면 오류를 돌려준다(알림별 실패는 로그만 남기고 다음 알림으로 간다).
    pub fn publish_pending_notifications(&self) -> Result<(), ApiError> {
        let ids = {
            let mut conn = self.pool.get()?;
            NotificationRepository.find_due_publication_ids(
                &mut conn,
                self.clock.now(),
                BATCH_SIZE,
            )?
        };
        for notification_id in ids {
            // 발행기가 결과를 못 만들고 실패하면 그 예외 이름을 오류 코드로 남긴다.
            let publisher_failure = RefCell::new(None);
            match transact(&self.pool, |tx| {
                self.publish(tx, notification_id, &publisher_failure)
            }) {
                Ok(true) => self
                    .logger
                    .info(RELAY_LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", "relay_transaction_committed")
                    .field("notificationId", notification_id.to_string())
                    .log("알림 발행 처리 트랜잭션 완료"),
                Ok(false) => {}
                Err(error) => {
                    // SQS 수락 뒤 DB 반영 실패도 여기에 포함된다. 원문/스택에 메시지·자격증명을 남기지 않는다.
                    let code = publisher_failure
                        .into_inner()
                        .unwrap_or_else(|| error.exception_name().to_owned());
                    self.logger
                        .error(RELAY_LOGGER)
                        .field("type", TYPE_NOTIFICATION)
                        .field("stage", "relay_transaction_failed")
                        .field("notificationId", notification_id.to_string())
                        .field("errorCode", code)
                        .log("알림 발행 처리 트랜잭션 실패");
                }
            }
        }
        Ok(())
    }

    /// 알림 하나를 잠그고 발행해 결과를 저장한다. 처리할 알림이 없으면(이미 처리됨·다른 Relay가 잠금) `false`.
    fn publish(
        &self,
        tx: &mut chalkak_db::Tx<'_>,
        notification_id: Uuid,
        publisher_failure: &RefCell<Option<String>>,
    ) -> Result<bool, ApiError> {
        let repository = NotificationRepository;
        let now = self.clock.now();
        let Some(mut notification) =
            repository.find_pending_publication_for_update(tx, notification_id, now)?
        else {
            return Ok(false);
        };
        if notification.is_push_expired(now) {
            notification.expire_publication()?;
            self.log_result(&notification, "EXPIRED", None, None);
            repository.save_publication(tx, &notification)?;
            return Ok(true);
        }
        let message = push_message(&notification);
        self.log_result(&notification, "REQUESTED", None, None);
        let result = self.publisher.publish(&message).map_err(|failure| {
            *publisher_failure.borrow_mut() = Some(failure.exception);
            ApiError::Internal("알림 발행기가 결과를 만들지 못했습니다".to_owned())
        })?;
        self.apply_result(&mut notification, &result)?;
        repository.save_publication(tx, &notification)?;
        Ok(true)
    }

    fn apply_result(
        &self,
        notification: &mut Notification,
        result: &PushPublicationResult,
    ) -> Result<(), ApiError> {
        let attempted_at = self.clock.now();
        if result.is_accepted() {
            notification.mark_published(attempted_at)?;
            self.log_result(
                notification,
                "SQS_ACCEPTED",
                result.message_id.as_deref(),
                None,
            );
            return Ok(());
        }
        if result.retryable {
            notification.retry_publication(attempted_at)?;
            let outcome = if result.is_acceptance_unknown() {
                "SQS_UNKNOWN"
            } else {
                "SQS_FAILED"
            };
            self.log_result(notification, outcome, None, result.error_code.as_deref());
            return Ok(());
        }
        notification.fail_publication()?;
        self.log_result(notification, "FAILED", None, result.error_code.as_deref());
        Ok(())
    }

    fn log_result(
        &self,
        notification: &Notification,
        result: &str,
        message_id: Option<&str>,
        error_code: Option<&str>,
    ) {
        // 오류 코드가 있으면 WARN, 없으면 INFO
        let event = if error_code.is_some() {
            self.logger.warn(RELAY_LOGGER)
        } else {
            self.logger.info(RELAY_LOGGER)
        };
        event
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "relay")
            .field("eventId", notification.event_key.to_string())
            .field("notificationId", notification.id.map(|id| id.to_string()))
            .field("userId", notification.user_id.to_string())
            .field("notificationType", notification.kind.as_str())
            .field("sourceId", notification.source_id.map(|id| id.to_string()))
            .field("occurredAt", format_instant(notification.created_at))
            .field("expiresAt", format_instant(notification.push_expires_at()))
            .field("publishStatus", notification.publish_status.as_str())
            .field("sqsMessageId", message_id)
            .field("result", result)
            .field("errorCode", error_code)
            .field(
                "nextAttemptAt",
                notification.next_attempt_at.map(format_instant),
            )
            .log("알림 SQS 발행 처리");
    }
}

/// 알림에서 SQS로 보낼 메시지를 만든다(`createPushMessage`). 사건 ID는 알림의 사건 키다.
fn push_message(notification: &Notification) -> PushMessage {
    PushMessage {
        event_id: notification.event_key,
        notification_id: notification.id,
        user_id: notification.user_id,
        kind: notification.kind,
        source_type: notification.source_type,
        source_id: notification.source_id,
        occurred_at: notification.created_at,
        expires_at: notification.push_expires_at(),
    }
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
