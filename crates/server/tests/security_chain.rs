//! 미들웨어 체인 전체(방화벽, 보안 헤더, CORS, Bearer 인증, 인가, 접근 로그, actuator)를 DB 없이 통과시켜 보는 테스트.
//!
//! 도메인 엔드포인트는 가짜 라우트로 대신하고, 라우트에 닿았는지·어떤 응답이 나가는지만 확인한다. 원본
//! `SecurityFilterChainTest`, `AdminSecurityFilterChainTest`, `AccessLogTest`를 옮겼고, 하네스 시나리오(`harness/golden`)가
//! 같은 동작을 실제 DB와 함께 다시 확인한다.

use std::io::Write;
use std::sync::{Arc, Mutex};

use chalkak_app::web::{ApiError, Routes};
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock};
use chalkak_http::{Handler, Method, Request, Response};
use chalkak_server::build_chain;
use chalkak_server::settings::Settings;
use serde_json::{Value, json};

const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const NOW: &str = "2026-10-12T00:00:00Z";
const NOW_SECONDS: i64 = 1_791_763_200;
const USER: &str = "01990000-0000-7000-8000-000000000001";
const ADMIN: &str = "01990000-0000-7000-8000-0000000000a1";

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
}

struct Harness {
    chain: chalkak_http::Chain,
    log: LogBuf,
}

fn harness_with(bypass: bool, database_up: bool) -> Harness {
    let env = Env::from_pairs([
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
        (
            "ADMIN_DEVELOPMENT_BYPASS_ENABLED",
            if bypass { "true" } else { "false" },
        ),
        ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
        (
            "IMAGE_PROCESSOR_CALLBACK_SECRET",
            "test-callback-secret-with-enough-length",
        ),
        ("CALLBACK_MAX_BODY_BYTES", "65536"),
        ("POST_IMAGE_PROCESSING_TIMEOUT", "7m"),
    ]);
    let settings = Settings::from_env(&env).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let log = LogBuf::default();
    let logger = Arc::new(Logger::new(
        Box::new(log.clone()),
        Level::Info,
        Arc::clone(&clock),
    ));
    let not_found = || {
        ApiError::App(AppError::not_found(
            ErrorCode::BUSINESS_ERROR,
            "주제를 찾을 수 없습니다.",
        ))
    };
    let ok = || Ok(Response::new(200).with_body("{}"));
    let router = Routes::new(Arc::clone(&logger))
        .get("/api/v1/topics", move |_| Err(not_found()))
        .get("/api/v1/posts", move |_| ok())
        .get("/api/v1/posts/{postId}", move |_| ok())
        .post("/api/v1/posts/{postId}/likes", move |_| ok())
        .post("/api/v1/auth/login", move |_| ok())
        .post("/api/v1/auth/refresh", |_| {
            Err(ApiError::App(AppError::unauthorized(
                ErrorCode::UNAUTHORIZED,
                "유효하지 않은 인증 정보입니다.",
            )))
        })
        .post("/api/v1/auth/boom", |_| panic!("boom"))
        .post("/api/v1/admin/auth/login", move |_| ok())
        .post("/api/v1/admin/auth/refresh", move |_| ok())
        .post("/api/v1/admin/auth/logout", move |_| ok())
        .get("/api/v1/admin/feedbacks", move |_| ok())
        .post("/internal/v1/post-images/callback", move |_| ok())
        .into_router();
    let chain = build_chain(
        &settings,
        router,
        Arc::new(move || database_up),
        clock,
        logger,
    );
    Harness { chain, log }
}

fn harness() -> Harness {
    harness_with(false, true)
}

fn token(payload: Value) -> String {
    let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
    chalkak_jwt::hs256_encode(&json!({"typ":"JWT","alg":"HS256"}), &payload, &key)
}

fn claims(sub: &str, scope: Option<&str>) -> Value {
    let mut c = json!({
        "iss": "chalkak-backend",
        "aud": ["chalkak-access"],
        "sub": sub,
        "iat": NOW_SECONDS - 5,
        "exp": NOW_SECONDS + 900,
        "jti": "j",
        "purpose": "ACCESS",
    });
    if let Some(scope) = scope {
        c["scope"] = json!(scope);
    }
    c
}

