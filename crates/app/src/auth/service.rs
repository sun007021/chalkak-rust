//! 소셜 로그인과 가입. 원본 `SocialLoginService`, `ExistingSocialAccountLoginProcessor`,
//! `AppleSignupAuthorizationService`, `SocialSignupService`.
//!
//! 외부 호출(ID Token 검증, Apple 토큰 교환, S3)은 DB 트랜잭션 밖에서 하고, DB 작업은 단계마다 짧은 트랜잭션으로 한다.
//! 단 가입은 원본처럼 한 트랜잭션 안에서 S3 존재 확인까지 한다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Db;
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use super::apple::AppleTokenClient;
use super::domain::{
    IssuedAccessToken, IssuedRefreshToken, IssuedSocialSignupToken, SocialProvider,
    VerifiedSocialIdentity, VerifiedSocialSignupToken, validate_apple_authorization,
    validate_consumed_signup_token, validate_pending_apple_authorization, validate_social_account,
};
use super::oidc::SocialIdentityVerifier;
use super::refresh::UserRefreshTokenService;
use super::repository::{
    AppleAuthorizationRepository, ConsumedSignupTokenRepository,
    PendingAppleAuthorizationRepository, SocialAccountRepository,
};
use super::tokens::{AccessTokenIssuer, AppleCipher, IdentityFingerprint, SignupTokens};
use crate::common::transact;
use crate::user::UserRepository;
use crate::user::domain::{SignatureImagePolicy, User, UserStatus};
use crate::user::storage::{
    SignatureImageStorage, SignatureImageUpload, SignatureImageUploadIssuer,
};
use crate::web::ApiError;

/// 가입이 끝나지 않은 Apple 리프레시 토큰의 임시 보관 기간. ID Token 수명(10분) 안에 업로드 URL을 받고 그 뒤
/// 회원가입 토큰(5분) 안에 가입을 끝낼 시간을 덮는다.
const PENDING_AUTHORIZATION_EXPIRATION: SignedDuration = SignedDuration::from_mins(15);

fn business(message: &str) -> ApiError {
    ApiError::App(AppError::business(ErrorCode::BUSINESS_ERROR, message))
}

fn not_found(message: &str) -> ApiError {
    ApiError::App(AppError::not_found(ErrorCode::BUSINESS_ERROR, message))
}

fn unauthorized(message: &str) -> ApiError {
    ApiError::App(AppError::unauthorized(ErrorCode::UNAUTHORIZED, message))
}

fn forbidden(message: &str) -> ApiError {
    ApiError::App(AppError::forbidden(ErrorCode::FORBIDDEN, message))
}

// ---------------------------------------------------------------------------------------------
// Apple 가입 인증 정보 보관
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppleSignupAuthorizationService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    fingerprint: IdentityFingerprint,
    pending: PendingAppleAuthorizationRepository,
    apple: AppleAuthorizationRepository,
}

