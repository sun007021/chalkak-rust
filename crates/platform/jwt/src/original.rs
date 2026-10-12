//! 원본 발급 코드와 같은 헤더·클레임 `Value`를 만든다(키 순서까지 같다).
//!
//! 원본은 Spring `NimbusJwtEncoder`가 `JwtClaimsSet`의 클레임을 Nimbus `JWTClaimsSet`으로 옮겨 쓴다. Nimbus는
//! 클레임을 Java `HashMap`에 담아 직렬화하므로 순서는 입력 순서가 아니라 키 문자열 해시로 정해진다. 키 집합이
//! 정해져 있으면 순서도 고정이라, 여기서는 원본이 실제로 낸 순서를 상수로 박아 두고
//! `tests/fixtures/GenFixtures.java`가 원본 클래스를 실행해 얻은 토큰과 바이트가 같은지 테스트로 확인한다.
//!
//! 근거 클래스: `JwtAccessTokenProvider#issue`, `JwtSocialSignupTokenProvider#issue`,
//! `AppleClientSecretGenerator#generate`.

use serde_json::{Map, Value, json};

/// 액세스 토큰·가입 토큰 헤더: `{"typ":"JWT","alg":"HS256"}`.
fn hs256_header() -> Value {
    json!({"typ": "JWT", "alg": "HS256"})
}

pub fn access_token_header() -> Value {
    hs256_header()
}

pub fn social_signup_header() -> Value {
    hs256_header()
}

/// 원본 액세스 토큰 클레임(`JwtAccessTokenProvider#issue`).
#[derive(Debug, Clone, Copy)]
pub struct AccessTokenClaims<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub subject: &'a str,
    /// 에포크 초.
    pub issued_at: i64,
    pub expires_at: i64,
    pub jti: &'a str,
    /// `USER` 또는 `ADMIN`.
    pub scope: &'a str,
}

impl AccessTokenClaims<'_> {
    /// 순서: sub, aud, purpose, scope, iss, exp, iat, jti. `aud`는 값이 하나라 문자열로 나간다.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("sub".into(), self.subject.into());
        m.insert("aud".into(), self.audience.into());
        m.insert("purpose".into(), "ACCESS".into());
        m.insert("scope".into(), self.scope.into());
        m.insert("iss".into(), self.issuer.into());
        m.insert("exp".into(), self.expires_at.into());
        m.insert("iat".into(), self.issued_at.into());
        m.insert("jti".into(), self.jti.into());
        Value::Object(m)
    }
}

/// 원본 소셜 가입 토큰 클레임(`JwtSocialSignupTokenProvider#issue`).
#[derive(Debug, Clone, Copy)]
pub struct SocialSignupClaims<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub subject: &'a str,
    pub issued_at: i64,
    pub expires_at: i64,
    pub jti: &'a str,
    /// `GOOGLE` `KAKAO` `APPLE`.
    pub provider: &'a str,
    pub upload_id: &'a str,
    /// 없으면 클레임 자체를 넣지 않는다.
    pub email: Option<&'a str>,
}

impl SocialSignupClaims<'_> {
    /// 순서: sub, aud, uploadId, purpose, provider, iss, exp, iat, jti, (email).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("sub".into(), self.subject.into());
        m.insert("aud".into(), self.audience.into());
        m.insert("uploadId".into(), self.upload_id.into());
        m.insert("purpose".into(), "SOCIAL_SIGNUP".into());
        m.insert("provider".into(), self.provider.into());
        m.insert("iss".into(), self.issuer.into());
        m.insert("exp".into(), self.expires_at.into());
        m.insert("iat".into(), self.issued_at.into());
        m.insert("jti".into(), self.jti.into());
        if let Some(email) = self.email {
            m.insert("email".into(), email.into());
        }
        Value::Object(m)
    }
}

/// Apple client secret의 `aud`(`AppleClientSecretGenerator.APPLE_AUDIENCE`).
pub const APPLE_AUDIENCE: &str = "https://appleid.apple.com";
/// Apple client secret 수명(`AppleClientSecretGenerator.EXPIRATION`, 5분).
pub const APPLE_CLIENT_SECRET_LIFETIME_SECS: i64 = 300;

/// Apple client secret 헤더: `{"kid":<key id>,"alg":"ES256"}`(typ 없음).
pub fn apple_client_secret_header(key_id: &str) -> Value {
    json!({"kid": key_id, "alg": "ES256"})
}

/// Apple client secret 클레임. 순서: iss, sub, aud, exp, iat. `iss`는 team id, `sub`는 client id.
pub fn apple_client_secret_claims(team_id: &str, client_id: &str, issued_at: i64) -> Value {
    let mut m = Map::new();
    m.insert("iss".into(), team_id.into());
    m.insert("sub".into(), client_id.into());
    m.insert("aud".into(), APPLE_AUDIENCE.into());
    m.insert(
        "exp".into(),
        (issued_at + APPLE_CLIENT_SECRET_LIFETIME_SECS).into(),
    );
    m.insert("iat".into(), issued_at.into());
    Value::Object(m)
}
