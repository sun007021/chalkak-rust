//! 컨트롤러 계층 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `AuthControllerTest`, `UserControllerTest`, `SignatureProcessingCallbackControllerTest`는 `@WebMvcTest`에서
//! 서비스를 Mockito로 바꾸고 필터를 끈 채 상태 코드·오류 코드·직렬화를 본다. 여기서는 같은 요청을 라우터에 직접 넣고
//! (인증 필터 없음, 주체는 `security::enter`), 서비스는 `World`의 실제 서비스와 PostgreSQL을 쓴다. Mockito로 만든
//! 반환값 대신 같은 결과를 내는 실제 상태를 DB에 만들어 둔다. `verifyNoInteractions`는 DB가 그대로인지로 확인한다.

mod common;

use chalkak_app::auth::apple::AppleTokenExchangeResult;
use chalkak_app::auth::domain::SocialProvider;
use chalkak_app::security::{self, AUTHORITY_USER, Principal, PrincipalGuard};
use chalkak_http::{Handler, Method, Request, Response, Router};
use common::{PRESIGNED_URL, World, png};
use serde_json::{Value, json};
use uuid::Uuid;

const ID_TOKEN: &str = "google-id-token";
const RAW_NONCE: &str = "raw-nonce";
const SUBJECT: &str = "google-subject";
const UNAUTHORIZED_MESSAGE: &str = "유효하지 않은 인증 정보입니다.";
const TIMESTAMP_HEADER: &str = "X-Chalkak-Callback-Timestamp";
const SIGNATURE_HEADER: &str = "X-Chalkak-Callback-Signature";

fn send(router: &Router, method: Method, path: &str, body: Option<&str>) -> Response {
    let mut req = Request::new(method, path);
    if let Some(body) = body {
        req.headers.append("Content-Type", "application/json");
        req.body = body.as_bytes().to_vec();
    }
    router.handle(req)
}

fn post_json(router: &Router, path: &str, body: &str) -> Response {
    send(router, Method::Post, path, Some(body))
}

fn json_of(response: &Response) -> Value {
    serde_json::from_slice(&response.body).unwrap_or(Value::Null)
}

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

fn assert_error_message(response: &Response, status: u16, error_code: &str, message: &str) {
    let body = assert_error(response, status, error_code);
    assert_eq!(body["message"], message);
}

fn login_as(user_id: Uuid) -> PrincipalGuard {
    security::enter(Principal {
        subject: user_id.to_string(),
        authorities: vec![AUTHORITY_USER.to_owned()],
    })
}

fn social_login_body(provider: &str, extra: &str) -> String {
    format!(r#"{{"provider":"{provider}","idToken":"{ID_TOKEN}","rawNonce":"{RAW_NONCE}"{extra}}}"#)
}

fn given_google_identity(w: &World) {
    w.google
        .will_return(ID_TOKEN, RAW_NONCE, World::google_identity(SUBJECT, None));
}

/// 소셜 계정이 이미 있는 회원.
fn existing_google_user(w: &World) -> Uuid {
    given_google_identity(w);
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);
    user.id
}

fn processed_upload(w: &World) -> Uuid {
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    w.storage.processing_completed(upload_id, &[true]);
    upload_id
}

fn assert_token_pair(body: &Value) {
    assert!(body["accessToken"].as_str().is_some_and(|t| !t.is_empty()));
    assert_eq!(body["expiresIn"], 900);
    assert!(body["refreshToken"].as_str().is_some_and(|t| !t.is_empty()));
    assert_eq!(body["refreshTokenExpiresIn"], 2592000);
}

// ---------------------------------------------------------------------------------------------
// AuthControllerTest: 소셜 로그인
// ---------------------------------------------------------------------------------------------

// AuthControllerTest#socialLogin_appleWithoutAuthorizationCode_returnsBadRequest
#[test]
fn social_login_apple_without_authorization_code_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        &social_login_body("APPLE", ""),
    );

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "Authorization Code는 APPLE 로그인에만 필요합니다.",
    );
    assert!(w.apple_client.exchanged().is_empty());
}

