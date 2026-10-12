//! 시각 공급자. 도메인 코드는 `Timestamp::now()`를 직접 부르지 않고 `Clock`을 주입받는다.
//!
//! 원본의 `java.time.Clock` 주입과 같은 역할이다. 하네스는 `libfaketime`으로 프로세스 시각을 옮기므로
//! `SystemClock`은 매 호출마다 OS 시각을 읽는다(값을 캐시하지 않는다).

use std::sync::Mutex;
use std::time::Duration;

use jiff::Timestamp;

/// 현재 시각을 돌려주는 계약.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// 실제 시각. 호출할 때마다 OS 시계를 읽는다.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// 테스트용 고정 시계. 시각을 직접 옮길 수 있고, 옮기기 전에는 같은 값만 돌려준다.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<Timestamp>,
}

impl FixedClock {
    pub fn new(now: Timestamp) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// 시각을 지정한 값으로 바꾼다. 과거로도 옮길 수 있다.
    pub fn set(&self, now: Timestamp) {
        *self.lock() = now;
    }

    /// 시각을 앞으로 옮긴다. 표현 범위를 넘으면 `Timestamp::MAX`에서 멈춘다.
    pub fn advance(&self, by: Duration) {
        let mut guard = self.lock();
        *guard = guard.checked_add(by).unwrap_or(Timestamp::MAX);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Timestamp> {
        // 시각 값 하나뿐이라 다른 스레드가 패닉해도 값은 일관된다.
        self.now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("테스트 시각")
    }

    #[test]
    fn system_clock_은_실제_시각에_가깝다() {
        let before = Timestamp::now();
        let now = SystemClock.now();
        let after = Timestamp::now();
        assert!(before <= now && now <= after);
    }

    #[test]
    fn fixed_clock_은_옮기기_전까지_같은_값을_돌려준다() {
        let clock = FixedClock::new(ts("2026-10-12T00:00:00Z"));
        assert_eq!(clock.now(), ts("2026-10-12T00:00:00Z"));
        assert_eq!(clock.now(), ts("2026-10-12T00:00:00Z"));
    }

    #[test]
    fn fixed_clock_은_앞으로_옮길_수_있다() {
        let clock = FixedClock::new(ts("2026-10-12T00:00:00Z"));
        clock.advance(Duration::from_secs(7 * 60));
        assert_eq!(clock.now(), ts("2026-10-12T00:07:00Z"));
    }

    #[test]
    fn fixed_clock_은_지정한_시각으로_바꿀_수_있다_과거_포함() {
        let clock = FixedClock::new(ts("2026-10-12T00:00:00Z"));
        clock.set(ts("2026-01-01T00:00:00Z"));
        assert_eq!(clock.now(), ts("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn clock_은_trait_object_로_공유할_수_있다() {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(ts("2026-10-12T00:00:00Z")));
        let shared = Arc::clone(&clock);
        let handle = std::thread::spawn(move || shared.now());
        assert_eq!(handle.join().unwrap(), clock.now());
    }

    #[test]
    fn advance_는_표현_범위를_넘으면_최댓값에서_멈춘다() {
        let clock = FixedClock::new(Timestamp::MAX);
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Timestamp::MAX);
    }
}
