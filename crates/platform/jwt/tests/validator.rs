//! 클레임 검증기. 기대값은 원본 디코더를 직접 실행해 얻은 판정이다:
//!   tests/fixtures/ProbeAccessRules.java  → probe-access-rules.out  (JwtAccessTokenProvider, JwtSocialSignupTokenProvider)
//!   tests/fixtures/ProbeOidcRules.java    → probe-oidc-rules.out    (OidcIdTokenDecoderFactory)
//! 근거 클래스: Spring JwtTimestampValidator, JwtIssuerValidator, DelegatingOAuth2TokenValidator,
//!   OidcIdTokenAudienceValidator, OidcIdTokenClaimsValidator.

use chalkak_jwt::{ClaimError, Claims, Validator};
use serde_json::{Value, json};

/// 2026-10-12T00:00:00Z. 초 경계가 아닌 시각(+123ms)도 쓴다.
const NOW: i64 = 1_791_763_200;
const NOW_MS: i64 = NOW * 1000;
const SUB: &str = "01990000-0000-7000-8000-000000000001";

fn claims(v: Value) -> Claims {
    match v {
        Value::Object(m) => Claims::from_map(m).unwrap(),
        _ => panic!(),
    }
}

fn access() -> Value {
    json!({
        "iss": "chalkak-backend", "aud": "chalkak-access", "sub": SUB,
        "iat": NOW - 60, "exp": NOW + 840, "jti": "j", "purpose": "ACCESS", "scope": "USER"
    })
}

fn signup() -> Value {
    json!({
        "iss": "chalkak-backend", "aud": "chalkak-social-signup", "sub": "s",
        "iat": NOW - 60, "exp": NOW + 240, "jti": "j", "purpose": "SOCIAL_SIGNUP",
        "provider": "KAKAO", "uploadId": "01990000-0000-7000-8000-0000000000b1"
    })
}

fn oidc() -> Value {
    json!({"iss": "https://iss", "aud": "A", "sub": "s", "iat": NOW - 60, "exp": NOW + 300})
}

fn with(mut base: Value, edits: &[(&str, Value)]) -> Value {
    for (k, v) in edits {
        if v.is_null() {
            base.as_object_mut().unwrap().remove(*k);
        } else {
            base[*k] = v.clone();
        }
    }
    base
}

fn access_validator() -> Validator {
    Validator::access_token("chalkak-backend", "chalkak-access")
}

fn signup_validator() -> Validator {
    Validator::social_signup_token("chalkak-backend", "chalkak-social-signup")
}

fn oidc_validator() -> Validator {
    Validator::oidc_id_token("https://iss", "A")
}

// ---------------------------------------------------------------- 액세스 토큰

#[test]
fn 액세스_토큰_정상() {
    assert_eq!(
        access_validator().validate(&claims(access()), NOW_MS),
        Ok(())
    );
}

#[test]
fn 액세스_토큰_만료_허용_오차는_30초이고_경계는_초과일_때만_거절한다() {
    // JwtAccessTokenProvider.CLOCK_SKEW = 30s, JwtTimestampValidator: now - skew 가 exp 보다 뒤여야 만료.
    let v = access_validator();
    let at = |secs_ago: i64| {
        claims(with(
            access(),
            &[("exp", json!(NOW - secs_ago)), ("iat", json!(NOW - 900))],
        ))
    };
    assert_eq!(v.validate(&at(10), NOW_MS), Ok(())); // 만료 10초 지남 + iat가 충분히 앞 → 통과
    assert_eq!(v.validate(&at(29), NOW_MS), Ok(()));
    assert_eq!(v.validate(&at(30), NOW_MS), Ok(())); // 정확히 30초: 통과
    assert_eq!(v.validate(&at(30), NOW_MS + 1), Err(ClaimError::Expired)); // 1ms 넘으면 만료
    assert_eq!(v.validate(&at(31), NOW_MS), Err(ClaimError::Expired));
}

#[test]
fn 액세스_토큰_exp가_없으면_거절한다_set_allow_empty_expiry_claim_false() {
    let c = claims(with(access(), &[("exp", Value::Null)]));
    assert_eq!(
        access_validator().validate(&c, NOW_MS),
        Err(ClaimError::ExpiryRequired)
    );
}