// AuthControllerTest#socialLogin_nonAppleWithAuthorizationCode_returnsBadRequest
#[test]
fn social_login_non_apple_with_authorization_code_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        &social_login_body(
            "GOOGLE",
            r#","authorizationCode":"apple-authorization-code""#,
        ),
    );

    assert_error(&response, 400, "BUSINESS_ERROR");
    assert!(w.apple_client.exchanged().is_empty());
}

// AuthControllerTest#socialLogin_appleWithAuthorizationCode_returnsSignUpRequired
#[test]
fn social_login_apple_with_authorization_code_returns_sign_up_required() {
    let Some(w) = World::create() else { return };
    w.apple_verifier.will_return(
        "apple-id-token",
        RAW_NONCE,
        World::apple_identity("apple-subject"),
    );
    w.apple_client.exchange_returns(
        "apple-authorization-code",
        AppleTokenExchangeResult {
            id_token: "apple-exchanged-id-token".to_owned(),
            refresh_token: "apple-refresh-token".to_owned(),
        },
    );
    w.apple_verifier.will_return(
        "apple-exchanged-id-token",
        RAW_NONCE,
        World::apple_identity("apple-subject"),
    );
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        r#"{"provider":"APPLE","idToken":"apple-id-token","rawNonce":"raw-nonce","authorizationCode":"apple-authorization-code"}"#,
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(body["status"], "SIGN_UP_REQUIRED");
    assert!(body.get("accessToken").is_none());
}

// AuthControllerTest#socialLogin_existingUser_returnsLoginSuccess
#[test]
fn social_login_existing_user_returns_login_success() {
    let Some(w) = World::create() else { return };
    let user_id = existing_google_user(&w);
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        &social_login_body("GOOGLE", ""),
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(body["status"], "LOGIN_SUCCESS");
    assert_eq!(body["userId"], user_id.to_string());
    assert_token_pair(&body);
}

// AuthControllerTest#socialLogin_newUser_returnsSignUpRequired
#[test]
fn social_login_new_user_returns_sign_up_required() {
    let Some(w) = World::create() else { return };
    given_google_identity(&w);
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        &social_login_body("GOOGLE", ""),
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(body["status"], "SIGN_UP_REQUIRED");
    for field in ["userId", "accessToken", "expiresIn"] {
        assert!(body.get(field).is_none(), "{field}");
    }
}

// AuthControllerTest#socialLogin_kakaoProvider_returnsLoginResult
#[test]
fn social_login_kakao_provider_returns_login_result() {
    let Some(w) = World::create() else { return };
    w.kakao.will_return(
        "kakao-id-token",
        RAW_NONCE,
        World::google_identity("kakao-subject", None),
    );
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        r#"{"provider":"KAKAO","idToken":"kakao-id-token","rawNonce":"raw-nonce"}"#,
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(body["status"], "SIGN_UP_REQUIRED");
    for field in ["userId", "accessToken", "expiresIn"] {
        assert!(body.get(field).is_none(), "{field}");
    }
}

// AuthControllerTest#socialLogin_blankIdToken_returnsBadRequest
#[test]
fn social_login_blank_id_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        r#"{"provider":"GOOGLE","idToken":" ","rawNonce":"raw-nonce"}"#,
    );

    assert_error(&response, 400, "BUSINESS_ERROR");
}

// AuthControllerTest#socialLogin_blankOrMissingRawNonce_returnsBadRequest (ValueSource 2건)
#[test]
fn social_login_blank_or_missing_raw_nonce_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    for raw_nonce_field in [r#""rawNonce": " ","#, ""] {
        let body = format!(
            r#"{{ {raw_nonce_field} "provider": "GOOGLE", "idToken": "google-id-token" }}"#
        );
        let response = post_json(&router, "/api/v1/auth/social-login", &body);
        assert_error(&response, 400, "BUSINESS_ERROR");
    }
}

// AuthControllerTest#socialLogin_missingProvider_returnsBadRequest
#[test]
fn social_login_missing_provider_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-login",
        r#"{"idToken":"google-id-token","rawNonce":"raw-nonce"}"#,
    );

    assert_error(&response, 400, "BUSINESS_ERROR");
}

// ---------------------------------------------------------------------------------------------
// AuthControllerTest: 회원가입 서명 업로드·회원가입
// ---------------------------------------------------------------------------------------------

