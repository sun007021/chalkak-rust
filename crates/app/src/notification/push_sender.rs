//! 기기 하나에 푸시를 보내는 경계. 원본 `DevicePushRequest`, `DevicePushResult`, `DevicePushSender`,
//! `PushProcessingResult`.

use std::time::Duration;

use jiff::Timestamp;

use super::sqs_push::PushMessage;

/// 기기 한 대에 보낼 요청(`DevicePushRequest`). `expires_at`은 이 푸시를 보낼 수 있는 마지막 시각이다(정각부터 만료).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePushRequest {
    pub message: PushMessage,
    pub token: String,
    pub title: String,
    pub body: String,
    pub expires_at: Timestamp,
}

/// 기기 하나의 처리 결과 종류(`DevicePushResult.Status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceStatus {
    Accepted,
    Skipped,
    Retryable,
    InvalidToken,
    PermanentFailure,
}

impl DeviceStatus {
    /// 로그에 남기는 이름(Java enum 이름).
    pub const fn as_str(self) -> &'static str {
        match self {
            DeviceStatus::Accepted => "ACCEPTED",
            DeviceStatus::Skipped => "SKIPPED",
            DeviceStatus::Retryable => "RETRYABLE",
            DeviceStatus::InvalidToken => "INVALID_TOKEN",
            DeviceStatus::PermanentFailure => "PERMANENT_FAILURE",
        }
    }
}

/// 기기 하나의 처리 결과(`DevicePushResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePushResult {
    pub status: DeviceStatus,
    pub error_code: Option<String>,
    pub retry_after: Duration,
}

impl DevicePushResult {
    pub fn accepted() -> Self {
        Self::of(DeviceStatus::Accepted, None, Duration::ZERO)
    }

    pub fn retryable(error_code: &str) -> Self {
        Self::retryable_after(error_code, Duration::ZERO)
    }

    pub fn retryable_after(error_code: &str, retry_after: Duration) -> Self {
        Self::of(DeviceStatus::Retryable, Some(error_code), retry_after)
    }

    pub fn invalid_token(error_code: &str) -> Self {
        Self::of(DeviceStatus::InvalidToken, Some(error_code), Duration::ZERO)
    }

    pub fn permanent_failure(error_code: &str) -> Self {
        Self::of(
            DeviceStatus::PermanentFailure,
            Some(error_code),
            Duration::ZERO,
        )
    }

    pub fn skipped(reason: &str) -> Self {
        Self::of(DeviceStatus::Skipped, Some(reason), Duration::ZERO)
    }

    fn of(status: DeviceStatus, error_code: Option<&str>, retry_after: Duration) -> Self {
        Self {
            status,
            error_code: error_code.map(str::to_owned),
            retry_after,
        }
    }

    pub fn is_accepted(&self) -> bool {
        self.status == DeviceStatus::Accepted
    }

    pub fn is_retryable(&self) -> bool {
        self.status == DeviceStatus::Retryable
    }

    pub fn is_invalid_token(&self) -> bool {
        self.status == DeviceStatus::InvalidToken
    }

    pub fn is_permanent_failure(&self) -> bool {
        self.status == DeviceStatus::PermanentFailure
    }
}

/// 발송기가 결과를 만들지 못하고 실패했다(원본에서는 `send`가 던진 `RuntimeException`). `exception`은 로그의 `errorCode`로
/// 남기는 예외 이름이다. 메시지는 토큰이나 자격증명이 섞일 수 있어 담지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderFailure {
    pub exception: String,
}

impl SenderFailure {
    pub fn new(exception: &str) -> Self {
        Self {
            exception: exception.to_owned(),
        }
    }
}

/// `DevicePushSender`
pub trait DevicePushSender: Send + Sync {
    fn send(&self, request: &DevicePushRequest) -> Result<DevicePushResult, SenderFailure>;
}

/// 사용자 메시지 하나의 처리 결과(`PushProcessingResult`). 재시도해야 하면 SQS 숨김 시간을 `retry_after`만큼 미룬다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PushProcessingResult {
    pub retryable: bool,
    pub retry_after: Duration,
}

impl PushProcessingResult {
    pub fn completed() -> Self {
        Self {
            retryable: false,
            retry_after: Duration::ZERO,
        }
    }
}
