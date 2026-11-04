//! 푸시 발송 기록. 원본 `PushDelivery`, `PushDeliveryRepository`, `PushDeliveryLocks`, `PushDeliveryAttempt`,
//! `PushDeliveryService`, `PushDeliveryRecordException`.
//!
//! (사건 ID, 회원) 한 쌍에 기록 한 행이 있고, 기기별 결과(`device_results`)를 JSONB로 담는다. 한 기기의 FCM 호출 전에
//! `SENDING`을 먼저 커밋하고, 호출이 끝나면 결과를 저장한다. 같은 메시지가 다시 오면 이미 처리된 기기(수락·무효 토큰·영구
//! 실패·건너뜀)는 다시 보내지 않고, 일시 실패·결과 불명·발송 중이던 기기만 다시 보낸다. 모든 기기가 끝나면 기록을 완료한다.
//!
//! - 같은 프로세스 안의 같은 (사건, 회원) 처리는 [`PushDeliveryLocks`]로 막는다. 막힌 처리는 1분 뒤 다시 받도록 넘긴다.
//!   (프로세스 사이의 중복은 DB 행의 `SENDING`/결과 기록이 막는다. `ADR 2026-10-27-push-delivery-local-lock`)
//! - 기록이 불명확해지면(저장 실패, 갱신 0건) [`PushDeliveryError::Record`]로 중단하고 같은 1분 뒤 재처리로 넘긴다.
//! - 원본은 `Propagation.NEVER`로 상위 트랜잭션 안에서 부르는 것을 막는다. Rust에는 암묵적 트랜잭션이 없어 해당하지 않는다.
//! - 원본 저장소의 `@Transactional(timeout = 5)`에 해당하는 문장 제한은 두지 않는다(이 저장소의 DB 계층에는 트랜잭션별 제한이
//!   없다).

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::Logger;
use chalkak_db::{Db, DbError, Pool, Tx};
use jiff::Timestamp;
use postgres::Row;
use serde_json::{Value, json};
use uuid::Uuid;

use super::push_sender::{DevicePushResult, PushProcessingResult};
use super::sqs_push::PushMessage;
use crate::web::ApiError;
use crate::web::json::format_instant;

pub(super) const DELIVERY_SERVICE_LOGGER: &str =
    "com.chalkak.backend.notification.service.PushDeliveryService";
pub(super) const DELIVERY_ATTEMPT_LOGGER: &str =
    "com.chalkak.backend.notification.service.PushDeliveryAttempt";
const TYPE_NOTIFICATION: &str = "notification";
/// 같은 키를 처리 중이거나 기록을 확인하지 못했을 때 다시 받을 때까지 미루는 시간.
pub const RETRY_DELAY: Duration = Duration::from_secs(60);

const INVALID_DEVICE_RESULT: &str = "유효하지 않은 기기별 발송 기록입니다.";

/// `PushDeliveryRecordException`과 그 밖의 실패.
#[derive(Debug)]
pub enum PushDeliveryError {
    /// 기록이 불명확해져 사용자 작업을 중단한다(`PushDeliveryRecordException`).
    Record,
    Api(ApiError),
}

impl PushDeliveryError {
    /// 원본 예외 클래스 단순 이름.
    pub fn exception_name(&self) -> &'static str {
        match self {
            PushDeliveryError::Record => "PushDeliveryRecordException",
            PushDeliveryError::Api(error) => error.exception_name(),
        }
    }
}

impl From<ApiError> for PushDeliveryError {
    fn from(error: ApiError) -> Self {
        PushDeliveryError::Api(error)
    }
}

impl From<DbError> for PushDeliveryError {
    fn from(error: DbError) -> Self {
        PushDeliveryError::Api(error.into())
    }
}

impl From<AppError> for PushDeliveryError {
    fn from(error: AppError) -> Self {
        PushDeliveryError::Api(error.into())
    }
}

impl From<postgres::Error> for PushDeliveryError {
    fn from(error: postgres::Error) -> Self {
        PushDeliveryError::Api(DbError::from(error).into())
    }
}

/// `PushDeliveryStatus`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushDeliveryStatus {
    Pending,
    Completed,
}