// AuthControllerTest#createSocialSignupSignatureUpload_validRequest_returnsUploadInformation
#[test]
fn create_social_signup_signature_upload_valid_request_returns_upload_information() {
    let Some(w) = World::create() else { return };
    given_google_identity(&w);
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-signup/signature/uploads",
        &social_login_body("GOOGLE", ""),
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert!(Uuid::parse_str(body["uploadId"].as_str().unwrap()).is_ok());
    assert_eq!(body["uploadUrl"], PRESIGNED_URL);
    assert_eq!(body["expiresInSeconds"], 300);
    assert!(body["signupToken"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(body.get("signupTokenExpiresInSeconds").is_none());
}

// AuthControllerTest#createSocialSignupSignatureUpload_blankIdToken_returnsBadRequest
#[test]
fn create_social_signup_signature_upload_blank_id_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-signup/signature/uploads",
        r#"{"provider":"GOOGLE","idToken":" ","rawNonce":"raw-nonce"}"#,
    );

    assert_error(&response, 400, "BUSINESS_ERROR");
    assert_eq!(w.issuer.interactions(), 0);
}

// AuthControllerTest#createSocialSignupSignatureUpload_blankOrMissingRawNonce_returnsBadRequest (ValueSource 2건)
#[test]
fn create_social_signup_signature_upload_blank_or_missing_raw_nonce_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    for raw_nonce_field in [r#""rawNonce": " ","#, ""] {
        let body = format!(
            r#"{{ {raw_nonce_field} "provider": "GOOGLE", "idToken": "google-id-token" }}"#
        );
        let response = post_json(
            &router,
            "/api/v1/auth/social-signup/signature/uploads",
            &body,
        );
        assert_error(&response, 400, "BUSINESS_ERROR");
    }
    assert_eq!(w.issuer.interactions(), 0);
}

// AuthControllerTest#socialSignup_validRequest_returnsUserId
#[test]
fn social_signup_valid_request_returns_user_id() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&World::google_identity(SUBJECT, None), upload_id);
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-signup",
        &json!({ "signupToken": token }).to_string(),
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    let user_id = Uuid::parse_str(body["userId"].as_str().unwrap()).unwrap();
    assert!(w.find_active_user(user_id).is_some());
    assert_token_pair(&body);
}

// AuthControllerTest#socialSignup_missingSignupToken_returnsBadRequest
#[test]
fn social_signup_missing_signup_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(&router, "/api/v1/auth/social-signup", "{}");

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "회원가입 토큰은 필수입니다.",
    );
}

// AuthControllerTest#socialSignup_blankSignupToken_returnsBadRequest
#[test]
fn social_signup_blank_signup_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-signup",
        r#"{"signupToken":" "}"#,
    );

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "회원가입 토큰은 필수입니다.",
    );
}

// AuthControllerTest#socialSignup_processingSignature_returnsProcessingPendingError
#[test]
fn social_signup_processing_signature_returns_processing_pending_error() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    w.storage.processing_completed(upload_id, &[false]);
    let token = w.signup_token(&World::google_identity(SUBJECT, None), upload_id);
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/social-signup",
        &json!({ "signupToken": token }).to_string(),
    );

    assert_error_message(
        &response,
        400,
        "SIGNATURE_PROCESSING_PENDING",
        "사인 이미지 처리 중입니다.",
    );
}

// ---------------------------------------------------------------------------------------------
// AuthControllerTest: 재발급·로그아웃
// ---------------------------------------------------------------------------------------------

/// 로그인으로 받은 리프레시 토큰.
fn login_refresh_token(w: &World, router: &Router) -> (Uuid, String) {
    let user_id = existing_google_user(w);
    let response = post_json(
        router,
        "/api/v1/auth/social-login",
        &social_login_body("GOOGLE", ""),
    );
    assert_eq!(response.status, 200);
    let token = json_of(&response)["refreshToken"]
        .as_str()
        .unwrap()
        .to_owned();
    (user_id, token)
}