fn user_token() -> String {
    token(claims(USER, Some("USER")))
}

fn admin_token() -> String {
    token(claims(ADMIN, Some("ADMIN")))
}

fn send(h: &Harness, method: Method, path: &str, bearer: Option<&str>) -> Response {
    let mut req = Request::new(method, path);
    req.headers.append("Host", "127.0.0.1:18082");
    if let Some(t) = bearer {
        req.headers.append("Authorization", format!("Bearer {t}"));
    }
    h.chain.handle(req)
}

fn get(h: &Harness, path: &str, bearer: Option<&str>) -> Response {
    send(h, Method::Get, path, bearer)
}

fn body(r: &Response) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn assert_unauthorized(r: &Response) {
    assert_eq!(r.status, 401);
    assert_eq!(r.headers.get("www-authenticate"), Some("Bearer"));
    assert_eq!(
        r.headers.get("content-type"),
        Some("application/json;charset=UTF-8")
    );
    assert_eq!(
        body(r),
        json!({"errorCode": "UNAUTHORIZED", "message": "유효하지 않은 인증 정보입니다."})
    );
}

fn assert_forbidden(r: &Response) {
    assert_eq!(r.status, 403);
    assert!(!r.headers.contains("www-authenticate"));
    assert_eq!(
        body(r),
        json!({"errorCode": "FORBIDDEN", "message": "접근 권한이 없습니다."})
    );
}

// SecurityFilterChainTest#protectedApi_withoutToken_returnsUnauthorized, #protectedApi_withoutToken_setsWwwAuthenticateHeader
#[test]
fn protected_api_without_token_returns_unauthorized() {
    let h = harness();
    assert_unauthorized(&send(&h, Method::Post, "/api/v1/posts/1/likes", None));
}

// SecurityFilterChainTest#protectedApi_validAccessToken_reachesController
#[test]
fn protected_api_valid_access_token_reaches_controller() {
    let h = harness();
    let r = send(
        &h,
        Method::Post,
        "/api/v1/posts/1/likes",
        Some(&user_token()),
    );
    assert_eq!(r.status, 200);
}

// SecurityFilterChainTest#protectedApi_tamperedToken_returnsUnauthorized
#[test]
fn protected_api_tampered_token_returns_unauthorized() {
    let h = harness();
    let tampered = format!("{}x", user_token());
    assert_unauthorized(&send(
        &h,
        Method::Post,
        "/api/v1/posts/1/likes",
        Some(&tampered),
    ));
}

// SecurityFilterChainTest#protectedApi_socialSignupToken_returnsUnauthorized
#[test]
fn protected_api_social_signup_token_returns_unauthorized() {
    let h = harness();
    let mut c = claims(USER, None);
    c["aud"] = json!(["chalkak-social-signup"]);
    c["purpose"] = json!("SOCIAL_SIGNUP");
    assert_unauthorized(&send(
        &h,
        Method::Post,
        "/api/v1/posts/1/likes",
        Some(&token(c)),
    ));
}

// SecurityFilterChainTest#authApi_withoutToken_reachesController
#[test]
fn auth_api_without_token_reaches_controller() {
    let h = harness();
    assert_eq!(
        send(&h, Method::Post, "/api/v1/auth/login", None).status,
        200
    );
}

// SecurityFilterChainTest#publicPath_controllerRejection_doesNotSetWwwAuthenticateHeader
#[test]
fn public_path_controller_rejection_does_not_set_www_authenticate_header() {
    let h = harness();
    let r = send(&h, Method::Post, "/api/v1/auth/refresh", None);
    assert_eq!(r.status, 401);
    assert!(!r.headers.contains("www-authenticate"));
    assert_eq!(r.headers.get("content-type"), Some("application/json"));
}

