//! JWKS 캐시: 원본 `OidcIdTokenDecoderFactory#createJwkSource`가 조립하는 Nimbus 체인을 그대로 옮긴다.
//!
//! ```text
//! 모르는 kid 처리(JWKSetBasedJWKSource) → 캐시 5분(CachingJWKSetSource)
//!     → 속도 제한 30초 구간 2회(RateLimitedJWKSetSource)
//!     → 장애 시 마지막 목록 50분(OutageTolerantJWKSetSource)
//!     → 통신 실패 한 번 재시도(RetryingJWKSetSource) → URL 조회
//! ```
//! 순서는 `JWKSourceBuilder#build`에서, 세부 규칙은 각 클래스 바이트코드에서 확인했다.
//!
//! - 캐시는 `조회 시각 + 5분`까지 유효하다(`now < 만료`). 만료되면 다시 가져온다. 캐시 선갱신은 쓰지 않는다.
//! - 토큰의 kid가 캐시한 목록에 없고(어떤 종류의 키로도 없을 때만) 조건에 맞는 키도 없으면 목록을 한 번 다시 가져온다.
//!   토큰에 kid가 없으면 다시 가져오지 않는다.
//! - 조회(만료 갱신과 재조회 모두, 실패한 조회 포함)는 첫 조회 시각에 열리는 30초 구간마다 2회까지만 한다.
//!   넘치면 제공자에 요청하지 않고 [`JwksError::RateLimited`]로 끝낸다. 캐시에 있는 kid는 영향이 없다.
//! - 한 번의 조회에서 통신 실패나 본문 해석 실패가 나면 한 번 더 시도한다(재시도는 횟수를 따로 쓰지 않는다).
//! - 그래도 실패하면 마지막으로 성공한 목록을 그 성공 시각부터 50분까지 쓴다. 이때 돌려주는 것은 같은 키의
//!   복사본이라 캐시도 그 시각부터 다시 5분 채워진다. 아직 한 번도 성공한 적이 없으면 오류다.
//!
//! 한 번에 한 스레드만 조회한다(뮤텍스). 기다린 스레드는 먼저 들어간 스레드가 채운 캐시를 그대로 쓴다
//! (`ReferenceComparisonRefreshJWKSetEvaluator`와 같은 효과). Nimbus의 15초 대기 제한은 두지 않았다.
//! 조회는 [`JwksSource`]의 연결·읽기 타임아웃(각 0.5초)이 끊어 주므로 최대 대기는 그 두 배(재시도 포함)다.

use crate::jwks::{rs256_check_candidates, rs256_prepare};
use crate::{Claims, Jwks, JwksError, JwtError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `JWKSourceBuilder.DEFAULT_HTTP_CONNECT_TIMEOUT`(0.5초). [`JwksSource`] 구현이 지켜야 한다.
pub const JWKS_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// `JWKSourceBuilder.DEFAULT_HTTP_READ_TIMEOUT`(0.5초).
pub const JWKS_READ_TIMEOUT: Duration = Duration::from_millis(500);
/// 요청 `Accept` 헤더(`OidcIdTokenDecoderFactory.JWK_SET_ACCEPT_TYPES`).
pub const JWKS_ACCEPT: &str = "application/json, application/jwk-set+json";

/// JWKS 조회 실패(통신 오류, 2xx가 아닌 응답, 시간 초과 등).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwksFetchError {
    message: String,
}

