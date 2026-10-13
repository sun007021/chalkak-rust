//! 제공자 ID Token 검증. 원본 `OidcIdTokenVerifier`, `OidcEmailPolicy`, `OidcIdTokenDecoderFactory`.
//!
//! 서명·발급자·aud·시각은 `chalkak-jwt`(RS256 + JWKS 캐시)가 검증하고, 여기서는 nonce와 사용자 식별 정보를 확인한다.
//! 검증 실패는 모두 401이다(원본은 `JwtException`을 `UnauthorizedException`으로 바꾼다).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::Logger;
use chalkak_http::client::HttpClient;
use chalkak_jwt::{
    Claims, JWKS_ACCEPT, JWKS_CONNECT_TIMEOUT, JWKS_READ_TIMEOUT, JwksCache, JwksCacheConfig,
    JwksEvents, JwksFetchError, JwksSource, Validator,
};
use serde_json::Value;

use super::domain::{SocialProvider, VerifiedSocialIdentity};
use crate::common::{java_len, sha256_hex};
use crate::config::{AuthSettings, OidcProvider};
use crate::web::ApiError;
use crate::web::binding::is_java_whitespace;

const SUBJECT_MAX_LENGTH: usize = 255;

/// `IdTokenVerifier`.
pub trait IdTokenVerifier: Send + Sync {
    fn provider(&self) -> SocialProvider;
    fn verify(&self, id_token: &str, raw_nonce: &str) -> Result<VerifiedSocialIdentity, ApiError>;
}

/// `SocialIdentityVerifier`: 제공자에 맞는 검증기로 보낸다.
#[derive(Clone)]
pub struct SocialIdentityVerifier {
    verifiers: Vec<Arc<dyn IdTokenVerifier>>,
}

impl SocialIdentityVerifier {
    /// 같은 제공자의 검증기가 둘 이상이면 만들 수 없다(원본 생성자의 `IllegalStateException`).
    pub fn new(verifiers: Vec<Arc<dyn IdTokenVerifier>>) -> Result<SocialIdentityVerifier, String> {
        for (index, verifier) in verifiers.iter().enumerate() {
            if verifiers[..index]
                .iter()
                .any(|other| other.provider() == verifier.provider())
            {
                return Err(format!(
                    "같은 제공자의 ID Token 검증기가 중복 등록되었습니다: {}",
                    verifier.provider().name()
                ));
            }
        }
        Ok(SocialIdentityVerifier { verifiers })
    }

    /// 설정에 맞춘 Google·Kakao·Apple 검증기.
    pub fn from_settings(
        auth: &AuthSettings,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
    ) -> Result<SocialIdentityVerifier, String> {
        let mut verifiers: Vec<Arc<dyn IdTokenVerifier>> = Vec::new();
        for (provider, settings) in [
            (SocialProvider::Google, &auth.google),
            (SocialProvider::Kakao, &auth.kakao),
            (SocialProvider::Apple, &auth.apple_oidc),
        ] {
            verifiers.push(Arc::new(
                OidcIdTokenVerifier::new(
                    provider,
                    settings,
                    email_policy_for(provider),
                    Arc::clone(&clock),
                )?
                .with_logger(Arc::clone(&logger)),
            ));
        }
        SocialIdentityVerifier::new(verifiers)
    }

    /// 등록된 제공자(등록 순서).
    pub fn providers(&self) -> Vec<SocialProvider> {
        self.verifiers.iter().map(|v| v.provider()).collect()
    }

    pub fn verify(
        &self,
        provider: SocialProvider,
        id_token: &str,
        raw_nonce: &str,
    ) -> Result<VerifiedSocialIdentity, ApiError> {
        let verifier = self
            .verifiers
            .iter()
            .find(|v| v.provider() == provider)
            .ok_or_else(|| {
                ApiError::App(AppError::business(
                    ErrorCode::BUSINESS_ERROR,
                    "지원하지 않는 소셜 로그인 제공자입니다.",
                ))
            })?;
        verifier.verify(id_token, raw_nonce)
    }
}

/// `OidcEmailPolicy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailPolicy {
    /// `email_verified`가 true(불리언 또는 문자열 "true", 대소문자 무시)일 때만 이메일을 쓴다.
    VerifiedOnly,
    /// 제공자가 준 이메일을 그대로 쓴다.
    AsProvided,
}

