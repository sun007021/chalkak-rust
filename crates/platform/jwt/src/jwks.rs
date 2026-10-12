//! JWK Set 해석과 키 선택, RS256 검증.
//!
//! 근거: Nimbus `JWKSet.parse`, `JWKMatcher.forJWSHeader`, `JWSVerificationKeySelector`, `DefaultJWTProcessor`,
//! Spring `NimbusJwtDecoder.withJwkSource`(기본 서명 알고리즘 RS256, 허용 `typ`는 JWT). 판정의 근거는
//! `tests/fixtures/ProbeOidcRules.java` 출력이다.
//!
//! - 목록 최상위는 `keys` 배열이어야 한다. 각 항목은 객체이고 `kty`가 있어야 한다.
//! - `kty`가 RSA면 `n`, `e`를 읽는다. 하나라도 잘못되면 **목록 전체**를 거절한다.
//! - `kty`가 EC·OKP·oct면 검증 없이 목록에 둔다(kid 조회에만 쓴다). 그 밖의 `kty`는 건너뛴다.
//! - 키 후보 조건: `kty=RSA`, `use`가 `sig`이거나 없음, `alg`가 `RS256`이거나 없음, 토큰에 `kid`가 있으면 일치.
//!   `key_ops`는 보지 않는다.

use crate::{Claims, JwksError, JwtError, b64, jws::Jws};
use rsa::signature::Verifier;
use rsa::{BigUint, RsaPublicKey, pkcs1v15};
use serde_json::{Map, Value};
use sha2::Sha256;

#[derive(Debug, Clone)]
enum KeyMaterial {
    Rsa(pkcs1v15::VerifyingKey<Sha256>),
    /// EC·OKP·oct. RS256 검증에는 쓰지 않는다.
    Other,
}

#[derive(Debug, Clone)]
pub struct Jwk {
    kid: Option<String>,
    key_use: Option<String>,
    alg: Option<String>,
    material: KeyMaterial,
}

impl Jwk {
    pub fn key_id(&self) -> Option<&str> {
        self.kid.as_deref()
    }

    fn matches_rs256(&self, kid: Option<&str>) -> bool {
        matches!(self.material, KeyMaterial::Rsa(_))
            && self.key_use.as_deref().is_none_or(|u| u == "sig")
            && self.alg.as_deref().is_none_or(|a| a == "RS256")
            && kid.is_none_or(|k| self.kid.as_deref() == Some(k))
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let KeyMaterial::Rsa(key) = &self.material else {
            return false;
        };
        pkcs1v15::Signature::try_from(signature).is_ok_and(|sig| key.verify(message, &sig).is_ok())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Jwks {
    keys: Vec<Jwk>,
}

fn parse_error(message: impl Into<String>) -> JwksError {
    JwksError::Parse(message.into())
}

fn optional_string(
    obj: &Map<String, Value>,
    name: &str,
    index: usize,
) -> Result<Option<String>, JwksError> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(parse_error(format!(
            "Invalid JWK at position {index}: \"{name}\" must be a string"
        ))),
    }
}

fn big(obj: &Map<String, Value>, name: &str, index: usize) -> Result<BigUint, JwksError> {
    let text = match obj.get(name) {
        Some(Value::String(s)) => s,
        _ => {
            return Err(parse_error(format!(
                "Invalid JWK at position {index}: The {name} value must not be null"
            )));
        }
    };
    let bytes = b64::decode_url(text).map_err(|()| {
        parse_error(format!(
            "Invalid JWK at position {index}: bad base64url in {name}"
        ))
    })?;
    Ok(BigUint::from_bytes_be(&bytes))
}