impl PushDeliveryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            PushDeliveryStatus::Pending => "PENDING",
            PushDeliveryStatus::Completed => "COMPLETED",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "PENDING" => Some(PushDeliveryStatus::Pending),
            "COMPLETED" => Some(PushDeliveryStatus::Completed),
            _ => None,
        }
    }
}

/// `PushDeviceDeliveryStatus`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceDeliveryStatus {
    Sending,
    Accepted,
    Retryable,
    Unknown,
    InvalidToken,
    PermanentFailure,
    Skipped,
}

impl DeviceDeliveryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            DeviceDeliveryStatus::Sending => "SENDING",
            DeviceDeliveryStatus::Accepted => "ACCEPTED",
            DeviceDeliveryStatus::Retryable => "RETRYABLE",
            DeviceDeliveryStatus::Unknown => "UNKNOWN",
            DeviceDeliveryStatus::InvalidToken => "INVALID_TOKEN",
            DeviceDeliveryStatus::PermanentFailure => "PERMANENT_FAILURE",
            DeviceDeliveryStatus::Skipped => "SKIPPED",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [
            DeviceDeliveryStatus::Sending,
            DeviceDeliveryStatus::Accepted,
            DeviceDeliveryStatus::Retryable,
            DeviceDeliveryStatus::Unknown,
            DeviceDeliveryStatus::InvalidToken,
            DeviceDeliveryStatus::PermanentFailure,
            DeviceDeliveryStatus::Skipped,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
    }
}

fn invalid_device_result() -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, INVALID_DEVICE_RESULT)
}

/// `PushDelivery.DeviceResult`: 기기 하나의 발송 기록.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceResult {
    pub status: DeviceDeliveryStatus,
    pub attempt_count: i32,
    pub last_error_code: Option<String>,
    pub send_started_at: Timestamp,
    pub accepted_at: Option<Timestamp>,
}

impl DeviceResult {
    /// 생성자의 검증: 시도 횟수는 1 이상이고, 수락 시각은 `ACCEPTED`일 때만 있다.
    pub fn new(
        status: DeviceDeliveryStatus,
        attempt_count: i32,
        last_error_code: Option<String>,
        send_started_at: Timestamp,
        accepted_at: Option<Timestamp>,
    ) -> Result<Self, AppError> {
        if attempt_count < 1 || (status == DeviceDeliveryStatus::Accepted) != accepted_at.is_some()
        {
            return Err(invalid_device_result());
        }
        Ok(Self {
            status,
            attempt_count,
            last_error_code,
            send_started_at,
            accepted_at,
        })
    }

    /// `finish`: 발송 중인 시도에 결과를 기록한다. 수락이면 `finished_at`이 수락 시각이다.
    pub fn finish(
        &self,
        result: DeviceDeliveryStatus,
        error_code: Option<&str>,
        finished_at: Timestamp,
    ) -> Result<Self, AppError> {
        if self.status != DeviceDeliveryStatus::Sending || result == DeviceDeliveryStatus::Sending {
            return Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "발송 중인 시도만 결과를 기록할 수 있습니다.",
            ));
        }
        let accepted = (result == DeviceDeliveryStatus::Accepted).then_some(finished_at);
        Self::new(
            result,
            self.attempt_count,
            error_code.map(str::to_owned),
            self.send_started_at,
            accepted,
        )
    }

    pub fn is_accepted(&self) -> bool {
        self.status == DeviceDeliveryStatus::Accepted
    }

    /// `canRetry`: 발송 중(이전 처리가 도중에 끊김)·일시 실패·결과 불명만 다시 보낸다.
    pub fn can_retry(&self) -> bool {
        matches!(
            self.status,
            DeviceDeliveryStatus::Sending
                | DeviceDeliveryStatus::Retryable
                | DeviceDeliveryStatus::Unknown
        )
    }

    /// Jackson이 `device_results`에 쓰는 모양(`null`도 쓴다, `Instant`는 ISO 문자열).
    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status.as_str(),
            "attemptCount": self.attempt_count,
            "lastErrorCode": self.last_error_code,
            "sendStartedAt": format_instant(self.send_started_at),
            "acceptedAt": self.accepted_at.map(format_instant),
        })
    }

    fn from_json(value: &Value) -> Result<Self, AppError> {
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .and_then(DeviceDeliveryStatus::parse);
        let attempt_count = value
            .get("attemptCount")
            .and_then(Value::as_i64)
            .and_then(|count| i32::try_from(count).ok());
        let started = parse_instant(value.get("sendStartedAt"));
        let accepted = value.get("acceptedAt").filter(|v| !v.is_null());
        let accepted_at = match accepted {
            Some(text) => Some(parse_instant(Some(text)).ok_or_else(invalid_device_result)?),
            None => None,
        };
        let last_error_code = value
            .get("lastErrorCode")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match (status, attempt_count, started) {
            (Some(status), Some(count), Some(started)) => {
                Self::new(status, count, last_error_code, started, accepted_at)
            }
            _ => Err(invalid_device_result()),
        }
    }
}

