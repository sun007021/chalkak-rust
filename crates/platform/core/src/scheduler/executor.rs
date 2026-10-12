//! 주기 작업 실행기. 작업마다 std 스레드 하나를 쓰고, 종료 신호를 받으면 대기를 즉시 깨워 끝낸다.
//!
//! 원본(`@Scheduled`, 풀 크기 4) 동작을 따른다.
//!
//! - fixed-rate: 시작 즉시 첫 실행, 이후 **시작 시각 기준** `period` 간격. 실행이 `period`보다 길어지면 다음
//!   실행은 끝나자마자 이어지고(밀린 횟수만큼 따라잡는다) 같은 작업이 겹쳐 실행되지는 않는다
//!   (`ScheduledExecutorService.scheduleAtFixedRate`와 같다). 간격은 단조 시계(`Instant`)로 잰다.
//! - cron: [`Clock`]이 가리키는 시각이 다음 실행 시각에 닿으면 실행한다. 시계를 옮기는 테스트나 `libfaketime`
//!   하네스를 따라가도록 짧은 간격(기본 1초)으로 시계를 다시 읽는다. 실행 뒤에는 현재 시각 **다음 초부터** 다시
//!   계산하므로 같은 시각에 두 번 실행하지 않는다.
//! - 작업이 패닉하면 오류 로그를 남기고 일정은 계속된다(Spring의 `LOG_AND_SUPPRESS_ERROR_HANDLER`).

use std::any::Any;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::clock::Clock;
use crate::log::{Logger, fields};
use crate::scheduler::cron::CronSchedule;

/// 종료 신호. 복제해서 여러 곳(서버, 스케줄러)이 같은 신호를 본다.
#[derive(Clone, Debug, Default)]
pub struct ShutdownSignal {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// 종료를 알린다. 기다리던 모든 스레드가 깨어난다. 여러 번 불러도 된다.
    pub fn trigger(&self) {
        let (lock, cvar) = &*self.inner;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
    }

    pub fn is_triggered(&self) -> bool {
        *self.inner.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 최대 `timeout`만큼 기다린다. 종료 신호가 오면 `true`, 시간이 다 되면 `false`.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (lock, cvar) = &*self.inner;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = cvar
            .wait_timeout_while(guard, timeout, |triggered| !*triggered)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

/// 작업 스레드를 모아 관리한다.
pub struct Scheduler {
    shutdown: ShutdownSignal,
    clock: Arc<dyn Clock>,
    logger: Option<Arc<Logger>>,
    poll_interval: Duration,
    handles: Vec<JoinHandle<()>>,
}

impl Scheduler {
    pub fn new(shutdown: ShutdownSignal, clock: Arc<dyn Clock>) -> Self {
        Self {
            shutdown,
            clock,
            logger: None,
            poll_interval: Duration::from_secs(1),
            handles: Vec::new(),
        }
    }

    /// 작업 실패·일정 종료를 남길 로거. 없으면 표준 오류에 쓴다.
    #[must_use]
    pub fn with_logger(mut self, logger: Arc<Logger>) -> Self {
        self.logger = Some(logger);
        self
    }

    /// cron 작업이 시계를 다시 읽는 최대 간격(기본 1초).
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval.max(Duration::from_millis(1));
        self
    }

    /// `@Scheduled(fixedRate = ...)`. 시작하자마자 한 번 실행하고 `period`마다 반복한다.
    pub fn fixed_rate(
        &mut self,
        name: &str,
        period: Duration,
        mut task: impl FnMut() + Send + 'static,
    ) -> io::Result<()> {
        if period.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixed-rate 간격은 0보다 커야 합니다",
            ));
        }
        let shutdown = self.shutdown.clone();
        let reporter = self.reporter(name);
        self.spawn(name, move || {
            let start = Instant::now();
            let mut runs: u32 = 0;
            while !shutdown.is_triggered() {
                run_guarded(&reporter, &mut task);
                runs = runs.saturating_add(1);
                let Some(next) = period
                    .checked_mul(runs)
                    .and_then(|offset| start.checked_add(offset))
                else {
                    return; // 표현할 수 없을 만큼 먼 시각
                };
                let now = Instant::now();
                if next > now && shutdown.wait_timeout(next - now) {
                    return;
                }
            }
        })
    }

    /// `@Scheduled(cron = ..., zone = ...)`. 다음 실행 시각이 없으면 스레드를 끝낸다.
    pub fn cron(
        &mut self,
        name: &str,
        schedule: CronSchedule,
        mut task: impl FnMut() + Send + 'static,
    ) -> io::Result<()> {
        let shutdown = self.shutdown.clone();
        let clock = Arc::clone(&self.clock);
        let poll = self.poll_interval;
        let reporter = self.reporter(name);
        self.spawn(name, move || {
            let mut base = clock.now();
            loop {
                let Some(next) = schedule.next_after(base) else {
                    reporter.report("다음 실행 시각이 없어 일정을 끝냅니다", None);
                    return;
                };
                // 시계가 다음 실행 시각에 닿을 때까지 짧게 끊어 기다린다.
                loop {
                    if shutdown.is_triggered() {
                        return;
                    }
                    let now = clock.now();
                    if now >= next {
                        break;
                    }
                    let remaining = next.duration_since(now).unsigned_abs();
                    if shutdown.wait_timeout(remaining.min(poll)) {
                        return;
                    }
                }
                run_guarded(&reporter, &mut task);
                base = clock.now().max(next);
            }
        })
    }

    /// 모든 작업 스레드가 끝나기를 기다린다. 먼저 종료 신호를 보내야 한다.
    pub fn join(self) {
        for handle in self.handles {
            // 작업 패닉은 run_guarded가 잡으므로 여기서 실패하면 스레드 자체가 이상한 것이다.
            let _ = handle.join();
        }
    }

    fn spawn(&mut self, name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
        let handle = std::thread::Builder::new()
            .name(format!("scheduler-{name}"))
            .spawn(body)?;
        self.handles.push(handle);
        Ok(())
    }

    fn reporter(&self, name: &str) -> Reporter {
        Reporter {
            task: name.to_owned(),
            logger: self.logger.clone(),
        }
    }
}

