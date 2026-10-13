//! 회원 상태에 따른 접근 정책을 실제 체인(인증, 인가, 콜백 본문 제한)과 DB로 확인한다. `CHALKAK_TEST_DB_URL`이 없으면
//! 건너뛴다.
//!
//! 원본 `SuspendedUserAccessTest`, `WithdrawnUserAccessTest`, `CallbackBodySizeLimitFilterTest` 중 auth·user 도메인
//! 엔드포인트(사인 업로드·교체·조회, 탈퇴)와 콜백 필터를 옮겼다. 게시물·좋아요·피드백 행은 각 도메인에서 옮긴다.
//! 원본처럼 회원 행은 SQL로 직접 넣고, 토큰은 서명한 액세스 토큰을 쓴다.

use std::sync::Arc;

use chalkak_app::testing::TestDb;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock};
use chalkak_http::{Handler, Method, Request, Response};
use chalkak_server::build_handler;
use chalkak_server::settings::Settings;
use serde_json::{Value, json};
use uuid::Uuid;

const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const NOW: &str = "2026-10-12T03:00:00Z";
const NOW_SECONDS: i64 = 1_791_774_000;
const UPLOAD_ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6570e1";

fn handler(db: &TestDb) -> Arc<dyn Handler> {
    let mut pairs = vec![
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false"),
    ];
    pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
    let settings = Settings::from_env(&Env::from_pairs(pairs)).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let logger = Arc::new(Logger::new(
        Box::new(std::io::sink()),
        Level::Info,
        Arc::clone(&clock),
    ));
    build_handler(&settings, db.pool(4), clock, logger)
}

fn token_for(user_id: Uuid) -> String {
    let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
    chalkak_jwt::hs256_encode(
        &json!({"typ":"JWT","alg":"HS256"}),
        &json!({
            "iss": "chalkak-backend", "aud": ["chalkak-access"], "sub": user_id.to_string(),
            "iat": NOW_SECONDS - 5, "exp": NOW_SECONDS + 900, "jti": "j",
            "purpose": "ACCESS", "scope": "USER",
        }),
        &key,
    )
}

/// SQL로 직접 넣은 회원과 그 회원의 Bearer 토큰.
fn insert_user(db: &TestDb, status: &str, withdrawn: bool) -> String {
    let user_id = Uuid::new_v4();
    let deleted_at = if withdrawn {
        "CURRENT_TIMESTAMP"
    } else {
        "NULL"
    };
    db.client()
        .execute(
            &format!(
                "INSERT INTO users (id, email, status, signature_original_storage_key, \
                 signature_thumbnail_storage_key, created_at, updated_at, deleted_at) \
                 VALUES ($1, 'access@chalkak.test', '{status}', \
                 'chalkak/signatures/original/access.png', 'chalkak/signatures/thumbnail/access.png', \
                 CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, {deleted_at})"
            ),
            &[&user_id],
        )
        .unwrap();
    token_for(user_id)
}

fn call(
    h: &Arc<dyn Handler>,
    method: Method,
    path: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> Response {
    let mut req = Request::new(method, path);
    if let Some(t) = bearer {
        req.headers.append("Authorization", format!("Bearer {t}"));
    }
    if let Some(body) = body {
        req.headers.append("Content-Type", "application/json");
        req.body = body.as_bytes().to_vec();
    }
    h.handle(req)
}

fn json_body(r: &Response) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn update_body() -> String {
    format!(r#"{{"signatureOriginalUploadId":"{UPLOAD_ID}"}}"#)
}

/// `suspended()`.
fn assert_suspended(r: &Response) {
    assert_eq!(r.status, 403);
    let body = json_body(r);
    assert_eq!(body["errorCode"], "FORBIDDEN");
    assert_eq!(body["message"], "차단된 회원입니다.");
}

/// `notSuspended()`: 정지를 이유로 막히지 않았다는 사실만 본다.
fn assert_not_suspended(r: &Response) {
    assert_ne!(r.status, 403, "{}", String::from_utf8_lossy(&r.body));
}

/// `withdrawn()`.
fn assert_withdrawn(r: &Response) {
    assert_eq!(r.status, 401);
    assert_eq!(json_body(r)["errorCode"], "UNAUTHORIZED");
}

// ---------------------------------------------------------------------------------------------
// SuspendedUserAccessTest
// ---------------------------------------------------------------------------------------------

// SuspendedUserAccessTest#createSignatureUpload_suspendedUser_returnsForbidden
#[test]
fn create_signature_upload_suspended_user_returns_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "BANNED", false);
    let h = handler(&db);

    let r = call(
        &h,
        Method::Post,
        "/api/v1/users/me/signature/uploads",
        Some(&token),
        None,
    );

    assert_suspended(&r);
}

