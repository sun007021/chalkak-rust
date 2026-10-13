//! JWKS 캐시. 근거: 원본 OidcIdTokenDecoderFactory#createJwkSource가 조립하는 Nimbus JWKSourceBuilder 체인
//!   URL 조회 ← RetryingJWKSetSource ← OutageTolerantJWKSetSource(50분) ← RateLimitedJWKSetSource(30초) ← CachingJWKSetSource(5분)
//! 와 JWKSetBasedJWKSource(모르는 kid면 한 번 다시 가져옴). 체인 순서는 JWKSourceBuilder#build 바이트코드에서,
//! 속도 제한 규칙은 RateLimitedJWKSetSource 바이트코드에서(구간이 처음 조회 시각에 열리고, 구간마다 2회) 확인했다.
//! 시각을 주입하는 가짜 시계와 호출 횟수를 세는 가짜 조회기로 시나리오를 재현한다.

mod common;

use chalkak_jwt::{
    Clock, JwksCache, JwksCacheConfig, JwksError, JwksEvents, JwksFetchError, JwksSource, JwtError,
};
use common::*;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const T0: i64 = 1_791_763_200_000;
const SEC: i64 = 1000;
const MIN: i64 = 60 * SEC;

struct FakeClock(AtomicI64);

impl FakeClock {
    fn new() -> Arc<FakeClock> {
        Arc::new(FakeClock(AtomicI64::new(T0)))
    }
    fn set(&self, offset_ms: i64) {
        self.0.store(T0 + offset_ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct Script {
    calls: AtomicUsize,
    queue: Mutex<VecDeque<Result<String, JwksFetchError>>>,
    body: Mutex<String>,
}

impl Script {
    fn new(body: String) -> Arc<Script> {
        let s = Script::default();
        *s.body.lock().unwrap() = body;
        Arc::new(s)
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn set_body(&self, body: String) {
        *self.body.lock().unwrap() = body;
    }
    /// 다음 조회들의 결과를 미리 정한다(소진되면 `body`를 돌려준다).
    fn push(&self, r: Result<String, JwksFetchError>) {
        self.queue.lock().unwrap().push_back(r);
    }
    fn fail(&self, times: usize) {
        for _ in 0..times {
            self.push(Err(JwksFetchError::new("connection refused")));
        }
    }
}

impl JwksSource for Script {
    fn fetch(&self) -> Result<String, JwksFetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(r) = self.queue.lock().unwrap().pop_front() {
            return r;
        }
        Ok(self.body.lock().unwrap().clone())
    }
}

/// 같은 RFC 키를 kid만 바꿔 담은 목록.
fn jwks(kids: &[&str]) -> String {
    let keys: Vec<Value> = kids
        .iter()
        .map(|k| rfc_rsa_jwk(json!({"kid": k, "use": "sig", "alg": "RS256"})))
        .collect();
    jwks_json(&keys)
}

fn token(kid: &str) -> String {
    rs256_token(
        &json!({"alg": "RS256", "typ": "JWT", "kid": kid}),
        &json!({"iss": "i", "aud": "a", "sub": "s"}),
    )
}

fn cache(script: &Arc<Script>, clock: &Arc<FakeClock>) -> JwksCache {
    JwksCache::with_config(script.clone(), JwksCacheConfig::default(), clock.clone()).unwrap()
}

#[test]
fn 기본_설정은_원본_값이다() {
    // 캐시 5분, 속도 제한 30초 구간 2회, 장애 시 마지막 목록 50분, 크기 제한 50KiB(51200).
    let c = JwksCacheConfig::default();
    assert_eq!(c.cache_ttl, Duration::from_secs(300));
    assert_eq!(c.rate_limit_interval, Duration::from_secs(30));
    assert_eq!(c.rate_limit_requests, 2);
    assert_eq!(c.outage_ttl, Duration::from_secs(50 * 60));
    assert_eq!(c.max_body_bytes, 51_200);
    assert_eq!(
        chalkak_jwt::JWKS_CONNECT_TIMEOUT,
        Duration::from_millis(500)
    );
    assert_eq!(chalkak_jwt::JWKS_READ_TIMEOUT, Duration::from_millis(500));
    assert_eq!(
        chalkak_jwt::JWKS_ACCEPT,
        "application/json, application/jwk-set+json"
    );
}

#[test]
fn 설정이_모순이면_만들_수_없다() {
    // JWKSourceBuilder#build: 캐시 수명은 속도 제한 간격보다 길어야 한다(IllegalStateException).
    let script = Script::new(jwks(&["k1"]));
    let config = JwksCacheConfig {
        cache_ttl: Duration::from_secs(30),
        ..JwksCacheConfig::default()
    };
    assert!(JwksCache::with_config(script, config, FakeClock::new()).is_err());
}

#[test]
fn 처음_한_번만_가져오고_5분_동안은_캐시를_쓴다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 1);
    clock.set(4 * MIN + 59 * SEC + 999);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 1);
    // 정확히 5분이 되면 만료(CachedObject.isValid: now < 만료 시각)
    clock.set(5 * MIN);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 2);
}

