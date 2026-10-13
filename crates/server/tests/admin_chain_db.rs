//! 관리자 보안 체인을 실제 DB와 함께 확인한다. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `AdminSecurityFilterChainTest` 중 저장된 관리자로 로그인·재발급·로그아웃하는 흐름을 옮겼다. 토큰 없이 거절되는
//! 경계는 DB 없이 `security_chain.rs`가 확인한다.

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
/// `crates/app/src/admin/testing.rs`의 해시가 가리키는 비밀번호.
const PASSWORD: &str = "operator-password";
const PASSWORD_HASH: &str = "$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6";

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

fn insert_admin(db: &TestDb, username: &str) -> Uuid {
    db.client()
        .query_one(
            "INSERT INTO admins (username, password) VALUES ($1, $2) RETURNING id",
            &[&username, &PASSWORD_HASH],
        )
        .unwrap()
        .get(0)
}

fn call(
    h: &Arc<dyn Handler>,
    method: Method,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Response {
    let mut req = Request::new(method, path);
    if let Some(t) = bearer {
        req.headers.append("Authorization", format!("Bearer {t}"));
    }
    if let Some(body) = body {
        req.headers.append("Content-Type", "application/json");
        req.body = body.to_string().into_bytes();
    }
    h.handle(req)
}

fn json_body(r: &Response) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn login(h: &Arc<dyn Handler>, admin_id: Uuid, username: &str) -> Value {
    let r = call(
        h,
        Method::Post,
        "/api/v1/admin/auth/login",
        None,
        Some(json!({"username": username, "password": PASSWORD})),
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let body = json_body(&r);
    assert_eq!(body["adminId"], admin_id.to_string());
    assert!(body.get("password").is_none());
    assert!(body.get("passwordHash").is_none());
    body
}

// AdminSecurityFilterChainTest#login_withoutToken_reachesController
#[test]
fn login_without_token_reaches_controller() {
    let Some(db) = TestDb::create() else { return };
    insert_admin(&db, "operator");
    let h = handler(&db);

    let r = call(
        &h,
        Method::Post,
        "/api/v1/admin/auth/login",
        None,
        Some(json!({"username": "operator", "password": "wrong-password"})),
    );

    assert_eq!(r.status, 401);
    assert_eq!(
        json_body(&r)["message"],
        "아이디 또는 비밀번호가 올바르지 않습니다."
    );
}

// AdminSecurityFilterChainTest#adminApi_adminToken_injectsCurrentAdmin
// (원본은 시험용 컨트롤러가 주입된 관리자를 그대로 돌려준다. 여기서는 /me가 토큰의 관리자를 읽는다.)
#[test]
fn admin_api_admin_token_injects_current_admin() {
    let Some(db) = TestDb::create() else { return };
    let admin_id = insert_admin(&db, "operator");
    let h = handler(&db);
    let token = login(&h, admin_id, "operator")["accessToken"]
        .as_str()
        .unwrap()
        .to_owned();

    let r = call(&h, Method::Get, "/api/v1/admin/auth/me", Some(&token), None);

    assert_eq!(r.status, 200);
    assert_eq!(json_body(&r)["adminId"], admin_id.to_string());
}

// AdminSecurityFilterChainTest#login_persistedAdmin_accessesProtectedApis
#[test]
fn login_persisted_admin_accesses_protected_apis() {
    let Some(db) = TestDb::create() else { return };
    let admin_id = insert_admin(&db, "security-operator");
    let h = handler(&db);
    let token = login(&h, admin_id, "security-operator")["accessToken"]
        .as_str()
        .unwrap()
        .to_owned();

    let me = call(&h, Method::Get, "/api/v1/admin/auth/me", Some(&token), None);
    assert_eq!(me.status, 200);
    assert_eq!(json_body(&me)["username"], "security-operator");
    let audit = call(
        &h,
        Method::Get,
        "/api/v1/admin/audit-logs",
        Some(&token),
        None,
    );
    assert_eq!(audit.status, 200);
}

// AdminSecurityFilterChainTest#refreshAndLogout_withoutAccessToken_reachController
#[test]
fn refresh_and_logout_without_access_token_reach_controller() {
    let Some(db) = TestDb::create() else { return };
    let admin_id = insert_admin(&db, "refresh-security-operator");
    let h = handler(&db);
    let refresh_token = login(&h, admin_id, "refresh-security-operator")["refreshToken"]
        .as_str()
        .unwrap()
        .to_owned();

    let refreshed = call(
        &h,
        Method::Post,
        "/api/v1/admin/auth/refresh",
        None,
        Some(json!({"refreshToken": refresh_token})),
    );
    assert_eq!(refreshed.status, 200);
    let rotated = json_body(&refreshed)["refreshToken"]
        .as_str()
        .unwrap()
        .to_owned();

    let logout = call(
        &h,
        Method::Post,
        "/api/v1/admin/auth/logout",
        None,
        Some(json!({"refreshToken": rotated})),
    );
    assert_eq!(logout.status, 204);

    let again = call(
        &h,
        Method::Post,
        "/api/v1/admin/auth/refresh",
        None,
        Some(json!({"refreshToken": rotated})),
    );
    assert_eq!(again.status, 401);
    assert_eq!(json_body(&again)["errorCode"], "REAUTHENTICATION_REQUIRED");
}

// AdminSecurityFilterChainTest#refresh_unknownRefreshToken_returnsReauthenticationRequired
#[test]
fn refresh_unknown_refresh_token_returns_reauthentication_required() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);

    let r = call(
        &h,
        Method::Post,
        "/api/v1/admin/auth/refresh",
        None,
        Some(json!({"refreshToken": "unknown-admin-refresh-token"})),
    );

    assert_eq!(r.status, 401);
    assert_eq!(json_body(&r)["errorCode"], "REAUTHENTICATION_REQUIRED");
}
