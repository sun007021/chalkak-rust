//! 푸시 Worker의 업무 처리. 원본 `PushWorkerService`: SQS에서 받은 메시지 하나를 검증하고 회원의 기기마다 최신 조건을 다시
//! 확인해 FCM으로 보낸 뒤, 메시지를 끝낼지(삭제) 다시 받을지(재시도)를 정한다.
//!
//! - 메시지가 올바르지 않거나 기한이 지났으면 보내지 않고 완료한다(`INVALID_MESSAGE`, `EXPIRED`).
//! - 기기 목록은 트랜잭션 하나로 읽고, 기기마다 별도 트랜잭션에서 로그인·회원·수신 설정·알림(또는 오늘 주제)을 다시 확인한다.
//!   FCM 호출은 트랜잭션 밖이다. 조건이 바뀌었으면 그 기기는 건너뛴다.
//! - 기기별 발송은 [`PushDeliveryService`]의 발송 기록(`push_deliveries`)을 거친다. 이미 처리된 기기는 다시 보내지 않고, 일시 실패한
//!   기기가 하나라도 있으면(다른 기기가 수락됐어도) 가장 긴 `Retry-After`로 재시도를 요청한다. 모두 끝나야 메시지가 완료다.
//! - 무효 토큰이면 그 기기 행을 (기기 ID, 토큰 해시)로 지운다. 지우기에 실패해도 재시도 사유가 아니다.
//! - 기기 처리 중 예상하지 못한 오류는 그 기기를 일시 실패로 보고 로그만 남긴다(다른 기기는 계속 처리한다).

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::{EventBuilder, Logger};
use chalkak_db::{Db, Pool};
use serde_json::{Value, json};
use uuid::Uuid;

use super::domain::{NotificationType, PublishStatus, is_push_expired_at, is_valid_push_lifetime};
use super::push_delivery::{PushDeliveryAttempt, PushDeliveryError, PushDeliveryService};
use super::push_device::{FcmToken, PushDevice, PushDeviceRepository, find_usable_user};
use super::push_message::RawPushMessage;
use super::push_sender::{
    DevicePushRequest, DevicePushResult, DevicePushSender, DevicePushTiming, DeviceStatus,
    PushProcessingResult,
};
use super::reminder::{ReminderRepository, ReminderTarget};
use super::repository::NotificationRepository;
use super::sqs_push::PushMessage;
use crate::common::{Elapsed, transact};
use crate::post::repository::PostRepository;
use crate::topic::domain::kst_date;
use crate::topic::repository::TopicRepository;
use crate::user::domain::User;
use crate::web::ApiError;

/// 원본 `@Slf4j` 로거 이름.
pub(super) const WORKER_LOGGER: &str = "com.chalkak.backend.notification.service.PushWorkerService";
const TYPE_NOTIFICATION: &str = "notification";
const TOPIC_PUSH_TITLE: &str = "오늘의 주제";

/// 메시지 하나를 처리하는 쪽(`PushWorkerService`). `SqsPushService`가 이 경계 너머로 부른다.
pub trait PushProcessor: Send + Sync {
    /// 처리하지 못하고 예외가 난 경우 그 예외 이름을 돌려준다(원본에서는 `process`가 던진 `RuntimeException`).
    fn process(&self, message: &RawPushMessage) -> Result<PushProcessingResult, String>;
}

#[derive(Clone)]
pub struct PushWorkerService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    sender: Arc<dyn DevicePushSender>,
    logger: Arc<Logger>,
    delivery: PushDeliveryService,
}

