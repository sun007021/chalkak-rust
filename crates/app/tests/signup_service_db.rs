//! 소셜 회원가입 서비스의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `SocialSignupServiceTest`, `SocialSignupRetryTest`를 옮겼다. 원본은 회원가입 토큰 발급·검증기를 Mockito로
//! 바꿨지만 여기서는 실제 `SignupTokens`(서명한 JWT)를 쓴다. 같은 토큰 문자열을 다시 내면 같은 jti가 재전송된다.

mod common;

use chalkak_app::auth::domain::SocialProvider;
use chalkak_app::user::UserRepository;
use chalkak_app::user::domain::User;
use chalkak_core::error::{ErrorCode, ErrorKind};
use common::{
    PRESIGNED_URL, World, access_token_subject, assert_app_error, assert_lower_hex_64, image,
    keys_for, minutes_later, png,
};
use uuid::Uuid;

const ID_TOKEN: &str = "google-id-token";
const RAW_NONCE: &str = "raw-nonce";
const SUBJECT: &str = "google-subject";
const EMAIL: &str = "user@chalkak.test";
const APPLE_ID_TOKEN: &str = "apple-id-token";
const APPLE_SUBJECT: &str = "apple-subject";
const APPLE_PENDING_MESSAGE: &str =
    "Apple 회원가입 인증 정보가 만료되었거나 없습니다. 다시 로그인해 주세요.";

fn identity(email: Option<&str>) -> chalkak_app::auth::domain::VerifiedSocialIdentity {
    World::google_identity(SUBJECT, email)
}

/// 이미지가 올라왔고 처리도 끝난 업로드.
fn processed_upload(w: &World) -> Uuid {
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    w.storage.processing_completed(upload_id, &[true]);
    upload_id
}

fn encrypted_apple_token(w: &World) -> String {
    w.cipher.encrypt("apple-refresh-token").unwrap()
}

fn pending_expiry(w: &World) -> jiff::Timestamp {
    minutes_later(w.now(), 5)
}

// ---------------------------------------------------------------------------------------------
// signup
// ---------------------------------------------------------------------------------------------

// SocialSignupServiceTest#signup_completedSignature_savesUserAndSocialAccount
#[test]
fn signup_completed_signature_saves_user_and_social_account() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);

    let result = w.signup.signup(&token).unwrap();

    let user = w.find_user(result.user_id);
    let keys = keys_for(upload_id);
    assert_eq!(user.email.as_deref(), Some(EMAIL));
    assert_eq!(user.signature_original_storage_key, keys.original);
    assert_eq!(
        user.signature_thumbnail_storage_key.as_deref(),
        Some(keys.thumbnail.as_str())
    );
    let (account, account_user) = w
        .find_social_account(SocialProvider::Google, SUBJECT)
        .expect("소셜 계정이 있다");
    assert_eq!(account_user.id, result.user_id);
    assert_eq!(
        account.subject_hmac,
        w.subject_hmac(SocialProvider::Google, SUBJECT)
    );
    let stored = w.stored_refresh_hashes(result.user_id);
    assert_eq!(stored.len(), 1);
    assert_lower_hex_64(&stored[0]);
}

// SocialSignupServiceTest#signup_appleSignupToken_savesAppleAuthorization
#[test]
fn signup_apple_signup_token_saves_apple_authorization() {
    let Some(w) = World::create() else { return };
    let encrypted = encrypted_apple_token(&w);
    w.save_pending(APPLE_SUBJECT, &encrypted, pending_expiry(&w));
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&World::apple_identity(APPLE_SUBJECT), upload_id);

    w.signup.signup(&token).unwrap();

    let (account, _) = w
        .find_social_account(SocialProvider::Apple, APPLE_SUBJECT)
        .expect("Apple 소셜 계정이 있다");
    let authorizations = w.apple_authorizations(account.id);
    assert_eq!(authorizations[0].encrypted_refresh_token, encrypted);
    assert!(w.latest_pending(APPLE_SUBJECT).is_none());
}

