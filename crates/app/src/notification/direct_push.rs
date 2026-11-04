//! 알림함 없이 SQS로 직접 보내는 푸시의 공통 발행(원본 `DirectPushPublisher`, `DirectPushOccurrence`,
//! `DirectPublicationSummary`). 주제 푸시와 리마인더가 함께 쓴다. 한 회차의 회원별 메시지를 차례로 발행하고, 실패·결과 불명도
//! 재발행하지 않는다(Best Effort).

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::{Level, Logger};
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::{
    NotificationSourceType, NotificationType, is_push_expired_at, push_expires_at,
};
use super::sqs_push::{PushMessage, PushMessagePublisher, PushPublicationResult};
use crate::web::json::format_instant;

/// 원본 `@Slf4j` 로거 이름.
const LOGGER: &str = "com.chalkak.backend.notification.service.DirectPushPublisher";
const TYPE_NOTIFICATION: &str = "notification";

/// 알림함 없이 SQS로 직접 보내는 한 회차의 공통 값이다. 로그 단계 이름은 접두사로 구분한다.
#[derive(Clone, Debug)]
pub struct DirectPushOccurrence {
    pub event_id: Uuid,
    pub kind: NotificationType,
    pub source_type: NotificationSourceType,
    pub source_id: Uuid,
    pub occurred_at: Timestamp,
    pub stage_prefix: &'static str,
}

impl DirectPushOccurrence {
    pub fn to_message(&self, user_id: Uuid, expires_at: Timestamp) -> PushMessage {
        PushMessage {
            event_id: self.event_id,
            notification_id: None,
            user_id,
            kind: self.kind,
            source_type: Some(self.source_type),
            source_id: Some(self.source_id),
            occurred_at: self.occurred_at,
            expires_at,
        }
    }
}

/// 발행 집계(`DirectPublicationSummary`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectPublicationSummary {
    pub expires_at: Timestamp,
    pub target_count: usize,
    pub accepted_count: usize,
    pub failed_count: usize,
    pub unknown_count: usize,
    pub expired_count: usize,
}

#[derive(Clone)]
pub struct DirectPushPublisher {
    publisher: Arc<dyn PushMessagePublisher>,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
}

impl DirectPushPublisher {
    pub fn new(
        publisher: Arc<dyn PushMessagePublisher>,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            publisher,
            clock,
            logger,
        }
    }

    /// 발행 도중 원래 예정 시각의 30분 기한에 도달하면 남은 회원은 기한 생략으로 센다.
    pub fn publish(
        &self,
        occurrence: &DirectPushOccurrence,
        user_ids: &[Uuid],
    ) -> DirectPublicationSummary {
        let expires_at = push_expires_at(occurrence.occurred_at);
        let (mut accepted, mut failed, mut unknown, mut expired) = (0usize, 0usize, 0usize, 0usize);
        for user_id in user_ids {
            if is_push_expired_at(expires_at, self.clock.now()) {
                expired = user_ids.len() - accepted - failed - unknown;
                break;
            }
            let result = self.publish_message(
                &occurrence.to_message(*user_id, expires_at),
                occurrence.stage_prefix,
            );
            if result.is_accepted() {
                accepted += 1;
            } else if result.is_acceptance_unknown() {
                unknown += 1;
            } else {
                failed += 1;
            }
        }
        DirectPublicationSummary {
            expires_at,
            target_count: user_ids.len(),
            accepted_count: accepted,
            failed_count: failed,
            unknown_count: unknown,
            expired_count: expired,
        }
    }

    /// 단계 이름은 `{접두사}_publication_result`·`{접두사}_publication_unknown`이다. 운영 `PublicationErrors` 지표가 이
    /// 이름으로 거른다(`backend/deploy/docs/notification-monitoring.md`).
    fn publish_message(&self, message: &PushMessage, stage_prefix: &str) -> PushPublicationResult {
        match self.publisher.publish(message) {
            Ok(result) => {
                if !result.is_accepted() {
                    self.logger
                        .event(Level::Warn, LOGGER)
                        .field("type", TYPE_NOTIFICATION)
                        .field("stage", format!("{stage_prefix}_publication_result"))
                        .field("notificationType", message.kind.as_str())
                        .field("eventId", message.event_id.to_string())
                        .field("userId", message.user_id.to_string())
                        .field("sourceId", message.source_id.map(|id| id.to_string()))
                        .field("occurredAt", format_instant(message.occurred_at))
                        .field(
                            "result",
                            if result.is_acceptance_unknown() {
                                "UNKNOWN"
                            } else {
                                "FAILED"
                            },
                        )
                        .field("errorCode", result.error_code.as_deref())
                        .log("푸시 SQS 직접 발행 결과");
                }
                result
            }
            Err(failure) => {
                self.logger
                    .error(LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", format!("{stage_prefix}_publication_unknown"))
                    .field("notificationType", message.kind.as_str())
                    .field("sourceId", message.source_id.map(|id| id.to_string()))
                    .field("occurredAt", format_instant(message.occurred_at))
                    .field("result", "UNKNOWN")
                    .field("eventId", message.event_id.to_string())
                    .field("userId", message.user_id.to_string())
                    .field("errorCode", failure.exception)
                    .log("푸시 SQS 직접 발행 결과 확인 실패");
                PushPublicationResult::unknown("SQS_ACCEPTANCE_UNKNOWN")
            }
        }
    }
}

#[cfg(test)]
#[path = "direct_push_tests.rs"]
mod tests;
