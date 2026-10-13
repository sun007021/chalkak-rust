//! 소셜 로그인 서비스의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `SocialLoginServiceTest`, `ExistingSocialAccountLoginProcessorTest`, `AppleAuthorizationServiceTest`를 옮겼다.
//! 외부 경계(ID Token 검증, Apple 토큰 교환)만 가짜이고 서비스·저장소·토큰 발급은 실제 것이다.

mod common;

use chalkak_app::auth::apple::AppleTokenExchangeResult;
use chalkak_app::auth::domain::SocialProvider;
use chalkak_app::auth::service::SocialLoginResult;
use chalkak_app::auth::tokens::apple_authorization_fingerprint;
use chalkak_app::user::service::SocialConnectionRevoker;
use chalkak_app::web::ApiError;
use chalkak_core::error::{ErrorCode, ErrorKind};
use common::{
    World, access_token_subject, assert_app_error, assert_app_error_message, assert_lower_hex_64,
    minutes_later,
};
use uuid::Uuid;

const ID_TOKEN: &str = "google-id-token";
const RAW_NONCE: &str = "raw-nonce";
const SUBJECT: &str = "google-subject";
const APPLE_ID_TOKEN: &str = "apple-id-token";
const APPLE_SUBJECT: &str = "apple-subject";
const AUTHORIZATION_CODE: &str = "apple-authorization-code";
const EXCHANGED_ID_TOKEN: &str = "exchanged-apple-id-token";
const REFRESH_TOKEN: &str = "apple-refresh-token";

fn google_login(w: &World) -> Result<SocialLoginResult, ApiError> {
    w.google.will_return(
        ID_TOKEN,
        RAW_NONCE,
        World::google_identity(SUBJECT, Some("user@chalkak.test")),
    );
    w.login
        .login(SocialProvider::Google, ID_TOKEN, RAW_NONCE, None)
}

fn login_with_apple(w: &World) -> Result<SocialLoginResult, ApiError> {
    w.login.login(
        SocialProvider::Apple,
        APPLE_ID_TOKEN,
        RAW_NONCE,
        Some(AUTHORIZATION_CODE),
    )
}

fn given_verified_apple_id_token(w: &World) {
    w.apple_verifier.will_return(
        APPLE_ID_TOKEN,
        RAW_NONCE,
        World::apple_identity(APPLE_SUBJECT),
    );
}

fn given_successful_apple_authentication(w: &World) {
    given_verified_apple_id_token(w);
    w.apple_client.exchange_returns(
        AUTHORIZATION_CODE,
        AppleTokenExchangeResult {
            id_token: EXCHANGED_ID_TOKEN.to_owned(),
            refresh_token: REFRESH_TOKEN.to_owned(),
        },
    );
    w.apple_verifier.will_return(
        EXCHANGED_ID_TOKEN,
        RAW_NONCE,
        World::apple_identity(APPLE_SUBJECT),
    );
}

fn success(result: SocialLoginResult) -> chalkak_app::auth::service::SocialLoginSuccess {
    match result {
        SocialLoginResult::LoginSuccess(s) => s,
        SocialLoginResult::SignUpRequired => panic!("로그인 성공이어야 한다"),
    }
}

fn user_with_google_account(w: &World) -> chalkak_app::user::domain::User {
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);
    user
}

// ---------------------------------------------------------------------------------------------
// SocialLoginServiceTest
// ---------------------------------------------------------------------------------------------

// SocialLoginServiceTest#login_existingSocialAccount_returnsLoginSuccess
#[test]
fn login_existing_social_account_returns_login_success() {
    let Some(w) = World::create() else { return };
    let user = user_with_google_account(&w);

    let result = google_login(&w).unwrap();

    let SocialLoginResult::LoginSuccess(s) = result else {
        panic!("LOGIN_SUCCESS여야 한다");
    };
    assert_eq!(s.user_id, user.id);
}

// SocialLoginServiceTest#login_existingSocialAccount_issuesRefreshToken
#[test]
fn login_existing_social_account_issues_refresh_token() {
    let Some(w) = World::create() else { return };
    let user = user_with_google_account(&w);

    let s = success(google_login(&w).unwrap());

    assert!(!s.refresh_token.value.trim().is_empty());
    let stored = w.stored_refresh_hashes(user.id);
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0], s.refresh_token.value);
    assert_lower_hex_64(&stored[0]);
}

