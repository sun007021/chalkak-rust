//! HS256 발급·검증. 근거:
//! - RFC 7515 부록 A.1 예제 벡터
//! - 원본 JwtAccessTokenProvider / JwtSocialSignupTokenProvider(NimbusJwtEncoder)가 만든 토큰과 바이트 동일
//!   (기대값은 tests/fixtures/GenFixtures.java가 원본 클래스를 직접 실행해 oracle.json에 남긴다)

use chalkak_jwt::original::{
    AccessTokenClaims, SocialSignupClaims, access_token_header, social_signup_header,
};
use chalkak_jwt::{JwtError, decode_hex, hs256_decode, hs256_encode};
use serde_json::{Value, json};

const RFC: &str = include_str!("fixtures/rfc7515.json");
const ORACLE: &str = include_str!("fixtures/oracle.json");

fn rfc() -> Value {
    serde_json::from_str(RFC).unwrap()
}

fn oracle() -> Value {
    serde_json::from_str(ORACLE).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn b64url_decode(text: &str) -> Vec<u8> {
    // 테스트 전용 간이 디코더(패딩 없는 base64url).
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

#[test]
fn rfc7515_a1_hs256_토큰을_검증한다() {
    let rfc = rfc();
    let key = b64url_decode(s(&rfc["hs256"]["k"]));
    let claims = hs256_decode(s(&rfc["hs256"]["token"]), &key).unwrap();
    assert_eq!(claims.issuer(), Some("joe"));
    assert_eq!(claims.expires_at(), Some(1_300_819_380));
    assert_eq!(
        claims.get("http://example.com/is_root"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn rfc7515_a1_서명이_한_비트라도_다르면_거절한다() {
    let rfc = rfc();
    let key = b64url_decode(s(&rfc["hs256"]["k"]));
    let token = s(&rfc["hs256"]["token"]);
    // 마지막 글자를 바꾼다(서명 변조).
    let mut tampered = token.to_string();
    tampered.pop();
    tampered.push('A');
    assert_eq!(
        hs256_decode(&tampered, &key),
        Err(JwtError::InvalidSignature)
    );
    // 다른 키
    assert_eq!(
        hs256_decode(token, b"wrong-key"),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn 페이로드를_바꾸면_서명이_맞지_않는다() {
    let rfc = rfc();
    let key = b64url_decode(s(&rfc["hs256"]["k"]));
    let token = s(&rfc["hs256"]["token"]);
    let parts: Vec<&str> = token.split('.').collect();
    let other = hs256_encode(
        &json!({"typ":"JWT","alg":"HS256"}),
        &json!({"iss":"mallory"}),
        &key,
    );
    let other_payload = other.split('.').nth(1).unwrap();
    let forged = format!("{}.{}.{}", parts[0], other_payload, parts[2]);
    assert_eq!(hs256_decode(&forged, &key), Err(JwtError::InvalidSignature));
}

#[test]
fn 알고리즘이_hs256이_아니면_거절한다() {
    // NimbusJwtDecoder.withSecretKey(..).macAlgorithm(HS256): 다른 alg는 키 선택에서 탈락한다.
    let key = b"0123456789abcdef0123456789abcdef";
    for alg in ["HS384", "HS512", "RS256", "none", "hs256"] {
        let token = hs256_encode(&json!({"typ":"JWT","alg":alg}), &json!({"sub":"x"}), key);
        assert!(
            matches!(
                hs256_decode(&token, key),
                Err(JwtError::UnsupportedAlgorithm(_))
            ),
            "alg {alg}"
        );
    }
}

#[test]
fn 형식이_틀린_토큰은_거절한다() {
    let key = b"k";
    for token in [
        "",
        "abc",
        "a.b",
        "a.b.c.d",
        "a.b.c.d.e", // 5조각은 JWE
        "not-a-jwt",
        "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.AAAA", // 서명 위조
        "..",
        "e30.e30.", // 빈 서명
    ] {
        assert!(hs256_decode(token, key).is_err(), "token {token:?}");
    }
}

#[test]
fn 헤더에_crit이_있으면_거절한다() {
    // Nimbus DefaultJWSProcessor: 처리할 수 없는 crit 헤더는 거절.
    let key = b"0123456789abcdef0123456789abcdef";
    let token = hs256_encode(
        &json!({"typ":"JWT","alg":"HS256","crit":["x"],"x":1}),
        &json!({"sub":"x"}),
        key,
    );
    assert!(matches!(
        hs256_decode(&token, key),
        Err(JwtError::UnsupportedAlgorithm(_))
    ));
}

#[test]
fn 클레임의_형식이_틀리면_서명이_맞아도_거절한다() {
    // Nimbus JWTClaimsSet.parse: exp 문자열, iss 숫자 등은 "Malformed payload".
    let key = b"0123456789abcdef0123456789abcdef";
    let header = json!({"typ":"JWT","alg":"HS256"});
    for claims in [
        json!({"exp":"123"}),
        json!({"nbf":"abc"}),
        json!({"iat":"1"}),
        json!({"iss":5}),
        json!({"aud":[5]}),
        json!({"aud":5}),
        json!({"jti":5}),
    ] {
        let token = hs256_encode(&header, &claims, key);
        assert!(
            matches!(hs256_decode(&token, key), Err(JwtError::Malformed(_))),
            "claims {claims}"
        );
    }
    // 페이로드가 JSON 객체가 아니면 거절
    let payload_array = hs256_encode(&header, &json!([1, 2]), key);
    assert!(matches!(
        hs256_decode(&payload_array, key),
        Err(JwtError::Malformed(_))
    ));
}

#[test]
fn exp가_iat보다_앞서면_거절한다_spring_jwt_생성자의_검증() {
    // 하네스 "공개 GET 만료 10초 지남": iat=now, exp=now-10인 토큰이 원본에서 401이었던 원인.
    // 허용 오차(30초)와 무관하게 Spring Jwt 생성자가 "expiresAt must be after issuedAt"으로 거절한다.
    let key = b"0123456789abcdef0123456789abcdef";
    let header = json!({"typ":"JWT","alg":"HS256"});
    for (iat, exp) in [(1_000, 990), (1_000, 1_000)] {
        let token = hs256_encode(&header, &json!({"iat": iat, "exp": exp}), key);
        match hs256_decode(&token, key) {
            Err(JwtError::InvalidClaims(message)) => {
                assert_eq!(message, "expiresAt must be after issuedAt")
            }
            other => panic!("iat {iat} exp {exp}: {other:?}"),
        }
    }
    // iat가 없으면 비교하지 않는다(원본 액세스 토큰 디코더는 iat 없는 토큰을 통과시킨다).
    let token = hs256_encode(&header, &json!({"exp": 990}), key);
    assert!(hs256_decode(&token, key).is_ok());
    // exp가 iat보다 1초라도 뒤면 통과
    let token = hs256_encode(&header, &json!({"iat": 1_000, "exp": 1_001}), key);
    assert!(hs256_decode(&token, key).is_ok());
}

#[test]
fn 널_값_클레임은_없는_것으로_본다() {
    let key = b"0123456789abcdef0123456789abcdef";
    let token = hs256_encode(
        &json!({"typ":"JWT","alg":"HS256"}),
        &json!({"exp": null, "aud": null, "sub": "x"}),
        key,
    );
    let claims = hs256_decode(&token, key).unwrap();
    assert_eq!(claims.expires_at(), None);
    assert_eq!(claims.audience(), None);
}

#[test]
fn 원본_액세스_토큰과_바이트가_같다_jwt_access_token_provider() {
    let oracle = oracle();
    let access = &oracle["access"];
    let key = decode_hex(s(&access["secret_hex"])).unwrap();
    let now = oracle["now_epoch"].as_i64().unwrap();
    let tokens = access["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2);
    for t in tokens {
        let claims = AccessTokenClaims {
            issuer: s(&access["issuer"]),
            audience: s(&access["audience"]),
            subject: s(&t["sub"]),
            issued_at: now,
            expires_at: now + access["expiration_secs"].as_i64().unwrap(),
            jti: s(&t["jti"]),
            scope: s(&t["scope"]),
            session_id: None,
        };
        let issued = hs256_encode(&access_token_header(), &claims.to_value(), &key);
        assert_eq!(issued, s(&t["token"]), "scope {}", s(&t["scope"]));
        // 우리가 만든 토큰을 우리가 다시 읽을 수 있다.
        let decoded = hs256_decode(&issued, &key).unwrap();
        assert_eq!(decoded.get_str("purpose"), Some("ACCESS"));
        assert_eq!(decoded.get_str("scope"), Some(s(&t["scope"])));
        assert_eq!(decoded.audience(), Some(vec![s(&access["audience"])]));
    }
}

#[test]
fn 원본_소셜_가입_토큰과_바이트가_같다_jwt_social_signup_token_provider() {
    let oracle = oracle();
    let signup = &oracle["signup"];
    let key = decode_hex(s(&signup["secret_hex"])).unwrap();
    let now = oracle["now_epoch"].as_i64().unwrap();
    let tokens = signup["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 3); // 이메일 있음, 이메일 없음, 특수 문자
    for t in tokens {
        let claims = SocialSignupClaims {
            issuer: s(&signup["issuer"]),
            audience: s(&signup["audience"]),
            subject: s(&t["sub"]),
            issued_at: now,
            expires_at: now + signup["expiration_secs"].as_i64().unwrap(),
            jti: s(&t["jti"]),
            provider: s(&t["provider"]),
            upload_id: s(&t["upload_id"]),
            email: t["email"].as_str(),
        };
        let issued = hs256_encode(&social_signup_header(), &claims.to_value(), &key);
        assert_eq!(issued, s(&t["token"]), "sub {}", s(&t["sub"]));
    }
}

#[test]
fn 원본이_만든_토큰을_읽어_다시_인코딩해도_같다() {
    // 헤더·클레임 순서와 이스케이프를 원본 토큰에서 그대로 읽어 온 값으로도 확인한다.
    let oracle = oracle();
    for (kind, secret) in [("access", "secret_hex"), ("signup", "secret_hex")] {
        let key = decode_hex(s(&oracle[kind][secret])).unwrap();
        for t in oracle[kind]["tokens"].as_array().unwrap() {
            let token = s(&t["token"]);
            let parts: Vec<&str> = token.split('.').collect();
            let header: Value = serde_json::from_slice(&b64url_decode(parts[0])).unwrap();
            let payload: Value = serde_json::from_slice(&b64url_decode(parts[1])).unwrap();
            assert_eq!(hs256_encode(&header, &payload, &key), token);
        }
    }
}

#[test]
fn 잘못된_16진수_비밀키는_거절한다() {
    // HexFormat.of().parseHex: 홀수 길이, 16진수가 아닌 글자는 IllegalArgumentException.
    assert!(decode_hex("abc").is_err());
    assert!(decode_hex("zz").is_err());
    assert!(decode_hex("0g").is_err());
    assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
    assert_eq!(decode_hex("00FfA0").unwrap(), vec![0x00, 0xff, 0xa0]);
}

#[test]
fn json_직렬화는_nimbus_gson_이스케이프를_따른다() {
    // 원본 토큰 payload에서 확인한 규칙: / 와 <>&'= 는 그대로, \b \f \n \r \t 는 짧은 이스케이프,
    // 나머지 제어 문자는 \u00xx(소문자), U+2028/2029 는  / , 0x7f 는 그대로.
    let key = b"k";
    let token = hs256_encode(
        &json!({"alg":"HS256"}),
        &json!({"s": "\"\\/\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{7f} <>&'=é한\u{2028}\u{2029}😀"}),
        key,
    );
    let payload = String::from_utf8(b64url_decode(token.split('.').nth(1).unwrap())).unwrap();
    assert_eq!(
        payload,
        "{\"s\":\"\\\"\\\\/\\b\\f\\n\\r\\t\\u0001\\u001f\u{7f} <>&'=é한\\u2028\\u2029😀\"}"
    );
}