/// 제공자별 이메일 정책. Kakao ID Token에는 `email_verified`가 없어 받은 이메일을 그대로 쓴다.
pub fn email_policy_for(provider: SocialProvider) -> EmailPolicy {
    match provider {
        SocialProvider::Kakao => EmailPolicy::AsProvided,
        SocialProvider::Google | SocialProvider::Apple => EmailPolicy::VerifiedOnly,
    }
}

/// `Jwt.getClaimAsString`: 문자열이 아닌 스칼라는 문자열로 바꾼다.
fn claim_as_string(claims: &Claims, name: &str) -> Option<String> {
    match claims.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl EmailPolicy {
    pub fn extract_email(self, claims: &Claims) -> Option<String> {
        match self {
            EmailPolicy::VerifiedOnly => {
                let verified = match claims.get("email_verified") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
                    _ => false,
                };
                if verified {
                    claim_as_string(claims, "email")
                } else {
                    None
                }
            }
            EmailPolicy::AsProvided => claim_as_string(claims, "email"),
        }
    }
}

/// JWKS를 HTTP로 가져온다(`DefaultResourceRetriever`: `Accept`, 연결·읽기 각 0.5초).
pub struct HttpJwksSource {
    client: HttpClient,
    url: String,
}

impl HttpJwksSource {
    pub fn new(url: &str) -> Result<HttpJwksSource, String> {
        // `OidcIdTokenDecoderFactory#createJwkSource`: URI로 해석되지 않거나 절대 URL이 아니면 기동하지 않는다.
        let scheme_ok = url.starts_with("http://") || url.starts_with("https://");
        if !scheme_ok || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("JWK Set URI 형식이 올바르지 않습니다: {url}"));
        }
        let client = HttpClient::builder()
            .connect_timeout(JWKS_CONNECT_TIMEOUT)
            .read_timeout(JWKS_READ_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(HttpJwksSource {
            client,
            url: url.to_owned(),
        })
    }
}

impl JwksSource for HttpJwksSource {
    fn fetch(&self) -> Result<String, JwksFetchError> {
        let response = self
            .client
            .get(&self.url, &[("Accept", JWKS_ACCEPT)])
            .map_err(|e| JwksFetchError::new(e.to_string()))?;
        if !response.is_success() {
            return Err(JwksFetchError::new(format!(
                "HTTP {}를 받았습니다",
                response.status
            )));
        }
        String::from_utf8(response.body).map_err(|e| JwksFetchError::new(e.to_string()))
    }
}

/// 앱 시계를 JWKS 캐시의 시계로 쓴다.
struct JwksClock(Arc<dyn Clock>);

impl chalkak_jwt::Clock for JwksClock {
    fn now_ms(&self) -> i64 {
        self.0.now().as_millisecond()
    }
}

const EVENT_LOGGER_NAME: &str =
    "com.chalkak.backend.auth.infrastructure.infra.oidc.OidcJwkSetEventLogger";

/// 재조회 제한 알림을 이 간격마다 한 번만 남긴다(원본 `JWK_SET_REFETCH_INTERVAL`).
const RATE_LIMITED_LOG_INTERVAL_MS: i64 = 30_000;

/// 원본 `OidcJwkSetEventLogger`. 공개키 목록 조회가 정상 흐름을 벗어난 순간을 제공자 이름과 함께 경고로 남긴다.
/// 검증 실패는 401로만 바뀌어 원인이 남지 않으므로, 재시도·이전 목록 사용·재조회 제한을 운영 중에 구분하게 한다.
/// 재시도와 이전 목록 사용은 재조회 제한 아래에서만 일어나 횟수가 제한되지만, 재조회 제한 알림은 넘친 요청마다
/// 오므로 임의 kid 토큰을 반복해 보내도 로그가 쌓이지 않게 구간마다 한 번만 남긴다.
struct OidcJwkSetEventLogger {
    provider: SocialProvider,
    logger: Arc<Logger>,
    clock: Arc<dyn Clock>,
    next_rate_limited_log_at: AtomicI64,
}

