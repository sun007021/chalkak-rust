//! Rust가 발급한 토큰을 원본 Java 검증 코드가 받는지(그리고 Rust가 거절하는 토큰을 Java도 거절하는지) 교차 확인한다.
//!
//! 흐름:
//! 1. 이 테스트가 사례(토큰 + Rust 판정)를 만들어 tests/fixtures/rust-tokens.json과 같은지 확인한다. HS256과 ES256(RFC 6979)은
//!    결정적이라 토큰이 매번 같다. 다르면 `UPDATE_FIXTURES=1 cargo test -p chalkak-jwt --test cross_java`로 다시 쓴다.
//! 2. Java와 원본 classpath가 있으면 `fixtures/VerifyRustTokens.java`를 실행해 원본 판정이 같은지 확인한다.
//!    준비: `tests/fixtures/setup-oracle.sh` (환경 변수 CHALKAK_ORACLE_DIR, 기본 /tmp/chalkak-jwt-oracle).
//!    Java가 없으면 건너뛴다. 건너뛰지 않고 실패시키려면 CHALKAK_REQUIRE_JAVA=1.

mod common;

use chalkak_jwt::original::{
    AccessTokenClaims, SocialSignupClaims, access_token_header, apple_client_secret_claims,
    apple_client_secret_header, social_signup_header,
};
use chalkak_jwt::{
    Es256PublicKey, Validator, decode_hex, es256_sign, es256_verify, hs256_decode, hs256_encode,
};
use common::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Java 쪽에서 고정하는 현재 시각: 오라클 발급 시각 + 1초.
fn now_secs(oracle: &Value) -> i64 {
    oracle["now_epoch"].as_i64().unwrap() + 1
}

struct Case {
    name: &'static str,
    kind: &'static str,
    token: String,
    /// 사람이 의도한 판정. Rust 판정과 같은지 먼저 확인한다.
    intended: &'static str,
}

fn access_claims(oracle: &Value, sub: &str, scope: &str, iat: i64, exp: i64) -> Value {
    let a = &oracle["access"];
    AccessTokenClaims {
        issuer: s(&a["issuer"]),
        audience: s(&a["audience"]),
        subject: sub,
        issued_at: iat,
        expires_at: exp,
        jti: "00000000-0000-4000-8000-000000000001",
        scope,
        session_id: None,
    }
    .to_value()
}

fn signup_claims(oracle: &Value, sub: &str, email: Option<&str>, iat: i64, exp: i64) -> Value {
    let g = &oracle["signup"];
    SocialSignupClaims {
        issuer: s(&g["issuer"]),
        audience: s(&g["audience"]),
        subject: sub,
        issued_at: iat,
        expires_at: exp,
        jti: "00000000-0000-4000-8000-000000000002",
        provider: "KAKAO",
        upload_id: "01990000-0000-7000-8000-0000000000b1",
        email,
    }
    .to_value()
}

