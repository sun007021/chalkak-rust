//! 알림 수신 설정·푸시 기기 컨트롤러 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `NotificationSettingsControllerTest`, `PushDeviceControllerTest`(`@WebMvcTest`, 필터 없음)를 옮겼다.
//! 서비스를 Mockito로 바꾸는 대신 실제 서비스와 DB를 쓰고, 호출 여부(`verify`)는 DB 상태로 확인한다.

mod common;

use chalkak_app::security::{self, AUTHORITY_USER, Principal, PrincipalGuard};
use chalkak_http::{Handler, Method, Request, Response, Router};
use common::World;
use serde_json::Value;
use uuid::Uuid;

const SETTINGS: &str = "/api/v1/notification-settings";
const DEVICE: &str = "/api/v1/push-devices/current";
const UNAUTHORIZED_MESSAGE: &str = "유효하지 않은 인증 정보입니다.";
const UPDATE_REQUIRED: &str = "변경할 푸시 수신 설정을 하나 이상 입력해주세요.";

fn send(router: &Router, method: Method, path: &str, body: Option<&str>) -> Response {
    let mut req = Request::new(method, path);
    if let Some(body) = body {
        req.headers.append("Content-Type", "application/json");
        req.body = body.as_bytes().to_vec();
    }
    router.handle(req)
}

fn json_of(response: &Response) -> Value {
    serde_json::from_slice(&response.body).unwrap_or(Value::Null)
}

#[track_caller]
fn assert_error(response: &Response, status: u16, error_code: &str) -> Value {
    assert_eq!(
        response.status,
        status,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    let body = json_of(response);
    assert_eq!(body["errorCode"], error_code, "{body}");
    body
}

fn login_as(user_id: Uuid, session_id: Option<Uuid>) -> PrincipalGuard {
    security::enter_with_session(
        Principal {
            subject: user_id.to_string(),
            authorities: vec![AUTHORITY_USER.to_owned()],
        },
        session_id,
    )
}

fn device_count(w: &World) -> i64 {
    w.count("SELECT COUNT(*) FROM push_devices")
}

// NotificationSettingsControllerTest#getSettings_authenticatedUser_returnsSettings
#[test]
fn get_settings_authenticated_user_returns_settings() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.modify_user(user.id, |u| u.update_push_preferences(false, true).unwrap());
    let router = w.push_router();
    let _login = login_as(user.id, None);

    let response = send(&router, Method::Get, SETTINGS, None);

    assert_eq!(response.status, 200);
    assert!(
        response
            .headers
            .get("Content-Type")
            .is_some_and(|v| v.starts_with("application/json"))
    );
    let body = json_of(&response);
    assert_eq!(body["topicPushEnabled"], false);
    assert_eq!(body["moderationPushEnabled"], true);
    assert_eq!(body.as_object().unwrap().len(), 2);
}

// NotificationSettingsControllerTest#getSettings_invalidUser_returnsUnauthorized
#[test]
fn get_settings_invalid_user_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.push_router();
    let _login = login_as(Uuid::new_v4(), None);

    let response = send(&router, Method::Get, SETTINGS, None);

    let body = assert_error(&response, 401, "UNAUTHORIZED");
    assert_eq!(body["message"], UNAUTHORIZED_MESSAGE);
}