impl OidcJwkSetEventLogger {
    fn new(provider: SocialProvider, logger: Arc<Logger>, clock: Arc<dyn Clock>) -> Self {
        OidcJwkSetEventLogger {
            provider,
            logger,
            clock,
            next_rate_limited_log_at: AtomicI64::new(i64::MIN),
        }
    }
}

impl JwksEvents for OidcJwkSetEventLogger {
    fn retrial(&self, cause: &str) {
        self.logger.warn(EVENT_LOGGER_NAME).log(&format!(
            "{} 공개키 목록 조회에 실패해 한 번 더 시도합니다. cause={cause}",
            self.provider.name()
        ));
    }

    fn outage(&self, remaining_ms: i64, cause: &str) {
        self.logger.warn(EVENT_LOGGER_NAME).log(&format!(
            "{} 공개키 목록을 받지 못해 이전에 받은 목록을 사용합니다. remainingMillis={remaining_ms}, cause={cause}",
            self.provider.name()
        ));
    }

    fn rate_limited(&self) {
        let now = self.clock.now().as_millisecond();
        let next = self.next_rate_limited_log_at.load(Ordering::SeqCst);
        if now < next {
            return;
        }
        let following = now.saturating_add(RATE_LIMITED_LOG_INTERVAL_MS);
        if self
            .next_rate_limited_log_at
            .compare_exchange(next, following, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.logger.warn(EVENT_LOGGER_NAME).log(&format!(
            "{} 공개키 목록 재조회 제한에 걸려 조회 없이 ID Token 검증에 실패했습니다.",
            self.provider.name()
        ));
    }
}

pub struct OidcIdTokenVerifier {
    provider: SocialProvider,
    cache: JwksCache,
    validator: Validator,
    email_policy: EmailPolicy,
    clock: Arc<dyn Clock>,
}

impl OidcIdTokenVerifier {
    /// 공개키 목록 조회의 재시도·이전 목록 사용·재조회 제한을 경고 로그로 남긴다.
    pub fn with_logger(mut self, logger: Arc<Logger>) -> Self {
        let events = OidcJwkSetEventLogger::new(self.provider, logger, Arc::clone(&self.clock));
        self.cache.set_events(Arc::new(events));
        self
    }

    pub fn new(
        provider: SocialProvider,
        settings: &OidcProvider,
        email_policy: EmailPolicy,
        clock: Arc<dyn Clock>,
    ) -> Result<OidcIdTokenVerifier, String> {
        let source = Arc::new(HttpJwksSource::new(&settings.jwks_uri)?);
        Self::with_source(provider, settings, email_policy, source, clock)
    }

    /// 공개키 조회를 바꿔 끼울 수 있다(시험).
    pub fn with_source(
        provider: SocialProvider,
        settings: &OidcProvider,
        email_policy: EmailPolicy,
        source: Arc<dyn JwksSource>,
        clock: Arc<dyn Clock>,
    ) -> Result<OidcIdTokenVerifier, String> {
        let cache = JwksCache::with_config(
            source,
            JwksCacheConfig::default(),
            Arc::new(JwksClock(Arc::clone(&clock))),
        )?;
        Ok(OidcIdTokenVerifier {
            provider,
            cache,
            validator: Validator::oidc_id_token(&settings.issuer, &settings.audience),
            email_policy,
            clock,
        })
    }

    fn unauthorized(&self, message: String) -> ApiError {
        ApiError::App(AppError::unauthorized(ErrorCode::UNAUTHORIZED, message))
    }

    fn invalid_id_token(&self) -> ApiError {
        self.unauthorized(format!(
            "유효하지 않은 {} ID Token입니다.",
            self.provider.name()
        ))
    }

    fn decode(&self, id_token: &str) -> Result<Claims, ApiError> {
        if id_token.chars().all(is_java_whitespace) {
            return Err(self.invalid_id_token());
        }
        let claims = self
            .cache
            .verify(id_token)
            .map_err(|_| self.invalid_id_token())?;
        self.validator
            .validate(&claims, self.clock.now().as_millisecond())
            .map_err(|_| self.invalid_id_token())?;
        Ok(claims)
    }
}

impl IdTokenVerifier for OidcIdTokenVerifier {
    fn provider(&self) -> SocialProvider {
        self.provider
    }

