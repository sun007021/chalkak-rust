//! 예외 처리(응답 본문·로그)를 실제 미들웨어 체인 전체로 확인한다. DB는 쓰지 않고, 컨트롤러 대신 각 오류를 던지는
//! 가짜 라우트를 둔다.
//!
//! 원본 `GlobalExceptionHandlerTest`, `GlobalExceptionHandlerLogTest`를 옮겼다. 원본은 MockMvc로 컨트롤러가 던진 예외를
//! 핸들러가 바꾸는 것을 보았고, 여기서는 라우트가 돌려준 오류가 체인을 지나 어떤 응답과 로그가 되는지를 본다.

use std::io::Write;
use std::sync::{Arc, Mutex};

use chalkak_app::web::{ApiError, Routes};
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock};
use chalkak_http::{Chain, Handler, Method, Request, Response};
use chalkak_server::build_chain;
use chalkak_server::settings::Settings;
use serde_json::{Value, json};

const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const NOW: &str = "2026-10-12T00:00:00Z";
const NOW_SECONDS: i64 = 1_791_763_200;
const USER: &str = "01990000-0000-7000-8000-000000000001";

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuf {
    fn lines(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// 접근 로그를 뺀 오류 로그(`type=error`)만.
    fn errors(&self) -> Vec<Value> {
        self.lines()
            .into_iter()
            .filter(|l| l["type"] == "error")
            .collect()
    }
}

struct Harness {
    chain: Chain,
    log: LogBuf,
}

fn harness() -> Harness {
    let mut pairs = vec![
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false"),
    ];
    pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
    let settings = Settings::from_env(&Env::from_pairs(pairs)).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let log = LogBuf::default();
    let logger = Arc::new(Logger::new(
        Box::new(log.clone()),
        Level::Info,
        Arc::clone(&clock),
    ));
    let router = Routes::new(Arc::clone(&logger))
        // @Valid 본문 검증 실패
        .post("/api/v1/test/requests", |_| {
            Err(ApiError::Validation("이름은 비어 있을 수 없습니다.".into()))
        })
        // 파라미터 형식 오류, 제약 위반
        .get("/api/v1/test/count", |req| match req.query_param("count") {
            Some("0") => Err(ApiError::Validation("수량은 1 이상이어야 합니다.".into())),
            _ => Err(ApiError::ArgumentTypeMismatch("count".into())),
        })
        .put("/api/v1/test/unreadable", |_| Err(ApiError::UnreadableBody))
        .post("/api/v1/test/media", |_| {
            Err(ApiError::UnsupportedMediaType)
        })
        .get("/api/v1/test/business-error", |_| {
            Err(ApiError::App(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "비즈니스 요청이 올바르지 않습니다.",
            )))
        })
        .get("/api/v1/test/not-found-error", |_| {
            Err(ApiError::App(AppError::not_found(
                ErrorCode::BUSINESS_ERROR,
                "대상을 찾을 수 없습니다.",
            )))
        })
        .get("/api/v1/test/forbidden-error", |_| {
            Err(ApiError::App(AppError::forbidden(
                ErrorCode::FORBIDDEN,
                "관리자 API에 접근할 수 없습니다.",
            )))
        })
        .get("/api/v1/test/unexpected-error", |_| {
            panic!("예상치 못한 예외")
        })
        // DB 오류는 원인을 응답에 싣지 않는다(DataIntegrityViolationException).
        .get("/api/v1/test/data-integrity-error", |_| {
            Err(ApiError::Internal(
                "duplicate key violates constraint secret_database_constraint".into(),
            ))
        })
        .post("/api/v1/auth/login", |_| Err(ApiError::UnreadableBody))
        .into_router();
    let chain = build_chain(&settings, router, Arc::new(|| true), clock, logger);
    Harness { chain, log }
}

fn user_token() -> String {
    let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
    chalkak_jwt::hs256_encode(
        &json!({"typ":"JWT","alg":"HS256"}),
        &json!({
            "iss": "chalkak-backend", "aud": ["chalkak-access"], "sub": USER,
            "iat": NOW_SECONDS - 5, "exp": NOW_SECONDS + 900, "jti": "j",
            "purpose": "ACCESS", "scope": "USER",
        }),
        &key,
    )
}

/// `target`의 `?` 뒤는 `이름=값&...` 쿼리로 나눠 넣는다(값은 이미 디코딩된 문자열).
fn request(method: Method, target: &str) -> Request {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut req = Request::new(method, path);
    req.raw_query = query.to_owned();
    req.query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (name.to_owned(), value.to_owned())
        })
        .collect();
    req
}

fn send(h: &Harness, method: Method, target: &str, authed: bool) -> Response {
    let mut req = request(method, target);
    req.headers.append("Host", "127.0.0.1:18082");
    if authed {
        req.headers
            .append("Authorization", format!("Bearer {}", user_token()));
    }
    h.chain.handle(req)
}

