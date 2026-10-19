//! Google 서비스 계정 인증. 원본은 `ServiceAccountCredentials`(google-auth-library 1.52.0)를 쓴다.
//!
//! 서비스 계정 JSON -> RS256 JWT(`assertion`) -> `token_uri`에 `urn:ietf:params:oauth:grant-type:jwt-bearer`로 교환 ->
//! 액세스 토큰. SDK와 같은 점:
//! - JWT 헤더 `{"alg":"RS256","kid":<private_key_id>,"typ":"JWT"}`, 클레임 `aud`(항상 구글 토큰 주소)·`exp`·`iat`·`iss`·`scope`.
//!   `exp`는 `iat`보다 1시간 뒤고 `scope`는 SDK 기본 범위 6개를 공백으로 이은 문자열이다.
//! - 토큰 요청이 `408`·`429`·`500`·`503`이거나 연결에 실패하면 최대 3번 다시 보낸다(1·2·4초 간격). 그 밖의 상태는 바로 실패다.
//! - 토큰은 만료 3분 45초 전부터 새로 받는다.
//! - `expires_in`은 정수여야 한다. 실수면 `ArithmeticException`, 문자열이면 `IOException`이다.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chalkak_core::Clock;
use chalkak_jwt::Rs256Signer;
use jiff::Timestamp;
use serde_json::{Value, json};

use super::transport::{ExchangeRequest, HttpExchange};

/// 서비스 계정 JSON에 `token_uri`가 없을 때 쓰는 주소이자 JWT의 `aud`(SDK는 `aud`를 항상 이 값으로 둔다).
pub const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// SDK `FirebaseOptions` 기본 범위(`FirebaseCredentials`의 `FIREBASE_SCOPES`).
const SCOPES: [&str; 6] = [
    "https://www.googleapis.com/auth/firebase.database",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/identitytoolkit",
    "https://www.googleapis.com/auth/devstorage.full_control",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/datastore",
];
const ASSERTION_LIFETIME_SECONDS: i64 = 3600;
/// 만료까지 이 시간이 남지 않았으면 새로 받는다(`OAuth2Credentials`의 갱신 여유).
const REFRESH_MARGIN: Duration = Duration::from_secs(225);
const TOKEN_RETRIES: usize = 3;
const TOKEN_FIRST_DELAY: Duration = Duration::from_secs(1);
const RETRYABLE_TOKEN_STATUSES: [u16; 4] = [408, 429, 500, 503];
const TOKEN_API_CLIENT: &str = "gl-rust/chalkak auth/1.52.0 auth-request-type/at cred-type/sa";

/// 서비스 계정 키 파일 내용.
pub struct ServiceAccount {
    client_email: String,
    private_key_id: String,
    token_uri: String,
    signer: Rs256Signer,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

impl ServiceAccount {
    /// `ServiceAccountCredentials.fromStream`. 키가 올바르지 않으면 기동 때 알리도록 만들 때 모두 확인한다.
    /// 오류 설명에는 키 내용을 싣지 않는다.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|_| "서비스 계정 파일이 JSON이 아닙니다.".to_owned())?;
        let object = value
            .as_object()
            .ok_or_else(|| "서비스 계정 파일이 JSON 객체가 아닙니다.".to_owned())?;
        match object.get("type").and_then(Value::as_str) {
            Some("service_account") => {}
            _ => return Err("서비스 계정 파일의 type이 service_account가 아닙니다.".to_owned()),
        }
        let field = |name: &str| object.get(name).and_then(Value::as_str);
        let (Some(_client_id), Some(client_email), Some(private_key), Some(private_key_id)) = (
            field("client_id"),
            field("client_email"),
            field("private_key"),
            field("private_key_id"),
        ) else {
            return Err("서비스 계정 파일에 client_id, client_email, private_key, private_key_id가 모두 있어야 합니다.".to_owned());
        };
        let token_uri = field("token_uri").unwrap_or(GOOGLE_TOKEN_URI);
        if !(token_uri.starts_with("http://") || token_uri.starts_with("https://")) {
            return Err("서비스 계정 파일의 token_uri가 http(s) 주소가 아닙니다.".to_owned());
        }
        let signer = Rs256Signer::from_pkcs8_pem(private_key)
            .map_err(|_| "서비스 계정 파일의 private_key가 PKCS#8 RSA 키가 아닙니다.".to_owned())?;
        Ok(Self {
            client_email: client_email.to_owned(),
            private_key_id: private_key_id.to_owned(),
            token_uri: token_uri.to_owned(),
            signer,
        })
    }

    /// 토큰 교환 요청 본문(`createAssertion`).
    fn assertion(&self, now: Timestamp) -> String {
        let issued_at = now.as_second();
        let header = json!({"alg": "RS256", "kid": self.private_key_id, "typ": "JWT"});
        let claims = json!({
            "aud": GOOGLE_TOKEN_URI,
            "exp": issued_at + ASSERTION_LIFETIME_SECONDS,
            "iat": issued_at,
            "iss": self.client_email,
            "scope": SCOPES.join(" "),
        });
        self.signer.sign(&header, &claims)
    }
}