impl AppleSignupAuthorizationService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        fingerprint: IdentityFingerprint,
    ) -> AppleSignupAuthorizationService {
        AppleSignupAuthorizationService {
            pool,
            clock,
            fingerprint,
            pending: PendingAppleAuthorizationRepository,
            apple: AppleAuthorizationRepository,
        }
    }

    fn expires_at(&self) -> Timestamp {
        self.clock
            .now()
            .saturating_add(PENDING_AUTHORIZATION_EXPIRATION)
            .unwrap_or(Timestamp::MAX)
    }

    /// 만료 전 보관분이 있으면 만료만 다시 밀고 `true`(`renewIfPresent`). 행을 잠그고 읽어, 만료를 미는 두 경로가
    /// 겹쳐도 "앞으로만 간다"가 지켜진다.
    pub fn renew_if_present(&self, identity: &VerifiedSocialIdentity) -> Result<bool, ApiError> {
        transact(&self.pool, |tx| {
            let hmac = self
                .fingerprint
                .encode(identity.provider, &identity.subject);
            let found =
                self.pending
                    .find_latest_unexpired_for_update(tx, &hmac, self.clock.now())?;
            let Some(mut pending) = found else {
                return Ok(false);
            };
            if pending.extend_to(self.expires_at()) {
                self.pending
                    .update_expires_at(tx, pending.id, pending.expires_at)?;
            }
            Ok(true)
        })
    }

    /// 교환한 리프레시 토큰(암호문)을 신원별로 임시 보관한다(`store`).
    pub fn store(
        &self,
        identity: &VerifiedSocialIdentity,
        encrypted_refresh_token: &str,
    ) -> Result<(), ApiError> {
        let hmac = self
            .fingerprint
            .encode(identity.provider, &identity.subject);
        validate_pending_apple_authorization(&hmac, encrypted_refresh_token)?;
        let mut conn = self.pool.get()?;
        self.pending.insert(
            &mut conn,
            &hmac,
            encrypted_refresh_token,
            self.expires_at(),
            self.clock.now(),
        )?;
        Ok(())
    }

    /// Apple 신규 회원은 보관분이 있어야 가입을 끝낼 수 있으므로 없거나 만료됐으면 오류, 있으면 회원가입 토큰이 살아 있는
    /// 동안 먼저 만료되지 않게 만료를 맞춘다(`extendIfApple`).
    pub fn extend_if_apple(
        &self,
        identity: &VerifiedSocialIdentity,
        expires_at: Timestamp,
    ) -> Result<(), ApiError> {
        if identity.provider != SocialProvider::Apple {
            return Ok(());
        }
        transact(&self.pool, |tx| {
            let hmac = self
                .fingerprint
                .encode(identity.provider, &identity.subject);
            let mut pending = self.latest_for_update(tx, &hmac)?;
            if pending.extend_to(expires_at) {
                self.pending
                    .update_expires_at(tx, pending.id, pending.expires_at)?;
            }
            Ok(())
        })
    }

    /// 가입 트랜잭션 안에서 최신 임시 보관분 하나를 정식 보관으로 옮긴다(`saveIfApple`). 나머지 행은 폐기용으로 남긴다.
    pub fn save_if_apple(
        &self,
        db: &mut impl Db,
        token: &VerifiedSocialSignupToken,
        social_account_id: Uuid,
        subject_hmac: &str,
        now: Timestamp,
    ) -> Result<(), ApiError> {
        if token.provider != SocialProvider::Apple {
            return Ok(());
        }
        let pending = self.latest_for_update(db, subject_hmac)?;
        validate_apple_authorization(token.provider, &pending.encrypted_refresh_token)?;
        self.apple
            .insert(db, social_account_id, &pending.encrypted_refresh_token, now)?;
        self.pending.delete(db, pending.id)?;
        Ok(())
    }

    fn latest_for_update(
        &self,
        db: &mut impl Db,
        subject_hmac: &str,
    ) -> Result<super::repository::PendingAppleAuthorization, ApiError> {
        self.pending
            .find_latest_unexpired_for_update(db, subject_hmac, self.clock.now())?
            .ok_or_else(|| {
                business("Apple 회원가입 인증 정보가 만료되었거나 없습니다. 다시 로그인해 주세요.")
            })
    }
}

// ---------------------------------------------------------------------------------------------
// 로그인
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocialLoginSuccess {
    pub user_id: Uuid,
    pub access_token: IssuedAccessToken,
    pub refresh_token: IssuedRefreshToken,
}

/// `SocialLoginResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocialLoginResult {
    LoginSuccess(SocialLoginSuccess),
    SignUpRequired,
}

/// `ExistingSocialAccountLoginProcessor`.
#[derive(Clone)]
pub struct ExistingSocialAccountLoginProcessor {
    pool: Pool,
    fingerprint: IdentityFingerprint,
    social_accounts: SocialAccountRepository,
    access_tokens: AccessTokenIssuer,
    refresh_tokens: UserRefreshTokenService,
}

impl ExistingSocialAccountLoginProcessor {
    pub fn new(
        pool: Pool,
        fingerprint: IdentityFingerprint,
        access_tokens: AccessTokenIssuer,
        refresh_tokens: UserRefreshTokenService,
    ) -> ExistingSocialAccountLoginProcessor {
        ExistingSocialAccountLoginProcessor {
            pool,
            fingerprint,
            social_accounts: SocialAccountRepository,
            access_tokens,
            refresh_tokens,
        }
    }