// SocialSignupServiceTest#signup_missingPendingAppleAuthorization_throwsBusinessException
#[test]
fn signup_missing_pending_apple_authorization_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&World::apple_identity(APPLE_SUBJECT), upload_id);

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        APPLE_PENDING_MESSAGE,
    );
    // 실패한 가입은 회원도 소셜 계정도 소진 기록도 남기지 않는다.
    assert_eq!(w.count("SELECT COUNT(*) FROM users"), 0);
    assert_eq!(w.count("SELECT COUNT(*) FROM social_accounts"), 0);
    assert_eq!(w.count("SELECT COUNT(*) FROM consumed_signup_tokens"), 0);
}

// SocialSignupServiceTest#signup_missingSignature_keepsPendingAppleAuthorization
#[test]
fn signup_missing_signature_keeps_pending_apple_authorization() {
    let Some(w) = World::create() else { return };
    w.save_pending(
        APPLE_SUBJECT,
        &encrypted_apple_token(&w),
        pending_expiry(&w),
    );
    let token = w.signup_token(&World::apple_identity(APPLE_SUBJECT), Uuid::new_v4());

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::NotFound,
        ErrorCode::BUSINESS_ERROR,
        "업로드한 사인 이미지를 찾을 수 없습니다.",
    );
    assert!(w.latest_pending(APPLE_SUBJECT).is_some());
}

// SocialSignupServiceTest#signup_processingSignature_throwsProcessingPendingError
#[test]
fn signup_processing_signature_throws_processing_pending_error() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    w.storage.processing_completed(upload_id, &[false]);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::SIGNATURE_PROCESSING_PENDING,
        "사인 이미지 처리 중입니다.",
    );
}

// SocialSignupServiceTest#signup_existingActiveSocialAccount_returnsExistingUserId
#[test]
fn signup_existing_active_social_account_returns_existing_user_id() {
    let Some(w) = World::create() else { return };
    let existing = w.save_user();
    w.save_social_account(existing.id, SocialProvider::Google, SUBJECT);
    let token = w.signup_token(&identity(Some(EMAIL)), Uuid::new_v4());

    let result = w.signup.signup(&token).unwrap();

    assert_eq!(result.user_id, existing.id);
}

// SocialSignupServiceTest#signup_withdrawnSocialAccount_createsNewUser
#[test]
fn signup_withdrawn_social_account_creates_new_user() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);
    let withdrawn = w.save_user();
    w.save_social_account(withdrawn.id, SocialProvider::Google, SUBJECT);
    w.withdrawal.withdraw(withdrawn.id).unwrap();

    let new_user_id = w.signup.signup(&token).unwrap().user_id;

    let old_user = w.find_user(withdrawn.id);
    let new_user = w.find_user(new_user_id);
    let (_, account_user) = w
        .find_social_account(SocialProvider::Google, SUBJECT)
        .unwrap();
    assert_ne!(new_user_id, withdrawn.id);
    assert!(old_user.is_deleted());
    assert_eq!(
        old_user.email.as_deref(),
        Some(format!("withdrawn+{}@chalkak.invalid", withdrawn.id).as_str())
    );
    assert_eq!(
        old_user.signature_original_storage_key,
        format!("withdrawn/{}", withdrawn.id)
    );
    assert_eq!(new_user.email.as_deref(), Some(EMAIL));
    assert_eq!(account_user.id, new_user_id);
}

// SocialSignupServiceTest#signup_replayedTokenAfterWithdrawal_throwsBusinessException
#[test]
fn signup_replayed_token_after_withdrawal_throws_business_exception() {
    let Some(w) = World::create() else { return };
    // 회원가입 토큰은 서버가 상태를 추적하지 않는 JWT라 만료 전까지 몇 번이든 검증을 통과한다. 같은 토큰(=같은 jti)을
    // 두 번째 signup 호출에도 그대로 내 재전송을 흉내낸다.
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);
    let first_user_id = w.signup.signup(&token).unwrap().user_id;
    w.withdrawal.withdraw(first_user_id).unwrap();

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "이미 사용된 회원가입 토큰입니다.",
    );
}