impl JwksFetchError {
    pub fn new(message: impl Into<String>) -> JwksFetchError {
        JwksFetchError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for JwksFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JwksFetchError {}

/// 공개키 목록을 가져오는 쪽. HTTP 클라이언트 crate가 구현한다.
///
/// 구현이 지킬 것(원본 `DefaultResourceRetriever` 설정): `GET`, 헤더 `Accept: `[`JWKS_ACCEPT`], 연결·읽기 타임아웃
/// 각 [`JWKS_CONNECT_TIMEOUT`]·[`JWKS_READ_TIMEOUT`], 2xx가 아니면 오류. 본문 크기 제한은 캐시가 한 번 더 확인한다.
pub trait JwksSource: Send + Sync {
    /// 응답 본문(JWK Set JSON).
    fn fetch(&self) -> Result<String, JwksFetchError>;
}

/// 시각 공급자. 테스트에서 가짜로 바꾼다.
pub trait Clock: Send + Sync {
    /// 에포크 밀리초.
    fn now_ms(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

#[derive(Debug, Clone)]
pub struct JwksCacheConfig {
    /// 캐시 수명. 원본 `DEFAULT_CACHE_TIME_TO_LIVE` 5분.
    pub cache_ttl: Duration,
    /// 속도 제한 구간. 원본 `JWK_SET_REFETCH_INTERVAL` 30초.
    pub rate_limit_interval: Duration,
    /// 구간당 조회 횟수. `RateLimitedJWKSetSource`는 구간마다 2회를 허용한다.
    pub rate_limit_requests: u32,
    /// 장애 시 마지막 목록을 쓰는 기간. 원본 `JWK_SET_OUTAGE_TIME_TO_LIVE` 50분.
    pub outage_ttl: Duration,
    /// 응답 본문 상한. 원본 `DEFAULT_HTTP_SIZE_LIMIT` 50KiB(51200바이트).
    pub max_body_bytes: usize,
}

impl Default for JwksCacheConfig {
    fn default() -> Self {
        JwksCacheConfig {
            cache_ttl: Duration::from_secs(5 * 60),
            rate_limit_interval: Duration::from_secs(30),
            rate_limit_requests: 2,
            outage_ttl: Duration::from_secs(50 * 60),
            max_body_bytes: 51_200,
        }
    }
}

struct Cached {
    set: Arc<Jwks>,
    expires_at: i64,
}

struct State {
    cached: Option<Cached>,
    /// 마지막으로 성공한 목록과 그것을 쓸 수 있는 시각(`OutageTolerantJWKSetSource`).
    last_good: Option<(Arc<Jwks>, i64)>,
    /// 속도 제한 구간이 끝나는 시각(`nextOpeningTime`)과 남은 횟수(`counter`).
    window_end: i64,
    remaining: u32,
}

/// 조회가 정상 흐름을 벗어난 순간을 받는 쪽(원본 `OidcJwkSetEventLogger`가 받는 Nimbus 이벤트).
/// 호출은 캐시 잠금 안에서 일어나므로 빨리 돌아와야 한다.
pub trait JwksEvents: Send + Sync {
    /// 첫 조회가 실패해 한 번 더 시도한다(`RetryingJWKSetSource.RetrialEvent`).
    fn retrial(&self, cause: &str);
    /// 조회가 끝내 실패해 이전에 받은 목록을 쓴다(`OutageTolerantJWKSetSource.OutageEvent`).
    /// `remaining_ms`는 그 목록을 더 쓸 수 있는 밀리초.
    fn outage(&self, remaining_ms: i64, cause: &str);
    /// 재조회 제한에 걸려 제공자에 요청하지 않았다(`RateLimitedJWKSetSource.RateLimitedEvent`).
    fn rate_limited(&self);
}

pub struct JwksCache {
    source: Arc<dyn JwksSource>,
    clock: Arc<dyn Clock>,
    config: JwksCacheConfig,
    state: Mutex<State>,
    events: Option<Arc<dyn JwksEvents>>,
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl JwksCache {
    /// 원본 기본 설정과 시스템 시계.
    pub fn new(source: Arc<dyn JwksSource>) -> JwksCache {
        // 기본 설정은 항상 유효하다.
        JwksCache::with_config(source, JwksCacheConfig::default(), Arc::new(SystemClock))
            .expect("default JWKS cache config is valid")
    }

    /// `JWKSourceBuilder#build`처럼 캐시 수명이 속도 제한 간격보다 길지 않으면 만들지 않는다.
    pub fn with_config(
        source: Arc<dyn JwksSource>,
        config: JwksCacheConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<JwksCache, String> {
        if config.cache_ttl <= config.rate_limit_interval {
            return Err("cache_ttl must be longer than rate_limit_interval".into());
        }
        if config.rate_limit_requests == 0 {
            return Err("rate_limit_requests must be at least 1".into());
        }
        Ok(JwksCache {
            source,
            clock,
            config,
            state: Mutex::new(State {
                cached: None,
                last_good: None,
                window_end: -1,
                remaining: 0,
            }),
            events: None,
        })
    }

    /// 조회 알림을 받을 쪽을 정한다. 쓰기 전에(캐시를 공유하기 전에) 한 번 부른다.
    pub fn set_events(&mut self, events: Arc<dyn JwksEvents>) {
        self.events = Some(events);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 지금 쓸 수 있는 키 목록. `refresh_if_same`이 현재 캐시와 같은 목록이면 만료 전이어도 다시 가져온다.
    pub fn jwks_refreshing(
        &self,
        refresh_if_same: Option<&Arc<Jwks>>,
    ) -> Result<Arc<Jwks>, JwksError> {
        let mut state = self.lock();
        let now = self.clock.now_ms();
        if let Some(cached) = &state.cached {
            let stale = refresh_if_same.is_some_and(|r| Arc::ptr_eq(r, &cached.set));
            if !stale && now < cached.expires_at {
                return Ok(cached.set.clone());
            }
        }
        self.take_rate_limit_token(&mut state, now)?;
        match self.fetch_with_retry() {
            Ok(set) => {
                let set = Arc::new(set);
                state.last_good =
                    Some((set.clone(), now.saturating_add(ms(self.config.outage_ttl))));
                state.cached = Some(Cached {
                    set: set.clone(),
                    expires_at: now.saturating_add(ms(self.config.cache_ttl)),
                });
                Ok(set)
            }
            Err(error) => {
                let Some((good, usable_until)) = &state.last_good else {
                    return Err(error);
                };
                if now >= *usable_until {
                    return Err(error);
                }
                if let Some(events) = &self.events {
                    events.outage(*usable_until - now, &error.to_string());
                }
                // 같은 키의 새 복사본. 호출자가 들고 있던 목록과 다른 객체라 그대로 돌려줘도 된다.
                let copy = Arc::new((**good).clone());
                state.cached = Some(Cached {
                    set: copy.clone(),
                    expires_at: now.saturating_add(ms(self.config.cache_ttl)),
                });
                Ok(copy)
            }
        }
    }

    /// `RateLimitedJWKSetSource#getJWKSet`: 구간이 끝났으면 새 구간을 열고 한 번 쓴 것으로 센다.
    fn take_rate_limit_token(&self, state: &mut State, now: i64) -> Result<(), JwksError> {
        if state.window_end <= now {
            state.window_end = now.saturating_add(ms(self.config.rate_limit_interval));
            state.remaining = self.config.rate_limit_requests - 1;
            Ok(())
        } else if state.remaining == 0 {
            if let Some(events) = &self.events {
                events.rate_limited();
            }
            Err(JwksError::RateLimited)
        } else {
            state.remaining -= 1;
            Ok(())
        }
    }

    fn fetch_with_retry(&self) -> Result<Jwks, JwksError> {
        // RetryingJWKSetSource: 조회 실패(통신·해석)는 한 번 더 시도한다.
        self.fetch_once().or_else(|first| {
            if let Some(events) = &self.events {
                events.retrial(&first.to_string());
            }
            self.fetch_once()
        })
    }

    fn fetch_once(&self) -> Result<Jwks, JwksError> {
        let body = self
            .source
            .fetch()
            .map_err(|e| JwksError::Fetch(e.to_string()))?;
        if body.len() > self.config.max_body_bytes {
            return Err(JwksError::Fetch(format!(
                "response exceeds the {} byte limit",
                self.config.max_body_bytes
            )));
        }
        Jwks::parse(&body)
    }

    /// RS256 토큰을 검증한다. kid로 키를 고르고, 목록에 없는 kid면 한 번 다시 가져온다
    /// (`JWKSetBasedJWKSource`). 기대 클레임 검증은 호출 측 [`crate::Validator`]가 한다.
    pub fn verify(&self, token: &str) -> Result<Claims, JwtError> {
        let jws = rs256_prepare(token)?;
        let kid = jws.kid.as_deref();
        let mut set = self.jwks_refreshing(None)?;
        if set.select_rs256(kid).is_empty()
            && let Some(kid) = kid
            && !set.has_key_id(kid)
        {
            set = self.jwks_refreshing(Some(&set))?;
        }
        let candidates = set.select_rs256(kid);
        rs256_check_candidates(&jws, &candidates)
    }
}
