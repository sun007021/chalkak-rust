//! FCM 발송. 원본 `FcmDevicePushSender`와 Firebase Admin SDK 9.11.0·google-auth-library 1.52.0의 FCM HTTP v1 호출.
//!
//! 원본은 Admin SDK로 `messages:send`를 부른다. 여기서는 SDK 없이 같은 요청(서비스 계정 JSON -> RS256 JWT -> OAuth 토큰 교환 ->
//! `POST /v1/projects/<프로젝트>/messages:send`)을 직접 보낸다. 요청 바이트와 응답 분류, 재시도는 원본 JAR의 SDK에 같은 입력을
//! 돌려 얻은 기록(`tests/fixtures/expected_fcm.tsv`, 생성은 `FcmFixture.java`)과 같다(`tests/fcm_java_parity.rs`).
//!
//! - [`FcmDevicePushSender`]는 원본 `FcmDevicePushSender`다: 기한을 확인하고 메시지를 만들고 SDK 오류를 푸시 결과로 분류한다.
//! - [`client::FirebaseMessagingClient`]는 SDK의 `FirebaseMessaging.send`다: 토큰, 503 재시도, 401 토큰 갱신, 오류 응답 해석.
//! - SDK가 `FirebaseMessagingException`이 아닌 예외(`IllegalArgumentException` 등)를 던지는 경우는 [`MessagingFailure::Thrown`]이고,
//!   발송기는 이를 그대로 실패([`SenderFailure`])로 올려 PushWorker가 기기 단위 재시도로 다룬다.
//! - 원본이 FCM 주소(`https://fcm.googleapis.com`)를 바꿀 수 없으므로 로컬 스택·하네스용으로 `NOTIFICATION_FCM_ENDPOINT_URL`을
//!   더했다. OAuth 주소는 원본처럼 서비스 계정 JSON의 `token_uri`다.

pub mod auth;
pub mod client;
pub mod message;
pub mod retry_after;
pub mod transport;

use std::sync::Arc;
use std::time::Duration;

use chalkak_core::Clock;
use jiff::Timestamp;

use self::message::FcmMessage;
use super::domain::is_push_expired_at;
use super::push_sender::{DevicePushRequest, DevicePushResult, DevicePushSender, SenderFailure};

/// `MINIMUM_RETRY_DELAY`: 재시도 지연의 최소 간격
const MINIMUM_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Firebase `ErrorCode`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidArgument,
    FailedPrecondition,
    OutOfRange,
    Unauthenticated,
    PermissionDenied,
    NotFound,
    Conflict,
    Aborted,
    AlreadyExists,
    ResourceExhausted,
    Cancelled,
    DataLoss,
    Unknown,
    Internal,
    Unavailable,
    DeadlineExceeded,
}

impl ErrorCode {
    const ALL: [ErrorCode; 16] = [
        ErrorCode::InvalidArgument,
        ErrorCode::FailedPrecondition,
        ErrorCode::OutOfRange,
        ErrorCode::Unauthenticated,
        ErrorCode::PermissionDenied,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::Aborted,
        ErrorCode::AlreadyExists,
        ErrorCode::ResourceExhausted,
        ErrorCode::Cancelled,
        ErrorCode::DataLoss,
        ErrorCode::Unknown,
        ErrorCode::Internal,
        ErrorCode::Unavailable,
        ErrorCode::DeadlineExceeded,
    ];

    /// Java enum 이름(`ErrorCode.name()`)
    pub const fn name(self) -> &'static str {
        match self {
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::FailedPrecondition => "FAILED_PRECONDITION",
            ErrorCode::OutOfRange => "OUT_OF_RANGE",
            ErrorCode::Unauthenticated => "UNAUTHENTICATED",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::Aborted => "ABORTED",
            ErrorCode::AlreadyExists => "ALREADY_EXISTS",
            ErrorCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorCode::Cancelled => "CANCELLED",
            ErrorCode::DataLoss => "DATA_LOSS",
            ErrorCode::Unknown => "UNKNOWN",
            ErrorCode::Internal => "INTERNAL",
            ErrorCode::Unavailable => "UNAVAILABLE",
            ErrorCode::DeadlineExceeded => "DEADLINE_EXCEEDED",
        }
    }

    /// `ErrorCode.valueOf`: 이름이 정확히 같아야 한다.
    pub fn from_name(name: &str) -> Option<ErrorCode> {
        Self::ALL.into_iter().find(|code| code.name() == name)
    }

    /// 응답 본문에 `status`가 없을 때 HTTP 상태로 정하는 코드(SDK `AbstractHttpErrorHandler`). 표에 없으면 `UNKNOWN`.
    pub fn from_http_status(status: u16) -> ErrorCode {
        match status {
            400 => ErrorCode::InvalidArgument,
            401 => ErrorCode::Unauthenticated,
            403 => ErrorCode::PermissionDenied,
            404 => ErrorCode::NotFound,
            409 => ErrorCode::Conflict,
            429 => ErrorCode::ResourceExhausted,
            500 => ErrorCode::Internal,
            503 => ErrorCode::Unavailable,
            504 => ErrorCode::DeadlineExceeded,
            _ => ErrorCode::Unknown,
        }
    }
}

