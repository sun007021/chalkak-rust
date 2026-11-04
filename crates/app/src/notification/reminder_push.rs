//! 리마인더 푸시 발행(원본 `ReminderPushService`). 관리자가 등록한 리마인더의 회차가 되면 대상 회원에게 회원별 SQS 메시지를
//! 직접 발행한다. 알림함 행은 만들지 않는다. 언제 부를지는 [`crate::jobs`]가 정한다(`ReminderScheduler`, KST 매분 0초).
//!
//! 같은 회차를 두 번 발행하지 않으려고 이 프로세스가 시도한 회차를 메모리에만 기억한다. 재시작하면 사라져 중복 발행이
//! 가능하다(원본과 같은 한계). 다만 사건 ID가 회차로 정해지므로 중복 발행해도 같은 사건으로 식별된다.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::Pool;
use jiff::Timestamp;
use uuid::Uuid;

use super::direct_push::{DirectPushOccurrence, DirectPushPublisher};
use super::domain::{NotificationSourceType, NotificationType};
use super::push_event_id::create_for_occurrence;
use super::reminder::{Reminder, ReminderRepository, ReminderTarget};
use super::repository::NotificationRepository;
use super::sqs_push::PushMessagePublisher;
use crate::common::transact;
use crate::topic::domain::kst_date;
use crate::topic::repository::TopicRepository;
use crate::web::ApiError;
use crate::web::json::format_instant;

const REMINDER_LOGGER: &str = "com.chalkak.backend.notification.service.ReminderPushService";
const TYPE_NOTIFICATION: &str = "notification";

#[derive(Clone)]
pub struct ReminderPushService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    publisher: DirectPushPublisher,
    logger: Arc<Logger>,
    /// 회차 시각까지 시도한 리마인더. 리마인더가 사라지면 함께 지운다.
    attempted: Arc<Mutex<HashMap<Uuid, Timestamp>>>,
}

/// 한 회차의 발행 대상. 건너뛰면 사유가 있다.
struct PublicationTargets {
    user_ids: Vec<Uuid>,
    skip_reason: Option<&'static str>,
}

impl PublicationTargets {
    fn of(user_ids: Vec<Uuid>) -> Self {
        Self {
            user_ids,
            skip_reason: None,
        }
    }

    fn skipped(reason: &'static str) -> Self {
        Self {
            user_ids: Vec::new(),
            skip_reason: Some(reason),
        }
    }
}

