//! 알림 푸시의 SQS 발행. 원본 `PushMessage`, `PushPublicationResult`, `PushMessagePublisher`, `SqsPushMessagePublisher`,
//! `SqsPushProperties`, `SqsPushConfiguration`의 SQS 클라이언트 부분.
//!
//! - 메시지 JSON은 원본 Jackson 출력과 바이트가 같다: 필드 순서는 선언 순서, 시각은 `Instant.toString` 표기, 값이 없는
//!   알림함 ID는 `null`이다.
//! - SQS 호출은 5초 안에 끝내고 자동 재시도는 하지 않는다(`apiCallTimeout`·`apiCallAttemptTimeout` 5초, `maxAttempts` 1).
//!   응답을 확인하지 못한 경우는 수락 여부 불명이다.
//! - 원본의 직렬화 실패(`SQS_MESSAGE_SERIALIZATION_ERROR`)는 옮기지 않았다. 고정 구조를 손으로 쓰므로 실패할 수 없다.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use chalkak_aws::credentials::{HttpRequest, HttpTransport, NowFn};
use chalkak_aws::{
    AwsError, ChainCredentialsProvider, SqsClient, SqsEndpoint, SqsError, SqsHttpResponse,
    SqsTransport, is_standard_queue_url,
};
use chalkak_core::Clock;
use chalkak_core::config::{ConfigError, Env};
use chalkak_http::client::{HttpClient, Method};
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::{NotificationSourceType, NotificationType};
use crate::user::storage::ClientTransport;
use crate::web::json::format_instant;

/// 큐 URL이 올바르지 않을 때의 설명(`SqsPushProperties`의 예외 메시지).
const INVALID_QUEUE_URL: &str = "알림 푸시를 사용하려면 Standard SQS 큐 URL이 필요합니다.";
/// 오류 코드로 로그에 남겨도 되는 모양(`[A-Za-z0-9._-]{1,100}`).
const MAX_ERROR_CODE_LENGTH: usize = 100;

/// SQS로 보내는 푸시 메시지(`PushMessage`). 승인·반려는 `notification_id`가 있고 주제 푸시는 없다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushMessage {
    pub event_id: Uuid,
    pub notification_id: Option<Uuid>,
    pub user_id: Uuid,
    pub kind: NotificationType,
    pub source_type: Option<NotificationSourceType>,
    pub source_id: Option<Uuid>,
    pub occurred_at: Timestamp,
    pub expires_at: Timestamp,
}

impl PushMessage {
    /// SQS `MessageBody`. 원본 `ObjectMapper.writeValueAsString(message)`와 같은 바이트다.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(400);
        let uuid = |out: &mut String, key: &str, value: Option<Uuid>| match value {
            Some(id) => {
                let _ = write!(out, "\"{key}\":\"{id}\"");
            }
            None => {
                let _ = write!(out, "\"{key}\":null");
            }
        };
        out.push('{');
        uuid(&mut out, "eventId", Some(self.event_id));
        out.push(',');
        uuid(&mut out, "notificationId", self.notification_id);
        out.push(',');
        uuid(&mut out, "userId", Some(self.user_id));
        let _ = write!(out, ",\"type\":\"{}\"", self.kind.as_str());
        match self.source_type {
            Some(t) => {
                let _ = write!(out, ",\"sourceType\":\"{}\"", t.as_str());
            }
            None => out.push_str(",\"sourceType\":null"),
        }
        out.push(',');
        uuid(&mut out, "sourceId", self.source_id);
        let _ = write!(
            out,
            ",\"occurredAt\":\"{}\",\"expiresAt\":\"{}\"}}",
            format_instant(self.occurred_at),
            format_instant(self.expires_at)
        );
        out
    }
}

/// 발행 결과의 상태(`PushPublicationResult.Status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationStatus {
    Accepted,
    Failed,
    Unknown,
}

/// 발행 결과(`PushPublicationResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushPublicationResult {
    pub status: PublicationStatus,
    pub message_id: Option<String>,
    pub error_code: Option<String>,
    pub retryable: bool,
}

impl PushPublicationResult {
    pub fn accepted(message_id: &str) -> Self {
        Self {
            status: PublicationStatus::Accepted,
            message_id: Some(message_id.to_owned()),
            error_code: None,
            retryable: false,
        }
    }

    /// 일시 거절: 확실히 수락되지 않았고 다시 시도할 수 있다.
    pub fn retry(error_code: &str) -> Self {
        Self::error(PublicationStatus::Failed, error_code, true)
    }

    /// 수락 여부 불명: 다시 시도하되 이미 수락됐을 수 있다.
    pub fn unknown(error_code: &str) -> Self {
        Self::error(PublicationStatus::Unknown, error_code, true)
    }

    /// 영구 실패
    pub fn failed(error_code: &str) -> Self {
        Self::error(PublicationStatus::Failed, error_code, false)
    }

