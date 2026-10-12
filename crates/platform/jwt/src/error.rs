//! 오류 종류. 원본에서는 모두 `JwtException`(401)이지만, 호출 측이 로그·분기에 쓸 수 있게 나눠 둔다.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    /// 토큰 형식, base64url, JSON, 클레임 타입이 맞지 않음(Nimbus `ParseException`, "Malformed payload").
    Malformed(String),
    /// `alg` 불일치, `none`, 암호화(JWE), 처리할 수 없는 `crit`, 허용하지 않는 `typ`.
    UnsupportedAlgorithm(String),
    /// 서명이 맞지 않음.
    InvalidSignature,
    /// kid(또는 kty·use·alg 조건)에 맞는 키가 없음("no matching key(s) found").
    NoMatchingKey,
    /// 서명 뒤 Jwt 객체를 만드는 단계의 실패(Spring `Jwt` 생성자): `expiresAt must be after issuedAt`.
    InvalidClaims(String),
    /// 키 형식이 올바르지 않음(PEM, PKCS#8, 16진수 비밀키, JWK 좌표).
    InvalidKey(String),
    /// 공개키 목록을 얻지 못함.
    Jwks(JwksError),
}

/// 공개키 목록(JWKS) 조회·해석 실패.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwksError {
    /// 통신 실패, 비정상 상태 코드, 크기 초과 등(`JWKSetRetrievalException`).
    Fetch(String),
    /// 본문이 JWK Set 형식이 아님(`JWKSetParseException`).
    Parse(String),
    /// 조회 횟수 제한에 걸림(`RateLimitReachedException`): 30초 구간마다 최대 2회.
    RateLimited,
}

impl fmt::Display for JwksError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JwksError::Fetch(m) => write!(f, "Couldn't retrieve JWK set: {m}"),
            JwksError::Parse(m) => write!(f, "Unable to parse JWK set: {m}"),
            JwksError::RateLimited => write!(f, "JWK set retrieval rate limit reached"),
        }
    }
}

impl fmt::Display for JwtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JwtError::Malformed(m) => write!(f, "Malformed token: {m}"),
            JwtError::UnsupportedAlgorithm(m) => write!(f, "Unsupported: {m}"),
            JwtError::InvalidSignature => write!(f, "Signed JWT rejected: Invalid signature"),
            JwtError::NoMatchingKey => write!(
                f,
                "Signed JWT rejected: Another algorithm expected, or no matching key(s) found"
            ),
            JwtError::InvalidClaims(m) => write!(f, "{m}"),
            JwtError::InvalidKey(m) => write!(f, "Invalid key: {m}"),
            JwtError::Jwks(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JwtError {}
impl std::error::Error for JwksError {}

impl From<JwksError> for JwtError {
    fn from(e: JwksError) -> Self {
        JwtError::Jwks(e)
    }
}
