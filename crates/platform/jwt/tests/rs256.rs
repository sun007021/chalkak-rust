//! RS256 검증과 JWKS 해석. 근거:
//! - RFC 7515 부록 A.2
//! - 원본 OidcIdTokenDecoderFactory(NimbusJwtDecoder.withJwkSource) — 기대값은 tests/fixtures/ProbeOidcRules.java 출력
//! - Java(Nimbus RSASSASigner)가 만든 RS256 토큰: tests/fixtures/oracle.json

mod common;

use chalkak_jwt::{ClaimError, Jwks, JwksError, JwtError, Validator, rs256_verify};
use common::*;
use serde_json::{Value, json};

const NOW: i64 = 1_791_763_200;

fn claims_ok() -> Value {
    json!({"iss": "https://iss", "aud": "A", "sub": "s", "iat": NOW, "exp": NOW + 300})
}

fn header(extra: Value) -> Value {
    let mut h = json!({"alg": "RS256"});
    for (k, v) in extra.as_object().unwrap() {
        h[k] = v.clone();
    }
    h
}

fn jwks_with(keys: &[Value]) -> Jwks {
    Jwks::parse(&jwks_json(keys)).unwrap()
}

fn verify_with(extra_header: Value, jwk_extra: Value) -> Result<chalkak_jwt::Claims, JwtError> {
    let token = rs256_token(&header(extra_header), &claims_ok());
    rs256_verify(&token, &jwks_with(&[rfc_rsa_jwk(jwk_extra)]))
}

#[test]
fn rfc7515_a2_rs256_토큰을_검증한다() {
    let rfc = rfc();
    let jwks = jwks_with(&[json!({"kty":"RSA","n":rfc["rs256"]["n"],"e":rfc["rs256"]["e"]})]);
    let claims = rs256_verify(s(&rfc["rs256"]["token"]), &jwks).unwrap();
    assert_eq!(claims.issuer(), Some("joe"));
    assert_eq!(claims.expires_at(), Some(1_300_819_380));
}

#[test]
fn rfc7515_a2_서명을_바꾸면_거절한다() {
    let rfc = rfc();
    let jwks = jwks_with(&[rfc_rsa_jwk(json!({}))]);
    let token = s(&rfc["rs256"]["token"]);
    let mut tampered = token.to_string();
    tampered.replace_range(token.len() - 6..token.len() - 5, "A");
    assert_eq!(
        rs256_verify(&tampered, &jwks),
        Err(JwtError::InvalidSignature)
    );
    // 길이가 모자란 서명
    let short = &token[..token.len() - 8];
    assert_eq!(rs256_verify(short, &jwks), Err(JwtError::InvalidSignature));
}

#[test]
fn 테스트_서명기가_만든_토큰도_같은_키로_검증된다() {
    assert!(verify_with(json!({}), json!({})).is_ok());
}

