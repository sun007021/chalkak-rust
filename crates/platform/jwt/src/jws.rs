//! JWS 컴팩트 직렬화 파싱(서명 검증 전 단계).

use crate::{JwtError, b64};
use serde_json::{Map, Value};

pub(crate) struct Jws<'a> {
    pub alg: String,
    #[allow(dead_code)] // RS256 검증 커밋에서 사용
    pub kid: Option<String>,
    #[allow(dead_code)]
    pub typ: Option<String>,
    pub payload: Vec<u8>,
    /// `base64url(header) "." base64url(payload)` — 서명 대상 바이트 그대로.
    pub signing_input: &'a str,
    pub signature: Vec<u8>,
}

impl<'a> Jws<'a> {
    pub(crate) fn parse(token: &'a str) -> Result<Jws<'a>, JwtError> {
        let bad = |m: &str| JwtError::Malformed(m.to_string());
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad(
                "Invalid serialized unsecured/JWS/JWE object: Missing part delimiters",
            ));
        };
        match parts.next() {
            None => {}
            // 점이 4개면 JWE(5조각). Spring: "Encrypted JWTs are not supported".
            Some(_) if token.matches('.').count() == 4 => {
                return Err(JwtError::UnsupportedAlgorithm(
                    "Encrypted JWTs are not supported".into(),
                ));
            }
            Some(_) => return Err(bad("Unexpected number of Base64URL parts")),
        }
        let header_bytes = b64::decode_url(h).map_err(|()| bad("Invalid JWS header: base64url"))?;
        let header: Map<String, Value> = match serde_json::from_slice(&header_bytes) {
            Ok(Value::Object(m)) => m,
            _ => return Err(bad("Invalid JWS header: not a JSON object")),
        };
        let alg = match header.get("alg") {
            Some(Value::String(a)) => a.clone(),
            _ => return Err(bad("Invalid JWS header: Missing \"alg\"")),
        };
        if alg == "none" {
            return Err(JwtError::UnsupportedAlgorithm(
                "Unsupported algorithm of none".into(),
            ));
        }
        if header.contains_key("crit") {
            return Err(JwtError::UnsupportedAlgorithm(
                "Unsupported critical header parameter".into(),
            ));
        }
        let string_header = |name: &str| -> Result<Option<String>, JwtError> {
            match header.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(v)) => Ok(Some(v.clone())),
                Some(_) => Err(JwtError::Malformed(format!(
                    "Invalid JWS header: \"{name}\" must be a string"
                ))),
            }
        };
        let kid = string_header("kid")?;
        let typ = string_header("typ")?;
        let payload = b64::decode_url(p).map_err(|()| bad("Invalid payload: base64url"))?;
        let signature = b64::decode_url(s).map_err(|()| bad("Invalid signature: base64url"))?;
        let signing_input = &token[..h.len() + 1 + p.len()];
        Ok(Jws {
            alg,
            kid,
            typ,
            payload,
            signing_input,
            signature,
        })
    }
}