// SecurityFilterChainTest#publicPath_unhandledException_returnsInternalServerError
#[test]
fn public_path_unhandled_exception_returns_internal_server_error() {
    let h = harness();
    let r = send(&h, Method::Post, "/api/v1/auth/boom", None);
    assert_eq!(r.status, 500);
    assert_eq!(body(&r)["errorCode"], "INTERNAL_ERROR");
}

// SecurityFilterChainTest#topics_withoutToken_reachesController, #posts_withoutToken_reachesController
#[test]
fn public_lists_without_token_reach_controller() {
    let h = harness();
    // 가짜 주제 라우트는 404 업무 오류를 낸다. 인증 단계에서 막혔다면 401이었을 것이다.
    let r = get(&h, "/api/v1/topics", None);
    assert_eq!(r.status, 404);
    assert_eq!(body(&r)["message"], "주제를 찾을 수 없습니다.");
    assert_eq!(get(&h, "/api/v1/posts", None).status, 200);
}

// SecurityFilterChainTest#postDetail_withoutToken_isRejectedByFilter
#[test]
fn post_detail_without_token_is_rejected_by_filter() {
    let h = harness();
    assert_unauthorized(&get(&h, "/api/v1/posts/1", None));
}

// SecurityFilterChainTest#posts_adminToken_returnsForbidden, #protectedApi_adminToken_returnsForbidden
#[test]
fn admin_token_is_forbidden_on_user_apis() {
    let h = harness();
    assert_forbidden(&get(&h, "/api/v1/posts", Some(&admin_token())));
    assert_forbidden(&get(&h, "/api/v1/topics", Some(&admin_token())));
    assert_forbidden(&send(
        &h,
        Method::Post,
        "/api/v1/posts/1/likes",
        Some(&admin_token()),
    ));
}

// SecurityFilterChainTest#protectedApi_tokenWithoutScope_returnsForbidden
#[test]
fn token_without_scope_is_forbidden_on_user_apis() {
    let h = harness();
    let t = token(claims(USER, None));
    assert_forbidden(&send(&h, Method::Post, "/api/v1/posts/1/likes", Some(&t)));
}

// SecurityFilterChainTest#internalCallback_withoutAccessToken_reachesController
#[test]
fn internal_callback_without_access_token_reaches_controller() {
    let h = harness();
    let r = send(&h, Method::Post, "/internal/v1/post-images/callback", None);
    assert_eq!(r.status, 200);
}

// CallbackBodySizeLimitFilterTest#oversizedBody_keepsRequestIdHeader
#[test]
fn oversized_body_keeps_request_id_header() {
    let h = harness();
    let mut req = Request::new(Method::Post, "/internal/v1/post-images/callback");
    req.headers.append("Host", "127.0.0.1:18082");
    req.headers.append("Content-Length", "65537");
    req.body = vec![b'x'; 65_537];
    let r = h.chain.handle(req);
    assert_eq!(r.status, 413);
    assert_eq!(
        body(&r),
        json!({"errorCode": "BUSINESS_ERROR", "message": "이미지 처리 콜백 본문이 너무 큽니다."})
    );
    assert!(uuid::Uuid::parse_str(r.headers.get("x-request-id").unwrap()).is_ok());
}

// SecurityFilterChainTest#health_withoutToken_returnsOk
#[test]
fn health_without_token_returns_ok() {
    let h = harness();
    let r = get(&h, "/actuator/health", None);
    assert_eq!(r.status, 200);
    assert_eq!(
        r.headers.get("content-type"),
        Some("application/vnd.spring-boot.actuator.v3+json")
    );
    assert_eq!(
        body(&r),
        json!({"groups": ["liveness", "readiness"], "status": "UP"})
    );
    assert_eq!(get(&h, "/actuator/info", None).status, 200);
}

#[test]
fn health_is_down_with_503_when_database_is_unreachable() {
    let h = harness_with(false, false);
    let r = get(&h, "/actuator/health", None);
    assert_eq!(r.status, 503);
    assert_eq!(body(&r)["status"], "DOWN");
}

