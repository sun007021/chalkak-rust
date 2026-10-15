//! 회원 서비스: 사인 이미지 등록·조회·변경, 처리 콜백, 탈퇴. 원본 `UserService`, `UserWithdrawalService`,
//! `SignatureProcessingPolicy`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use super::UserRepository;
use super::domain::{SignatureImagePolicy, SignatureProcessingStatus, User, UserStatus};
use super::storage::{
    SignatureImageStorage, SignatureImageUpload, SignatureImageUploadIssuer,
    SignatureProcessingImageUpload,
};
use crate::auth::refresh::UserRefreshTokenService;
use crate::auth::repository::{AppleAuthorizationRepository, SocialAccountRepository};
use crate::auth::tokens::apple_authorization_fingerprint;
use crate::common::transact;
use crate::web::ApiError;

/// 외부 제공자에서 폐기한 소셜 연결의 DB 상태. 지문은 인증 값을 노출하지 않고 탈퇴 트랜잭션에서 현재 상태와 같은지
/// 대조하는 용도로만 쓴다.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SocialConnectionRevocationSnapshot {
    pub authorization_id: Uuid,
    pub credential_fingerprint: String,
}

/// `SocialConnectionRevoker`.
pub trait SocialConnectionRevoker: Send + Sync {
    fn revoke_all(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<SocialConnectionRevocationSnapshot>, ApiError>;
}

/// `UserSignatureResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSignatureResult {
    pub original_image_url: String,
    pub thumbnail_image_url: String,
}

fn unauthorized() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 인증 정보입니다.",
    ))
}

fn image_not_found() -> ApiError {
    ApiError::App(AppError::not_found(
        ErrorCode::BUSINESS_ERROR,
        "업로드한 사인 이미지를 찾을 수 없습니다.",
    ))
}

#[derive(Clone)]
pub struct UserService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    users: UserRepository,
    social_accounts: SocialAccountRepository,
    apple: AppleAuthorizationRepository,
    refresh_tokens: UserRefreshTokenService,
    storage: Arc<dyn SignatureImageStorage>,
    upload_issuer: Arc<dyn SignatureImageUploadIssuer>,
    image_policy: SignatureImagePolicy,
    processing_policy: SignatureProcessingPolicy,
}

/// `SignatureProcessingPolicy`: 사인 처리를 얼마나 기다릴지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignatureProcessingPolicy {
    timeout: SignedDuration,
}

impl SignatureProcessingPolicy {
    /// 제한 시간은 0보다 커야 한다(원본은 `IllegalArgumentException`).
    pub fn new(timeout: Duration) -> Result<SignatureProcessingPolicy, String> {
        if timeout.is_zero() {
            return Err("사인 처리 제한 시간은 0보다 커야 합니다.".to_owned());
        }
        Ok(SignatureProcessingPolicy {
            timeout: SignedDuration::try_from(timeout).unwrap_or(SignedDuration::MAX),
        })
    }

    /// `isProcessingTimedOut`: `startedAt + timeout < now`(경계는 아직 유효).
    pub fn is_processing_timed_out(&self, started_at: Timestamp, now: Timestamp) -> bool {
        started_at
            .saturating_add(self.timeout)
            .is_ok_and(|deadline| deadline < now)
    }
}

