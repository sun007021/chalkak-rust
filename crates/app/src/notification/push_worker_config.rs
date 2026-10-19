//! 푸시 Worker 조립. 원본 `PushWorkerConfiguration`: Firebase 설정을 읽고 FCM 발송기, 업무 서비스, SQS 수신 서비스를 만든다.
//!
//! 설정(`chalkak.notification.fcm.*`):
//! - `NOTIFICATION_WORKER_ENABLED`: 켜면 Firebase 프로젝트와 서비스 계정 파일 경로가 있어야 한다(없으면 기동을 거절한다).
//! - `NOTIFICATION_FCM_PROJECT_ID`, `NOTIFICATION_FCM_CREDENTIALS_PATH`: 프로젝트 ID와 서비스 계정 JSON 파일 경로.
//! - `NOTIFICATION_FCM_ENDPOINT_URL`: 원본에는 없는 설정이다. 원본은 FCM 주소(`https://fcm.googleapis.com`)를 바꿀 수 없어
//!   로컬 스택·하네스가 가짜 FCM 서버를 쓸 수 없다. 비워 두면 원본과 같다. OAuth 주소는 원본처럼 서비스 계정 JSON의 `token_uri`다.
//!
//! 큐 URL·리전·엔드포인트는 Relay와 같은 `NOTIFICATION_SQS_QUEUE_URL`·`AWS_REGION`·`AWS_ENDPOINT_URL_SQS`를 쓴다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_core::config::{ConfigError, Env};
use chalkak_core::log::Logger;
use chalkak_core::{Clock, ShutdownSignal};
use chalkak_db::Pool;

use super::fcm::FcmDevicePushSender;
use super::fcm::auth::{ServiceAccount, TokenProvider};
use super::fcm::client::FirebaseMessagingClient;
use super::fcm::transport::{ClientExchange, HttpExchange};
use super::push_worker::PushWorkerService;
use super::sqs_push::{SqsPushSettings, build_sqs_client};
use super::sqs_worker::{SqsPushService, SqsWorkerQueue, ThreadHeartbeatScheduler};

/// FCM 기본 주소.
pub const DEFAULT_FCM_ENDPOINT: &str = "https://fcm.googleapis.com";
/// 설정이 비었을 때의 오류 설명(`PushWorkerConfiguration`의 예외 메시지).
const MISSING_FCM_SETTINGS: &str =
    "PushWorker를 켜려면 Firebase 프로젝트와 서버 인증 파일 경로가 필요합니다.";
/// FCM 요청의 연결·읽기 제한(`FirebaseOptions.setConnectTimeout(10000)`, `setReadTimeout(10000)`).
const FCM_TIMEOUT: Duration = Duration::from_secs(10);
/// OAuth 토큰 교환의 연결·읽기 제한(google-auth-library 기본 HTTP 요청 제한 20초).
const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
/// SQS 전송의 연결·읽기 제한. 수신 호출이 롱 폴링 20초를 기다리므로 호출 전체 제한(`apiCallTimeout` 30초)에 맞춘다.
const SQS_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// `chalkak.notification.fcm.*`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FcmSettings {
    pub project_id: String,
    pub credentials_path: String,
    /// `NOTIFICATION_FCM_ENDPOINT_URL`(없으면 [`DEFAULT_FCM_ENDPOINT`]).
    pub endpoint_url: String,
}

impl FcmSettings {
    /// `worker_enabled`가 켜져 있으면 프로젝트 ID와 인증 파일 경로가 비어 있지 않아야 한다(`isBlank` 기준: 공백뿐이면 비어 있다).
    pub fn from_env(env: &Env, worker_enabled: bool) -> Result<Self, ConfigError> {
        let settings = FcmSettings {
            project_id: env.string_or("NOTIFICATION_FCM_PROJECT_ID", "").to_owned(),
            credentials_path: env
                .string_or("NOTIFICATION_FCM_CREDENTIALS_PATH", "")
                .to_owned(),
            endpoint_url: env
                .get("NOTIFICATION_FCM_ENDPOINT_URL")
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(DEFAULT_FCM_ENDPOINT)
                .to_owned(),
        };
        if worker_enabled
            && (is_blank(&settings.project_id) || is_blank(&settings.credentials_path))
        {
            return Err(ConfigError::Invalid {
                key: "NOTIFICATION_FCM_PROJECT_ID".to_owned(),
                reason: MISSING_FCM_SETTINGS.to_owned(),
            });
        }
        Ok(settings)
    }
}

fn is_blank(text: &str) -> bool {
    text.chars().all(crate::web::binding::is_java_whitespace)
}

/// 조립한 Worker. 폴링 작업이 [`SqsPushService::poll`]을 부른다.
pub struct PushWorker {
    pub service: Arc<SqsPushService>,
}

/// Worker를 조립한다. 서비스 계정 파일을 읽어 키를 확인하므로 파일이나 키가 올바르지 않으면 기동 때 실패한다.
pub fn build_push_worker(
    sqs: &SqsPushSettings,
    fcm: &FcmSettings,
    pool: &Pool,
    clock: &Arc<dyn Clock>,
    logger: &Arc<Logger>,
    shutdown: &ShutdownSignal,
) -> Result<PushWorker, String> {
    let json = std::fs::read_to_string(&fcm.credentials_path)
        .map_err(|e| format!("서비스 계정 파일을 읽을 수 없습니다: {}", e.kind()))?;
    let account = ServiceAccount::from_json(&json)?;
    let sleep: Arc<dyn Fn(Duration) + Send + Sync> = Arc::new(std::thread::sleep);
    let token_transport: Arc<dyn HttpExchange> =
        Arc::new(ClientExchange::new(TOKEN_TIMEOUT, TOKEN_TIMEOUT)?);
    let fcm_transport: Arc<dyn HttpExchange> =
        Arc::new(ClientExchange::new(FCM_TIMEOUT, FCM_TIMEOUT)?);
    let tokens = TokenProvider::new(
        account,
        token_transport,
        Arc::clone(clock),
        Arc::clone(&sleep),
    );
    let messaging = FirebaseMessagingClient::new(
        &fcm.endpoint_url,
        &fcm.project_id,
        tokens,
        fcm_transport,
        Arc::clone(clock),
        sleep,
    );
    let sender = Arc::new(FcmDevicePushSender::new(
        Arc::new(messaging),
        Arc::clone(clock),
    ));
    let processor = Arc::new(PushWorkerService::new(
        pool.clone(),
        Arc::clone(clock),
        sender,
        Arc::clone(logger),
    ));
    let client = build_sqs_client(sqs, Arc::clone(clock), SQS_TRANSPORT_TIMEOUT)?;
    let queue = Arc::new(SqsWorkerQueue::new(&client, &sqs.queue_url));
    let heartbeat = Arc::new(ThreadHeartbeatScheduler::start().map_err(|e| e.to_string())?);
    Ok(PushWorker {
        service: Arc::new(SqsPushService::new(
            queue,
            processor,
            Arc::clone(clock),
            Arc::clone(logger),
            heartbeat,
            shutdown.clone(),
        )),
    })
}
