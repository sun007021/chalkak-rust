//! 토큰과 암호 도구: 액세스 토큰·회원가입 토큰 발급과 검증, 소셜 식별자 지문, 리프레시 토큰 생성·해시,
//! Apple 리프레시 토큰 암호화, Apple client secret.
//!
//! 근거 클래스: `JwtAccessTokenProvider`, `JwtSocialSignupTokenProvider`, `HmacSocialIdentityFingerprintEncoder`,
//! `SecureRandomRefreshTokenProvider`, `AesGcmAppleAuthorizationCipher`, `AppleClientSecretGenerator`,
//! `AppleAuthorizationFingerprintEncoder`.

use std::sync::Arc;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_jwt::original::{
    AccessTokenClaims, SocialSignupClaims, access_token_header, apple_client_secret_claims,
    apple_client_secret_header, social_signup_header,
};
use chalkak_jwt::{ClaimError, Es256Signer, JwtError, Validator, hs256_decode, hs256_encode};
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use super::domain::{
    GeneratedRefreshToken, IssuedAccessToken, IssuedSocialSignupToken, SocialProvider,
    VerifiedSocialIdentity, VerifiedSocialSignupToken,
};
use crate::common::{
    base64_std_decode, base64_std_encode, base64_url_encode_no_pad, hex_encode, hmac_sha256,
    java_len, java_uuid_from_string, random_bytes, sha256_hex,
};
use crate::config::TOKEN_ISSUER;
use crate::web::ApiError;

const SUBJECT_MAX_LENGTH: usize = 255;
const EMAIL_MAX_LENGTH: usize = 320;

fn to_signed(d: Duration) -> SignedDuration {
    SignedDuration::try_from(d).unwrap_or(SignedDuration::MAX)
}

/// `JwtAccessTokenProvider#issue`.
#[derive(Clone)]
pub struct AccessTokenIssuer {
    key: Vec<u8>,
    audience: String,
    expiration: Duration,
    clock: Arc<dyn Clock>,
}

impl AccessTokenIssuer {
    pub fn new(
        key: Vec<u8>,
        audience: &str,
        expiration: Duration,
        clock: Arc<dyn Clock>,
    ) -> AccessTokenIssuer {
        AccessTokenIssuer {
            key,
            audience: audience.to_owned(),
            expiration,
            clock,
        }
    }

    /// 회원 scope(`USER`) 액세스 토큰. 로그인 세션을 알 수 없는 발급(`issue(subject, scope)`)이다.
    pub fn issue(&self, user_id: Uuid) -> IssuedAccessToken {
        self.issue_with_scope(&user_id.to_string(), "USER", None)
    }

    /// 회원 scope(`USER`) 액세스 토큰에 로그인 세션 식별자(`session_id`)를 실어 낸다(`issueForSession`).
    pub fn issue_for_session(&self, user_id: Uuid, session_id: Uuid) -> IssuedAccessToken {
        self.issue_with_scope(&user_id.to_string(), "USER", Some(session_id))
    }

    pub fn issue_with_scope(
        &self,
        subject: &str,
        scope: &str,
        session_id: Option<Uuid>,
    ) -> IssuedAccessToken {
        let issued_at = self.clock.now();
        let expires_at = issued_at
            .saturating_add(to_signed(self.expiration))
            .unwrap_or(issued_at);
        let jti = Uuid::new_v4().to_string();
        let session_id = session_id.map(|id| id.to_string());
        let claims = AccessTokenClaims {
            issuer: TOKEN_ISSUER,
            audience: &self.audience,
            subject,
            issued_at: issued_at.as_second(),
            expires_at: expires_at.as_second(),
            jti: &jti,
            scope,
            session_id: session_id.as_deref(),
        };
        let value = hs256_encode(&access_token_header(), &claims.to_value(), &self.key);
        IssuedAccessToken {
            value,
            expires_in_seconds: i64::try_from(self.expiration.as_secs()).unwrap_or(i64::MAX),
        }
    }
}

/// `JwtSocialSignupTokenProvider`.
#[derive(Clone)]
pub struct SignupTokens {
    key: Vec<u8>,
    audience: String,
    lifetime: Duration,
    clock: Arc<dyn Clock>,
}

fn invalid_signup() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 회원가입 정보입니다.",
    ))
}

impl SignupTokens {
    pub fn new(
        key: Vec<u8>,
        audience: &str,
        lifetime: Duration,
        clock: Arc<dyn Clock>,
    ) -> SignupTokens {
        SignupTokens {
            key,
            audience: audience.to_owned(),
            lifetime,
            clock,
        }
    }

    pub fn issue(
        &self,
        identity: &VerifiedSocialIdentity,
        upload_id: Uuid,
    ) -> IssuedSocialSignupToken {
        let issued_at = self.clock.now();
        let expires_at = issued_at
            .saturating_add(to_signed(self.lifetime))
            .unwrap_or(issued_at);
        let jti = Uuid::new_v4().to_string();
        let upload = upload_id.to_string();
        let claims = SocialSignupClaims {
            issuer: TOKEN_ISSUER,
            audience: &self.audience,
            subject: &identity.subject,
            issued_at: issued_at.as_second(),
            expires_at: expires_at.as_second(),
            jti: &jti,
            provider: identity.provider.name(),
            upload_id: &upload,
            email: identity.email.as_deref(),
        };
        IssuedSocialSignupToken {
            value: hs256_encode(&social_signup_header(), &claims.to_value(), &self.key),
            expires_at,
        }
    }

