//! SQS JSON 프로토콜(`AmazonSQS.*`, `application/x-amz-json-1.0`)과 SigV4 서명. AWS SDK 없이 직접 구현한다.
//!
//! 원본 백엔드가 쓰는 AWS SDK for Java v2(2.54.1) `SqsClient`와 같은 요청·같은 응답 해석을 한다
//! (`tests/sqs_java_parity.rs`, 기대값은 `tests/fixtures/SqsFixture.java`를 원본 JAR의 SDK에 돌려 만든 `expected_sqs.tsv`).
//! 눈여겨볼 점:
//! - 요청은 항상 `POST <엔드포인트>/`이고 큐 URL은 본문(`QueueUrl`)에 싣는다. 헤더는 `X-Amz-Target`, `Content-Type`,
//!   `x-amzn-query-mode: true`(SQS가 `x-amzn-query-error` 헤더로 쿼리 호환 오류 코드를 주게 한다), `x-amz-content-sha256`이다.
//!   서명 헤더는 `content-length;content-type;host;x-amz-content-sha256;x-amz-date;x-amz-target;x-amzn-query-mode`
//!   (+ 세션 토큰). SDK는 여기에 `amz-sdk-invocation-id`·`amz-sdk-request`를 더 서명하는데 서버가 요구하지 않아 보내지 않는다.
//! - 엔드포인트는 `AWS_ENDPOINT_URL_SQS` → `AWS_ENDPOINT_URL` → `https://sqs.<리전>.amazonaws.com`.
//!   큐 URL의 호스트는 엔드포인트와 무관하다(로컬 스택은 amazonaws 형식 큐 URL을 그대로 받는다).
//! - 오류 코드는 `x-amzn-query-error` 헤더의 `코드;분류`(첫 `;` 앞이 비어 있지 않을 때), 없으면 본문 `__type`(첫 `:` 앞에서
//!   자르고, 그 안의 첫 `#` 뒤), 그것도 없으면 본문 `code`다. 원문 오류 메시지는 어디에도 싣지 않는다.
//! - 2xx 응답 해석에 실패하거나(JSON이 아님) SendMessage의 `MD5OfMessageBody`·ReceiveMessage의 `MD5OfBody`가 없거나 다르면
//!   SDK처럼 클라이언트 오류다. 응답을 믿을 수 없으므로 호출 측이 "수락 불명"으로 다뤄야 한다.
//! - 재시도하지 않는다. 호출 제한 시간은 [`SqsClient::with_timeout`]으로 정하고 [`SqsTransport`]가 지킨다.

use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::Value;

use crate::credentials::{Credentials, CredentialsProvider, HttpRequest, NowFn};
use crate::error::AwsError;
use crate::sigv4::{self, Request, Scope, UriMode};

const SERVICE: &str = "sqs";
const CONTENT_TYPE: &str = "application/x-amz-json-1.0";
const QUERY_ERROR_HEADER: &str = "x-amzn-query-error";
/// 원본 `SqsPushConfiguration`의 `apiCallTimeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// SDK `isThrottlingException`이 조절 오류로 보는 코드. 상태 코드 429도 조절이다.
const THROTTLING_CODES: [&str; 12] = [
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "RequestThrottled",
    "SlowDown",
    "EC2ThrottledException",
];

// ---- 큐 URL ----

/// 원본 `SqsPushProperties.validateQueueUrl`(`SqsPushProperties.java:19-23`)과 같은 검사:
/// `https://sqs.<리전>.amazonaws.com/<12자리 숫자>/<큐 이름>`, 리전은 `[a-z0-9-]+`, 큐 이름은 `[A-Za-z0-9_-]+`
/// (그래서 `.fifo` 큐는 거절된다). Java의 `String.matches`처럼 전체가 맞아야 한다.
pub fn is_standard_queue_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://sqs.") else {
        return false;
    };
    let Some((region, rest)) = rest.split_once(".amazonaws.com/") else {
        return false;
    };
    let region_ok = !region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let Some((account, name)) = rest.split_once('/') else {
        return false;
    };
    let account_ok = account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit());
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    region_ok && account_ok && name_ok
}

// ---- 엔드포인트 ----