// SocialLoginServiceTest#login_newSocialAccount_returnsSignUpRequired, login_newSocialAccount_doesNotIssueAccessToken
#[test]
fn login_new_social_account_returns_sign_up_required() {
    let Some(w) = World::create() else { return };

    let result = google_login(&w).unwrap();

    // 회원가입이 필요하면 회원 식별자도 액세스 토큰도 없다(`SignUpRequired`에는 담을 곳이 없다).
    assert_eq!(result, SocialLoginResult::SignUpRequired);
}

#[test]
fn login_new_social_account_does_not_issue_access_token() {
    let Some(w) = World::create() else { return };

    let result = google_login(&w).unwrap();

    assert_eq!(result, SocialLoginResult::SignUpRequired);
    assert_eq!(w.count("SELECT COUNT(*) FROM user_refresh_tokens"), 0);
}

// SocialLoginServiceTest#login_withdrawnUser_returnsSignUpRequired
#[test]
fn login_withdrawn_user_returns_sign_up_required() {
    let Some(w) = World::create() else { return };
    let user = user_with_google_account(&w);
    w.withdrawal.withdraw(user.id).unwrap();

    let result = google_login(&w).unwrap();

    assert_eq!(result, SocialLoginResult::SignUpRequired);
}

// SocialLoginServiceTest#login_withdrawnBannedUser_throwsForbiddenException
#[test]
fn login_withdrawn_banned_user_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let user = user_with_google_account(&w);
    w.ban(user.id);
    w.withdrawal.withdraw(user.id).unwrap();

    assert_app_error(
        google_login(&w),
        ErrorKind::Forbidden,
        ErrorCode::FORBIDDEN,
        "탈퇴한 차단 소셜 계정입니다.",
    );
}

// SocialLoginServiceTest#login_bannedUser_issuesAccessToken
#[test]
fn login_banned_user_issues_access_token() {
    let Some(w) = World::create() else { return };
    let user = w.save_banned_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);

    let s = success(google_login(&w).unwrap());

    assert_eq!(s.user_id, user.id);
    assert_eq!(
        access_token_subject(&s.access_token.value),
        user.id.to_string()
    );
}

// SocialLoginServiceTest#login_existingSocialAccount_issuesAccessToken
#[test]
fn login_existing_social_account_issues_access_token() {
    let Some(w) = World::create() else { return };
    let user = user_with_google_account(&w);

    let s = success(google_login(&w).unwrap());

    assert_eq!(
        access_token_subject(&s.access_token.value),
        user.id.to_string()
    );
    assert!(s.access_token.expires_in_seconds > 0);
}

// SocialLoginServiceTest#login_existingAppleAccount_returnsLoginSuccessWithoutExchange
#[test]
fn login_existing_apple_account_returns_login_success_without_exchange() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Apple, APPLE_SUBJECT);
    given_verified_apple_id_token(&w);

    let s = success(login_with_apple(&w).unwrap());

    assert_eq!(s.user_id, user.id);
    assert!(!s.access_token.value.is_empty());
    assert!(!s.refresh_token.value.trim().is_empty());
    assert!(w.apple_client.exchanged().is_empty());
}

// SocialLoginServiceTest#login_existingAppleAuthorization_keepsStoredRefreshToken
#[test]
fn login_existing_apple_authorization_keeps_stored_refresh_token() {
    let Some(w) = World::create() else { return };
    // 로그인마다 교환하면 Apple에 폐기할 수 없는 grant가 하나씩 쌓이므로, 가입 시점에 저장한 RT를 그대로 둔다.
    let user = w.save_user();
    let account = w.save_social_account(user.id, SocialProvider::Apple, APPLE_SUBJECT);
    w.save_apple_authorization(account, "encrypted-apple-refresh-token");
    given_verified_apple_id_token(&w);

    login_with_apple(&w).unwrap();

    let authorizations = w.apple_authorizations(account);
    assert_eq!(authorizations.len(), 1);
    assert_eq!(
        authorizations[0].encrypted_refresh_token,
        "encrypted-apple-refresh-token"
    );
    assert!(w.apple_client.exchanged().is_empty());
}

