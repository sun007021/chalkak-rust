//! 회원 서비스·탈퇴 서비스의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `UserServiceTest`, `UserWithdrawalServiceTest`, `UserWithdrawalTransactionTest`, `UserRepositoryTest`를 옮겼다.
//! 시각은 `FixedClock`이라 원본의 "8분 전"은 시계 기준 8분 전이다.

mod common;

use chalkak_app::auth::domain::SocialProvider;
use chalkak_app::auth::repository::AppleAuthorizationRepository;
use chalkak_app::auth::tokens::apple_authorization_fingerprint;
use chalkak_app::user::UserRepository;
use chalkak_app::user::domain::{
    SignatureProcessingStatus, SignatureStorageKeys, User, UserStatus,
};
use chalkak_app::user::service::SocialConnectionRevocationSnapshot;
use chalkak_app::web::ApiError;
use chalkak_core::error::{ErrorCode, ErrorKind};
use common::{PRESIGNED_URL, World, assert_app_error, image, keys_for, plus, png};
use postgres::NoTls;
use uuid::Uuid;

const UNAUTHORIZED_MESSAGE: &str = "유효하지 않은 인증 정보입니다.";
const NOT_FOUND_MESSAGE: &str = "업로드한 사인 이미지를 찾을 수 없습니다.";
const REUPLOAD_MESSAGE: &str =
    "사인 이미지 처리에 실패했습니다. 새로운 업로드 ID를 발급받아 이미지를 다시 업로드해 주세요.";

fn cdn(key: &str) -> String {
    format!("https://cdn.test.chalkak/{key}")
}

/// 이미지가 올라온 새 업로드(처리는 아직).
fn uploaded(w: &World) -> Uuid {
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, png());
    upload_id
}

/// `user.startSignatureProcessing(uploadId, startedAt)` 상태의 회원.
fn processing_user(w: &World, upload_id: Uuid, started_at: jiff::Timestamp) -> User {
    let user = w.save_user();
    w.modify_user(user.id, |u| {
        u.start_signature_processing(upload_id, started_at).unwrap();
    });
    user
}

fn expect_unauthorized<T: std::fmt::Debug>(result: Result<T, ApiError>) {
    assert_app_error(
        result,
        ErrorKind::Unauthorized,
        ErrorCode::UNAUTHORIZED,
        UNAUTHORIZED_MESSAGE,
    );
}

// ---------------------------------------------------------------------------------------------
// 탈퇴 완료(completeWithdrawal)
// ---------------------------------------------------------------------------------------------

// UserServiceTest#withdraw_activeUser_anonymizesPersonalData
#[test]
fn withdraw_active_user_anonymizes_personal_data() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;

    w.users.complete_withdrawal(id, &[]).unwrap();

    let withdrawn = w.find_user(id);
    assert_eq!(
        withdrawn.email.as_deref(),
        Some(format!("withdrawn+{id}@chalkak.invalid").as_str())
    );
    assert_eq!(
        withdrawn.signature_original_storage_key,
        format!("withdrawn/{id}")
    );
    assert_eq!(withdrawn.signature_thumbnail_storage_key, None);
    assert!(withdrawn.is_deleted());
}

// UserServiceTest#withdraw_activeUser_deletesSocialAccount
#[test]
fn withdraw_active_user_deletes_social_account() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, "google-subject");

    w.users.complete_withdrawal(user.id, &[]).unwrap();

    assert!(
        w.find_social_account(SocialProvider::Google, "google-subject")
            .is_none()
    );
}

// UserServiceTest#withdraw_activeUser_excludesFromActiveQuery
#[test]
fn withdraw_active_user_excludes_from_active_query() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;

    w.users.complete_withdrawal(id, &[]).unwrap();

    assert!(w.find_active_user(id).is_none());
    assert!(
        UserRepository
            .find_by_id(&mut w.db.client(), id)
            .unwrap()
            .is_some()
    );
}

