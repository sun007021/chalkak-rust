//! 자동 재시도 전에 기다릴 시간(원본 `AiReviewRetryBackoff`). 첫 재시도는 10초, 그 뒤는 30초를 기본으로 하고 같은 시각에 실패한
//! 행들이 한꺼번에 다시 몰리지 않도록 ±20% 범위에서 흩는다. 외부 서비스가 더 오래 기다리라고 알려 줬다면 그 시간을 따르되 60초를
//! 넘기지 않는다.

use std::time::Duration;

const FIRST_DELAY: Duration = Duration::from_secs(10);
const LATER_DELAY: Duration = Duration::from_secs(30);
const MAX_DELAY: Duration = Duration::from_secs(60);
const JITTER_RATIO: f64 = 0.2;

/// 실제로 적용할 대기 시간과, 외부 서비스가 알려 준 시간이 백오프보다 길어 그 시간을 따랐는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryDelay {
    pub delay: Duration,
    pub retry_after_used: bool,
}

impl RetryDelay {
    pub const NONE: RetryDelay = RetryDelay {
        delay: Duration::ZERO,
        retry_after_used: false,
    };
}

pub struct AiReviewRetryBackoff {
    jitter: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl AiReviewRetryBackoff {
    /// `jitter`는 0 이상 1 미만의 값. 0.5가 기본 대기 시간이고 0이 -20%, 1에 가까울수록 +20%에 가깝다.
    pub fn new(jitter: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        AiReviewRetryBackoff {
            jitter: Box::new(jitter),
        }
    }

    /// 운영용: 난수로 흩는다.
    pub fn random() -> Self {
        AiReviewRetryBackoff::new(|| {
            let bytes = crate::common::random_bytes::<8>();
            // 상위 53비트로 [0, 1)을 만든다.
            (u64::from_be_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
        })
    }

    /// `round_attempt`: 방금 실패한 시도가 이번 회차에서 몇 번째 시도였는지(1부터).
    /// `retry_after`: 외부 서비스가 알려 준 최소 대기 시간. 없으면 백오프만 따른다.
    pub fn delay_after(&self, round_attempt: i32, retry_after: Option<Duration>) -> RetryDelay {
        let base = if round_attempt <= 1 {
            FIRST_DELAY
        } else {
            LATER_DELAY
        };
        let factor = 1.0 + JITTER_RATIO * (2.0 * (self.jitter)() - 1.0);
        let backoff = Duration::from_millis((base.as_millis() as f64 * factor).round() as u64);
        match retry_after {
            Some(requested) if requested > backoff => RetryDelay {
                delay: requested.min(MAX_DELAY),
                retry_after_used: true,
            },
            _ => RetryDelay {
                delay: backoff,
                retry_after_used: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff(jitter: f64) -> AiReviewRetryBackoff {
        AiReviewRetryBackoff::new(move || jitter)
    }

    // AiReviewRetryBackoffTest#delayAfter_middleJitter_returnsBaseDelay
    #[test]
    fn middle_jitter_returns_base_delay() {
        for (round, millis) in [(1, 10_000), (2, 30_000), (3, 30_000)] {
            let delay = backoff(0.5).delay_after(round, None);
            assert_eq!(delay.delay, Duration::from_millis(millis));
            assert!(!delay.retry_after_used);
        }
    }

    // AiReviewRetryBackoffTest#delayAfter_jitterBounds_staysWithinTwentyPercent
    #[test]
    fn jitter_bounds_stay_within_twenty_percent() {
        for (jitter, round, millis) in [
            (0.0, 1, 8_000),
            (1.0, 1, 12_000),
            (0.0, 2, 24_000),
            (1.0, 2, 36_000),
        ] {
            assert_eq!(
                backoff(jitter).delay_after(round, None).delay,
                Duration::from_millis(millis)
            );
        }
    }

    // AiReviewRetryBackoffTest#delayAfter_shorterRetryAfter_usesBackoff
    #[test]
    fn shorter_retry_after_uses_backoff() {
        let delay = backoff(0.5).delay_after(2, Some(Duration::from_secs(5)));
        assert_eq!(delay.delay, Duration::from_secs(30));
        assert!(!delay.retry_after_used);
    }

    // AiReviewRetryBackoffTest#delayAfter_longerRetryAfter_usesRetryAfter
    #[test]
    fn longer_retry_after_uses_retry_after() {
        let delay = backoff(0.5).delay_after(1, Some(Duration::from_secs(45)));
        assert_eq!(delay.delay, Duration::from_secs(45));
        assert!(delay.retry_after_used);
    }

    // AiReviewRetryBackoffTest#delayAfter_equalRetryAfter_isNotRetryAfterUsed
    #[test]
    fn equal_retry_after_is_not_retry_after_used() {
        let delay = backoff(0.5).delay_after(1, Some(Duration::from_secs(10)));
        assert_eq!(delay.delay, Duration::from_secs(10));
        assert!(!delay.retry_after_used);
    }

    // AiReviewRetryBackoffTest#delayAfter_hugeRetryAfter_capsAtSixtySeconds
    #[test]
    fn huge_retry_after_caps_at_sixty_seconds() {
        for seconds in [90, u64::MAX] {
            let delay = backoff(0.5).delay_after(1, Some(Duration::from_secs(seconds)));
            assert_eq!(delay.delay, Duration::from_secs(60));
            assert!(delay.retry_after_used);
        }
    }

    #[test]
    fn random_jitter_stays_within_bounds() {
        let backoff = AiReviewRetryBackoff::random();
        for _ in 0..100 {
            let millis = backoff.delay_after(1, None).delay.as_millis();
            assert!((8_000..=12_000).contains(&millis));
        }
    }
}
