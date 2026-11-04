//! AI 리뷰 워커 폴러와 설정(원본 `AiReviewWorker`, `AiReviewWorkerConfiguration`, `AiReviewWorkerProperties`).
//!
//! 폴러 하나가 대기열에서 행을 하나씩 가져가 스레드에 넘긴다. 처리 중인 건이 `concurrency`에 이르면 더 가져가지 않으며, 이 상한은
//! 서버 자원이 아니라 OpenAI의 분당 요청·토큰 한도를 지키기 위한 것이다. 원본은 가상 스레드를 쓰지만 여기서는 처리 건마다 스레드
//! 하나를 만든다(동시 처리 상한이 작아 같은 효과다).
//!
//! 종료할 때는 [`AiReviewWorker::stop`]이 새 건을 가져가지 않고 진행 중인 처리를 유예 시간까지 기다린다. 끝나지 못한 행은 처리
//! 중으로 남아 임대가 끝난 뒤 다시 가져간다. 원본은 유예가 지나면 스레드를 interrupt하지만 블로킹 HTTP 호출은 중간에 끊을 수 없어
//! 여기서는 기다림만 멈춘다(호출이 실패로 돌아오면 `stopping` 확인으로 실패를 쓰지 않고 행을 그대로 둔다).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chalkak_core::config::{ConfigError, Env};

use super::openai::OpenAiSettings;
use super::worker_service::{AbandonReason, AiReviewWorkerService, Claim};

/// 폴러가 앞 실행을 마친 뒤 다음 실행까지 쉬는 시간(`@Scheduled(fixedDelay = 1000)`).
pub const POLL_DELAY: Duration = Duration::from_secs(1);

// 종료에는 이 유예 외에도 서버의 종료 대기와 관리자 미리보기의 동기 OpenAI 호출 대기가 더해질 수 있어 systemd의
// TimeoutStopSec(30초)보다 넉넉히 짧게 둔다. 끝내지 못한 행은 임대 만료 뒤 회수되므로 데이터는 안전하다.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

// 호출이 끝난 뒤 결과를 반영하고 로그를 남길 여유
const LEASE_MARGIN: Duration = Duration::from_secs(20);

/// `chalkak.ai-review.worker.*`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiReviewWorkerSettings {
    /// `enabled`(기본 켜짐). 켜져 있어도 `chalkak.scheduling.enabled`가 꺼져 있으면 폴러는 만들지 않는다.
    pub enabled: bool,
    pub concurrency: usize,
    pub lease_duration: Duration,
    pub max_claims: i32,
}

fn invalid(key: &str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        reason: reason.to_owned(),
    }
}

impl AiReviewWorkerSettings {
    pub const DEFAULT_CONCURRENCY: usize = 10;
    pub const DEFAULT_LEASE: Duration = Duration::from_secs(120);
    pub const DEFAULT_MAX_CLAIMS: i32 = 3;

    pub fn from_env(env: &Env) -> Result<AiReviewWorkerSettings, ConfigError> {
        let concurrency_key = "CHALKAK_AIREVIEW_WORKER_CONCURRENCY";
        let concurrency: usize = env.parsed_or(concurrency_key, Self::DEFAULT_CONCURRENCY)?;
        if concurrency < 1 {
            return Err(invalid(
                concurrency_key,
                "AI 리뷰 동시 처리 수는 1 이상이어야 합니다.",
            ));
        }
        let claims_key = "CHALKAK_AIREVIEW_WORKER_MAXCLAIMS";
        let max_claims: i32 = env.parsed_or(claims_key, Self::DEFAULT_MAX_CLAIMS)?;
        if max_claims < 1 {
            return Err(invalid(
                claims_key,
                "AI 리뷰 가져가기 상한은 1 이상이어야 합니다.",
            ));
        }
        Ok(AiReviewWorkerSettings {
            enabled: env.bool_or("CHALKAK_AIREVIEW_WORKER_ENABLED", true)?,
            concurrency,
            lease_duration: env
                .duration_or("CHALKAK_AIREVIEW_WORKER_LEASEDURATION", Self::DEFAULT_LEASE)?,
            max_claims,
        })
    }