// UserServiceTest#withdraw_activeUser_revokesEveryRefreshTokenOfThatUser
#[test]
fn withdraw_active_user_revokes_every_refresh_token_of_that_user() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let other = w.save_user();
    let first = w.refresh.issue(&mut w.db.client(), user.id).unwrap();
    let second = w.refresh.issue(&mut w.db.client(), user.id).unwrap();
    let other_device = w.refresh.issue(&mut w.db.client(), other.id).unwrap();

    w.users.complete_withdrawal(user.id, &[]).unwrap();

    assert_eq!(w.live_refresh_token_count(user.id), 0);
    assert!(w.refresh_token_revoked_at(&first.value).is_some());
    assert!(w.refresh_token_revoked_at(&second.value).is_some());
    assert!(w.refresh_token_revoked_at(&other_device.value).is_none());
}

// UserServiceTest#withdraw_activeUser_blocksRefreshWithIssuedToken
#[test]
fn withdraw_active_user_blocks_refresh_with_issued_token() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let issued = w.refresh.issue(&mut w.db.client(), user.id).unwrap();
    w.users.complete_withdrawal(user.id, &[]).unwrap();

    assert_app_error(
        w.refresh.refresh(&issued.value),
        ErrorKind::Unauthorized,
        ErrorCode::REAUTHENTICATION_REQUIRED,
        "다시 로그인해 주세요.",
    );
}

// UserServiceTest#withdraw_notExistingUser_throwsUnauthorizedException
#[test]
fn withdraw_not_existing_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };

    expect_unauthorized(w.users.complete_withdrawal(Uuid::new_v4(), &[]));
}

// UserServiceTest#withdraw_alreadyWithdrawnUser_throwsUnauthorizedException
#[test]
fn withdraw_already_withdrawn_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    w.users.complete_withdrawal(id, &[]).unwrap();

    expect_unauthorized(w.users.complete_withdrawal(id, &[]));
}

// UserServiceTest#withdraw_bannedUser_preservesSocialAccount
#[test]
fn withdraw_banned_user_preserves_social_account() {
    let Some(w) = World::create() else { return };
    let user = w.save_banned_user();
    w.save_social_account(user.id, SocialProvider::Google, "banned-google-subject");

    w.users.complete_withdrawal(user.id, &[]).unwrap();

    assert!(w.find_active_user(user.id).is_none());
    let (_, account_user) = w
        .find_social_account(SocialProvider::Google, "banned-google-subject")
        .expect("재가입 차단을 위해 소셜 계정을 유지한다");
    assert_eq!(account_user.id, user.id);
    assert!(account_user.is_deleted());
    assert_eq!(account_user.status, UserStatus::Banned);
}

fn snapshot_for(
    w: &World,
    social_account_id: Uuid,
    encrypted: &str,
) -> SocialConnectionRevocationSnapshot {
    SocialConnectionRevocationSnapshot {
        authorization_id: w.apple_authorizations(social_account_id)[0].id,
        credential_fingerprint: apple_authorization_fingerprint(encrypted),
    }
}

// UserServiceTest#withdraw_appleUser_deletesAppleAuthorization
#[test]
fn withdraw_apple_user_deletes_apple_authorization() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let account = w.save_social_account(user.id, SocialProvider::Apple, "apple-subject");
    w.save_apple_authorization(account, "encrypted-refresh-token");
    let snapshot = snapshot_for(&w, account, "encrypted-refresh-token");

    w.users.complete_withdrawal(user.id, &[snapshot]).unwrap();

    assert!(w.apple_authorizations(account).is_empty());
}

// UserServiceTest#withdraw_authorizationSnapshotMismatch_throwsBusinessExceptionWithoutDeleting
#[test]
fn withdraw_authorization_snapshot_mismatch_throws_business_exception_without_deleting() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let account = w.save_social_account(user.id, SocialProvider::Apple, "apple-subject");
    w.save_apple_authorization(account, "encrypted-refresh-token");

    // 실제로는 인증 정보가 하나 저장돼 있는데 아무것도 폐기하지 않은 것으로(빈 스냅샷) 탈퇴를 시도한다.
    assert_app_error(
        w.users.complete_withdrawal(user.id, &[]),
        ErrorKind::Business,
        ErrorCode::RESOURCE_STATE_CHANGED,
        "탈퇴 처리 중 Apple 인증 정보가 변경되었습니다. 다시 시도해 주세요.",
    );

    assert!(w.find_active_user(user.id).is_some());
    assert_eq!(w.apple_authorizations(account).len(), 1);
}