// SuspendedUserAccessTest#updateSignature_suspendedUser_returnsForbidden
#[test]
fn update_signature_suspended_user_returns_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "BANNED", false);
    let h = handler(&db);

    let r = call(
        &h,
        Method::Put,
        "/api/v1/users/me/signature",
        Some(&token),
        Some(&update_body()),
    );

    assert_suspended(&r);
}

// SuspendedUserAccessTest#withdraw_suspendedUser_isNotForbidden
#[test]
fn withdraw_suspended_user_is_not_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "BANNED", false);
    let h = handler(&db);

    let r = call(&h, Method::Delete, "/api/v1/users/me", Some(&token), None);

    assert_not_suspended(&r);
    assert_eq!(r.status, 204);
}

// SuspendedUserAccessTest#getSignature_suspendedUser_isNotForbidden
#[test]
fn get_signature_suspended_user_is_not_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "BANNED", false);
    let h = handler(&db);

    let r = call(
        &h,
        Method::Get,
        "/api/v1/users/me/signature",
        Some(&token),
        None,
    );

    assert_not_suspended(&r);
}

// ---------------------------------------------------------------------------------------------
// WithdrawnUserAccessTest
// ---------------------------------------------------------------------------------------------

// WithdrawnUserAccessTest#createSignatureUpload_withdrawnUser_returnsUnauthorized
#[test]
fn create_signature_upload_withdrawn_user_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "ACTIVE", true);
    let h = handler(&db);

    let r = call(
        &h,
        Method::Post,
        "/api/v1/users/me/signature/uploads",
        Some(&token),
        None,
    );

    assert_withdrawn(&r);
}

// WithdrawnUserAccessTest#updateSignature_withdrawnUser_returnsUnauthorized
#[test]
fn update_signature_withdrawn_user_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "ACTIVE", true);
    let h = handler(&db);

    let r = call(
        &h,
        Method::Put,
        "/api/v1/users/me/signature",
        Some(&token),
        Some(&update_body()),
    );

    assert_withdrawn(&r);
}

// WithdrawnUserAccessTest#withdraw_withdrawnUser_returnsUnauthorized
#[test]
fn withdraw_withdrawn_user_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "ACTIVE", true);
    let h = handler(&db);

    let r = call(&h, Method::Delete, "/api/v1/users/me", Some(&token), None);

    assert_withdrawn(&r);
}

// WithdrawnUserAccessTest#getSignature_withdrawnUser_returnsUnauthorized
#[test]
fn get_signature_withdrawn_user_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let token = insert_user(&db, "ACTIVE", true);
    let h = handler(&db);

    let r = call(
        &h,
        Method::Get,
        "/api/v1/users/me/signature",
        Some(&token),
        None,
    );

    assert_withdrawn(&r);
}

// ---------------------------------------------------------------------------------------------
// CallbackBodySizeLimitFilterTest (필터가 인증보다 먼저 도는지: 서명 헤더가 전혀 없어도 413)
// ---------------------------------------------------------------------------------------------

fn callback_post(h: &Arc<dyn Handler>, body: &str) -> Response {
    let mut req = Request::new(
        Method::Post,
        format!("/internal/v1/signature-processing/{UPLOAD_ID}/complete"),
    );
    req.headers.append("Content-Type", "application/json");
    req.headers.append("Content-Length", body.len().to_string());
    req.body = body.as_bytes().to_vec();
    h.handle(req)
}

// CallbackBodySizeLimitFilterTest#oversizedBody_rejectedBeforeAuthentication
#[test]
fn oversized_body_rejected_before_authentication() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let body = format!(r#"{{"capturedAt":"{}"}}"#, "A".repeat(70_000));

    let r = callback_post(&h, &body);

    assert_eq!(r.status, 413);
    assert!(
        String::from_utf8_lossy(&r.body).contains("이미지 처리 콜백 본문이 너무 큽니다."),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
}

// CallbackBodySizeLimitFilterTest#oversizedBody_keepsRequestIdHeader
#[test]
fn oversized_body_keeps_request_id_header() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let body = format!(r#"{{"capturedAt":"{}"}}"#, "A".repeat(70_000));

    let r = callback_post(&h, &body);

    assert_eq!(r.status, 413);
    assert!(r.headers.get("X-Request-Id").is_some());
}

// CallbackBodySizeLimitFilterTest#normalBody_reachesAuthentication
#[test]
fn normal_body_reaches_authentication() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);

    let r = callback_post(&h, r#"{"width":4032,"height":3024}"#);

    assert_ne!(r.status, 413);
    assert_eq!(r.status, 401, "서명 헤더가 없으면 인증에서 거절된다");
}
