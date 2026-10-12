//! 시나리오용 소셜 로그인 ID Token 발급. 가짜 서버(`harness/fakes`)의 JWKS가 내놓는 테스트 공개키와
//! 짝인 개인키(`harness/keys/oidc-test-private.pem`)로 RS256 서명한다. 원본이 검증하는 발급자·aud·nonce·시각
//! 규칙을 그대로 따르는 토큰과, 시나리오가 일부러 어긋나게 만든 토큰을 모두 만든다.

use crate::normalize::base64url_encode;
use crate::scenario::IdTokenSpec;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const PRIVATE_KEY_PEM: &str = include_str!("../../keys/oidc-test-private.pem");

/// 가짜 JWKS가 내놓는 키 ID.
pub const KEY_ID: &str = "harness-test-key";

/// 제공자별 (발급자, aud 기본값). aud는 `harness/env/oracle.env`의 클라이언트 ID와 같아야 한다.
fn provider_defaults(provider: &str) -> Result<(&'static str, &'static str), String> {
    match provider {
        "google" => Ok(("https://accounts.google.com", "test-google-client-id")),
        "kakao" => Ok(("https://kauth.kakao.com", "test-kakao-app-key")),
        "apple" => Ok(("https://appleid.apple.com", "test-apple-client-id")),
        other => Err(format!(
            "id_token.provider는 google, kakao, apple 중 하나여야 합니다: {other}"
        )),
    }
}

/// `clock`(시나리오 시작 epoch 초) 기준으로 ID Token을 만든다. `rawNonce`의 SHA-256 소문자 hex가 nonce 클레임이 된다.
pub fn sign(spec: &IdTokenSpec, clock: i64) -> Result<String, String> {
    let (iss, aud) = provider_defaults(&spec.provider)?;
    let iat = clock + spec.issued_in.unwrap_or(0);
    let exp = clock + spec.expires_in.unwrap_or(600);

    let mut claims = Map::new();
    claims.insert("iss".into(), json!(spec.iss.as_deref().unwrap_or(iss)));
    claims.insert("aud".into(), json!(spec.aud.as_deref().unwrap_or(aud)));
    claims.insert("sub".into(), json!(spec.sub));
    claims.insert("iat".into(), json!(iat));
    claims.insert("exp".into(), json!(exp));
    if let Some(raw) = &spec.nonce {
        claims.insert("nonce".into(), json!(sha256_hex(raw)));
    }
    if let Some(email) = &spec.email {
        claims.insert("email".into(), json!(email));
    }
    if let Some(v) = &spec.email_verified {
        claims.insert("email_verified".into(), v.clone());
    }
    for (k, v) in &spec.claims {
        claims.insert(k.clone(), v.clone());
    }
    for k in &spec.omit {
        claims.remove(k);
    }

    let header = json!({
        "alg": "RS256",
        "kid": spec.kid.as_deref().unwrap_or(KEY_ID),
        "typ": "JWT",
    });
    let signing_input = format!(
        "{}.{}",
        base64url_encode(header.to_string().as_bytes()),
        base64url_encode(Value::Object(claims).to_string().as_bytes())
    );
    let key = RsaPrivateKey::from_pkcs8_pem(PRIVATE_KEY_PEM).map_err(|e| e.to_string())?;
    let sig = SigningKey::<Sha256>::new(key)
        .sign(signing_input.as_bytes())
        .to_bytes();
    let mut sig = base64url_encode(&sig);
    if spec.tamper_signature {
        // 마지막 문자를 바꿔 서명을 깨뜨린다. 형식은 그대로 유효한 JWT다.
        let last = sig.pop().unwrap_or('A');
        sig.push(if last == 'A' { 'B' } else { 'A' });
    }
    Ok(format!("{signing_input}.{sig}"))
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base64url_decode;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::traits::PublicKeyParts;

    const JWKS: &str = include_str!("../../keys/jwks.json");

    fn spec(extra: &str) -> IdTokenSpec {
        toml::from_str(&format!(
            "name = \"t\"\nprovider = \"google\"\nsub = \"sub-1\"\n{extra}"
        ))
        .unwrap()
    }

    fn parts(token: &str) -> (Value, Value, Vec<u8>) {
        let p: Vec<&str> = token.split('.').collect();
        assert_eq!(p.len(), 3);
        (
            serde_json::from_slice(&base64url_decode(p[0]).unwrap()).unwrap(),
            serde_json::from_slice(&base64url_decode(p[1]).unwrap()).unwrap(),
            base64url_decode(p[2]).unwrap(),
        )
    }

    #[test]
    fn 개인키와_jwks_공개키가_짝이다() {
        let key = RsaPrivateKey::from_pkcs8_pem(PRIVATE_KEY_PEM).unwrap();
        let jwks: Value = serde_json::from_str(JWKS).unwrap();
        let k = &jwks["keys"][0];
        assert_eq!(k["kid"], KEY_ID);
        assert_eq!(k["e"], "AQAB");
        let n = base64url_encode(&key.to_public_key().n().to_bytes_be());
        assert_eq!(k["n"], n.as_str());
    }

    #[test]
    fn 서명이_공개키로_검증된다() {
        let token = sign(&spec("nonce = \"n1\"\nemail = \"a@b.c\""), 1000).unwrap();
        let (header, claims, sig) = parts(&token);
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["kid"], KEY_ID);
        assert_eq!(claims["iss"], "https://accounts.google.com");
        assert_eq!(claims["aud"], "test-google-client-id");
        assert_eq!(claims["iat"], 1000);
        assert_eq!(claims["exp"], 1600);
        assert_eq!(claims["email"], "a@b.c");

        let key = RsaPrivateKey::from_pkcs8_pem(PRIVATE_KEY_PEM).unwrap();
        let verifier = VerifyingKey::<Sha256>::new(key.to_public_key());
        let signing_input = token.rsplit_once('.').unwrap().0;
        let sig = Signature::try_from(sig.as_slice()).unwrap();
        verifier.verify(signing_input.as_bytes(), &sig).unwrap();
    }

    #[test]
    fn nonce_클레임은_원본_nonce의_sha256_hex다() {
        let token = sign(&spec("nonce = \"abc\""), 0).unwrap();
        assert_eq!(
            parts(&token).1["nonce"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn 같은_입력은_같은_토큰이다() {
        let s = spec("");
        assert_eq!(sign(&s, 5).unwrap(), sign(&s, 5).unwrap());
    }

    #[test]
    fn 시각_오프셋과_클레임_덮어쓰기와_제거를_적용한다() {
        let s = spec(
            "issued_in = -30\nexpires_in = -120\nomit = [\"iat\"]\nemail_verified = true\naud = \"other\"\n[claims]\nextra = \"x\"",
        );
        let (_, c, _) = parts(&sign(&s, 1000).unwrap());
        assert_eq!(c["exp"], 880);
        assert!(c.get("iat").is_none());
        assert_eq!(c["email_verified"], true);
        assert_eq!(c["aud"], "other");
        assert_eq!(c["extra"], "x");
    }

    #[test]
    fn 서명을_깨뜨리면_검증이_실패한다() {
        let ok = sign(&spec(""), 0).unwrap();
        let bad = sign(&spec("tamper_signature = true"), 0).unwrap();
        assert_ne!(ok, bad);
        assert_eq!(
            ok.split('.').take(2).collect::<Vec<_>>(),
            bad.split('.').take(2).collect::<Vec<_>>()
        );
    }

    #[test]
    fn 알_수_없는_제공자는_거절한다() {
        let mut s = spec("");
        s.provider = "naver".into();
        assert!(sign(&s, 0).is_err());
    }
}