fn cases(oracle: &Value) -> Vec<Case> {
    let now = now_secs(oracle);
    let access_key = decode_hex(s(&oracle["access"]["secret_hex"])).unwrap();
    let signup_key = decode_hex(s(&oracle["signup"]["secret_hex"])).unwrap();
    let user = "01990000-0000-7000-8000-000000000001";
    let admin = "01990000-0000-7000-8000-0000000000a1";
    let sign_access =
        |claims: &Value, key: &[u8]| hs256_encode(&access_token_header(), claims, key);
    let sign_signup =
        |claims: &Value, key: &[u8]| hs256_encode(&social_signup_header(), claims, key);
    let tricky = s(&oracle["signup"]["tokens"][2]["sub"]);

    let mut out = vec![];
    let mut add = |name, kind, token, intended| {
        out.push(Case {
            name,
            kind,
            token,
            intended,
        })
    };

    add(
        "access: 회원",
        "access",
        sign_access(
            &access_claims(oracle, user, "USER", now, now + 900),
            &access_key,
        ),
        "accept",
    );
    add(
        "access: 관리자",
        "access",
        sign_access(
            &access_claims(oracle, admin, "ADMIN", now, now + 900),
            &access_key,
        ),
        "accept",
    );
    add(
        "access: 만료 29초 지남(허용 오차 안)",
        "access",
        sign_access(
            &access_claims(oracle, user, "USER", now - 1000, now - 29),
            &access_key,
        ),
        "accept",
    );
    add(
        "access: 만료 31초 지남",
        "access",
        sign_access(
            &access_claims(oracle, user, "USER", now - 1000, now - 31),
            &access_key,
        ),
        "reject",
    );
    add(
        "access: 하네스 만료 10초(iat=now, exp<iat)",
        "access",
        sign_access(
            &access_claims(oracle, user, "USER", now, now - 10),
            &access_key,
        ),
        "reject",
    );
    add(
        "access: exp==iat",
        "access",
        sign_access(&access_claims(oracle, user, "USER", now, now), &access_key),
        "reject",
    );
    add(
        "access: 다른 키로 서명",
        "access",
        sign_access(
            &access_claims(oracle, user, "USER", now, now + 900),
            &signup_key,
        ),
        "reject",
    );
    add(
        "access: sub가 UUID가 아님",
        "access",
        sign_access(
            &access_claims(oracle, "not-a-uuid", "USER", now, now + 900),
            &access_key,
        ),
        "reject",
    );
    add(
        "access: sub 1-2-3-4-5(Java UUID 관대함)",
        "access",
        sign_access(
            &access_claims(oracle, "1-2-3-4-5", "USER", now, now + 900),
            &access_key,
        ),
        "accept",
    );
    for (name, field, value) in [
        ("access: purpose 다름", "purpose", json!("OTHER")),
        (
            "access: audience 다름",
            "aud",
            json!("chalkak-social-signup"),
        ),
        ("access: issuer 다름", "iss", json!("other")),
    ] {
        let mut c = access_claims(oracle, user, "USER", now, now + 900);
        c[field] = value;
        add(name, "access", sign_access(&c, &access_key), "reject");
    }
    let mut no_aud = access_claims(oracle, user, "USER", now, now + 900);
    no_aud.as_object_mut().unwrap().remove("aud");
    add(
        "access: aud 없음(Java는 NullPointerException)",
        "access",
        sign_access(&no_aud, &access_key),
        "reject",
    );
    let mut aud_list = access_claims(oracle, user, "USER", now, now + 900);
    aud_list["aud"] = json!(["chalkak-access", "other"]);
    add(
        "access: aud 목록에 기대 값 포함",
        "access",
        sign_access(&aud_list, &access_key),
        "accept",
    );
    add(
        "access: 가입 토큰 클레임을 액세스 키로",
        "access",
        sign_access(
            &signup_claims(oracle, "s", None, now, now + 300),
            &access_key,
        ),
        "reject",
    );

    add(
        "signup: 이메일 있음",
        "signup",
        sign_signup(
            &signup_claims(
                oracle,
                "kakao-subject-12345",
                Some("user@example.com"),
                now,
                now + 300,
            ),
            &signup_key,
        ),
        "accept",
    );
    add(
        "signup: 이메일 없음",
        "signup",
        sign_signup(
            &signup_claims(oracle, "kakao-subject-12345", None, now, now + 300),
            &signup_key,
        ),
        "accept",
    );
    add(
        "signup: 특수 문자 subject·email",
        "signup",
        sign_signup(
            &signup_claims(
                oracle,
                tricky,
                Some("o'neil+tag&x=<y>@example.com"),
                now,
                now + 300,
            ),
            &signup_key,
        ),
        "accept",
    );
    add(
        "signup: 만료 31초 지남",
        "signup",
        sign_signup(
            &signup_claims(oracle, "s", None, now - 400, now - 31),
            &signup_key,
        ),
        "reject",
    );
    add(
        "signup: iat 미래 31초",
        "signup",
        sign_signup(
            &signup_claims(oracle, "s", None, now + 31, now + 331),
            &signup_key,
        ),
        "reject",
    );
    add(
        "signup: iat 미래 29초",
        "signup",
        sign_signup(
            &signup_claims(oracle, "s", None, now + 29, now + 329),
            &signup_key,
        ),
        "accept",
    );
    add(
        "signup: 액세스 클레임을 가입 키로",
        "signup",
        sign_signup(
            &access_claims(oracle, user, "USER", now, now + 900),
            &signup_key,
        ),
        "reject",
    );
    add(
        "signup: 하네스식 exp<iat",
        "signup",
        sign_signup(
            &signup_claims(oracle, "s", None, now, now - 10),
            &signup_key,
        ),
        "reject",
    );

    let es = &oracle["es256"];
    let secret = es256_sign(
        &apple_client_secret_header(s(&es["key_id"])),
        &apple_client_secret_claims(s(&es["team_id"]), s(&es["client_id"]), now),
        s(&es["private_key_pem"]),
    )
    .unwrap();
    // 서명 중간 글자를 바꾼다. (마지막 글자는 남는 비트라 바꿔도 같은 서명으로 읽힌다 — Java도 같다.)
    let mut tampered = secret.clone();
    let at = tampered.len() - 10;
    let replacement = if &tampered[at..=at] == "A" { "B" } else { "A" };
    tampered.replace_range(at..=at, replacement);
    add("es256: Apple client secret", "es256", secret, "accept");
    add("es256: 서명 변조", "es256", tampered, "reject");
    out
}

