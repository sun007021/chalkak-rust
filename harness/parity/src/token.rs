//! 시나리오용 액세스 토큰 발급. 원본 `JwtAccessTokenProvider`와 같은 헤더·클레임·HS256 서명을 만든다.
//! 소셜 로그인 흐름을 거치지 않고 인증 이후 동작을 검증하기 위한 것이며, 발급 경로 자체는 auth 시나리오가
//! 실제 로그인으로 따로 검증한다.

use crate::normalize::base64url_encode;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub enum Scope {
    User,
    Admin,
}

pub fn access_token(
    secret_hex: &str,
    subject: &str,
    scope: Scope,
    iat: i64,
    exp: i64,
    jti: &str,
    session_id: Option<&str>,
) -> Result<String, String> {
    let header = r#"{"typ":"JWT","alg":"HS256"}"#;
    let scope = match scope {
        Scope::User => "USER",
        Scope::Admin => "ADMIN",
    };
    let mut payload = serde_json::json!({
        "iss": "chalkak-backend",
        "aud": ["chalkak-access"],
        "sub": subject,
        "iat": iat,
        "exp": exp,
        "jti": jti,
        "purpose": "ACCESS",
        "scope": scope,
    });
    // 로그인 세션 식별자. 없으면 로그인 ID 도입 전의 이전 토큰이다.
    if let Some(session_id) = session_id {
        payload["session_id"] = session_id.into();
    }
    let signing_input = format!(
        "{}.{}",
        base64url_encode(header.as_bytes()),
        base64url_encode(payload.to_string().as_bytes())
    );
    let key = hex_decode(secret_hex)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|e| e.to_string())?;
    mac.update(signing_input.as_bytes());
    let sig = mac.finalize().into_bytes();
    Ok(format!("{signing_input}.{}", base64url_encode(&sig)))
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex 길이가 홀수입니다".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 서명이_rfc4231_방식과_같다() {
        // RFC 4231 Test Case 2: key "Jefe", data "what do ya want for nothing?"
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn 로그인_세션_식별자는_지정했을_때만_싣는다() {
        let payload = |t: &str| -> serde_json::Value {
            let part = t.split('.').nth(1).unwrap();
            let bytes = crate::normalize::base64url_decode(part).unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        let with = access_token("00ff", "s", Scope::User, 1, 2, "j", Some("sid")).unwrap();
        assert_eq!(payload(&with)["session_id"], "sid");
        let without = access_token("00ff", "s", Scope::User, 1, 2, "j", None).unwrap();
        assert!(payload(&without).get("session_id").is_none());
    }

    #[test]
    fn 토큰은_세_부분이다() {
        let t = access_token("00ff", "s", Scope::User, 1, 2, "j", None).unwrap();
        assert_eq!(t.split('.').count(), 3);
    }
}
