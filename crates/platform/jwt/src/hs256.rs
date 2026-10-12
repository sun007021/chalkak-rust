//! HS256 발급·검증(원본 액세스 토큰, 소셜 가입 토큰).
//!
//! 발급: 헤더와 클레임을 호출자가 만든 `Value`의 키 순서 그대로 직렬화한다. 원본(NimbusJwtEncoder)은
//! 헤더 `{"typ":"JWT","alg":"HS256"}`, 클레임은 Java `HashMap` 순서로 내보내므로 호출 측이
//! [`crate::original`]의 생성 함수를 쓰면 바이트가 같아진다.
//!
//! 검증: `NimbusJwtDecoder.withSecretKey(key).macAlgorithm(HS256)` — `alg`가 정확히 `HS256`이어야 하고,
//! 서명은 상수 시간으로 비교한다. 클레임 검증은 [`crate::Validator`]가 맡는다.

use crate::{Claims, JwtError, b64, json, jws::Jws};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn mac(key: &[u8]) -> HmacSha256 {
    // HMAC은 어떤 길이의 키도 받는다. `new_from_slice`는 실패하지 않는다.
    HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length")
}

pub fn hs256_encode(header: &Value, claims: &Value, key: &[u8]) -> String {
    let signing_input = format!(
        "{}.{}",
        b64::encode_url(json::to_compact(header).as_bytes()),
        b64::encode_url(json::to_compact(claims).as_bytes())
    );
    let mut mac = mac(key);
    mac.update(signing_input.as_bytes());
    let signature = mac.finalize().into_bytes();
    format!("{signing_input}.{}", b64::encode_url(&signature))
}

/// 서명을 검증하고 클레임을 돌려준다. 만료·발급자 같은 클레임 규칙은 호출 측 `Validator`가 따로 적용한다.
pub fn hs256_decode(token: &str, key: &[u8]) -> Result<Claims, JwtError> {
    let jws = Jws::parse(token)?;
    if jws.alg != "HS256" {
        return Err(JwtError::UnsupportedAlgorithm(format!(
            "Unsupported algorithm {}",
            jws.alg
        )));
    }
    let mut mac = mac(key);
    mac.update(jws.signing_input.as_bytes());
    // verify_slice는 상수 시간 비교다.
    mac.verify_slice(&jws.signature)
        .map_err(|_| JwtError::InvalidSignature)?;
    Claims::from_slice(&jws.payload)
}