#[test]
fn 액세스_토큰_iat는_없어도_미래여도_통과한다() {
    let v = access_validator();
    assert_eq!(
        v.validate(&claims(with(access(), &[("iat", Value::Null)])), NOW_MS),
        Ok(())
    );
    let future = with(
        access(),
        &[("iat", json!(NOW + 31)), ("exp", json!(NOW + 300))],
    );
    assert_eq!(v.validate(&claims(future), NOW_MS), Ok(()));
}

#[test]
fn 액세스_토큰_nbf는_30초_허용() {
    let v = access_validator();
    let at = |ahead: i64| claims(with(access(), &[("nbf", json!(NOW + ahead))]));
    assert_eq!(v.validate(&at(29), NOW_MS), Ok(()));
    assert_eq!(v.validate(&at(30), NOW_MS), Ok(()));
    assert_eq!(
        v.validate(&at(30), NOW_MS - 1),
        Err(ClaimError::NotYetValid)
    );
    assert_eq!(v.validate(&at(31), NOW_MS), Err(ClaimError::NotYetValid));
}

#[test]
fn 액세스_토큰_발급자와_audience_purpose를_확인한다() {
    let v = access_validator();
    let check = |edits: &[(&str, Value)]| v.validate(&claims(with(access(), edits)), NOW_MS);
    assert_eq!(
        check(&[("iss", Value::Null)]),
        Err(ClaimError::IssuerMismatch)
    );
    assert_eq!(
        check(&[("iss", json!("other"))]),
        Err(ClaimError::IssuerMismatch)
    );
    assert_eq!(
        check(&[("aud", json!("other"))]),
        Err(ClaimError::AudienceMismatch)
    );
    // contains 이므로 기대 값이 목록에 있으면 통과(OIDC와 다르다).
    assert_eq!(
        check(&[("aud", json!(["chalkak-access", "other"]))]),
        Ok(())
    );
    assert_eq!(
        check(&[("purpose", Value::Null)]),
        Err(ClaimError::ClaimMismatch("purpose".into()))
    );
    assert_eq!(
        check(&[("purpose", json!("OTHER"))]),
        Err(ClaimError::ClaimMismatch("purpose".into()))
    );
    assert_eq!(check(&[("jti", Value::Null)]), Ok(()));
}

#[test]
fn 액세스_토큰_aud가_없으면_원본은_nullpointerexception이다() {
    // JwtAccessTokenProvider#validateAudience: jwt.getAudience()가 null이면 contains 호출에서 NPE.
    // JwtException이 아니라서 401이 아니라 서블릿 오류(500)로 끝난다. 호출 측이 원본과 맞추려면 500으로 매핑한다.
    // 근거: 원본 java-a에 aud 클레임이 없는(또는 null) 토큰을 보내면 500이다.
    let v = access_validator();
    let c = claims(with(access(), &[("aud", Value::Null)]));
    assert_eq!(v.validate(&c, NOW_MS), Err(ClaimError::AudienceMissing));
    // 다른 검증이 이미 실패해도(만료) NPE가 우선한다: DelegatingOAuth2TokenValidator는 모두 실행하고 NPE는 전파된다.
    let c = claims(with(
        access(),
        &[
            ("aud", Value::Null),
            ("exp", json!(NOW - 100)),
            ("iat", json!(NOW - 900)),
        ],
    ));
    assert_eq!(v.validate(&c, NOW_MS), Err(ClaimError::AudienceMissing));
}

#[test]
fn 액세스_토큰_aud가_비었거나_문자열이_아니어도_원본은_401이다() {
    // 원본 java-a 확인: "aud": [] / "" / [1] / 5 는 모두 401(NPE가 아니라 audience 불일치 또는 디코드 실패).
    // [] 와 ""는 Spring이 목록으로 변환해 getAudience()가 null이 아니고 contains가 false를 돌려준다.
    // [1]·5 같은 문자열이 아닌 값은 Claims 생성 단계(디코드)에서 이미 거절된다.
    let v = access_validator();
    for aud in [json!([1]), json!(5), json!(true)] {
        let Value::Object(mut m) = access() else {
            unreachable!()
        };
        m.insert("aud".into(), aud.clone());
        assert!(Claims::from_map(m).is_err(), "aud = {aud}");
    }
    for aud in [json!([]), json!("")] {
        let c = claims(with(access(), &[("aud", aud.clone())]));
        assert_eq!(
            v.validate(&c, NOW_MS),
            Err(ClaimError::AudienceMismatch),
            "aud = {aud}"
        );
    }
}