    /// 서명·시각·발급자·대상·용도를 확인하고 클레임을 꺼낸다. 실패는 모두 401이다. 단 `aud`가 없는 토큰은 원본이
    /// `NullPointerException`으로 끝나므로 500이다.
    pub fn verify(&self, token: &str) -> Result<VerifiedSocialSignupToken, ApiError> {
        let claims = hs256_decode(token, &self.key).map_err(|_| invalid_signup())?;
        let now_ms = self.clock.now().as_millisecond();
        match Validator::social_signup_token(TOKEN_ISSUER, &self.audience).validate(&claims, now_ms)
        {
            Ok(()) => {}
            Err(ClaimError::AudienceMissing) => {
                return Err(ApiError::Internal(
                    "회원가입 토큰에 aud 클레임이 없습니다".to_owned(),
                ));
            }
            Err(_) => return Err(invalid_signup()),
        }

        let provider = claims
            .get_str("provider")
            .filter(|p| !p.chars().all(crate::web::binding::is_java_whitespace))
            .and_then(SocialProvider::from_name)
            .ok_or_else(invalid_signup)?;
        let subject = claims
            .subject()
            .filter(|s| {
                !s.chars().all(crate::web::binding::is_java_whitespace)
                    && java_len(s) <= SUBJECT_MAX_LENGTH
            })
            .ok_or_else(invalid_signup)?
            .to_owned();
        let upload_id = claims
            .get_str("uploadId")
            .filter(|u| !u.chars().all(crate::web::binding::is_java_whitespace))
            .and_then(java_uuid_from_string)
            .ok_or_else(invalid_signup)?;
        let email = match claims.get_str("email") {
            None => None,
            Some(e)
                if e.chars().all(crate::web::binding::is_java_whitespace)
                    || java_len(e) > EMAIL_MAX_LENGTH =>
            {
                return Err(invalid_signup());
            }
            Some(e) => Some(e.to_owned()),
        };
        let token_id = claims
            .jwt_id()
            .filter(|j| !j.chars().all(crate::web::binding::is_java_whitespace))
            .ok_or_else(invalid_signup)?
            .to_owned();
        let expires_at = claims
            .expires_at()
            .and_then(|s| Timestamp::from_second(s).ok())
            .ok_or_else(invalid_signup)?;
        Ok(VerifiedSocialSignupToken {
            provider,
            subject,
            upload_id,
            email,
            token_id,
            expires_at,
        })
    }
}

/// `HmacSocialIdentityFingerprintEncoder`: `HMAC_SHA256(secret, "<PROVIDER>:<subject>")`의 소문자 16진수.
#[derive(Clone)]
pub struct IdentityFingerprint {
    secret: Vec<u8>,
}

impl IdentityFingerprint {
    pub fn new(secret: Vec<u8>) -> IdentityFingerprint {
        IdentityFingerprint { secret }
    }

    pub fn encode(&self, provider: SocialProvider, subject: &str) -> String {
        hex_encode(&hmac_sha256(
            &self.secret,
            format!("{}:{}", provider.name(), subject).as_bytes(),
        ))
    }
}

/// `AppleAuthorizationFingerprintEncoder`: 암호문의 SHA-256. 탈퇴 직전 상태 대조에 쓴다.
pub fn apple_authorization_fingerprint(encrypted_refresh_token: &str) -> String {
    sha256_hex(encrypted_refresh_token.as_bytes())
}

/// `SecureRandomRefreshTokenProvider#generateToken`: 난수 32바이트를 패딩 없는 URL-safe base64로.
pub fn generate_refresh_token() -> GeneratedRefreshToken {
    let value = base64_url_encode_no_pad(&random_bytes::<32>());
    let token_hash = hash_refresh_token(&value);
    GeneratedRefreshToken { value, token_hash }
}

/// 저장용 해시(`encode`): UTF-8 바이트의 SHA-256 소문자 16진수.
pub fn hash_refresh_token(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// `AesGcmAppleAuthorizationCipher`: `base64(iv12 || ciphertext || tag16)`.
#[derive(Clone)]
pub struct AppleCipher {
    cipher: Aes256Gcm,
}

const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;

impl AppleCipher {
    pub fn new(key: &[u8; 32]) -> AppleCipher {
        AppleCipher {
            cipher: Aes256Gcm::new_from_slice(key).expect("키 길이는 32바이트다"),
        }
    }

    pub fn encrypt(&self, refresh_token: &str) -> Result<String, String> {
        if refresh_token
            .chars()
            .all(crate::web::binding::is_java_whitespace)
        {
            return Err("Apple refresh token이 필요합니다.".to_owned());
        }
        let iv = random_bytes::<IV_LEN>();
        let encrypted = self
            .cipher
            .encrypt(Nonce::from_slice(&iv), refresh_token.as_bytes())
            .map_err(|_| "Apple refresh token을 암호화할 수 없습니다.".to_owned())?;
        let mut payload = iv.to_vec();
        payload.extend_from_slice(&encrypted);
        Ok(base64_std_encode(&payload))
    }

    pub fn decrypt(&self, encrypted: &str) -> Result<String, String> {
        const FAILED: &str = "암호화된 Apple refresh token을 복호화할 수 없습니다.";
        if encrypted
            .chars()
            .all(crate::web::binding::is_java_whitespace)
        {
            return Err("암호화된 Apple refresh token이 필요합니다.".to_owned());
        }
        let payload = base64_std_decode(encrypted).ok_or_else(|| FAILED.to_owned())?;
        if payload.len() <= IV_LEN + TAG_LEN {
            return Err(FAILED.to_owned());
        }
        let (iv, body) = payload.split_at(IV_LEN);
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(iv), body)
            .map_err(|_| FAILED.to_owned())?;
        Ok(String::from_utf8_lossy(&plain).into_owned())
    }
}