// UserServiceTest#withdraw_bannedAppleUser_deletesAppleAuthorizationOnly
#[test]
fn withdraw_banned_apple_user_deletes_apple_authorization_only() {
    let Some(w) = World::create() else { return };
    let user = w.save_banned_user();
    let account = w.save_social_account(user.id, SocialProvider::Apple, "banned-apple-subject");
    w.save_apple_authorization(account, "encrypted-refresh-token");
    let snapshot = snapshot_for(&w, account, "encrypted-refresh-token");

    w.users.complete_withdrawal(user.id, &[snapshot]).unwrap();

    assert!(w.apple_authorizations(account).is_empty());
    assert!(
        w.find_social_account(SocialProvider::Apple, "banned-apple-subject")
            .is_some()
    );
}

// ---------------------------------------------------------------------------------------------
// 사인 업로드 URL 발급
// ---------------------------------------------------------------------------------------------

// UserServiceTest#createSignatureUpload_activeUser_issuesUploadUrl
#[test]
fn create_signature_upload_active_user_issues_upload_url() {
    let Some(w) = World::create() else { return };
    let user_id = w.save_user().id;

    let upload = w.users.create_signature_upload(user_id).unwrap();

    assert_ne!(upload.upload_id, Uuid::nil());
    assert_eq!(upload.upload_url, PRESIGNED_URL);
    assert_eq!(upload.expires_in_seconds, 300);
}

// UserServiceTest#createSignatureUpload_notExistingUser_throwsUnauthorizedException
#[test]
fn create_signature_upload_not_existing_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };

    expect_unauthorized(w.users.create_signature_upload(Uuid::new_v4()));
    assert_eq!(w.issuer.interactions(), 0);
}

// UserServiceTest#createSignatureUpload_withdrawnUser_throwsUnauthorizedException
#[test]
fn create_signature_upload_withdrawn_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let user_id = w.save_user().id;
    w.users.complete_withdrawal(user_id, &[]).unwrap();

    expect_unauthorized(w.users.create_signature_upload(user_id));
    assert_eq!(w.issuer.interactions(), 0);
}

// ---------------------------------------------------------------------------------------------
// 사인 조회
// ---------------------------------------------------------------------------------------------

// UserServiceTest#getSignature_withoutPending_returnsActiveSignatureUrls
#[test]
fn get_signature_without_pending_returns_active_signature_urls() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();

    let result = w.users.get_signature(saved.id).unwrap();

    assert_eq!(
        result.original_image_url,
        cdn(&saved.signature_original_storage_key)
    );
    assert_eq!(
        result.thumbnail_image_url,
        cdn(saved.signature_thumbnail_storage_key.as_deref().unwrap())
    );
}

// UserServiceTest#getSignature_timedOutPending_keepsProcessingAndThrowsBusinessException
#[test]
fn get_signature_timed_out_pending_keeps_processing_and_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user = processing_user(&w, upload_id, plus(w.now(), -8 * 60));

    assert_app_error(
        w.users.get_signature(user.id),
        ErrorKind::Business,
        ErrorCode::SIGNATURE_REGISTRATION_REQUIRED,
        "사인 이미지 처리에 실패했습니다. 사인을 다시 등록해 주세요.",
    );

    let updated = w.find_user(user.id);
    assert_eq!(updated.pending_signature_upload_id, Some(upload_id));
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
    assert!(updated.signature_processing_started_at.is_some());
}

// UserServiceTest#getSignature_processingWithinTimeout_returnsActiveSignatureUrls
#[test]
fn get_signature_processing_within_timeout_returns_active_signature_urls() {
    let Some(w) = World::create() else { return };
    let user = processing_user(&w, Uuid::new_v4(), plus(w.now(), -6 * 60));

    let result = w.users.get_signature(user.id).unwrap();

    assert_eq!(
        result.original_image_url,
        cdn(&user.signature_original_storage_key)
    );
    assert_eq!(
        result.thumbnail_image_url,
        cdn(user.signature_thumbnail_storage_key.as_deref().unwrap())
    );
    assert_eq!(
        w.find_user(user.id).signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
}

// UserServiceTest#getSignature_failedPending_throwsBusinessException
#[test]
fn get_signature_failed_pending_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user = processing_user(&w, upload_id, w.now());
    w.modify_user(user.id, |u| {
        u.fail_signature_processing(upload_id);
    });

    let error = common::app_error(w.users.get_signature(user.id).unwrap_err());

    assert_eq!(
        (error.kind, error.code),
        (
            ErrorKind::Business,
            ErrorCode::SIGNATURE_REGISTRATION_REQUIRED
        )
    );
}