    fn error(status: PublicationStatus, error_code: &str, retryable: bool) -> Self {
        Self {
            status,
            message_id: None,
            error_code: Some(error_code.to_owned()),
            retryable,
        }
    }

    /// 수락 ID가 있어야 수락으로 본다(`isAccepted`; 공백뿐인 ID는 없는 것과 같다).
    pub fn is_accepted(&self) -> bool {
        self.status == PublicationStatus::Accepted
            && self
                .message_id
                .as_deref()
                .is_some_and(|id| !id.chars().all(crate::web::binding::is_java_whitespace))
    }

    pub fn is_acceptance_unknown(&self) -> bool {
        self.status == PublicationStatus::Unknown
    }
}

/// 발행기가 결과를 만들지 못하고 실패했다(원본에서는 `publish`가 던진 `RuntimeException`). `exception`은 로그의
/// `errorCode`로 남기는 예외 이름이다. 메시지는 요청 본문이나 자격증명이 섞일 수 있어 담지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherFailure {
    pub exception: String,
}

/// `PushMessagePublisher`
pub trait PushMessagePublisher: Send + Sync {
    fn publish(&self, message: &PushMessage) -> Result<PushPublicationResult, PublisherFailure>;
}

/// `SqsPushMessagePublisher`: SQS 표준 큐에 한 건을 보내고 결과를 분류한다.
pub struct SqsPushMessagePublisher {
    client: SqsClient,
    queue_url: String,
}

impl SqsPushMessagePublisher {
    pub fn new(client: SqsClient, queue_url: &str) -> Self {
        Self {
            client,
            queue_url: queue_url.to_owned(),
        }
    }
}

impl PushMessagePublisher for SqsPushMessagePublisher {
    fn publish(&self, message: &PushMessage) -> Result<PushPublicationResult, PublisherFailure> {
        Ok(
            match self
                .client
                .send_message(&self.queue_url, &message.to_json())
            {
                Ok(output) => match output.message_id {
                    Some(id) if !id.chars().all(crate::web::binding::is_java_whitespace) => {
                        PushPublicationResult::accepted(&id)
                    }
                    _ => PushPublicationResult::unknown("SQS_ACCEPTANCE_UNKNOWN"),
                },
                Err(SqsError::Service(error)) => {
                    let code = safe_error_code(error.code.as_deref());
                    if error.status >= 500 || error.status < 400 {
                        PushPublicationResult::unknown(&code)
                    } else if error.is_throttling() {
                        PushPublicationResult::retry(&code)
                    } else {
                        PushPublicationResult::failed(&code)
                    }
                }
                // 응답을 확인하지 못했으므로 수락 여부를 보수적으로 불명 처리한다.
                Err(SqsError::Client(_)) => PushPublicationResult::unknown("SQS_CLIENT_ERROR"),
            },
        )
    }
}

/// 외부가 준 오류 코드는 알려진 문자만 있을 때만 로그에 남긴다(`getSafeErrorCode`).
fn safe_error_code(code: Option<&str>) -> String {
    match code {
        Some(code)
            if (1..=MAX_ERROR_CODE_LENGTH).contains(&code.len())
                && code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) =>
        {
            code.to_owned()
        }
        _ => "SQS_SERVICE_ERROR".to_owned(),
    }
}

/// `SqsPushProperties`(`chalkak.notification.sqs.*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqsPushSettings {
    /// `NOTIFICATION_RELAY_ENABLED`(기본 꺼짐)
    pub relay_enabled: bool,
    /// `NOTIFICATION_TOPIC_PUSH_ENABLED`(기본 꺼짐)
    pub topic_push_enabled: bool,
    /// `NOTIFICATION_WORKER_ENABLED`(기본 꺼짐). 켜면 이 서버가 큐에서 메시지를 받아 FCM으로 보낸다.
    pub worker_enabled: bool,
    /// `NOTIFICATION_SQS_QUEUE_URL`
    pub queue_url: String,
    /// `AWS_REGION`
    pub region: String,
    /// SDK 공통 `AWS_ENDPOINT_URL_SQS`(없으면 `AWS_ENDPOINT_URL`). 로컬 스택·하네스 가짜 서버용이다.
    pub endpoint_url: Option<String>,
}

