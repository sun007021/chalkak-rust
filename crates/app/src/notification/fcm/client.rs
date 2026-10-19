//! SDK의 `FirebaseMessaging.send`: 액세스 토큰을 붙여 `messages:send`를 부르고, 503·401을 SDK처럼 다시 시도하고, 오류 응답을
//! [`MessagingFailure`]로 해석한다. 규칙은 모두 원본 JAR의 SDK에 같은 입력을 돌려 얻은 기록(`expected_fcm.tsv`)이다.
//!
//! - 재시도: 최대 4번(총 5번). `503`만 다시 보내며 `Retry-After`가 1~60초(숫자 또는 날짜)면 그만큼, 0 이하이거나 해석할 수
//!   없으면 0.5·1·2·4초(2배씩), 60초보다 길면 다시 보내지 않는다. `401`은 `WWW-Authenticate`가 없거나 Bearer가 아니거나
//!   `error="invalid_token"`일 때 토큰을 새로 받고(마지막 시도에서도 받는다) 바로 다시 보낸다. 연결·읽기 오류는 다시 보내지 않는다.
//! - 오류 분류: `401`은 본문을 보지 않고 `UNAUTHENTICATED`다. 그 밖의 오류는 본문 `error.status`(`ErrorCode` 이름), 없으면 HTTP
//!   상태 표로 `ErrorCode`를 정하고, `error.details`에서 처음 만난 `google.firebase.fcm.v1.FcmError`의 `errorCode`로
//!   [`MessagingErrorCode`]를 정한다. 모양이 틀린 본문(`error`가 객체가 아님, 알 수 없는 `status`, 문자열이 아닌 `errorCode`)은
//!   SDK가 예외를 던지므로 [`MessagingFailure::Thrown`]이다.
//! - 성공 응답(2xx)은 본문이 JSON 객체여야 한다. 비었거나 객체가 아닌 JSON은 `IllegalArgumentException`, JSON이 아니면 `UNKNOWN`이다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_core::Clock;
use serde_json::Value;

use super::auth::{TokenError, TokenProvider};
use super::message::FcmMessage;
use super::retry_after::{parse_http_date, parse_seconds};
use super::transport::{ExchangeError, ExchangeRequest, ExchangeResponse, HttpExchange};
use super::{
    ErrorCode, FirebaseMessaging, MessagingErrorCode, MessagingException, MessagingFailure,
};

/// 재시도 횟수(`RetryConfig` 기본 최대 4번)
const MAX_RETRIES: u32 = 4;
/// 503 재시도 간격의 시작(0.5초, 2배씩)
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
/// 이보다 긴 `Retry-After`는 기다리지 않고 포기한다.
const MAX_RETRY_AFTER_MILLIS: i64 = 60_000;
const FCM_ERROR_TYPE: &str = "type.googleapis.com/google.firebase.fcm.v1.FcmError";
const FIREBASE_CLIENT: &str = "fire-admin-java/9.11.0";
const API_CLIENT: &str = "gl-rust/chalkak fire-admin/9.11.0";

