//! 서버가 내는 OpenAPI 문서(`/v3/api-docs/user-api`)가 원본 계약 테스트의 기대를 그대로 만족하는지 본다.
//!
//! 원본 `AuthOpenApiTest`, `FeedbackOpenApiTest`, `NotificationOpenApiTest`, `PostOpenApiTest`와 관리자 문서 테스트
//! (`AdminAuditLogOpenApiTest`, `AdminAuthOpenApiTest`, `AdminFeedbackOpenApiTest`, `AdminPostOpenApiTest`,
//! `AdminTopicOpenApiTest`, `AdminUserOpenApiTest`)는 MockMvc로 springdoc 문서를
//! 받아 JSON 경로를 확인한다. 이 서버는 원본이 만든 문서를 그대로 싣고(`crates/server/assets/springdoc`), 같은 경로로
//! 낸다. 아래 테스트는 같은 JSON 경로를 같은 값으로 확인한다. 문서 바이트 전체의 동치는 하네스 시나리오
//! `platform/springdoc`가 원본 서버와 비교해 고정한다.

use std::sync::Arc;

use chalkak_app::web::Routes;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock};
use chalkak_http::{Chain, Handler, Method, Request};
use chalkak_server::build_chain;
use chalkak_server::settings::Settings;
use serde_json::Value;

const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn user_api() -> Value {
    docs("user-api")
}

fn admin_api() -> Value {
    docs("admin-api")
}

/// 서버가 내는 그룹별 문서.
fn docs(group: &str) -> Value {
    let mut pairs = vec![
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false"),
    ];
    pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
    let settings = Settings::from_env(&Env::from_pairs(pairs)).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
    let logger = Arc::new(Logger::new(
        Box::new(std::io::sink()),
        Level::Info,
        Arc::clone(&clock),
    ));
    let router = Routes::new(Arc::clone(&logger)).into_router();
    let chain: Chain = build_chain(&settings, router, Arc::new(|| true), clock, logger);
    let mut req = Request::new(Method::Get, format!("/v3/api-docs/{group}"));
    req.headers.append("Host", "127.0.0.1:18082");
    let response = chain.handle(req);
    assert_eq!(response.status, 200);
    serde_json::from_slice(&response.body).expect("문서는 JSON이다")
}

/// `jsonPath("$.a['b'].c")`: 키를 차례로 따라간다(없으면 `None`).
fn at<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |value, key| value.get(*key))
}

fn exists(doc: &Value, path: &[&str]) -> bool {
    at(doc, path).is_some()
}

fn text<'a>(doc: &'a Value, path: &[&str]) -> &'a str {
    at(doc, path)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("문자열이 없다: {path:?}"))
}

/// `containsString`.
fn assert_contains(doc: &Value, path: &[&str], expected: &str) {
    let actual = text(doc, path);
    assert!(
        actual.contains(expected),
        "{path:?}: {actual:?}에 {expected:?}가 없다"
    );
}

/// `hasItems`/`containsInAnyOrder`에서 쓰는 문자열 배열(순서 무시).
fn strings(doc: &Value, path: &[&str]) -> Vec<String> {
    let mut items: Vec<String> = at(doc, path)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("배열이 없다: {path:?}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    items.sort();
    items
}

fn assert_any_order(doc: &Value, path: &[&str], expected: &[&str]) {
    let mut expected: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(strings(doc, path), expected, "{path:?}");
}

fn size(doc: &Value, path: &[&str]) -> usize {
    at(doc, path)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("객체가 없다: {path:?}"))
        .len()
}

/// 작업 모음: `paths['/x'].post.responses['200']`처럼 경로 앞부분이 같은 경우.
fn operation<'a>(doc: &'a Value, path: &str, method: &str) -> &'a Value {
    at(doc, &["paths", path, method]).unwrap_or_else(|| panic!("{method} {path}가 없다"))
}

/// `parameters[?(@.name == 'postId')].schema.format`.
fn path_param_format<'a>(operation: &'a Value, name: &str) -> Option<&'a str> {
    operation
        .get("parameters")?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))?
        .get("schema")?
        .get("format")?
        .as_str()
}

fn has_access_token_security(operation: &Value) -> bool {
    operation
        .get("security")
        .and_then(|s| s.get(0))
        .is_some_and(|first| first.get("accessToken").is_some())
}

// ---------------------------------------------------------------------------------------------
// AuthOpenApiTest
// ---------------------------------------------------------------------------------------------

// AuthOpenApiTest#userApiDocs_socialLogin_exposesBannedUserContract
#[test]
fn user_api_docs_social_login_exposes_banned_user_contract() {
    let doc = user_api();
    let post = ["paths", "/api/v1/auth/social-login", "post", "responses"];
    assert_eq!(
        text(
            &doc,
            &[post[0], post[1], post[2], post[3], "200", "description"]
        ),
        "로그인 성공(차단 회원 포함) 또는 회원가입 필요"
    );
    assert_eq!(
        text(
            &doc,
            &[post[0], post[1], post[2], post[3], "403", "description"]
        ),
        "탈퇴한 차단 소셜 계정"
    );
}

