//! RS256 서명(Google 서비스 계정 JWT).
//!
//! 근거: google-auth-library 1.52.0 `ServiceAccountCredentials.createAssertion`. 서비스 계정 JSON의 `private_key`는
//! PKCS#8 PEM(`-----BEGIN PRIVATE KEY-----`)이다. 키는 만들 때 파싱해 잘못된 설정이 첫 호출이 아니라 기동 시점에 드러나게 한다.
//!
//! 서명은 RSASSA-PKCS1-v1_5 + SHA-256이라 같은 입력이면 같은 토큰이 나온다(무작위 값이 없다).

use crate::{JwtError, b64, json};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use serde_json::Value;

/// RSA 개인키로 RS256 JWT를 만든다.
#[derive(Clone)]
pub struct Rs256Signer {
    key: SigningKey<Sha256>,
}

impl std::fmt::Debug for Rs256Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rs256Signer(..)")
    }
}

impl Rs256Signer {
    /// PKCS#8 PEM(`-----BEGIN PRIVATE KEY-----`)에서 만든다.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Rs256Signer, JwtError> {
        let key = RsaPrivateKey::from_pkcs8_pem(pem)
            .map_err(|_| JwtError::InvalidKey("not a PKCS#8 RSA private key".to_string()))?;
        Ok(Rs256Signer {
            key: SigningKey::<Sha256>::new(key),
        })
    }

    /// `base64url(header).base64url(claims).base64url(signature)`. JSON은 `Value`가 가진 키 순서 그대로 쓴다.
    pub fn sign(&self, header: &Value, claims: &Value) -> String {
        let signing_input = format!(
            "{}.{}",
            b64::encode_url(json::to_compact(header).as_bytes()),
            b64::encode_url(json::to_compact(claims).as_bytes())
        );
        let signature = self.key.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", b64::encode_url(&signature.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use serde_json::json;

    const PEM: &str = include_str!("../../../app/tests/fixtures/fcm-test-key.pem");

    // ServiceAccountCredentials.createAssertion: RS256 서명은 같은 키의 공개키로 검증되고 입력이 같으면 결과도 같다.
    #[test]
    fn 서명은_공개키로_검증되고_결정적이다() {
        let signer = Rs256Signer::from_pkcs8_pem(PEM).unwrap();
        let header = json!({"alg": "RS256", "kid": "k", "typ": "JWT"});
        let claims = json!({"iss": "worker@test", "exp": 3600});

        let token = signer.sign(&header, &claims);

        assert_eq!(token, signer.sign(&header, &claims));
        let (input, signature) = token.rsplit_once('.').unwrap();
        let signature =
            Signature::try_from(b64::decode_url(signature).unwrap().as_slice()).unwrap();
        let public = RsaPrivateKey::from_pkcs8_pem(PEM).unwrap().to_public_key();
        VerifyingKey::<Sha256>::new(public)
            .verify(input.as_bytes(), &signature)
            .unwrap();
        let (head, _) = input.split_once('.').unwrap();
        assert_eq!(
            b64::decode_url(head).unwrap(),
            br#"{"alg":"RS256","kid":"k","typ":"JWT"}"#
        );
    }

    #[test]
    fn pem이_아니면_거절한다() {
        assert!(matches!(
            Rs256Signer::from_pkcs8_pem("not a key"),
            Err(JwtError::InvalidKey(_))
        ));
    }
}
