#![allow(dead_code)] // 테스트 파일마다 쓰는 도구가 다르다.

use rsa::signature::{SignatureEncoding, Signer};
use rsa::{BigUint, RsaPrivateKey, pkcs1v15::SigningKey};
use serde_json::{Value, json};
use sha2::Sha256;

pub fn rfc() -> Value {
    serde_json::from_str(include_str!("../fixtures/rfc7515.json")).unwrap()
}

pub fn oracle() -> Value {
    serde_json::from_str(include_str!("../fixtures/oracle.json")).unwrap()
}

pub fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn b64url_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("bad char"),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// RFC 7515 A.2 개인키로 RS256 토큰을 만든다. 헤더·페이로드는 호출자가 만든 JSON 그대로 쓴다.
pub fn rfc_rsa_signer() -> SigningKey<Sha256> {
    static KEY: std::sync::OnceLock<SigningKey<Sha256>> = std::sync::OnceLock::new();
    KEY.get_or_init(build_rfc_rsa_signer).clone()
}

fn build_rfc_rsa_signer() -> SigningKey<Sha256> {
    let rfc = rfc();
    let rsa = &rfc["rs256"];
    let big = |name: &str| BigUint::from_bytes_be(&b64url_decode(s(&rsa[name])));
    let key =
        RsaPrivateKey::from_components(big("n"), big("e"), big("d"), vec![big("p"), big("q")])
            .expect("RFC 7515 A.2 key");
    SigningKey::<Sha256>::new(key)
}

pub fn rs256_token(header: &Value, claims: &Value) -> String {
    let input = format!(
        "{}.{}",
        b64url_encode(serde_json::to_string(header).unwrap().as_bytes()),
        b64url_encode(serde_json::to_string(claims).unwrap().as_bytes())
    );
    let sig = rfc_rsa_signer().sign(input.as_bytes()).to_bytes();
    format!("{input}.{}", b64url_encode(&sig))
}

/// RFC A.2 공개키 JWK. 필드는 호출자가 덧붙인다.
pub fn rfc_rsa_jwk(extra: Value) -> Value {
    let rfc = rfc();
    let mut jwk = json!({"kty": "RSA", "n": rfc["rs256"]["n"], "e": rfc["rs256"]["e"]});
    for (k, v) in extra.as_object().unwrap() {
        jwk[k] = v.clone();
    }
    jwk
}

pub fn jwks_json(keys: &[Value]) -> String {
    json!({"keys": keys}).to_string()
}