    fn verify(&self, id_token: &str, raw_nonce: &str) -> Result<VerifiedSocialIdentity, ApiError> {
        let name = self.provider.name();
        if raw_nonce.chars().all(is_java_whitespace) {
            return Err(self.unauthorized(format!("{name} 로그인 nonce가 필요합니다.")));
        }
        let claims = self.decode(id_token)?;
        self.identity_from_claims(&claims, raw_nonce)
    }
}

impl OidcIdTokenVerifier {
    /// 서명·시각 검증이 끝난 클레임에서 nonce와 사용자 식별 정보를 확인한다.
    pub fn identity_from_claims(
        &self,
        claims: &Claims,
        raw_nonce: &str,
    ) -> Result<VerifiedSocialIdentity, ApiError> {
        let name = self.provider.name();
        let expected = sha256_hex(raw_nonce.as_bytes());
        let matches = claim_as_string(claims, "nonce")
            .is_some_and(|nonce| constant_time_eq(expected.as_bytes(), nonce.as_bytes()));
        if !matches {
            return Err(self.unauthorized(format!("{name} ID Token nonce가 일치하지 않습니다.")));
        }

        let subject = match claims.subject() {
            Some(s) if !s.chars().all(is_java_whitespace) => s,
            _ => {
                return Err(
                    self.unauthorized(format!("{name} ID Token에 사용자 식별 정보가 없습니다."))
                );
            }
        };
        if java_len(subject) > SUBJECT_MAX_LENGTH {
            return Err(
                self.unauthorized(format!("{name} ID Token의 사용자 식별 정보가 너무 깁니다."))
            );
        }
        Ok(VerifiedSocialIdentity {
            provider: self.provider,
            subject: subject.to_owned(),
            email: self.email_policy.extract_email(claims),
        })
    }
}

/// `MessageDigest.isEqual`. 길이가 다르면 바로 거짓이다.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;
    use chalkak_core::log::Level;
    use serde_json::json;

    const EMAIL: &str = "user@chalkak.test";
    const ID_TOKEN: &str = "id-token";
    const RAW_NONCE: &str = "raw-nonce";
    const ALL_PROVIDERS: [SocialProvider; 3] = [
        SocialProvider::Google,
        SocialProvider::Kakao,
        SocialProvider::Apple,
    ];

    fn claims(value: Value) -> Claims {
        Claims::from_map(value.as_object().unwrap().clone()).unwrap()
    }

    /// `jwtBuilder()`: subject와 nonce 해시를 가진 ID Token 클레임.
    fn id_claims() -> Value {
        json!({
            "sub": "provider-subject",
            "nonce": sha256_hex(RAW_NONCE.as_bytes()),
            "iat": 1_789_430_400,
            "exp": 1_789_431_000,
        })
    }

    // ----- OidcEmailPolicyTest -----

    #[test]
    fn extract_email_verified_only_with_boolean_true_returns_email() {
        let c = claims(json!({"email": EMAIL, "email_verified": true}));
        assert_eq!(
            EmailPolicy::VerifiedOnly.extract_email(&c).as_deref(),
            Some(EMAIL)
        );
    }

    #[test]
    fn extract_email_verified_only_with_string_true_returns_email() {
        for verified in ["true", "TRUE"] {
            let c = claims(json!({"email": EMAIL, "email_verified": verified}));
            assert_eq!(
                EmailPolicy::VerifiedOnly.extract_email(&c).as_deref(),
                Some(EMAIL)
            );
        }
    }

    #[test]
    fn extract_email_verified_only_without_true_returns_null() {
        for verified in ["false", "yes", ""] {
            let c = claims(json!({"email": EMAIL, "email_verified": verified}));
            assert_eq!(EmailPolicy::VerifiedOnly.extract_email(&c), None);
        }
        for verified in [json!(1), json!(null)] {
            let c = claims(json!({"email": EMAIL, "email_verified": verified}));
            assert_eq!(EmailPolicy::VerifiedOnly.extract_email(&c), None);
        }
    }