/// `AppleClientSecretGenerator`: ES256 JWT, 수명 5분.
#[derive(Clone)]
pub struct AppleClientSecret {
    signer: Arc<Es256Signer>,
    team_id: String,
    key_id: String,
    client_id: String,
    clock: Arc<dyn Clock>,
}

impl AppleClientSecret {
    /// 개인키를 지금 해석해, 설정이 틀리면 첫 요청이 아니라 기동 시점에 실패하게 한다.
    pub fn new(
        private_key_base64: &str,
        team_id: &str,
        key_id: &str,
        client_id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<AppleClientSecret, JwtError> {
        Ok(AppleClientSecret {
            signer: Arc::new(Es256Signer::from_base64_pem(private_key_base64)?),
            team_id: team_id.to_owned(),
            key_id: key_id.to_owned(),
            client_id: client_id.to_owned(),
            clock,
        })
    }

    pub fn generate(&self) -> String {
        let issued_at = self.clock.now().as_second();
        self.signer.sign(
            &apple_client_secret_header(&self.key_id),
            &apple_client_secret_claims(&self.team_id, &self.client_id, issued_at),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;
    use serde_json::{Value, json};

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ACCESS_AUDIENCE: &str = "chalkak-access";
    const SIGNUP_AUDIENCE: &str = "chalkak-social-signup";
    const ENCRYPTION_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const OTHER_ENCRYPTION_KEY: &str =
        "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    const REFRESH_TOKEN: &str = "apple-refresh-token";
    const SUBJECT: &str = "google-subject";

    fn secret() -> Vec<u8> {
        crate::common::hex_decode(KEY).unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn clock_at(s: &str) -> Arc<FixedClock> {
        Arc::new(FixedClock::new(ts(s)))
    }

    fn plus(t: Timestamp, seconds: i64) -> Timestamp {
        t.checked_add(SignedDuration::from_secs(seconds)).unwrap()
    }

    // ----- JwtAccessTokenProviderTest -----

    const ACCESS_NOW: &str = "2026-08-27T00:00:00Z";
    const FIFTEEN_MINUTES: i64 = 15 * 60;

    fn access_issuer(now: &str) -> AccessTokenIssuer {
        AccessTokenIssuer::new(
            secret(),
            ACCESS_AUDIENCE,
            Duration::from_secs(900),
            clock_at(now),
        )
    }

    fn access_claims() -> Value {
        let now = ts(ACCESS_NOW);
        json!({
            "iss": TOKEN_ISSUER,
            "aud": ACCESS_AUDIENCE,
            "sub": Uuid::new_v4().to_string(),
            "iat": now.as_second(),
            "exp": plus(now, FIFTEEN_MINUTES).as_second(),
            "purpose": "ACCESS",
        })
    }

    fn encode_with_same_secret(claims: &Value) -> String {
        hs256_encode(&social_signup_header(), claims, &secret())
    }

    fn access_token_accepted(token: &str, verifier_now: &str) -> bool {
        let Ok(claims) = hs256_decode(token, &secret()) else {
            return false;
        };
        Validator::access_token(TOKEN_ISSUER, ACCESS_AUDIENCE)
            .validate(&claims, ts(verifier_now).as_millisecond())
            .is_ok()
    }

    #[test]
    fn issue_valid_user_id_issues_verifiable_access_token() {
        let user = Uuid::new_v4();
        let issued = access_issuer(ACCESS_NOW).issue(user);
        assert!(!issued.value.trim().is_empty());
        assert_eq!(issued.expires_in_seconds, FIFTEEN_MINUTES);
        assert!(access_token_accepted(&issued.value, ACCESS_NOW));
        let claims = hs256_decode(&issued.value, &secret()).unwrap();
        assert_eq!(claims.subject(), Some(user.to_string().as_str()));
        assert_eq!(claims.get_str("scope"), Some("USER"));
    }

    // JwtAccessTokenProviderTest#issueForSession: 회원 토큰에만 로그인 ID(session_id)를 싣는다.
    #[test]
    fn issue_for_session_adds_the_session_id_claim_after_the_issuer() {
        let user = Uuid::new_v4();
        let session = Uuid::new_v4();
        let issued = access_issuer(ACCESS_NOW).issue_for_session(user, session);
        assert!(access_token_accepted(&issued.value, ACCESS_NOW));
        let claims = hs256_decode(&issued.value, &secret()).unwrap();
        assert_eq!(
            claims.get_str("session_id"),
            Some(session.to_string().as_str())
        );
        assert_eq!(claims.get_str("scope"), Some("USER"));
        let keys: Vec<&str> = claims.as_map().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "sub",
                "aud",
                "purpose",
                "scope",
                "iss",
                "session_id",
                "exp",
                "iat",
                "jti"
            ]
        );
        // 로그인 ID를 모르는 발급에는 클레임 자체가 없다.
        let plain = access_issuer(ACCESS_NOW).issue(user);
        let plain_claims = hs256_decode(&plain.value, &secret()).unwrap();
        assert_eq!(plain_claims.get_str("session_id"), None);
    }

    #[test]
    fn issue_admin_scope_issues_admin_access_token() {
        let admin = Uuid::new_v4();
        let issued = access_issuer(ACCESS_NOW).issue_with_scope(&admin.to_string(), "ADMIN", None);
        let claims = hs256_decode(&issued.value, &secret()).unwrap();
        assert_eq!(claims.subject(), Some(admin.to_string().as_str()));
        assert_eq!(claims.get_str("scope"), Some("ADMIN"));
        assert!(access_token_accepted(&issued.value, ACCESS_NOW));
    }

    #[test]
    fn decode_social_signup_token_signed_with_same_secret_throws_jwt_exception() {
        let now = ts(ACCESS_NOW);
        let token = encode_with_same_secret(&json!({
            "iss": TOKEN_ISSUER,
            "aud": SIGNUP_AUDIENCE,
            "sub": SUBJECT,
            "iat": now.as_second(),
            "exp": plus(now, 300).as_second(),
            "purpose": "SOCIAL_SIGNUP",
        }));
        assert!(!access_token_accepted(&token, ACCESS_NOW));
    }

    #[test]
    fn decode_audience_mismatch_throws_jwt_exception() {
        let mut claims = access_claims();
        claims["aud"] = json!("chalkak-other");
        assert!(!access_token_accepted(
            &encode_with_same_secret(&claims),
            ACCESS_NOW
        ));
    }

    #[test]
    fn decode_purpose_mismatch_throws_jwt_exception() {
        let mut claims = access_claims();
        claims["purpose"] = json!("SOCIAL_SIGNUP");
        assert!(!access_token_accepted(
            &encode_with_same_secret(&claims),
            ACCESS_NOW
        ));
    }

    #[test]
    fn decode_missing_purpose_throws_jwt_exception() {
        let mut claims = access_claims();
        claims.as_object_mut().unwrap().remove("purpose");
        assert!(!access_token_accepted(
            &encode_with_same_secret(&claims),
            ACCESS_NOW
        ));
    }

    #[test]
    fn decode_issuer_mismatch_throws_jwt_exception() {
        let mut claims = access_claims();
        claims["iss"] = json!("attacker");
        assert!(!access_token_accepted(
            &encode_with_same_secret(&claims),
            ACCESS_NOW
        ));
    }

    #[test]
    fn decode_tampered_token_throws_jwt_exception() {
        let token = access_issuer(ACCESS_NOW).issue(Uuid::new_v4()).value;
        let start = token.rfind('.').unwrap() + 1;
        let replacement = if token.as_bytes()[start] == b'A' {
            'B'
        } else {
            'A'
        };
        let tampered = format!("{}{}{}", &token[..start], replacement, &token[start + 1..]);
        assert!(!access_token_accepted(&tampered, ACCESS_NOW));
    }

    #[test]
    fn decode_just_past_expiry_within_clock_skew_verifies_access_token() {
        let token = access_issuer(ACCESS_NOW).issue(Uuid::new_v4()).value;
        let verifier_now = plus(ts(ACCESS_NOW), FIFTEEN_MINUTES + 29).to_string();
        assert!(access_token_accepted(&token, &verifier_now));
    }

    #[test]
    fn decode_expired_beyond_clock_skew_throws_jwt_exception() {
        let token = access_issuer(ACCESS_NOW).issue(Uuid::new_v4()).value;
        let verifier_now = plus(ts(ACCESS_NOW), FIFTEEN_MINUTES + 31).to_string();
        assert!(!access_token_accepted(&token, &verifier_now));
    }

    #[test]
    fn decode_missing_expiry_throws_jwt_exception() {
        let mut claims = access_claims();
        claims.as_object_mut().unwrap().remove("exp");
        assert!(!access_token_accepted(
            &encode_with_same_secret(&claims),
            ACCESS_NOW
        ));
    }

    #[test]
    fn decode_exactly_at_clock_skew_boundary_verifies_access_token() {
        let token = access_issuer(ACCESS_NOW).issue(Uuid::new_v4()).value;
        let verifier_now = plus(ts(ACCESS_NOW), FIFTEEN_MINUTES + 30).to_string();
        assert!(access_token_accepted(&token, &verifier_now));
    }

    #[test]
    fn decode_non_uuid_subject_throws_jwt_exception() {
        let mut claims = access_claims();
        claims["sub"] = json!("google-subject");
        assert!(!access_token_accepted(
            &encode_with_same_secret(&claims),
            ACCESS_NOW
        ));
    }

    #[test]
    fn 액세스_토큰은_원본과_같은_클레임_순서와_만료를_담는다() {
        let user = Uuid::new_v4();
        let issued = access_issuer("2026-10-12T00:00:00Z").issue(user);
        assert_eq!(issued.expires_in_seconds, 900);
        let claims = hs256_decode(&issued.value, &secret()).unwrap();
        assert_eq!(claims.get_str("purpose"), Some("ACCESS"));
        assert_eq!(claims.issuer(), Some("chalkak-backend"));
        assert_eq!(claims.audience(), Some(vec!["chalkak-access"]));
        assert_eq!(claims.issued_at(), Some(1_791_763_200));
        assert_eq!(claims.expires_at(), Some(1_791_763_200 + 900));
        assert!(claims.jwt_id().is_some());
    }

    // ----- JwtSocialSignupTokenProviderTest -----

    const SIGNUP_NOW: &str = "2026-08-26T00:00:00Z";

    fn signup_tokens(now: &str) -> SignupTokens {
        signup_tokens_with(now, SIGNUP_AUDIENCE, secret())
    }

    fn signup_tokens_with(now: &str, audience: &str, key: Vec<u8>) -> SignupTokens {
        SignupTokens::new(key, audience, Duration::from_secs(300), clock_at(now))
    }

    fn identity() -> VerifiedSocialIdentity {
        VerifiedSocialIdentity {
            provider: SocialProvider::Google,
            subject: SUBJECT.into(),
            email: Some("user@chalkak.test".into()),
        }
    }

    fn assert_invalid_signup_token(result: Result<VerifiedSocialSignupToken, ApiError>) {
        let error = result.unwrap_err();
        assert_eq!(error.status(), 401);
        assert_eq!(error.message(), "유효하지 않은 회원가입 정보입니다.");
    }

    /// `createSignedToken`: 실제 발급 경로와 달리 `jti`를 채우지 않는다.
    fn signed_signup_token(
        purpose: &str,
        include_upload_id: bool,
        issued_at: Option<Timestamp>,
        expires_at: Option<Timestamp>,
        jti: Option<&str>,
    ) -> String {
        let mut claims = json!({
            "iss": TOKEN_ISSUER,
            "aud": SIGNUP_AUDIENCE,
            "sub": SUBJECT,
            "purpose": purpose,
            "provider": "GOOGLE",
        });
        let object = claims.as_object_mut().unwrap();
        if let Some(t) = issued_at {
            object.insert("iat".into(), json!(t.as_second()));
        }
        if let Some(t) = expires_at {
            object.insert("exp".into(), json!(t.as_second()));
        }
        if include_upload_id {
            object.insert("uploadId".into(), json!(Uuid::new_v4().to_string()));
        }
        if let Some(jti) = jti {
            object.insert("jti".into(), json!(jti));
        }
        encode_with_same_secret(&claims)
    }

    fn signup_now() -> Timestamp {
        ts(SIGNUP_NOW)
    }

    #[test]
    fn issue_valid_identity_and_upload_id_verifies_signup_token() {
        let tokens = signup_tokens(SIGNUP_NOW);
        let upload = Uuid::new_v4();
        let issued = tokens.issue(&identity(), upload);
        let verified = tokens.verify(&issued.value).unwrap();
        assert!(!issued.value.is_empty());
        assert_eq!(verified.provider, SocialProvider::Google);
        assert_eq!(verified.subject, SUBJECT);
        assert_eq!(verified.upload_id, upload);
        assert_eq!(verified.email.as_deref(), Some("user@chalkak.test"));
        assert!(!verified.token_id.trim().is_empty());
        assert_eq!(verified.expires_at, plus(signup_now(), 300));
        assert_eq!(issued.expires_at, plus(signup_now(), 300));
    }

    #[test]
    fn issue_apple_identity_contains_no_refresh_token() {
        let tokens = signup_tokens(SIGNUP_NOW);
        let apple = VerifiedSocialIdentity {
            provider: SocialProvider::Apple,
            subject: "apple-subject".into(),
            email: Some("user@privaterelay.appleid.com".into()),
        };
        let upload = Uuid::new_v4();
        let issued = tokens.issue(&apple, upload);
        let verified = tokens.verify(&issued.value).unwrap();
        let payload_part = issued.value.split('.').nth(1).unwrap();
        let payload = String::from_utf8(
            base64_std_decode(&payload_part.replace('-', "+").replace('_', "/")).unwrap(),
        )
        .unwrap();
        assert_eq!(verified.provider, SocialProvider::Apple);
        assert_eq!(verified.subject, "apple-subject");
        assert_eq!(verified.upload_id, upload);
        assert!(!verified.token_id.trim().is_empty());
        assert!(!payload.contains("appleEncryptedRefreshToken"));
        assert!(!payload.contains("encrypted-apple-refresh-token"));
    }

    #[test]
    fn issue_called_twice_generates_distinct_token_ids() {
        let tokens = signup_tokens(SIGNUP_NOW);
        let first = tokens
            .verify(&tokens.issue(&identity(), Uuid::new_v4()).value)
            .unwrap();
        let second = tokens
            .verify(&tokens.issue(&identity(), Uuid::new_v4()).value)
            .unwrap();
        assert_ne!(first.token_id, second.token_id);
    }

    #[test]
    fn issue_null_email_verifies_signup_token_without_email() {
        let tokens = signup_tokens(SIGNUP_NOW);
        let mut no_email = identity();
        no_email.email = None;
        let issued = tokens.issue(&no_email, Uuid::new_v4());
        assert_eq!(tokens.verify(&issued.value).unwrap().email, None);
    }

    #[test]
    fn verify_tampered_token_throws_unauthorized_exception() {
        let tokens = signup_tokens(SIGNUP_NOW);
        let token = tokens.issue(&identity(), Uuid::new_v4()).value;
        let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
        let replacement = if parts[1].starts_with('A') { 'B' } else { 'A' };
        parts[1] = format!("{replacement}{}", &parts[1][1..]);
        assert_invalid_signup_token(tokens.verify(&parts.join(".")));
    }

    #[test]
    fn verify_expired_token_throws_unauthorized_exception() {
        let token = signup_tokens(SIGNUP_NOW)
            .issue(&identity(), Uuid::new_v4())
            .value;
        let later = plus(signup_now(), 300 + 31).to_string();
        assert_invalid_signup_token(signup_tokens(&later).verify(&token));
    }

    #[test]
    fn verify_wrong_issuer_throws_unauthorized_exception() {
        // 발급자가 다른 토큰: 같은 키로 서명한 임의의 발급자 클레임.
        let token = encode_with_same_secret(&json!({
            "iss": "other-backend",
            "aud": SIGNUP_AUDIENCE,
            "sub": SUBJECT,
            "purpose": "SOCIAL_SIGNUP",
            "provider": "GOOGLE",
            "uploadId": Uuid::new_v4().to_string(),
            "jti": Uuid::new_v4().to_string(),
            "iat": signup_now().as_second(),
            "exp": plus(signup_now(), 300).as_second(),
        }));
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_wrong_audience_throws_unauthorized_exception() {
        let token = signup_tokens_with(SIGNUP_NOW, "other-audience", secret())
            .issue(&identity(), Uuid::new_v4())
            .value;
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_wrong_purpose_throws_unauthorized_exception() {
        let token = signed_signup_token(
            "OTHER_PURPOSE",
            true,
            Some(signup_now()),
            Some(plus(signup_now(), 1800)),
            Some(&Uuid::new_v4().to_string()),
        );
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_missing_token_id_throws_unauthorized_exception() {
        let token = signed_signup_token(
            "SOCIAL_SIGNUP",
            true,
            Some(signup_now()),
            Some(plus(signup_now(), 1800)),
            None,
        );
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_missing_upload_id_throws_unauthorized_exception() {
        let token = signed_signup_token(
            "SOCIAL_SIGNUP",
            false,
            Some(signup_now()),
            Some(plus(signup_now(), 1800)),
            Some(&Uuid::new_v4().to_string()),
        );
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_missing_issued_at_throws_unauthorized_exception() {
        let token = signed_signup_token(
            "SOCIAL_SIGNUP",
            true,
            None,
            Some(plus(signup_now(), 1800)),
            Some(&Uuid::new_v4().to_string()),
        );
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_missing_expires_at_throws_unauthorized_exception() {
        let token = signed_signup_token(
            "SOCIAL_SIGNUP",
            true,
            Some(signup_now()),
            None,
            Some(&Uuid::new_v4().to_string()),
        );
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn verify_issued_at_beyond_clock_skew_throws_unauthorized_exception() {
        let token = signed_signup_token(
            "SOCIAL_SIGNUP",
            true,
            Some(plus(signup_now(), 31)),
            Some(plus(signup_now(), 1800)),
            Some(&Uuid::new_v4().to_string()),
        );
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token));
    }

    #[test]
    fn 허용_오차_안의_만료와_다른_키_다른_대상_형식_오류() {
        let c = clock_at("2026-10-12T00:00:00Z");
        let tokens = SignupTokens::new(
            secret(),
            SIGNUP_AUDIENCE,
            Duration::from_secs(300),
            c.clone(),
        );
        let issued = tokens.issue(&identity(), Uuid::new_v4());
        assert_eq!(issued.expires_at, ts("2026-10-12T00:05:00Z"));
        c.advance(Duration::from_secs(300 + 30));
        assert!(tokens.verify(&issued.value).is_ok());
        c.advance(Duration::from_secs(1));
        assert_invalid_signup_token(tokens.verify(&issued.value));

        let other = signup_tokens_with("2026-10-12T00:00:00Z", SIGNUP_AUDIENCE, vec![1; 32]);
        let fresh = signup_tokens_with("2026-10-12T00:00:00Z", SIGNUP_AUDIENCE, secret())
            .issue(&identity(), Uuid::new_v4());
        assert_invalid_signup_token(other.verify(&fresh.value));
        assert_invalid_signup_token(tokens.verify("not-a-jwt"));
    }

    #[test]
    fn 액세스_토큰은_회원가입_토큰으로_쓸_수_없다() {
        let token = access_issuer(SIGNUP_NOW).issue(Uuid::new_v4());
        assert_invalid_signup_token(signup_tokens(SIGNUP_NOW).verify(&token.value));
    }

    // ----- HmacSocialIdentityFingerprintEncoderTest, AppleAuthorizationFingerprintEncoderTest -----

    #[test]
    fn encode_provider_and_subject_returns_hmac_sha256_fingerprint() {
        let encoder = IdentityFingerprint::new(secret());
        assert_eq!(
            encoder.encode(SocialProvider::Google, SUBJECT),
            "921c5d35312df654eaa8ec114fd1de5a156cbcc64b23ddb6a709a9423f90c218"
        );
    }

    #[test]
    fn 소셜_식별자_지문은_제공자와_subject에_따라_다르다() {
        let enc = IdentityFingerprint::new(vec![7; 32]);
        let a = enc.encode(SocialProvider::Google, "sub-1");
        assert_eq!(a.len(), 64);
        assert_eq!(a, enc.encode(SocialProvider::Google, "sub-1"));
        assert_ne!(a, enc.encode(SocialProvider::Kakao, "sub-1"));
        assert_ne!(a, enc.encode(SocialProvider::Google, "sub-2"));
        assert_eq!(a, hex_encode(&hmac_sha256(&[7; 32], b"GOOGLE:sub-1")));
    }

    #[test]
    fn encode_encrypted_refresh_token_returns_sha256_fingerprint() {
        assert_eq!(
            apple_authorization_fingerprint("encrypted-refresh-token"),
            "fa52a3a1ce506d39fc04ecb7ea2fde99ba09f5839ba1062e9f6c2b2aaafb1d9d"
        );
    }

    // ----- SecureRandomRefreshTokenProviderTest -----

    fn is_base64url_43(text: &str) -> bool {
        text.len() == 43
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    #[test]
    fn generate_token_always_returns_base64_url_value_without_padding() {
        assert!(is_base64url_43(&generate_refresh_token().value));
    }

    #[test]
    fn generate_token_called_twice_returns_different_values() {
        let first = generate_refresh_token();
        let second = generate_refresh_token();
        assert_ne!(first.value, second.value);
        assert_ne!(first.token_hash, second.token_hash);
    }

    #[test]
    fn generate_token_always_returns_hash_of_value() {
        let generated = generate_refresh_token();
        assert_eq!(generated.token_hash.len(), 64);
        assert!(
            generated
                .token_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(generated.token_hash, hash_refresh_token(&generated.value));
    }

    #[test]
    fn encode_same_token_returns_same_hash() {
        let token = generate_refresh_token().value;
        assert_eq!(hash_refresh_token(&token), hash_refresh_token(&token));
    }

    #[test]
    fn encode_different_tokens_returns_different_hashes() {
        assert_ne!(
            hash_refresh_token("refresh-token-a"),
            hash_refresh_token("refresh-token-b")
        );
        assert_eq!(
            hash_refresh_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // ----- AesGcmAppleAuthorizationCipherTest -----

    fn cipher(key_hex: &str) -> AppleCipher {
        let key: [u8; 32] = crate::common::hex_decode(key_hex)
            .unwrap()
            .try_into()
            .unwrap();
        AppleCipher::new(&key)
    }

    const DECRYPT_FAILED: &str = "암호화된 Apple refresh token을 복호화할 수 없습니다.";

    #[test]
    fn decrypt_encrypted_refresh_token_returns_original_refresh_token() {
        let c = cipher(ENCRYPTION_KEY);
        let encrypted = c.encrypt(REFRESH_TOKEN).unwrap();
        assert_eq!(c.decrypt(&encrypted).unwrap(), REFRESH_TOKEN);
        assert_eq!(
            base64_std_decode(&encrypted).unwrap().len(),
            12 + REFRESH_TOKEN.len() + 16
        );
    }

    #[test]
    fn encrypt_same_refresh_token_generates_different_ciphertext() {
        let c = cipher(ENCRYPTION_KEY);
        assert_ne!(
            c.encrypt(REFRESH_TOKEN).unwrap(),
            c.encrypt(REFRESH_TOKEN).unwrap()
        );
    }

    #[test]
    fn decrypt_with_different_key_throws_exception() {
        let encrypted = cipher(ENCRYPTION_KEY).encrypt(REFRESH_TOKEN).unwrap();
        assert_eq!(
            cipher(OTHER_ENCRYPTION_KEY)
                .decrypt(&encrypted)
                .unwrap_err(),
            DECRYPT_FAILED
        );
    }

    #[test]
    fn decrypt_tampered_ciphertext_throws_exception() {
        let c = cipher(ENCRYPTION_KEY);
        let mut payload = base64_std_decode(&c.encrypt(REFRESH_TOKEN).unwrap()).unwrap();
        let last = payload.len() - 1;
        payload[last] ^= 1;
        assert_eq!(
            c.decrypt(&base64_std_encode(&payload)).unwrap_err(),
            DECRYPT_FAILED
        );
    }

    #[test]
    fn encrypt_missing_refresh_token_throws_exception() {
        let c = cipher(ENCRYPTION_KEY);
        for blank in ["", "   "] {
            assert_eq!(
                c.encrypt(blank).unwrap_err(),
                "Apple refresh token이 필요합니다."
            );
        }
    }

    #[test]
    fn decrypt_missing_encrypted_refresh_token_throws_exception() {
        let c = cipher(ENCRYPTION_KEY);
        for blank in ["", "   "] {
            assert_eq!(
                c.decrypt(blank).unwrap_err(),
                "암호화된 Apple refresh token이 필요합니다."
            );
        }
    }

    #[test]
    fn decrypt_invalid_base64_throws_exception() {
        assert_eq!(
            cipher(ENCRYPTION_KEY)
                .decrypt("invalid-base64")
                .unwrap_err(),
            DECRYPT_FAILED
        );
        // 너무 짧은 페이로드
        assert!(cipher(ENCRYPTION_KEY).decrypt("AAAA").is_err());
    }

    // ----- AppleClientSecretGeneratorTest -----

    fn test_private_key() -> &'static str {
        crate::config::TEST_ENV
            .iter()
            .find(|(k, _)| *k == "APPLE_PRIVATE_KEY_BASE64")
            .unwrap()
            .1
    }

    /// RSA 개인키 PEM(PKCS8)을 base64로 감싼 값. EC 키가 아니다.
    const RSA_PRIVATE_KEY_BASE64: &str = "LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tCk1JSUNkZ0lCQURBTkJna3Foa2lHOXcwQkFRRUZBQVNDQW1Bd2dnSmNBZ0VBQW9HQkFOQlluOEdYb0pLYU1JUi8KUGpadEtOeTMwTC81bGRnUjkreUVoaTl4a2NIY1JKRVB1TlBaamdEMjkyMktVb3lHMWprWFdGa1RZcFhub1BBegpNM2JBRDVUTTFTWHFjMCtNdCtHMTRRc25rVkNlSXZ3ZmNqVUNlRXlPeml2ZEtvRFZSQm1JZngzcHA3R29WMlY3CkNhaEtEUll4R2VwaCtBMStFRjhsZHNXQlpFSHBBZ01CQUFFQ2dZQmlESE1TYmtNWSt3cytMNHVkblgyUGNjb0IKcm5tQU4rU1BJRWJ0Nk9xSmF4bXhrczZPbEdHcHZvL3VzeHZxR2tLU2w1UHlSNWNycU5ncWNrNFQ0b3J4eUgwcApDa2N6Z2tFSXQyZ2ZqK3Eyd0RjcjZxR25oVmlzVXBwaDl1MmdIYTdxUEV1YUh6aVFyM2JKZ1RFaDVOa1BKbGE1ClI2b1NMSGd0dUU1NDBnbDhDUUpCQU83TlcyQjNDZ0JwaXdISXYvaWlGS2plTkdsNVg3L1RxVmFvMEtKR3RYSWkKYnBRUnd2UDk5Qm5yRVhrU2xtNzl1SElyd3FwV01MZnN4R3VMNnNKOFd5Y0NRUURmV2NUL3Y4T0UzVWVUbWF2NwpYczFmY3BqWlBFd1VnZytjanRVdlRsQVE2b1ZUMVQxQkNwTEdwM3d5djlpLzA2L0NLeUlBNFUvUUI3cnRla3JICnB1UnZBa0J3dWo0dGJtUXBSNmxZQVVVbUFKVVNGanJWbzNjZ25iRXRlR2xzZ1VaOEIyYjl2Tml6eHlqbzV6K0wKVU12MHZ3N0h6b0tKV0VDTlFsYUNrajBTN0JNM0FrRUF4N0JaQTFlT0ZXSkp0bHQwZWovZVduSDQybkNnak0xago2OU9BSXRWZm5HQXhhQ3lFb1h3MGtrdjRMTjk5aUxSWXRiOGVuYTdVVHVnNlprd3FRNnFoSFFKQVNCeE1ycFNTCjdIZmJvbDNHODR0dEVobUV3bkN5bCtUcUExRGdiSUZtMDY3ZmtQZ29WWnBSNWhTb0RNQU9PTld1SjEwaG45NFcKZWIra0x4SjZ3S0E0T1E9PQotLS0tLUVORCBQUklWQVRFIEtFWS0tLS0tCg==";

    fn base64url_text(part: &str) -> String {
        String::from_utf8(base64_std_decode(&part.replace('-', "+").replace('_', "/")).unwrap())
            .unwrap()
    }

    #[test]
    fn generate_valid_private_key_returns_signed_client_secret() {
        let secret = AppleClientSecret::new(
            test_private_key(),
            "APPLETEAM1",
            "APPLEKEY01",
            "com.chalkak.ios",
            clock_at("2026-09-03T00:00:00Z"),
        )
        .unwrap();
        let token = secret.generate();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            base64url_text(parts[0]),
            r#"{"kid":"APPLEKEY01","alg":"ES256"}"#
        );
        let iat = ts("2026-09-03T00:00:00Z").as_second();
        assert_eq!(
            base64url_text(parts[1]),
            format!(
                r#"{{"iss":"APPLETEAM1","sub":"com.chalkak.ios","aud":"https://appleid.apple.com","exp":{},"iat":{}}}"#,
                iat + 300,
                iat
            )
        );
        // 서명은 JOSE 형식 r||s 64바이트다.
        assert_eq!(
            base64_std_decode(&parts[2].replace('-', "+").replace('_', "/"))
                .unwrap()
                .len(),
            64
        );
    }

    fn client_secret_with(key: &str) -> Result<AppleClientSecret, JwtError> {
        AppleClientSecret::new(key, "t", "k", "c", clock_at("2026-09-03T00:00:00Z"))
    }

    #[test]
    fn create_invalid_base64_throws_illegal_state_exception() {
        assert!(client_secret_with("not-base64").is_err());
    }

    #[test]
    fn create_invalid_pem_throws_illegal_state_exception() {
        // base64("not-pem")
        assert!(client_secret_with("bm90LXBlbQ==").is_err());
    }

    #[test]
    fn create_non_ec_private_key_throws_illegal_state_exception() {
        assert!(client_secret_with(RSA_PRIVATE_KEY_BASE64).is_err());
    }
}