/// SQS 요청을 보낼 곳. `http(s)://호스트[:포트]`(경로 없음).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqsEndpoint {
    scheme: String,
    authority: String,
}

impl SqsEndpoint {
    /// 리전별 기본 엔드포인트(`https://sqs.<리전>.amazonaws.com`, 중국 리전은 `.com.cn`).
    pub fn aws(region: &str) -> Result<Self, AwsError> {
        if region.is_empty()
            || !region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(AwsError::InvalidInput(format!("invalid region: {region}")));
        }
        let suffix = if region.starts_with("cn-") {
            "amazonaws.com.cn"
        } else {
            "amazonaws.com"
        };
        Ok(Self {
            scheme: "https".to_owned(),
            authority: format!("sqs.{region}.{suffix}"),
        })
    }

    /// 사용자 지정 엔드포인트(로컬 스택, 하네스 가짜 서버).
    pub fn custom(url: &str) -> Result<Self, AwsError> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            AwsError::InvalidInput(format!("endpoint must be http(s)://host[:port]: {url}"))
        })?;
        if scheme != "http" && scheme != "https" {
            return Err(AwsError::InvalidInput(format!(
                "unsupported endpoint scheme: {scheme}"
            )));
        }
        let authority = rest.trim_end_matches('/');
        if authority.is_empty()
            || authority.contains(['/', '?', '#', '@', ' '])
            || authority.chars().any(char::is_control)
        {
            return Err(AwsError::InvalidInput(format!(
                "endpoint must be http(s)://host[:port]: {url}"
            )));
        }
        Ok(Self {
            scheme: scheme.to_owned(),
            authority: authority.to_owned(),
        })
    }

    /// `AWS_ENDPOINT_URL_SQS`, 없으면 `AWS_ENDPOINT_URL`(비어 있으면 없는 것으로 본다), 둘 다 없으면 기본 엔드포인트.
    pub fn from_env(
        lookup: &dyn Fn(&str) -> Option<String>,
        region: &str,
    ) -> Result<Self, AwsError> {
        let value = ["AWS_ENDPOINT_URL_SQS", "AWS_ENDPOINT_URL"]
            .iter()
            .find_map(|name| lookup(name).filter(|v| !v.trim().is_empty()));
        match value {
            Some(url) => Self::custom(url.trim()),
            None => Self::aws(region),
        }
    }

    pub fn from_process_env(region: &str) -> Result<Self, AwsError> {
        Self::from_env(&|name| std::env::var(name).ok(), region)
    }

    /// 요청 URL(`<스킴>://<호스트>/`).
    pub fn url(&self) -> String {
        format!("{}://{}/", self.scheme, self.authority)
    }

    /// `Host` 헤더 값. 기본 포트(`http` 80, `https` 443)는 생략한다(HTTP 클라이언트가 보내는 값과 같아야 서명이 맞는다).
    pub fn host(&self) -> String {
        let default = if self.scheme == "https" {
            ":443"
        } else {
            ":80"
        };
        self.authority
            .strip_suffix(default)
            .unwrap_or(&self.authority)
            .to_owned()
    }
}

// ---- 전송 ----

/// 헤더를 포함한 HTTP 응답. 오류 코드를 `x-amzn-query-error` 헤더에서 읽으므로 [`crate::HttpResponse`]를 쓰지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqsHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl SqsHttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 동기 HTTP 호출. `timeout`은 요청 하나의 전체 제한(연결부터 응답 끝까지)이며 넘기면 `Err`를 낸다. 리다이렉트를 따르지 않고
/// 4xx·5xx도 `Ok`로 돌려준다.
pub trait SqsTransport: Send + Sync {
    fn send(&self, request: &HttpRequest, timeout: Duration) -> Result<SqsHttpResponse, AwsError>;
}

// ---- 오류 ----

/// SQS가 오류 응답을 돌려줬다(SDK `SqsException`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqsServiceError {
    pub status: u16,
    /// 오류 코드 원문. 외부 값이므로 로그에 남기기 전에 형식을 확인해야 한다.
    pub code: Option<String>,
}