// AuthControllerTest#refresh_validRefreshToken_returnsRotatedTokens
#[test]
fn refresh_valid_refresh_token_returns_rotated_tokens() {
    let Some(w) = World::create() else { return };
    let router = w.router();
    let (_, refresh_token) = login_refresh_token(&w, &router);

    let response = post_json(
        &router,
        "/api/v1/auth/refresh",
        &json!({ "refreshToken": refresh_token }).to_string(),
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_token_pair(&body);
    assert_ne!(body["refreshToken"], refresh_token.as_str());
}

// AuthControllerTest#refresh_missingRefreshToken_returnsBadRequest
#[test]
fn refresh_missing_refresh_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(&router, "/api/v1/auth/refresh", "{}");

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "리프레시 토큰은 필수입니다.",
    );
}

// AuthControllerTest#refresh_blankRefreshToken_returnsBadRequest
#[test]
fn refresh_blank_refresh_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(&router, "/api/v1/auth/refresh", r#"{"refreshToken":" "}"#);

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "리프레시 토큰은 필수입니다.",
    );
}

// AuthControllerTest#refresh_rejectedRefreshToken_returnsReauthenticationRequired
#[test]
fn refresh_rejected_refresh_token_returns_reauthentication_required() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/refresh",
        r#"{"refreshToken":"chalkak-refresh-token"}"#,
    );

    assert_error_message(
        &response,
        401,
        "REAUTHENTICATION_REQUIRED",
        "다시 로그인해 주세요.",
    );
}

// AuthControllerTest#logout_validRefreshToken_returnsNoContent
#[test]
fn logout_valid_refresh_token_returns_no_content() {
    let Some(w) = World::create() else { return };
    let router = w.router();
    let (user_id, refresh_token) = login_refresh_token(&w, &router);
    assert_eq!(w.live_refresh_token_count(user_id), 1);

    let response = post_json(
        &router,
        "/api/v1/auth/logout",
        &json!({ "refreshToken": refresh_token }).to_string(),
    );

    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    assert_eq!(w.live_refresh_token_count(user_id), 0);
}

// AuthControllerTest#logout_unknownRefreshToken_returnsNoContent
#[test]
fn logout_unknown_refresh_token_returns_no_content() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(
        &router,
        "/api/v1/auth/logout",
        r#"{"refreshToken":"unknown-refresh-token"}"#,
    );

    assert_eq!(response.status, 204);
}

// AuthControllerTest#logout_missingRefreshToken_returnsBadRequest
#[test]
fn logout_missing_refresh_token_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = post_json(&router, "/api/v1/auth/logout", "{}");

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "리프레시 토큰은 필수입니다.",
    );
}

// ---------------------------------------------------------------------------------------------
// UserControllerTest
// ---------------------------------------------------------------------------------------------

fn delete_me(router: &Router) -> Response {
    send(router, Method::Delete, "/api/v1/users/me", None)
}

// UserControllerTest#withdraw_validRequest_returnsNoContent
#[test]
fn withdraw_valid_request_returns_no_content() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let response = delete_me(&router);

    assert_eq!(response.status, 204);
    assert!(w.find_active_user(user.id).is_none());
}

// UserControllerTest#withdraw_notExistingUser_returnsUnauthorized
#[test]
fn withdraw_not_existing_user_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.router();
    let _login = login_as(Uuid::new_v4());

    let response = delete_me(&router);

    assert_error_message(&response, 401, "UNAUTHORIZED", UNAUTHORIZED_MESSAGE);
}

// UserControllerTest#withdraw_unauthenticated_returnsUnauthorized
#[test]
fn withdraw_unauthenticated_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();

    let response = delete_me(&router);

    assert_error_message(&response, 401, "UNAUTHORIZED", UNAUTHORIZED_MESSAGE);
    assert!(w.find_active_user(user.id).is_some());
}

// UserControllerTest#withdraw_userIdAsQueryParameter_returnsUnauthorized
#[test]
fn withdraw_user_id_as_query_parameter_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let victim = w.save_user();
    let router = w.router();

    let mut req = Request::new(Method::Delete, "/api/v1/users/me");
    req.query = vec![("userId".to_owned(), victim.id.to_string())];
    let response = router.handle(req);

    assert_eq!(response.status, 401);
    assert!(w.find_active_user(victim.id).is_some());
}