/// 토큰을 받지 못한 이유. `Thrown`은 SDK에서 `IOException`이 아닌 예외(그대로 던져진다)다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// `IOException`: 호출 쪽에서는 `UNKNOWN` 오류가 된다.
    Io,
    /// 그 밖의 예외 이름(`ArithmeticException`)
    Thrown(&'static str),
}

struct CachedToken {
    value: String,
    expires_at: Timestamp,
}

/// 액세스 토큰 발급·캐시(`OAuth2Credentials`).
pub struct TokenProvider {
    account: ServiceAccount,
    transport: Arc<dyn HttpExchange>,
    clock: Arc<dyn Clock>,
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    cached: Mutex<Option<CachedToken>>,
}

impl TokenProvider {
    pub fn new(
        account: ServiceAccount,
        transport: Arc<dyn HttpExchange>,
        clock: Arc<dyn Clock>,
        sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        Self {
            account,
            transport,
            clock,
            sleep,
            cached: Mutex::new(None),
        }
    }

    /// 쓸 수 있는 토큰. 없거나 곧 만료되면 새로 받는다(여러 스레드가 동시에 와도 한 번만 받는다).
    pub fn access_token(&self) -> Result<String, TokenError> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.clock.now();
        if let Some(token) = cached.as_ref()
            && token.expires_at.as_millisecond() - now.as_millisecond()
                > REFRESH_MARGIN.as_millis() as i64
        {
            return Ok(token.value.clone());
        }
        let token = self.fetch(now)?;
        let value = token.value.clone();
        *cached = Some(token);
        Ok(value)
    }

    /// 캐시를 무시하고 새로 받는다(서버가 401로 토큰을 거절했을 때).
    pub fn refresh(&self) -> Result<String, TokenError> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let token = self.fetch(self.clock.now())?;
        let value = token.value.clone();
        *cached = Some(token);
        Ok(value)
    }

    fn fetch(&self, now: Timestamp) -> Result<CachedToken, TokenError> {
        let assertion = self.account.assertion(now);
        let body = format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"
        );
        let request = ExchangeRequest {
            url: self.account.token_uri.clone(),
            headers: vec![
                (
                    "Content-Type".to_owned(),
                    "application/x-www-form-urlencoded; charset=UTF-8".to_owned(),
                ),
                ("x-goog-api-client".to_owned(), TOKEN_API_CLIENT.to_owned()),
            ],
            body: body.into_bytes(),
        };
        let mut delay = TOKEN_FIRST_DELAY;
        for attempt in 0..=TOKEN_RETRIES {
            let last = attempt == TOKEN_RETRIES;
            match self.transport.execute(&request) {
                Ok(response) if (200..300).contains(&response.status) => {
                    return parse_token(&response.body, now);
                }
                Ok(response) => {
                    if last || !RETRYABLE_TOKEN_STATUSES.contains(&response.status) {
                        return Err(TokenError::Io);
                    }
                }
                Err(_) => {
                    if last {
                        return Err(TokenError::Io);
                    }
                }
            }
            (self.sleep)(delay);
            delay *= 2;
        }
        Err(TokenError::Io)
    }
}

/// 토큰 응답: `access_token`(문자열)과 `expires_in`(정수)이 있어야 한다.
fn parse_token(body: &[u8], now: Timestamp) -> Result<CachedToken, TokenError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| TokenError::Io)?;
    let object = value.as_object().ok_or(TokenError::Io)?;
    let token = object
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(TokenError::Io)?;
    let expires_in = match object.get("expires_in") {
        Some(Value::Number(n)) => expires_in_seconds(n)?,
        _ => return Err(TokenError::Io),
    };
    let expires_at = now
        .checked_add(jiff::SignedDuration::from_secs(i64::from(expires_in)))
        .map_err(|_| TokenError::Io)?;
    Ok(CachedToken {
        value: token.to_owned(),
        expires_at,
    })
}

/// `BigDecimal.intValueExact`: 값이 32비트 정수로 딱 떨어져야 한다(3600.0은 되고 3600.5는 안 된다).
fn expires_in_seconds(number: &serde_json::Number) -> Result<i32, TokenError> {
    let thrown = TokenError::Thrown("ArithmeticException");
    if let Some(integer) = number.as_i64() {
        return i32::try_from(integer).map_err(|_| thrown);
    }
    match number.as_f64() {
        Some(float)
            if float.fract() == 0.0
                && float >= f64::from(i32::MIN)
                && float <= f64::from(i32::MAX) =>
        {
            Ok(float as i32)
        }
        _ => Err(thrown),
    }
}