// UserServiceTest#getSignature_notExistingUser_throwsUnauthorizedException
#[test]
fn get_signature_not_existing_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };

    expect_unauthorized(w.users.get_signature(Uuid::new_v4()));
}

// ---------------------------------------------------------------------------------------------
// 사인 변경과 처리 콜백
// ---------------------------------------------------------------------------------------------

// UserServiceTest#updateSignature_uploadedImage_startsProcessingAndPreservesActiveSignature
#[test]
fn update_signature_uploaded_image_starts_processing_and_preserves_active_signature() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let upload_id = uploaded(&w);

    let image_url = w.users.update_signature(saved.id, upload_id).unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        saved.signature_thumbnail_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, Some(upload_id));
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
    assert!(updated.signature_processing_started_at.is_some());
    assert_eq!(image_url, cdn(&saved.signature_original_storage_key));
}

// UserServiceTest#updateSignature_alreadyProcessedImage_promotesImmediately
#[test]
fn update_signature_already_processed_image_promotes_immediately() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    let upload_id = uploaded(&w);
    w.storage.processing_completed(upload_id, &[true]);
    let keys = keys_for(upload_id);

    let image_url = w.users.update_signature(id, upload_id).unwrap();

    let updated = w.find_user(id);
    assert_eq!(updated.signature_original_storage_key, keys.original);
    assert_eq!(
        updated.signature_thumbnail_storage_key.as_deref(),
        Some(keys.thumbnail.as_str())
    );
    assert_eq!(updated.pending_signature_upload_id, None);
    assert_eq!(updated.signature_processing_status, None);
    assert_eq!(updated.signature_processing_started_at, None);
    assert_eq!(image_url, cdn(&keys.original));
}

// UserServiceTest#completeSignatureProcessing_matchingPending_promotesSignature
#[test]
fn complete_signature_processing_matching_pending_promotes_signature() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    let upload_id = uploaded(&w);
    let keys = keys_for(upload_id);
    w.users.update_signature(id, upload_id).unwrap();

    w.users.complete_signature_processing(upload_id).unwrap();

    let updated = w.find_user(id);
    assert_eq!(updated.signature_original_storage_key, keys.original);
    assert_eq!(
        updated.signature_thumbnail_storage_key.as_deref(),
        Some(keys.thumbnail.as_str())
    );
    assert_eq!(updated.pending_signature_upload_id, None);
    assert_eq!(updated.signature_processing_status, None);
    assert_eq!(updated.signature_processing_started_at, None);
}

// UserServiceTest#completeSignatureProcessing_timedOutPending_ignoresCallback
#[test]
fn complete_signature_processing_timed_out_pending_ignores_callback() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user = processing_user(&w, upload_id, plus(w.now(), -8 * 60));

    w.users.complete_signature_processing(upload_id).unwrap();

    let updated = w.find_user(user.id);
    assert_eq!(
        updated.signature_original_storage_key,
        user.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        user.signature_thumbnail_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, Some(upload_id));
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
    assert!(updated.signature_processing_started_at.is_some());
}

// UserServiceTest#failSignatureProcessing_matchingPending_preservesActiveSignature
#[test]
fn fail_signature_processing_matching_pending_preserves_active_signature() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let upload_id = uploaded(&w);
    w.users.update_signature(saved.id, upload_id).unwrap();

    w.users.fail_signature_processing(upload_id).unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        saved.signature_thumbnail_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, Some(upload_id));
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Failed)
    );
    assert!(updated.signature_processing_started_at.is_some());
}

// UserServiceTest#completeSignatureProcessing_staleUpload_ignoresCallback
#[test]
fn complete_signature_processing_stale_upload_ignores_callback() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();

    w.users
        .complete_signature_processing(Uuid::new_v4())
        .unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        saved.signature_thumbnail_storage_key
    );
}

