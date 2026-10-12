//! ES256 서명(Apple client secret)과 검증.
//!
//! 근거: `AppleClientSecretGenerator`(Nimbus `ECDSASigner`). 개인키는 PKCS#8 PEM이며 설정에는 PEM 전체를 base64로
//! 감싼 문자열(`privateKeyBase64`)이 들어온다. 원본은 생성자에서 키를 즉시 파싱하므로 [`Es256Signer`]도 만들 때
//! 파싱해 잘못된 설정이 첫 요청이 아니라 기동 시점에 드러나게 한다.
//!
//! 서명은 JOSE 형식(r||s, 64바이트)이다. Nimbus는 서명마다 무작위 k를 쓰지만 여기서는 RFC 6979 결정적 ECDSA를
//! 쓰므로 같은 입력이면 같은 토큰이 나온다. 둘 다 같은 공개키로 검증된다.

use crate::{Claims, JwtError, b64, json, jws::Jws};
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::DecodePrivateKey;
use serde_json::Value;

const PEM_BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
const PEM_END: &str = "-----END PRIVATE KEY-----";

fn invalid_key(message: &str) -> JwtError {
    JwtError::InvalidKey(message.to_string())
}

/// P-256 개인키로 ES256 JWT를 만든다.
#[derive(Clone)]
pub struct Es256Signer {
    key: SigningKey,
}

impl std::fmt::Debug for Es256Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Es256Signer(..)")
    }
}

impl Es256Signer {
    /// PKCS#8 PEM(`-----BEGIN PRIVATE KEY-----`). 원본과 같이 머리말 두 줄을 지우고 공백을 모두 없앤 뒤
    /// base64로 해석한다.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Es256Signer, JwtError> {
        if !pem.contains(PEM_BEGIN) || !pem.contains(PEM_END) {
            return Err(invalid_key("PEM markers not found"));
        }
        let body: String = pem
            .replace(PEM_BEGIN, "")
            .replace(PEM_END, "")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let der = b64::decode(&body, b64::Alphabet::Standard)
            .map_err(|()| invalid_key("PEM body is not valid base64"))?;
        let key = SigningKey::from_pkcs8_der(&der)
            .map_err(|_| invalid_key("not a PKCS#8 P-256 private key"))?;
        Ok(Es256Signer { key })
    }

    /// `AppleTokenProperties.privateKeyBase64`: PEM 전체를 표준 base64로 감싼 값.
    pub fn from_base64_pem(encoded: &str) -> Result<Es256Signer, JwtError> {
        let bytes = b64::decode(encoded, b64::Alphabet::Standard)
            .map_err(|()| invalid_key("private key is not valid base64"))?;
        let pem = String::from_utf8(bytes).map_err(|_| invalid_key("PEM is not ASCII text"))?;
        Es256Signer::from_pkcs8_pem(&pem)
    }

    /// 32바이트 비밀 스칼라(JWK의 `d`를 디코딩한 값).
    pub fn from_secret_scalar(bytes: &[u8]) -> Result<Es256Signer, JwtError> {
        let key = SigningKey::from_slice(bytes).map_err(|_| invalid_key("invalid P-256 scalar"))?;
        Ok(Es256Signer { key })
    }

    /// `base64url(header).base64url(claims).base64url(r||s)`.
    pub fn sign(&self, header: &Value, claims: &Value) -> String {
        let signing_input = format!(
            "{}.{}",
            b64::encode_url(json::to_compact(header).as_bytes()),
            b64::encode_url(json::to_compact(claims).as_bytes())
        );
        let signature: Signature = self.key.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", b64::encode_url(&signature.to_bytes()))
    }
}

/// 한 번 쓰고 마는 경우를 위한 편의 함수. 반복해서 쓰면 [`Es256Signer`]를 한 번만 만들어 재사용한다.
pub fn es256_sign(header: &Value, claims: &Value, pkcs8_pem: &str) -> Result<String, JwtError> {
    Ok(Es256Signer::from_pkcs8_pem(pkcs8_pem)?.sign(header, claims))
}

/// P-256 공개키.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Es256PublicKey {
    key: VerifyingKey,
}

impl Es256PublicKey {
    /// JWK의 `x`, `y`(base64url, 각 32바이트). 곡선 위의 점이 아니면 거절한다.
    pub fn from_coordinates(x: &str, y: &str) -> Result<Es256PublicKey, JwtError> {
        let decode = |text: &str| -> Result<[u8; 32], JwtError> {
            b64::decode_url(text)
                .ok()
                .and_then(|v| <[u8; 32]>::try_from(v).ok())
                .ok_or_else(|| invalid_key("EC coordinate must be 32 bytes of base64url"))
        };
        let (x, y) = (decode(x)?, decode(y)?);
        let point = p256::EncodedPoint::from_affine_coordinates(&x.into(), &y.into(), false);
        let key = VerifyingKey::from_encoded_point(&point)
            .map_err(|_| invalid_key("point is not on the P-256 curve"))?;
        Ok(Es256PublicKey { key })
    }
}

/// ES256 토큰의 서명을 검증하고 클레임을 돌려준다.
pub fn es256_verify(token: &str, key: &Es256PublicKey) -> Result<Claims, JwtError> {
    let jws = Jws::parse(token)?;
    if jws.alg != "ES256" {
        return Err(JwtError::UnsupportedAlgorithm(format!(
            "Unsupported algorithm {}",
            jws.alg
        )));
    }
    // JOSE 형식은 정확히 64바이트. DER 등 다른 길이는 거절한다.
    let signature =
        Signature::from_slice(&jws.signature).map_err(|_| JwtError::InvalidSignature)?;
    key.key
        .verify(jws.signing_input.as_bytes(), &signature)
        .map_err(|_| JwtError::InvalidSignature)?;
    Claims::from_slice(&jws.payload)
}