#[test]
fn 원본_nimbus가_서명한_rs256_토큰을_검증한다_kid로_키를_고른다() {
    let oracle = oracle();
    let rs = &oracle["rs256"];
    let both = Jwks::parse(s(&rs["jwks_k1_k2"])).unwrap();
    let only_k1 = Jwks::parse(s(&rs["jwks_k1"])).unwrap();
    let tokens = rs["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2);
    for t in tokens {
        let claims = rs256_verify(s(&t["token"]), &both).unwrap();
        assert_eq!(claims.subject(), Some("google-subject"));
        assert_eq!(claims.get_str("nonce"), Some("abc"));
    }
    // kid-2 키가 없는 목록으로는 kid-2 토큰을 풀 수 없다.
    assert!(rs256_verify(s(&tokens[0]["token"]), &only_k1).is_ok());
    assert_eq!(
        rs256_verify(s(&tokens[1]["token"]), &only_k1),
        Err(JwtError::NoMatchingKey)
    );
    // 검증기까지 이어서: Google ID 토큰 설정과 같게
    let claims = rs256_verify(s(&tokens[0]["token"]), &both).unwrap();
    let v = Validator::oidc_id_token("https://accounts.google.com", "google-client-id");
    assert_eq!(v.validate(&claims, NOW * 1000 + 1_000), Ok(()));
    assert_eq!(
        v.validate(&claims, (NOW + 3600 + 61) * 1000),
        Err(ClaimError::Expired)
    );
}

#[test]
fn kid가_가리키는_키로만_검증하고_다른_키의_서명은_거절한다() {
    let oracle = oracle();
    let rs = &oracle["rs256"];
    let both = Jwks::parse(s(&rs["jwks_k1_k2"])).unwrap();
    // kid-1 토큰의 kid를 kid-2로 바꾸면(서명 입력이 달라져 서명도 깨진다) 거절
    let tokens = rs["tokens"].as_array().unwrap();
    let parts: Vec<&str> = s(&tokens[0]["token"]).split('.').collect();
    let header: Value = serde_json::from_slice(&b64url_decode(parts[0])).unwrap();
    let swapped = {
        let mut h = header.clone();
        h["kid"] = json!("kid-2");
        format!(
            "{}.{}.{}",
            b64url_encode(h.to_string().as_bytes()),
            parts[1],
            parts[2]
        )
    };
    assert_eq!(
        rs256_verify(&swapped, &both),
        Err(JwtError::InvalidSignature)
    );
}

#[test]
fn kid가_없으면_조건에_맞는_모든_rsa_키를_차례로_시도한다() {
    // Nimbus JWSVerificationKeySelector + DefaultJWTProcessor: 후보 키를 하나씩 시도해 맞으면 통과.
    let oracle = oracle();
    let both: Value = serde_json::from_str(s(&oracle["rs256"]["jwks_k1_k2"])).unwrap();
    let mut keys = both["keys"].as_array().unwrap().clone();
    keys.push(rfc_rsa_jwk(json!({"kid": "rfc"})));
    let jwks = jwks_with(&keys);
    let token = rs256_token(&header(json!({})), &claims_ok()); // kid 없음, RFC 키로 서명
    assert!(rs256_verify(&token, &jwks).is_ok());
}

#[test]
fn 알고리즘이_rs256이_아니면_키를_찾지_못한_것으로_거절한다() {
    // ProbeOidcRules: "FAIL RS384 :: ... Another algorithm expected, or no matching key(s) found"
    for alg in ["RS384", "RS512", "HS256", "ES256", "PS256", "none"] {
        let token = rs256_token(&json!({"alg": alg}), &claims_ok());
        let jwks = jwks_with(&[rfc_rsa_jwk(json!({}))]);
        assert!(rs256_verify(&token, &jwks).is_err(), "alg {alg}");
    }
    let token = rs256_token(&json!({"alg": "RS384"}), &claims_ok());
    assert_eq!(
        rs256_verify(&token, &jwks_with(&[rfc_rsa_jwk(json!({}))])),
        Err(JwtError::NoMatchingKey)
    );
}

#[test]
fn typ는_jwt만_허용한다_대소문자_무시() {
    // ProbeOidcRules: typ jwt/JWT/Jwt 통과, at+jwt/JOSE 거절 ("the given typ value needs to be one of [JWT]")
    for ok in ["JWT", "jwt", "Jwt"] {
        assert!(
            verify_with(json!({"typ": ok}), json!({})).is_ok(),
            "typ {ok}"
        );
    }
    for bad in ["at+jwt", "JOSE", "foo", ""] {
        assert!(
            matches!(
                verify_with(json!({"typ": bad}), json!({})),
                Err(JwtError::UnsupportedAlgorithm(_))
            ),
            "typ {bad}"
        );
    }
    // typ가 없으면 통과
    assert!(verify_with(json!({}), json!({})).is_ok());
}

#[test]
fn crit_헤더는_거절한다() {
    assert!(matches!(
        verify_with(json!({"crit": ["x"], "x": 1}), json!({})),
        Err(JwtError::UnsupportedAlgorithm(_))
    ));
}

#[test]
fn jwk_조건_use_alg_kid() {
    // JWKMatcher.forJWSHeader: kty=RSA, use ∈ {sig, 없음}, alg ∈ {RS256, 없음}, kid 일치.
    // key_ops는 보지 않는다. (ProbeOidcRules의 jwk use=enc / alg=RS384 / key_ops 결과)
    assert!(verify_with(json!({}), json!({"use": "sig"})).is_ok());
    assert!(verify_with(json!({}), json!({"alg": "RS256"})).is_ok());
    assert!(verify_with(json!({}), json!({"key_ops": ["verify"]})).is_ok());
    assert!(verify_with(json!({}), json!({"key_ops": ["sign"]})).is_ok());
    assert_eq!(
        verify_with(json!({}), json!({"use": "enc"})),
        Err(JwtError::NoMatchingKey)
    );
    assert_eq!(
        verify_with(json!({}), json!({"alg": "RS384"})),
        Err(JwtError::NoMatchingKey)
    );
    assert!(verify_with(json!({"kid": "a"}), json!({"kid": "a"})).is_ok());
    assert_eq!(
        verify_with(json!({"kid": "a"}), json!({"kid": "b"})),
        Err(JwtError::NoMatchingKey)
    );
    // 토큰에 kid가 없으면 JWK의 kid는 조건이 아니다
    assert!(verify_with(json!({}), json!({"kid": "b"})).is_ok());
}

#[test]
fn rsa가_아닌_키는_후보에서_빠진다() {
    let token = rs256_token(&header(json!({"kid": "k"})), &claims_ok());
    let jwks = jwks_with(&[json!({"kty": "oct", "k": "AAAA", "kid": "k"})]);
    assert_eq!(rs256_verify(&token, &jwks), Err(JwtError::NoMatchingKey));
}

#[test]
fn jwks_해석_규칙_jwk_set_parse() {
    // ProbeOidcRules 마지막 구간(Nimbus JWKSet.parse) 결과
    // 지원하지 않는 kty는 건너뛴다.
    let set = Jwks::parse(r#"{"keys":[{"kty":"FOO","kid":"o"}]}"#).unwrap();
    assert_eq!(set.len(), 0);
    // oct 같은 다른 종류의 키는 목록에 남는다(kid 조회용).
    let set = Jwks::parse(r#"{"keys":[{"kty":"oct","k":"AAAA","kid":"o"}]}"#).unwrap();
    assert_eq!(set.len(), 1);
    assert!(set.has_key_id("o"));
    assert!(!set.has_key_id("x"));
    // 빈 목록
    assert_eq!(Jwks::parse(r#"{"keys":[]}"#).unwrap().len(), 0);
    // 형식 오류는 목록 전체를 거절한다.
    for bad in [
        r#"{"keys":[{"kty":"RSA","kid":"o"}]}"#, // n 없음
        r#"{"keys":[{"kid":"o"}]}"#,             // kty 없음
        r#"{}"#,
        r#"[]"#,
        r#"{"keys":{}}"#,
        r#"{"keys":[1]}"#,
        r#"not json"#,
        r#"{"keys":[{"kty":"RSA","kid":5,"n":"AQAB","e":"AQAB"}]}"#,
    ] {
        assert!(
            matches!(Jwks::parse(bad), Err(JwksError::Parse(_))),
            "{bad}"
        );
    }
}

#[test]
fn 형식이_틀린_토큰은_거절한다() {
    let jwks = jwks_with(&[rfc_rsa_jwk(json!({}))]);
    for token in ["", "a.b", "a.b.c.d", "not-a-jwt", "a.b.c.d.e"] {
        assert!(rs256_verify(token, &jwks).is_err(), "{token:?}");
    }
}