// UserServiceTest#updateSignature_samePendingUploadIdTwice_returnsSameActiveImageUrl
#[test]
fn update_signature_same_pending_upload_id_twice_returns_same_active_image_url() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let upload_id = uploaded(&w);
    let active_url = cdn(&saved.signature_original_storage_key);

    let first = w.users.update_signature(saved.id, upload_id).unwrap();
    let first_started_at = w.find_user(saved.id).signature_processing_started_at;
    w.clock.advance(std::time::Duration::from_secs(30));
    let second = w.users.update_signature(saved.id, upload_id).unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(first, active_url);
    assert_eq!(second, active_url);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        saved.signature_thumbnail_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, Some(upload_id));
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
    assert_eq!(updated.signature_processing_started_at, first_started_at);
}

// UserServiceTest#updateSignature_sameTimedOutUploadId_throwsReuploadRequiredException
#[test]
fn update_signature_same_timed_out_upload_id_throws_reupload_required_exception() {
    let Some(w) = World::create() else { return };
    let upload_id = Uuid::new_v4();
    let user = processing_user(&w, upload_id, plus(w.now(), -8 * 60));

    assert_app_error(
        w.users.update_signature(user.id, upload_id),
        ErrorKind::Business,
        ErrorCode::SIGNATURE_REUPLOAD_REQUIRED,
        REUPLOAD_MESSAGE,
    );

    let updated = w.find_user(user.id);
    assert_eq!(
        updated.signature_original_storage_key,
        user.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
}

// UserServiceTest#updateSignature_newUploadAfterTimeout_startsNewProcessing
#[test]
fn update_signature_new_upload_after_timeout_starts_new_processing() {
    let Some(w) = World::create() else { return };
    let timed_out_started_at = plus(w.now(), -8 * 60);
    let user = processing_user(&w, Uuid::new_v4(), timed_out_started_at);
    let new_upload_id = uploaded(&w);

    w.users.update_signature(user.id, new_upload_id).unwrap();

    let updated = w.find_user(user.id);
    assert_eq!(
        updated.signature_original_storage_key,
        user.signature_original_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, Some(new_upload_id));
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Processing)
    );
    assert!(updated.signature_processing_started_at.unwrap() > timed_out_started_at);
}

// UserServiceTest#updateSignature_sameFailedUploadId_throwsReuploadRequiredException
#[test]
fn update_signature_same_failed_upload_id_throws_reupload_required_exception() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let upload_id = uploaded(&w);
    w.users.update_signature(saved.id, upload_id).unwrap();
    w.users.fail_signature_processing(upload_id).unwrap();

    assert_app_error(
        w.users.update_signature(saved.id, upload_id),
        ErrorKind::Business,
        ErrorCode::SIGNATURE_REUPLOAD_REQUIRED,
        REUPLOAD_MESSAGE,
    );

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Failed)
    );
}

// UserServiceTest#updateSignature_storageKeyAlreadyOwnedByUser_keepsThumbnailKey
#[test]
fn update_signature_storage_key_already_owned_by_user_keeps_thumbnail_key() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let upload_id = Uuid::new_v4();
    w.storage.storage_keys_for(
        upload_id,
        SignatureStorageKeys {
            original: saved.signature_original_storage_key.clone(),
            thumbnail: format!("chalkak/signatures/dev/thumbnail/{upload_id}.png"),
        },
    );

    w.users.update_signature(saved.id, upload_id).unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        saved.signature_thumbnail_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, None);
}

// UserServiceTest#updateSignature_alreadyUsedStorageKey_throwsNotFoundException
#[test]
fn update_signature_already_used_storage_key_throws_not_found_exception() {
    let Some(w) = World::create() else { return };
    let owner = w.save_user();
    let id = w.save_user().id;
    let upload_id = Uuid::new_v4();
    w.storage.storage_keys_for(
        upload_id,
        SignatureStorageKeys {
            original: owner.signature_original_storage_key.clone(),
            thumbnail: format!("chalkak/signatures/dev/thumbnail/{upload_id}.png"),
        },
    );

    assert_app_error(
        w.users.update_signature(id, upload_id),
        ErrorKind::NotFound,
        ErrorCode::BUSINESS_ERROR,
        NOT_FOUND_MESSAGE,
    );
}