/// Rust 쪽 판정: 서명 검증 + 해당 토큰의 클레임 검증.
fn rust_verdict(oracle: &Value, case: &Case) -> &'static str {
    let now_ms = now_secs(oracle) * 1000;
    let ok = match case.kind {
        "access" => {
            let key = decode_hex(s(&oracle["access"]["secret_hex"])).unwrap();
            hs256_decode(&case.token, &key).is_ok_and(|c| {
                Validator::access_token(
                    s(&oracle["access"]["issuer"]),
                    s(&oracle["access"]["audience"]),
                )
                .validate(&c, now_ms)
                .is_ok()
            })
        }
        "signup" => {
            let key = decode_hex(s(&oracle["signup"]["secret_hex"])).unwrap();
            hs256_decode(&case.token, &key).is_ok_and(|c| {
                Validator::social_signup_token(
                    s(&oracle["signup"]["issuer"]),
                    s(&oracle["signup"]["audience"]),
                )
                .validate(&c, now_ms)
                .is_ok()
            })
        }
        "es256" => {
            let jwk = &oracle["es256"]["public_jwk"];
            let key = Es256PublicKey::from_coordinates(s(&jwk["x"]), s(&jwk["y"])).unwrap();
            es256_verify(&case.token, &key).is_ok()
        }
        other => panic!("{other}"),
    };
    if ok { "accept" } else { "reject" }
}

fn document(oracle: &Value) -> Value {
    let cases: Vec<Value> = cases(oracle)
        .iter()
        .map(|c| {
            json!({"name": c.name, "kind": c.kind, "token": c.token, "expect": rust_verdict(oracle, c)})
        })
        .collect();
    json!({
        "_comment": "Rust 발급 토큰과 Rust 판정. cross_java.rs가 만들고 VerifyRustTokens.java가 원본 판정과 비교한다.",
        "now_ms": now_secs(oracle) * 1000,
        "access_secret_hex": oracle["access"]["secret_hex"],
        "signup_secret_hex": oracle["signup"]["secret_hex"],
        "es256_public_jwk": oracle["es256"]["public_jwk"],
        "cases": cases,
    })
}

#[test]
fn rust_판정은_사람이_의도한_판정과_같다() {
    let oracle = oracle();
    for case in cases(&oracle) {
        assert_eq!(rust_verdict(&oracle, &case), case.intended, "{}", case.name);
    }
}

#[test]
fn 기록된_rust_토큰이_현재_구현의_출력과_같다() {
    let oracle = oracle();
    let fresh = document(&oracle);
    let path = fixture_path("rust-tokens.json");
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, serde_json::to_string_pretty(&fresh).unwrap() + "\n").unwrap();
    }
    let recorded: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        recorded, fresh,
        "rust-tokens.json이 오래됐다. UPDATE_FIXTURES=1 cargo test -p chalkak-jwt --test cross_java"
    );
}

#[test]
fn 원본_java_검증_코드가_rust_토큰을_같은_판정으로_처리한다() {
    let dir = PathBuf::from(
        std::env::var("CHALKAK_ORACLE_DIR").unwrap_or_else(|_| "/tmp/chalkak-jwt-oracle".into()),
    );
    let java = std::env::var("JAVA").unwrap_or_else(|_| "/opt/jdk/temurin-25/bin/java".into());
    if !dir.join("classes").is_dir() || !std::path::Path::new(&java).exists() {
        assert!(
            std::env::var_os("CHALKAK_REQUIRE_JAVA").is_none(),
            "Java 또는 원본 classpath({}) 없음",
            dir.display()
        );
        eprintln!(
            "건너뜀: Java 또는 원본 classpath({})가 없다. tests/fixtures/setup-oracle.sh 참고",
            dir.display()
        );
        return;
    }
    let output = std::process::Command::new(fixture_path("run-java.sh"))
        .arg(fixture_path("VerifyRustTokens.java"))
        .arg(fixture_path("rust-tokens.json"))
        .env("CHALKAK_ORACLE_DIR", &dir)
        .env("JAVA", &java)
        .output()
        .expect("run-java.sh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("{stdout}");
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("ALL MATCH"));
}