impl UserService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        refresh_tokens: UserRefreshTokenService,
        storage: Arc<dyn SignatureImageStorage>,
        upload_issuer: Arc<dyn SignatureImageUploadIssuer>,
        image_policy: SignatureImagePolicy,
        processing_policy: SignatureProcessingPolicy,
    ) -> UserService {
        UserService {
            pool,
            clock,
            users: UserRepository,
            social_accounts: SocialAccountRepository,
            apple: AppleAuthorizationRepository,
            refresh_tokens,
            storage,
            upload_issuer,
            image_policy,
            processing_policy,
        }
    }

    fn is_signature_processing_timed_out(&self, user: &User, now: Timestamp) -> bool {
        if user.signature_processing_status != Some(SignatureProcessingStatus::Processing) {
            return false;
        }
        user.signature_processing_started_at
            .is_some_and(|started| self.processing_policy.is_processing_timed_out(started, now))
    }

    fn active_user(&self, user_id: Uuid) -> Result<User, ApiError> {
        let mut conn = self.pool.get()?;
        self.users
            .find_active_by_id(&mut conn, user_id)?
            .ok_or_else(unauthorized)
    }

    /// 사인 변경용 업로드 URL 발급(`createSignatureUpload`).
    pub fn create_signature_upload(&self, user_id: Uuid) -> Result<SignatureImageUpload, ApiError> {
        self.active_user(user_id)?;
        self.upload_issuer.issue(Uuid::new_v4())
    }

    /// 이미지 처리 람다가 쓸 업로드 URL(`issueSignatureProcessingUpload`).
    pub fn issue_signature_processing_upload(
        &self,
        upload_id: Uuid,
    ) -> Result<SignatureProcessingImageUpload, ApiError> {
        self.upload_issuer.issue_processing_upload(upload_id)
    }

    /// 내 사인 조회(`getSignature`). 처리에 실패했거나 처리 시간을 넘겼으면 다시 등록해야 한다.
    pub fn get_signature(&self, user_id: Uuid) -> Result<UserSignatureResult, ApiError> {
        let user = self.active_user(user_id)?;
        if user.signature_processing_status == Some(SignatureProcessingStatus::Failed)
            || self.is_signature_processing_timed_out(&user, self.clock.now())
        {
            return Err(ApiError::App(AppError::business(
                ErrorCode::SIGNATURE_REGISTRATION_REQUIRED,
                "사인 이미지 처리에 실패했습니다. 사인을 다시 등록해 주세요.",
            )));
        }
        let thumbnail = user
            .signature_thumbnail_storage_key
            .as_deref()
            .ok_or_else(|| ApiError::Internal("썸네일 저장 키가 없습니다".to_owned()))?;
        Ok(UserSignatureResult {
            original_image_url: self
                .storage
                .to_image_url(&user.signature_original_storage_key)?,
            thumbnail_image_url: self.storage.to_image_url(thumbnail)?,
        })
    }

    /// 사인 변경(`updateSignature`). 회원 행을 잠그고 업로드 상태에 따라 분기한다.
    pub fn update_signature(&self, user_id: Uuid, upload_id: Uuid) -> Result<String, ApiError> {
        let keys = self.storage.to_storage_keys(upload_id);
        transact(&self.pool, |tx| {
            let before = self
                .users
                .find_active_by_id_for_update(tx, user_id)?
                .ok_or_else(unauthorized)?;
            let mut user = before.clone();
            let now = self.clock.now();

            let url = 'decide: {
                if user.has_signature(&keys.original) {
                    user.cancel_signature_processing();
                    break 'decide self.signature_image_url(&user)?;
                }
                if user.is_signature_processing_failed(upload_id)
                    || (user.is_signature_processing(upload_id)
                        && self.is_signature_processing_timed_out(&user, now))
                {
                    return Err(ApiError::App(AppError::business(
                        ErrorCode::SIGNATURE_REUPLOAD_REQUIRED,
                        "사인 이미지 처리에 실패했습니다. 새로운 업로드 ID를 발급받아 이미지를 다시 업로드해 주세요.",
                    )));
                }
                if user.is_signature_processing(upload_id) {
                    break 'decide self.signature_image_url(&user)?;
                }
                if self
                    .users
                    .exists_by_signature_original_storage_key(tx, &keys.original)?
                {
                    return Err(image_not_found());
                }
                let image = self
                    .storage
                    .find_uploaded_image(upload_id)?
                    .ok_or_else(image_not_found)?;
                self.image_policy.validate(&image)?;

                user.start_signature_processing(upload_id, now)?;
                if self.storage.is_processing_completed(upload_id)? {
                    user.complete_signature_processing(upload_id, &keys)?;
                }
                self.signature_image_url(&user)?
            };
            self.users.update_if_changed(tx, &before, &user, now)?;
            Ok(url)
        })
    }

    fn signature_image_url(&self, user: &User) -> Result<String, ApiError> {
        self.storage
            .to_image_url(&user.signature_original_storage_key)
    }

    /// 처리 완료 콜백(`completeSignatureProcessing`). 처리 중이 아니거나 시간이 지났으면 무시한다.
    pub fn complete_signature_processing(&self, upload_id: Uuid) -> Result<(), ApiError> {
        let now = self.clock.now();
        transact(&self.pool, |tx| {
            let Some(before) = self
                .users
                .find_active_by_pending_signature_upload_id_for_update(tx, upload_id)?
            else {
                return Ok(());
            };
            if !before.is_signature_processing(upload_id)
                || self.is_signature_processing_timed_out(&before, now)
            {
                return Ok(());
            }
            let mut user = before.clone();
            user.complete_signature_processing(
                upload_id,
                &self.storage.to_storage_keys(upload_id),
            )?;
            self.users.update_if_changed(tx, &before, &user, now)?;
            Ok(())
        })
    }

    /// 처리 실패 콜백(`failSignatureProcessing`).
    pub fn fail_signature_processing(&self, upload_id: Uuid) -> Result<(), ApiError> {
        let now = self.clock.now();
        transact(&self.pool, |tx| {
            let Some(before) = self
                .users
                .find_active_by_pending_signature_upload_id_for_update(tx, upload_id)?
            else {
                return Ok(());
            };
            let mut user = before.clone();
            user.fail_signature_processing(upload_id);
            self.users.update_if_changed(tx, &before, &user, now)?;
            Ok(())
        })
    }

    /// 탈퇴 처리(`completeWithdrawal`). 외부 제공자 폐기가 끝난 연결의 스냅샷이 현재 DB와 같을 때만 진행한다.
    /// 차단 회원은 재가입 우회를 막으려고 소셜 계정을 남기고, 모든 기기의 리프레시 계보는 폐기한다.
    pub fn complete_withdrawal(
        &self,
        user_id: Uuid,
        revoked: &[SocialConnectionRevocationSnapshot],
    ) -> Result<(), ApiError> {
        transact(&self.pool, |tx| {
            let now = self.clock.now();
            let before = self
                .users
                .find_active_by_id(tx, user_id)?
                .ok_or_else(unauthorized)?;

            if let Some(social_account_id) = self.social_accounts.find_id_by_user_id(tx, user_id)? {
                let current: Vec<SocialConnectionRevocationSnapshot> = self
                    .apple
                    .find_all_by_social_account_id(tx, social_account_id)?
                    .into_iter()
                    .map(|a| SocialConnectionRevocationSnapshot {
                        authorization_id: a.id,
                        credential_fingerprint: apple_authorization_fingerprint(
                            &a.encrypted_refresh_token,
                        ),
                    })
                    .collect();
                if !matches_revoked(&current, revoked) {
                    return Err(ApiError::App(AppError::business(
                        ErrorCode::RESOURCE_STATE_CHANGED,
                        "탈퇴 처리 중 Apple 인증 정보가 변경되었습니다. 다시 시도해 주세요.",
                    )));
                }
                self.apple
                    .delete_all_by_social_account_id(tx, social_account_id)?;
            }
            if before.status != UserStatus::Banned {
                self.social_accounts.delete_by_user_id(tx, user_id)?;
            }
            self.refresh_tokens.revoke_all(tx, user_id)?;

            let mut user = before.clone();
            user.withdraw(now)?;
            self.users.update_if_changed(tx, &before, &user, now)?;
            Ok(())
        })
    }
}