impl SqsPushSettings {
    pub fn from_env(env: &Env) -> Result<Self, ConfigError> {
        let settings = SqsPushSettings {
            relay_enabled: env.bool_or("NOTIFICATION_RELAY_ENABLED", false)?,
            topic_push_enabled: env.bool_or("NOTIFICATION_TOPIC_PUSH_ENABLED", false)?,
            worker_enabled: env.bool_or("NOTIFICATION_WORKER_ENABLED", false)?,
            queue_url: env.string_or("NOTIFICATION_SQS_QUEUE_URL", "").to_owned(),
            region: env.string_or("AWS_REGION", "ap-northeast-2").to_owned(),
            endpoint_url: ["AWS_ENDPOINT_URL_SQS", "AWS_ENDPOINT_URL"]
                .iter()
                .find_map(|key| env.get(key).map(str::trim).filter(|v| !v.is_empty()))
                .map(str::to_owned),
        };
        // 발행이나 Worker를 켰는데 Standard 큐 URL이 아니면 기동 때 거절한다(Relay와 Worker가 같은 조건이다).
        if settings.queue_required() && !is_standard_queue_url(&settings.queue_url) {
            return Err(ConfigError::Invalid {
                key: "NOTIFICATION_SQS_QUEUE_URL".to_owned(),
                reason: INVALID_QUEUE_URL.to_owned(),
            });
        }
        if settings.queue_required()
            && let Some(url) = &settings.endpoint_url
        {
            SqsEndpoint::custom(url).map_err(|e| ConfigError::Invalid {
                key: "AWS_ENDPOINT_URL_SQS".to_owned(),
                reason: e.to_string(),
            })?;
        }
        Ok(settings)
    }

    /// 어느 쪽이든 발행이 켜져 있으면 SQS 발행기가 필요하다.
    pub fn publishing_enabled(&self) -> bool {
        self.relay_enabled || self.topic_push_enabled
    }

    /// 발행이나 수신 중 하나라도 켜져 있으면 올바른 큐 URL이 필요하다.
    pub fn queue_required(&self) -> bool {
        self.publishing_enabled() || self.worker_enabled
    }
}

/// 설정에 맞는 SQS 클라이언트. 자격증명은 환경변수 -> IMDS 순서(`DefaultCredentialsProvider`)다. `transport_timeout`은
/// 연결·읽기 제한이고, 호출별 제한(`SqsClient::with_timeout`)은 쓰는 쪽이 정한다.
pub fn build_sqs_client(
    settings: &SqsPushSettings,
    clock: Arc<dyn Clock>,
    transport_timeout: Duration,
) -> Result<SqsClient, String> {
    let endpoint = match &settings.endpoint_url {
        Some(url) => SqsEndpoint::custom(url),
        None => SqsEndpoint::aws(&settings.region),
    }
    .map_err(|e| e.to_string())?;
    let imds: Arc<dyn HttpTransport> = Arc::new(ClientTransport::new()?);
    let now: NowFn = {
        let clock = Arc::clone(&clock);
        Arc::new(move || clock.now())
    };
    let credentials = Arc::new(ChainCredentialsProvider::default_chain(
        imds,
        Arc::clone(&now),
    ));
    Ok(SqsClient::new(
        &settings.region,
        endpoint,
        credentials,
        Arc::new(HttpSqsTransport::new(transport_timeout)?),
        now,
    ))
}

/// 설정에 맞는 SQS 발행기. SQS 호출은 5초 안에 끝내고 재시도하지 않는다(`SqsPushConfiguration.notificationSqsClient`).
pub fn build_publisher(
    settings: &SqsPushSettings,
    clock: Arc<dyn Clock>,
) -> Result<Arc<dyn PushMessagePublisher>, String> {
    let client = build_sqs_client(settings, clock, chalkak_aws::sqs::DEFAULT_TIMEOUT)?;
    Ok(Arc::new(SqsPushMessagePublisher::new(
        client,
        &settings.queue_url,
    )))
}

/// [`SqsTransport`]를 `HttpClient`로 구현한다. 요청 하나의 전체 제한(`timeout`)을 지키기 위해 별도 스레드에서 보내고
/// 시간이 지나면 기다리지 않는다. 남은 스레드는 클라이언트의 연결·읽기 제한으로 곧 끝난다.
pub struct HttpSqsTransport {
    client: Arc<HttpClient>,
}

impl HttpSqsTransport {
    pub fn new(timeout: Duration) -> Result<Self, String> {
        let client = HttpClient::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client: Arc::new(client),
        })
    }
}

impl SqsTransport for HttpSqsTransport {
    fn send(&self, request: &HttpRequest, timeout: Duration) -> Result<SqsHttpResponse, AwsError> {
        let client = Arc::clone(&self.client);
        let request = request.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("sqs-request".to_owned())
            .spawn(move || {
                let headers: Vec<(&str, &str)> = request
                    .headers
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_str()))
                    .collect();
                let result = client
                    .request(Method::Post, &request.url, &headers, Some(&request.body))
                    .map(|response| SqsHttpResponse {
                        status: response.status,
                        headers: response.headers,
                        body: response.body,
                    })
                    .map_err(|e| AwsError::Transport(e.to_string()));
                let _ = sender.send(result);
            })
            .map_err(|e| AwsError::Transport(e.to_string()))?;
        receiver
            .recv_timeout(timeout)
            .map_err(|_| AwsError::Transport("SQS 호출 시간 초과".to_owned()))?
    }
}

#[cfg(test)]
#[path = "sqs_push_tests.rs"]
mod tests;