fn assert_error(r: &Response, status: u16, code: &str, message: &str) {
    assert_eq!(r.status, status, "{}", String::from_utf8_lossy(&r.body));
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["errorCode"], code);
    assert_eq!(body["message"], message);
}

// ---------------------------------------------------------------------------------------------
// GlobalExceptionHandlerTest
// ---------------------------------------------------------------------------------------------

// GlobalExceptionHandlerTest#dtoValidationFailure
#[test]
fn dto_validation_failure() {
    let h = harness();
    let r = send(&h, Method::Post, "/api/v1/test/requests", true);
    assert_error(&r, 400, "BUSINESS_ERROR", "이름은 비어 있을 수 없습니다.");
}

// GlobalExceptionHandlerTest#typeMismatchFailureKeepsFieldName
#[test]
fn type_mismatch_failure_keeps_field_name() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/count?count=한글", true);
    assert_error(
        &r,
        400,
        "BUSINESS_ERROR",
        "count: 요청 값의 형식이 올바르지 않습니다.",
    );
}

// GlobalExceptionHandlerTest#invalidJsonValue, #emptyRequestBody (둘 다 읽을 수 없는 본문)
#[test]
fn invalid_json_value_and_empty_request_body() {
    let h = harness();
    let r = send(&h, Method::Put, "/api/v1/test/unreadable", true);
    assert_error(
        &r,
        400,
        "BUSINESS_ERROR",
        "JSON 형식이 올바르지 않거나 요청 본문이 비어 있습니다.",
    );
}

// GlobalExceptionHandlerTest#methodParameterValidationFailure
#[test]
fn method_parameter_validation_failure() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/count?count=0", true);
    assert_eq!(r.status, 400);
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["errorCode"], "BUSINESS_ERROR");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("수량은 1 이상이어야 합니다.")
    );
}

// GlobalExceptionHandlerTest#apiNotFound
#[test]
fn api_not_found() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/not-found", true);
    assert_error(&r, 404, "BUSINESS_ERROR", "요청한 API를 찾을 수 없습니다.");
}

// GlobalExceptionHandlerTest#businessException
#[test]
fn business_exception() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/business-error", true);
    assert_error(
        &r,
        400,
        "BUSINESS_ERROR",
        "비즈니스 요청이 올바르지 않습니다.",
    );
}

// GlobalExceptionHandlerTest#notFoundException
#[test]
fn not_found_exception() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/not-found-error", true);
    assert_error(&r, 404, "BUSINESS_ERROR", "대상을 찾을 수 없습니다.");
}

// GlobalExceptionHandlerTest#forbiddenException
#[test]
fn forbidden_exception() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/forbidden-error", true);
    assert_error(&r, 403, "FORBIDDEN", "관리자 API에 접근할 수 없습니다.");
}

// GlobalExceptionHandlerTest#unsupportedHttpMethod
#[test]
fn unsupported_http_method() {
    let h = harness();
    let r = send(&h, Method::Put, "/api/v1/test/business-error", true);
    assert_error(
        &r,
        405,
        "BUSINESS_ERROR",
        "지원하지 않는 요청 방식이거나 형식입니다.",
    );
}

// GlobalExceptionHandlerTest#unsupportedMediaType
#[test]
fn unsupported_media_type() {
    let h = harness();
    let r = send(&h, Method::Post, "/api/v1/test/media", true);
    assert_error(
        &r,
        415,
        "BUSINESS_ERROR",
        "지원하지 않는 요청 방식이거나 형식입니다.",
    );
}

// GlobalExceptionHandlerTest#unexpectedException
#[test]
fn unexpected_exception() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/unexpected-error", true);
    assert_error(
        &r,
        500,
        "INTERNAL_ERROR",
        "서버에서 요청을 처리하지 못했습니다.",
    );
}

// GlobalExceptionHandlerTest#handleUnexpectedException_dataIntegrityViolation_returnsSanitizedInternalError
#[test]
fn handle_unexpected_exception_data_integrity_violation_returns_sanitized_internal_error() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/data-integrity-error", true);
    assert_error(
        &r,
        500,
        "INTERNAL_ERROR",
        "서버에서 요청을 처리하지 못했습니다.",
    );
    assert!(!String::from_utf8_lossy(&r.body).contains("secret_database_constraint"));
}

// ---------------------------------------------------------------------------------------------
// GlobalExceptionHandlerLogTest
// ---------------------------------------------------------------------------------------------