#[test]
fn 액세스_토큰_subject는_자바_uuid_파서로_확인한다() {
    // JwtAccessTokenProvider#validateSubject: UUID.fromString 이 던지지 않아야 한다(대문자, "1-2-3-4-5"도 통과).
    let v = access_validator();
    let check = |sub: Value| v.validate(&claims(with(access(), &[("sub", sub)])), NOW_MS);
    assert_eq!(check(json!(SUB.to_uppercase())), Ok(()));
    assert_eq!(check(json!("1-2-3-4-5")), Ok(()));
    assert_eq!(check(json!("+1-+2-3-4-5")), Ok(()));
    assert_eq!(check(Value::Null), Err(ClaimError::SubjectInvalid));
    for bad in [
        "",
        " ",
        "not-a-uuid",
        "1-2-3-4",
        "1-2-3-4-5-6",
        "1-2-3-4-",
        "--1-2-3",
        "g-2-3-4-5",
        "-1-2-3-4-5",
        "+-2-3-4-5",
        "1-2-3-4-5 ",
        "0x1-2-3-4-5",
        // 36자 초과
        "01990000-0000-7000-8000-0000000000011",
        // 16자리 16진수(i64) 초과 성분
        "10000000000000000-2-3-4-5",
    ] {
        assert_eq!(
            check(json!(bad)),
            Err(ClaimError::SubjectInvalid),
            "sub {bad:?}"
        );
    }
    // i64 범위 안의 긴 성분은 통과
    assert_eq!(check(json!("7fffffffffffffff-2-3-4-5")), Ok(()));
    assert_eq!(check(json!("+7fffffffffffffff-2-3-4-5")), Ok(()));
    assert_eq!(check(json!("00000000000000007-2-3-4-5")), Ok(())); // 앞의 0은 자릿수에 세지 않는다
    assert_eq!(
        check(json!("8000000000000000-2-3-4-5")),
        Err(ClaimError::SubjectInvalid)
    );
}

// ---------------------------------------------------------------- 소셜 가입 토큰

#[test]
fn 소셜_가입_토큰_정상과_허용_오차_30초() {
    let v = signup_validator();
    assert_eq!(v.validate(&claims(signup()), NOW_MS), Ok(()));
    let at = |secs_ago: i64| {
        claims(with(
            signup(),
            &[("exp", json!(NOW - secs_ago)), ("iat", json!(NOW - 900))],
        ))
    };
    assert_eq!(v.validate(&at(29), NOW_MS), Ok(()));
    assert_eq!(v.validate(&at(31), NOW_MS), Err(ClaimError::Expired));
}

#[test]
fn 소셜_가입_토큰은_iat와_exp가_모두_있어야_하고_iat는_30초까지만_미래다() {
    // JwtSocialSignupTokenProvider#validateRequiredTimestamps
    let v = signup_validator();
    let check = |edits: &[(&str, Value)]| v.validate(&claims(with(signup(), edits)), NOW_MS);
    assert!(matches!(
        check(&[("exp", Value::Null)]),
        Err(ClaimError::TimestampsInvalid(_))
    ));
    assert!(matches!(
        check(&[("iat", Value::Null)]),
        Err(ClaimError::TimestampsInvalid(_))
    ));
    assert_eq!(
        check(&[("iat", json!(NOW + 29)), ("exp", json!(NOW + 300))]),
        Ok(())
    );
    assert_eq!(
        check(&[("iat", json!(NOW + 30)), ("exp", json!(NOW + 300))]),
        Ok(())
    );
    assert!(matches!(
        check(&[("iat", json!(NOW + 31)), ("exp", json!(NOW + 300))]),
        Err(ClaimError::TimestampsInvalid(_))
    ));
}