impl Jwks {
    pub fn parse(json: &str) -> Result<Jwks, JwksError> {
        let value: Value =
            serde_json::from_str(json).map_err(|e| parse_error(format!("invalid JSON: {e}")))?;
        let Value::Object(top) = value else {
            return Err(parse_error("JWK set is not a JSON object"));
        };
        let items = match top.get("keys") {
            None | Some(Value::Null) => {
                return Err(parse_error("Missing required \"keys\" member"));
            }
            Some(Value::Array(items)) => items,
            Some(_) => return Err(parse_error("Unexpected type of JSON object member keys")),
        };
        let mut keys = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let Value::Object(obj) = item else {
                return Err(parse_error(format!(
                    "Invalid JWK at position {index}: not an object"
                )));
            };
            let kty = match obj.get("kty") {
                Some(Value::String(k)) => k.as_str(),
                _ => {
                    return Err(parse_error(format!(
                        "Invalid JWK at position {index}: Missing key type \"kty\" parameter"
                    )));
                }
            };
            let material = match kty {
                "RSA" => {
                    let n = big(obj, "n", index)?;
                    let e = big(obj, "e", index)?;
                    let public = RsaPublicKey::new(n, e).map_err(|e| {
                        parse_error(format!(
                            "Invalid JWK at position {index}: invalid RSA key: {e}"
                        ))
                    })?;
                    KeyMaterial::Rsa(pkcs1v15::VerifyingKey::<Sha256>::new(public))
                }
                "EC" | "OKP" | "oct" => KeyMaterial::Other,
                // 지원하지 않는 종류는 건너뛴다(Nimbus가 무시한다).
                _ => continue,
            };
            keys.push(Jwk {
                kid: optional_string(obj, "kid", index)?,
                key_use: optional_string(obj, "use", index)?,
                alg: optional_string(obj, "alg", index)?,
                material,
            });
        }
        Ok(Jwks { keys })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&self) -> &[Jwk] {
        &self.keys
    }

    /// kid가 같은 키가 (종류와 상관없이) 있는지. 있으면 다시 가져와도 소용없다(`JWKSet.getKeyByKeyId`).
    pub fn has_key_id(&self, kid: &str) -> bool {
        self.keys.iter().any(|k| k.kid.as_deref() == Some(kid))
    }

    /// RS256 서명 키 후보.
    pub(crate) fn select_rs256(&self, kid: Option<&str>) -> Vec<&Jwk> {
        self.keys.iter().filter(|k| k.matches_rs256(kid)).collect()
    }
}

/// 서명 검증 전 단계: 헤더 확인(`alg`, `typ`, `crit`)을 마치고 키 선택에 쓸 kid를 돌려준다.
pub(crate) fn rs256_prepare(token: &str) -> Result<Jws<'_>, JwtError> {
    let jws = Jws::parse(token)?;
    // DefaultJWTProcessor: typ 검사가 키 선택보다 먼저다. Spring 기본 JOSEObjectTypeVerifier.JWT.
    if let Some(typ) = &jws.typ
        && !typ.eq_ignore_ascii_case("JWT")
    {
        return Err(JwtError::UnsupportedAlgorithm(
            "the given typ value needs to be one of [JWT]".into(),
        ));
    }
    // 알고리즘이 RS256이 아니면 키 선택에서 빈 목록이 나온다.
    if jws.alg != "RS256" {
        return Err(JwtError::NoMatchingKey);
    }
    Ok(jws)
}

/// 후보 키를 차례로 시도해 서명이 맞으면 클레임을 돌려준다(`DefaultJWTProcessor`).
pub(crate) fn rs256_check_candidates(
    jws: &Jws<'_>,
    candidates: &[&Jwk],
) -> Result<Claims, JwtError> {
    if candidates.is_empty() {
        return Err(JwtError::NoMatchingKey);
    }
    let message = jws.signing_input.as_bytes();
    if candidates.iter().any(|k| k.verify(message, &jws.signature)) {
        Claims::from_slice(&jws.payload)
    } else {
        Err(JwtError::InvalidSignature)
    }
}

/// 이미 가진 공개키 목록으로 RS256 토큰의 서명을 검증한다. 키는 `kid`로 고른다.
/// 목록을 캐시하고 모르는 kid에서 다시 가져오는 쪽은 `JwksCache::verify`.
pub fn rs256_verify(token: &str, jwks: &Jwks) -> Result<Claims, JwtError> {
    let jws = rs256_prepare(token)?;
    let candidates = jwks.select_rs256(jws.kid.as_deref());
    rs256_check_candidates(&jws, &candidates)
}