// SocialSignupServiceTest#signup_appleTokenReplayedAfterWithdrawal_doesNotResurrectAuthorization
#[test]
fn signup_apple_token_replayed_after_withdrawal_does_not_resurrect_authorization() {
    let Some(w) = World::create() else { return };
    w.save_pending(
        APPLE_SUBJECT,
        &encrypted_apple_token(&w),
        pending_expiry(&w),
    );
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&World::apple_identity(APPLE_SUBJECT), upload_id);
    let first_user_id = w.signup.signup(&token).unwrap().user_id;
    let (account, _) = w
        .find_social_account(SocialProvider::Apple, APPLE_SUBJECT)
        .unwrap();

    w.withdrawal.withdraw(first_user_id).unwrap();

    // 탈퇴 직후 상태: 소셜 계정과 인증 정보가 없고 Apple에 폐기 요청이 나갔다.
    assert!(
        w.find_social_account(SocialProvider::Apple, APPLE_SUBJECT)
            .is_none()
    );
    assert!(w.apple_authorizations(account.id).is_empty());
    assert_eq!(
        w.apple_client.revoked(),
        vec!["apple-refresh-token".to_owned()]
    );
    // 같은 토큰을 재전송하면 이미 폐기를 요청한 RT가 새 계정에 되살아나는 대신 거부된다.
    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "이미 사용된 회원가입 토큰입니다.",
    );
}

// SocialSignupServiceTest#signup_bannedSocialAccount_throwsForbiddenException
#[test]
fn signup_banned_social_account_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let banned = w.save_banned_user();
    w.save_social_account(banned.id, SocialProvider::Google, SUBJECT);
    let token = w.signup_token(&identity(Some(EMAIL)), Uuid::new_v4());

    let error = common::app_error(w.signup.signup(&token).unwrap_err());

    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Forbidden, ErrorCode::FORBIDDEN)
    );
}

// SocialSignupServiceTest#signup_withdrawnBannedUser_throwsForbiddenException
#[test]
fn signup_withdrawn_banned_user_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);
    w.ban(user.id);
    w.withdraw_directly(user.id);
    let interactions = w.storage.interactions();

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Forbidden,
        ErrorCode::FORBIDDEN,
        "차단된 소셜 계정입니다.",
    );
    assert_eq!(w.storage.interactions(), interactions);
}

// SocialSignupServiceTest#signup_withdrawnUserWithSocialAccount_throwsBusinessException
#[test]
fn signup_withdrawn_user_with_social_account_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let token = w.signup_token(&identity(Some(EMAIL)), Uuid::new_v4());
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);
    w.withdraw_directly(user.id);
    let interactions = w.storage.interactions();

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "이미 가입된 소셜 계정입니다.",
    );
    assert_eq!(w.storage.interactions(), interactions);
}

// SocialSignupServiceTest#signup_usedSignatureUploadId_throwsNotFoundException
#[test]
fn signup_used_signature_upload_id_throws_not_found_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    UserRepository
        .insert(
            &mut w.db.client(),
            &User::create(
                Some("existing@chalkak.test".to_owned()),
                &keys_for(upload_id),
            )
            .unwrap(),
            w.now(),
        )
        .unwrap();
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::NotFound,
        ErrorCode::BUSINESS_ERROR,
        "업로드한 사인 이미지를 찾을 수 없습니다.",
    );
}

// SocialSignupServiceTest#signup_missingSignatureImage_throwsNotFoundException
#[test]
fn signup_missing_signature_image_throws_not_found_exception() {
    let Some(w) = World::create() else { return };
    let token = w.signup_token(&identity(Some(EMAIL)), Uuid::new_v4());

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::NotFound,
        ErrorCode::BUSINESS_ERROR,
        "업로드한 사인 이미지를 찾을 수 없습니다.",
    );
}

// SocialSignupServiceTest#signup_invalidSignatureImage_throwsBusinessException
#[test]
fn signup_invalid_signature_image_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, image("image/jpeg", 1024));
    w.storage.processing_completed(upload_id, &[true]);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);

    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "사용할 수 없는 사인 이미지입니다.",
    );
}

// SocialSignupServiceTest#signup_nullEmail_savesUserWithoutEmail
#[test]
fn signup_null_email_saves_user_without_email() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&identity(None), upload_id);

    let user_id = w.signup.signup(&token).unwrap().user_id;

    assert_eq!(w.find_user(user_id).email, None);
}