    /// 연결된 기존 회원이 있으면 로그인시키고 없으면 `None`. 소셜 계정이 아예 없는 경우와 일반 탈퇴 회원의 남은 연결은
    /// 모두 `None`(가입 흐름으로 이어진다).
    pub fn process_if_exists(
        &self,
        identity: &VerifiedSocialIdentity,
    ) -> Result<Option<SocialLoginSuccess>, ApiError> {
        transact(&self.pool, |tx| {
            let hmac = self
                .fingerprint
                .encode(identity.provider, &identity.subject);
            let Some((_, user)) = self.social_accounts.find_by_provider_and_subject_hmac(
                tx,
                identity.provider,
                &hmac,
            )?
            else {
                return Ok(None);
            };
            // 차단 회원은 탈퇴해도 재가입 우회를 막으려고 소셜 계정을 유지하므로 로그인을 거부한다.
            if user.is_deleted() && user.status == UserStatus::Banned {
                return Err(forbidden("탈퇴한 차단 소셜 계정입니다."));
            }
            if user.is_deleted() {
                return Ok(None);
            }
            let refresh_token = self.refresh_tokens.issue(tx, user.id)?;
            Ok(Some(SocialLoginSuccess {
                user_id: user.id,
                access_token: self
                    .access_tokens
                    .issue_for_session(user.id, refresh_token.session_id),
                refresh_token,
            }))
        })
    }
}

/// `SocialLoginService`.
#[derive(Clone)]
pub struct SocialLoginService {
    verifier: SocialIdentityVerifier,
    apple_client: Arc<dyn AppleTokenClient>,
    cipher: AppleCipher,
    apple_signup: AppleSignupAuthorizationService,
    existing: ExistingSocialAccountLoginProcessor,
}

impl SocialLoginService {
    pub fn new(
        verifier: SocialIdentityVerifier,
        apple_client: Arc<dyn AppleTokenClient>,
        cipher: AppleCipher,
        apple_signup: AppleSignupAuthorizationService,
        existing: ExistingSocialAccountLoginProcessor,
    ) -> SocialLoginService {
        SocialLoginService {
            verifier,
            apple_client,
            cipher,
            apple_signup,
            existing,
        }
    }

    /// 트랜잭션을 열지 않는다. ID Token 검증과 Apple 토큰 교환 같은 외부 호출을 하는 동안 DB 연결을 잡지 않으려는 것이다.
    pub fn login(
        &self,
        provider: SocialProvider,
        id_token: &str,
        raw_nonce: &str,
        authorization_code: Option<&str>,
    ) -> Result<SocialLoginResult, ApiError> {
        let identity = self.verifier.verify(provider, id_token, raw_nonce)?;
        if let Some(success) = self.existing.process_if_exists(&identity)? {
            return Ok(SocialLoginResult::LoginSuccess(success));
        }
        self.prepare_apple_signup(&identity, authorization_code, raw_nonce)?;
        Ok(SocialLoginResult::SignUpRequired)
    }

    /// Apple 신규 회원만 탈퇴 시 폐기할 리프레시 토큰을 미리 받아 둔다. 만료 전 보관분이 있으면 요청의
    /// authorizationCode는 쓰지 않는다.
    fn prepare_apple_signup(
        &self,
        identity: &VerifiedSocialIdentity,
        authorization_code: Option<&str>,
        raw_nonce: &str,
    ) -> Result<(), ApiError> {
        if identity.provider != SocialProvider::Apple {
            return Ok(());
        }
        if self.apple_signup.renew_if_present(identity)? {
            return Ok(());
        }
        let exchanged = self
            .apple_client
            .exchange_authorization_code(authorization_code.unwrap_or(""))?;
        let exchanged_identity =
            self.verifier
                .verify(SocialProvider::Apple, &exchanged.id_token, raw_nonce)?;
        if exchanged_identity.subject != identity.subject {
            return Err(unauthorized(
                "Apple 로그인 사용자 정보가 일치하지 않습니다.",
            ));
        }
        let encrypted = self
            .cipher
            .encrypt(&exchanged.refresh_token)
            .map_err(ApiError::Internal)?;
        self.apple_signup.store(identity, &encrypted)
    }
}