// NotificationSettingsControllerTest#updateSettings_validBody_returnsNoContent
#[test]
fn update_settings_valid_body_returns_no_content() {
    let cases = [
        (r#"{"topicPushEnabled":false}"#, (false, true)),
        (r#"{"moderationPushEnabled":false}"#, (true, false)),
        (
            r#"{"topicPushEnabled":null,"moderationPushEnabled":true}"#,
            (true, true),
        ),
        (
            r#"{"topicPushEnabled":true,"moderationPushEnabled":null}"#,
            (true, true),
        ),
        (
            r#"{"topicPushEnabled":false,"moderationPushEnabled":false}"#,
            (false, false),
        ),
    ];
    for (body, expected) in cases {
        let Some(w) = World::create() else { return };
        let user = w.save_user();
        let router = w.push_router();
        let _login = login_as(user.id, None);

        let response = send(&router, Method::Patch, SETTINGS, Some(body));

        assert_eq!(response.status, 204, "{body}");
        assert!(response.body.is_empty());
        let saved = w.find_user(user.id);
        assert_eq!(
            (saved.topic_push_enabled, saved.moderation_push_enabled),
            expected,
            "{body}"
        );
    }
}

// NotificationSettingsControllerTest#updateSettings_emptyUpdate_returnsBadRequest
#[test]
fn update_settings_empty_update_returns_bad_request() {
    for body in [
        "{}",
        r#"{"topicPushEnabled":null}"#,
        r#"{"moderationPushEnabled":null}"#,
        r#"{"topicPushEnabled":null,"moderationPushEnabled":null}"#,
    ] {
        let Some(w) = World::create() else { return };
        let user = w.save_user();
        let router = w.push_router();
        let _login = login_as(user.id, None);

        let response = send(&router, Method::Patch, SETTINGS, Some(body));

        let json = assert_error(&response, 400, "BUSINESS_ERROR");
        assert_eq!(json["message"], UPDATE_REQUIRED, "{body}");
    }
}

// NotificationSettingsControllerTest#updateSettings_unreadableBody_returnsBadRequest
#[test]
fn update_settings_unreadable_body_returns_bad_request() {
    for body in [
        r#"{"topicPushEnabled":{}}"#,
        r#"{"moderationPushEnabled":{}}"#,
        "",
    ] {
        let Some(w) = World::create() else { return };
        let user = w.save_user();
        let router = w.push_router();
        let _login = login_as(user.id, None);

        let response = send(&router, Method::Patch, SETTINGS, Some(body));

        assert_error(&response, 400, "BUSINESS_ERROR");
    }
}

// NotificationSettingsControllerTest#updateSettings_invalidUser_returnsUnauthorized
#[test]
fn update_settings_invalid_user_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.push_router();
    let _login = login_as(Uuid::new_v4(), None);

    let response = send(
        &router,
        Method::Patch,
        SETTINGS,
        Some(r#"{"topicPushEnabled":false}"#),
    );

    let body = assert_error(&response, 401, "UNAUTHORIZED");
    assert_eq!(body["message"], UNAUTHORIZED_MESSAGE);
}

// PushDeviceControllerTest#registerCurrentDevice_validRequest_returnsNoContent
#[test]
fn register_current_device_valid_request_returns_no_content() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let session = w
        .refresh
        .issue(&mut w.db.client(), user.id)
        .unwrap()
        .session_id;
    let router = w.push_router();
    let _login = login_as(user.id, Some(session));

    let response = send(
        &router,
        Method::Put,
        DEVICE,
        Some(r#"{"fcmToken":"fcm-token"}"#),
    );

    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    let token: String =
        w.db.client()
            .query_one(
                "SELECT fcm_token FROM push_devices WHERE session_id = $1 AND user_id = $2",
                &[&session, &user.id],
            )
            .unwrap()
            .get(0);
    assert_eq!(token, "fcm-token");
}

// PushDeviceControllerTest#registerCurrentDevice_invalidRequest_returnsBadRequest
#[test]
fn register_current_device_invalid_request_returns_bad_request() {
    for body in [
        "{}",
        r#"{"fcmToken":null}"#,
        r#"{"fcmToken":""}"#,
        r#"{"fcmToken":"   "}"#,
        "{",
        "",
    ] {
        let Some(w) = World::create() else { return };
        let user = w.save_user();
        let session = w
            .refresh
            .issue(&mut w.db.client(), user.id)
            .unwrap()
            .session_id;
        let router = w.push_router();
        let _login = login_as(user.id, Some(session));

        let response = send(&router, Method::Put, DEVICE, Some(body));

        assert_error(&response, 400, "BUSINESS_ERROR");
        assert_eq!(device_count(&w), 0, "{body}");
    }
}

// PushDeviceControllerTest#registerCurrentDevice_missingSession_returnsReauthenticationRequired
#[test]
fn register_current_device_missing_session_returns_reauthentication_required() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.push_router();
    let _login = login_as(user.id, None);

    let response = send(
        &router,
        Method::Put,
        DEVICE,
        Some(r#"{"fcmToken":"fcm-token"}"#),
    );

    let body = assert_error(&response, 401, "REAUTHENTICATION_REQUIRED");
    assert_eq!(body["message"], "다시 로그인해 주세요.");
    assert_eq!(device_count(&w), 0);
}

// PushDeviceControllerTest#registerCurrentDevice_withdrawnUser_returnsUnauthorized
#[test]
fn register_current_device_withdrawn_user_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let session = w
        .refresh
        .issue(&mut w.db.client(), user.id)
        .unwrap()
        .session_id;
    w.withdraw_directly(user.id);
    let router = w.push_router();
    let _login = login_as(user.id, Some(session));

    let response = send(
        &router,
        Method::Put,
        DEVICE,
        Some(r#"{"fcmToken":"fcm-token"}"#),
    );

    assert_error(&response, 401, "UNAUTHORIZED");
    assert_eq!(device_count(&w), 0);
}