    #[test]
    fn extract_email_verified_only_with_false_or_missing_returns_null() {
        let false_verified = claims(json!({"email": EMAIL, "email_verified": false}));
        let missing_verified = claims(json!({"email": EMAIL}));
        assert_eq!(
            EmailPolicy::VerifiedOnly.extract_email(&false_verified),
            None
        );
        assert_eq!(
            EmailPolicy::VerifiedOnly.extract_email(&missing_verified),
            None
        );
        assert_eq!(
            EmailPolicy::VerifiedOnly.extract_email(&claims(json!({"email_verified": true}))),
            None
        );
    }

    #[test]
    fn extract_email_as_provided_without_verification_returns_email() {
        let c = claims(json!({"email": EMAIL, "email_verified": false}));
        assert_eq!(
            EmailPolicy::AsProvided.extract_email(&c).as_deref(),
            Some(EMAIL)
        );
    }

    #[test]
    fn extract_email_as_provided_without_email_returns_null() {
        assert_eq!(
            EmailPolicy::AsProvided.extract_email(&claims(json!({}))),
            None
        );
    }

    // ----- OidcIdTokenVerifierTest -----

    struct Failing;
    impl JwksSource for Failing {
        fn fetch(&self) -> Result<String, JwksFetchError> {
            Err(JwksFetchError::new("down"))
        }
    }