impl SqsServiceError {
    /// SDK `isThrottlingException`: 조절 코드이거나 상태 코드가 429.
    pub fn is_throttling(&self) -> bool {
        self.status == 429
            || self
                .code
                .as_deref()
                .is_some_and(|c| THROTTLING_CODES.contains(&c))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqsError {
    /// SQS가 응답했고 2xx가 아니다.
    Service(SqsServiceError),
    /// 응답을 확인하지 못했다(연결·시간 초과·자격증명·JSON 해석 실패·MD5 불일치). SDK `SdkClientException`.
    /// 메시지에는 요청 본문이나 자격증명을 싣지 않는다.
    Client(String),
}

impl fmt::Display for SqsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(e) => write!(
                f,
                "SQS returned {} ({})",
                e.status,
                e.code.as_deref().unwrap_or("no error code")
            ),
            Self::Client(m) => write!(f, "SQS client error: {m}"),
        }
    }
}

impl std::error::Error for SqsError {}

// ---- 입력·출력 ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendMessageOutput {
    /// SQS가 부여한 메시지 ID. 응답에 없으면 `None`.
    pub message_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiveMessageInput {
    pub max_number_of_messages: Option<u32>,
    pub visibility_timeout: Option<u32>,
    pub wait_time_seconds: Option<u32>,
    /// `MessageSystemAttributeNames`(예: `ApproximateReceiveCount`, `All`).
    pub message_system_attribute_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedMessage {
    pub message_id: Option<String>,
    pub receipt_handle: Option<String>,
    pub body: String,
    /// `Attributes`(시스템 속성). 값은 문자열 그대로.
    pub attributes: Vec<(String, String)>,
}

impl ReceivedMessage {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

// ---- 클라이언트 ----

#[derive(Clone)]
pub struct SqsClient {
    endpoint: SqsEndpoint,
    region: String,
    credentials: Arc<dyn CredentialsProvider>,
    transport: Arc<dyn SqsTransport>,
    now: NowFn,
    timeout: Duration,
}

impl SqsClient {
    pub fn new(
        region: &str,
        endpoint: SqsEndpoint,
        credentials: Arc<dyn CredentialsProvider>,
        transport: Arc<dyn SqsTransport>,
        now: NowFn,
    ) -> Self {
        Self {
            endpoint,
            region: region.to_owned(),
            credentials,
            transport,
            now,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 호출 제한 시간만 바꾼 복사본(원본의 요청별 `overrideConfiguration`).
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            timeout,
            ..self.clone()
        }
    }

    /// `AmazonSQS.SendMessage`. 본문의 MD5가 응답과 다르면 [`SqsError::Client`].
    pub fn send_message(
        &self,
        queue_url: &str,
        message_body: &str,
    ) -> Result<SendMessageOutput, SqsError> {
        let mut body = String::from("{\"QueueUrl\":");
        write_json_string(&mut body, queue_url);
        body.push_str(",\"MessageBody\":");
        write_json_string(&mut body, message_body);
        body.push('}');
        let json = self.call("SendMessage", &body)?;
        // SDK: 응답의 MD5OfMessageBody가 없거나 다르면 SdkClientException.
        let expected = md5_hex(message_body.as_bytes());
        if json.get("MD5OfMessageBody").and_then(Value::as_str) != Some(expected.as_str()) {
            return Err(md5_mismatch());
        }
        Ok(SendMessageOutput {
            message_id: json
                .get("MessageId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// `AmazonSQS.ReceiveMessage`. 메시지마다 `MD5OfBody`를 확인한다.
    pub fn receive_message(
        &self,
        queue_url: &str,
        input: &ReceiveMessageInput,
    ) -> Result<Vec<ReceivedMessage>, SqsError> {
        let mut body = String::from("{\"QueueUrl\":");
        write_json_string(&mut body, queue_url);
        if !input.message_system_attribute_names.is_empty() {
            body.push_str(",\"MessageSystemAttributeNames\":[");
            for (i, name) in input.message_system_attribute_names.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                write_json_string(&mut body, name);
            }
            body.push(']');
        }
        for (key, value) in [
            ("MaxNumberOfMessages", input.max_number_of_messages),
            ("VisibilityTimeout", input.visibility_timeout),
            ("WaitTimeSeconds", input.wait_time_seconds),
        ] {
            if let Some(value) = value {
                let _ = write!(body, ",\"{key}\":{value}");
            }
        }
        body.push('}');
        let json = self.call("ReceiveMessage", &body)?;
        let Some(messages) = json.get("Messages").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        messages
            .iter()
            .map(|m| {
                let text = |field: &str| m.get(field).and_then(Value::as_str);
                let message_body = text("Body").ok_or_else(md5_mismatch)?;
                if text("MD5OfBody") != Some(md5_hex(message_body.as_bytes()).as_str()) {
                    return Err(md5_mismatch());
                }
                let attributes = m
                    .get("Attributes")
                    .and_then(Value::as_object)
                    .map(|map| {
                        map.iter()
                            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(ReceivedMessage {
                    message_id: text("MessageId").map(str::to_owned),
                    receipt_handle: text("ReceiptHandle").map(str::to_owned),
                    body: message_body.to_owned(),
                    attributes,
                })
            })
            .collect()
    }

    /// `AmazonSQS.DeleteMessage`
    pub fn delete_message(&self, queue_url: &str, receipt_handle: &str) -> Result<(), SqsError> {
        let mut body = String::from("{\"QueueUrl\":");
        write_json_string(&mut body, queue_url);
        body.push_str(",\"ReceiptHandle\":");
        write_json_string(&mut body, receipt_handle);
        body.push('}');
        self.call("DeleteMessage", &body).map(|_| ())
    }

    /// `AmazonSQS.ChangeMessageVisibility`
    pub fn change_message_visibility(
        &self,
        queue_url: &str,
        receipt_handle: &str,
        visibility_timeout_seconds: u32,
    ) -> Result<(), SqsError> {
        let mut body = String::from("{\"QueueUrl\":");
        write_json_string(&mut body, queue_url);
        body.push_str(",\"ReceiptHandle\":");
        write_json_string(&mut body, receipt_handle);
        let _ = write!(
            body,
            ",\"VisibilityTimeout\":{visibility_timeout_seconds}}}"
        );
        self.call("ChangeMessageVisibility", &body).map(|_| ())
    }

    /// 서명한 요청을 보내고 2xx 본문을 JSON으로 읽는다(본문이 비어 있으면 `{}`).
    fn call(&self, operation: &str, body: &str) -> Result<Value, SqsError> {
        let credentials = self
            .credentials
            .credentials()
            .map_err(|e| SqsError::Client(e.to_string()))?;
        let request = sign_request(
            &credentials,
            &self.region,
            &self.endpoint,
            operation,
            body,
            (self.now)(),
            &[],
        );
        let response = self
            .transport
            .send(&request, self.timeout)
            .map_err(|e| SqsError::Client(e.to_string()))?;
        if !(200..300).contains(&response.status) {
            return Err(SqsError::Service(SqsServiceError {
                status: response.status,
                code: error_code(&response),
            }));
        }
        if response.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_slice(&response.body)
            .map_err(|_| SqsError::Client("unable to unmarshall response".to_owned()))
    }
}

fn md5_mismatch() -> SqsError {
    SqsError::Client(
        "MD5 returned by SQS does not match the calculation on the original request".to_owned(),
    )
}

// ---- 서명 ----

/// JSON 1.0 요청 하나를 서명해 보낼 수 있는 형태로 만든다. `extra_headers`는 호출 측이 더 서명할 헤더
/// (바이트 비교 시험이 SDK의 `amz-sdk-invocation-id` 등을 그대로 넣어 보는 데 쓴다).
pub fn sign_request(
    credentials: &Credentials,
    region: &str,
    endpoint: &SqsEndpoint,
    operation: &str,
    body: &str,
    now: Timestamp,
    extra_headers: &[(&str, &str)],
) -> HttpRequest {
    let amz_date = sigv4::amz_date(now);
    let payload_hash = sigv4::sha256_hex(body.as_bytes());
    let content_length = body.len().to_string();
    let target = format!("AmazonSQS.{operation}");
    let host = endpoint.host();

    let mut headers: Vec<(&str, &str)> = vec![
        ("content-length", &content_length),
        ("content-type", CONTENT_TYPE),
        ("host", &host),
        ("x-amz-content-sha256", &payload_hash),
        ("x-amz-date", &amz_date),
        ("x-amz-target", &target),
        ("x-amzn-query-mode", "true"),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token", token));
    }
    headers.extend_from_slice(extra_headers);

    let scope = Scope {
        region,
        service: SERVICE,
    };
    let canonical = sigv4::canonical_request(&Request {
        method: "POST",
        path: "/",
        query: &[],
        headers: &headers,
        payload_hash: &payload_hash,
        uri_mode: UriMode::DoubleEncodeNormalize,
    });
    let string_to_sign = sigv4::string_to_sign(&scope, now, &canonical.text);
    let key = sigv4::signing_key(
        &credentials.secret_access_key,
        &sigv4::date_stamp(now),
        region,
        SERVICE,
    );
    let signature = sigv4::sign(&key, &string_to_sign);
    let authorization = sigv4::authorization_header(
        &credentials.access_key_id,
        &scope,
        now,
        &canonical.signed_headers,
        &signature,
    );

    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    out.push(("authorization".to_owned(), authorization));
    out.push(("accept".to_owned(), "application/json".to_owned()));
    HttpRequest {
        method: "POST".to_owned(),
        url: endpoint.url(),
        headers: out,
        body: body.as_bytes().to_vec(),
    }
}

// ---- 오류 응답 해석 ----

/// SDK의 오류 코드 규칙(`expected_sqs.tsv`로 확인): `x-amzn-query-error` 헤더의 첫 `;` 앞(비어 있지 않을 때) → 본문
/// `__type`(첫 `:` 앞, 그 안의 첫 `#` 뒤) → 본문 `code`.
fn error_code(response: &SqsHttpResponse) -> Option<String> {
    if let Some(header) = response.header(QUERY_ERROR_HEADER)
        && let Some(index) = header.find(';')
        && index > 0
    {
        return Some(header[..index].to_owned());
    }
    let json: Value = serde_json::from_slice(&response.body).ok()?;
    let scalar = |value: &Value| match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    if let Some(raw) = json.get("__type").and_then(scalar) {
        let before_colon = raw.split(':').next().unwrap_or("");
        let code = match before_colon.split_once('#') {
            Some((_, after)) => after,
            None => before_colon,
        };
        return Some(code.to_owned());
    }
    json.get("code").and_then(scalar)
}

// ---- 도구 ----

fn write_json_string(out: &mut String, text: &str) {
    // serde_json의 문자열 이스케이프를 그대로 쓴다(`"`, `\`, 제어 문자만 이스케이프하고 나머지는 UTF-8 그대로).
    out.push_str(&Value::String(text.to_owned()).to_string());
}

/// MD5(RFC 1321) 소문자 hex. SQS가 본문 무결성 확인에 MD5를 쓰므로 필요하다(보안 용도가 아니다).
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // K[i] = floor(2^32 * abs(sin(i + 1)))
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());

    for chunk in message.chunks_exact(64) {
        let words: Vec<u32> = chunk
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(k[i])
                .wrapping_add(words[g])
                .rotate_left(S[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
    let mut out = String::with_capacity(32);
    for word in state {
        for byte in word.to_le_bytes() {
            let _ = write!(out, "{byte:02x}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5는_rfc1321_시험_값과_같다() {
        for (input, expected) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(md5_hex(input.as_bytes()), expected, "{input}");
        }
        // 패딩 경계(55, 56, 63, 64, 65바이트)
        assert_eq!(md5_hex(&[b'a'; 64]), "014842d480b571495a4a0363793f7367");
    }

    #[test]
    fn 큐_url_검증은_원본_정규식과_같다() {
        let ok = [
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-dev-push",
            "https://sqs.us-east-1.amazonaws.com/123456789012/a_B-9",
        ];
        for url in ok {
            assert!(is_standard_queue_url(url), "{url}");
        }
        let bad = [
            "",
            "REPLACE_WITH_QUEUE",
            "http://sqs.ap-northeast-2.amazonaws.com/000000000000/push",
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/push.fifo",
            "https://sqs.ap-northeast-2.amazonaws.com/00000000000/push",
            "https://sqs.ap-northeast-2.amazonaws.com/0000000000000/push",
            "https://sqs.ap-northeast-2.amazonaws.com/00000000000a/push",
            "https://sqs.AP-northeast-2.amazonaws.com/000000000000/push",
            "https://sqs..amazonaws.com/000000000000/push",
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/",
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/push/",
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/push\n",
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/pu sh",
            "https://sqs.ap.northeast.amazonaws.com/000000000000/push",
            "https://sqs.ap-northeast-2.amazonaws.com.evil.com/000000000000/push",
            "https://sqs.ap-northeast-2.amazonaws.com/000000000000/푸시",
            " https://sqs.ap-northeast-2.amazonaws.com/000000000000/push",
        ];
        for url in bad {
            assert!(!is_standard_queue_url(url), "{url:?}");
        }
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn 엔드포인트는_sqs_전용_변수_공통_변수_기본값_순이다() {
        let region = "ap-northeast-2";
        let default = SqsEndpoint::from_env(&lookup(&[]), region).unwrap();
        assert_eq!(default.url(), "https://sqs.ap-northeast-2.amazonaws.com/");
        assert_eq!(default.host(), "sqs.ap-northeast-2.amazonaws.com");

        let common = SqsEndpoint::from_env(
            &lookup(&[("AWS_ENDPOINT_URL", "http://common:4566")]),
            region,
        )
        .unwrap();
        assert_eq!(common.url(), "http://common:4566/");

        let both = SqsEndpoint::from_env(
            &lookup(&[
                ("AWS_ENDPOINT_URL", "http://common:4566"),
                ("AWS_ENDPOINT_URL_SQS", "http://sqs-only:9324/"),
            ]),
            region,
        )
        .unwrap();
        assert_eq!(both.url(), "http://sqs-only:9324/");
        assert_eq!(both.host(), "sqs-only:9324");

        // 빈 값은 없는 것으로 본다.
        let blank = SqsEndpoint::from_env(
            &lookup(&[
                ("AWS_ENDPOINT_URL_SQS", "  "),
                ("AWS_ENDPOINT_URL", "http://c:1"),
            ]),
            region,
        )
        .unwrap();
        assert_eq!(blank.url(), "http://c:1/");

        assert_eq!(
            SqsEndpoint::aws("cn-north-1").unwrap().host(),
            "sqs.cn-north-1.amazonaws.com.cn"
        );
        assert!(SqsEndpoint::aws("AP").is_err());
        assert!(SqsEndpoint::aws("").is_err());
    }

    #[test]
    fn 기본_포트는_host_헤더에서_생략한다() {
        assert_eq!(SqsEndpoint::custom("http://h:80").unwrap().host(), "h");
        assert_eq!(SqsEndpoint::custom("https://h:443").unwrap().host(), "h");
        assert_eq!(SqsEndpoint::custom("http://h:443").unwrap().host(), "h:443");
        assert_eq!(SqsEndpoint::custom("https://h:80").unwrap().host(), "h:80");
    }

    #[test]
    fn 잘못된_엔드포인트는_거절한다() {
        for url in [
            "",
            "localhost:4566",
            "ftp://h",
            "http://",
            "http://h/path",
            "http://u@h",
            "http://h?x=1",
            "http://h x",
        ] {
            assert!(SqsEndpoint::custom(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn 조절_판정은_코드_목록_또는_429다() {
        let err = |status, code: Option<&str>| SqsServiceError {
            status,
            code: code.map(str::to_owned),
        };
        assert!(err(400, Some("RequestThrottled")).is_throttling());
        assert!(err(400, Some("ThrottledException")).is_throttling());
        assert!(err(429, Some("SomethingElse")).is_throttling());
        assert!(err(429, None).is_throttling());
        assert!(!err(400, Some("LimitExceededException")).is_throttling());
        assert!(!err(400, Some("PriorRequestNotComplete")).is_throttling());
        assert!(!err(403, Some("AccessDenied")).is_throttling());
        assert!(!err(400, None).is_throttling());
    }
}