#[test]
fn 모르는_kid면_한_번_다시_가져와서_새_키로_검증한다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    script.set_body(jwks(&["k1", "k2"])); // 키 회전
    clock.set(SEC);
    assert!(cache.verify(&token("k2")).is_ok());
    assert_eq!(script.calls(), 2);
    // 이후 k2는 캐시에서
    assert!(cache.verify(&token("k2")).is_ok());
    assert_eq!(script.calls(), 2);
}

#[test]
fn 다시_가져와도_없는_kid는_거절한다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert_eq!(cache.verify(&token("zz")), Err(JwtError::NoMatchingKey));
    assert_eq!(script.calls(), 2); // 처음 조회 + 모르는 kid 재조회
}

#[test]
fn kid가_목록에_있으면_조건이_안_맞아도_다시_가져오지_않는다() {
    // JWKSetBasedJWKSource: kid로 getKeyByKeyId가 null이 아니면 빈 목록을 돌려준다.
    let (script, clock) = (
        Script::new(jwks_json(&[rfc_rsa_jwk(
            json!({"kid": "k1", "use": "enc"}),
        )])),
        FakeClock::new(),
    );
    let cache = cache(&script, &clock);
    assert_eq!(cache.verify(&token("k1")), Err(JwtError::NoMatchingKey));
    assert_eq!(script.calls(), 1);
}

#[test]
fn kid가_없는_토큰은_키가_없어도_다시_가져오지_않는다() {
    let (script, clock) = (Script::new(jwks_json(&[])), FakeClock::new());
    let cache = cache(&script, &clock);
    let t = rs256_token(&json!({"alg": "RS256"}), &json!({"sub": "s"}));
    assert_eq!(cache.verify(&t), Err(JwtError::NoMatchingKey));
    assert_eq!(script.calls(), 1);
}

#[test]
fn 모르는_kid_조회는_30초_구간마다_최대_2회로_제한한다() {
    // 구간은 첫 조회 시각에 열린다(처음 조회가 1회, 모르는 kid 재조회가 2회). 3번째는 조회 없이 거절.
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok()); // 조회 1 (구간 열림)
    clock.set(SEC);
    assert_eq!(cache.verify(&token("x1")), Err(JwtError::NoMatchingKey)); // 조회 2
    assert_eq!(script.calls(), 2);
    clock.set(2 * SEC);
    assert_eq!(
        cache.verify(&token("x2")),
        Err(JwtError::Jwks(JwksError::RateLimited))
    );
    assert_eq!(script.calls(), 2); // 제공자에 요청하지 않았다
    // 캐시에 있는 kid는 제한과 상관없이 통과
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 2);
    // 구간이 끝나면(첫 조회 + 30초) 다시 허용
    clock.set(30 * SEC - 1);
    assert_eq!(
        cache.verify(&token("x3")),
        Err(JwtError::Jwks(JwksError::RateLimited))
    );
    clock.set(30 * SEC);
    assert_eq!(cache.verify(&token("x3")), Err(JwtError::NoMatchingKey));
    assert_eq!(script.calls(), 3);
}