// UserServiceTest#updateSignature_notUploadedImage_throwsNotFoundException
#[test]
fn update_signature_not_uploaded_image_throws_not_found_exception() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;

    assert_app_error(
        w.users.update_signature(id, Uuid::new_v4()),
        ErrorKind::NotFound,
        ErrorCode::BUSINESS_ERROR,
        NOT_FOUND_MESSAGE,
    );
}

// UserServiceTest#updateSignature_disallowedContentType_throwsBusinessException
#[test]
fn update_signature_disallowed_content_type_throws_business_exception() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    let upload_id = Uuid::new_v4();
    w.storage.uploaded(upload_id, image("image/jpeg", 1024));

    assert_app_error(
        w.users.update_signature(id, upload_id),
        ErrorKind::Business,
        ErrorCode::BUSINESS_ERROR,
        "사용할 수 없는 사인 이미지입니다.",
    );
}

// UserServiceTest#updateSignature_notExistingUser_throwsUnauthorizedException
#[test]
fn update_signature_not_existing_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };

    expect_unauthorized(w.users.update_signature(Uuid::new_v4(), Uuid::new_v4()));
}

// UserServiceTest#updateSignature_withdrawnUser_throwsUnauthorizedException
#[test]
fn update_signature_withdrawn_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    w.users.complete_withdrawal(id, &[]).unwrap();

    expect_unauthorized(w.users.update_signature(id, Uuid::new_v4()));
}

// UserServiceTest#updateSignature_revertToActiveSignature_cancelsPendingProcessing
#[test]
fn update_signature_revert_to_active_signature_cancels_pending_processing() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let pending_upload_id = uploaded(&w);
    let active_upload_id = Uuid::new_v4();
    w.storage.storage_keys_for(
        active_upload_id,
        SignatureStorageKeys {
            original: saved.signature_original_storage_key.clone(),
            thumbnail: saved.signature_thumbnail_storage_key.clone().unwrap(),
        },
    );
    w.users
        .update_signature(saved.id, pending_upload_id)
        .unwrap();

    let image_url = w
        .users
        .update_signature(saved.id, active_upload_id)
        .unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(image_url, cdn(&saved.signature_original_storage_key));
    assert_eq!(updated.pending_signature_upload_id, None);
    assert_eq!(updated.signature_processing_status, None);
    assert_eq!(updated.signature_processing_started_at, None);
}

// UserServiceTest#completeSignatureProcessing_cancelledPending_doesNotOverwriteActive
#[test]
fn complete_signature_processing_cancelled_pending_does_not_overwrite_active() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let pending_upload_id = uploaded(&w);
    let active_upload_id = Uuid::new_v4();
    w.storage.storage_keys_for(
        active_upload_id,
        SignatureStorageKeys {
            original: saved.signature_original_storage_key.clone(),
            thumbnail: saved.signature_thumbnail_storage_key.clone().unwrap(),
        },
    );
    w.users
        .update_signature(saved.id, pending_upload_id)
        .unwrap();
    w.users
        .update_signature(saved.id, active_upload_id)
        .unwrap();

    w.users
        .complete_signature_processing(pending_upload_id)
        .unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_thumbnail_storage_key,
        saved.signature_thumbnail_storage_key
    );
    assert_eq!(updated.pending_signature_upload_id, None);
}

// UserServiceTest#completeSignatureProcessing_duplicateCallback_isIdempotent
#[test]
fn complete_signature_processing_duplicate_callback_is_idempotent() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    let upload_id = uploaded(&w);
    let keys = keys_for(upload_id);
    w.users.update_signature(id, upload_id).unwrap();
    w.users.complete_signature_processing(upload_id).unwrap();

    w.users.complete_signature_processing(upload_id).unwrap();

    let updated = w.find_user(id);
    assert_eq!(updated.signature_original_storage_key, keys.original);
    assert_eq!(
        updated.signature_thumbnail_storage_key.as_deref(),
        Some(keys.thumbnail.as_str())
    );
    assert_eq!(updated.pending_signature_upload_id, None);
    assert_eq!(updated.signature_processing_status, None);
}

