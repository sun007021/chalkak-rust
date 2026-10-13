//! OIDC ID Token 디코더(공개키 목록 조회·캐시·재조회 제한과 클레임 검증)를 `OidcIdTokenVerifier`로 확인한다.
//!
//! 원본 `OidcIdTokenDecoderFactoryTest`, `OidcIdTokenAudienceValidatorTest`, `OidcIdTokenClaimsValidatorTest`를 옮겼다.
//! 원본은 `JwtDecoder.decode`가 `JwtException`을 던지는지 보지만, 여기서는 검증기가 같은 토큰을 401(`유효하지 않은
//! GOOGLE ID Token입니다.`)로 거절하는지 본다. 시각은 `FixedClock`이라 재조회 제한 구간(30초)이 흐르지 않는다.
//! 경고 로그 문구를 확인하는 원본 테스트(재조회 제한 로그, 이전 목록 사용 로그, `OidcJwkSetEventLoggerTest`)는
//! 옮기지 않았다: 러스트 JWKS 캐시는 이 로그를 남기지 않는다.

#[path = "../../platform/jwt/tests/common/mod.rs"]
mod jwt_common;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use chalkak_app::auth::domain::SocialProvider;
use chalkak_app::auth::oidc::{EmailPolicy, HttpJwksSource, IdTokenVerifier, OidcIdTokenVerifier};
use chalkak_app::config::OidcProvider;
use chalkak_core::FixedClock;
use chalkak_jwt::{JwksFetchError, JwksSource};
use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ISSUER: &str = "https://accounts.google.com";
const AUDIENCE: &str = "backend-client-id";
const KEY_ID: &str = "signing-key";
const RAW_NONCE: &str = "raw-nonce";
const NOW: &str = "2026-10-12T00:00:00Z";
const REJECTED: &str = "유효하지 않은 GOOGLE ID Token입니다.";

fn now_seconds() -> i64 {
    NOW.parse::<Timestamp>().unwrap().as_second()
}

fn nonce_hash() -> String {
    Sha256::digest(RAW_NONCE.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `validClaims()`. `edits`로 값을 바꾸거나(null이면 클레임을 뺀다) 더한다.
fn valid_claims(edits: &[(&str, Value)]) -> Value {
    let now = now_seconds();
    let mut claims = json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "sub": "provider-subject",
        "iat": now - 10,
        "exp": now + 300,
        "nonce": nonce_hash(),
    });
    for (name, value) in edits {
        if value.is_null() {
            claims.as_object_mut().unwrap().remove(*name);
        } else {
            claims[*name] = value.clone();
        }
    }
    claims
}

fn sign(kid: &str, claims: &Value) -> String {
    jwt_common::rs256_token(&json!({"alg": "RS256", "typ": "JWT", "kid": kid}), claims)
}

fn public_jwk(kid: &str) -> Value {
    jwt_common::rfc_rsa_jwk(json!({"kid": kid, "use": "sig", "alg": "RS256"}))
}

fn jwk_set(kids: &[&str]) -> String {
    let keys: Vec<Value> = kids.iter().map(|k| public_jwk(k)).collect();
    jwt_common::jwks_json(&keys)
}

/// `CountingResourceRetriever`.
struct CountingSource {
    body: Mutex<String>,
    count: AtomicUsize,
    failures: Mutex<VecDeque<()>>,
}

impl CountingSource {
    fn new(body: String) -> Arc<CountingSource> {
        Arc::new(CountingSource {
            body: Mutex::new(body),
            count: AtomicUsize::new(0),
            failures: Mutex::new(VecDeque::new()),
        })
    }

    fn fail_next(&self, times: usize) {
        *self.failures.lock().unwrap() = (0..times).map(|_| ()).collect();
    }

    fn change(&self, body: String) {
        *self.body.lock().unwrap() = body;
    }

    fn retrieval_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl JwksSource for CountingSource {
    fn fetch(&self) -> Result<String, JwksFetchError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.failures.lock().unwrap().pop_front().is_some() {
            return Err(JwksFetchError::new("temporary network failure"));
        }
        Ok(self.body.lock().unwrap().clone())
    }
}

fn settings() -> OidcProvider {
    OidcProvider {
        issuer: ISSUER.to_owned(),
        jwks_uri: "https://www.googleapis.com/oauth2/v3/certs".to_owned(),
        audience: AUDIENCE.to_owned(),
    }
}

