//! Apple 토큰 서버 호출(`AppleHttpTokenClient`): authorizationCode 교환과 리프레시 토큰 폐기.
//!
//! 매핑은 원본 Spring `RestClient`의 동작을 따른다.
//! - 교환 2xx는 JSON 응답이 유효할 때만 성공이다(그렇지 않으면 500).
//! - 교환 400 + `{"error":"invalid_grant"}`는 401, 그 밖의 400과 다른 4xx·5xx·통신 실패는 모두 500.
//! - 폐기는 4xx·5xx·통신 실패가 500이고 나머지는 성공이다(`toBodilessEntity`).

use std::sync::Arc;
use std::time::Duration;

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_http::client::{ClientResponse, HttpClient};
use serde_json::Value;

use super::tokens::AppleClientSecret;
use crate::web::ApiError;
use crate::web::binding::is_java_whitespace;

/// `AppleTokenClientConfig`: 연결 2초, 읽기 5초, 리다이렉트 안 따라감.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppleTokenExchangeResult {
    pub id_token: String,
    pub refresh_token: String,
}

/// `AppleTokenClient`.
pub trait AppleTokenClient: Send + Sync {
    fn exchange_authorization_code(
        &self,
        authorization_code: &str,
    ) -> Result<AppleTokenExchangeResult, ApiError>;

    fn revoke_refresh_token(&self, refresh_token: &str) -> Result<(), ApiError>;
}

/// `AppleClientSecretGenerator`. 요청마다 새 client secret을 만든다.
pub trait ClientSecretSource: Send + Sync {
    fn generate(&self) -> String;
}

impl ClientSecretSource for AppleClientSecret {
    fn generate(&self) -> String {
        AppleClientSecret::generate(self)
    }
}

pub struct AppleHttpTokenClient {
    client: HttpClient,
    token_uri: String,
    revoke_uri: String,
    client_id: String,
    client_secret: Arc<dyn ClientSecretSource>,
}

fn internal(message: &str) -> ApiError {
    ApiError::Internal(message.to_owned())
}

fn is_blank(text: &str) -> bool {
    text.chars().all(is_java_whitespace)
}