/// Firebase `MessagingErrorCode`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessagingErrorCode {
    ThirdPartyAuthError,
    InvalidArgument,
    Internal,
    QuotaExceeded,
    SenderIdMismatch,
    Unavailable,
    Unregistered,
}

impl MessagingErrorCode {
    pub const fn name(self) -> &'static str {
        match self {
            MessagingErrorCode::ThirdPartyAuthError => "THIRD_PARTY_AUTH_ERROR",
            MessagingErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            MessagingErrorCode::Internal => "INTERNAL",
            MessagingErrorCode::QuotaExceeded => "QUOTA_EXCEEDED",
            MessagingErrorCode::SenderIdMismatch => "SENDER_ID_MISMATCH",
            MessagingErrorCode::Unavailable => "UNAVAILABLE",
            MessagingErrorCode::Unregistered => "UNREGISTERED",
        }
    }

    /// FCM `FcmError.errorCode` 문자열에서(`APNS_AUTH_ERROR`는 `THIRD_PARTY_AUTH_ERROR`와 같다). 모르는 값은 `None`.
    pub fn from_wire(code: &str) -> Option<MessagingErrorCode> {
        match code {
            "APNS_AUTH_ERROR" | "THIRD_PARTY_AUTH_ERROR" => {
                Some(MessagingErrorCode::ThirdPartyAuthError)
            }
            "INVALID_ARGUMENT" => Some(MessagingErrorCode::InvalidArgument),
            "INTERNAL" => Some(MessagingErrorCode::Internal),
            "QUOTA_EXCEEDED" => Some(MessagingErrorCode::QuotaExceeded),
            "SENDER_ID_MISMATCH" => Some(MessagingErrorCode::SenderIdMismatch),
            "UNAVAILABLE" => Some(MessagingErrorCode::Unavailable),
            "UNREGISTERED" => Some(MessagingErrorCode::Unregistered),
            _ => None,
        }
    }
}

/// `FirebaseMessagingException`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessagingException {
    pub error_code: ErrorCode,
    pub messaging_code: Option<MessagingErrorCode>,
    /// HTTP 응답을 받은 오류면 그 응답 헤더. 응답을 받지 못한 오류(연결 실패 등)면 `None`.
    pub response_headers: Option<Vec<(String, String)>>,
}

/// `FirebaseMessaging.send`가 실패한 모양.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessagingFailure {
    /// `FirebaseMessagingException`
    Exception(MessagingException),
    /// 그 밖의 예외(`IllegalArgumentException` 등). 값은 예외 클래스의 단순 이름이다.
    Thrown(&'static str),
}

/// Firebase `FirebaseMessaging`. 메시지 하나를 보낸다.
pub trait FirebaseMessaging: Send + Sync {
    fn send(&self, message: &FcmMessage) -> Result<(), MessagingFailure>;
}

/// `FcmDevicePushSender`
pub struct FcmDevicePushSender {
    messaging: Arc<dyn FirebaseMessaging>,
    clock: Arc<dyn Clock>,
}

impl FcmDevicePushSender {
    pub fn new(messaging: Arc<dyn FirebaseMessaging>, clock: Arc<dyn Clock>) -> Self {
        Self { messaging, clock }
    }
}

impl DevicePushSender for FcmDevicePushSender {
    fn send(&self, request: &DevicePushRequest) -> Result<DevicePushResult, SenderFailure> {
        let now = self.clock.now();
        if is_push_expired_at(request.expires_at, now) {
            return Ok(DevicePushResult::skipped("EXPIRED"));
        }
        let message = create_message(request, now)?;
        match self.messaging.send(&message) {
            Ok(()) => Ok(DevicePushResult::accepted()),
            Err(MessagingFailure::Exception(exception)) => Ok(self.to_result(&exception)),
            Err(MessagingFailure::Thrown(name)) => Err(SenderFailure::new(name)),
        }
    }
}