#[test]
fn other_actuator_endpoints_need_a_user_token() {
    let h = harness();
    for path in [
        "/actuator",
        "/actuator/health/liveness",
        "/actuator/health/",
    ] {
        assert_unauthorized(&get(&h, path, None));
    }
    let t = user_token();
    let r = get(&h, "/actuator/health/liveness", Some(&t));
    assert_eq!(body(&r), json!({"status": "UP"}));
    let r = get(&h, "/actuator", Some(&t));
    assert_eq!(
        body(&r)["_links"]["self"]["href"],
        "http://127.0.0.1:18082/actuator"
    );
    assert_eq!(get(&h, "/actuator/health/foo", Some(&t)).status, 404);
    assert!(get(&h, "/actuator/health/foo", Some(&t)).body.is_empty());
    assert_eq!(get(&h, "/actuator/beans", Some(&t)).status, 404);
    assert_eq!(send(&h, Method::Post, "/actuator/health", None).status, 405);
}

// 원본 /error 디스패치: 모든 메서드에서 500, status 999.
#[test]
fn error_page_is_public_and_closes_the_connection() {
    let h = harness();
    let r = send(&h, Method::Post, "/error", None);
    assert_eq!(r.status, 500);
    assert_eq!(r.headers.get("connection"), Some("close"));
    assert_eq!(body(&r)["status"], 999);
    assert_eq!(body(&r)["error"], "None");
}

// AdminSecurityFilterChainTest#adminApi_withoutToken_returnsUnauthorized, #adminApi_userToken_returnsForbidden
#[test]
fn admin_api_requires_an_admin_token() {
    let h = harness();
    assert_unauthorized(&get(&h, "/api/v1/admin/feedbacks", None));
    assert_forbidden(&get(&h, "/api/v1/admin/feedbacks", Some(&user_token())));
    assert_eq!(
        get(&h, "/api/v1/admin/feedbacks", Some(&admin_token())).status,
        200
    );
}

// AdminSecurityFilterChainTest#login_withoutToken_reachesController, #refreshAndLogout_withoutAccessToken_reachController
#[test]
fn admin_login_refresh_logout_are_public() {
    let h = harness();
    for path in [
        "/api/v1/admin/auth/login",
        "/api/v1/admin/auth/refresh",
        "/api/v1/admin/auth/logout",
    ] {
        assert_eq!(send(&h, Method::Post, path, None).status, 200, "{path}");
    }
}

#[test]
fn admin_development_bypass_opens_admin_api_without_a_token() {
    let h = harness_with(true, true);
    assert_eq!(get(&h, "/api/v1/admin/feedbacks", None).status, 200);
    // 우회는 관리자 경로만 연다.
    assert_unauthorized(&get(&h, "/api/v1/posts/1", None));
}

#[test]
fn an_invalid_token_is_rejected_even_on_public_paths() {
    let h = harness();
    assert_unauthorized(&get(&h, "/api/v1/topics", Some("abc.def.ghi")));
    let mut req = Request::new(Method::Get, "/api/v1/topics");
    req.headers.append("Authorization", "Basic dXNlcjpwYXNz");
    assert_eq!(h.chain.handle(req).status, 404, "Basic은 익명 취급");
}

#[test]
fn a_token_without_audience_ends_in_a_boot_style_500() {
    let h = harness();
    let mut c = claims(USER, Some("USER"));
    c.as_object_mut().unwrap().remove("aud");
    let r = get(&h, "/api/v1/topics", Some(&token(c)));
    assert_eq!(r.status, 500);
    assert_eq!(r.headers.get("connection"), Some("close"));
    assert_eq!(r.headers.get("content-type"), Some("application/json"));
    assert!(r.headers.contains("x-request-id"));
    assert!(r.headers.contains("cache-control"));
    let b = body(&r);
    assert_eq!(b["status"], 500);
    assert_eq!(b["error"], "Internal Server Error");
    assert_eq!(b["path"], "/api/v1/topics");
}

