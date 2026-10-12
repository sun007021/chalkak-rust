//! auth·user 도메인의 객체 조립. 원본에서 스프링이 빈으로 엮던 부분을 생성자로 직접 엮는다.
//!
//! 외부 시스템(OIDC 공개 키, Apple, S3)에 닿는 구현은 여기서만 고른다. 시험은 서비스를 직접 만들거나
//! [`crate::config`]의 주소를 가짜 서버로 돌려서 쓴다.

use std::sync::Arc;

use crate::AppContext;
use crate::auth::UsableUserPolicy;
use crate::auth::apple::{AppleHttpTokenClient, AppleTokenClient};
use crate::auth::callback::ProcessingCallbackAuthenticator;
use crate::auth::domain::RefreshTokenPolicy;
use crate::auth::oidc::SocialIdentityVerifier;
use crate::auth::refresh::UserRefreshTokenService;
use crate::auth::revoker::AppleSocialConnectionRevoker;
use crate::auth::service::{
    AppleSignupAuthorizationService, ExistingSocialAccountLoginProcessor, SocialLoginService,
    SocialSignupDeps, SocialSignupService,
};
use crate::auth::tokens::{
    AccessTokenIssuer, AppleCipher, AppleClientSecret, IdentityFingerprint, SignupTokens,
};
use crate::config::{ACCESS_TOKEN_AUDIENCE, SIGNUP_TOKEN_AUDIENCE, SIGNUP_TOKEN_LIFETIME};
use crate::user::domain::SignatureImagePolicy;
use crate::user::service::{SignatureProcessingPolicy, UserService, UserWithdrawalService};
use crate::user::storage::{
    HeadObjects, HttpHeadObjects, S3SignatureImageStorage, S3SignatureImageUploadIssuer,
    SignatureImageStorage, SignatureImageUploadIssuer, credentials_for,
};

/// 라우트가 쓰는 서비스 묶음.
#[derive(Clone)]
pub struct Components {
    pub policy: UsableUserPolicy,
    pub login: SocialLoginService,
    pub signup: SocialSignupService,
    pub refresh: UserRefreshTokenService,
    pub users: UserService,
    pub withdrawal: UserWithdrawalService,
    pub callback_auth: ProcessingCallbackAuthenticator,
}

impl Components {
    pub fn build(ctx: &AppContext) -> Result<Components, String> {
        let settings = &*ctx.settings;
        let auth = &settings.auth;
        let pool = ctx.pool.clone();
        let clock = Arc::clone(&ctx.clock);

        let access_tokens = AccessTokenIssuer::new(
            auth.access_token_key.clone(),
            ACCESS_TOKEN_AUDIENCE,
            auth.access_token_expiration,
            Arc::clone(&clock),
        );
        let refresh = UserRefreshTokenService::new(
            pool.clone(),
            Arc::clone(&clock),
            RefreshTokenPolicy::new(auth.refresh_inactivity, auth.refresh_absolute)?,
            access_tokens.clone(),
        );
        let fingerprint = IdentityFingerprint::new(auth.social_identity_key.clone());
        let verifier = SocialIdentityVerifier::from_settings(auth, Arc::clone(&clock))?;

        let client_secret = Arc::new(
            AppleClientSecret::new(
                &auth.apple.private_key_base64,
                &auth.apple.team_id,
                &auth.apple.key_id,
                &auth.apple.client_id,
                Arc::clone(&clock),
            )
            .map_err(|e| format!("APPLE_PRIVATE_KEY_BASE64: {e}"))?,
        );
        let apple_client: Arc<dyn AppleTokenClient> = Arc::new(AppleHttpTokenClient::new(
            &auth.apple.token_uri,
            &auth.apple.revoke_uri,
            &auth.apple.client_id,
            client_secret,
        )?);
        let cipher = AppleCipher::new(&auth.apple.encryption_key);
        let apple_signup = AppleSignupAuthorizationService::new(
            pool.clone(),
            Arc::clone(&clock),
            fingerprint.clone(),
        );
        let existing = ExistingSocialAccountLoginProcessor::new(
            pool.clone(),
            fingerprint.clone(),
            access_tokens.clone(),
            refresh.clone(),
        );
        let login = SocialLoginService::new(
            verifier.clone(),
            Arc::clone(&apple_client),
            cipher.clone(),
            apple_signup.clone(),
            existing,
        );

        let credentials = credentials_for(&settings.image, Arc::clone(&clock))?;
        let head: Arc<dyn HeadObjects> = Arc::new(HttpHeadObjects::new(
            &settings.image,
            credentials.clone(),
            Arc::clone(&clock),
        )?);
        let storage: Arc<dyn SignatureImageStorage> =
            Arc::new(S3SignatureImageStorage::new(&settings.image, head));
        let upload_issuer: Arc<dyn SignatureImageUploadIssuer> =
            Arc::new(S3SignatureImageUploadIssuer::new(
                &settings.image,
                credentials,
                Arc::clone(&storage),
                Arc::clone(&clock),
            ));
        let image_policy = SignatureImagePolicy {
            max_bytes: i64::try_from(settings.image.signature_max_bytes).unwrap_or(i64::MAX),
            allowed_content_types: settings.image.signature_content_types.clone(),
        };

        let signup = SocialSignupService::new(SocialSignupDeps {
            pool: pool.clone(),
            clock: Arc::clone(&clock),
            verifier,
            fingerprint,
            signup_tokens: SignupTokens::new(
                auth.signup_token_key.clone(),
                SIGNUP_TOKEN_AUDIENCE,
                SIGNUP_TOKEN_LIFETIME,
                Arc::clone(&clock),
            ),
            access_tokens,
            refresh_tokens: refresh.clone(),
            apple_signup,
            upload_issuer: Arc::clone(&upload_issuer),
            storage: Arc::clone(&storage),
            image_policy: image_policy.clone(),
        });

        let users = UserService::new(
            pool.clone(),
            Arc::clone(&clock),
            refresh.clone(),
            storage,
            upload_issuer,
            image_policy,
            SignatureProcessingPolicy::new(settings.signature_processing_timeout)?,
        );
        let revoker = Arc::new(AppleSocialConnectionRevoker::new(
            pool.clone(),
            cipher,
            apple_client,
        ));
        let withdrawal = UserWithdrawalService::new(revoker, users.clone());
        let callback_auth = ProcessingCallbackAuthenticator::new(
            &settings.callback.secret,
            &settings.image.environment,
            clock,
        )?;

        Ok(Components {
            policy: UsableUserPolicy::new(pool),
            login,
            signup,
            refresh,
            users,
            withdrawal,
            callback_auth,
        })
    }
}