impl FcmDevicePushSender {
    fn to_result(&self, exception: &MessagingException) -> DevicePushResult {
        let error_code = exception
            .messaging_code
            .map_or(exception.error_code.name(), MessagingErrorCode::name);
        if exception.messaging_code == Some(MessagingErrorCode::Unregistered) {
            return DevicePushResult::invalid_token(error_code);
        }
        if is_retryable(exception) {
            return DevicePushResult::retryable_after(error_code, self.find_retry_after(exception));
        }
        // INVALID_ARGUMENT은 메시지 형식 오류도 포함하며, 프로젝트·APNs 인증 오류도 토큰 무효를 뜻하지 않는다.
        // 정상 기기를 잘못 삭제하지 않도록 UNREGISTERED만 삭제 대상으로 분류한다.
        DevicePushResult::permanent_failure(error_code)
    }

    fn find_retry_after(&self, exception: &MessagingException) -> Duration {
        let Some(headers) = &exception.response_headers else {
            return MINIMUM_RETRY_DELAY;
        };
        let Some(header) = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Retry-After"))
            .map(|(_, value)| value.trim())
        else {
            return MINIMUM_RETRY_DELAY;
        };
        if let Some(seconds) = retry_after::parse_seconds(header) {
            return normalize(seconds);
        }
        match retry_after::parse_http_date(header) {
            Some(retry_at) => normalize(seconds_until(self.clock.now(), retry_at)),
            None => MINIMUM_RETRY_DELAY,
        }
    }
}

fn seconds_until(now: Timestamp, retry_at: i64) -> i64 {
    // `Duration.between(...).toSeconds()`: 0 쪽으로 버린 초
    let nanos = i128::from(retry_at) * 1_000_000_000 - now.as_nanosecond();
    (nanos / 1_000_000_000) as i64
}

/// 60초보다 길 때만 그 값을, 아니면 60초를 쓴다.
fn normalize(seconds: i64) -> Duration {
    if seconds > MINIMUM_RETRY_DELAY.as_secs() as i64 {
        Duration::from_secs(seconds as u64)
    } else {
        MINIMUM_RETRY_DELAY
    }
}

fn is_retryable(exception: &MessagingException) -> bool {
    matches!(
        exception.messaging_code,
        Some(
            MessagingErrorCode::Unavailable
                | MessagingErrorCode::Internal
                | MessagingErrorCode::QuotaExceeded
        )
    ) || matches!(
        exception.error_code,
        ErrorCode::Unavailable
            | ErrorCode::Internal
            | ErrorCode::DeadlineExceeded
            | ErrorCode::Unknown
    )
}

/// `createMessage`: 알림 제목·본문, 화면 이동 정보, Android TTL, APNs 절대 만료. TTL은 이 시각(`now`) 기준으로 한 번만 계산해
/// SDK가 재시도해도 바뀌지 않는다.
fn create_message(
    request: &DevicePushRequest,
    now: Timestamp,
) -> Result<FcmMessage, SenderFailure> {
    let source = &request.message;
    let (Some(source_type), Some(source_id)) = (source.source_type, source.source_id) else {
        return Err(SenderFailure::new("NullPointerException"));
    };
    let mut data: Vec<(&'static str, String)> = vec![
        ("sourceId", source_id.to_string()),
        ("eventId", source.event_id.to_string()),
        ("sourceType", source_type.as_str().to_owned()),
    ];
    if let Some(notification_id) = source.notification_id {
        data.push(("notificationId", notification_id.to_string()));
    }
    data.push(("type", source.kind.as_str().to_owned()));
    let remaining_nanos = request.expires_at.as_nanosecond() - now.as_nanosecond();
    let ttl_millis = i64::try_from(remaining_nanos / 1_000_000).unwrap_or(i64::MAX);
    if ttl_millis < 0 {
        // SDK `AndroidConfig.Builder.setTtl`이 음수를 거절한다.
        return Err(SenderFailure::new("IllegalArgumentException"));
    }
    Ok(FcmMessage {
        token: request.token.clone(),
        title: request.title.clone(),
        body: request.body.clone(),
        data,
        ttl_millis,
        apns_expiration: request.expires_at.as_second(),
    })
}

#[cfg(test)]
mod tests;