pub struct FirebaseMessagingClient {
    send_url: String,
    tokens: TokenProvider,
    transport: Arc<dyn HttpExchange>,
    clock: Arc<dyn Clock>,
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl FirebaseMessagingClient {
    /// `endpoint`는 `https://fcm.googleapis.com`(끝의 `/` 없음)이나 로컬 스택 주소다.
    pub fn new(
        endpoint: &str,
        project_id: &str,
        tokens: TokenProvider,
        transport: Arc<dyn HttpExchange>,
        clock: Arc<dyn Clock>,
        sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        Self {
            send_url: format!(
                "{}/v1/projects/{project_id}/messages:send",
                endpoint.trim_end_matches('/')
            ),
            tokens,
            transport,
            clock,
            sleep,
        }
    }

    fn request(&self, token: &str, body: &str) -> ExchangeRequest {
        ExchangeRequest {
            url: self.send_url.clone(),
            headers: vec![
                ("Authorization".to_owned(), format!("Bearer {token}")),
                (
                    "Content-Type".to_owned(),
                    "application/json; charset=UTF-8".to_owned(),
                ),
                ("X-Firebase-Client".to_owned(), FIREBASE_CLIENT.to_owned()),
                ("X-Goog-Api-Client".to_owned(), API_CLIENT.to_owned()),
                ("X-GOOG-API-FORMAT-VERSION".to_owned(), "2".to_owned()),
            ],
            body: body.as_bytes().to_vec(),
        }
    }

    /// 503 재시도 전 기다릴 시간. `None`이면 다시 보내지 않는다.
    fn retry_delay(&self, response: &ExchangeResponse, retries_done: u32) -> Option<Duration> {
        let backoff = FIRST_BACKOFF * 2u32.pow(retries_done);
        let Some(header) = response.header("Retry-After") else {
            return Some(backoff);
        };
        let delay_millis = match parse_seconds(header) {
            Some(seconds) => seconds.saturating_mul(1000),
            None => match parse_http_date(header) {
                Some(retry_at) => (i128::from(retry_at) * 1000
                    - self.clock.now().as_millisecond() as i128)
                    .clamp(0, i128::from(i64::MAX)) as i64,
                None => 0,
            },
        };
        if delay_millis > MAX_RETRY_AFTER_MILLIS {
            return None;
        }
        if delay_millis <= 0 {
            return Some(backoff);
        }
        Some(Duration::from_millis(delay_millis as u64))
    }
}

impl FirebaseMessaging for FirebaseMessagingClient {
    fn send(&self, message: &FcmMessage) -> Result<(), MessagingFailure> {
        let body = message.to_json();
        let mut retries_done = 0;
        loop {
            let token = match self.tokens.access_token() {
                Ok(token) => token,
                Err(error) => return Err(token_failure(&error)),
            };
            let response = match self.transport.execute(&self.request(&token, &body)) {
                Ok(response) => response,
                Err(error) => return Err(io_failure(&error)),
            };
            if (200..300).contains(&response.status) {
                return parse_success(&response);
            }
            let supports_retry = retries_done < MAX_RETRIES;
            if response.status == 401 && should_refresh_token(&response) {
                // 토큰 갱신에 실패하면 다시 보내지 않고 이 오류를 돌려준다.
                if self.tokens.refresh().is_ok() && supports_retry {
                    retries_done += 1;
                    continue;
                }
            } else if response.status == 503
                && supports_retry
                && let Some(delay) = self.retry_delay(&response, retries_done)
            {
                (self.sleep)(delay);
                retries_done += 1;
                continue;
            }
            return Err(parse_error(&response));
        }
    }
}

/// `HttpCredentialsAdapter.handleResponse`: `WWW-Authenticate`의 첫 Bearer 항목이 `invalid_token`일 때만 갱신한다. Bearer 항목이
/// 없으면 401이므로 갱신한다.
fn should_refresh_token(response: &ExchangeResponse) -> bool {
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("WWW-Authenticate") && has_bearer_prefix(value.trim_start()) {
            return mentions_invalid_token(value);
        }
    }
    true
}

fn has_bearer_prefix(text: &str) -> bool {
    text.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer"))
}

/// 정규식 `\s*error\s*=\s*"?invalid_token"?`가 어디서든 맞는가.
fn mentions_invalid_token(header: &str) -> bool {
    header.match_indices("error").any(|(at, word)| {
        let rest = header[at + word.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            return false;
        };
        let rest = rest.trim_start();
        rest.strip_prefix('"')
            .unwrap_or(rest)
            .starts_with("invalid_token")
    })
}

/// 토큰을 얻지 못했다: `IOException`은 `UNKNOWN`, 그 밖의 예외는 그대로 던져진다.
fn token_failure(error: &TokenError) -> MessagingFailure {
    match error {
        TokenError::Io => MessagingFailure::Exception(MessagingException {
            error_code: ErrorCode::Unknown,
            messaging_code: None,
            response_headers: None,
        }),
        TokenError::Thrown(name) => MessagingFailure::Thrown(name),
    }
}

