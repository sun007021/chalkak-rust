//! 알림 SQS 발행 테스트. 원본 `SqsPushMessagePublisherTest`, `SqsPushConfigurationTest`(설정 검증)에 대응한다.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chalkak_aws::sqs::md5_hex;
use chalkak_aws::{Credentials, CredentialsProvider, SqsEndpoint};
use chalkak_core::config::Env;
use serde_json::Value;

use super::*;

const OCCURRED_AT: &str = "2026-10-06T00:00:00Z";
const EXPIRES_AT: &str = "2026-10-06T00:30:00Z";
const QUEUE: &str = "https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-test-push";

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn message() -> PushMessage {
    PushMessage {
        event_id: Uuid::from_u128(1),
        notification_id: Some(Uuid::from_u128(2)),
        user_id: Uuid::from_u128(3),
        kind: NotificationType::PostRejected,
        source_type: Some(NotificationSourceType::Post),
        source_id: Some(Uuid::from_u128(4)),
        occurred_at: ts(OCCURRED_AT),
        expires_at: ts(EXPIRES_AT),
    }
}

struct StaticCredentials;
impl CredentialsProvider for StaticCredentials {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        Ok(Credentials {
            access_key_id: "test-key".into(),
            secret_access_key: "test-secret".into(),
            session_token: None,
        })
    }
}

type Respond = dyn Fn(&HttpRequest) -> Result<SqsHttpResponse, AwsError> + Send + Sync;

/// 요청마다 `respond`가 정한 응답을 돌려주는 전송.
struct Fake {
    respond: Box<Respond>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl SqsTransport for Fake {
    fn send(&self, request: &HttpRequest, _timeout: Duration) -> Result<SqsHttpResponse, AwsError> {
        self.requests.lock().unwrap().push(request.clone());
        (self.respond)(request)
    }
}

fn publisher_with(
    endpoint: &str,
    transport: Arc<dyn SqsTransport>,
    timeout: Option<Duration>,
) -> SqsPushMessagePublisher {
    let mut client = SqsClient::new(
        "ap-northeast-2",
        SqsEndpoint::custom(endpoint).unwrap(),
        Arc::new(StaticCredentials),
        transport,
        Arc::new(|| ts("2026-10-06T00:00:00Z")),
    );
    if let Some(timeout) = timeout {
        client = client.with_timeout(timeout);
    }
    SqsPushMessagePublisher::new(client, QUEUE)
}

fn fake(
    respond: impl Fn(&HttpRequest) -> Result<SqsHttpResponse, AwsError> + Send + Sync + 'static,
) -> Arc<Fake> {
    Arc::new(Fake {
        respond: Box::new(respond),
        requests: Mutex::new(Vec::new()),
    })
}

fn response(status: u16, body: &str) -> Result<SqsHttpResponse, AwsError> {
    Ok(SqsHttpResponse {
        status,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    })
}

/// SendMessage 요청의 `MessageBody`로 `MD5OfMessageBody`를 맞춰 성공 응답을 만든다.
fn accepted(request: &HttpRequest, id_field: &str) -> Result<SqsHttpResponse, AwsError> {
    let envelope: Value = serde_json::from_slice(&request.body).unwrap();
    let body = envelope["MessageBody"].as_str().unwrap();
    response(
        200,
        &format!(
            "{{{id_field}\"MD5OfMessageBody\":\"{}\"}}",
            md5_hex(body.as_bytes())
        ),
    )
}

fn publisher(transport: Arc<Fake>) -> SqsPushMessagePublisher {
    publisher_with("http://127.0.0.1:1", transport, None)
}

fn sent_message_body(transport: &Fake) -> String {
    let requests = transport.requests.lock().unwrap();
    let envelope: Value = serde_json::from_slice(&requests[0].body).unwrap();
    envelope["MessageBody"].as_str().unwrap().to_owned()
}

// SqsPushMessagePublisherTest#publish_acceptedHttpResponse_sendsMinimalJsonAndReturnsMessageId
#[test]
fn 수락_응답이면_식별_정보만_담아_보내고_수락_id를_돌려준다() {
    let transport = fake(|r| accepted(r, "\"MessageId\":\"sqs-accepted-id\","));
    let result = publisher(transport.clone()).publish(&message()).unwrap();

    assert!(result.is_accepted());
    assert_eq!(result.message_id.as_deref(), Some("sqs-accepted-id"));
    assert_eq!(result.error_code, None);
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].header("x-amz-target"),
            Some("AmazonSQS.SendMessage")
        );
        assert_eq!(
            requests[0].header("content-type"),
            Some("application/x-amz-json-1.0")
        );
    }
    let body: Value = serde_json::from_str(&sent_message_body(&transport)).unwrap();
    let object = body.as_object().unwrap();
    assert_eq!(object.len(), 8);
    assert_eq!(body["eventId"], Uuid::from_u128(1).to_string());
    assert_eq!(body["notificationId"], Uuid::from_u128(2).to_string());
    assert_eq!(body["userId"], Uuid::from_u128(3).to_string());
    assert_eq!(body["type"], "POST_REJECTED");
    assert_eq!(body["sourceType"], "POST");
    assert_eq!(body["sourceId"], Uuid::from_u128(4).to_string());
    assert_eq!(body["occurredAt"], OCCURRED_AT);
    assert_eq!(body["expiresAt"], EXPIRES_AT);
    for forbidden in ["rejectionReason", "fcmToken", "originalImageUrl"] {
        assert!(!object.contains_key(forbidden), "{forbidden}");
    }
}