impl ReminderPushService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        publisher: Arc<dyn PushMessagePublisher>,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            pool,
            publisher: DirectPushPublisher::new(publisher, Arc::clone(&clock), Arc::clone(&logger)),
            clock,
            logger,
            attempted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `publishDueReminders`. 목록 조회가 실패하면 오류를 돌려준다(회차별 실패는 로그만 남기고 다음 리마인더로 간다).
    pub fn publish_due_reminders(&self) -> Result<(), ApiError> {
        let reminders = self.find_active_reminders()?;
        self.publish_listed(&reminders);
        Ok(())
    }

    fn find_active_reminders(&self) -> Result<Vec<Reminder>, ApiError> {
        let mut conn = self.pool.get()?;
        Ok(ReminderRepository.find_active_reminders(&mut conn)?)
    }

    /// 읽어 둔 목록의 리마인더를 차례로 발행한다. 목록을 읽은 뒤 바뀐 리마인더는 발행 직전에 다시 확인한다.
    fn publish_listed(&self, reminders: &[Reminder]) {
        let mut active_ids = HashSet::new();
        for reminder in reminders {
            if let Some(id) = reminder.id {
                active_ids.insert(id);
            }
            self.publish_reminder(reminder);
        }
        self.attempted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| active_ids.contains(id));
    }

    fn publish_reminder(&self, reminder: &Reminder) {
        let Some(reminder_id) = reminder.id else {
            return;
        };
        let Some(occurred_at) = reminder.find_due_occurrence(self.clock.now()) else {
            return;
        };
        if self
            .attempted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&reminder_id)
            == Some(&occurred_at)
        {
            return;
        }
        let event_id = create_for_occurrence(NotificationType::Reminder, reminder_id, occurred_at);
        match self.find_targets(reminder_id, occurred_at) {
            Ok(targets) => {
                if let Some(reason) = targets.skip_reason {
                    self.logger
                        .info(REMINDER_LOGGER)
                        .field("type", TYPE_NOTIFICATION)
                        .field("stage", "reminder_execution_skipped")
                        .field("eventId", event_id.to_string())
                        .field("reminderId", reminder_id.to_string())
                        .field("occurredAt", format_instant(occurred_at))
                        .field("reason", reason)
                        .log("리마인더 실행 생략");
                    return;
                }
                self.publish_targets(reminder_id, event_id, occurred_at, &targets.user_ids);
            }
            Err(error) => self
                .logger
                .error(REMINDER_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "reminder_execution_failed")
                .field("eventId", event_id.to_string())
                .field("reminderId", reminder_id.to_string())
                .field("occurredAt", format_instant(occurred_at))
                .field("errorCode", error.exception_name())
                .log("리마인더 실행 실패"),
        }
    }

    /// 리마인더 행을 잠근 채 다시 확인한다. 수정·삭제 중이거나 일정이 바뀌었으면 기록하지 않고 다음 확인 때 새 내용으로
    /// 판단한다.
    fn find_targets(
        &self,
        reminder_id: Uuid,
        occurred_at: Timestamp,
    ) -> Result<PublicationTargets, ApiError> {
        transact(&self.pool, |tx| {
            let Some(current) = ReminderRepository.find_active_for_publication(tx, reminder_id)?
            else {
                return Ok(PublicationTargets::skipped("REMINDER_LOCKED_OR_DELETED"));
            };
            if current.find_due_occurrence(self.clock.now()) != Some(occurred_at) {
                return Ok(PublicationTargets::skipped("SCHEDULE_CHANGED"));
            }
            // 발행 도중 중단되거나 결과가 불명이어도 같은 프로세스에서 다시 발행하지 않는다.
            self.attempted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(reminder_id, occurred_at);
            if current.target == ReminderTarget::All {
                return Ok(PublicationTargets::of(
                    NotificationRepository.find_topic_push_enabled_user_ids(tx)?,
                ));
            }
            find_topic_targets(tx, current.target, occurred_at)
        })
    }

    fn publish_targets(
        &self,
        reminder_id: Uuid,
        event_id: Uuid,
        occurred_at: Timestamp,
        user_ids: &[Uuid],
    ) {
        let summary = self.publisher.publish(
            &DirectPushOccurrence {
                event_id,
                kind: NotificationType::Reminder,
                source_type: NotificationSourceType::Reminder,
                source_id: reminder_id,
                occurred_at,
                stage_prefix: "reminder",
            },
            user_ids,
        );
        let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        self.logger
            .info(REMINDER_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "reminder_publication_summary")
            .field("eventId", event_id.to_string())
            .field("reminderId", reminder_id.to_string())
            .field("occurredAt", format_instant(occurred_at))
            .field("expiresAt", format_instant(summary.expires_at))
            .field("targetCount", count(summary.target_count))
            .field("acceptedCount", count(summary.accepted_count))
            .field("failedCount", count(summary.failed_count))
            .field("unknownCount", count(summary.unknown_count))
            .field("expiredCount", count(summary.expired_count))
            .log("리마인더 SQS 발행 집계");
    }
}

/// 오늘 주제는 회차의 KST 날짜로 찾는다. 참여 여부는 오늘 주제가 있어야 판단할 수 있으므로 주제가 없으면 보내지 않는다.
/// 리마인더는 독려만이 아니므로 주제의 참여 가능 시간과 관계없이 보낸다.
fn find_topic_targets(
    tx: &mut impl chalkak_db::Db,
    target: ReminderTarget,
    occurred_at: Timestamp,
) -> Result<PublicationTargets, ApiError> {
    let Some(topic_id) = TopicRepository
        .find_active_by_topic_date(tx, kst_date(occurred_at))?
        .and_then(|topic| topic.id)
    else {
        return Ok(PublicationTargets::skipped("NO_TOPIC"));
    };
    let user_ids = if target == ReminderTarget::TopicParticipants {
        NotificationRepository.find_topic_push_enabled_participant_ids(tx, topic_id)?
    } else {
        NotificationRepository.find_topic_push_enabled_non_participant_ids(tx, topic_id)?
    };
    Ok(PublicationTargets::of(user_ids))
}

#[cfg(test)]
#[path = "reminder_push_tests.rs"]
mod tests;