/// 응답을 받지 못했다: 시간 초과는 `DEADLINE_EXCEEDED`, 이름 해석 실패는 `UNAVAILABLE`, 그 밖에는 `UNKNOWN`.
fn io_failure(error: &ExchangeError) -> MessagingFailure {
    let error_code = match error {
        ExchangeError::Timeout => ErrorCode::DeadlineExceeded,
        ExchangeError::UnknownHost => ErrorCode::Unavailable,
        ExchangeError::Other => ErrorCode::Unknown,
    };
    MessagingFailure::Exception(MessagingException {
        error_code,
        messaging_code: None,
        response_headers: None,
    })
}

/// 본문의 첫 JSON 값. 비었으면 `Ok(None)`, JSON이 아니면 `Err`. 값 뒤의 글자는 무시한다.
fn first_json(body: &[u8]) -> Result<Option<Value>, ()> {
    match serde_json::Deserializer::from_slice(body)
        .into_iter::<Value>()
        .next()
    {
        None => Ok(None),
        Some(Ok(value)) => Ok(Some(value)),
        Some(Err(_)) => Err(()),
    }
}

fn parse_success(response: &ExchangeResponse) -> Result<(), MessagingFailure> {
    match first_json(&response.body) {
        Ok(Some(Value::Object(_))) => Ok(()),
        Ok(_) => Err(MessagingFailure::Thrown("IllegalArgumentException")),
        Err(()) => Err(MessagingFailure::Exception(MessagingException {
            error_code: ErrorCode::Unknown,
            messaging_code: None,
            response_headers: Some(response.headers.clone()),
        })),
    }
}

fn parse_error(response: &ExchangeResponse) -> MessagingFailure {
    let headers = Some(response.headers.clone());
    if response.status == 401 {
        return MessagingFailure::Exception(MessagingException {
            error_code: ErrorCode::Unauthenticated,
            messaging_code: None,
            response_headers: headers,
        });
    }
    let (status, messaging_code) = match parse_error_body(&response.body) {
        Ok(parsed) => parsed,
        Err(name) => return MessagingFailure::Thrown(name),
    };
    MessagingFailure::Exception(MessagingException {
        error_code: status.unwrap_or_else(|| ErrorCode::from_http_status(response.status)),
        messaging_code,
        response_headers: headers,
    })
}

/// 오류 본문에서 `error.status`와 첫 `FcmError`의 코드를 읽는다. 본문이 JSON 객체가 아니거나 `error`가 없으면 둘 다 없다.
fn parse_error_body(
    body: &[u8],
) -> Result<(Option<ErrorCode>, Option<MessagingErrorCode>), &'static str> {
    let Ok(Some(Value::Object(root))) = first_json(body) else {
        return Ok((None, None));
    };
    let error = match root.get("error") {
        None | Some(Value::Null) => return Ok((None, None)),
        Some(Value::Object(error)) => error,
        Some(_) => return Err("IllegalArgumentException"),
    };
    let status = match error.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) if name.is_empty() => None,
        Some(Value::String(name)) => {
            Some(ErrorCode::from_name(name).ok_or("IllegalArgumentException")?)
        }
        Some(_) => return Err("IllegalArgumentException"),
    };
    let mut messaging_code = None;
    if let Some(Value::Array(details)) = error.get("details") {
        for detail in details {
            let Value::Object(detail) = detail else {
                continue;
            };
            if detail.get("@type").and_then(Value::as_str) != Some(FCM_ERROR_TYPE) {
                continue;
            }
            messaging_code = match detail.get("errorCode") {
                None | Some(Value::Null) => None,
                Some(Value::String(code)) => MessagingErrorCode::from_wire(code),
                Some(_) => return Err("ClassCastException"),
            };
            break;
        }
    }
    Ok((status, messaging_code))
}