// SqsPushMessagePublisherTest#publish_topicMessage_preservesContractAndRoundTrip
#[test]
fn 주제_메시지도_원본_시각과_null_알림함_id를_json으로_전달한다() {
    let topic = PushMessage {
        notification_id: None,
        kind: NotificationType::Topic,
        source_type: Some(NotificationSourceType::Topic),
        ..message()
    };
    let transport = fake(|r| accepted(r, "\"MessageId\":\"sqs-accepted-id\","));
    let result = publisher(transport.clone()).publish(&topic).unwrap();

    assert!(result.is_accepted());
    let text = sent_message_body(&transport);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 8);
    assert!(body["notificationId"].is_null());
    assert_eq!(body["type"], "TOPIC");
    assert!(body["expiresAt"].is_string());
    // 필드 순서와 표기까지 원본 Jackson 출력과 같다.
    assert_eq!(
        text,
        "{\"eventId\":\"00000000-0000-0000-0000-000000000001\",\"notificationId\":null,\
         \"userId\":\"00000000-0000-0000-0000-000000000003\",\"type\":\"TOPIC\",\
         \"sourceType\":\"TOPIC\",\"sourceId\":\"00000000-0000-0000-0000-000000000004\",\
         \"occurredAt\":\"2026-10-06T00:00:00Z\",\"expiresAt\":\"2026-10-06T00:30:00Z\"}"
    );
}

#[test]
fn 시각의_소수초는_instant_표기를_따른다() {
    let m = PushMessage {
        occurred_at: ts("2026-10-06T00:00:00.120Z"),
        expires_at: ts("2026-10-06T00:30:00.123456Z"),
        source_type: None,
        source_id: None,
        ..message()
    };
    let body: Value = serde_json::from_str(&m.to_json()).unwrap();
    assert_eq!(body["occurredAt"], "2026-10-06T00:00:00.120Z");
    assert_eq!(body["expiresAt"], "2026-10-06T00:30:00.123456Z");
    assert!(body["sourceType"].is_null() && body["sourceId"].is_null());
}

// SqsPushMessagePublisherTest#publish_serviceError_classifiesRetryability
#[test]
fn sqs_오류_코드와_상태를_재시도_여부로_바꾸고_원문을_전달하지_않는다() {
    // (상태, 코드, 재시도, 수락 불명)
    for (status, code, retryable, unknown) in [
        (500, "ServiceUnavailable", true, true),
        (503, "InternalError", true, true),
        (400, "RequestThrottled", true, false),
        (429, "TooManyRequests", true, false),
        (403, "AccessDenied", false, false),
        (400, "QueueDoesNotExist", false, false),
        (400, "InvalidMessageContents", false, false),
    ] {
        let body = format!("{{\"__type\":\"{code}\",\"message\":\"SECRET_REQUEST_BODY\"}}");
        let transport = fake(move |_| response(status, &body));
        let result = publisher(transport.clone()).publish(&message()).unwrap();

        assert!(!result.is_accepted(), "{status} {code}");
        assert_eq!(result.retryable, retryable, "{status} {code}");
        assert_eq!(result.is_acceptance_unknown(), unknown, "{status} {code}");
        assert_eq!(result.error_code.as_deref(), Some(code));
        assert!(!format!("{result:?}").contains("SECRET_REQUEST_BODY"));
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            1,
            "재시도하지 않는다"
        );
    }
}

// SqsPushMessagePublisherTest#publish_invalidErrorCode_returnsSafeErrorCode
#[test]
fn 오류_코드_형식이_부적절하면_고정_코드로_바꾼다() {
    for code in [
        "SECRET PRIVATE BODY".to_owned(),
        "오류".to_owned(),
        "A".repeat(101),
        String::new(),
    ] {
        let body = format!("{{\"__type\":\"{code}\",\"message\":\"SECRET_REQUEST_BODY\"}}");
        let result = publisher(fake(move |_| response(500, &body)))
            .publish(&message())
            .unwrap();
        assert_eq!(
            result.error_code.as_deref(),
            Some("SQS_SERVICE_ERROR"),
            "{code:?}"
        );
    }
    // 코드가 아예 없어도 같다. 100자는 허용한다.
    let none = publisher(fake(|_| response(500, "<html>bad</html>")))
        .publish(&message())
        .unwrap();
    assert_eq!(none.error_code.as_deref(), Some("SQS_SERVICE_ERROR"));
    let longest = "A".repeat(100);
    let body = format!("{{\"__type\":\"{longest}\"}}");
    let ok = publisher(fake(move |_| response(400, &body)))
        .publish(&message())
        .unwrap();
    assert_eq!(ok.error_code.as_deref(), Some(longest.as_str()));
}

