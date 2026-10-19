//! 알림 수신 설정·푸시 기기 API의 접근 정책을 실제 체인(인증, 인가)과 DB로 확인한다. `CHALKAK_TEST_DB_URL`이 없으면
//! 건너뛴다.
//!
//! 원본 `NotificationSettingsAccessTest`, `PushDeviceAccessTest`를 옮겼다.

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
const SETTINGS: &str = "/api/v1/notification-settings";
const DEVICE: &str = "/api/v1/push-devices/current";

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

/// 액세스 토큰. `session_id`가 `None`이면 로그인 ID 없는 이전 토큰이다.
fn token(subject: Uuid, scope: &str, session_id: Option<Uuid>) -> String {
    let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
    let mut claims = json!({
        "iss": "chalkak-backend", "aud": ["chalkak-access"], "sub": subject.to_string(),
        "iat": NOW_SECONDS - 5, "exp": NOW_SECONDS + 900, "jti": "j",
        "purpose": "ACCESS", "scope": scope,
    });
    if let Some(id) = session_id {
        claims["session_id"] = json!(id.to_string());
    }
    chalkak_jwt::hs256_encode(&json!({"typ":"JWT","alg":"HS256"}), &claims, &key)
}

fn insert_user(db: &TestDb, topic: bool, moderation: bool) -> Uuid {
    let id = Uuid::new_v4();
    db.client()
        .execute(
            "INSERT INTO users (id, email, signature_original_storage_key, topic_push_enabled, moderation_push_enabled) \
             VALUES ($1, $2, $3, $4, $5)",
            &[&id, &format!("{id}@chalkak.test"), &format!("chalkak/signatures/original/{id}.png"), &topic, &moderation],
        )
        .unwrap();
    id
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

fn error_code(r: &Response, status: u16) -> String {
    assert_eq!(r.status, status, "{}", String::from_utf8_lossy(&r.body));
    json_body(r)["errorCode"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// NotificationSettingsAccessTest#getSettings_unauthenticated_returnsUnauthorized
#[test]
fn get_settings_unauthenticated_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let r = call(&h, Method::Get, SETTINGS, None, None);
    assert_eq!(error_code(&r, 401), "UNAUTHORIZED");
}

// NotificationSettingsAccessTest#getSettings_adminToken_returnsForbidden
#[test]
fn get_settings_admin_token_returns_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let admin = token(Uuid::new_v4(), "ADMIN", None);
    let r = call(&h, Method::Get, SETTINGS, Some(&admin), None);
    assert_eq!(error_code(&r, 403), "FORBIDDEN");
}

// NotificationSettingsAccessTest#getSettings_legacyTokens_returnsEachUsersOwnSettings
#[test]
fn get_settings_legacy_tokens_return_each_users_own_settings() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let first = insert_user(&db, false, true);
    let second = insert_user(&db, true, false);
    for (user, topic, moderation) in [(first, false, true), (second, true, false)] {
        let r = call(
            &h,
            Method::Get,
            SETTINGS,
            Some(&token(user, "USER", None)),
            None,
        );
        assert_eq!(r.status, 200);
        let body = json_body(&r);
        assert_eq!(body["topicPushEnabled"], topic);
        assert_eq!(body["moderationPushEnabled"], moderation);
    }
}

// NotificationSettingsAccessTest#updateSettings_unauthenticated_returnsUnauthorized
#[test]
fn update_settings_unauthenticated_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let r = call(
        &h,
        Method::Patch,
        SETTINGS,
        None,
        Some(r#"{"topicPushEnabled":false}"#),
    );
    assert_eq!(error_code(&r, 401), "UNAUTHORIZED");
}

// NotificationSettingsAccessTest#updateSettings_adminToken_returnsForbidden
#[test]
fn update_settings_admin_token_returns_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let admin = token(Uuid::new_v4(), "ADMIN", None);
    let r = call(
        &h,
        Method::Patch,
        SETTINGS,
        Some(&admin),
        Some(r#"{"topicPushEnabled":false}"#),
    );
    assert_eq!(error_code(&r, 403), "FORBIDDEN");
}