// SocialLoginServiceTest#login_newAppleAccount_storesPendingAuthorizationWithConfiguredExpiry
#[test]
fn login_new_apple_account_stores_pending_authorization_with_configured_expiry() {
    let Some(w) = World::create() else { return };
    given_successful_apple_authentication(&w);

    let result = login_with_apple(&w).unwrap();

    assert_eq!(result, SocialLoginResult::SignUpRequired);
    let pending = w.latest_pending(APPLE_SUBJECT).expect("임시 보관분이 있다");
    // 암호문은 원문이 아니고 복호화하면 교환한 리프레시 토큰이 나온다.
    assert_ne!(pending.encrypted_refresh_token, REFRESH_TOKEN);
    assert_eq!(
        w.cipher.decrypt(&pending.encrypted_refresh_token).unwrap(),
        REFRESH_TOKEN
    );
    assert_eq!(pending.expires_at, minutes_later(w.now(), 15));
}

// SocialLoginServiceTest#login_differentAppleTokenSubjects_throwsUnauthorizedException
#[test]
fn login_different_apple_token_subjects_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    given_verified_apple_id_token(&w);
    w.apple_client.exchange_returns(
        AUTHORIZATION_CODE,
        AppleTokenExchangeResult {
            id_token: EXCHANGED_ID_TOKEN.to_owned(),
            refresh_token: REFRESH_TOKEN.to_owned(),
        },
    );
    w.apple_verifier.will_return(
        EXCHANGED_ID_TOKEN,
        RAW_NONCE,
        World::apple_identity("different-apple-subject"),
    );

    assert_app_error(
        login_with_apple(&w),
        ErrorKind::Unauthorized,
        ErrorCode::UNAUTHORIZED,
        "Apple 로그인 사용자 정보가 일치하지 않습니다.",
    );
    // 암호화는 일치 확인 뒤에 하므로 아무것도 저장하지 않는다.
    assert_eq!(w.pending_count(), 0);
}

// SocialLoginServiceTest#login_withdrawnBannedAppleAccount_throwsForbiddenExceptionBeforeExchange
#[test]
fn login_withdrawn_banned_apple_account_throws_forbidden_exception_before_exchange() {
    let Some(w) = World::create() else { return };
    let banned = w.save_banned_user();
    w.withdraw_directly(banned.id);
    w.save_social_account(banned.id, SocialProvider::Apple, APPLE_SUBJECT);
    given_verified_apple_id_token(&w);

    assert_app_error(
        login_with_apple(&w),
        ErrorKind::Forbidden,
        ErrorCode::FORBIDDEN,
        "탈퇴한 차단 소셜 계정입니다.",
    );
    assert!(w.apple_client.exchanged().is_empty());
}

// SocialLoginServiceTest#login_appleTokenExchangeFailure_storesNoPendingAuthorization
#[test]
fn login_apple_token_exchange_failure_stores_no_pending_authorization() {
    let Some(w) = World::create() else { return };
    given_verified_apple_id_token(&w);
    w.apple_client
        .exchange_fails(AUTHORIZATION_CODE, "Apple 통신 실패");

    let error = login_with_apple(&w).unwrap_err();

    assert!(matches!(&error, ApiError::Internal(m) if m == "Apple 통신 실패"));
    assert!(w.latest_pending(APPLE_SUBJECT).is_none());
    assert_eq!(w.pending_count(), 0);
}

// SocialLoginServiceTest#login_newAppleAccountWithPendingAuthorization_skipsExchange
#[test]
fn login_new_apple_account_with_pending_authorization_skips_exchange() {
    let Some(w) = World::create() else { return };
    // 재로그인마다 교환하면 Apple에 폐기할 수 없는 grant가 하나씩 쌓인다.
    given_verified_apple_id_token(&w);
    w.save_pending(
        APPLE_SUBJECT,
        "stored-encrypted-token",
        minutes_later(w.now(), 1),
    );

    let result = login_with_apple(&w).unwrap();

    assert_eq!(result, SocialLoginResult::SignUpRequired);
    assert!(w.apple_client.exchanged().is_empty());
    assert_eq!(w.pending_count(), 1);
    assert_eq!(
        w.latest_pending(APPLE_SUBJECT)
            .unwrap()
            .encrypted_refresh_token,
        "stored-encrypted-token"
    );
}