// UserControllerTest#withdraw_bothHeaderAndQueryParameter_usesHeader
#[test]
fn withdraw_both_header_and_query_parameter_uses_header() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let victim = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let mut req = Request::new(Method::Delete, "/api/v1/users/me");
    req.query = vec![("userId".to_owned(), victim.id.to_string())];
    let response = router.handle(req);

    assert_eq!(response.status, 204);
    assert!(w.find_active_user(user.id).is_none());
    assert!(w.find_active_user(victim.id).is_some());
}

// UserControllerTest#createSignatureUpload_validRequest_returnsUploadInformation
#[test]
fn create_signature_upload_valid_request_returns_upload_information() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let response = send(
        &router,
        Method::Post,
        "/api/v1/users/me/signature/uploads",
        None,
    );

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert!(Uuid::parse_str(body["uploadId"].as_str().unwrap()).is_ok());
    assert_eq!(body["uploadUrl"], PRESIGNED_URL);
    assert_eq!(body["expiresInSeconds"], 300);
}

// UserControllerTest#createSignatureUpload_missingUserIdHeader_returnsUnauthorized
#[test]
fn create_signature_upload_missing_user_id_header_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = send(
        &router,
        Method::Post,
        "/api/v1/users/me/signature/uploads",
        None,
    );

    assert_error(&response, 401, "UNAUTHORIZED");
    assert_eq!(w.issuer.interactions(), 0);
}

// UserControllerTest#createSignatureUpload_notExistingUser_returnsUnauthorized
#[test]
fn create_signature_upload_not_existing_user_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.router();
    let _login = login_as(Uuid::new_v4());

    let response = send(
        &router,
        Method::Post,
        "/api/v1/users/me/signature/uploads",
        None,
    );

    assert_error_message(&response, 401, "UNAUTHORIZED", UNAUTHORIZED_MESSAGE);
}

// UserControllerTest#getSignature_validRequest_returnsOriginalAndThumbnailImageUrls
#[test]
fn get_signature_valid_request_returns_original_and_thumbnail_image_urls() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let response = send(&router, Method::Get, "/api/v1/users/me/signature", None);

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(
        body["signatureOriginalImageUrl"],
        format!(
            "https://cdn.test.chalkak/{}",
            user.signature_original_storage_key
        )
    );
    assert_eq!(
        body["signatureThumbnailImageUrl"],
        format!(
            "https://cdn.test.chalkak/{}",
            user.signature_thumbnail_storage_key.as_deref().unwrap()
        )
    );
}

// UserControllerTest#getSignature_unauthenticated_returnsUnauthorized
#[test]
fn get_signature_unauthenticated_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = send(&router, Method::Get, "/api/v1/users/me/signature", None);

    assert_error(&response, 401, "UNAUTHORIZED");
}

// UserControllerTest#getSignature_notExistingUser_returnsUnauthorized
#[test]
fn get_signature_not_existing_user_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.router();
    let _login = login_as(Uuid::new_v4());

    let response = send(&router, Method::Get, "/api/v1/users/me/signature", None);

    assert_error_message(&response, 401, "UNAUTHORIZED", UNAUTHORIZED_MESSAGE);
}

// UserControllerTest#getSignature_processingFailed_returnsBadRequest
#[test]
fn get_signature_processing_failed_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let upload_id = Uuid::new_v4();
    w.modify_user(user.id, |u| {
        u.start_signature_processing(upload_id, w.now()).unwrap();
        u.fail_signature_processing(upload_id);
    });
    let router = w.router();
    let _login = login_as(user.id);

    let response = send(&router, Method::Get, "/api/v1/users/me/signature", None);

    assert_error_message(
        &response,
        400,
        "SIGNATURE_REGISTRATION_REQUIRED",
        "사인 이미지 처리에 실패했습니다. 사인을 다시 등록해 주세요.",
    );
}

fn put_signature(router: &Router, body: &str) -> Response {
    send(
        router,
        Method::Put,
        "/api/v1/users/me/signature",
        Some(body),
    )
}

fn upload_id_body(upload_id: Uuid) -> String {
    format!(r#"{{"signatureOriginalUploadId":"{upload_id}"}}"#)
}

// UserControllerTest#updateSignature_validRequest_returnsImageUrl
#[test]
fn update_signature_valid_request_returns_image_url() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    let router = w.router();
    let _login = login_as(user.id);

    let response = put_signature(&router, &upload_id_body(upload_id));

    assert_eq!(response.status, 200);
    assert_eq!(
        json_of(&response)["signatureOriginalImageUrl"],
        format!(
            "https://cdn.test.chalkak/{}",
            user.signature_original_storage_key
        )
    );
}