// NotificationSettingsAccessTest#updateSettings_legacyToken_changesOnlyOwnAccount
#[test]
fn update_settings_legacy_token_changes_only_own_account() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let first = insert_user(&db, true, true);
    let second = insert_user(&db, true, true);
    let bearer = token(first, "USER", None);

    let r = call(
        &h,
        Method::Patch,
        SETTINGS,
        Some(&bearer),
        Some(r#"{"topicPushEnabled":false}"#),
    );
    assert_eq!(r.status, 204);

    let r = call(&h, Method::Get, SETTINGS, Some(&bearer), None);
    let body = json_body(&r);
    assert_eq!(
        (
            body["topicPushEnabled"].clone(),
            body["moderationPushEnabled"].clone()
        ),
        (json!(false), json!(true))
    );
    let unchanged: (bool, bool) = {
        let row = db
            .client()
            .query_one(
                "SELECT topic_push_enabled, moderation_push_enabled FROM users WHERE id = $1",
                &[&second],
            )
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!(unchanged, (true, true));
}

/// 로그인(`user_refresh_tokens` 한 줄)을 SQL로 만든다.
fn insert_login(db: &TestDb, user: Uuid) -> Uuid {
    let session = Uuid::new_v4();
    db.client()
        .execute(
            "INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, absolute_expires_at) \
             VALUES ($1, $2, $3, '2027-01-01T00:00:00Z', '2027-02-01T00:00:00Z')",
            &[&user, &session, &format!("{}{}", session.simple(), session.simple())],
        )
        .unwrap();
    session
}

fn device_token(db: &TestDb, session: Uuid) -> (Uuid, String) {
    let row = db
        .client()
        .query_one(
            "SELECT id, fcm_token FROM push_devices WHERE session_id = $1",
            &[&session],
        )
        .unwrap();
    (row.get(0), row.get(1))
}

// PushDeviceAccessTest#registerCurrentDevice_validLogin_registersAndUpdatesDevice
#[test]
fn register_current_device_valid_login_registers_and_updates_device() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let user = insert_user(&db, true, true);
    let session = insert_login(&db, user);
    let bearer = token(user, "USER", Some(session));

    let r = call(
        &h,
        Method::Put,
        DEVICE,
        Some(&bearer),
        Some(r#"{"fcmToken":"first-token"}"#),
    );
    assert_eq!(r.status, 204);
    let (device_id, _) = device_token(&db, session);
    let r = call(
        &h,
        Method::Put,
        DEVICE,
        Some(&bearer),
        Some(r#"{"fcmToken":"updated-token"}"#),
    );
    assert_eq!(r.status, 204);

    assert_eq!(
        device_token(&db, session),
        (device_id, "updated-token".to_owned())
    );
}

// PushDeviceAccessTest#registerCurrentDevice_unauthenticated_returnsUnauthorized
#[test]
fn register_current_device_unauthenticated_returns_unauthorized() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let r = call(
        &h,
        Method::Put,
        DEVICE,
        None,
        Some(r#"{"fcmToken":"fcm-token"}"#),
    );
    assert_eq!(error_code(&r, 401), "UNAUTHORIZED");
}

// PushDeviceAccessTest#registerCurrentDevice_adminToken_returnsForbidden
#[test]
fn register_current_device_admin_token_returns_forbidden() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let admin = token(Uuid::new_v4(), "ADMIN", None);
    let r = call(
        &h,
        Method::Put,
        DEVICE,
        Some(&admin),
        Some(r#"{"fcmToken":"fcm-token"}"#),
    );
    assert_eq!(error_code(&r, 403), "FORBIDDEN");
}

// PushDeviceAccessTest#registerCurrentDevice_legacyToken_preservesInboxAndRequiresReauthentication
#[test]
fn register_current_device_legacy_token_preserves_inbox_and_requires_reauthentication() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let user = insert_user(&db, true, true);
    let bearer = token(user, "USER", None);

    let r = call(
        &h,
        Method::Get,
        "/api/v1/notifications",
        Some(&bearer),
        None,
    );
    assert_eq!(r.status, 200);
    let r = call(
        &h,
        Method::Put,
        DEVICE,
        Some(&bearer),
        Some(r#"{"fcmToken":"fcm-token"}"#),
    );
    assert_eq!(error_code(&r, 401), "REAUTHENTICATION_REQUIRED");
    let count: i64 = db
        .client()
        .query_one("SELECT count(*) FROM push_devices", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}