struct Reporter {
    task: String,
    logger: Option<Arc<Logger>>,
}

impl Reporter {
    fn report(&self, message: &str, panic_message: Option<&str>) {
        match &self.logger {
            Some(logger) => {
                let mut event = logger
                    .error("chalkak.scheduler")
                    .field(fields::TYPE, fields::TYPE_ERROR)
                    .field("task", self.task.as_str());
                if let Some(detail) = panic_message {
                    event = event.field("panic", detail);
                }
                event.log(message);
            }
            None => eprintln!("[scheduler] {}: {message} {panic_message:?}", self.task),
        }
    }
}

fn run_guarded(reporter: &Reporter, task: &mut (impl FnMut() + ?Sized)) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(task)) {
        reporter.report("예약 작업 실패", Some(&panic_message(payload.as_ref())));
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "알 수 없는 패닉".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::scheduler::cron::SEOUL;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "시간 초과: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn fixed_clock(at: &str) -> Arc<FixedClock> {
        Arc::new(FixedClock::new(at.parse().unwrap()))
    }

    #[test]
    fn 종료_신호는_대기를_즉시_깨운다() {
        let signal = ShutdownSignal::new();
        assert!(!signal.wait_timeout(Duration::from_millis(10)));
        let waiter = signal.clone();
        let started = Instant::now();
        let handle = std::thread::spawn(move || waiter.wait_timeout(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(20));
        signal.trigger();
        assert!(handle.join().unwrap());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(signal.is_triggered());
        // 이미 신호가 온 뒤에는 기다리지 않는다.
        assert!(signal.wait_timeout(Duration::from_secs(30)));
    }

    // RuntimeSnapshotLogger: @Scheduled(fixedRate = 60_000) - 시작 즉시 첫 실행
    #[test]
    fn fixed_rate_는_시작_즉시_실행하고_반복한다() {
        let shutdown = ShutdownSignal::new();
        let mut scheduler = Scheduler::new(shutdown.clone(), fixed_clock("2026-10-12T00:00:00Z"));
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        scheduler
            .fixed_rate("tick", Duration::from_millis(20), move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        wait_until("첫 실행", || count.load(Ordering::SeqCst) >= 1);
        wait_until("반복 실행", || count.load(Ordering::SeqCst) >= 4);
        shutdown.trigger();
        scheduler.join();
        let after_stop = count.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            count.load(Ordering::SeqCst),
            after_stop,
            "종료 뒤에는 실행하지 않는다"
        );
    }

    #[test]
    fn 간격이_긴_fixed_rate도_종료_신호에_바로_끝난다() {
        let shutdown = ShutdownSignal::new();
        let mut scheduler = Scheduler::new(shutdown.clone(), fixed_clock("2026-10-12T00:00:00Z"));
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        scheduler
            .fixed_rate("slow", Duration::from_secs(3600), move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        wait_until("첫 실행", || count.load(Ordering::SeqCst) == 1);
        let started = Instant::now();
        shutdown.trigger();
        scheduler.join();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn 간격_0은_거절한다() {
        let mut scheduler =
            Scheduler::new(ShutdownSignal::new(), fixed_clock("2026-10-12T00:00:00Z"));
        let err = scheduler
            .fixed_rate("x", Duration::ZERO, || {})
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    // scheduleAtFixedRate: 실행이 간격보다 길어도 같은 작업이 겹치지 않는다
    #[test]
    fn 오래_걸리는_작업도_겹쳐_실행되지_않는다() {
        let shutdown = ShutdownSignal::new();
        let mut scheduler = Scheduler::new(shutdown.clone(), fixed_clock("2026-10-12T00:00:00Z"));
        let running = Arc::new(AtomicBool::new(false));
        let overlapped = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        let (r, o, n) = (
            Arc::clone(&running),
            Arc::clone(&overlapped),
            Arc::clone(&runs),
        );
        scheduler
            .fixed_rate("slow-task", Duration::from_millis(5), move || {
                if r.swap(true, Ordering::SeqCst) {
                    o.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(20));
                r.store(false, Ordering::SeqCst);
                n.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        wait_until("3회 실행", || runs.load(Ordering::SeqCst) >= 3);
        shutdown.trigger();
        scheduler.join();
        assert!(!overlapped.load(Ordering::SeqCst));
    }

    #[test]
    fn 작업이_패닉해도_일정은_계속된다() {
        let shutdown = ShutdownSignal::new();
        let mut scheduler = Scheduler::new(shutdown.clone(), fixed_clock("2026-10-12T00:00:00Z"));
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        scheduler
            .fixed_rate("flaky", Duration::from_millis(10), move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                assert_ne!(n, 0, "첫 실행은 실패한다");
            })
            .unwrap();
        wait_until("패닉 뒤 재실행", || count.load(Ordering::SeqCst) >= 3);
        shutdown.trigger();
        scheduler.join();
    }

    // RefreshTokenCleanupScheduler: 서울 04:30 매일
    #[test]
    fn cron_작업은_시계가_다음_실행_시각에_닿을_때만_실행한다() {
        let shutdown = ShutdownSignal::new();
        // 서울 2026-10-12 04:29:59 = 2026-10-11T19:29:59Z
        let clock = fixed_clock("2026-10-11T19:29:59Z");
        let mut scheduler = Scheduler::new(shutdown.clone(), clock.clone())
            .with_poll_interval(Duration::from_millis(5));
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        scheduler
            .cron(
                "refresh-token-cleanup",
                CronSchedule::parse("0 30 4 * * *", SEOUL).unwrap(),
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                },
            )
            .unwrap();

        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(count.load(Ordering::SeqCst), 0, "아직 04:30이 아니다");

        clock.advance(Duration::from_secs(1)); // 04:30:00
        wait_until("04:30 실행", || count.load(Ordering::SeqCst) == 1);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "같은 시각에 두 번 실행하지 않는다"
        );

        clock.advance(Duration::from_secs(24 * 3600)); // 이튿날 04:30:00
        wait_until("이튿날 실행", || count.load(Ordering::SeqCst) == 2);

        shutdown.trigger();
        scheduler.join();
    }

    #[test]
    fn 먼_cron_도_종료_신호에_바로_끝난다() {
        let shutdown = ShutdownSignal::new();
        let mut scheduler = Scheduler::new(shutdown.clone(), fixed_clock("2026-10-12T00:00:00Z"));
        scheduler
            .cron(
                "far",
                CronSchedule::parse("0 30 4 * * *", SEOUL).unwrap(),
                || {},
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        shutdown.trigger();
        scheduler.join();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn 다음_실행_시각이_없으면_스레드가_끝난다() {
        // 2월 31일은 없다. 신호 없이도 join이 끝나야 한다.
        let mut scheduler =
            Scheduler::new(ShutdownSignal::new(), fixed_clock("2026-10-12T00:00:00Z"));
        scheduler
            .cron(
                "never",
                CronSchedule::parse("0 0 0 31 2 *", SEOUL).unwrap(),
                || unreachable!("실행되면 안 된다"),
            )
            .unwrap();
        scheduler.join();
    }
}