// AuthOpenApiTest#userApiDocs_socialIdTokenRequests_exposeRawNonceContract
#[test]
fn user_api_docs_social_id_token_requests_expose_raw_nonce_contract() {
    let doc = user_api();
    for schema in ["SocialLoginRequest", "SocialSignupSignatureUploadRequest"] {
        let required = strings(&doc, &["components", "schemas", schema, "required"]);
        for field in ["provider", "idToken", "rawNonce"] {
            assert!(required.contains(&field.to_owned()), "{schema}.{field}");
        }
        assert_contains(
            &doc,
            &[
                "components",
                "schemas",
                schema,
                "properties",
                "rawNonce",
                "description",
            ],
            "SHA-256 소문자 hex",
        );
    }
    for path in [
        "/api/v1/auth/social-login",
        "/api/v1/auth/social-signup/signature/uploads",
    ] {
        assert_contains(
            &doc,
            &["paths", path, "post", "responses", "401", "description"],
            "nonce",
        );
    }
}

// AuthOpenApiTest#userApiDocs_socialLoginRequest_exposesAuthorizationCodeContract
#[test]
fn user_api_docs_social_login_request_exposes_authorization_code_contract() {
    let doc = user_api();
    assert!(exists(
        &doc,
        &[
            "components",
            "schemas",
            "SocialLoginRequest",
            "properties",
            "authorizationCode"
        ]
    ));
    assert!(
        !strings(
            &doc,
            &["components", "schemas", "SocialLoginRequest", "required"]
        )
        .contains(&"authorizationCode".to_owned())
    );
    assert!(!exists(
        &doc,
        &[
            "components",
            "schemas",
            "SocialSignupSignatureUploadRequest",
            "properties",
            "authorizationCode"
        ]
    ));
}

// AuthOpenApiTest#userApiDocs_signatureUpload_exposesApplePendingAuthorizationContract
#[test]
fn user_api_docs_signature_upload_exposes_apple_pending_authorization_contract() {
    let doc = user_api();
    assert_contains(
        &doc,
        &[
            "paths",
            "/api/v1/auth/social-signup/signature/uploads",
            "post",
            "responses",
            "400",
            "description",
        ],
        "Apple 임시 인증 정보",
    );
}

// AuthOpenApiTest#userApiDocs_signatureUpload_exposesUploadUrlContract
#[test]
fn user_api_docs_signature_upload_exposes_upload_url_contract() {
    let doc = user_api();
    let properties = [
        "components",
        "schemas",
        "SocialSignupSignatureUploadResponse",
        "properties",
    ];
    let upload_url = [
        properties[0],
        properties[1],
        properties[2],
        properties[3],
        "uploadUrl",
        "description",
    ];
    for expected in ["S3 Presigned PUT URL", "image/png", "1 MiB"] {
        assert_contains(&doc, &upload_url, expected);
    }
    assert_contains(
        &doc,
        &[
            properties[0],
            properties[1],
            properties[2],
            properties[3],
            "expiresInSeconds",
            "description",
        ],
        "signupToken의 만료 시간이 아님",
    );
    assert_eq!(
        at(
            &doc,
            &[
                properties[0],
                properties[1],
                properties[2],
                properties[3],
                "expiresInSeconds",
                "example"
            ]
        ),
        Some(&Value::from(300))
    );
}

// AuthOpenApiTest#userApiDocs_socialSignup_exposesSignatureProcessingPendingErrorCode
#[test]
fn user_api_docs_social_signup_exposes_signature_processing_pending_error_code() {
    let doc = user_api();
    assert_contains(
        &doc,
        &[
            "paths",
            "/api/v1/auth/social-signup",
            "post",
            "responses",
            "400",
            "description",
        ],
        "SIGNATURE_PROCESSING_PENDING",
    );
}

// AuthOpenApiTest#userApiDocs_appleEndpoints_exposeNoDedicatedContract
#[test]
fn user_api_docs_apple_endpoints_expose_no_dedicated_contract() {
    let doc = user_api();
    assert!(!exists(
        &doc,
        &[
            "paths",
            "/api/v1/auth/apple/social-signup/signature/uploads"
        ]
    ));
    assert!(!exists(&doc, &["paths", "/api/v1/auth/apple/social-login"]));
    assert!(exists(
        &doc,
        &[
            "paths",
            "/api/v1/auth/social-signup/signature/uploads",
            "post"
        ]
    ));
    assert!(exists(
        &doc,
        &["paths", "/api/v1/auth/social-login", "post"]
    ));
    assert!(exists(
        &doc,
        &[
            "components",
            "schemas",
            "SocialLoginResponse",
            "properties",
            "status"
        ]
    ));
    assert!(!exists(
        &doc,
        &[
            "components",
            "schemas",
            "SocialLoginResponse",
            "properties",
            "signupToken"
        ]
    ));
}