// ---------------------------------------------------------------------------------------------
// 가입
// ---------------------------------------------------------------------------------------------

/// `SocialSignupSignatureUploadResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocialSignupSignatureUploadResult {
    pub upload: SignatureImageUpload,
    pub signup_token: IssuedSocialSignupToken,
}

/// `SocialSignupResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocialSignupResult {
    pub user_id: Uuid,
    pub access_token: IssuedAccessToken,
    pub refresh_token: IssuedRefreshToken,
}

#[derive(Clone)]
pub struct SocialSignupService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    verifier: SocialIdentityVerifier,
    fingerprint: IdentityFingerprint,
    signup_tokens: SignupTokens,
    access_tokens: AccessTokenIssuer,
    refresh_tokens: UserRefreshTokenService,
    apple_signup: AppleSignupAuthorizationService,
    upload_issuer: Arc<dyn SignatureImageUploadIssuer>,
    storage: Arc<dyn SignatureImageStorage>,
    image_policy: SignatureImagePolicy,
    social_accounts: SocialAccountRepository,
    consumed_tokens: ConsumedSignupTokenRepository,
    users: UserRepository,
}

/// `SocialSignupService`의 협력 객체 묶음(생성자 인자가 많아 묶었다).
pub struct SocialSignupDeps {
    pub pool: Pool,
    pub clock: Arc<dyn Clock>,
    pub verifier: SocialIdentityVerifier,
    pub fingerprint: IdentityFingerprint,
    pub signup_tokens: SignupTokens,
    pub access_tokens: AccessTokenIssuer,
    pub refresh_tokens: UserRefreshTokenService,
    pub apple_signup: AppleSignupAuthorizationService,
    pub upload_issuer: Arc<dyn SignatureImageUploadIssuer>,
    pub storage: Arc<dyn SignatureImageStorage>,
    pub image_policy: SignatureImagePolicy,
}

impl SocialSignupService {
    pub fn new(deps: SocialSignupDeps) -> SocialSignupService {
        SocialSignupService {
            pool: deps.pool,
            clock: deps.clock,
            verifier: deps.verifier,
            fingerprint: deps.fingerprint,
            signup_tokens: deps.signup_tokens,
            access_tokens: deps.access_tokens,
            refresh_tokens: deps.refresh_tokens,
            apple_signup: deps.apple_signup,
            upload_issuer: deps.upload_issuer,
            storage: deps.storage,
            image_policy: deps.image_policy,
            social_accounts: SocialAccountRepository,
            consumed_tokens: ConsumedSignupTokenRepository,
            users: UserRepository,
        }
    }

    /// 사인 업로드 URL과 회원가입 토큰 발급(`createSignatureUpload`). Apple 보관분 확인을 업로드 URL 발급보다 앞에 둔다.
    pub fn create_signature_upload(
        &self,
        provider: SocialProvider,
        id_token: &str,
        raw_nonce: &str,
    ) -> Result<SocialSignupSignatureUploadResult, ApiError> {
        let identity = self.verifier.verify(provider, id_token, raw_nonce)?;
        self.validate_new_social_account(&identity)?;

        let upload_id = Uuid::new_v4();
        let signup_token = self.signup_tokens.issue(&identity, upload_id);
        self.apple_signup
            .extend_if_apple(&identity, signup_token.expires_at)?;

        let upload = self.upload_issuer.issue(upload_id)?;
        Ok(SocialSignupSignatureUploadResult {
            upload,
            signup_token,
        })
    }

