//! 주제 푸시 발행(원본 `TopicPushService`). 관리자가 정한 KST 시각마다 오늘 주제에 아직 참여하지 않은 회원에게 회원별 SQS
//! 메시지를 직접 발행한다. 알림함 행도, 트랜잭션도 만들지 않는다. 언제 부를지는 [`crate::jobs`]가 정한다(`TopicPushScheduler`,
//! KST 매분 0초).
//!
//! 같은 회차를 두 번 발행하지 않으려고 이 프로세스가 시도한 회차를 메모리에만 기억한다. 재시작하면 사라져 중복 발행이
//! 가능하다(원본과 같은 한계).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use chalkak_core::Clock;
use chalkak_core::log::{Level, Logger};
use chalkak_db::Pool;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use uuid::Uuid;

use super::domain::{
    NotificationSourceType, NotificationType, is_push_expired_at, push_expires_at,
};
use super::repository::NotificationRepository;
use super::sqs_push::{PushMessage, PushMessagePublisher, PushPublicationResult};
use super::topic_push_schedule::{TopicPushSchedule, TopicPushScheduleRepository};
use crate::topic::repository::TopicRepository;
use crate::web::ApiError;
use crate::web::json::format_instant;

const TOPIC_LOGGER: &str = "com.chalkak.backend.notification.service.TopicPushService";
const TYPE_NOTIFICATION: &str = "notification";

#[derive(Clone)]
pub struct TopicPushService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    publisher: Arc<dyn PushMessagePublisher>,
    logger: Arc<Logger>,
    /// 회차 시각까지 시도한 설정. 설정이 사라지면 함께 지운다.
    attempted: Arc<Mutex<HashMap<Uuid, Timestamp>>>,
}

/// 한 회차의 발행 대상.
struct Targets {
    topic_id: Uuid,
    user_ids: Vec<Uuid>,
}

