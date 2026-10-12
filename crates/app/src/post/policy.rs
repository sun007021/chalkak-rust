//! 원본 `PostProcessingPolicy`: 이미지 처리 결과를 언제까지 기다릴지.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

#[derive(Clone, Copy, Debug)]
pub struct PostProcessingPolicy {
    image_processing_timeout: Duration,
}

impl PostProcessingPolicy {
    /// 0인 기간은 만들 수 없다(`IllegalArgumentException`).
    pub fn new(image_processing_timeout: Duration) -> Result<Self, String> {
        if image_processing_timeout.is_zero() {
            return Err("chalkak.post.image-processing-timeout은 0보다 커야 합니다.".to_owned());
        }
        Ok(PostProcessingPolicy {
            image_processing_timeout,
        })
    }

    /// `started_at + timeout < now`. 정확히 같은 시각이면 아직 시간 안이다.
    pub fn is_processing_timed_out(&self, started_at: Timestamp, now: Timestamp) -> bool {
        let timeout =
            SignedDuration::try_from(self.image_processing_timeout).unwrap_or(SignedDuration::MAX);
        match started_at.checked_add(timeout) {
            Ok(deadline) => deadline < now,
            Err(_) => false,
        }
    }

    pub fn image_processing_timeout(&self) -> Duration {
        self.image_processing_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn policy() -> PostProcessingPolicy {
        PostProcessingPolicy::new(Duration::from_secs(7 * 60)).unwrap()
    }

    #[test]
    fn is_processing_timed_out_after_timeout_returns_true() {
        let started = ts("2026-08-20T00:00:00Z");
        assert!(policy().is_processing_timed_out(started, ts("2026-08-20T00:07:01Z")));
    }

    #[test]
    fn is_processing_timed_out_at_timeout_returns_false() {
        let started = ts("2026-08-20T00:00:00Z");
        assert!(!policy().is_processing_timed_out(started, ts("2026-08-20T00:07:00Z")));
        assert!(!policy().is_processing_timed_out(started, ts("2026-08-20T00:03:00Z")));
    }

    #[test]
    fn constructor_non_positive_timeout_throws_illegal_argument_exception() {
        assert!(PostProcessingPolicy::new(Duration::ZERO).is_err());
    }
}