// UserControllerTest#updateSignature_failedUpload_returnsReuploadRequired
#[test]
fn update_signature_failed_upload_returns_reupload_required() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let upload_id = Uuid::new_v4();
    w.modify_user(user.id, |u| {
        u.start_signature_processing(upload_id, w.now()).unwrap();
        u.fail_signature_processing(upload_id);
    });
    let router = w.router();
    let _login = login_as(user.id);

    let response = put_signature(&router, &upload_id_body(upload_id));

    assert_error_message(
        &response,
        400,
        "SIGNATURE_REUPLOAD_REQUIRED",
        "사인 이미지 처리에 실패했습니다. 새로운 업로드 ID를 발급받아 이미지를 다시 업로드해 주세요.",
    );
}

// UserControllerTest#updateSignature_missingUserIdHeader_returnsUnauthorized
#[test]
fn update_signature_missing_user_id_header_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let router = w.router();

    let response = put_signature(&router, &upload_id_body(Uuid::new_v4()));

    assert_error(&response, 401, "UNAUTHORIZED");
}

// UserControllerTest#updateSignature_missingUploadId_returnsBadRequest
#[test]
fn update_signature_missing_upload_id_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let response = put_signature(&router, "{}");

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "사인 이미지 업로드 정보가 올바르지 않습니다.",
    );
}

// UserControllerTest#updateSignature_invalidUploadId_returnsBadRequest
#[test]
fn update_signature_invalid_upload_id_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let response = put_signature(&router, r#"{"signatureOriginalUploadId":"not-a-uuid"}"#);

    assert_error(&response, 400, "BUSINESS_ERROR");
}

// UserControllerTest#updateSignature_notUploadedImage_returnsNotFound
#[test]
fn update_signature_not_uploaded_image_returns_not_found() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let router = w.router();
    let _login = login_as(user.id);

    let response = put_signature(&router, &upload_id_body(Uuid::new_v4()));

    assert_error_message(
        &response,
        404,
        "BUSINESS_ERROR",
        "업로드한 사인 이미지를 찾을 수 없습니다.",
    );
}

// UserControllerTest#updateSignature_disallowedImage_returnsBadRequest
#[test]
fn update_signature_disallowed_image_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let upload_id = Uuid::new_v4();
    w.storage
        .uploaded(upload_id, common::image("image/gif", 1024));
    let router = w.router();
    let _login = login_as(user.id);

    let response = put_signature(&router, &upload_id_body(upload_id));

    assert_error_message(
        &response,
        400,
        "BUSINESS_ERROR",
        "사용할 수 없는 사인 이미지입니다.",
    );
}

// ---------------------------------------------------------------------------------------------
// SignatureProcessingCallbackControllerTest
// ---------------------------------------------------------------------------------------------

fn callback_path(upload_id: &str, result: &str) -> String {
    format!("/internal/v1/signature-processing/{upload_id}/{result}")
}

/// 시계의 현재 시각으로 서명한 콜백 요청.
fn signed_callback(w: &World, upload_id: Uuid, result: &str) -> Request {
    let path = callback_path(&upload_id.to_string(), result);
    let timestamp = common::ts(common::NOW).as_second().to_string();
    let signature = w.callback_auth().sign(&timestamp, &path, None);
    let mut req = Request::new(Method::Post, path);
    req.headers.append(TIMESTAMP_HEADER, timestamp);
    req.headers.append(SIGNATURE_HEADER, signature);
    req
}

// SignatureProcessingCallbackControllerTest#issueUploadUrls_authenticatedRequest_returnsPresignedUrls
#[test]
fn issue_upload_urls_authenticated_request_returns_presigned_urls() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let router = w.router();

    let response = router.handle(signed_callback(&w, upload_id, "upload-urls"));

    assert_eq!(response.status, 200);
    let body = json_of(&response);
    assert_eq!(
        body["originalUploadUrl"],
        format!("{PRESIGNED_URL}/original/{upload_id}")
    );
    assert_eq!(
        body["thumbnailUploadUrl"],
        format!("{PRESIGNED_URL}/thumbnail/{upload_id}")
    );
    assert_eq!(body["contentType"], "image/png");
    assert_eq!(body["cacheControl"], "public, max-age=86400");
}