    /// 임대가 호출 제한 시간보다 짧으면 호출이 끝나기 전에 다른 워커가 같은 행을 다시 가져가 OpenAI를 두 번 호출한다.
    /// 워커가 켜져 있을 때만 검사한다.
    pub fn validate_lease(&self, openai: &OpenAiSettings) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        let minimum = openai.connect_timeout + openai.read_timeout + LEASE_MARGIN;
        if self.lease_duration < minimum {
            return Err(invalid(
                "CHALKAK_AIREVIEW_WORKER_LEASEDURATION",
                &format!(
                    "AI 리뷰 임대 시간({:?})은 OpenAI 연결·응답 제한 시간에 여유 {:?}를 더한 {:?} 이상이어야 합니다.",
                    self.lease_duration, LEASE_MARGIN, minimum
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Permits {
    in_flight: Mutex<usize>,
    idle: Condvar,
}

impl Permits {
    fn try_acquire(&self, limit: usize) -> bool {
        let mut n = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *n >= limit {
            return false;
        }
        *n += 1;
        true
    }

    fn release(&self) {
        let mut n = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.idle.notify_all();
        }
    }

    /// 진행 중인 처리가 모두 끝나기를 `timeout`까지 기다린다.
    fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut n = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *n > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            n = self
                .idle
                .wait_timeout(n, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

pub struct AiReviewWorker {
    service: Arc<AiReviewWorkerService>,
    concurrency: usize,
    permits: Arc<Permits>,
    stopping: Arc<AtomicBool>,
}

impl AiReviewWorker {
    pub fn new(service: Arc<AiReviewWorkerService>, concurrency: usize) -> AiReviewWorker {
        AiReviewWorker {
            service,
            concurrency,
            permits: Arc::new(Permits::default()),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 빈 자리가 있는 동안 대기열에서 행을 가져가 처리 스레드에 넘긴다. 폴링 실패는 로그를 남기고 삼킨다.
    pub fn poll(&self) {
        while !self.stopping.load(Ordering::SeqCst) && self.permits.try_acquire(self.concurrency) {
            match self.submit_next() {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    self.service.log_poll_failed(&error);
                    return;
                }
            }
        }
    }

    /// 새 건을 가져가지 않고 진행 중인 처리를 유예 시간까지 기다린다. 유예 안에 끝났으면 `true`.
    pub fn stop(&self, grace: Duration) -> bool {
        self.stopping.store(true, Ordering::SeqCst);
        self.permits.wait_idle(grace)
    }

    /// 얻어 둔 자리 하나로 행을 가져가 스레드에 넘긴다. 넘기지 못하면 자리를 바로 돌려준다.
    fn submit_next(&self) -> Result<bool, crate::web::ApiError> {
        let claim = match self.service.claim_next() {
            Ok(Some(claim)) => claim,
            Ok(None) => {
                self.permits.release();
                return Ok(false);
            }
            Err(error) => {
                self.permits.release();
                return Err(error);
            }
        };
        let (service, permits, stopping) = (
            Arc::clone(&self.service),
            Arc::clone(&self.permits),
            Arc::clone(&self.stopping),
        );
        let task_claim = claim.clone();
        let spawned = std::thread::Builder::new()
            .name("ai-review-worker".to_owned())
            .spawn(move || run(&service, &permits, &stopping, &task_claim));
        if spawned.is_err() {
            self.permits.release();
            self.service.abandon(&claim, AbandonReason::Shutdown, None);
            return Ok(false);
        }
        Ok(true)
    }
}

fn run(service: &AiReviewWorkerService, permits: &Permits, stopping: &AtomicBool, claim: &Claim) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        service.process(claim, &|| stopping.load(Ordering::SeqCst));
    }));
    if outcome.is_err() {
        service.abandon(claim, AbandonReason::UnexpectedWhileProcessing, None);
    }
    permits.release();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai() -> OpenAiSettings {
        OpenAiSettings {
            api_key: "test-key".to_owned(),
            base_url: "https://api.openai.test/v1".to_owned(),
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(60),
            allowed_models: vec!["gpt-6.1-sol".to_owned()],
        }
    }

    fn settings(pairs: &[(&str, &str)]) -> Result<AiReviewWorkerSettings, ConfigError> {
        AiReviewWorkerSettings::from_env(&Env::from_pairs(pairs.iter().copied()))
    }

    // AiReviewWorkerConfigurationTest#configuration_enabled_createsWorker (application.yml 값)
    #[test]
    fn 기본값은_동시_10_임대_120초_가져가기_3회다() {
        let s = settings(&[]).unwrap();
        assert!(s.enabled);
        assert_eq!(s.concurrency, 10);
        assert_eq!(s.lease_duration, Duration::from_secs(120));
        assert_eq!(s.max_claims, 3);
        assert_eq!(POLL_DELAY, Duration::from_secs(1));
        assert!(s.validate_lease(&openai()).is_ok());
    }

    // AiReviewWorkerConfigurationTest#configuration_leaseShorterThanCallTimeouts_failsFast: 84초는 거절, 85초는 통과
    #[test]
    fn 임대_시간이_연결_응답_제한_시간_더하기_20초보다_짧으면_거절한다() {
        let short = settings(&[("CHALKAK_AIREVIEW_WORKER_LEASEDURATION", "84s")]).unwrap();
        let error = short.validate_lease(&openai()).unwrap_err();
        assert!(error.to_string().contains("임대 시간"), "{error}");
        let enough = settings(&[("CHALKAK_AIREVIEW_WORKER_LEASEDURATION", "85s")]).unwrap();
        assert!(enough.validate_lease(&openai()).is_ok());
    }

    // 워커를 켜지 않으면 임대를 검사하지 않는다.
    #[test]
    fn 워커를_끄면_임대를_검사하지_않는다() {
        let off = settings(&[
            ("CHALKAK_AIREVIEW_WORKER_ENABLED", "false"),
            ("CHALKAK_AIREVIEW_WORKER_LEASEDURATION", "1s"),
        ])
        .unwrap();
        assert!(off.validate_lease(&openai()).is_ok());
    }

    // AiReviewWorkerConfigurationTest#configuration_nonPositiveLimits_failsFast
    #[test]
    fn 동시_처리_수와_가져가기_상한이_1보다_작으면_거절한다() {
        assert!(settings(&[("CHALKAK_AIREVIEW_WORKER_CONCURRENCY", "0")]).is_err());
        assert!(settings(&[("CHALKAK_AIREVIEW_WORKER_MAXCLAIMS", "0")]).is_err());
    }
}