#[test]
fn every_response_carries_request_id_and_security_headers() {
    let h = harness();
    for r in [
        get(&h, "/api/v1/topics", None),
        get(&h, "/api/v1/nope", Some(&user_token())),
        get(&h, "/api/v1/posts/1", None),
        send(&h, Method::Delete, "/api/v1/posts", Some(&user_token())),
    ] {
        assert!(uuid::Uuid::parse_str(r.headers.get("x-request-id").unwrap()).is_ok());
        assert_eq!(r.headers.get("x-frame-options"), Some("DENY"));
        assert_eq!(r.headers.get("expires"), Some("0"));
    }
}

#[test]
fn firewall_rejections_have_only_the_container_headers() {
    let h = harness();
    let r = get(&h, "/api/v1/topics;jsessionid=1", None);
    assert_eq!(r.status, 400);
    assert_eq!(r.headers.get("connection"), Some("close"));
    assert!(r.headers.contains("x-request-id"));
    assert!(!r.headers.contains("x-frame-options"));
    assert!(!r.headers.contains("cache-control"));
}

#[test]
fn trace_ends_before_the_filter_chain() {
    let h = harness();
    let r = send(&h, Method::Trace, "/api/v1/topics", None);
    assert_eq!(r.status, 405);
    assert!(!r.headers.contains("x-request-id"));
    assert!(!r.headers.contains("x-frame-options"));
    assert!(h.log.lines().is_empty());
}

// MVC 404/405: 인증을 통과한 뒤에야 판정된다. platform/unknown-path, platform/method-not-allowed
#[test]
fn mvc_fallbacks_are_decided_after_authentication() {
    let h = harness();
    assert_unauthorized(&get(&h, "/api/v1/nope", None));
    let t = user_token();
    let r = get(&h, "/api/v1/nope", Some(&t));
    assert_eq!(r.status, 404);
    assert_eq!(body(&r)["message"], "요청한 API를 찾을 수 없습니다.");

    assert_unauthorized(&send(&h, Method::Delete, "/api/v1/posts", None));
    let r = send(&h, Method::Delete, "/api/v1/posts", Some(&t));
    assert_eq!(r.status, 405);
    assert_eq!(
        body(&r)["message"],
        "지원하지 않는 요청 방식이거나 형식입니다."
    );
    assert!(!r.headers.contains("allow"));

    // HEAD와 OPTIONS는 공개 GET 규칙에 맞지 않아 익명이면 401이다.
    assert_unauthorized(&send(&h, Method::Head, "/api/v1/posts", None));
    assert_unauthorized(&send(&h, Method::Options, "/api/v1/posts", None));
    let r = send(&h, Method::Options, "/api/v1/posts", Some(&t));
    assert_eq!(r.status, 200);
    assert_eq!(r.headers.get("allow"), Some("GET,HEAD,OPTIONS"));
}

// 비관리자 경로의 사전 요청은 CORS 처리 없이 빈 200이다(인가를 통과하면).
#[test]
fn preflight_on_non_admin_path_is_an_empty_200() {
    let h = harness();
    let mut req = Request::new(Method::Options, "/api/v1/auth/login");
    req.headers.append("Origin", "http://localhost:3000");
    req.headers.append("Access-Control-Request-Method", "POST");
    let r = h.chain.handle(req);
    assert_eq!(r.status, 200);
    assert!(!r.headers.contains("access-control-allow-origin"));
    assert!(!r.headers.contains("vary"));
}