// AuthOpenApiTest#userApiDocs_refresh_exposesUnauthenticatedContract
#[test]
fn user_api_docs_refresh_exposes_unauthenticated_contract() {
    let doc = user_api();
    let post = operation(&doc, "/api/v1/auth/refresh", "post");
    assert!(post.get("security").is_none());
    for status in ["200", "400", "401"] {
        assert!(post["responses"].get(status).is_some(), "{status}");
    }
}

// AuthOpenApiTest#userApiDocs_logout_exposesUnauthenticatedContract
#[test]
fn user_api_docs_logout_exposes_unauthenticated_contract() {
    let doc = user_api();
    let post = operation(&doc, "/api/v1/auth/logout", "post");
    assert!(post.get("security").is_none());
    for status in ["204", "400"] {
        assert!(post["responses"].get(status).is_some(), "{status}");
    }
}

// ---------------------------------------------------------------------------------------------
// FeedbackOpenApiTest
// ---------------------------------------------------------------------------------------------

// FeedbackOpenApiTest#userApiDocs_submitFeedback_exposesContract
#[test]
fn user_api_docs_submit_feedback_exposes_contract() {
    let doc = user_api();
    let post = operation(&doc, "/api/v1/feedbacks", "post");
    assert!(has_access_token_security(post));
    assert!(
        post["requestBody"]["content"]["application/json"]["schema"]
            .get("$ref")
            .is_some()
    );
    for status in ["201", "400", "401"] {
        assert!(post["responses"].get(status).is_some(), "{status}");
    }
    assert!(post["responses"].get("403").is_none());
    assert_eq!(
        at(
            &doc,
            &[
                "components",
                "schemas",
                "FeedbackSubmissionRequest",
                "properties",
                "content",
                "maxLength"
            ]
        ),
        Some(&Value::from(1000))
    );
}

// FeedbackOpenApiTest#userApiDocs_feedback_exposesSubmissionOnly
#[test]
fn user_api_docs_feedback_exposes_submission_only() {
    let doc = user_api();
    for method in ["get", "put", "delete"] {
        assert!(
            !exists(&doc, &["paths", "/api/v1/feedbacks", method]),
            "{method}"
        );
    }
    assert!(!exists(&doc, &["paths", "/api/v1/feedbacks/me"]));
}

// ---------------------------------------------------------------------------------------------
// NotificationOpenApiTest
// ---------------------------------------------------------------------------------------------

// NotificationOpenApiTest#userApiDocs_exposesNotificationInboxContract
#[test]
fn user_api_docs_exposes_notification_inbox_contract() {
    let doc = user_api();
    let response = |path: &str, method: &str, status: &str| {
        exists(&doc, &["paths", path, method, "responses", status])
    };
    assert!(response("/api/v1/notifications", "get", "200"));
    assert!(response(
        "/api/v1/notifications/{notificationId}",
        "get",
        "404"
    ));
    assert!(response(
        "/api/v1/notifications/{notificationId}/read",
        "patch",
        "204"
    ));
    assert!(response("/api/v1/notifications/read-all", "patch", "204"));
    assert!(response(
        "/api/v1/notifications/unread-status",
        "get",
        "200"
    ));
    assert!(has_access_token_security(operation(
        &doc,
        "/api/v1/notifications",
        "get"
    )));
}

// ---------------------------------------------------------------------------------------------
// NotificationSettingsOpenApiTest, PushDeviceOpenApiTest
// ---------------------------------------------------------------------------------------------

fn schema_ref<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut full = path.to_vec();
    full.push("$ref");
    at(doc, &full).and_then(Value::as_str)
}