#[test]
fn 소셜_가입_토큰_발급자_audience_purpose() {
    let v = signup_validator();
    let check = |edits: &[(&str, Value)]| v.validate(&claims(with(signup(), edits)), NOW_MS);
    assert_eq!(
        check(&[("iss", json!("other"))]),
        Err(ClaimError::IssuerMismatch)
    );
    assert_eq!(
        check(&[("aud", json!("other"))]),
        Err(ClaimError::AudienceMismatch)
    );
    assert_eq!(
        check(&[("aud", Value::Null)]),
        Err(ClaimError::AudienceMissing)
    );
    assert_eq!(
        check(&[("aud", json!(["chalkak-social-signup", "x"]))]),
        Ok(())
    );
    assert_eq!(
        check(&[("purpose", json!("ACCESS"))]),
        Err(ClaimError::ClaimMismatch("purpose".into()))
    );
    // sub, jti, provider, uploadId 는 디코더가 아니라 verify() 안에서 확인한다.
    for missing in ["sub", "jti", "provider", "uploadId"] {
        assert_eq!(check(&[(missing, Value::Null)]), Ok(()), "{missing}");
    }
}

#[test]
fn 액세스_토큰과_가입_토큰은_서로_통과하지_못한다() {
    // 비밀키를 같은 값으로 잘못 설정해도 audience·purpose로 분리된다(JwtAccessTokenProvider#validateAudience 주석).
    assert!(
        access_validator()
            .validate(&claims(signup()), NOW_MS)
            .is_err()
    );
    assert!(
        signup_validator()
            .validate(&claims(access()), NOW_MS)
            .is_err()
    );
}

// ---------------------------------------------------------------- OIDC ID 토큰

#[test]
fn oidc_정상() {
    assert_eq!(oidc_validator().validate(&claims(oidc()), NOW_MS), Ok(()));
}

#[test]
fn oidc_만료_허용_오차는_60초_스프링_기본값() {
    // JwtValidators.createDefaultWithIssuer → JwtTimestampValidator() 기본 60초.
    let v = oidc_validator();
    let at = |secs_ago: i64| {
        claims(with(
            oidc(),
            &[("exp", json!(NOW - secs_ago)), ("iat", json!(NOW - 900))],
        ))
    };
    assert_eq!(v.validate(&at(59), NOW_MS), Ok(()));
    assert_eq!(v.validate(&at(60), NOW_MS), Ok(()));
    assert_eq!(v.validate(&at(60), NOW_MS + 1), Err(ClaimError::Expired));
    assert_eq!(v.validate(&at(61), NOW_MS), Err(ClaimError::Expired));
}

#[test]
fn oidc_nbf_60초_iat_미래_60초() {
    let v = oidc_validator();
    let nbf = |ahead: i64| claims(with(oidc(), &[("nbf", json!(NOW + ahead))]));
    assert_eq!(v.validate(&nbf(59), NOW_MS), Ok(()));
    assert_eq!(v.validate(&nbf(61), NOW_MS), Err(ClaimError::NotYetValid));
    let iat = |ahead: i64| {
        claims(with(
            oidc(),
            &[
                ("iat", json!(NOW + ahead)),
                ("exp", json!(NOW + ahead + 300)),
            ],
        ))
    };
    assert_eq!(v.validate(&iat(59), NOW_MS), Ok(()));
    assert_eq!(v.validate(&iat(60), NOW_MS), Ok(()));
    assert!(matches!(
        v.validate(&iat(61), NOW_MS),
        Err(ClaimError::TimestampsInvalid(_))
    ));
}

#[test]
fn oidc_exp와_iat는_필수다_oidc_id_token_claims_validator() {
    let v = oidc_validator();
    let check = |edits: &[(&str, Value)]| v.validate(&claims(with(oidc(), edits)), NOW_MS);
    assert!(matches!(
        check(&[("exp", Value::Null)]),
        Err(ClaimError::TimestampsInvalid(_))
    ));
    assert!(matches!(
        check(&[("iat", Value::Null)]),
        Err(ClaimError::TimestampsInvalid(_))
    ));
}

#[test]
fn oidc_audience는_허용한_값_하나만_들어_있어야_한다_oidc_id_token_audience_validator() {
    let v = oidc_validator();
    let check = |edits: &[(&str, Value)]| v.validate(&claims(with(oidc(), edits)), NOW_MS);
    assert_eq!(check(&[("aud", json!("A"))]), Ok(()));
    assert_eq!(check(&[("aud", json!(["A"]))]), Ok(()));
    assert_eq!(
        check(&[("aud", json!(["A", "B"]))]),
        Err(ClaimError::AudienceMismatch)
    );
    assert_eq!(
        check(&[("aud", json!(["A", "A"]))]),
        Err(ClaimError::AudienceMismatch)
    );
    assert_eq!(
        check(&[("aud", json!("B"))]),
        Err(ClaimError::AudienceMismatch)
    );
    // aud가 없어도 NPE가 아니라 일반 실패(401)다.
    assert_eq!(
        check(&[("aud", Value::Null)]),
        Err(ClaimError::AudienceMismatch)
    );
    assert_eq!(
        check(&[("aud", json!([]))]),
        Err(ClaimError::AudienceMismatch)
    );
}