// UserServiceTest#failSignatureProcessing_afterPromotion_keepsActiveSignature
#[test]
fn fail_signature_processing_after_promotion_keeps_active_signature() {
    let Some(w) = World::create() else { return };
    let id = w.save_user().id;
    let upload_id = uploaded(&w);
    let keys = keys_for(upload_id);
    w.users.update_signature(id, upload_id).unwrap();
    w.users.complete_signature_processing(upload_id).unwrap();

    w.users.fail_signature_processing(upload_id).unwrap();

    let updated = w.find_user(id);
    assert_eq!(updated.signature_original_storage_key, keys.original);
    assert_eq!(
        updated.signature_thumbnail_storage_key.as_deref(),
        Some(keys.thumbnail.as_str())
    );
    assert_eq!(updated.signature_processing_status, None);
}

// UserServiceTest#completeSignatureProcessing_afterPermanentFailure_doesNotPromote
#[test]
fn complete_signature_processing_after_permanent_failure_does_not_promote() {
    let Some(w) = World::create() else { return };
    let saved = w.save_user();
    let upload_id = uploaded(&w);
    w.users.update_signature(saved.id, upload_id).unwrap();
    w.users.fail_signature_processing(upload_id).unwrap();

    w.users.complete_signature_processing(upload_id).unwrap();

    let updated = w.find_user(saved.id);
    assert_eq!(
        updated.signature_original_storage_key,
        saved.signature_original_storage_key
    );
    assert_eq!(
        updated.signature_processing_status,
        Some(SignatureProcessingStatus::Failed)
    );
}

// ---------------------------------------------------------------------------------------------
// UserWithdrawalServiceTest
// ---------------------------------------------------------------------------------------------

fn apple_user_with_authorization(w: &World, banned: bool) -> (User, Uuid, String) {
    let user = if banned {
        w.save_banned_user()
    } else {
        w.save_user()
    };
    let account = w.save_social_account(user.id, SocialProvider::Apple, "apple-subject");
    let encrypted = w.cipher.encrypt("apple-refresh-token").unwrap();
    w.save_apple_authorization(account, &encrypted);
    (user, account, encrypted)
}

// UserWithdrawalServiceTest#withdraw_appleUser_revokesRefreshTokenAndDeletesAuthorization
#[test]
fn withdraw_apple_user_revokes_refresh_token_and_deletes_authorization() {
    let Some(w) = World::create() else { return };
    let (user, account, _) = apple_user_with_authorization(&w, false);

    w.withdrawal.withdraw(user.id).unwrap();

    assert_eq!(
        w.apple_client.revoked(),
        vec!["apple-refresh-token".to_owned()]
    );
    assert!(w.find_active_user(user.id).is_none());
    assert!(w.apple_authorizations(account).is_empty());
}

// UserWithdrawalServiceTest#withdraw_bannedAppleUser_preservesSocialAccountAndDeletesAuthorization
#[test]
fn withdraw_banned_apple_user_preserves_social_account_and_deletes_authorization() {
    let Some(w) = World::create() else { return };
    let (user, account, _) = apple_user_with_authorization(&w, true);

    w.withdrawal.withdraw(user.id).unwrap();

    assert_eq!(
        w.apple_client.revoked(),
        vec!["apple-refresh-token".to_owned()]
    );
    assert!(w.apple_authorizations(account).is_empty());
    let (_, preserved) = w
        .find_social_account(SocialProvider::Apple, "apple-subject")
        .expect("소셜 계정이 남는다");
    assert!(preserved.is_deleted());
    assert_eq!(preserved.status, UserStatus::Banned);
}

// UserWithdrawalServiceTest#withdraw_nonAppleUser_withdrawsWithoutRevocation
#[test]
fn withdraw_non_apple_user_withdraws_without_revocation() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, "apple-subject");

    w.withdrawal.withdraw(user.id).unwrap();

    assert!(w.apple_client.revoked().is_empty());
    assert!(w.find_active_user(user.id).is_none());
}