impl TopicPushService {
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
            attempted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `publishDueTopics`. 설정 목록 조회가 실패하면 오류를 돌려준다(회차별 실패는 로그만 남기고 다음 설정으로 간다).
    pub fn publish_due_topics(&self) -> Result<(), ApiError> {
        let schedules = {
            let mut conn = self.pool.get()?;
            TopicPushScheduleRepository.find_all_order_by_send_time(&mut conn)?
        };
        let mut configured_ids = HashSet::new();
        for schedule in &schedules {
            if let Some(id) = schedule.id {
                configured_ids.insert(id);
            }
            self.publish_schedule(schedule);
        }
        self.attempted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| configured_ids.contains(id));
        Ok(())
    }

    fn publish_schedule(&self, schedule: &TopicPushSchedule) {
        let Some(schedule_id) = schedule.id else {
            return;
        };
        let Some(scheduled_at) = schedule.find_due_occurrence(self.clock.now()) else {
            return;
        };
        {
            let mut attempted = self
                .attempted
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if attempted.get(&schedule_id) == Some(&scheduled_at) {
                return;
            }
            // 발행 도중 중단되거나 결과가 불명이어도 같은 프로세스에서 다시 발행하지 않는다.
            attempted.insert(schedule_id, scheduled_at);
        }
        let event_id = Uuid::new_v4();
        match self.find_targets(schedule_id, scheduled_at) {
            Ok(Some(targets)) => {
                self.publish_targets(schedule_id, event_id, scheduled_at, &targets)
            }
            Ok(None) => self
                .logger
                .info(TOPIC_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "topic_execution_skipped")
                .field("eventId", event_id.to_string())
                .field("scheduleId", schedule_id.to_string())
                .field("occurredAt", format_instant(scheduled_at))
                .field("reason", "NO_TOPIC_OR_SCHEDULE_CHANGED")
                .log("주제 푸시 실행 생략"),
            Err(error) => self
                .logger
                .error(TOPIC_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "topic_execution_failed")
                .field("eventId", event_id.to_string())
                .field("scheduleId", schedule_id.to_string())
                .field("occurredAt", format_instant(scheduled_at))
                .field("errorCode", error.exception_name())
                .log("주제 푸시 실행 실패"),
        }
    }

    /// 목록을 읽은 뒤 설정이 바뀌었거나 주제가 없으면 `None`.
    fn find_targets(
        &self,
        schedule_id: Uuid,
        scheduled_at: Timestamp,
    ) -> Result<Option<Targets>, ApiError> {
        let mut conn = self.pool.get()?;
        let Some(current) = TopicPushScheduleRepository.find_by_id(&mut conn, schedule_id)? else {
            return Ok(None);
        };
        if current.find_due_occurrence(self.clock.now()) != Some(scheduled_at) {
            return Ok(None);
        }
        let topic_date = scheduled_at
            .to_zoned(TimeZone::get("Asia/Seoul").map_err(|e| {
                ApiError::Internal(format!("Asia/Seoul 시간대를 찾을 수 없습니다: {e}"))
            })?)
            .date();
        let Some(topic_id) = TopicRepository
            .find_active_by_topic_date(&mut conn, topic_date)?
            .and_then(|topic| topic.id)
        else {
            return Ok(None);
        };
        let user_ids = NotificationRepository.find_topic_push_recipient_ids(&mut conn, topic_id)?;
        Ok(Some(Targets { topic_id, user_ids }))
    }

    fn publish_targets(
        &self,
        schedule_id: Uuid,
        event_id: Uuid,
        scheduled_at: Timestamp,
        targets: &Targets,
    ) {
        let expires_at = push_expires_at(scheduled_at);
        let (mut accepted, mut failed, mut unknown, mut expired) = (0usize, 0usize, 0usize, 0usize);
        for user_id in &targets.user_ids {
            if is_push_expired_at(expires_at, self.clock.now()) {
                expired = targets.user_ids.len() - accepted - failed - unknown;
                break;
            }
            let message = PushMessage {
                event_id,
                notification_id: None,
                user_id: *user_id,
                kind: NotificationType::Topic,
                source_type: Some(NotificationSourceType::Topic),
                source_id: Some(targets.topic_id),
                occurred_at: scheduled_at,
                expires_at,
            };
            let result = self.publish_message(&message);
            if result.is_accepted() {
                accepted += 1;
            } else if result.is_acceptance_unknown() {
                unknown += 1;
            } else {
                failed += 1;
            }
        }
        let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        self.logger
            .info(TOPIC_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "topic_publication_summary")
            .field("eventId", event_id.to_string())
            .field("scheduleId", schedule_id.to_string())
            .field("sourceId", targets.topic_id.to_string())
            .field("occurredAt", format_instant(scheduled_at))
            .field("expiresAt", format_instant(expires_at))
            .field("targetCount", count(targets.user_ids.len()))
            .field("acceptedCount", count(accepted))
            .field("failedCount", count(failed))
            .field("unknownCount", count(unknown))
            .field("expiredCount", count(expired))
            .log("주제 푸시 SQS 발행 집계");
    }

    fn publish_message(&self, message: &PushMessage) -> PushPublicationResult {
        match self.publisher.publish(message) {
            Ok(result) => {
                if !result.is_accepted() {
                    self.logger
                        .event(Level::Warn, TOPIC_LOGGER)
                        .field("type", TYPE_NOTIFICATION)
                        .field("stage", "topic_publication_result")
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
                        .log("주제 푸시 SQS 발행 결과");
                }
                result
            }
            Err(failure) => {
                self.logger
                    .error(TOPIC_LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", "topic_publication_unknown")
                    .field("notificationType", message.kind.as_str())
                    .field("sourceId", message.source_id.map(|id| id.to_string()))
                    .field("occurredAt", format_instant(message.occurred_at))
                    .field("result", "UNKNOWN")
                    .field("eventId", message.event_id.to_string())
                    .field("userId", message.user_id.to_string())
                    .field("errorCode", failure.exception)
                    .log("주제 푸시 SQS 발행 결과 확인 실패");
                PushPublicationResult::unknown("SQS_ACCEPTANCE_UNKNOWN")
            }
        }
    }
}

#[cfg(test)]
#[path = "topic_push_tests.rs"]
mod tests;