#[test]
fn oidc_발급자() {
    let v = oidc_validator();
    let check = |edits: &[(&str, Value)]| v.validate(&claims(with(oidc(), edits)), NOW_MS);
    assert_eq!(
        check(&[("iss", json!("https://other"))]),
        Err(ClaimError::IssuerMismatch)
    );
    assert_eq!(
        check(&[("iss", Value::Null)]),
        Err(ClaimError::IssuerMismatch)
    );
}

// ---------------------------------------------------------------- 만료 10초 401 원인

#[test]
fn 하네스_만료_10초_지남_토큰은_exp가_iat보다_앞서_디코드에서_거절된다() {
    // 하네스 시나리오(harness/scenarios/platform/auth-matrix.toml): iat=now, exp=now-10.
    // 허용 오차 30초 때문에 통과해야 할 것 같지만 원본은 401이다. 원인은 Validator가 아니라 그 앞단:
    // Spring 'Jwt' 생성자가 expiresAt은 issuedAt보다 뒤여야 한다고 단언(BadJwtException)한다.
    // 따라서 Claims::from_map 단계에서 거절한다. (ProbeAccessRules: "exp 10s ago, iat now (exp<iat)")
    let Value::Object(map) = with(access(), &[("iat", json!(NOW)), ("exp", json!(NOW - 10))])
    else {
        panic!()
    };
    assert_eq!(
        Claims::from_map(map),
        Err(chalkak_jwt::JwtError::InvalidClaims(
            "expiresAt must be after issuedAt".into()
        ))
    );
    // iat가 충분히 앞서 있으면(오래전에 발급된 토큰) 같은 exp라도 허용 오차 안이라 통과한다.
    let old = claims(with(
        access(),
        &[("iat", json!(NOW - 900)), ("exp", json!(NOW - 10))],
    ));
    assert_eq!(access_validator().validate(&old, NOW_MS), Ok(()));
}

// ---------------------------------------------------------------- 원본이 발급한 토큰으로 처음부터 끝까지

#[test]
fn 원본이_발급한_토큰은_hs256_디코드와_검증기를_통과한다() {
    use chalkak_jwt::{decode_hex, hs256_decode};
    let oracle: Value = serde_json::from_str(include_str!("fixtures/oracle.json")).unwrap();
    let issued_ms = oracle["now_epoch"].as_i64().unwrap() * 1000;

    let access_key = decode_hex(oracle["access"]["secret_hex"].as_str().unwrap()).unwrap();
    for t in oracle["access"]["tokens"].as_array().unwrap() {
        let c = hs256_decode(t["token"].as_str().unwrap(), &access_key).unwrap();
        let v = access_validator();
        assert_eq!(v.validate(&c, issued_ms + 1_000), Ok(()));
        // 15분 + 30초가 지나면 만료
        assert_eq!(v.validate(&c, issued_ms + 900_000 + 30_000), Ok(()));
        assert_eq!(
            v.validate(&c, issued_ms + 900_000 + 30_001),
            Err(ClaimError::Expired)
        );
        // 가입 토큰용 키로는 풀리지 않는다.
        assert!(
            hs256_decode(
                t["token"].as_str().unwrap(),
                b"0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
    }

    let signup_key = decode_hex(oracle["signup"]["secret_hex"].as_str().unwrap()).unwrap();
    for t in oracle["signup"]["tokens"].as_array().unwrap() {
        let c = hs256_decode(t["token"].as_str().unwrap(), &signup_key).unwrap();
        let v = signup_validator();
        assert_eq!(v.validate(&c, issued_ms + 1_000), Ok(()));
        assert_eq!(
            v.validate(&c, issued_ms + 300_000 + 30_001),
            Err(ClaimError::Expired)
        );
        // 액세스 검증기는 가입 토큰을 거절한다.
        assert!(access_validator().validate(&c, issued_ms + 1_000).is_err());
    }
}