// NotificationSettingsOpenApiTest#userApiDocs_exposesSettingsQueryContract
#[test]
fn user_api_docs_exposes_settings_query_contract() {
    let doc = user_api();
    let op = ["paths", "/api/v1/notification-settings", "get"];
    let sub =
        |rest: &[&str]| -> Vec<String> { op.iter().chain(rest).map(|s| (*s).to_owned()).collect() };
    let get = |rest: &[&str]| {
        let owned = sub(rest);
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        at(&doc, &refs).cloned()
    };
    assert!(get(&["security"]).is_some_and(|s| s[0].get("accessToken").is_some()));
    assert!(get(&["parameters"]).is_none());
    assert!(get(&["requestBody"]).is_none());
    assert_eq!(get(&["responses"]).unwrap().as_object().unwrap().len(), 3);
    let ok = get(&["responses", "200", "content", "*/*", "schema"]).unwrap();
    assert_eq!(
        ok["$ref"],
        "#/components/schemas/NotificationSettingsResponse"
    );
    let unauthorized = get(&["responses", "401", "content", "application/json", "schema"]).unwrap();
    assert_eq!(unauthorized["$ref"], "#/components/schemas/ErrorResponse");
    assert!(get(&["responses", "403"]).is_some());
    let props = at(
        &doc,
        &[
            "components",
            "schemas",
            "NotificationSettingsResponse",
            "properties",
        ],
    )
    .unwrap();
    assert_eq!(props.as_object().unwrap().len(), 2);
    assert_eq!(props["topicPushEnabled"]["type"], "boolean");
    assert_eq!(props["moderationPushEnabled"]["type"], "boolean");
}

