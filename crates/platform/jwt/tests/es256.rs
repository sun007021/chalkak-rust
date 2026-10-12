//! ES256 서명·검증. 근거:
//! - RFC 7515 부록 A.3
//! - 원본 AppleClientSecretGenerator(Nimbus ECDSASigner, PKCS#8 개인키): Java가 만든 client secret을 Rust가 검증하고,
//!   Java와 같은 PEM으로 Rust가 서명한 토큰은 형식(헤더·클레임 순서, 64바이트 r||s)이 같다.
//!   Rust 서명 토큰을 Java가 검증하는 쪽은 tests/cross_java.rs.

mod common;

use chalkak_jwt::original::{apple_client_secret_claims, apple_client_secret_header};
use chalkak_jwt::{Es256PublicKey, Es256Signer, JwtError, es256_sign, es256_verify};
use common::*;
use serde_json::{Value, json};

fn rfc_public_key() -> Es256PublicKey {
    let rfc = rfc();
    Es256PublicKey::from_coordinates(s(&rfc["es256"]["x"]), s(&rfc["es256"]["y"])).unwrap()
}

fn oracle_es() -> (Value, Es256PublicKey) {
    let oracle = oracle();
    let es = oracle["es256"].clone();
    let key =
        Es256PublicKey::from_coordinates(s(&es["public_jwk"]["x"]), s(&es["public_jwk"]["y"]))
            .unwrap();
    (es, key)
}

#[test]
fn rfc7515_a3_es256_토큰을_검증한다() {
    let rfc = rfc();
    let claims = es256_verify(s(&rfc["es256"]["token"]), &rfc_public_key()).unwrap();
    assert_eq!(claims.issuer(), Some("joe"));
    assert_eq!(claims.expires_at(), Some(1_300_819_380));
}