/// Jackson 스칼라 강제 변환: 문자열 필드는 숫자·불리언도 문자열로 읽는다.
fn text_field(body: &Value, name: &str) -> Option<String> {
    match body.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `Long` 필드: 정수, 정수로 바꿀 수 있는 실수, 숫자 문자열을 받는다.
fn long_field(body: &Value, name: &str) -> Option<i64> {
    match body.get(name)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Jackson 컨버터가 읽는 미디어 타입(`application/json`, `application/*+json`).
fn is_json(response: &ClientResponse) -> bool {
    let Some(raw) = response.header("content-type") else {
        return false;
    };
    let essence = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

fn parse_body(response: &ClientResponse) -> Option<Value> {
    if !is_json(response) {
        return None;
    }
    serde_json::from_slice(&response.body).ok()
}

/// 토큰 응답 검증(`AppleTokenResponse#isValid`).
pub fn parse_token_response(body: &Value) -> Option<AppleTokenExchangeResult> {
    let access_token = text_field(body, "access_token")?;
    let token_type = text_field(body, "token_type")?;
    let expires_in = long_field(body, "expires_in")?;
    let refresh_token = text_field(body, "refresh_token")?;
    let id_token = text_field(body, "id_token")?;
    let valid = !is_blank(&access_token)
        && token_type.eq_ignore_ascii_case("bearer")
        && expires_in > 0
        && !is_blank(&refresh_token)
        && !is_blank(&id_token);
    valid.then_some(AppleTokenExchangeResult {
        id_token,
        refresh_token,
    })
}

impl AppleHttpTokenClient {
    pub fn new(
        token_uri: &str,
        revoke_uri: &str,
        client_id: &str,
        client_secret: Arc<dyn ClientSecretSource>,
    ) -> Result<AppleHttpTokenClient, String> {
        let client = HttpClient::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(AppleHttpTokenClient {
            client,
            token_uri: token_uri.to_owned(),
            revoke_uri: revoke_uri.to_owned(),
            client_id: client_id.to_owned(),
            client_secret,
        })
    }
}

impl AppleTokenClient for AppleHttpTokenClient {
    fn exchange_authorization_code(
        &self,
        authorization_code: &str,
    ) -> Result<AppleTokenExchangeResult, ApiError> {
        if is_blank(authorization_code) {
            return Err(ApiError::App(AppError::unauthorized(
                ErrorCode::UNAUTHORIZED,
                "Apple authorizationCode가 필요합니다.",
            )));
        }
        let secret = self.client_secret.generate();
        let response = self
            .client
            .post_form(
                &self.token_uri,
                &[],
                &[
                    ("client_id", &self.client_id),
                    ("client_secret", &secret),
                    ("code", authorization_code),
                    ("grant_type", "authorization_code"),
                ],
            )
            .map_err(|_| internal("Apple 토큰 서버와 통신할 수 없습니다."))?;

        if response.status == 400 {
            let invalid_grant = parse_body(&response)
                .and_then(|b| text_field(&b, "error"))
                .is_some_and(|e| e == "invalid_grant");
            return Err(if invalid_grant {
                ApiError::App(AppError::unauthorized(
                    ErrorCode::UNAUTHORIZED,
                    "만료되었거나 이미 사용된 Apple authorizationCode입니다.",
                ))
            } else {
                internal("Apple 토큰 요청 설정이 올바르지 않습니다.")
            });
        }
        if response.status >= 400 {
            return Err(internal("Apple 토큰 서버와 통신할 수 없습니다."));
        }
        parse_body(&response)
            .and_then(|body| parse_token_response(&body))
            .ok_or_else(|| internal("Apple 토큰 응답이 올바르지 않습니다."))
    }

    fn revoke_refresh_token(&self, refresh_token: &str) -> Result<(), ApiError> {
        if is_blank(refresh_token) {
            return Err(internal("Apple refresh token이 필요합니다."));
        }
        let secret = self.client_secret.generate();
        let response = self
            .client
            .post_form(
                &self.revoke_uri,
                &[],
                &[
                    ("client_id", &self.client_id),
                    ("client_secret", &secret),
                    ("token", refresh_token),
                    ("token_type_hint", "refresh_token"),
                ],
            )
            .map_err(|_| internal("Apple 토큰 서버와 통신할 수 없습니다."))?;
        if response.status == 400 {
            return Err(internal("Apple 토큰 폐기 요청이 올바르지 않습니다."));
        }
        if response.status >= 400 {
            return Err(internal("Apple 토큰 서버와 통신할 수 없습니다."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    fn valid() -> Value {
        json!({
            "access_token": "at", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": "rt", "id_token": "idt"
        })
    }

    // 응답 검증(AppleTokenResponse#isValid)
    #[test]
    fn 유효한_토큰_응답만_받는다() {
        let ok = parse_token_response(&valid()).unwrap();
        assert_eq!(ok.id_token, "idt");
        assert_eq!(ok.refresh_token, "rt");

        let mut with_extra = valid();
        with_extra["scope"] = json!("x");
        assert!(parse_token_response(&with_extra).is_some());

        for (name, value) in [
            ("access_token", json!(" ")),
            ("access_token", json!(null)),
            ("token_type", json!("mac")),
            ("expires_in", json!(0)),
            ("expires_in", json!(-1)),
            ("expires_in", json!(null)),
            ("refresh_token", json!("")),
            ("id_token", json!("\t")),
        ] {
            let mut body = valid();
            body[name] = value.clone();
            assert!(parse_token_response(&body).is_none(), "{name}={value}");
        }
        let mut missing = valid();
        missing.as_object_mut().unwrap().remove("id_token");
        assert!(parse_token_response(&missing).is_none());
    }

    #[test]
    fn 토큰_종류는_대소문자를_무시한다() {
        let mut body = valid();
        body["token_type"] = json!("bEaReR");
        assert!(parse_token_response(&body).is_some());
    }

    #[test]
    fn jackson처럼_스칼라를_강제_변환한다() {
        let mut body = valid();
        body["expires_in"] = json!("3600");
        assert!(parse_token_response(&body).is_some());
        body["expires_in"] = json!(3600.5);
        assert!(parse_token_response(&body).is_some());
        body["expires_in"] = json!(true);
        assert!(parse_token_response(&body).is_none());
    }

    // AppleHttpTokenClientTest: 실제 소켓으로 응답을 돌려주는 한 번짜리 서버를 쓴다.

    struct FixedSecret;
    impl ClientSecretSource for FixedSecret {
        fn generate(&self) -> String {
            "test-client-secret".to_owned()
        }
    }

    /// 요청 한 건을 받아 `response`를 돌려주고 받은 요청 전체(머리글+본문)를 넘긴다.
    fn serve_once(response: &'static str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&received).into_owned();
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let length = text[..header_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if received.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });
        (url, handle)
    }

    fn unused_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    fn client(token_uri: &str, revoke_uri: &str) -> AppleHttpTokenClient {
        AppleHttpTokenClient::new(
            token_uri,
            revoke_uri,
            "com.chalkak.ios",
            Arc::new(FixedSecret),
        )
        .unwrap()
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }

    fn token_json(
        access: &str,
        token_type: &str,
        expires_in: i64,
        refresh: &str,
        id: &str,
    ) -> String {
        format!(
            r#"{{"access_token":"{access}","token_type":"{token_type}","expires_in":{expires_in},"refresh_token":"{refresh}","id_token":"{id}"}}"#
        )
    }

    fn valid_token_json() -> String {
        token_json(
            "apple-access-token",
            "bearer",
            3600,
            "apple-refresh-token",
            "apple-id-token",
        )
    }

    fn message_of(error: ApiError) -> (u16, String) {
        (error.status(), error.message())
    }

    #[test]
    fn exchange_authorization_code_valid_code_returns_apple_tokens() {
        let (url, server) = serve_once(leak(json_response("200 OK", &valid_token_json())));
        let result = client(&url, &url)
            .exchange_authorization_code("valid-authorization-code")
            .unwrap();
        assert_eq!(result.id_token, "apple-id-token");
        assert_eq!(result.refresh_token, "apple-refresh-token");

        let request = server.join().unwrap();
        assert!(request.starts_with("POST / HTTP/1.1"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{request}"
        );
        assert!(request.ends_with(
            "client_id=com.chalkak.ios&client_secret=test-client-secret&code=valid-authorization-code&grant_type=authorization_code"
        ), "{request}");
    }

    #[test]
    fn exchange_authorization_code_missing_code_throws_unauthorized_exception() {
        for code in ["", " "] {
            let error = client(&unused_url(), &unused_url())
                .exchange_authorization_code(code)
                .unwrap_err();
            assert_eq!(
                message_of(error),
                (401, "Apple authorizationCode가 필요합니다.".to_owned())
            );
        }
    }

    #[test]
    fn exchange_authorization_code_invalid_grant_throws_unauthorized_exception() {
        let (url, server) = serve_once(leak(json_response(
            "400 Bad Request",
            r#"{"error":"invalid_grant"}"#,
        )));
        let error = client(&url, &url)
            .exchange_authorization_code("invalid-code")
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            message_of(error),
            (
                401,
                "만료되었거나 이미 사용된 Apple authorizationCode입니다.".to_owned()
            )
        );
    }

    #[test]
    fn exchange_authorization_code_invalid_client_throws_illegal_state_exception() {
        let (url, server) = serve_once(leak(json_response(
            "400 Bad Request",
            r#"{"error":"invalid_client"}"#,
        )));
        let error = client(&url, &url)
            .exchange_authorization_code("valid-code")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 요청 설정이 올바르지 않습니다.")
        );
        assert_eq!(error.status(), 500);
    }

    #[test]
    fn exchange_authorization_code_server_error_throws_illegal_state_exception() {
        let (url, server) = serve_once(leak(json_response("500 Internal Server Error", "")));
        let error = client(&url, &url)
            .exchange_authorization_code("valid-code")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 서버와 통신할 수 없습니다.")
        );
    }

    #[test]
    fn exchange_authorization_code_network_error_throws_illegal_state_exception() {
        let url = unused_url();
        let error = client(&url, &url)
            .exchange_authorization_code("valid-code")
            .unwrap_err();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 서버와 통신할 수 없습니다.")
        );
    }

    #[test]
    fn exchange_authorization_code_invalid_token_response_throws_illegal_state_exception() {
        for body in [
            token_json("", "bearer", 3600, "apple-refresh-token", "apple-id-token"),
            token_json(
                "apple-access-token",
                "unknown",
                3600,
                "apple-refresh-token",
                "apple-id-token",
            ),
            token_json(
                "apple-access-token",
                "bearer",
                0,
                "apple-refresh-token",
                "apple-id-token",
            ),
            token_json("apple-access-token", "bearer", 3600, "", "apple-id-token"),
            token_json(
                "apple-access-token",
                "bearer",
                3600,
                "apple-refresh-token",
                "",
            ),
        ] {
            let (url, server) = serve_once(leak(json_response("200 OK", &body)));
            let error = client(&url, &url)
                .exchange_authorization_code("valid-code")
                .unwrap_err();
            server.join().unwrap();
            assert!(
                matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 응답이 올바르지 않습니다."),
                "{body}"
            );
        }
    }

    #[test]
    fn revoke_refresh_token_valid_refresh_token_completes_revocation() {
        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        client(&url, &url)
            .revoke_refresh_token("apple-refresh-token")
            .unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST / HTTP/1.1"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{request}"
        );
        assert!(request.ends_with(
            "client_id=com.chalkak.ios&client_secret=test-client-secret&token=apple-refresh-token&token_type_hint=refresh_token"
        ), "{request}");
    }

    #[test]
    fn revoke_refresh_token_missing_refresh_token_throws_illegal_argument_exception() {
        for token in ["", " "] {
            let error = client(&unused_url(), &unused_url())
                .revoke_refresh_token(token)
                .unwrap_err();
            assert!(
                matches!(&error, ApiError::Internal(m) if m == "Apple refresh token이 필요합니다.")
            );
        }
    }

    #[test]
    fn revoke_refresh_token_invalid_grant_throws_illegal_state_exception() {
        // Apple은 이미 폐기된 토큰을 다시 폐기하면 200을 돌려준다. invalid_grant는 진짜 오류이므로 성공으로 보지 않는다.
        let (url, server) = serve_once(leak(json_response(
            "400 Bad Request",
            r#"{"error":"invalid_grant"}"#,
        )));
        let error = client(&url, &url)
            .revoke_refresh_token("apple-refresh-token")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 폐기 요청이 올바르지 않습니다.")
        );
    }

    #[test]
    fn revoke_refresh_token_unreadable_bad_request_body_throws_illegal_state_exception() {
        let (url, server) = serve_once(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let error = client(&url, &url)
            .revoke_refresh_token("apple-refresh-token")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 폐기 요청이 올바르지 않습니다.")
        );
    }

    #[test]
    fn revoke_refresh_token_bad_request_throws_illegal_state_exception() {
        let (url, server) = serve_once(leak(json_response(
            "400 Bad Request",
            r#"{"error":"invalid_client"}"#,
        )));
        let error = client(&url, &url)
            .revoke_refresh_token("apple-refresh-token")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 폐기 요청이 올바르지 않습니다.")
        );
    }

    #[test]
    fn revoke_refresh_token_server_error_throws_illegal_state_exception() {
        let (url, server) = serve_once(leak(json_response("500 Internal Server Error", "")));
        let error = client(&url, &url)
            .revoke_refresh_token("apple-refresh-token")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 서버와 통신할 수 없습니다.")
        );
    }

    #[test]
    fn revoke_refresh_token_network_error_throws_illegal_state_exception() {
        let url = unused_url();
        let error = client(&url, &url)
            .revoke_refresh_token("apple-refresh-token")
            .unwrap_err();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 서버와 통신할 수 없습니다.")
        );
    }
}