// SocialSignupServiceTest#signup_completedSignature_issuesAccessToken
#[test]
fn signup_completed_signature_issues_access_token() {
    let Some(w) = World::create() else { return };
    let upload_id = processed_upload(&w);
    let token = w.signup_token(&identity(Some(EMAIL)), upload_id);

    let result = w.signup.signup(&token).unwrap();

    assert_eq!(
        access_token_subject(&result.access_token.value),
        result.user_id.to_string()
    );
    assert!(result.access_token.expires_in_seconds > 0);
}

// ---------------------------------------------------------------------------------------------
// createSignatureUpload
// ---------------------------------------------------------------------------------------------

fn google_upload(
    w: &World,
) -> Result<chalkak_app::auth::service::SocialSignupSignatureUploadResult, chalkak_app::web::ApiError>
{
    w.google
        .will_return(ID_TOKEN, RAW_NONCE, identity(Some(EMAIL)));
    w.signup
        .create_signature_upload(SocialProvider::Google, ID_TOKEN, RAW_NONCE)
}

fn apple_upload(
    w: &World,
) -> Result<chalkak_app::auth::service::SocialSignupSignatureUploadResult, chalkak_app::web::ApiError>
{
    w.apple_verifier.will_return(
        APPLE_ID_TOKEN,
        RAW_NONCE,
        World::apple_identity(APPLE_SUBJECT),
    );
    w.signup
        .create_signature_upload(SocialProvider::Apple, APPLE_ID_TOKEN, RAW_NONCE)
}

// SocialSignupServiceTest#createSignatureUpload_newSocialAccount_issuesUploadUrl
#[test]
fn create_signature_upload_new_social_account_issues_upload_url() {
    let Some(w) = World::create() else { return };

    let result = google_upload(&w).unwrap();

    assert_ne!(result.upload.upload_id, Uuid::nil());
    assert_eq!(result.upload.upload_url, PRESIGNED_URL);
    assert_eq!(result.upload.expires_in_seconds, 300);
    // 회원가입 토큰은 검증한 신원과 이 업로드 식별자를 담는다.
    let verified = w.signup_tokens.verify(&result.signup_token.value).unwrap();
    assert_eq!(verified.provider, SocialProvider::Google);
    assert_eq!(verified.subject, SUBJECT);
    assert_eq!(verified.email.as_deref(), Some(EMAIL));
    assert_eq!(verified.upload_id, result.upload.upload_id);
}

// SocialSignupServiceTest#createSignatureUpload_appleWithPendingAuthorization_issuesUploadUrl
#[test]
fn create_signature_upload_apple_with_pending_authorization_issues_upload_url() {
    let Some(w) = World::create() else { return };
    w.save_pending(
        APPLE_SUBJECT,
        "encrypted-apple-refresh-token",
        minutes_later(w.now(), 3),
    );

    let result = apple_upload(&w).unwrap();

    assert_eq!(result.upload.upload_url, PRESIGNED_URL);
    assert!(!result.signup_token.value.is_empty());
}

// SocialSignupServiceTest#createSignatureUpload_appleWithoutPendingAuthorization_throwsBusinessException
#[test]
fn create_signature_upload_apple_without_pending_authorization_throws_business_exception() {
    let Some(w) = World::create() else { return };

    assert_app_error(
        apple_upload(&w),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        APPLE_PENDING_MESSAGE,
    );
    assert_eq!(w.issuer.interactions(), 0);
}

// SocialSignupServiceTest#createSignatureUpload_appleWithExpiredPendingAuthorization_throwsBusinessException
#[test]
fn create_signature_upload_apple_with_expired_pending_authorization_throws_business_exception() {
    let Some(w) = World::create() else { return };
    w.save_pending(
        APPLE_SUBJECT,
        "encrypted-apple-refresh-token",
        common::plus(w.now(), -1),
    );

    assert_app_error(
        apple_upload(&w),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        APPLE_PENDING_MESSAGE,
    );
    assert_eq!(w.issuer.interactions(), 0);
}