// GlobalExceptionHandlerLogTest#handleHttpMessageNotReadableException_malformedJson_logsWarnWithoutMessage
#[test]
fn handle_http_message_not_readable_exception_malformed_json_logs_warn_without_message() {
    let h = harness();
    let r = send(&h, Method::Post, "/api/v1/auth/login", false);
    assert_eq!(r.status, 400);

    let errors = h.log.errors();
    assert_eq!(errors.len(), 1);
    let line = &errors[0];
    assert_eq!(line["level"], "WARN");
    assert_eq!(line["errorCode"], "BUSINESS_ERROR");
    assert_eq!(line["status"], 400);
    assert_eq!(line["exception"], "HttpMessageNotReadableException");
    assert!(line.get("stack_trace").is_none());
    assert!(line.get("requestId").is_some());
}

// GlobalExceptionHandlerLogTest#handleBaseException_businessException_logsWarnWithoutMessage
#[test]
fn handle_base_exception_business_exception_logs_warn_without_message() {
    let h = harness();
    let r = send(
        &h,
        Method::Get,
        "/api/v1/test/business-error?id=not-a-uuid",
        true,
    );
    assert_eq!(r.status, 400);

    let errors = h.log.errors();
    assert_eq!(errors.len(), 1);
    let line = &errors[0];
    assert_eq!(line["level"], "WARN");
    assert_eq!(line["errorCode"], "BUSINESS_ERROR");
    assert_eq!(line["status"], 400);
    assert_eq!(line["exception"], "BusinessException");
    assert!(
        !line["message"]
            .as_str()
            .unwrap()
            .contains("비즈니스 요청이 올바르지 않습니다.")
    );
    assert!(!line.to_string().contains("not-a-uuid"));
    assert!(line.get("stack_trace").is_none());
}

// GlobalExceptionHandlerLogTest#handleUnexpectedException_methodNotAllowed_logsWarnOnly
#[test]
fn handle_unexpected_exception_method_not_allowed_logs_warn_only() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/auth/login", false);
    assert_eq!(r.status, 405);

    let errors = h.log.errors();
    assert_eq!(errors.len(), 1);
    let line = &errors[0];
    assert_eq!(line["level"], "WARN");
    assert_eq!(line["errorCode"], "BUSINESS_ERROR");
    assert_eq!(line["status"], 405);
    assert_eq!(line["exception"], "HttpRequestMethodNotSupportedException");
    assert!(line.get("stack_trace").is_none());
}

// GlobalExceptionHandlerLogTest#handleUnexpectedException_unexpectedFailure_logsErrorWithThrowable
#[test]
fn handle_unexpected_exception_unexpected_failure_logs_error_with_throwable() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/test/unexpected-error", true);
    assert_eq!(r.status, 500);

    let errors = h.log.errors();
    assert_eq!(errors.len(), 1);
    let line = &errors[0];
    assert_eq!(line["level"], "ERROR");
    assert_eq!(line["errorCode"], "INTERNAL_ERROR");
    assert_eq!(line["status"], 500);
    assert!(
        line["stack_trace"]
            .as_str()
            .unwrap()
            .contains("예상치 못한 예외")
    );
    assert!(line.get("requestId").is_some());
}

// GlobalExceptionHandlerLogTest#handleApiNotFoundException_unknownPath_logsNothing
#[test]
fn handle_api_not_found_exception_unknown_path_logs_nothing() {
    let h = harness();
    let r = send(&h, Method::Get, "/api/v1/auth/no-such-path", false);
    assert_eq!(r.status, 404);

    assert!(h.log.errors().is_empty());
}

// GlobalExceptionHandlerLogTest#handleHttpMessageNotReadableException_requestWithSensitiveValues_logsWithoutThem
#[test]
fn handle_http_message_not_readable_exception_request_with_sensitive_values_logs_without_them() {
    let h = harness();
    let email = "leak-check@chalkak.test";
    let token = "secret-bearer-token-value";
    let cookie = "secret-cookie-value";

    let mut first = request(Method::Post, &format!("/api/v1/auth/login?email={email}"));
    first.headers.append("Host", "127.0.0.1:18082");
    first.headers.append("Cookie", format!("session={cookie}"));
    first.headers.append("Content-Type", "application/json");
    first.body = format!("{{\"email\":\"{email}\",").into_bytes();
    assert_eq!(h.chain.handle(first).status, 400);

    let mut second = Request::new(Method::Put, format!("/api/v1/posts/{USER}/likes"));
    second.headers.append("Host", "127.0.0.1:18082");
    second
        .headers
        .append("Authorization", format!("Bearer {token}"));
    second.headers.append("Cookie", format!("session={cookie}"));
    assert_eq!(h.chain.handle(second).status, 401);

    let errors = h.log.errors();
    assert_eq!(errors.len(), 2);
    for line in &errors {
        let text = line.to_string();
        for secret in [email, token, cookie] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(line.get("stack_trace").is_none());
    }
}