    fn verifier_for(provider: SocialProvider, policy: EmailPolicy) -> OidcIdTokenVerifier {
        let settings = OidcProvider {
            issuer: "https://accounts.google.com".into(),
            jwks_uri: "http://invalid".into(),
            audience: "client".into(),
        };
        OidcIdTokenVerifier::with_source(
            provider,
            &settings,
            policy,
            Arc::new(Failing),
            Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap())),
        )
        .unwrap()
    }

    fn verifier() -> OidcIdTokenVerifier {
        verifier_for(SocialProvider::Google, EmailPolicy::VerifiedOnly)
    }

    fn status_and_message(error: ApiError) -> (u16, String) {
        (error.status(), error.message())
    }

    #[test]
    fn verify_matching_nonce_returns_social_identity() {
        let mut c = id_claims();
        c["email"] = json!(EMAIL);
        c["email_verified"] = json!(true);
        let identity = verifier()
            .identity_from_claims(&claims(c), RAW_NONCE)
            .unwrap();
        assert_eq!(identity.provider, SocialProvider::Google);
        assert_eq!(identity.subject, "provider-subject");
        assert_eq!(identity.email.as_deref(), Some(EMAIL));
    }

    #[test]
    fn verify_mismatched_nonce_throws_unauthorized_exception() {
        for provider in ALL_PROVIDERS {
            let mut c = id_claims();
            c["nonce"] = json!(sha256_hex(b"other-raw-nonce"));
            let error = verifier_for(provider, EmailPolicy::VerifiedOnly)
                .identity_from_claims(&claims(c), RAW_NONCE)
                .unwrap_err();
            assert_eq!(
                status_and_message(error),
                (
                    401,
                    format!("{} ID Token nonce가 일치하지 않습니다.", provider.name())
                )
            );
        }
    }

    #[test]
    fn verify_missing_nonce_claim_throws_unauthorized_exception() {
        for provider in ALL_PROVIDERS {
            let mut c = id_claims();
            c.as_object_mut().unwrap().remove("nonce");
            let error = verifier_for(provider, EmailPolicy::VerifiedOnly)
                .identity_from_claims(&claims(c), RAW_NONCE)
                .unwrap_err();
            assert_eq!(
                status_and_message(error),
                (
                    401,
                    format!("{} ID Token nonce가 일치하지 않습니다.", provider.name())
                )
            );
        }
    }

    #[test]
    fn verify_uppercase_hex_nonce_throws_unauthorized_exception() {
        let mut c = id_claims();
        c["nonce"] = json!(sha256_hex(RAW_NONCE.as_bytes()).to_uppercase());
        let error = verifier_for(SocialProvider::Kakao, EmailPolicy::VerifiedOnly)
            .identity_from_claims(&claims(c), RAW_NONCE)
            .unwrap_err();
        assert_eq!(
            status_and_message(error),
            (401, "KAKAO ID Token nonce가 일치하지 않습니다.".to_owned())
        );
    }

    #[test]
    fn verify_raw_nonce_in_claim_throws_unauthorized_exception() {
        let mut c = id_claims();
        c["nonce"] = json!(RAW_NONCE);
        let error = verifier()
            .identity_from_claims(&claims(c), RAW_NONCE)
            .unwrap_err();
        assert_eq!(
            status_and_message(error),
            (401, "GOOGLE ID Token nonce가 일치하지 않습니다.".to_owned())
        );
    }

    #[test]
    fn verify_missing_raw_nonce_throws_unauthorized_exception() {
        // 원본 nonce가 비어 있으면 ID Token을 해석하기 전에 거절한다(해석했다면 "유효하지 않은 ..."이 나온다).
        for raw_nonce in ["", " "] {
            let error = verifier_for(SocialProvider::Kakao, EmailPolicy::AsProvided)
                .verify(ID_TOKEN, raw_nonce)
                .unwrap_err();
            assert_eq!(
                status_and_message(error),
                (401, "KAKAO 로그인 nonce가 필요합니다.".to_owned())
            );
        }
    }

    #[test]
    fn verify_blank_id_token_throws_unauthorized_exception() {
        for id_token in ["", " "] {
            let error = verifier_for(SocialProvider::Apple, EmailPolicy::VerifiedOnly)
                .verify(id_token, RAW_NONCE)
                .unwrap_err();
            assert_eq!(
                status_and_message(error),
                (401, "유효하지 않은 APPLE ID Token입니다.".to_owned())
            );
        }
    }

    #[test]
    fn verify_decoder_rejects_throws_unauthorized_exception() {
        // 공개키를 받지 못하거나 서명이 맞지 않는 토큰은 모두 같은 401이다.
        let error = verifier().verify("a.b.c", RAW_NONCE).unwrap_err();
        assert_eq!(
            status_and_message(error),
            (401, "유효하지 않은 GOOGLE ID Token입니다.".to_owned())
        );
    }

    #[test]
    fn verify_missing_subject_throws_unauthorized_exception() {
        let mut c = id_claims();
        c.as_object_mut().unwrap().remove("sub");
        let error = verifier_for(SocialProvider::Kakao, EmailPolicy::AsProvided)
            .identity_from_claims(&claims(c), RAW_NONCE)
            .unwrap_err();
        assert_eq!(
            status_and_message(error),
            (
                401,
                "KAKAO ID Token에 사용자 식별 정보가 없습니다.".to_owned()
            )
        );
    }

    #[test]
    fn verify_overlong_subject_throws_unauthorized_exception() {
        let mut c = id_claims();
        c["sub"] = json!("a".repeat(256));
        let error = verifier_for(SocialProvider::Apple, EmailPolicy::VerifiedOnly)
            .identity_from_claims(&claims(c), RAW_NONCE)
            .unwrap_err();
        assert_eq!(
            status_and_message(error),
            (
                401,
                "APPLE ID Token의 사용자 식별 정보가 너무 깁니다.".to_owned()
            )
        );
    }

    #[test]
    fn verify_max_length_subject_returns_social_identity() {
        let subject = "a".repeat(255);
        let mut c = id_claims();
        c["sub"] = json!(subject);
        let identity = verifier()
            .identity_from_claims(&claims(c), RAW_NONCE)
            .unwrap();
        assert_eq!(identity.subject, subject);
    }

    // ----- SocialIdentityVerifierTest -----

    struct Stub {
        provider: SocialProvider,
        identity: VerifiedSocialIdentity,
    }

    impl Stub {
        fn arc(provider: SocialProvider, subject: &str) -> Arc<dyn IdTokenVerifier> {
            Arc::new(Stub {
                provider,
                identity: VerifiedSocialIdentity {
                    provider,
                    subject: subject.to_owned(),
                    email: Some(EMAIL.to_owned()),
                },
            })
        }
    }

    impl IdTokenVerifier for Stub {
        fn provider(&self) -> SocialProvider {
            self.provider
        }
        fn verify(
            &self,
            id_token: &str,
            raw_nonce: &str,
        ) -> Result<VerifiedSocialIdentity, ApiError> {
            assert_eq!((id_token, raw_nonce), ("kakao-id-token", "raw-nonce"));
            Ok(self.identity.clone())
        }
    }

    #[test]
    fn verify_registered_provider_delegates_to_matching_verifier() {
        let v = SocialIdentityVerifier::new(vec![
            Stub::arc(SocialProvider::Google, "google-subject"),
            Stub::arc(SocialProvider::Kakao, "kakao-subject"),
        ])
        .unwrap();
        let identity = v
            .verify(SocialProvider::Kakao, "kakao-id-token", "raw-nonce")
            .unwrap();
        assert_eq!(identity.provider, SocialProvider::Kakao);
        assert_eq!(identity.subject, "kakao-subject");
        assert_eq!(identity.email.as_deref(), Some(EMAIL));
    }

    #[test]
    fn create_duplicate_provider_throws_illegal_state_exception() {
        let result = SocialIdentityVerifier::new(vec![
            Stub::arc(SocialProvider::Kakao, "a"),
            Stub::arc(SocialProvider::Kakao, "b"),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn verify_unsupported_provider_throws_business_exception() {
        let only_google =
            SocialIdentityVerifier::new(vec![Stub::arc(SocialProvider::Google, "g")]).unwrap();
        let error = only_google
            .verify(SocialProvider::Kakao, "kakao-id-token", "raw-nonce")
            .unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.message(), "지원하지 않는 소셜 로그인 제공자입니다.");
    }

    // ----- OidcIdTokenConfigTest -----

    fn auth_settings() -> crate::config::AuthSettings {
        let env = chalkak_core::config::Env::from_pairs(crate::config::TEST_ENV.to_vec());
        let post = crate::post::PostSettings::from_env(&env, "local").unwrap();
        crate::config::AppSettings::from_env(
            &env,
            &post,
            vec![0],
            std::time::Duration::from_secs(900),
        )
        .unwrap()
        .auth
    }

    /// 줄 단위로 모아 두는 로그 출력.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn lines(&self) -> usize {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .count()
        }
    }

    // OidcJwkSetEventLoggerTest#logRateLimited_repeatedAcrossInterval_logsOncePerInterval
    #[test]
    fn log_rate_limited_repeated_across_interval_logs_once_per_interval() {
        let clock = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        let capture = Capture::default();
        let logger = Arc::new(Logger::new(
            Box::new(capture.clone()),
            Level::Info,
            clock.clone(),
        ));
        let events = OidcJwkSetEventLogger::new(SocialProvider::Kakao, logger, clock.clone());

        events.rate_limited();
        clock.advance(std::time::Duration::from_millis(29_999));
        events.rate_limited();
        let before_interval = capture.lines();
        clock.advance(std::time::Duration::from_millis(1));
        events.rate_limited();

        assert_eq!(before_interval, 1);
        assert_eq!(capture.lines(), 2);
    }

    #[test]
    fn create_valid_properties_registers_decoders_and_verifiers() {
        let clock = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        let v = SocialIdentityVerifier::from_settings(
            &auth_settings(),
            clock,
            Arc::new(Logger::stdout(
                Level::Info,
                Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap())),
            )),
        )
        .unwrap();
        assert_eq!(
            v.providers(),
            vec![
                SocialProvider::Google,
                SocialProvider::Kakao,
                SocialProvider::Apple
            ]
        );
    }

    #[test]
    fn create_valid_properties_registers_provider_email_policies() {
        assert_eq!(
            email_policy_for(SocialProvider::Google),
            EmailPolicy::VerifiedOnly
        );
        assert_eq!(
            email_policy_for(SocialProvider::Apple),
            EmailPolicy::VerifiedOnly
        );
        assert_eq!(
            email_policy_for(SocialProvider::Kakao),
            EmailPolicy::AsProvided
        );
    }

    #[test]
    fn create_missing_required_property_fails_to_start() {
        let mut pairs: Vec<(&str, &str)> = crate::config::TEST_ENV
            .iter()
            .copied()
            .filter(|(k, _)| *k != "APPLE_OIDC_CLIENT_ID")
            .collect();
        pairs.push(("APPLE_OIDC_CLIENT_ID", ""));
        let env = chalkak_core::config::Env::from_pairs(pairs);
        let post = crate::post::PostSettings::from_env(&env, "local").unwrap();
        let result = crate::config::AppSettings::from_env(
            &env,
            &post,
            vec![0],
            std::time::Duration::from_secs(900),
        );
        assert!(result.is_err());
    }

    #[test]
    fn 상수_시간_비교() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