// SocialLoginServiceTest#login_newAppleAccountWithPendingAuthorization_renewsExpiry
#[test]
fn login_new_apple_account_with_pending_authorization_renews_expiry() {
    let Some(w) = World::create() else { return };
    given_verified_apple_id_token(&w);
    w.save_pending(
        APPLE_SUBJECT,
        "stored-encrypted-token",
        minutes_later(w.now(), 1),
    );

    login_with_apple(&w).unwrap();

    assert_eq!(
        w.latest_pending(APPLE_SUBJECT).unwrap().expires_at,
        minutes_later(w.now(), 15)
    );
}

// SocialLoginServiceTest#login_newAppleAccountWithExpiredPendingAuthorization_exchangesAgain
#[test]
fn login_new_apple_account_with_expired_pending_authorization_exchanges_again() {
    let Some(w) = World::create() else { return };
    // 이전 행의 RT는 아직 Apple에 폐기되지 않았으므로 정리 스케줄러가 처리하도록 남긴다.
    given_successful_apple_authentication(&w);
    w.save_pending(
        APPLE_SUBJECT,
        "expired-encrypted-token",
        common::plus(w.now(), -1),
    );

    login_with_apple(&w).unwrap();

    assert_eq!(
        w.apple_client.exchanged(),
        vec![AUTHORIZATION_CODE.to_owned()]
    );
    assert_eq!(w.pending_count(), 2);
    let latest = w.latest_pending(APPLE_SUBJECT).unwrap();
    assert_eq!(
        w.cipher.decrypt(&latest.encrypted_refresh_token).unwrap(),
        REFRESH_TOKEN
    );
}

// ---------------------------------------------------------------------------------------------
// ExistingSocialAccountLoginProcessorTest
// ---------------------------------------------------------------------------------------------

const EXISTING_SUBJECT: &str = "social-subject";

fn existing_identity(
    provider: SocialProvider,
) -> chalkak_app::auth::domain::VerifiedSocialIdentity {
    chalkak_app::auth::domain::VerifiedSocialIdentity {
        provider,
        subject: EXISTING_SUBJECT.to_owned(),
        email: Some("user@chalkak.test".to_owned()),
    }
}