#[test]
fn 조회_한_번_안에서_통신_실패를_한_번_더_시도한다() {
    // RetryingJWKSetSource: 한 번 재시도. 재시도는 속도 제한 횟수를 따로 쓰지 않는다.
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    script.fail(1);
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 2);
    // 구간에서 1회만 쓴 것이므로 모르는 kid 재조회가 아직 가능
    clock.set(SEC);
    assert_eq!(cache.verify(&token("x1")), Err(JwtError::NoMatchingKey));
    assert_eq!(script.calls(), 3);
}

#[test]
fn 두_번_다_실패하면_오류이고_본문이_json이_아니어도_같다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    script.fail(2);
    let cache = cache(&script, &clock);
    assert_eq!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::Fetch(
            "connection refused".into()
        )))
    );
    assert_eq!(script.calls(), 2);

    // JWKSetParseException도 JWKSetUnavailableException의 하위라 재시도 대상이다.
    let (script, clock) = (Script::new("not json".into()), FakeClock::new());
    let cache = self::cache(&script, &clock);
    assert!(matches!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::Parse(_)))
    ));
    assert_eq!(script.calls(), 2);
}

#[test]
fn 크기_제한을_넘는_본문은_조회_실패로_본다() {
    let big = format!("{{\"keys\":[],\"pad\":\"{}\"}}", "a".repeat(51_200));
    let (script, clock) = (Script::new(big), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(matches!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::Fetch(_)))
    ));
    assert_eq!(script.calls(), 2);
}

#[test]
fn 한_번도_받지_못한_기동_직후에는_실패한_조회도_횟수에_들어가_남은_구간_동안_거절된다() {
    // OidcIdTokenDecoderFactory 주석: "한 번도 목록을 받지 못한 기동 직후에는 쓸 목록이 없어 남은 구간 동안 거절될 수 있다."
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    script.fail(4); // 요청 2번 × (시도 + 재시도)
    let cache = cache(&script, &clock);
    assert!(matches!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::Fetch(_)))
    ));
    clock.set(SEC);
    assert!(matches!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::Fetch(_)))
    ));
    assert_eq!(script.calls(), 4);
    // 제공자가 회복돼도 구간이 끝날 때까지는 조회하지 않는다.
    clock.set(2 * SEC);
    assert_eq!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::RateLimited))
    );
    assert_eq!(script.calls(), 4);
    clock.set(30 * SEC);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 5);
}

#[test]
fn 캐시가_만료된_뒤_조회가_실패하면_마지막_목록을_최대_50분_계속_쓴다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok()); // t=0 성공
    assert_eq!(script.calls(), 1);

    // 5분 뒤 만료 → 조회 실패(재시도 포함 2회) → 이전 목록으로 통과
    script.fail(2);
    clock.set(5 * MIN);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 3);
    // 이전 목록이 캐시를 다시 채웠으므로 5분 동안은 조회하지 않는다.
    clock.set(9 * MIN);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 3);

    // 다시 만료, 계속 실패, 마지막 성공(t=0) 뒤 50분이 되기 전까지는 통과
    script.fail(2);
    clock.set(10 * MIN);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(script.calls(), 5);
    script.fail(2 * 20);
    for minutes in [15, 20, 25, 30, 35, 40, 45] {
        clock.set(minutes * MIN);
        assert!(cache.verify(&token("k1")).is_ok(), "{minutes}분");
    }
    // 50분: 마지막 성공(t=0)으로부터 50분이 지나 더는 쓰지 않는다.
    clock.set(50 * MIN);
    assert!(matches!(
        cache.verify(&token("k1")),
        Err(JwtError::Jwks(JwksError::Fetch(_)))
    ));
}

#[test]
fn 성공하면_50분_기준이_그때부터_다시_시작한다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok()); // t=0
    clock.set(5 * MIN);
    assert!(cache.verify(&token("k1")).is_ok()); // t=5분 성공(조회 2)
    script.fail(1000);
    clock.set(54 * MIN + 59 * SEC);
    assert!(cache.verify(&token("k1")).is_ok()); // 마지막 성공(5분)으로부터 50분 미만
    // 이전 목록이 캐시를 54분 59초에 다시 채웠으므로 59분 59초에 다시 만료된다. 이미 55분이 지나 더는 쓰지 않는다.
    clock.set(59 * MIN + 59 * SEC);
    assert!(cache.verify(&token("k1")).is_err());
}