fn parse_instant(value: Option<&Value>) -> Option<Timestamp> {
    value?.as_str()?.parse().ok()
}

/// `PushDelivery`: (사건, 회원) 한 쌍의 발송 기록.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushDelivery {
    pub id: Uuid,
    pub event_id: Uuid,
    pub user_id: Uuid,
    pub status: PushDeliveryStatus,
    pub device_results: BTreeMap<Uuid, DeviceResult>,
    pub completed_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl PushDelivery {
    pub fn is_completed(&self) -> bool {
        self.status == PushDeliveryStatus::Completed
    }

    pub fn has_accepted_device(&self) -> bool {
        self.device_results.values().any(DeviceResult::is_accepted)
    }

    /// `canSendTo`: 기록이 없거나, 다시 보낼 수 있는 상태인 기기.
    pub fn can_send_to(&self, device_id: Uuid) -> bool {
        self.device_results
            .get(&device_id)
            .is_none_or(DeviceResult::can_retry)
    }

    /// `startDevice`: 새 시도(`SENDING`)를 만든다. 아직 저장하지 않는다.
    pub fn start_device(
        &self,
        device_id: Uuid,
        started_at: Timestamp,
    ) -> Result<DeviceResult, AppError> {
        if !self.can_send_to(device_id) {
            return Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "처리 완료된 기기는 다시 발송할 수 없습니다.",
            ));
        }
        let count = match self.device_results.get(&device_id) {
            None => 1,
            Some(previous) => previous
                .attempt_count
                .checked_add(1)
                .ok_or_else(invalid_device_result)?,
        };
        DeviceResult::new(DeviceDeliveryStatus::Sending, count, None, started_at, None)
    }

    /// `recordDeviceResult`: 저장에 성공한 뒤 메모리의 결과도 갱신한다.
    pub fn record_device_result(&mut self, device_id: Uuid, result: DeviceResult) {
        self.device_results.insert(device_id, result);
    }
}

const COLUMNS: &str = "id, event_id, user_id, status, device_results::text AS device_results, \
                       completed_at, created_at, updated_at";