// SqsPushMessagePublisherTest#publish_missingAcceptanceId_retriesUnknownOutcome
#[test]
fn 수락_id가_없으면_성공으로_보지_않고_결과_불명으로_재시도한다() {
    for id_field in ["", "\"MessageId\":\"\",", "\"MessageId\":\" \","] {
        let result = publisher(fake(move |r| accepted(r, id_field)))
            .publish(&message())
            .unwrap();
        assert!(!result.is_accepted(), "{id_field}");
        assert!(result.retryable);
        assert!(result.is_acceptance_unknown());
        assert_eq!(result.error_code.as_deref(), Some("SQS_ACCEPTANCE_UNKNOWN"));
    }
}

// SqsPushMessagePublisherTest#publish_clientFailure_retriesWithoutRawException
#[test]
fn 연결_오류와_본문_검증_실패는_수락_불명으로_재시도한다() {
    let failing = fake(|_| Err(AwsError::Transport("SECRET_REQUEST_BODY".into())));
    let result = publisher(failing).publish(&message()).unwrap();
    assert!(result.retryable);
    assert!(result.is_acceptance_unknown());
    assert_eq!(result.error_code.as_deref(), Some("SQS_CLIENT_ERROR"));
    assert!(!format!("{result:?}").contains("SECRET_REQUEST_BODY"));

    // 응답의 MD5가 보낸 본문과 다르면 SDK가 예외를 던진다.
    let mismatch = fake(|_| response(200, "{\"MessageId\":\"id\",\"MD5OfMessageBody\":\"00\"}"));
    let result = publisher(mismatch).publish(&message()).unwrap();
    assert!(result.is_acceptance_unknown());
    assert_eq!(result.error_code.as_deref(), Some("SQS_CLIENT_ERROR"));
}

// ---- 실제 HTTP 전송 ----

/// 한 번만 응답하는 로컬 HTTP 서버. 요청 본문을 모아 둔다.
struct OneShotServer {
    url: String,
    requests: Arc<AtomicUsize>,
}

fn serve(delay: Duration) -> OneShotServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let mut data = Vec::new();
                let mut buffer = [0u8; 4096];
                let body = loop {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&data).into_owned();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap_or(0);
                        if rest.len() >= length {
                            break rest[..length].to_owned();
                        }
                    }
                };
                counter.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(delay);
                let envelope: Value = serde_json::from_str(&body).unwrap();
                let message_body = envelope["MessageBody"].as_str().unwrap();
                let reply = format!(
                    "{{\"MessageId\":\"over-http\",\"MD5OfMessageBody\":\"{}\"}}",
                    md5_hex(message_body.as_bytes())
                );
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-amz-json-1.0\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            });
        }
    });
    OneShotServer { url, requests }
}

#[test]
fn http_전송으로_실제_서버에_보내고_수락_id를_받는다() {
    let server = serve(Duration::ZERO);
    let transport = Arc::new(HttpSqsTransport::new(Duration::from_secs(5)).unwrap());
    let result = publisher_with(&server.url, transport, None)
        .publish(&message())
        .unwrap();
    assert!(result.is_accepted());
    assert_eq!(result.message_id.as_deref(), Some("over-http"));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn 호출_제한_시간을_넘기면_기다리지_않고_수락_불명으로_본다() {
    let server = serve(Duration::from_millis(800));
    let transport = Arc::new(HttpSqsTransport::new(Duration::from_secs(5)).unwrap());
    let started = Instant::now();
    let result = publisher_with(&server.url, transport, Some(Duration::from_millis(150)))
        .publish(&message())
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(600));
    assert!(result.is_acceptance_unknown());
    assert_eq!(result.error_code.as_deref(), Some("SQS_CLIENT_ERROR"));
}

#[test]
fn 연결할_수_없으면_수락_불명이다() {
    let transport = Arc::new(HttpSqsTransport::new(Duration::from_secs(1)).unwrap());
    let result = publisher_with("http://127.0.0.1:1", transport, None)
        .publish(&message())
        .unwrap();
    assert!(result.is_acceptance_unknown());
}

// ---- 설정 ----

