//! 제공자 ID Token 검증. 원본 `OidcIdTokenVerifier`, `OidcEmailPolicy`, `OidcIdTokenDecoderFactory`.
//!
//! 서명·발급자·aud·시각은 `chalkak-jwt`(RS256 + JWKS 캐시)가 검증하고, 여기서는 nonce와 사용자 식별 정보를 확인한다.
//! 검증 실패는 모두 401이다(원본은 `JwtException`을 `UnauthorizedException`으로 바꾼다).

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_http::client::HttpClient;
use chalkak_jwt::{
    Claims, JWKS_ACCEPT, JWKS_CONNECT_TIMEOUT, JWKS_READ_TIMEOUT, JwksCache, JwksCacheConfig,
    JwksFetchError, JwksSource, Validator,
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
    pub fn new(verifiers: Vec<Arc<dyn IdTokenVerifier>>) -> SocialIdentityVerifier {
        SocialIdentityVerifier { verifiers }
    }

    /// 설정에 맞춘 Google·Kakao·Apple 검증기.
    pub fn from_settings(
        auth: &AuthSettings,
        clock: Arc<dyn Clock>,
    ) -> Result<SocialIdentityVerifier, String> {
        let mut verifiers: Vec<Arc<dyn IdTokenVerifier>> = Vec::new();
        for (provider, settings, policy) in [
            (
                SocialProvider::Google,
                &auth.google,
                EmailPolicy::VerifiedOnly,
            ),
            (SocialProvider::Kakao, &auth.kakao, EmailPolicy::AsProvided),
            (
                SocialProvider::Apple,
                &auth.apple_oidc,
                EmailPolicy::VerifiedOnly,
            ),
        ] {
            verifiers.push(Arc::new(OidcIdTokenVerifier::new(
                provider,
                settings,
                policy,
                Arc::clone(&clock),
            )?));
        }
        Ok(SocialIdentityVerifier { verifiers })
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

pub struct OidcIdTokenVerifier {
    provider: SocialProvider,
    cache: JwksCache,
    validator: Validator,
    email_policy: EmailPolicy,
    clock: Arc<dyn Clock>,
}

impl OidcIdTokenVerifier {
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

        let expected = sha256_hex(raw_nonce.as_bytes());
        let matches = claim_as_string(&claims, "nonce")
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
            email: self.email_policy.extract_email(&claims),
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
    use serde_json::json;

    fn claims(value: Value) -> Claims {
        Claims::from_map(value.as_object().unwrap().clone()).unwrap()
    }

    // OidcEmailPolicy: VERIFIED_ONLY
    #[test]
    fn 검증된_이메일만_쓰는_정책() {
        let p = EmailPolicy::VerifiedOnly;
        assert_eq!(
            p.extract_email(&claims(json!({"email":"a@b.c","email_verified":true}))),
            Some("a@b.c".into())
        );
        assert_eq!(
            p.extract_email(&claims(json!({"email":"a@b.c","email_verified":"TRUE"}))),
            Some("a@b.c".into())
        );
        for verified in [json!(false), json!("false"), json!(1), json!(null)] {
            assert_eq!(
                p.extract_email(&claims(json!({"email":"a@b.c","email_verified":verified}))),
                None
            );
        }
        assert_eq!(p.extract_email(&claims(json!({"email":"a@b.c"}))), None);
        assert_eq!(
            p.extract_email(&claims(json!({"email_verified":true}))),
            None
        );
    }

    #[test]
    fn 제공된_이메일을_그대로_쓰는_정책() {
        let p = EmailPolicy::AsProvided;
        assert_eq!(
            p.extract_email(&claims(json!({"email":"a@b.c"}))),
            Some("a@b.c".into())
        );
        assert_eq!(p.extract_email(&claims(json!({}))), None);
    }

    struct Failing;
    impl JwksSource for Failing {
        fn fetch(&self) -> Result<String, JwksFetchError> {
            Err(JwksFetchError::new("down"))
        }
    }

    fn verifier() -> OidcIdTokenVerifier {
        let settings = OidcProvider {
            issuer: "https://accounts.google.com".into(),
            jwks_uri: "http://invalid".into(),
            audience: "client".into(),
        };
        OidcIdTokenVerifier::with_source(
            SocialProvider::Google,
            &settings,
            EmailPolicy::VerifiedOnly,
            Arc::new(Failing),
            Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap())),
        )
        .unwrap()
    }

    // OidcIdTokenVerifierTest: 입력 검증 순서와 메시지
    #[test]
    fn 빈_nonce와_빈_토큰과_깨진_토큰은_401이다() {
        let v = verifier();
        let err = v.verify("token", " ").unwrap_err();
        assert_eq!(
            (err.status(), err.message().as_str()),
            (401, "GOOGLE 로그인 nonce가 필요합니다.")
        );
        let err = v.verify("  ", "nonce").unwrap_err();
        assert_eq!(
            (err.status(), err.message().as_str()),
            (401, "유효하지 않은 GOOGLE ID Token입니다.")
        );
        let err = v.verify("a.b.c", "nonce").unwrap_err();
        assert_eq!(
            (err.status(), err.message().as_str()),
            (401, "유효하지 않은 GOOGLE ID Token입니다.")
        );
    }

    #[test]
    fn 등록되지_않은_제공자는_400이다() {
        let only_google = SocialIdentityVerifier::new(vec![Arc::new(verifier())]);
        let err = only_google
            .verify(SocialProvider::Kakao, "t", "n")
            .unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(err.message(), "지원하지 않는 소셜 로그인 제공자입니다.");
    }

    #[test]
    fn 상수_시간_비교() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