fn from_row(row: &Row) -> Result<PushDelivery, DbError> {
    let status: String = row.try_get("status")?;
    let status = PushDeliveryStatus::parse(&status)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 푸시 발송 상태: {status}")))?;
    let text: String = row.try_get("device_results")?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| DbError::Config(format!("푸시 발송 기록 JSON을 읽지 못했습니다: {e}")))?;
    let Value::Object(entries) = value else {
        return Err(DbError::Config(
            "푸시 발송 기록이 JSON 객체가 아닙니다.".to_owned(),
        ));
    };
    let mut device_results = BTreeMap::new();
    for (key, entry) in &entries {
        let device_id = key
            .parse::<Uuid>()
            .map_err(|e| DbError::Config(format!("기기 ID를 읽지 못했습니다: {e}")))?;
        let result = DeviceResult::from_json(entry)
            .map_err(|_| DbError::Config(INVALID_DEVICE_RESULT.to_owned()))?;
        device_results.insert(device_id, result);
    }
    Ok(PushDelivery {
        id: row.try_get("id")?,
        event_id: row.try_get("event_id")?,
        user_id: row.try_get("user_id")?,
        status,
        device_results,
        completed_at: row.try_get("completed_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// `PushDeliveryRepository`
#[derive(Clone, Copy, Debug, Default)]
pub struct PushDeliveryRepository;

impl PushDeliveryRepository {
    /// `createIfAbsent`: 회원이 있으면 행을 만들고(이미 있으면 그대로 둔다) 읽어 돌려준다. 회원이 없으면 `None`.
    pub fn create_if_absent(
        &self,
        db: &mut impl Db,
        event_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<PushDelivery>, DbError> {
        db.execute(
            "INSERT INTO push_deliveries (event_id, user_id) \
             SELECT $1::uuid, $2::uuid WHERE EXISTS (SELECT 1 FROM users WHERE id = $2::uuid) \
             ON CONFLICT (event_id, user_id) DO NOTHING",
            &[&event_id, &user_id],
        )?;
        self.find_by_event_id_and_user_id(db, event_id, user_id)
    }

    pub fn find_by_event_id_and_user_id(
        &self,
        db: &mut impl Db,
        event_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<PushDelivery>, DbError> {
        let sql =
            format!("SELECT {COLUMNS} FROM push_deliveries WHERE event_id = $1 AND user_id = $2");
        db.query_opt(&sql, &[&event_id, &user_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `saveDeviceResult`: 미완료 기록에 한해 기기 한 대의 결과만 바꾼다(다른 기기의 결과는 건드리지 않는다).
    /// 바뀐 행이 하나면 `true`.
    pub fn save_device_result(
        &self,
        db: &mut impl Db,
        id: Uuid,
        device_id: Uuid,
        result: &DeviceResult,
    ) -> Result<bool, DbError> {
        let json = result.to_json().to_string();
        let device_key = device_id.to_string();
        let updated = db.execute(
            "UPDATE push_deliveries \
             SET device_results = jsonb_set(device_results, ARRAY[CAST($2 AS text)], \
                 CAST(CAST($3 AS text) AS jsonb), true), updated_at = clock_timestamp() \
             WHERE id = $1 AND status = 'PENDING'",
            &[&id, &device_key, &json],
        )?;
        Ok(updated == 1)
    }

    /// `complete`: 미완료 기록을 완료로 바꾼다. 바뀐 행이 하나면 `true`.
    pub fn complete(&self, db: &mut impl Db, id: Uuid) -> Result<bool, DbError> {
        let updated = db.execute(
            "UPDATE push_deliveries SET status = 'COMPLETED', completed_at = clock_timestamp(), \
             updated_at = clock_timestamp() WHERE id = $1 AND status = 'PENDING'",
            &[&id],
        )?;
        Ok(updated == 1)
    }

    /// `deleteExpiredForActiveUsers`: 탈퇴하지 않은 회원의 기록 중 `created_before`보다 먼저 만들어진 것.
    pub fn delete_expired_for_active_users(
        &self,
        db: &mut impl Db,
        created_before: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM push_deliveries delivery WHERE delivery.created_at < $1 \
             AND EXISTS (SELECT 1 FROM users member \
                         WHERE member.id = delivery.user_id AND member.deleted_at IS NULL)",
            &[&created_before],
        )?)
    }

    /// `deleteExpiredForWithdrawnUsers`: `withdrawn_before`보다 먼저 탈퇴한 회원의 기록 전부.
    pub fn delete_expired_for_withdrawn_users(
        &self,
        db: &mut impl Db,
        withdrawn_before: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM push_deliveries delivery WHERE EXISTS (SELECT 1 FROM users member \
             WHERE member.id = delivery.user_id AND member.deleted_at < $1)",
            &[&withdrawn_before],
        )?)
    }
}

/// `PushDeliveryLocks`: 하나의 [`PushDeliveryService`]를 공유하는 프로세스 안에서만 유효한 (사건, 회원) 처리 표시.
#[derive(Default)]
pub struct PushDeliveryLocks {
    in_progress: Mutex<HashSet<(Uuid, Uuid)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PushDeliveryLocks {
    /// `tryAcquire`: 이미 처리 중이면 `None`. 등록에 성공한 처리만 돌려받은 표식을 놓을 때 자신의 키를 지운다.
    pub fn try_acquire(&self, event_id: Uuid, user_id: Uuid) -> Option<PushDeliveryLockGuard<'_>> {
        lock(&self.in_progress)
            .insert((event_id, user_id))
            .then_some(PushDeliveryLockGuard {
                locks: self,
                key: (event_id, user_id),
            })
    }

    pub fn is_held(&self, event_id: Uuid, user_id: Uuid) -> bool {
        lock(&self.in_progress).contains(&(event_id, user_id))
    }
}

/// 잡고 있는 동안 키를 유지하고, 놓으면(패닉 포함) 지운다(`release`를 `finally`에서 부르는 것).
pub struct PushDeliveryLockGuard<'a> {
    locks: &'a PushDeliveryLocks,
    key: (Uuid, Uuid),
}

impl Drop for PushDeliveryLockGuard<'_> {
    fn drop(&mut self) {
        lock(&self.locks.in_progress).remove(&self.key);
    }
}

/// 트랜잭션 하나 안에서 `f`를 실행한다. `Ok`면 커밋(커밋 실패는 오류), `Err`면 롤백한다.
fn run_transaction<T>(
    pool: &Pool,
    f: impl FnOnce(&mut Tx<'_>) -> Result<T, PushDeliveryError>,
) -> Result<T, PushDeliveryError> {
    let mut conn = pool.get()?;
    let mut tx = conn.begin()?;
    match f(&mut tx) {
        Ok(value) => {
            tx.commit()?;
            Ok(value)
        }
        Err(error) => {
            // 롤백 실패는 원래 오류를 가리지 않는다.
            let _ = tx.rollback();
            Err(error)
        }
    }
}

/// `PushDeliveryService`
#[derive(Clone)]
pub struct PushDeliveryService {
    pool: Pool,
    logger: Arc<Logger>,
    locks: Arc<PushDeliveryLocks>,
}

impl PushDeliveryService {
    pub fn new(pool: Pool, logger: Arc<Logger>) -> Self {
        Self {
            pool,
            logger,
            locks: Arc::new(PushDeliveryLocks::default()),
        }
    }

    pub fn locks(&self) -> &PushDeliveryLocks {
        &self.locks
    }

    /// `process`: 같은 (사건, 회원)을 이 프로세스에서 이미 처리 중이면 `action`에 들어가지 않고 1분 뒤 재처리로 넘긴다.
    /// 기록이 불명확해지면 같은 방식으로 넘긴다. 그 밖의 오류는 그대로 돌려준다.
    pub fn process<F>(
        &self,
        message: &PushMessage,
        action: F,
    ) -> Result<PushProcessingResult, PushDeliveryError>
    where
        F: FnOnce(&mut PushDeliveryAttempt<'_>) -> Result<PushProcessingResult, PushDeliveryError>,
    {
        let Some(_guard) = self.locks.try_acquire(message.event_id, message.user_id) else {
            self.logger
                .info(DELIVERY_SERVICE_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "worker_delivery_lock_busy")
                .field("eventId", message.event_id.to_string())
                .field("userId", message.user_id.to_string())
                .log("동일한 푸시 작업이 진행 중이므로 재처리 대기");
            return Ok(retry_later());
        };
        match self.process_locked(message, action) {
            Err(PushDeliveryError::Record) => {
                self.logger
                    .warn(DELIVERY_SERVICE_LOGGER)
                    .field("type", TYPE_NOTIFICATION)
                    .field("stage", "worker_delivery_record_unavailable")
                    .field("eventId", message.event_id.to_string())
                    .field("userId", message.user_id.to_string())
                    .log("푸시 기록 확인 불가로 재처리 대기");
                Ok(retry_later())
            }
            other => other,
        }
    }

    fn process_locked<F>(
        &self,
        message: &PushMessage,
        action: F,
    ) -> Result<PushProcessingResult, PushDeliveryError>
    where
        F: FnOnce(&mut PushDeliveryAttempt<'_>) -> Result<PushProcessingResult, PushDeliveryError>,
    {
        let existing = {
            let mut conn = self.pool.get()?;
            PushDeliveryRepository.find_by_event_id_and_user_id(
                &mut conn,
                message.event_id,
                message.user_id,
            )?
        };
        if existing.as_ref().is_some_and(PushDelivery::is_completed) {
            self.logger
                .info(DELIVERY_SERVICE_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "worker_delivery_skipped")
                .field("eventId", message.event_id.to_string())
                .field("userId", message.user_id.to_string())
                .field("reason", "ALREADY_COMPLETED")
                .log("기록된 푸시 발송 결과에 따라 재발송 생략");
            return Ok(PushProcessingResult::completed());
        }
        let mut attempt = PushDeliveryAttempt {
            pool: &self.pool,
            logger: &self.logger,
            message,
            delivery: existing,
        };
        attempt.process(action)
    }
}

fn retry_later() -> PushProcessingResult {
    PushProcessingResult {
        retryable: true,
        retry_after: RETRY_DELAY,
    }
}

/// `PushDeliveryAttempt`: 메시지 하나를 처리하는 동안 기록을 들고 기기별 발송 전후를 저장한다.
pub struct PushDeliveryAttempt<'a> {
    pool: &'a Pool,
    logger: &'a Logger,
    message: &'a PushMessage,
    delivery: Option<PushDelivery>,
}

impl PushDeliveryAttempt<'_> {
    fn process<F>(&mut self, action: F) -> Result<PushProcessingResult, PushDeliveryError>
    where
        F: FnOnce(&mut Self) -> Result<PushProcessingResult, PushDeliveryError>,
    {
        let result = action(self)?;
        let Some(delivery) = &self.delivery else {
            return Ok(result);
        };
        if result.retryable {
            return Ok(result);
        }
        let completed = {
            let mut conn = self.pool.get()?;
            PushDeliveryRepository.complete(&mut conn, delivery.id)?
        };
        if !completed {
            return Err(PushDeliveryError::Record);
        }
        Ok(PushProcessingResult::completed())
    }

    /// `send`: 기기 한 대의 발송. 시작 기록을 먼저 커밋한 뒤 `action`(FCM 호출)을 실행하고 결과를 저장한다.
    /// 이 기기가 이미 처리됐으면 `action`을 부르지 않고 `DELIVERY_ALREADY_PROCESSED`로 건너뛴다.
    /// `action`이 실패(예외 이름)하면 결과 불명으로 저장하고 일시 실패를 돌려준다.
    pub fn send(
        &mut self,
        device_id: Uuid,
        clock: &dyn Clock,
        action: impl FnOnce() -> Result<DevicePushResult, String>,
    ) -> Result<DevicePushResult, PushDeliveryError> {
        let Some(started) = self.start_device(device_id, clock.now())? else {
            return Ok(DevicePushResult::skipped("DELIVERY_ALREADY_PROCESSED"));
        };
        let outcome = action();
        match outcome {
            Err(exception) => {
                let finished =
                    started.finish(DeviceDeliveryStatus::Unknown, Some(&exception), clock.now())?;
                self.save(device_id, finished)?;
                Ok(DevicePushResult::retryable(&exception))
            }
            Ok(result) => {
                let finished = started.finish(
                    to_device_status(&result),
                    result.error_code.as_deref(),
                    clock.now(),
                )?;
                self.save(device_id, finished)?;
                Ok(result)
            }
        }
    }

    /// 준비 트랜잭션(제한 5초는 두지 않는다): 기록 생성, 발송 가능 확인, `SENDING` 저장을 한 번에 커밋한다.
    fn start_device(
        &mut self,
        device_id: Uuid,
        now: Timestamp,
    ) -> Result<Option<DeviceResult>, PushDeliveryError> {
        let (pool, logger, message) = (self.pool, self.logger, self.message);
        let delivery = &mut self.delivery;
        match run_transaction(pool, |tx| {
            prepare_device(tx, logger, message, delivery, device_id, now)
        }) {
            Err(PushDeliveryError::Api(error)) => {
                log_record_failure(logger, message, error.exception_name());
                Err(PushDeliveryError::Record)
            }
            other => other,
        }
    }

    fn save(&mut self, device_id: Uuid, result: DeviceResult) -> Result<(), PushDeliveryError> {
        let Some(delivery) = self.delivery.as_mut() else {
            return Err(PushDeliveryError::Record);
        };
        save_device_result(
            self.pool,
            self.logger,
            self.message,
            delivery,
            device_id,
            result,
        )
    }
}

fn prepare_device(
    tx: &mut Tx<'_>,
    logger: &Logger,
    message: &PushMessage,
    slot: &mut Option<PushDelivery>,
    device_id: Uuid,
    now: Timestamp,
) -> Result<Option<DeviceResult>, PushDeliveryError> {
    if slot.is_none() {
        let created = PushDeliveryRepository
            .create_if_absent(tx, message.event_id, message.user_id)
            .map_err(|error| {
                log_record_failure(logger, message, ApiError::from(error).exception_name());
                PushDeliveryError::Record
            })?;
        *slot = Some(created.ok_or(PushDeliveryError::Record)?);
    }
    let Some(delivery) = slot.as_mut() else {
        return Err(PushDeliveryError::Record);
    };
    if delivery.is_completed() || !delivery.can_send_to(device_id) {
        return Ok(None);
    }
    if delivery
        .device_results
        .get(&device_id)
        .is_some_and(|previous| {
            matches!(
                previous.status,
                DeviceDeliveryStatus::Sending | DeviceDeliveryStatus::Unknown
            )
        })
    {
        logger
            .warn(DELIVERY_ATTEMPT_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "worker_delivery_previous_unknown")
            .field("eventId", message.event_id.to_string())
            .field("userId", message.user_id.to_string())
            .field("deviceId", device_id.to_string())
            .log("이전 FCM 요청 결과 불명 기록을 재처리");
    }
    let started = delivery.start_device(device_id, now)?;
    save_in(tx, logger, message, delivery, device_id, started.clone())?;
    Ok(Some(started))
}

fn save_in(
    db: &mut impl Db,
    logger: &Logger,
    message: &PushMessage,
    delivery: &mut PushDelivery,
    device_id: Uuid,
    result: DeviceResult,
) -> Result<(), PushDeliveryError> {
    match PushDeliveryRepository.save_device_result(db, delivery.id, device_id, &result) {
        Ok(true) => {
            delivery.record_device_result(device_id, result);
            Ok(())
        }
        Ok(false) => Err(PushDeliveryError::Record),
        Err(error) => {
            log_record_failure(logger, message, ApiError::from(error).exception_name());
            Err(PushDeliveryError::Record)
        }
    }
}

fn save_device_result(
    pool: &Pool,
    logger: &Logger,
    message: &PushMessage,
    delivery: &mut PushDelivery,
    device_id: Uuid,
    result: DeviceResult,
) -> Result<(), PushDeliveryError> {
    let mut conn = match pool.get() {
        Ok(conn) => conn,
        Err(error) => {
            log_record_failure(logger, message, ApiError::from(error).exception_name());
            return Err(PushDeliveryError::Record);
        }
    };
    save_in(&mut conn, logger, message, delivery, device_id, result)
}

fn log_record_failure(logger: &Logger, message: &PushMessage, error_code: &str) {
    logger
        .warn(DELIVERY_ATTEMPT_LOGGER)
        .field("type", TYPE_NOTIFICATION)
        .field("stage", "worker_delivery_record_failed")
        .field("eventId", message.event_id.to_string())
        .field("userId", message.user_id.to_string())
        .field("errorCode", error_code)
        .log("푸시 발송 기록 저장 실패");
}

fn to_device_status(result: &DevicePushResult) -> DeviceDeliveryStatus {
    if result.is_accepted() {
        DeviceDeliveryStatus::Accepted
    } else if result.is_retryable() {
        DeviceDeliveryStatus::Retryable
    } else if result.is_invalid_token() {
        DeviceDeliveryStatus::InvalidToken
    } else if result.is_permanent_failure() {
        DeviceDeliveryStatus::PermanentFailure
    } else {
        DeviceDeliveryStatus::Skipped
    }
}