// SocialSignupServiceTest#createSignatureUpload_applePendingExpiresFirst_extendsPendingExpiry
#[test]
fn create_signature_upload_apple_pending_expires_first_extends_pending_expiry() {
    let Some(w) = World::create() else { return };
    // 연장하지 않으면 회원가입 토큰은 살아 있는데 임시 인증 정보가 먼저 사라져 가입 완료가 실패한다.
    w.save_pending(
        APPLE_SUBJECT,
        "encrypted-apple-refresh-token",
        minutes_later(w.now(), 1),
    );

    let result = apple_upload(&w).unwrap();

    assert_eq!(
        w.latest_pending(APPLE_SUBJECT).unwrap().expires_at,
        result.signup_token.expires_at
    );
    assert_eq!(result.signup_token.expires_at, minutes_later(w.now(), 5));
}

// SocialSignupServiceTest#createSignatureUpload_applePendingExpiresLater_keepsPendingExpiry
#[test]
fn create_signature_upload_apple_pending_expires_later_keeps_pending_expiry() {
    let Some(w) = World::create() else { return };
    let pending_expires_at = minutes_later(w.now(), 15);
    w.save_pending(
        APPLE_SUBJECT,
        "encrypted-apple-refresh-token",
        pending_expires_at,
    );

    apple_upload(&w).unwrap();

    assert_eq!(
        w.latest_pending(APPLE_SUBJECT).unwrap().expires_at,
        pending_expires_at
    );
}

// SocialSignupServiceTest#createSignatureUpload_existingSocialAccount_throwsBusinessException
#[test]
fn create_signature_upload_existing_social_account_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);

    assert_app_error(
        google_upload(&w),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "이미 가입된 소셜 계정입니다.",
    );
}

// SocialSignupServiceTest#createSignatureUpload_withdrawnUserWithSocialAccount_throwsBusinessException
#[test]
fn create_signature_upload_withdrawn_user_with_social_account_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);
    w.withdraw_directly(user.id);

    assert_app_error(
        google_upload(&w),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "이미 가입된 소셜 계정입니다.",
    );
    assert_eq!(w.issuer.interactions(), 0);
}

// SocialSignupServiceTest#createSignatureUpload_bannedSocialAccount_throwsForbiddenException
#[test]
fn create_signature_upload_banned_social_account_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let banned = w.save_banned_user();
    w.save_social_account(banned.id, SocialProvider::Google, SUBJECT);

    let error = common::app_error(google_upload(&w).unwrap_err());

    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Forbidden, ErrorCode::FORBIDDEN)
    );
}

// SocialSignupServiceTest#createSignatureUpload_withdrawnBannedUser_throwsForbiddenException
#[test]
fn create_signature_upload_withdrawn_banned_user_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, SUBJECT);
    w.ban(user.id);
    w.withdraw_directly(user.id);

    assert_app_error(
        google_upload(&w),
        ErrorKind::Forbidden,
        ErrorCode::FORBIDDEN,
        "차단된 소셜 계정입니다.",
    );
    assert_eq!(w.issuer.interactions(), 0);
}

// ---------------------------------------------------------------------------------------------
// SocialSignupRetryTest
// ---------------------------------------------------------------------------------------------

// SocialSignupRetryTest#signup_retryAfterProcessingPending_completesWithSameToken
#[test]
fn signup_retry_after_processing_pending_completes_with_same_token() {
    let Some(w) = World::create() else { return };
    // 가입 완료가 실패하면 소진 기록도 함께 롤백되어 같은 토큰으로 다시 시도할 수 있어야 한다.
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    w.storage.processing_completed(upload_id, &[false, true]);
    let token = w.signup_token(
        &World::google_identity("google-retry-subject", Some("retry@chalkak.test")),
        upload_id,
    );
    let jti = w.signup_tokens.verify(&token).unwrap().token_id;
    assert_app_error(
        w.signup.signup(&token),
        ErrorKind::Business,
        ErrorCode::SIGNATURE_PROCESSING_PENDING,
        "사인 이미지 처리 중입니다.",
    );
    assert_eq!(w.consumed_token_count(&jti), 0);

    let user_id = w.signup.signup(&token).unwrap().user_id;

    assert_ne!(user_id, Uuid::nil());
    assert_eq!(w.consumed_token_count(&jti), 1);
}