impl PushWorkerService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        sender: Arc<dyn DevicePushSender>,
        logger: Arc<Logger>,
    ) -> Self {
        let delivery = PushDeliveryService::new(pool.clone(), Arc::clone(&logger));
        Self {
            pool,
            clock,
            sender,
            logger,
            delivery,
        }
    }

    /// `process`. 처리 중 예상하지 못한 오류가 나면 DB 계측 집계(`processingCompleted=false`)를 남기고 오류를 돌려준다
    /// (그 메시지는 삭제하지 않고 SQS가 다시 전달한다). 같은 (사건, 회원)을 처리 중이거나 발송 기록을 확인하지 못하면 오류가 아니라
    /// 1분 뒤 재처리 결과를 돌려준다.
    pub fn process_message(
        &self,
        raw: &RawPushMessage,
    ) -> Result<PushProcessingResult, PushDeliveryError> {
        let Some(message) = self.validate(raw) else {
            self.log_skipped(raw, None, "INVALID_MESSAGE");
            return Ok(PushProcessingResult::completed());
        };
        if is_push_expired_at(message.expires_at, self.clock.now()) {
            self.log_skipped(raw, None, "EXPIRED");
            return Ok(PushProcessingResult::completed());
        }
        let mut db_timing = PushWorkerDbTiming::default();
        let outcome = self.delivery.process(&message, |attempt| {
            self.process_devices(&message, raw, &mut db_timing, attempt)
        });
        if let Err(error) = &outcome {
            self.log_summary_failed(&message, &db_timing, error.exception_name());
        }
        outcome
    }

    fn process_devices(
        &self,
        message: &PushMessage,
        raw: &RawPushMessage,
        db_timing: &mut PushWorkerDbTiming,
        attempt: &mut PushDeliveryAttempt<'_>,
    ) -> Result<PushProcessingResult, PushDeliveryError> {
        let device_ids = self.measured(db_timing, None, "DEVICE_LOOKUP", |tx| {
            Ok(PushDeviceRepository.find_ids_by_user_id(tx, message.user_id)?)
        })?;
        if device_ids.is_empty() {
            self.log_skipped(raw, None, "NO_DEVICES");
        }
        let mut counts = Counts::default();
        let mut retryable = false;
        let mut retry_after = std::time::Duration::ZERO;
        for device_id in &device_ids {
            let result = self.send_to_device(message, *device_id, db_timing, attempt)?;
            counts.add(result.status);
            retryable = retryable || result.is_retryable();
            if result.is_retryable() && result.retry_after > retry_after {
                retry_after = result.retry_after;
            }
        }
        self.log_summary(message, device_ids.len(), &counts, retryable, db_timing);
        Ok(PushProcessingResult {
            retryable,
            retry_after,
        })
    }

    /// `executeMeasuredTransaction`: 트랜잭션 하나를 실행하고 걸린 시간을 계측에 남긴다(끝까지 가지 못해도 남긴다).
    fn measured<T>(
        &self,
        db_timing: &mut PushWorkerDbTiming,
        device_id: Option<Uuid>,
        operation: &'static str,
        action: impl FnOnce(&mut chalkak_db::Tx<'_>) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let elapsed = Elapsed::start();
        let result = transact(&self.pool, action);
        db_timing.record_transaction(device_id, operation, elapsed, result.is_ok());
        result
    }

    /// `isValidMessage`: 필수 값이 모두 있고, 종류와 대상 종류가 맞고, 주제는 알림함 ID 없이 지금까지 발생한 사건이며
    /// 기한이 정확히 30분이다. 그 밖의 종류는 알림함 ID가 있어야 한다.
    fn validate(&self, raw: &RawPushMessage) -> Option<PushMessage> {
        let (event_id, user_id, kind, source_type, source_id, occurred_at, expires_at) = (
            raw.event_id?,
            raw.user_id?,
            raw.kind?,
            raw.source_type?,
            raw.source_id?,
            raw.occurred_at?,
            raw.expires_at?,
        );
        if kind.source_type() != source_type {
            return None;
        }
        // 주제·리마인더는 알림함 없이 SQS로 직접 발행하므로 알림 ID가 없고 원래 예정 시각의 수명을 지닌다.
        if matches!(kind, NotificationType::Topic | NotificationType::Reminder) {
            if raw.notification_id.is_some()
                || occurred_at > self.clock.now()
                || !is_valid_push_lifetime(occurred_at, expires_at)
            {
                return None;
            }
        } else if raw.notification_id.is_none() {
            return None;
        }
        Some(PushMessage {
            event_id,
            notification_id: raw.notification_id,
            user_id,
            kind,
            source_type: Some(source_type),
            source_id: Some(source_id),
            occurred_at,
            expires_at,
        })
    }

    fn send_to_device(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        db_timing: &mut PushWorkerDbTiming,
        attempt: &mut PushDeliveryAttempt<'_>,
    ) -> Result<DevicePushResult, PushDeliveryError> {
        let mut timing = DevicePushTiming::default();
        match self.send_to_current_device(message, device_id, &mut timing, db_timing, attempt) {
            Err(PushDeliveryError::Api(error)) => {
                let exception = error.exception_name();
                self.log_device_failure(
                    message,
                    device_id,
                    "worker_device_failed",
                    exception,
                    Some(&timing),
                );
                Ok(DevicePushResult::retryable(exception))
            }
            // 기록이 불명확하면 이 사용자 처리를 멈추고 재처리로 넘긴다.
            other => other,
        }
    }

    fn send_to_current_device(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        timing: &mut DevicePushTiming,
        db_timing: &mut PushWorkerDbTiming,
        attempt: &mut PushDeliveryAttempt<'_>,
    ) -> Result<DevicePushResult, PushDeliveryError> {
        let Some(request) = self.measured(
            db_timing,
            Some(device_id),
            "LATEST_CONDITIONS_LOOKUP",
            |tx| self.find_request(tx, message, device_id),
        )?
        else {
            return Ok(DevicePushResult::skipped("LATEST_CONDITIONS"));
        };
        let result = attempt.send(device_id, &*self.clock, || {
            self.send_to_fcm(message, device_id, &request, timing)
        })?;
        if result.status == DeviceStatus::Skipped {
            self.log_skipped_message(
                message,
                Some(device_id),
                result.error_code.as_deref(),
                Some(timing),
            );
            return Ok(result);
        }
        if result.is_invalid_token() {
            self.delete_invalid_token(message, device_id, &request, db_timing);
        }
        Ok(result)
    }

    /// `sendToFcm`: 발송기를 부르고 결과(건너뜀 제외)를 로그에 남긴다. 발송기가 실패하면 로그를 남기고 예외 이름을 돌려준다
    /// (발송 기록이 결과 불명으로 저장한다).
    fn send_to_fcm(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        request: &DevicePushRequest,
        timing: &mut DevicePushTiming,
    ) -> Result<DevicePushResult, String> {
        match self.sender.send(request, timing) {
            Err(failure) => {
                self.log_device_failure(
                    message,
                    device_id,
                    "worker_device_failed",
                    &failure.exception,
                    Some(timing),
                );
                Err(failure.exception)
            }
            Ok(result) => {
                if result.status != DeviceStatus::Skipped {
                    self.log_result(message, device_id, &result, timing);
                }
                Ok(result)
            }
        }
    }

    fn delete_invalid_token(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        request: &DevicePushRequest,
        db_timing: &mut PushWorkerDbTiming,
    ) {
        let outcome = FcmToken::new(&request.token)
            .map_err(|_| "BusinessException".to_owned())
            .and_then(|token| {
                self.measured(db_timing, Some(device_id), "INVALID_TOKEN_CLEANUP", |tx| {
                    Ok(PushDeviceRepository.delete_by_id_and_token_hash(
                        tx,
                        device_id,
                        token.hash(),
                    )?)
                })
                .map_err(|e| e.exception_name().to_owned())
            });
        if let Err(exception) = outcome {
            self.log_device_failure(
                message,
                device_id,
                "worker_invalid_token_cleanup_failed",
                &exception,
                None,
            );
        }
    }

    /// `findRequest`: 발송 직전의 최신 조건을 확인해 보낼 요청을 만든다. 보내면 안 되면 사유를 로그에 남기고 `None`.
    fn find_request(
        &self,
        db: &mut impl Db,
        message: &PushMessage,
        device_id: Uuid,
    ) -> Result<Option<DevicePushRequest>, ApiError> {
        let Some(device) =
            PushDeviceRepository.find_by_id_and_user_id(db, device_id, message.user_id)?
        else {
            return Ok(self.skip(message, device_id, "DEVICE_MISSING"));
        };
        let Some(user) =
            find_usable_user(db, message.user_id, device.session_id, self.clock.now())?
        else {
            return Ok(self.skip(message, device_id, "LOGIN_UNUSABLE"));
        };
        match message.kind {
            NotificationType::Topic => self.find_topic_request(db, message, &device, &user),
            NotificationType::Reminder => self.find_reminder_request(db, message, &device, &user),
            _ => self.find_notification_request(db, message, &device, &user),
        }
    }

    fn find_topic_request(
        &self,
        db: &mut impl Db,
        message: &PushMessage,
        device: &PushDevice,
        user: &User,
    ) -> Result<Option<DevicePushRequest>, ApiError> {
        let device_id = device.id.expect("읽어 온 기기는 식별자가 있다");
        if !user.is_active() {
            return Ok(self.skip(message, device_id, "USER_INACTIVE"));
        }
        if !message.kind.is_push_enabled_for(user) {
            return Ok(self.skip(message, device_id, "PUSH_DISABLED"));
        }
        let today = kst_date(self.clock.now());
        let topic = TopicRepository.find_active_by_topic_date(db, today)?;
        let Some(topic) = topic.filter(|t| t.id == message.source_id) else {
            return Ok(self.skip(message, device_id, "TOPIC_UNAVAILABLE"));
        };
        let topic_id = topic.id.expect("읽어 온 주제는 식별자가 있다");
        if PostRepository::default()
            .find_active_by_author_and_topic(db, user.id, topic_id, false)?
            .is_some()
        {
            return Ok(self.skip(message, device_id, "ALREADY_PARTICIPATED"));
        }
        Ok(Some(DevicePushRequest {
            message: message.clone(),
            token: device.fcm_token.clone(),
            title: TOPIC_PUSH_TITLE.to_owned(),
            body: topic.title,
            expires_at: message.expires_at,
        }))
    }

    /// `findReminderRequest`: 발행 뒤 바뀐 회원 상태·수신 설정·리마인더 삭제·참여 여부를 기기별 발송 직전에 다시 확인하고,
    /// 문구는 이때의 리마인더에서 읽는다. 오늘 주제는 발행과 같이 회차의 KST 날짜로 찾는다.
    fn find_reminder_request(
        &self,
        db: &mut impl Db,
        message: &PushMessage,
        device: &PushDevice,
        user: &User,
    ) -> Result<Option<DevicePushRequest>, ApiError> {
        let device_id = device.id.expect("읽어 온 기기는 식별자가 있다");
        if !user.is_active() {
            return Ok(self.skip(message, device_id, "USER_INACTIVE"));
        }
        if !message.kind.is_push_enabled_for(user) {
            return Ok(self.skip(message, device_id, "PUSH_DISABLED"));
        }
        let source_id = message
            .source_id
            .expect("검증한 리마인더 메시지는 대상 ID가 있다");
        let Some(reminder) = ReminderRepository.find_active_by_id(db, source_id)? else {
            return Ok(self.skip(message, device_id, "REMINDER_UNAVAILABLE"));
        };
        let request = DevicePushRequest {
            message: message.clone(),
            token: device.fcm_token.clone(),
            title: reminder.title.clone(),
            body: reminder.body.clone(),
            expires_at: message.expires_at,
        };
        if reminder.target == ReminderTarget::All {
            return Ok(Some(request));
        }
        let Some(topic_id) = TopicRepository
            .find_active_by_topic_date(db, kst_date(message.occurred_at))?
            .and_then(|topic| topic.id)
        else {
            return Ok(self.skip(message, device_id, "TOPIC_UNAVAILABLE"));
        };
        let participated = PostRepository::default()
            .find_active_by_author_and_topic(db, user.id, topic_id, false)?
            .is_some();
        if reminder.target == ReminderTarget::TopicParticipants && !participated {
            return Ok(self.skip(message, device_id, "NOT_PARTICIPATED"));
        }
        if reminder.target == ReminderTarget::TopicNonParticipants && participated {
            return Ok(self.skip(message, device_id, "ALREADY_PARTICIPATED"));
        }
        Ok(Some(request))
    }

    fn find_notification_request(
        &self,
        db: &mut impl Db,
        message: &PushMessage,
        device: &PushDevice,
        user: &User,
    ) -> Result<Option<DevicePushRequest>, ApiError> {
        let device_id = device.id.expect("읽어 온 기기는 식별자가 있다");
        if !message.kind.is_push_enabled_for(user) {
            return Ok(self.skip(message, device_id, "PUSH_DISABLED"));
        }
        let notification_id = message
            .notification_id
            .expect("검증한 비주제 메시지는 알림함 ID가 있다");
        let Some(notification) = NotificationRepository.find_for_push_by_id_and_user_id(
            db,
            notification_id,
            message.user_id,
        )?
        else {
            return Ok(self.skip(message, device_id, "NOTIFICATION_UNAVAILABLE"));
        };
        if notification.read_at.is_some() {
            return Ok(self.skip(message, device_id, "ALREADY_READ"));
        }
        if notification.is_push_expired(self.clock.now()) {
            return Ok(self.skip(message, device_id, "EXPIRED"));
        }
        if !matches_notification(message, &notification) {
            return Ok(self.skip(message, device_id, "MESSAGE_MISMATCH"));
        }
        if !matches!(
            notification.publish_status,
            PublishStatus::Pending | PublishStatus::Published
        ) {
            return Ok(self.skip(message, device_id, "PUBLICATION_INACTIVE"));
        }
        Ok(Some(DevicePushRequest {
            message: message.clone(),
            token: device.fcm_token.clone(),
            expires_at: notification.push_expires_at(),
            title: notification.title,
            body: notification.body,
        }))
    }

    fn skip(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        reason: &str,
    ) -> Option<DevicePushRequest> {
        self.log_skipped_message(message, Some(device_id), Some(reason), None);
        None
    }

    // ---- 로그 ----

    /// 검증을 통과하지 못한 메시지도 남길 수 있게 읽은 그대로의 값으로 쓴다(`logSkipped`).
    fn log_skipped(&self, raw: &RawPushMessage, device_id: Option<Uuid>, reason: &str) {
        self.logger
            .info(WORKER_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_skipped")
            .field("deviceId", device_id.map(|id| id.to_string()))
            .field("reason", reason)
            .field("userId", raw.user_id.map(|id| id.to_string()))
            .field("notificationType", raw.kind.map(|k| k.as_str()))
            .field("sourceId", raw.source_id.map(|id| id.to_string()))
            .field("eventId", raw.event_id.map(|id| id.to_string()))
            .field(
                "notificationId",
                raw.notification_id.map(|id| id.to_string()),
            )
            .log("푸시 발송 조건 미충족");
    }

    fn log_skipped_message(
        &self,
        message: &PushMessage,
        device_id: Option<Uuid>,
        reason: Option<&str>,
        timing: Option<&DevicePushTiming>,
    ) {
        let builder = self
            .logger
            .info(WORKER_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_skipped")
            .field("deviceId", device_id.map(|id| id.to_string()))
            .field("reason", reason)
            .field("userId", message.user_id.to_string())
            .field("notificationType", message.kind.as_str())
            .field("sourceId", message.source_id.map(|id| id.to_string()))
            .field("eventId", message.event_id.to_string())
            .field(
                "notificationId",
                message.notification_id.map(|id| id.to_string()),
            );
        add_request_timing(builder, timing).log("푸시 발송 조건 미충족");
    }

    fn log_result(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        result: &DevicePushResult,
        timing: &DevicePushTiming,
    ) {
        let builder = if result.is_permanent_failure() {
            self.logger.error(WORKER_LOGGER)
        } else if result.is_retryable() || result.is_invalid_token() {
            self.logger.warn(WORKER_LOGGER)
        } else {
            self.logger.info(WORKER_LOGGER)
        };
        let builder = builder
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "fcm_result")
            .field("userId", message.user_id.to_string())
            .field("notificationType", message.kind.as_str())
            .field("sourceId", message.source_id.map(|id| id.to_string()))
            .field("eventId", message.event_id.to_string())
            .field(
                "notificationId",
                message.notification_id.map(|id| id.to_string()),
            )
            .field("deviceId", device_id.to_string())
            .field("result", result.status.as_str())
            .field("errorCode", result.error_code.as_deref());
        add_request_timing(builder, Some(timing)).log("FCM 발송 요청 결과");
    }

    /// `createSummaryEvent`: 실패 원인은 기존 오류 로그로 추적하므로 요약은 INFO다(ERROR 지표를 중복 증가시키지 않는다).
    fn summary_event(
        &self,
        message: &PushMessage,
        db_timing: &PushWorkerDbTiming,
    ) -> EventBuilder<'_> {
        self.logger
            .info(WORKER_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_summary")
            .field("eventId", message.event_id.to_string())
            .field(
                "notificationId",
                message.notification_id.map(|id| id.to_string()),
            )
            .field("userId", message.user_id.to_string())
            .field("notificationType", message.kind.as_str())
            .field("dbTransactionCount", db_timing.transactions.len())
            .field("dbTransactionDurationMs", db_timing.duration_ms)
            .field(
                "dbTransactions",
                Value::Array(db_timing.transactions.clone()),
            )
    }

    fn log_summary(
        &self,
        message: &PushMessage,
        target_count: usize,
        counts: &Counts,
        retryable: bool,
        db_timing: &PushWorkerDbTiming,
    ) {
        self.summary_event(message, db_timing)
            .field("processingCompleted", true)
            .field("targetCount", target_count)
            .field("acceptedCount", counts.of(DeviceStatus::Accepted))
            .field("retryableCount", counts.of(DeviceStatus::Retryable))
            .field(
                "permanentFailureCount",
                counts.of(DeviceStatus::PermanentFailure),
            )
            .field("invalidTokenCount", counts.of(DeviceStatus::InvalidToken))
            .field("skippedCount", counts.of(DeviceStatus::Skipped))
            .field("retryable", retryable)
            .log("푸시 기기 처리 결과 집계");
    }

    fn log_summary_failed(
        &self,
        message: &PushMessage,
        db_timing: &PushWorkerDbTiming,
        exception: &str,
    ) {
        self.summary_event(message, db_timing)
            .field("processingCompleted", false)
            .field("errorCode", exception)
            .log("푸시 처리 중단 시점의 DB 계측 집계");
    }

    fn log_device_failure(
        &self,
        message: &PushMessage,
        device_id: Uuid,
        stage: &str,
        exception: &str,
        timing: Option<&DevicePushTiming>,
    ) {
        let builder = self
            .logger
            .error(WORKER_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", stage)
            .field("userId", message.user_id.to_string())
            .field("notificationType", message.kind.as_str())
            .field("sourceId", message.source_id.map(|id| id.to_string()))
            .field("eventId", message.event_id.to_string())
            .field(
                "notificationId",
                message.notification_id.map(|id| id.to_string()),
            )
            .field("deviceId", device_id.to_string())
            .field("errorCode", exception);
        add_request_timing(builder, timing).log("푸시 기기 처리 오류");
    }
}

impl PushProcessor for PushWorkerService {
    fn process(&self, message: &RawPushMessage) -> Result<PushProcessingResult, String> {
        self.process_message(message)
            .map_err(|e| e.exception_name().to_owned())
    }
}

/// `matches`: 메시지가 알림함 행이 가리키는 바로 그 사건인가(다른 사건으로 바꿔치거나 기한을 늘린 메시지를 거른다).
fn matches_notification(message: &PushMessage, notification: &super::domain::Notification) -> bool {
    message.event_id == notification.event_key
        && message.kind == notification.kind
        && message.source_type == notification.source_type
        && message.source_id == notification.source_id
        && message.occurred_at == notification.created_at
        && message.expires_at == notification.push_expires_at()
}

/// 결과 종류별 기기 수.
#[derive(Default)]
struct Counts([usize; 5]);

impl Counts {
    fn index(status: DeviceStatus) -> usize {
        match status {
            DeviceStatus::Accepted => 0,
            DeviceStatus::Skipped => 1,
            DeviceStatus::Retryable => 2,
            DeviceStatus::InvalidToken => 3,
            DeviceStatus::PermanentFailure => 4,
        }
    }

    fn add(&mut self, status: DeviceStatus) {
        self.0[Self::index(status)] += 1;
    }

    fn of(&self, status: DeviceStatus) -> usize {
        self.0[Self::index(status)]
    }
}

/// `addRequestTiming`: FCM 요청을 보냈을 때만 걸린 시간과 SDK 수락 여부를 덧붙인다.
fn add_request_timing<'a>(
    builder: EventBuilder<'a>,
    timing: Option<&DevicePushTiming>,
) -> EventBuilder<'a> {
    match timing {
        Some(timing) if timing.has_request() => builder
            .field("fcmRequestDurationMs", timing.request_duration_ms())
            .field("sdkAccepted", timing.sdk_accepted()),
        _ => builder,
    }
}

/// `PushWorkerDbTiming`: 메시지 하나를 처리하며 연 DB 트랜잭션의 종류별 소요 시간.
#[derive(Default)]
pub struct PushWorkerDbTiming {
    transactions: Vec<Value>,
    duration_ms: u64,
}

impl PushWorkerDbTiming {
    /// `recordTransaction`. 항목의 키 순서는 `deviceId`, `operation`, `transactionCompleted`, `dbTransactionDurationMs`다.
    fn record_transaction(
        &mut self,
        device_id: Option<Uuid>,
        operation: &str,
        elapsed: Elapsed,
        completed: bool,
    ) {
        let duration_ms = elapsed.to_millis();
        self.transactions.push(json!({
            "deviceId": device_id.map(|id| id.to_string()),
            "operation": operation,
            "transactionCompleted": completed,
            "dbTransactionDurationMs": duration_ms,
        }));
        self.duration_ms += duration_ms;
    }
}