#[test]
fn 장애_중_모르는_kid는_이전_목록에서_못_찾으면_거절한다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    script.fail(2);
    clock.set(SEC);
    // 재조회가 실패하면 이전 목록(복사본)이 돌아오지만 k2는 없다.
    assert_eq!(cache.verify(&token("k2")), Err(JwtError::NoMatchingKey));
    assert!(cache.verify(&token("k1")).is_ok());
}

#[test]
fn 복구되면_새_목록으로_바뀐다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = cache(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    script.set_body(jwks(&["k3"]));
    clock.set(5 * MIN);
    assert!(cache.verify(&token("k3")).is_ok());
    // k1은 새 목록에 없다(모르는 kid 재조회는 구간 한도 안에서 가능)
    assert_eq!(cache.verify(&token("k1")), Err(JwtError::NoMatchingKey));
}

#[test]
fn 여러_스레드가_동시에_와도_조회는_한_번이다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let cache = Arc::new(cache(&script, &clock));
    let t = token("k1");
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (cache, t) = (cache.clone(), t.clone());
            std::thread::spawn(move || cache.verify(&t).is_ok())
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap());
    }
    assert_eq!(script.calls(), 1);
}

/// 받은 알림을 문자열로 모아 둔다.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl JwksEvents for Recorder {
    fn retrial(&self, cause: &str) {
        self.0.lock().unwrap().push(format!("retrial:{cause}"));
    }
    fn outage(&self, remaining_ms: i64, cause: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("outage:{remaining_ms}:{cause}"));
    }
    fn rate_limited(&self) {
        self.0.lock().unwrap().push("rate_limited".into());
    }
}

fn observed(script: &Arc<Script>, clock: &Arc<FakeClock>) -> (JwksCache, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let mut cache = cache(script, clock);
    cache.set_events(recorder.clone());
    (cache, recorder)
}

// OidcIdTokenDecoderFactory#createJwkSource: RetryingJWKSetSource의 재시도 알림(OidcJwkSetEventLogger#logRetrial)
#[test]
fn 첫_조회가_실패해_다시_시도하면_재시도_알림을_보낸다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    script.fail(1);
    let (cache, recorder) = observed(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    assert_eq!(
        recorder.events(),
        ["retrial:Couldn't retrieve JWK set: connection refused"]
    );
}

// OidcIdTokenDecoderFactory#createJwkSource: OutageTolerantJWKSetSource의 이전 목록 사용 알림(#logOutage)
#[test]
fn 이전_목록을_쓰면_남은_시간과_함께_장애_알림을_보낸다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let (cache, recorder) = observed(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    script.fail(2);
    clock.set(5 * MIN);
    assert!(cache.verify(&token("k1")).is_ok());
    // 재시도 알림 한 번, 이어서 마지막 성공(t=0)에서 50분 중 45분이 남았다는 알림.
    assert_eq!(
        recorder.events(),
        [
            "retrial:Couldn't retrieve JWK set: connection refused".to_owned(),
            format!(
                "outage:{}:Couldn't retrieve JWK set: connection refused",
                45 * MIN
            )
        ]
    );
}

// OidcIdTokenDecoderFactory#createJwkSource: RateLimitedJWKSetSource의 제한 알림(#logRateLimited)
#[test]
fn 재조회_제한에_걸릴_때마다_제한_알림을_보낸다() {
    let (script, clock) = (Script::new(jwks(&["k1"])), FakeClock::new());
    let (cache, recorder) = observed(&script, &clock);
    assert!(cache.verify(&token("k1")).is_ok());
    clock.set(SEC);
    assert_eq!(cache.verify(&token("x1")), Err(JwtError::NoMatchingKey));
    assert!(recorder.events().is_empty());
    clock.set(2 * SEC);
    assert!(cache.verify(&token("x2")).is_err());
    assert!(cache.verify(&token("x3")).is_err());
    assert_eq!(recorder.events(), ["rate_limited", "rate_limited"]);
}