fn user_with_account(w: &World, provider: SocialProvider) -> chalkak_app::user::domain::User {
    let user = w.save_user();
    w.save_social_account(user.id, provider, EXISTING_SUBJECT);
    user
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_noSocialAccount_returnsEmpty
#[test]
fn process_if_exists_no_social_account_returns_empty() {
    let Some(w) = World::create() else { return };

    let result = w
        .existing
        .process_if_exists(&existing_identity(SocialProvider::Google))
        .unwrap();

    assert!(result.is_none());
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_withdrawnUser_returnsEmpty
#[test]
fn process_if_exists_withdrawn_user_returns_empty() {
    let Some(w) = World::create() else { return };
    let user = user_with_account(&w, SocialProvider::Google);
    w.withdrawal.withdraw(user.id).unwrap();

    let result = w
        .existing
        .process_if_exists(&existing_identity(SocialProvider::Google))
        .unwrap();

    assert!(result.is_none());
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_withdrawnBannedUser_throwsForbiddenException
#[test]
fn process_if_exists_withdrawn_banned_user_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let user = user_with_account(&w, SocialProvider::Google);
    w.ban(user.id);
    w.withdrawal.withdraw(user.id).unwrap();

    assert_app_error(
        w.existing
            .process_if_exists(&existing_identity(SocialProvider::Google)),
        ErrorKind::Forbidden,
        ErrorCode::FORBIDDEN,
        "탈퇴한 차단 소셜 계정입니다.",
    );
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_bannedUser_returnsLoginSuccess
#[test]
fn process_if_exists_banned_user_returns_login_success() {
    let Some(w) = World::create() else { return };
    let user = w.save_banned_user();
    w.save_social_account(user.id, SocialProvider::Google, EXISTING_SUBJECT);

    let result = w
        .existing
        .process_if_exists(&existing_identity(SocialProvider::Google))
        .unwrap();

    assert_eq!(result.expect("로그인된다").user_id, user.id);
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_activeUser_issuesAccessTokenAndRefreshToken
#[test]
fn process_if_exists_active_user_issues_access_token_and_refresh_token() {
    let Some(w) = World::create() else { return };
    let user = user_with_account(&w, SocialProvider::Google);

    let s = w
        .existing
        .process_if_exists(&existing_identity(SocialProvider::Google))
        .unwrap()
        .unwrap();

    assert_eq!(s.user_id, user.id);
    assert_eq!(
        access_token_subject(&s.access_token.value),
        user.id.to_string()
    );
    assert!(!s.refresh_token.value.trim().is_empty());
    assert_eq!(w.stored_refresh_hashes(user.id).len(), 1);
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_sameSubjectOtherProvider_returnsEmpty
#[test]
fn process_if_exists_same_subject_other_provider_returns_empty() {
    let Some(w) = World::create() else { return };
    user_with_account(&w, SocialProvider::Google);

    let result = w
        .existing
        .process_if_exists(&existing_identity(SocialProvider::Apple))
        .unwrap();

    assert!(result.is_none());
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_noSocialAccount_doesNotIssueRefreshToken
#[test]
fn process_if_exists_no_social_account_does_not_issue_refresh_token() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();

    w.existing
        .process_if_exists(&existing_identity(SocialProvider::Google))
        .unwrap();

    assert!(w.stored_refresh_hashes(user.id).is_empty());
}

// ExistingSocialAccountLoginProcessorTest#processIfExists_withdrawnBannedUser_doesNotIssueRefreshToken
#[test]
fn process_if_exists_withdrawn_banned_user_does_not_issue_refresh_token() {
    let Some(w) = World::create() else { return };
    let user = user_with_account(&w, SocialProvider::Google);
    w.ban(user.id);
    w.withdrawal.withdraw(user.id).unwrap();

    let result = w
        .existing
        .process_if_exists(&existing_identity(SocialProvider::Google));

    assert_app_error_message(result, ErrorKind::Forbidden, "탈퇴한 차단 소셜 계정입니다.");
    // 로그인을 거부하면 리프레시 토큰을 하나도 남기지 않는다.
    assert!(w.stored_refresh_hashes(user.id).is_empty());
}

// ---------------------------------------------------------------------------------------------
// AppleAuthorizationServiceTest (원본의 인증 정보 스냅샷 조회는 `AppleSocialConnectionRevoker`가 맡는다)
// ---------------------------------------------------------------------------------------------

// AppleAuthorizationServiceTest#findAuthorizationSnapshots_appleUser_returnsSnapshot
#[test]
fn find_authorization_snapshots_apple_user_returns_snapshot() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let account = w.save_social_account(user.id, SocialProvider::Apple, "apple-subject");
    let encrypted = w.cipher.encrypt("apple-refresh-token").unwrap();
    w.save_apple_authorization(account, &encrypted);
    let authorization_id = w.apple_authorizations(account)[0].id;

    let snapshots = w.revoker.revoke_all(user.id).unwrap();

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].authorization_id, authorization_id);
    assert_eq!(
        snapshots[0].credential_fingerprint,
        apple_authorization_fingerprint(&encrypted)
    );
    assert_eq!(
        w.apple_client.revoked(),
        vec!["apple-refresh-token".to_owned()]
    );
}

// AppleAuthorizationServiceTest#findAuthorizationSnapshots_nonAppleUser_returnsEmpty
#[test]
fn find_authorization_snapshots_non_apple_user_returns_empty() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, "apple-subject");

    let snapshots = w.revoker.revoke_all(user.id).unwrap();

    assert!(snapshots.is_empty());
    assert!(w.apple_client.revoked().is_empty());
}

// AppleAuthorizationServiceTest#findAuthorizationSnapshots_userWithoutSocialAccount_returnsEmpty
#[test]
fn find_authorization_snapshots_user_without_social_account_returns_empty() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();

    assert!(w.revoker.revoke_all(user.id).unwrap().is_empty());
}

// AppleAuthorizationServiceTest#findAuthorizationSnapshots_notExistingUser_returnsEmpty
#[test]
fn find_authorization_snapshots_not_existing_user_returns_empty() {
    let Some(w) = World::create() else { return };

    assert!(w.revoker.revoke_all(Uuid::new_v4()).unwrap().is_empty());
}