// platform/cors
#[test]
fn admin_cors_preflight_needs_no_token_and_other_origins_are_rejected() {
    let h = harness();
    let mut req = Request::new(Method::Options, "/api/v1/admin/feedbacks");
    req.headers.append("Host", "127.0.0.1:18082");
    req.headers.append("Origin", "http://localhost:3000");
    req.headers.append("Access-Control-Request-Method", "GET");
    req.headers.append(
        "Access-Control-Request-Headers",
        "authorization, content-type",
    );
    let r = h.chain.handle(req);
    assert_eq!(r.status, 200);
    assert_eq!(
        r.headers.get("access-control-allow-origin"),
        Some("http://localhost:3000")
    );
    assert!(r.headers.contains("x-request-id"));
    assert!(r.headers.contains("x-frame-options"));

    let mut req = Request::new(Method::Get, "/api/v1/admin/feedbacks");
    req.headers.append("Origin", "http://evil.example");
    let r = h.chain.handle(req);
    assert_eq!(r.status, 403);
    assert_eq!(r.body, b"Invalid CORS request");

    // 허용 Origin의 실제 요청: 인증이 실패해도 CORS 헤더는 붙는다.
    let mut req = Request::new(Method::Get, "/api/v1/admin/feedbacks");
    req.headers.append("Host", "127.0.0.1:18082");
    req.headers.append("Origin", "http://localhost:3000");
    let r = h.chain.handle(req);
    assert_unauthorized(&r);
    assert_eq!(
        r.headers.get("access-control-expose-headers"),
        Some("X-Request-Id")
    );
}

// AccessLogTest
#[test]
fn access_log_records_route_template_status_and_user() {
    let h = harness();
    let t = user_token();
    send(&h, Method::Post, "/api/v1/posts/1/likes", Some(&t));
    send(&h, Method::Post, "/api/v1/posts/1/likes", None);
    get(&h, "/api/v1/nope", Some(&t));
    get(&h, "/actuator/health", None);

    let access: Vec<Value> = h
        .log
        .lines()
        .into_iter()
        .filter(|l| l["logger_name"] == "chalkak.access")
        .collect();
    assert_eq!(access.len(), 3, "actuator는 남기지 않는다");

    // 매칭된 요청: 경로 템플릿(식별자가 아니라), 상태, 회원 식별자, requestId
    assert_eq!(access[0]["route"], "/api/v1/posts/{postId}/likes");
    assert_eq!(access[0]["method"], "POST");
    assert_eq!(access[0]["status"], 200);
    assert_eq!(access[0]["type"], "access");
    assert_eq!(access[0]["userId"], USER);
    assert!(access[0]["durationMs"].is_number());
    assert!(uuid::Uuid::parse_str(access[0]["requestId"].as_str().unwrap()).is_ok());

    // 보안 단계에서 거절된 요청: UNMATCHED, userId 없음
    assert_eq!(access[1]["route"], "UNMATCHED");
    assert_eq!(access[1]["status"], 401);
    assert!(access[1].get("userId").is_none());

    // 라우트가 없는 요청도 UNMATCHED
    assert_eq!(access[2]["route"], "UNMATCHED");
    assert_eq!(access[2]["status"], 404);
}

// GlobalExceptionHandlerLogTest, UnauthorizedEntryPoint, ForbiddenAccessDeniedHandler
#[test]
fn rejections_are_logged_without_secrets() {
    let h = harness();
    send(&h, Method::Post, "/api/v1/posts/1/likes", None);
    send(
        &h,
        Method::Post,
        "/api/v1/posts/1/likes",
        Some(&admin_token()),
    );
    get(&h, "/api/v1/topics", Some("abc.def.ghi"));

    let lines = h.log.lines();
    let by_message =
        |m: &str| -> Vec<&Value> { lines.iter().filter(|l| l["message"] == m).collect() };
    let fail = by_message("인증 실패");
    assert_eq!(fail.len(), 2);
    assert_eq!(
        fail[0]["exception"], "InsufficientAuthenticationException",
        "익명 거부"
    );
    assert_eq!(fail[1]["exception"], "InvalidBearerTokenException");
    assert_eq!(fail[0]["level"], "WARN");
    assert_eq!(fail[0]["errorCode"], "UNAUTHORIZED");
    assert_eq!(fail[0]["status"], 401);
    let denied = by_message("접근 거부");
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0]["errorCode"], "FORBIDDEN");
    assert_eq!(denied[0]["status"], 403);
    let all = serde_json::to_string(&lines).unwrap();
    assert!(!all.contains("abc.def.ghi"), "토큰은 로그에 남지 않는다");
}
