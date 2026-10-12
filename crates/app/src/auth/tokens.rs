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

    /// 회원 scope(`USER`) 액세스 토큰.
    pub fn issue(&self, user_id: Uuid) -> IssuedAccessToken {
        self.issue_with_scope(&user_id.to_string(), "USER")
    }

    pub fn issue_with_scope(&self, subject: &str, scope: &str) -> IssuedAccessToken {
        let issued_at = self.clock.now();
        let expires_at = issued_at
            .saturating_add(to_signed(self.expiration))
            .unwrap_or(issued_at);
        let jti = Uuid::new_v4().to_string();
        let claims = AccessTokenClaims {
            issuer: TOKEN_ISSUER,
            audience: &self.audience,
            subject,
            issued_at: issued_at.as_second(),
            expires_at: expires_at.as_second(),
            jti: &jti,
            scope,
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

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn clock() -> Arc<FixedClock> {
        Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()))
    }

    fn signup_tokens(clock: Arc<FixedClock>) -> SignupTokens {
        SignupTokens::new(
            crate::common::hex_decode(KEY).unwrap(),
            "chalkak-social-signup",
            Duration::from_secs(300),
            clock,
        )
    }

    fn identity() -> VerifiedSocialIdentity {
        VerifiedSocialIdentity {
            provider: SocialProvider::Apple,
            subject: "apple-subject".into(),
            email: Some("user@example.com".into()),
        }
    }

    // JwtSocialSignupTokenProviderTest: 발급한 토큰을 검증하면 같은 값을 돌려준다.
    #[test]
    fn 회원가입_토큰을_발급하고_검증한다() {
        let c = clock();
        let tokens = signup_tokens(c.clone());
        let upload = Uuid::new_v4();
        let issued = tokens.issue(&identity(), upload);
        assert_eq!(issued.expires_at, "2026-10-12T00:05:00Z".parse().unwrap());
        let verified = tokens.verify(&issued.value).unwrap();
        assert_eq!(verified.provider, SocialProvider::Apple);
        assert_eq!(verified.subject, "apple-subject");
        assert_eq!(verified.upload_id, upload);
        assert_eq!(verified.email.as_deref(), Some("user@example.com"));
        assert_eq!(verified.expires_at, issued.expires_at);
        assert_eq!(
            Uuid::parse_str(&verified.token_id)
                .unwrap()
                .get_version_num(),
            4
        );
    }

    #[test]
    fn 이메일이_없으면_클레임을_넣지_않는다() {
        let tokens = signup_tokens(clock());
        let mut id = identity();
        id.email = None;
        let issued = tokens.issue(&id, Uuid::new_v4());
        assert_eq!(tokens.verify(&issued.value).unwrap().email, None);
    }

    // JwtSocialSignupTokenProviderTest: 만료·변조·다른 키는 401
    #[test]
    fn 만료되거나_변조된_토큰은_401이다() {
        let c = clock();
        let tokens = signup_tokens(c.clone());
        let issued = tokens.issue(&identity(), Uuid::new_v4());
        // 허용 오차 30초 안에서는 통과, 넘으면 만료
        c.advance(Duration::from_secs(300 + 30));
        assert!(tokens.verify(&issued.value).is_ok());
        c.advance(Duration::from_secs(1));
        let err = tokens.verify(&issued.value).unwrap_err();
        assert_eq!(err.status(), 401);
        assert_eq!(err.message(), "유효하지 않은 회원가입 정보입니다.");

        let other = SignupTokens::new(
            vec![1; 32],
            "chalkak-social-signup",
            Duration::from_secs(300),
            clock(),
        );
        let issued = tokens.issue(&identity(), Uuid::new_v4());
        assert_eq!(other.verify(&issued.value).unwrap_err().status(), 401);
        assert_eq!(tokens.verify("not-a-jwt").unwrap_err().status(), 401);
    }

    #[test]
    fn 다른_대상의_토큰은_거절한다() {
        let c = clock();
        let tokens = signup_tokens(c.clone());
        let access = AccessTokenIssuer::new(
            crate::common::hex_decode(KEY).unwrap(),
            "chalkak-access",
            Duration::from_secs(900),
            c,
        );
        let token = access.issue(Uuid::new_v4());
        assert_eq!(tokens.verify(&token.value).unwrap_err().status(), 401);
    }

    // JwtAccessTokenProviderTest: 클레임 구성
    #[test]
    fn 액세스_토큰은_회원_scope와_만료를_담는다() {
        let c = clock();
        let issuer = AccessTokenIssuer::new(
            crate::common::hex_decode(KEY).unwrap(),
            "chalkak-access",
            Duration::from_secs(900),
            c,
        );
        let user = Uuid::new_v4();
        let issued = issuer.issue(user);
        assert_eq!(issued.expires_in_seconds, 900);
        let claims = hs256_decode(&issued.value, &crate::common::hex_decode(KEY).unwrap()).unwrap();
        assert_eq!(claims.subject(), Some(user.to_string().as_str()));
        assert_eq!(claims.get_str("scope"), Some("USER"));
        assert_eq!(claims.get_str("purpose"), Some("ACCESS"));
        assert_eq!(claims.issuer(), Some("chalkak-backend"));
        assert_eq!(claims.audience(), Some(vec!["chalkak-access"]));
        assert_eq!(claims.issued_at(), Some(1_791_763_200));
        assert_eq!(claims.expires_at(), Some(1_791_763_200 + 900));
        Validator::access_token("chalkak-backend", "chalkak-access")
            .validate(&claims, 1_791_763_200_000)
            .unwrap();
    }

    // HmacSocialIdentityFingerprintEncoder
    #[test]
    fn 소셜_식별자_지문은_제공자와_subject에_따라_다르다() {
        let enc = IdentityFingerprint::new(vec![7; 32]);
        let a = enc.encode(SocialProvider::Google, "sub-1");
        assert_eq!(a.len(), 64);
        assert_eq!(a, enc.encode(SocialProvider::Google, "sub-1"));
        assert_ne!(a, enc.encode(SocialProvider::Kakao, "sub-1"));
        assert_ne!(a, enc.encode(SocialProvider::Google, "sub-2"));
        // HMAC-SHA256(key=0x07*32, "GOOGLE:sub-1")
        assert_eq!(a, hex_encode(&hmac_sha256(&[7; 32], b"GOOGLE:sub-1")));
    }

    // SecureRandomRefreshTokenProvider
    #[test]
    fn 리프레시_토큰은_난수이고_해시로만_저장한다() {
        let a = generate_refresh_token();
        let b = generate_refresh_token();
        assert_ne!(a.value, b.value);
        assert_eq!(a.value.len(), 43);
        assert!(!a.value.contains(['=', '+', '/']));
        assert_eq!(a.token_hash, hash_refresh_token(&a.value));
        assert_eq!(a.token_hash.len(), 64);
        assert_eq!(
            hash_refresh_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // AesGcmAppleAuthorizationCipherTest
    #[test]
    fn apple_토큰을_암호화하고_복호화한다() {
        let cipher = AppleCipher::new(&[9; 32]);
        let a = cipher.encrypt("refresh-token").unwrap();
        let b = cipher.encrypt("refresh-token").unwrap();
        assert_ne!(a, b, "iv가 매번 다르다");
        assert_eq!(cipher.decrypt(&a).unwrap(), "refresh-token");
        assert_eq!(
            base64_std_decode(&a).unwrap().len(),
            12 + "refresh-token".len() + 16
        );
        assert!(cipher.encrypt("  ").is_err());
        assert!(cipher.decrypt("").is_err());
        assert!(cipher.decrypt("AAAA").is_err(), "너무 짧다");
        // 다른 키로는 복호화할 수 없다.
        assert!(AppleCipher::new(&[8; 32]).decrypt(&a).is_err());
        // 변조 거부
        let mut bytes = base64_std_decode(&a).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(cipher.decrypt(&base64_std_encode(&bytes)).is_err());
    }

    // AppleClientSecretGeneratorTest
    #[test]
    fn client_secret은_5분_수명의_es256_토큰이다() {
        let key = crate::config::TEST_ENV
            .iter()
            .find(|(k, _)| *k == "APPLE_PRIVATE_KEY_BASE64")
            .unwrap()
            .1;
        let secret = AppleClientSecret::new(key, "team", "kid", "client", clock()).unwrap();
        let token = secret.generate();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = String::from_utf8(
            base64_std_decode(&parts[0].replace('-', "+").replace('_', "/")).unwrap(),
        )
        .unwrap();
        assert_eq!(header, r#"{"kid":"kid","alg":"ES256"}"#);
        let payload = String::from_utf8(
            base64_std_decode(&parts[1].replace('-', "+").replace('_', "/")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            payload,
            r#"{"iss":"team","sub":"client","aud":"https://appleid.apple.com","exp":1791763500,"iat":1791763200}"#
        );
        assert!(AppleClientSecret::new("bm90LWEta2V5", "t", "k", "c", clock()).is_err());
    }
}