fn settings(pairs: &[(&str, &str)]) -> Result<SqsPushSettings, ConfigError> {
    SqsPushSettings::from_env(&Env::from_pairs(pairs.iter().copied()))
}

// SqsPushConfigurationTest#configuration_disabled_startsWithoutRelayScheduler
#[test]
fn 발행이_꺼져_있으면_큐_url_없이도_기동한다() {
    let s = settings(&[]).unwrap();
    assert!(!s.relay_enabled && !s.topic_push_enabled && !s.publishing_enabled());
    assert_eq!(s.queue_url, "");
    assert_eq!(s.region, "ap-northeast-2");
    assert!(
        settings(&[
            ("NOTIFICATION_RELAY_ENABLED", "false"),
            ("NOTIFICATION_SQS_QUEUE_URL", "")
        ])
        .is_ok()
    );
    // 꺼져 있으면 잘못된 URL도 검사하지 않는다.
    assert!(settings(&[("NOTIFICATION_SQS_QUEUE_URL", "REPLACE_WITH_QUEUE")]).is_ok());
}

// SqsPushConfigurationTest#configuration_enabledWithQueue_createsRelayScheduler 외
#[test]
fn 발행을_켜고_standard_큐_url을_주면_받아들인다() {
    let url = "https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-dev-push";
    let relay = settings(&[
        ("NOTIFICATION_RELAY_ENABLED", "true"),
        ("NOTIFICATION_SQS_QUEUE_URL", url),
    ])
    .unwrap();
    assert!(relay.relay_enabled && !relay.topic_push_enabled && relay.publishing_enabled());
    assert_eq!(relay.queue_url, url);
    let topic = settings(&[
        ("NOTIFICATION_TOPIC_PUSH_ENABLED", "true"),
        ("NOTIFICATION_RELAY_ENABLED", "false"),
        ("NOTIFICATION_SQS_QUEUE_URL", url),
        ("AWS_REGION", "us-east-1"),
    ])
    .unwrap();
    assert!(topic.topic_push_enabled && !topic.relay_enabled);
    assert_eq!(topic.region, "us-east-1");
}

// SDK 공통 엔드포인트 변수: SQS 전용이 먼저, 없으면 공통, 비어 있으면 없는 것으로 본다.
#[test]
fn 엔드포인트는_sqs_전용_변수_공통_변수_순으로_고른다() {
    assert_eq!(settings(&[]).unwrap().endpoint_url, None);
    let general = settings(&[("AWS_ENDPOINT_URL", " http://general:1 ")]).unwrap();
    assert_eq!(general.endpoint_url.as_deref(), Some("http://general:1"));
    let both = settings(&[
        ("AWS_ENDPOINT_URL", "http://general:1"),
        ("AWS_ENDPOINT_URL_SQS", "http://sqs:2"),
    ])
    .unwrap();
    assert_eq!(both.endpoint_url.as_deref(), Some("http://sqs:2"));
    let blank = settings(&[
        ("AWS_ENDPOINT_URL_SQS", " "),
        ("AWS_ENDPOINT_URL", "http://general:1"),
    ])
    .unwrap();
    assert_eq!(blank.endpoint_url.as_deref(), Some("http://general:1"));
}

#[test]
fn 발행을_켰을_때만_엔드포인트_형식을_검증한다() {
    let bad = [("AWS_ENDPOINT_URL_SQS", "not a url")];
    assert!(settings(&bad).is_ok());
    let url = "https://sqs.ap-northeast-2.amazonaws.com/000000000000/q";
    let enabled = [
        ("NOTIFICATION_RELAY_ENABLED", "true"),
        ("NOTIFICATION_SQS_QUEUE_URL", url),
        ("AWS_ENDPOINT_URL_SQS", "not a url"),
    ];
    assert!(settings(&enabled).is_err());
}

// SqsPushConfigurationTest#configuration_enabledWithInvalidQueue_rejectsConfiguration,
// configuration_topicEnabledWithInvalidQueue_rejectsConfiguration
#[test]
fn 발행을_켰는데_큐_url이_올바르지_않으면_기동을_거부한다() {
    for url in [
        "",
        "REPLACE_WITH_QUEUE",
        "http://sqs.ap-northeast-2.amazonaws.com/000000000000/push",
        "https://sqs.ap-northeast-2.amazonaws.com/000000000000/push.fifo",
    ] {
        for flag in [
            "NOTIFICATION_RELAY_ENABLED",
            "NOTIFICATION_TOPIC_PUSH_ENABLED",
        ] {
            let error =
                settings(&[(flag, "true"), ("NOTIFICATION_SQS_QUEUE_URL", url)]).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Invalid { key, .. } if key == "NOTIFICATION_SQS_QUEUE_URL"),
                "{flag} {url:?}"
            );
        }
    }
    assert!(settings(&[("NOTIFICATION_RELAY_ENABLED", "yes")]).is_err());
}