/// 현재 연결 수가 폐기한 수와 같고 현재 스냅샷이 폐기한 스냅샷을 모두 포함하는가(`matchesRevoked`).
fn matches_revoked(
    current: &[SocialConnectionRevocationSnapshot],
    revoked: &[SocialConnectionRevocationSnapshot],
) -> bool {
    if current.len() != revoked.len() {
        return false;
    }
    let current: HashSet<&SocialConnectionRevocationSnapshot> = current.iter().collect();
    revoked.iter().all(|r| current.contains(r))
}

/// `UserWithdrawalService`: 외부 폐기를 트랜잭션 밖에서 먼저 하고, 끝난 스냅샷으로 탈퇴를 마무리한다.
#[derive(Clone)]
pub struct UserWithdrawalService {
    revoker: Arc<dyn SocialConnectionRevoker>,
    users: UserService,
}

impl UserWithdrawalService {
    pub fn new(
        revoker: Arc<dyn SocialConnectionRevoker>,
        users: UserService,
    ) -> UserWithdrawalService {
        UserWithdrawalService { revoker, users }
    }

    pub fn withdraw(&self, user_id: Uuid) -> Result<(), ApiError> {
        let snapshots = self.revoker.revoke_all(user_id)?;
        self.users.complete_withdrawal(user_id, &snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(n: u8, fp: &str) -> SocialConnectionRevocationSnapshot {
        SocialConnectionRevocationSnapshot {
            authorization_id: Uuid::from_bytes([n; 16]),
            credential_fingerprint: fp.to_owned(),
        }
    }

    // AppleSocialConnectionRevocationStore#matchesRevoked
    #[test]
    fn 폐기한_연결과_현재_연결이_같을_때만_삭제를_진행한다() {
        assert!(matches_revoked(&[], &[]));
        assert!(matches_revoked(&[snap(1, "a")], &[snap(1, "a")]));
        assert!(!matches_revoked(&[snap(1, "a")], &[]));
        assert!(!matches_revoked(&[], &[snap(1, "a")]));
        assert!(!matches_revoked(&[snap(1, "a")], &[snap(1, "b")]));
        assert!(!matches_revoked(&[snap(1, "a")], &[snap(2, "a")]));
    }

    // SignatureProcessingPolicyTest
    fn started_at() -> Timestamp {
        "2026-08-26T00:00:00Z".parse().unwrap()
    }

    fn seven_minutes() -> SignatureProcessingPolicy {
        SignatureProcessingPolicy::new(Duration::from_secs(7 * 60)).unwrap()
    }

    fn after(seconds: i64) -> Timestamp {
        started_at()
            .checked_add(SignedDuration::from_secs(seconds))
            .unwrap()
    }

    #[test]
    fn is_processing_timed_out_after_timeout_returns_true() {
        assert!(seven_minutes().is_processing_timed_out(started_at(), after(421)));
    }

    #[test]
    fn is_processing_timed_out_at_timeout_returns_false() {
        assert!(!seven_minutes().is_processing_timed_out(started_at(), after(420)));
    }

    #[test]
    fn is_processing_timed_out_before_timeout_returns_false() {
        assert!(!seven_minutes().is_processing_timed_out(started_at(), after(419)));
    }

    #[test]
    fn constructor_non_positive_timeout_throws_exception() {
        // 음수 Duration은 표현할 수 없으므로 0만 확인한다.
        assert!(SignatureProcessingPolicy::new(Duration::ZERO).is_err());
    }
}