// NotificationSettingsOpenApiTest#userApiDocs_exposesSettingsUpdateContract
#[test]
fn user_api_docs_exposes_settings_update_contract() {
    let doc = user_api();
    let patch = at(&doc, &["paths", "/api/v1/notification-settings", "patch"]).unwrap();
    assert!(patch["security"][0].get("accessToken").is_some());
    assert!(patch.get("parameters").is_none());
    assert_eq!(patch["requestBody"]["required"], true);
    assert_eq!(
        patch["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/NotificationSettingsUpdateRequest"
    );
    assert_eq!(patch["responses"].as_object().unwrap().len(), 4);
    assert!(patch["responses"]["204"].get("content").is_none());
    assert_eq!(
        patch["responses"]["400"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ErrorResponse"
    );
    assert!(patch["responses"].get("401").is_some());
    assert!(patch["responses"].get("403").is_some());
    let request = at(
        &doc,
        &["components", "schemas", "NotificationSettingsUpdateRequest"],
    )
    .unwrap();
    assert!(request.get("required").is_none());
    assert_eq!(request["properties"].as_object().unwrap().len(), 2);
    for name in ["topicPushEnabled", "moderationPushEnabled"] {
        let mut types: Vec<&str> = request["properties"][name]["type"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        types.sort_unstable();
        assert_eq!(types, ["boolean", "null"], "{name}");
    }
    assert_eq!(
        schema_ref(
            &doc,
            &[
                "paths",
                "/api/v1/notification-settings",
                "patch",
                "requestBody",
                "content",
                "application/json",
                "schema"
            ]
        ),
        Some("#/components/schemas/NotificationSettingsUpdateRequest")
    );
}

// PushDeviceOpenApiTest#userApiDocs_exposesCurrentDeviceContract
#[test]
fn user_api_docs_exposes_current_device_contract() {
    let doc = user_api();
    let put = at(&doc, &["paths", "/api/v1/push-devices/current", "put"]).unwrap();
    assert!(put["security"][0].get("accessToken").is_some());
    assert!(put.get("parameters").is_none());
    assert_eq!(put["requestBody"]["required"], true);
    assert_eq!(
        put["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/PushDeviceRegistrationRequest"
    );
    let request = at(
        &doc,
        &["components", "schemas", "PushDeviceRegistrationRequest"],
    )
    .unwrap();
    assert_eq!(request["required"][0], "fcmToken");
    assert_eq!(request["properties"]["fcmToken"]["type"], "string");
    assert!(request["properties"].get("sessionId").is_none());
    assert!(request["properties"].get("userId").is_none());
    assert_eq!(put["responses"].as_object().unwrap().len(), 4);
    assert!(put["responses"]["204"].get("content").is_none());
    assert!(put["responses"].get("400").is_some());
    assert_eq!(
        put["responses"]["401"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ErrorResponse"
    );
    assert!(put["responses"].get("403").is_some());
}

// ---------------------------------------------------------------------------------------------
// PostOpenApiTest
// ---------------------------------------------------------------------------------------------

// PostOpenApiTest#userApiDocs_getMyPostCalendar_exposesPendingAndApprovedStatuses
#[test]
fn user_api_docs_get_my_post_calendar_exposes_pending_and_approved_statuses() {
    let doc = user_api();
    assert!(exists(&doc, &["paths", "/api/v1/posts/calendar", "get"]));
    let properties = [
        "components",
        "schemas",
        "CalendarPostResponse",
        "properties",
    ];
    assert_eq!(size(&doc, &properties), 4);
    for field in ["topicDate", "postId", "thumbnailImageUrl"] {
        assert!(
            exists(
                &doc,
                &[
                    properties[0],
                    properties[1],
                    properties[2],
                    properties[3],
                    field
                ]
            ),
            "{field}"
        );
    }
    assert_any_order(
        &doc,
        &[
            properties[0],
            properties[1],
            properties[2],
            properties[3],
            "status",
            "enum",
        ],
        &["PENDING", "APPROVED"],
    );
}

// PostOpenApiTest#userApiDocs_deletePost_exposesContract
#[test]
fn user_api_docs_delete_post_exposes_contract() {
    let doc = user_api();
    let delete = operation(&doc, "/api/v1/posts/{postId}", "delete");
    assert_eq!(path_param_format(delete, "postId"), Some("uuid"));
    assert!(has_access_token_security(delete));
    assert!(delete.get("requestBody").is_none());
    for status in ["204", "400", "401", "403", "404"] {
        assert!(delete["responses"].get(status).is_some(), "{status}");
    }
}

// PostOpenApiTest#userApiDocs_updatePost_exposesContract
#[test]
fn user_api_docs_update_post_exposes_contract() {
    let doc = user_api();
    let put = operation(&doc, "/api/v1/posts/{postId}", "put");
    assert_eq!(path_param_format(put, "postId"), Some("uuid"));
    assert!(has_access_token_security(put));
    assert_eq!(put["requestBody"]["required"], true);
    assert_eq!(
        put["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/PostUpdateRequest"
    );
    assert_any_order(
        &doc,
        &["components", "schemas", "PostUpdateRequest", "required"],
        &["title"],
    );
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "PostUpdateRequest",
            "properties",
            "title",
            "type",
        ],
        &["string", "null"],
    );
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "PostUpdateResponse",
            "properties",
            "title",
            "type",
        ],
        &["string", "null"],
    );
    assert!(!exists(
        &doc,
        &[
            "components",
            "schemas",
            "PostUpdateResponse",
            "properties",
            "moderationStatus"
        ]
    ));
    for status in ["200", "400", "401", "403", "404"] {
        assert!(put["responses"].get(status).is_some(), "{status}");
    }
}

// PostOpenApiTest#userApiDocs_getMyTodayPostStatus_exposesContract
#[test]
fn user_api_docs_get_my_today_post_status_exposes_contract() {
    let doc = user_api();
    let get = operation(&doc, "/api/v1/posts/today", "get");
    assert!(get.get("parameters").is_none());
    assert!(has_access_token_security(get));
    let properties = [
        "components",
        "schemas",
        "TodayPostStatusResponse",
        "properties",
    ];
    let field = |name: &'static str, leaf: &'static str| {
        [
            properties[0],
            properties[1],
            properties[2],
            properties[3],
            name,
            leaf,
        ]
    };
    assert_eq!(text(&doc, &field("isPosted", "type")), "boolean");
    assert!(!exists(
        &doc,
        &[
            properties[0],
            properties[1],
            properties[2],
            properties[3],
            "posted"
        ]
    ));
    assert_any_order(&doc, &field("postId", "type"), &["string", "null"]);
    assert_any_order(
        &doc,
        &field("moderationStatus", "type"),
        &["string", "null"],
    );
    assert_any_order(
        &doc,
        &field("moderationStatus", "enum"),
        &["VALIDATING", "PENDING", "APPROVED"],
    );
    for status in ["200", "401", "403", "404"] {
        assert!(get["responses"].get(status).is_some(), "{status}");
    }
}

// PostOpenApiTest#userApiDocs_postListEndpoint_declaresOptionalAuth
#[test]
fn user_api_docs_post_list_endpoint_declares_optional_auth() {
    let doc = user_api();
    let security = operation(&doc, "/api/v1/posts", "get")["security"]
        .as_array()
        .expect("security 배열");
    assert_eq!(security.len(), 2);
    assert_eq!(security[0].as_object().map(|o| o.len()), Some(0));
    assert_eq!(security[1].as_object().map(|o| o.len()), Some(1));
    assert!(security[1].get("accessToken").is_some());
}

// PostOpenApiTest#userApiDocs_getMyPostCalendarMonths_exposesMonthsAndSecurity
#[test]
fn user_api_docs_get_my_post_calendar_months_exposes_months_and_security() {
    let doc = user_api();
    let get = operation(&doc, "/api/v1/posts/calendar/months", "get");
    assert!(get.get("parameters").is_none());
    assert!(has_access_token_security(get));
    for status in ["200", "401", "403"] {
        assert!(get["responses"].get(status).is_some(), "{status}");
    }
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["PostCalendarMonthsResponse"]["properties"]
            .as_object()
            .map(|o| o.len()),
        Some(1)
    );
    let months = &schemas["PostCalendarMonthsResponse"]["properties"]["months"];
    assert_eq!(months["type"], "array");
    assert_eq!(
        months["items"]["$ref"],
        "#/components/schemas/CalendarMonthResponse"
    );
    let month = &schemas["CalendarMonthResponse"]["properties"];
    assert_eq!(month.as_object().map(|o| o.len()), Some(2));
    assert_eq!(month["year"]["type"], "integer");
    assert_eq!(month["month"]["type"], "integer");
}

// ---------------------------------------------------------------------------------------------
// 관리자 문서(admin-api)
// ---------------------------------------------------------------------------------------------

/// `parameters[*].name`.
fn param_names(operation: &Value) -> Vec<String> {
    let mut names: Vec<String> = operation
        .get("parameters")
        .and_then(Value::as_array)
        .map(|params| {
            params
                .iter()
                .map(|p| p["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn sorted(items: &[&str]) -> Vec<String> {
    let mut items: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
    items.sort();
    items
}

/// `parameters[?(@.name == 'status')].schema.enum`.
fn param_enum(operation: &Value, name: &str) -> Vec<String> {
    let mut values: Vec<String> = operation["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .and_then(|p| p["schema"]["enum"].as_array())
        .unwrap_or_else(|| panic!("{name}의 enum이 없다"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    values.sort();
    values
}

fn has_response(operation: &Value, status: &str) -> bool {
    operation["responses"].get(status).is_some()
}

/// 객체이고 비어 있지 않다(`properties[*]`가 있다).
fn has_properties(schema: &Value) -> bool {
    schema["properties"]
        .as_object()
        .is_some_and(|p| !p.is_empty())
}

// AdminAuditLogOpenApiTest#adminApiDocs_exposesReadOnlyAuditLogContract
#[test]
fn admin_api_docs_exposes_read_only_audit_log_contract() {
    let admin = admin_api();
    let path = &admin["paths"]["/api/v1/admin/audit-logs"];
    assert_eq!(
        param_names(&path["get"]),
        sorted(&[
            "adminId",
            "action",
            "targetType",
            "targetId",
            "occurredFrom",
            "occurredTo",
            "sort",
            "page",
            "pageSize"
        ])
    );
    assert!(path.get("put").is_none());
    assert!(path.get("delete").is_none());
    for status in ["200", "400", "403"] {
        assert!(has_response(&path["get"], status), "{status}");
    }
    assert!(has_properties(
        &admin["components"]["schemas"]["AuditLogResponse"]
    ));

    assert!(!exists(&user_api(), &["paths", "/api/v1/admin/audit-logs"]));
}

// AdminAuthOpenApiTest#adminApiDocs_exposesAdminSecurityAndRefreshTokenExceptions
#[test]
fn admin_api_docs_exposes_admin_security_and_refresh_token_exceptions() {
    let doc = admin_api();
    let scheme = &doc["components"]["securitySchemes"]["adminAccessToken"];
    assert_eq!(scheme["type"], "http");
    assert_eq!(scheme["scheme"], "bearer");
    for path in [
        "/api/v1/admin/auth/login",
        "/api/v1/admin/auth/refresh",
        "/api/v1/admin/auth/logout",
    ] {
        let security = &doc["paths"][path]["post"]["security"];
        assert_eq!(security.as_array().map(Vec::len), Some(0), "{path}");
    }
    for path in ["/api/v1/admin/auth/me", "/api/v1/admin/posts"] {
        assert!(
            doc["paths"][path]["get"]["security"][0]["adminAccessToken"].is_array(),
            "{path}"
        );
    }
    assert!(
        doc["paths"]["/api/v1/admin/auth/me"]["get"]
            .get("parameters")
            .is_none()
    );
    assert!(
        doc["paths"]["/api/v1/admin/auth/logout"]["post"]
            .get("parameters")
            .is_none()
    );
    let error_ref = "#/components/schemas/ErrorResponse";
    let schema_of = |path: &str, status: &str| {
        doc["paths"][path]["get"]["responses"][status]["content"]["application/json"]["schema"]
            ["$ref"]
            .clone()
    };
    assert_eq!(schema_of("/api/v1/admin/audit-logs", "401"), error_ref);
    assert_eq!(schema_of("/api/v1/admin/auth/me", "403"), error_ref);
}

// AdminAuthOpenApiTest#adminApiDocs_documentsRefreshFailureAndKeepsLogoutIdempotent
#[test]
fn admin_api_docs_documents_refresh_failure_and_keeps_logout_idempotent() {
    let doc = admin_api();
    let refresh_401 = &doc["paths"]["/api/v1/admin/auth/refresh"]["post"]["responses"]["401"];
    assert_eq!(
        refresh_401["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ErrorResponse"
    );
    assert!(
        refresh_401["description"]
            .as_str()
            .unwrap()
            .contains("REAUTHENTICATION_REQUIRED")
    );
    let logout = &doc["paths"]["/api/v1/admin/auth/logout"]["post"]["responses"];
    assert!(logout.get("401").is_none());
    assert!(logout.get("204").is_some());
}

// AdminFeedbackOpenApiTest#adminApiDocs_exposesReadOnlyFeedbackContract
#[test]
fn admin_api_docs_exposes_read_only_feedback_contract() {
    let doc = admin_api();
    let path = &doc["paths"]["/api/v1/admin/feedbacks"];
    assert_eq!(
        param_names(&path["get"]),
        sorted(&["sort", "page", "pageSize"])
    );
    for method in ["post", "patch", "delete"] {
        assert!(path.get(method).is_none(), "{method}");
    }
    for status in ["200", "400", "403"] {
        assert!(has_response(&path["get"], status), "{status}");
    }
    for schema in ["AdminFeedbackListItem", "AdminFeedbackListAuthor"] {
        assert!(
            has_properties(&doc["components"]["schemas"][schema]),
            "{schema}"
        );
    }
}

// AdminFeedbackOpenApiTest#userApiDocs_hidesAdminFeedbackPath
#[test]
fn user_api_docs_hides_admin_feedback_path() {
    assert!(!exists(&user_api(), &["paths", "/api/v1/admin/feedbacks"]));
}

// AdminPostOpenApiTest#adminApiDocs_adminPostEndpoints_exposeDistinctListAndDetailSchemas
#[test]
fn admin_api_docs_admin_post_endpoints_expose_distinct_list_and_detail_schemas() {
    let doc = admin_api();
    let list = &doc["paths"]["/api/v1/admin/posts"]["get"];
    assert_eq!(
        param_names(list),
        sorted(&[
            "status",
            "topicId",
            "topicDate",
            "userId",
            "createdAtFrom",
            "createdAtTo",
            "sort",
            "page",
            "pageSize"
        ])
    );
    assert_eq!(
        param_enum(list, "status"),
        sorted(&["PENDING", "APPROVED", "REJECTED"])
    );
    assert_eq!(
        path_param_format(
            &doc["paths"]["/api/v1/admin/posts/{postId}"]["get"],
            "postId"
        ),
        Some("uuid")
    );
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["AdminPostListItem"]["properties"]["topic"]["$ref"],
        "#/components/schemas/AdminPostListTopic"
    );
    for schema in ["AdminPostListItem", "AdminPostDetailResponse"] {
        assert_any_order(
            &doc,
            &[
                "components",
                "schemas",
                schema,
                "properties",
                "moderationStatus",
                "enum",
            ],
            &["PENDING", "APPROVED", "REJECTED"],
        );
    }
    assert_eq!(
        schemas["AdminPostDetailResponse"]["properties"]["topic"]["$ref"],
        "#/components/schemas/AdminPostDetailTopic"
    );
    for field in ["startsAt", "endsAt"] {
        assert!(
            schemas["AdminPostDetailTopic"]["properties"]
                .get(field)
                .is_some()
        );
    }
    assert_eq!(
        schemas["AdminPostDetailPhoto"]["properties"]["metadata"]["$ref"],
        "#/components/schemas/AdminPostDetailPhotoMetadata"
    );
    assert!(
        schemas["AdminPostDetailPhoto"]["properties"]
            .get("createdAt")
            .is_some()
    );
    for field in ["width", "height", "byteSize"] {
        assert!(
            schemas["AdminPostDetailPhotoMetadata"]["properties"]
                .get(field)
                .is_some(),
            "{field}"
        );
    }
    let upload = &schemas["AdminPostDetailResponse"]["properties"]["imageUpload"]["oneOf"];
    assert_eq!(
        upload[0]["$ref"],
        "#/components/schemas/AdminPostDetailImageUpload"
    );
    assert_eq!(upload[1]["type"], "null");
    for field in ["claimedAt", "expiresAt", "imageMetadata"] {
        assert!(
            schemas["AdminPostDetailImageUpload"]["properties"]
                .get(field)
                .is_none(),
            "{field}"
        );
    }

    // 검수
    let moderation = &doc["paths"]["/api/v1/admin/posts/{postId}/moderation"]["put"];
    assert_eq!(
        moderation["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/AdminPostModerationRequest"
    );
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "AdminPostModerationRequest",
            "properties",
            "status",
            "enum",
        ],
        &["APPROVED", "REJECTED"],
    );
    assert_eq!(
        schemas["AdminPostModerationRequest"]["properties"]["rejectionReason"]["maxLength"],
        500
    );
    for status in ["200", "400", "403", "404"] {
        assert!(has_response(moderation, status), "{status}");
    }
    for field in ["moderatedBy", "rejectionReason"] {
        assert!(
            schemas["AdminPostDetailResponse"]["properties"]
                .get(field)
                .is_some()
        );
    }

    // 삭제
    let delete = &doc["paths"]["/api/v1/admin/posts/{postId}"]["delete"];
    assert_eq!(
        delete["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/AdminPostDeletionRequest"
    );
    assert_eq!(schemas["AdminPostDeletionRequest"]["required"][0], "reason");
    let reason = &schemas["AdminPostDeletionRequest"]["properties"]["reason"];
    assert_eq!(reason["minLength"], 1);
    assert_eq!(reason["maxLength"], 500);
    for status in ["204", "400", "403", "404"] {
        assert!(has_response(delete, status), "{status}");
    }
    assert!(
        schemas["AdminPostDetailResponse"]["properties"]
            .get("mediaDeletion")
            .is_none()
    );
    assert!(schemas.get("AdminPostDetailMediaDeletion").is_none());

    // 사용자 문서에는 없다.
    let user = user_api();
    assert!(!exists(&user, &["paths", "/api/v1/admin/posts"]));
    assert!(!exists(&user, &["paths", "/api/v1/admin/posts/{postId}"]));
    assert!(!exists(
        &user,
        &["paths", "/api/v1/admin/posts/{postId}/moderation"]
    ));
}

// AdminTopicOpenApiTest#adminApiDocs_adminTopicEndpoints_exposeLifecycleContract
#[test]
fn admin_api_docs_admin_topic_endpoints_expose_lifecycle_contract() {
    let doc = admin_api();
    assert_eq!(
        param_names(&doc["paths"]["/api/v1/admin/topics"]["get"]),
        sorted(&["phase", "dateFrom", "dateTo", "sort", "page", "pageSize"])
    );
    assert!(has_response(
        &doc["paths"]["/api/v1/admin/topics"]["post"],
        "201"
    ));
    for method in ["get", "put", "delete"] {
        assert!(
            exists(&doc, &["paths", "/api/v1/admin/topics/{topicId}", method]),
            "{method}"
        );
    }
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "AdminTopicMutationRequest",
            "required",
        ],
        &["title", "topicDate", "startsAt", "endsAt"],
    );
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "AdminTopicDetailResponse",
            "properties",
            "phase",
            "enum",
        ],
        &["BEFORE_OPEN", "OPEN", "CLOSED"],
    );
    for field in ["total", "validating"] {
        assert!(
            doc["components"]["schemas"]["AdminTopicPostCounts"]["properties"]
                .get(field)
                .is_none(),
            "{field}"
        );
    }

    let user = user_api();
    assert!(!exists(&user, &["paths", "/api/v1/admin/topics"]));
    assert!(!exists(&user, &["paths", "/api/v1/admin/topics/{topicId}"]));
}

// AdminUserOpenApiTest#adminApiDocs_adminUserEndpoints_exposeQueryAndDetailContracts
#[test]
fn admin_api_docs_admin_user_endpoints_expose_query_and_detail_contracts() {
    let doc = admin_api();
    let list = &doc["paths"]["/api/v1/admin/users"]["get"];
    assert_eq!(
        param_names(list),
        sorted(&["status", "email", "sort", "page", "pageSize"])
    );
    assert_eq!(
        param_enum(list, "status"),
        sorted(&["ACTIVE", "BANNED", "WITHDRAWN"])
    );
    assert_eq!(
        path_param_format(
            &doc["paths"]["/api/v1/admin/users/{userId}"]["get"],
            "userId"
        ),
        Some("uuid")
    );
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["AdminUserListItem"]["properties"]["postCounts"]["$ref"],
        "#/components/schemas/AdminUserPostCounts"
    );
    for field in ["pending", "approved", "rejected"] {
        assert!(
            schemas["AdminUserPostCounts"]["properties"]
                .get(field)
                .is_some()
        );
    }
    for field in ["total", "validating"] {
        assert!(
            schemas["AdminUserPostCounts"]["properties"]
                .get(field)
                .is_none()
        );
    }
    assert_eq!(
        schemas["AdminUserDetailResponse"]["properties"]["signature"]["$ref"],
        "#/components/schemas/AdminUserSignature"
    );
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "AdminUserSignature",
            "properties",
            "originalImageUrl",
            "type",
        ],
        &["string", "null"],
    );
    assert!(has_response(
        &doc["paths"]["/api/v1/admin/users/{userId}"]["get"],
        "404"
    ));
    let status = &doc["paths"]["/api/v1/admin/users/{userId}/status"]["patch"];
    assert_eq!(
        status["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/AdminUserStatusUpdateRequest"
    );
    assert_any_order(
        &doc,
        &[
            "components",
            "schemas",
            "AdminUserStatusUpdateRequest",
            "properties",
            "status",
            "enum",
        ],
        &["ACTIVE", "BANNED"],
    );
    for code in ["400", "404"] {
        assert!(has_response(status, code), "{code}");
    }

    let user = user_api();
    for path in [
        "/api/v1/admin/users",
        "/api/v1/admin/users/{userId}",
        "/api/v1/admin/users/{userId}/status",
    ] {
        assert!(!exists(&user, &["paths", path]), "{path}");
    }
}