    fn validate_new_social_account(
        &self,
        identity: &VerifiedSocialIdentity,
    ) -> Result<(), ApiError> {
        let hmac = self
            .fingerprint
            .encode(identity.provider, &identity.subject);
        let mut conn = self.pool.get()?;
        let found = self.social_accounts.find_by_provider_and_subject_hmac(
            &mut conn,
            identity.provider,
            &hmac,
        )?;
        match found {
            None => Ok(()),
            Some((_, user)) if user.status == UserStatus::Banned => {
                Err(forbidden("차단된 소셜 계정입니다."))
            }
            Some(_) => Err(business("이미 가입된 소셜 계정입니다.")),
        }
    }

    /// 회원가입 완료(`signup`). 한 트랜잭션 안에서 토큰 소진 기록 -> 이미지 확인 -> 회원·소셜 계정 저장.
    pub fn signup(&self, signup_token: &str) -> Result<SocialSignupResult, ApiError> {
        let verified = self.signup_tokens.verify(signup_token)?;
        transact(&self.pool, |tx| {
            let now = self.clock.now();
            let hmac = self
                .fingerprint
                .encode(verified.provider, &verified.subject);
            if let Some((_, user)) = self.social_accounts.find_by_provider_and_subject_hmac(
                tx,
                verified.provider,
                &hmac,
            )? {
                return self.existing_user_result(tx, user);
            }
            self.consume_signup_token(tx, &verified)?;

            let keys = self.storage.to_storage_keys(verified.upload_id);
            if self
                .users
                .exists_by_signature_original_storage_key(tx, &keys.original)?
            {
                return Err(not_found("업로드한 사인 이미지를 찾을 수 없습니다."));
            }
            let image = self
                .storage
                .find_uploaded_image(verified.upload_id)?
                .ok_or_else(|| not_found("업로드한 사인 이미지를 찾을 수 없습니다."))?;
            if !self.storage.is_processing_completed(verified.upload_id)? {
                return Err(ApiError::App(AppError::business(
                    ErrorCode::SIGNATURE_PROCESSING_PENDING,
                    "사인 이미지 처리 중입니다.",
                )));
            }
            self.image_policy.validate(&image)?;

            let user = self
                .users
                .insert(tx, &User::create(verified.email.clone(), &keys)?, now)?;
            validate_social_account(&hmac)?;
            let social_account_id =
                self.social_accounts
                    .insert(tx, user.id, verified.provider, &hmac, now)?;
            self.apple_signup
                .save_if_apple(tx, &verified, social_account_id, &hmac, now)?;
            self.signup_result(tx, user.id)
        })
    }

    fn existing_user_result(
        &self,
        db: &mut impl Db,
        user: User,
    ) -> Result<SocialSignupResult, ApiError> {
        if user.status == UserStatus::Banned {
            return Err(forbidden("차단된 소셜 계정입니다."));
        }
        if user.is_deleted() {
            return Err(business("이미 가입된 소셜 계정입니다."));
        }
        self.signup_result(db, user.id)
    }

    /// 리프레시 토큰 계보는 회원 행에 FK로 매달리므로 가입 직후 액세스 토큰과 함께 발급한다.
    fn signup_result(
        &self,
        db: &mut impl Db,
        user_id: Uuid,
    ) -> Result<SocialSignupResult, ApiError> {
        let refresh_token = self.refresh_tokens.issue(db, user_id)?;
        Ok(SocialSignupResult {
            user_id,
            access_token: self
                .access_tokens
                .issue_for_session(user_id, refresh_token.session_id),
            refresh_token,
        })
    }

    /// 상태 없는 JWT는 만료 전까지 몇 번이든 검증을 통과하므로 jti를 처음 쓸 때 소진 처리해 두 번째 가입 완료를 막는다.
    /// 뒤 단계가 실패하면 가입 트랜잭션과 함께 롤백된다.
    fn consume_signup_token(
        &self,
        db: &mut impl Db,
        verified: &VerifiedSocialSignupToken,
    ) -> Result<(), ApiError> {
        validate_consumed_signup_token(&verified.token_id)?;
        if !self
            .consumed_tokens
            .consume_if_absent(db, &verified.token_id, verified.expires_at)?
        {
            return Err(business("이미 사용된 회원가입 토큰입니다."));
        }
        Ok(())
    }
}