fn verifier_with(source: Arc<dyn JwksSource>) -> OidcIdTokenVerifier {
    OidcIdTokenVerifier::with_source(
        SocialProvider::Google,
        &settings(),
        EmailPolicy::VerifiedOnly,
        source,
        Arc::new(FixedClock::new(NOW.parse().unwrap())),
    )
    .unwrap()
}

struct Fixture {
    source: Arc<CountingSource>,
    verifier: OidcIdTokenVerifier,
}

fn fixture() -> Fixture {
    let source = CountingSource::new(jwk_set(&[KEY_ID]));
    let verifier = verifier_with(source.clone());
    Fixture { source, verifier }
}

impl Fixture {
    /// `jwtDecoder.decode(token)`를 성공 기대로 호출하고 subject를 돌려준다.
    fn decode(&self, token: &str) -> String {
        self.verifier
            .verify(token, RAW_NONCE)
            .unwrap_or_else(|e| panic!("디코딩에 실패했다: {}", e.message()))
            .subject
    }

    fn assert_rejects(&self, token: &str) {
        let error = self.verifier.verify(token, RAW_NONCE).unwrap_err();
        assert_eq!((error.status(), error.message().as_str()), (401, REJECTED));
    }
}

// ---------------------------------------------------------------------------------------------
// OidcIdTokenDecoderFactoryTest
// ---------------------------------------------------------------------------------------------

// OidcIdTokenDecoderFactoryTest#createDecoder_validIdToken_decodes
#[test]
fn create_decoder_valid_id_token_decodes() {
    let f = fixture();

    assert_eq!(
        f.decode(&sign(KEY_ID, &valid_claims(&[]))),
        "provider-subject"
    );
}