// UserWithdrawalServiceTest#withdraw_revocationFailure_preservesUserAndAuthorization
#[test]
fn withdraw_revocation_failure_preserves_user_and_authorization() {
    let Some(w) = World::create() else { return };
    let (user, account, _) = apple_user_with_authorization(&w, false);
    w.apple_client
        .revoke_fails("Apple 토큰 서버와 통신할 수 없습니다.");

    let error = w.withdrawal.withdraw(user.id).unwrap_err();

    assert!(
        matches!(&error, ApiError::Internal(m) if m == "Apple 토큰 서버와 통신할 수 없습니다.")
    );
    assert!(w.find_active_user(user.id).is_some());
    assert_eq!(w.apple_authorizations(account).len(), 1);
}

// UserWithdrawalServiceTest#withdraw_authorizationReplacedDuringRevocation_failsAndKeepsReplacedToken
#[test]
fn withdraw_authorization_replaced_during_revocation_fails_and_keeps_replaced_token() {
    let Some(w) = World::create() else { return };
    // 개수가 그대로여서 스냅샷과 크기만으로는 구분되지 않는 상황이다. 삭제 직전 대조가 내용까지 비교하는지 본다.
    let (user, account, _) = apple_user_with_authorization(&w, false);
    let config = w.db.config.pg.clone();
    let now = w.now();
    w.apple_client.on_revoke(move || {
        let mut other = config.connect(NoTls).unwrap();
        AppleAuthorizationRepository
            .delete_all_by_social_account_id(&mut other, account)
            .unwrap();
        AppleAuthorizationRepository
            .insert(&mut other, account, "new-encrypted-refresh-token", now)
            .unwrap();
    });

    let error = common::app_error(w.withdrawal.withdraw(user.id).unwrap_err());

    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Business, ErrorCode::RESOURCE_STATE_CHANGED)
    );
    assert!(w.find_active_user(user.id).is_some());
    let remaining: Vec<String> = w
        .apple_authorizations(account)
        .into_iter()
        .map(|a| a.encrypted_refresh_token)
        .collect();
    assert_eq!(remaining, vec!["new-encrypted-refresh-token".to_owned()]);
}

// UserWithdrawalServiceTest#withdraw_notExistingUser_throwsUnauthorizedException
#[test]
fn withdraw_not_existing_user_throws_unauthorized_exception_without_revocation() {
    let Some(w) = World::create() else { return };

    expect_unauthorized(w.withdrawal.withdraw(Uuid::new_v4()));
    assert!(w.apple_client.revoked().is_empty());
}

// UserWithdrawalServiceTest#withdraw_alreadyWithdrawnUser_throwsUnauthorizedException
#[test]
fn withdraw_already_withdrawn_user_throws_unauthorized_exception_with_single_revocation() {
    let Some(w) = World::create() else { return };
    let (user, _, _) = apple_user_with_authorization(&w, false);
    w.withdrawal.withdraw(user.id).unwrap();

    expect_unauthorized(w.withdrawal.withdraw(user.id));
    assert_eq!(
        w.apple_client.revoked(),
        vec!["apple-refresh-token".to_owned()]
    );
}

// ---------------------------------------------------------------------------------------------
// UserWithdrawalTransactionTest, UserRepositoryTest
// ---------------------------------------------------------------------------------------------

// UserWithdrawalTransactionTest#withdraw_activeUser_completesWithdrawalInTransaction
#[test]
fn withdraw_active_user_completes_withdrawal_in_transaction() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.save_social_account(user.id, SocialProvider::Google, "google-subject");

    w.withdrawal.withdraw(user.id).unwrap();

    assert!(w.find_active_user(user.id).is_none());
    assert!(
        w.find_social_account(SocialProvider::Google, "google-subject")
            .is_none()
    );
}

// UserRepositoryTest#save_duplicateEmail_succeeds
#[test]
fn save_duplicate_email_succeeds() {
    let Some(w) = World::create() else { return };
    let create = || {
        let unique = Uuid::new_v4();
        User::create(
            Some("same@chalkak.test".to_owned()),
            &SignatureStorageKeys {
                original: format!("signatures/original/{unique}"),
                thumbnail: format!("signatures/thumbnail/{unique}"),
            },
        )
        .unwrap()
    };

    let first = UserRepository
        .insert(&mut w.db.client(), &create(), w.now())
        .unwrap();
    let second = UserRepository
        .insert(&mut w.db.client(), &create(), w.now())
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(first.email, second.email);
}