// SignatureProcessingCallbackControllerTest#issueUploadUrls_nonCanonicalUploadId_returnsBadRequest
#[test]
fn issue_upload_urls_non_canonical_upload_id_returns_bad_request() {
    let Some(w) = World::create() else { return };
    let router = w.router();
    let path = callback_path("1-1-1-1-1", "upload-urls");
    let mut req = Request::new(Method::Post, path.clone());
    req.headers.append(TIMESTAMP_HEADER, "1787562000");
    req.headers.append(SIGNATURE_HEADER, "signature");

    let response = router.handle(req);

    assert_eq!(response.status, 400);
}

/// 처리 중인 사인이 있는 회원.
fn processing_user(w: &World, upload_id: Uuid) -> Uuid {
    let user = w.save_user();
    w.modify_user(user.id, |u| {
        u.start_signature_processing(upload_id, w.now()).unwrap();
    });
    user.id
}

// SignatureProcessingCallbackControllerTest#complete_authenticatedCallback_completesProcessing
#[test]
fn complete_authenticated_callback_completes_processing() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user_id = processing_user(&w, upload_id);
    let router = w.router();

    let response = router.handle(signed_callback(&w, upload_id, "complete"));

    assert_eq!(response.status, 204);
    let user = w.find_user(user_id);
    assert_eq!(user.pending_signature_upload_id, None);
    assert_eq!(
        user.signature_original_storage_key,
        common::keys_for(upload_id).original
    );
}

// SignatureProcessingCallbackControllerTest#fail_authenticatedCallback_failsProcessing
#[test]
fn fail_authenticated_callback_fails_processing() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user_id = processing_user(&w, upload_id);
    let router = w.router();

    let response = router.handle(signed_callback(&w, upload_id, "failed"));

    assert_eq!(response.status, 204);
    assert_eq!(
        w.find_user(user_id).signature_processing_status,
        Some(chalkak_app::user::domain::SignatureProcessingStatus::Failed)
    );
}

// SignatureProcessingCallbackControllerTest#complete_invalidSignature_returnsUnauthorized
#[test]
fn complete_invalid_signature_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user_id = processing_user(&w, upload_id);
    let router = w.router();
    let mut req = signed_callback(&w, upload_id, "complete");
    req.headers.set(SIGNATURE_HEADER, "v1=invalid");

    let response = router.handle(req);

    assert_error(&response, 401, "UNAUTHORIZED");
    assert_eq!(
        w.find_user(user_id).pending_signature_upload_id,
        Some(upload_id)
    );
}

// SignatureProcessingCallbackControllerTest#complete_missingSignatureHeader_returnsUnauthorized
#[test]
fn complete_missing_signature_header_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user_id = processing_user(&w, upload_id);
    let router = w.router();
    let mut req = Request::new(
        Method::Post,
        callback_path(&upload_id.to_string(), "complete"),
    );
    req.headers.append(
        TIMESTAMP_HEADER,
        common::ts(common::NOW).as_second().to_string(),
    );

    let response = router.handle(req);

    assert_error_message(
        &response,
        401,
        "UNAUTHORIZED",
        "유효하지 않은 이미지 처리 콜백입니다.",
    );
    assert_eq!(
        w.find_user(user_id).pending_signature_upload_id,
        Some(upload_id)
    );
}

// SignatureProcessingCallbackControllerTest#complete_missingTimestampHeader_returnsUnauthorized
#[test]
fn complete_missing_timestamp_header_returns_unauthorized() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user_id = processing_user(&w, upload_id);
    let router = w.router();
    let mut req = Request::new(
        Method::Post,
        callback_path(&upload_id.to_string(), "complete"),
    );
    req.headers.append(SIGNATURE_HEADER, "v1=signature");

    let response = router.handle(req);

    assert_error_message(
        &response,
        401,
        "UNAUTHORIZED",
        "유효하지 않은 이미지 처리 콜백입니다.",
    );
    assert_eq!(
        w.find_user(user_id).pending_signature_upload_id,
        Some(upload_id)
    );
}