// OidcIdTokenDecoderFactoryTest#createDecoder_unknownIssuer_rejects
#[test]
fn create_decoder_unknown_issuer_rejects() {
    let f = fixture();

    f.assert_rejects(&sign(
        KEY_ID,
        &valid_claims(&[("iss", json!("https://attacker.example.com"))]),
    ));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_unknownAudience_rejects
#[test]
fn create_decoder_unknown_audience_rejects() {
    let f = fixture();

    f.assert_rejects(&sign(
        KEY_ID,
        &valid_claims(&[("aud", json!("other-client-id"))]),
    ));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_missingAudience_rejects
#[test]
fn create_decoder_missing_audience_rejects() {
    let f = fixture();

    f.assert_rejects(&sign(KEY_ID, &valid_claims(&[("aud", Value::Null)])));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_expiredBeyondClockSkew_rejects
#[test]
fn create_decoder_expired_beyond_clock_skew_rejects() {
    let f = fixture();
    let now = now_seconds();

    f.assert_rejects(&sign(
        KEY_ID,
        &valid_claims(&[("iat", json!(now - 600)), ("exp", json!(now - 90))]),
    ));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_expiredWithinClockSkew_decodes
#[test]
fn create_decoder_expired_within_clock_skew_decodes() {
    let f = fixture();
    let now = now_seconds();

    let subject = f.decode(&sign(
        KEY_ID,
        &valid_claims(&[("iat", json!(now - 600)), ("exp", json!(now - 30))]),
    ));

    assert_eq!(subject, "provider-subject");
}

// OidcIdTokenDecoderFactoryTest#createDecoder_missingIssuedAt_rejects
#[test]
fn create_decoder_missing_issued_at_rejects() {
    let f = fixture();

    f.assert_rejects(&sign(KEY_ID, &valid_claims(&[("iat", Value::Null)])));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_signedWithOtherKey_rejects
#[test]
fn create_decoder_signed_with_other_key_rejects() {
    // 같은 kid로 공개키 목록에 오른 키가 서명 키와 다르다(모듈러스를 바꾼 다른 키).
    let mut other = public_jwk(KEY_ID);
    let n = other["n"].as_str().unwrap().to_owned();
    other["n"] = json!(format!("{}A", &n[..n.len() - 1]));
    let source = CountingSource::new(jwt_common::jwks_json(&[other]));
    let verifier = verifier_with(source);

    let error = verifier
        .verify(&sign(KEY_ID, &valid_claims(&[])), RAW_NONCE)
        .unwrap_err();

    assert_eq!((error.status(), error.message().as_str()), (401, REJECTED));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_hs256Signature_rejects
#[test]
fn create_decoder_hs256_signature_rejects() {
    let f = fixture();
    let token = chalkak_jwt::hs256_encode(
        &json!({"alg": "HS256", "typ": "JWT", "kid": KEY_ID}),
        &valid_claims(&[]),
        b"0123456789abcdef0123456789abcdef",
    );

    f.assert_rejects(&token);
}

// OidcIdTokenDecoderFactoryTest#createDecoder_unknownKeyId_rejects
#[test]
fn create_decoder_unknown_key_id_rejects() {
    let f = fixture();

    f.assert_rejects(&sign("unknown-key", &valid_claims(&[])));
}

// OidcIdTokenDecoderFactoryTest#createDecoder_repeatedKnownKeyId_fetchesJwkSetOnce
#[test]
fn create_decoder_repeated_known_key_id_fetches_jwk_set_once() {
    let f = fixture();
    let token = sign(KEY_ID, &valid_claims(&[]));

    f.decode(&token);
    f.decode(&token);
    f.decode(&token);

    assert_eq!(f.source.retrieval_count(), 1);
}

// OidcIdTokenDecoderFactoryTest#createDecoder_repeatedUnknownKeyId_limitsJwkSetRefetch
#[test]
fn create_decoder_repeated_unknown_key_id_limits_jwk_set_refetch() {
    let f = fixture();
    f.decode(&sign(KEY_ID, &valid_claims(&[])));
    let unknown = sign("unknown-key", &valid_claims(&[]));

    for _ in 0..3 {
        f.assert_rejects(&unknown);
    }

    assert_eq!(f.source.retrieval_count(), 2);
}

// OidcIdTokenDecoderFactoryTest#createDecoder_refetchLimitReached_decodesKnownKeyId
#[test]
fn create_decoder_refetch_limit_reached_decodes_known_key_id() {
    let f = fixture();
    let token = sign(KEY_ID, &valid_claims(&[]));
    f.decode(&token);
    let unknown = sign("unknown-key", &valid_claims(&[]));
    for _ in 0..2 {
        f.assert_rejects(&unknown);
    }

    let subject = f.decode(&token);

    assert_eq!(subject, "provider-subject");
    assert_eq!(f.source.retrieval_count(), 2);
}

// OidcIdTokenDecoderFactoryTest#createDecoder_rotatedKeyId_refetchesJwkSetAndDecodes
#[test]
fn create_decoder_rotated_key_id_refetches_jwk_set_and_decodes() {
    let f = fixture();
    f.decode(&sign(KEY_ID, &valid_claims(&[])));
    f.source.change(jwk_set(&[KEY_ID, "rotated-key"]));

    let subject = f.decode(&sign("rotated-key", &valid_claims(&[])));

    assert_eq!(subject, "provider-subject");
    assert_eq!(f.source.retrieval_count(), 2);
}

// OidcIdTokenDecoderFactoryTest#createDecoder_transientRetrievalFailure_retriesAndDecodes (재시도 로그는 확인하지 않는다)
#[test]
fn create_decoder_transient_retrieval_failure_retries_and_decodes() {
    let f = fixture();
    f.source.fail_next(1);

    let subject = f.decode(&sign(KEY_ID, &valid_claims(&[])));

    assert_eq!(subject, "provider-subject");
    assert_eq!(f.source.retrieval_count(), 2);
}

// OidcIdTokenDecoderFactoryTest#createDecoder_retrievalFailsBeforeFirstSuccess_rejectsUntilRefetchWindowEnds
#[test]
fn create_decoder_retrieval_fails_before_first_success_rejects_until_refetch_window_ends() {
    let f = fixture();
    let token = sign(KEY_ID, &valid_claims(&[]));
    f.source.fail_next(4);
    for _ in 0..2 {
        f.assert_rejects(&token);
    }

    f.assert_rejects(&token);

    assert_eq!(f.source.retrieval_count(), 4);
}

// OidcIdTokenDecoderFactoryTest#createJwkSource_malformedUri_throwsException
#[test]
fn create_jwk_source_malformed_uri_throws_exception() {
    let error = HttpJwksSource::new("not a uri")
        .err()
        .expect("만들 수 없다");

    assert!(
        error.contains("JWK Set URI 형식이 올바르지 않습니다"),
        "{error}"
    );
}

// OidcIdTokenDecoderFactoryTest#createDecoder_jwkSetUri_fetchesJwkSetOverHttp
#[test]
fn create_decoder_jwk_set_uri_fetches_jwk_set_over_http() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accept = Arc::new(Mutex::new(None::<String>));
    let seen = Arc::clone(&accept);
    let body = jwk_set(&[KEY_ID]);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        for line in request.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("accept:") {
                *seen.lock().unwrap() = Some(value.trim().to_owned());
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    let source = HttpJwksSource::new(&format!("http://127.0.0.1:{port}/certs")).unwrap();
    let verifier = verifier_with(Arc::new(source));

    let identity = verifier
        .verify(&sign(KEY_ID, &valid_claims(&[])), RAW_NONCE)
        .unwrap();
    server.join().unwrap();

    assert_eq!(identity.subject, "provider-subject");
    assert_eq!(
        accept.lock().unwrap().as_deref(),
        Some("application/json, application/jwk-set+json")
    );
}

// ---------------------------------------------------------------------------------------------
// OidcIdTokenAudienceValidatorTest
// ---------------------------------------------------------------------------------------------

// OidcIdTokenAudienceValidatorTest#validate_singleAllowedAudience_succeeds
#[test]
fn validate_single_allowed_audience_succeeds() {
    let f = fixture();

    assert_eq!(
        f.decode(&sign(KEY_ID, &valid_claims(&[("aud", json!([AUDIENCE]))]))),
        "provider-subject"
    );
}

// OidcIdTokenAudienceValidatorTest#validate_unknownAudience_fails
#[test]
fn validate_unknown_audience_fails() {
    let f = fixture();

    f.assert_rejects(&sign(
        KEY_ID,
        &valid_claims(&[("aud", json!(["unknown-client-id"]))]),
    ));
}

// OidcIdTokenAudienceValidatorTest#validate_multipleAudiencesIncludingAllowed_fails
#[test]
fn validate_multiple_audiences_including_allowed_fails() {
    let f = fixture();

    f.assert_rejects(&sign(
        KEY_ID,
        &valid_claims(&[("aud", json!([AUDIENCE, "other-client-id"]))]),
    ));
}

// OidcIdTokenAudienceValidatorTest#validate_missingAudience_fails
#[test]
fn validate_missing_audience_fails() {
    let f = fixture();

    f.assert_rejects(&sign(KEY_ID, &valid_claims(&[("aud", Value::Null)])));
}

// OidcIdTokenAudienceValidatorTest#validate_emptyAudience_fails
#[test]
fn validate_empty_audience_fails() {
    let f = fixture();

    f.assert_rejects(&sign(KEY_ID, &valid_claims(&[("aud", json!([]))])));
}

// OidcIdTokenAudienceValidatorTest#validate_unknownAudience_describesProvider
// 원본은 검증 실패 설명에 제공자 이름을 담는다. 러스트는 설명 대신 거절 메시지에 제공자 이름을 담는다.
#[test]
fn validate_unknown_audience_describes_provider() {
    let f = fixture();

    let error = f
        .verifier
        .verify(
            &sign(
                KEY_ID,
                &valid_claims(&[("aud", json!(["unknown-client-id"]))]),
            ),
            RAW_NONCE,
        )
        .unwrap_err();

    assert_eq!(error.message(), "유효하지 않은 GOOGLE ID Token입니다.");
}

// ---------------------------------------------------------------------------------------------
// OidcIdTokenClaimsValidatorTest
// ---------------------------------------------------------------------------------------------

// OidcIdTokenClaimsValidatorTest#validate_missingExpiration_fails
#[test]
fn validate_missing_expiration_fails() {
    let f = fixture();

    f.assert_rejects(&sign(KEY_ID, &valid_claims(&[("exp", Value::Null)])));
}

// OidcIdTokenClaimsValidatorTest#validate_missingIssuedAt_fails
#[test]
fn validate_missing_issued_at_fails() {
    let f = fixture();

    f.assert_rejects(&sign(KEY_ID, &valid_claims(&[("iat", Value::Null)])));
}

// OidcIdTokenClaimsValidatorTest#validate_issuedAtBeyondClockSkew_fails
#[test]
fn validate_issued_at_beyond_clock_skew_fails() {
    let f = fixture();
    let now = now_seconds();

    f.assert_rejects(&sign(
        KEY_ID,
        &valid_claims(&[("iat", json!(now + 61)), ("exp", json!(now + 3600))]),
    ));
}

// OidcIdTokenClaimsValidatorTest#validate_issuedAtAtClockSkew_succeeds
#[test]
fn validate_issued_at_at_clock_skew_succeeds() {
    let f = fixture();
    let now = now_seconds();

    let subject = f.decode(&sign(
        KEY_ID,
        &valid_claims(&[("iat", json!(now + 60)), ("exp", json!(now + 3600))]),
    ));

    assert_eq!(subject, "provider-subject");
}