#[test]
fn rfc7515_a3_서명이_다르면_거절한다() {
    let rfc = rfc();
    let token = s(&rfc["es256"]["token"]);
    let mut tampered = token.to_string();
    tampered.replace_range(token.len() - 6..token.len() - 5, "A");
    assert_eq!(
        es256_verify(&tampered, &rfc_public_key()),
        Err(JwtError::InvalidSignature)
    );
    // 서명은 JOSE 형식 r||s 64바이트여야 한다(DER 등 다른 길이는 거절).
    let parts: Vec<&str> = token.split('.').collect();
    for len in [0usize, 63, 65, 70, 72] {
        let sig = b64url_encode(&vec![1u8; len]);
        let t = format!("{}.{}.{}", parts[0], parts[1], sig);
        assert_eq!(
            es256_verify(&t, &rfc_public_key()),
            Err(JwtError::InvalidSignature),
            "len {len}"
        );
    }
    // r=s=0
    let zero = format!("{}.{}.{}", parts[0], parts[1], b64url_encode(&[0u8; 64]));
    assert_eq!(
        es256_verify(&zero, &rfc_public_key()),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn rfc7515_a3_개인키_d로_서명하면_공개키로_검증되고_결정적이다() {
    // RFC 6979 결정적 ECDSA: 같은 입력이면 같은 토큰. (Nimbus는 무작위 k라 Java 토큰과 바이트 비교는 하지 않는다.)
    let rfc = rfc();
    let signer = Es256Signer::from_secret_scalar(&b64url_decode(s(&rfc["es256"]["d"]))).unwrap();
    let header = json!({"alg": "ES256"});
    let claims = json!({"iss": "joe", "exp": 1_300_819_380, "http://example.com/is_root": true});
    let a = signer.sign(&header, &claims);
    let b = signer.sign(&header, &claims);
    assert_eq!(a, b);
    let verified = es256_verify(&a, &rfc_public_key()).unwrap();
    assert_eq!(verified.issuer(), Some("joe"));
    // 서명은 정확히 64바이트(JOSE r||s)
    let sig = a.rsplit('.').next().unwrap();
    assert_eq!(b64url_decode(sig).len(), 64);
}

#[test]
fn 알고리즘이_es256이_아니면_거절한다() {
    let rfc = rfc();
    let signer = Es256Signer::from_secret_scalar(&b64url_decode(s(&rfc["es256"]["d"]))).unwrap();
    for alg in ["ES384", "RS256", "HS256", "none"] {
        let token = signer.sign(&json!({"alg": alg}), &json!({"sub": "x"}));
        assert!(
            matches!(
                es256_verify(&token, &rfc_public_key()),
                Err(JwtError::UnsupportedAlgorithm(_))
            ),
            "alg {alg}"
        );
    }
}

#[test]
fn 원본이_만든_apple_client_secret을_공개키로_검증한다() {
    let (es, key) = oracle_es();
    let now = oracle()["now_epoch"].as_i64().unwrap();
    let claims = es256_verify(s(&es["client_secret"]), &key).unwrap();
    assert_eq!(claims.issuer(), Some(s(&es["team_id"])));
    assert_eq!(claims.subject(), Some(s(&es["client_id"])));
    assert_eq!(claims.audience(), Some(vec!["https://appleid.apple.com"]));
    assert_eq!(claims.issued_at(), Some(now));
    assert_eq!(claims.expires_at(), Some(now + 300));
}

#[test]
fn 같은_pem으로_서명하면_원본과_같은_형식의_client_secret이_나온다() {
    let (es, key) = oracle_es();
    let now = oracle()["now_epoch"].as_i64().unwrap();
    let header = apple_client_secret_header(s(&es["key_id"]));
    let claims = apple_client_secret_claims(s(&es["team_id"]), s(&es["client_id"]), now);
    let token = es256_sign(&header, &claims, s(&es["private_key_pem"])).unwrap();

    // 헤더와 클레임(서명 제외)은 Java가 낸 토큰과 바이트가 같다.
    let java: Vec<&str> = s(&es["client_secret"]).split('.').collect();
    let rust: Vec<&str> = token.split('.').collect();
    assert_eq!(rust[0], java[0]);
    assert_eq!(rust[1], java[1]);
    // 서명은 다르지만(Java 무작위 k, Rust RFC 6979) 둘 다 같은 공개키로 검증된다.
    assert_ne!(rust[2], java[2]);
    assert!(es256_verify(&token, &key).is_ok());
    assert!(es256_verify(s(&es["client_secret"]), &key).is_ok());
}

#[test]
fn apple_개인키_설정값_base64_pem도_받는다() {
    // AppleTokenProperties.privateKeyBase64 = Base64(PEM). 원본은 생성자에서 파싱해 기동 시점에 실패한다.
    let (es, key) = oracle_es();
    let pem = s(&es["private_key_pem"]);
    let signer_pem = Es256Signer::from_pkcs8_pem(pem).unwrap();
    let encoded = {
        // 표준 base64(패딩 포함)
        const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in pem.as_bytes().chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(STD[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    };
    let signer_b64 = Es256Signer::from_base64_pem(&encoded).unwrap();
    let claims = json!({"a": 1});
    let header = json!({"alg": "ES256"});
    assert_eq!(
        signer_pem.sign(&header, &claims),
        signer_b64.sign(&header, &claims)
    );
    assert!(es256_verify(&signer_b64.sign(&header, &claims), &key).is_ok());
}

#[test]
fn 잘못된_개인키는_만들_때_거절한다() {
    let (es, _) = oracle_es();
    let pem = s(&es["private_key_pem"]);
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    for bad in [
        String::new(),
        "not a pem".to_string(),
        // 머리말만 있고 본문이 없음
        "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n".to_string(),
        // 본문이 base64가 아님
        "-----BEGIN PRIVATE KEY-----\n!!!!\n-----END PRIVATE KEY-----\n".to_string(),
        // 다른 종류의 PEM 머리말(SEC1): 원본은 "-----BEGIN PRIVATE KEY-----" 문자열을 찾으므로 거절
        format!("-----BEGIN EC PRIVATE KEY-----\n{body}\n-----END EC PRIVATE KEY-----\n"),
        // PKCS#8이 아닌 내용
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n".to_string(),
        // 끝 머리말 없음
        format!("-----BEGIN PRIVATE KEY-----\n{body}\n"),
    ] {
        assert!(
            matches!(
                es256_sign(&json!({"alg": "ES256"}), &json!({}), &bad),
                Err(JwtError::InvalidKey(_))
            ),
            "{bad:?}"
        );
    }
    // 줄 바꿈 없이 한 줄이어도, 공백이 섞여 있어도 받는다(원본은 \s 를 모두 제거한다).
    let one_line = format!("-----BEGIN PRIVATE KEY-----{body}-----END PRIVATE KEY-----");
    assert!(Es256Signer::from_pkcs8_pem(&one_line).is_ok());
    let spaced = format!(
        "  -----BEGIN PRIVATE KEY-----\r\n  {}\r\n-----END PRIVATE KEY-----  ",
        body
    );
    assert!(Es256Signer::from_pkcs8_pem(&spaced).is_ok());
}

#[test]
fn 공개키_좌표가_곡선_위에_없으면_거절한다() {
    assert!(matches!(
        Es256PublicKey::from_coordinates("AAAA", "AAAA"),
        Err(JwtError::InvalidKey(_))
    ));
    assert!(Es256PublicKey::from_coordinates("!!", "AAAA").is_err());
}
