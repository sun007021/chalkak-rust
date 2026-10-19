//! 사용자 지표 집계를 한 번에 하나만 실행하는 러너(원본 `UserMetricsAggregationRunner`).
//!
//! 스케줄러(동기)와 관리자 수동 요청(백그라운드)이 같은 가드를 쓰므로 두 실행이 겹치지 않는다. 실패 로그는 두 경로가 같은
//! 모양으로 남도록 여기서 남긴다. 백그라운드 실행의 실패는 로그만 남고 호출자에게 전달되지 않는다.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::{Logger, fields};
use jiff::civil::Date;
use uuid::Uuid;

use super::service::{KST, UserMetricsAggregationService};
use crate::jobs::JobFailure;
use crate::web::ApiError;

/// 원본 `@Slf4j`의 로거 이름(클래스 이름).
const LOGGER_NAME: &str = "com.chalkak.backend.analytics.service.UserMetricsAggregationRunner";

/// 백그라운드 작업을 돌리는 실행기(원본 `applicationTaskExecutor`).
pub trait TaskExecutor: Send + Sync {
    /// 작업을 맡긴다. 실행기가 받지 못하면 오류를 돌려준다(원본 `TaskRejectedException`).
    fn execute(&self, task: Box<dyn FnOnce() + Send>) -> Result<(), ApiError>;
}

/// 작업마다 `task-N` 이름의 스레드를 하나 띄운다. 러너가 동시에 하나만 허용하므로 스레드가 쌓이지 않는다.
#[derive(Default)]
pub struct ThreadTaskExecutor {
    next: AtomicU64,
}

impl TaskExecutor for ThreadTaskExecutor {
    fn execute(&self, task: Box<dyn FnOnce() + Send>) -> Result<(), ApiError> {
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        std::thread::Builder::new()
            .name(format!("task-{n}"))
            .spawn(task)
            .map(|_| ())
            .map_err(|e| ApiError::Internal(format!("집계 스레드를 시작할 수 없습니다: {e}")))
    }
}

#[derive(Clone)]
pub struct UserMetricsAggregationRunner {
    service: Arc<UserMetricsAggregationService>,
    executor: Arc<dyn TaskExecutor>,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
    running: Arc<AtomicBool>,
}

impl UserMetricsAggregationRunner {
    pub fn new(
        service: Arc<UserMetricsAggregationService>,
        executor: Arc<dyn TaskExecutor>,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
    ) -> Self {
        UserMetricsAggregationRunner {
            service,
            executor,
            clock,
            logger,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 끝난 날(KST)만 받아 백그라운드로 집계를 시작한다. 이미 집계 중이면 시작하지 않는다.
    pub fn start(&self, admin_id: Uuid, metric_date: Date) -> Result<(), ApiError> {
        self.validate_finished_day(metric_date)?;
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(in_progress());
        }
        self.logger
            .info(LOGGER_NAME)
            .field(fields::ADMIN_ID, admin_id.hyphenated().to_string())
            .field(fields::METRIC_DATE, metric_date.to_string())
            .log("사용자 지표 수동 집계 요청");

        let (service, logger, running) = (
            Arc::clone(&self.service),
            Arc::clone(&self.logger),
            Arc::clone(&self.running),
        );
        let task = move || {
            // 패닉이 나도 표시가 풀리도록 Drop으로 푼다(원본 `finally`).
            let _release = Release(running);
            // 받을 호출자가 없으니 로그로만 알린다.
            if let Err(error) = service.aggregate(metric_date) {
                log_failure(&logger, metric_date, &error);
            }
        };
        match self.executor.execute(Box::new(task)) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.running.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    /// 호출 스레드에서 집계한다. 실패하면 로그를 남기고 오류를 그대로 돌려준다.
    ///
    /// 수동 집계와 겹쳐 실행하지 못한 경우도 그날 집계가 빠진 것이므로 실패로 남긴다.
    pub fn run(&self, metric_date: Date) -> Result<(), ApiError> {
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let error = in_progress();
            log_failure(&self.logger, metric_date, &error);
            return Err(error);
        }
        let _release = Release(Arc::clone(&self.running));
        let result = self.service.aggregate(metric_date).map(|_| ());
        if let Err(error) = &result {
            log_failure(&self.logger, metric_date, error);
        }
        result
    }

    fn validate_finished_day(&self, metric_date: Date) -> Result<(), ApiError> {
        let today = self.clock.now().to_zoned(KST.clone()).date();
        if metric_date < today {
            return Ok(());
        }
        Err(ApiError::App(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            "아직 끝나지 않은 날짜는 집계할 수 없습니다.",
        )))
    }
}

/// 진행 중 표시를 푼다.
struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn in_progress() -> ApiError {
    ApiError::App(AppError::business(
        ErrorCode::USER_METRICS_AGGREGATION_IN_PROGRESS,
        "이미 사용자 지표 집계가 진행 중입니다.",
    ))
}

fn log_failure(logger: &Logger, metric_date: Date, error: &ApiError) {
    logger
        .error(LOGGER_NAME)
        .field(fields::TYPE, fields::TYPE_ERROR)
        .field(fields::METRIC_DATE, metric_date.to_string())
        .error(&JobFailure::of(error))
        .log("사용자 지표 집계 실패");
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chalkak_core::FixedClock;

    use super::*;
    use crate::analytics::service::UserMetricsRepository;
    use crate::analytics::testing::{Lines, NoAccessLogs, QueueExecutor, logger};

    // UTC 2030-01-02 16:30은 KST 2030-01-03 01:30이다.
    const NOW: &str = "2030-01-02T16:30:00Z";

    fn yesterday() -> Date {
        "2030-01-02".parse().unwrap()
    }

    fn admin() -> Uuid {
        "0198fd21-0000-7000-8000-000000000001".parse().unwrap()
    }

    /// 호출 횟수와 날짜를 세는 저장소. `fail`이면 첫 조회가 실패한다.
    #[derive(Default)]
    struct CountingRepository {
        post_author_calls: Mutex<Vec<(i64, i64)>>,
        fail: bool,
    }

    impl UserMetricsRepository for CountingRepository {
        fn count_post_authors(
            &self,
            from: jiff::Timestamp,
            to: jiff::Timestamp,
        ) -> Result<i64, ApiError> {
            if self.fail {
                return Err(ApiError::Internal("db down".into()));
            }
            self.post_author_calls
                .lock()
                .unwrap()
                .push((from.as_second(), to.as_second()));
            Ok(0)
        }
        fn count_posts(&self, _: jiff::Timestamp, _: jiff::Timestamp) -> Result<i64, ApiError> {
            Ok(0)
        }
        fn count_returning_post_authors(
            &self,
            _: jiff::Timestamp,
            _: jiff::Timestamp,
            _: jiff::Timestamp,
            _: jiff::Timestamp,
        ) -> Result<i64, ApiError> {
            Ok(0)
        }
        fn count_eligible_users(&self) -> Result<i64, ApiError> {
            Ok(0)
        }
        fn count_likes_on_live_posts(&self) -> Result<i64, ApiError> {
            Ok(0)
        }
    }

    struct Fixture {
        runner: UserMetricsAggregationRunner,
        repository: Arc<CountingRepository>,
        executor: Arc<QueueExecutor>,
        lines: Lines,
    }

    fn fixture(fail: bool, reject: bool) -> Fixture {
        let lines = Lines::default();
        let repository = Arc::new(CountingRepository {
            fail,
            ..CountingRepository::default()
        });
        let service = Arc::new(UserMetricsAggregationService::new(
            repository.clone(),
            Arc::new(NoAccessLogs),
            logger(&lines),
        ));
        let executor = Arc::new(QueueExecutor {
            reject,
            ..QueueExecutor::default()
        });
        let runner = UserMetricsAggregationRunner::new(
            service,
            executor.clone(),
            Arc::new(FixedClock::new(NOW.parse().unwrap())),
            logger(&lines),
        );
        Fixture {
            runner,
            repository,
            executor,
            lines,
        }
    }

    fn code(error: &ApiError) -> ErrorCode {
        error.code()
    }

    // UserMetricsAggregationRunnerTest#start_finishedDay_aggregatesInExecutor
    #[test]
    fn 백그라운드_집계는_요청_시점에는_집계하지_않고_실행기가_돌릴_때_집계한다() {
        let f = fixture(false, false);

        f.runner.start(admin(), yesterday()).unwrap();
        assert!(f.repository.post_author_calls.lock().unwrap().is_empty());

        f.executor.run_first();
        // 하루(dau)와 7일(wau) 두 번 센다. 2030-01-02 00:00 KST = 2030-01-01T15:00Z.
        assert_eq!(
            f.repository.post_author_calls.lock().unwrap()[0],
            (1_893_510_000, 1_893_596_400)
        );
    }

    // UserMetricsAggregationRunnerTest#start_finishedDay_logsAdminAndMetricDate
    #[test]
    fn 집계_요청은_관리자와_날짜를_info_로그로_남긴다() {
        let f = fixture(false, false);

        f.runner.start(admin(), yesterday()).unwrap();

        let events = f.lines.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["level"], "INFO");
        assert_eq!(events[0]["adminId"], "0198fd21-0000-7000-8000-000000000001");
        assert_eq!(events[0]["metricDate"], "2030-01-02");
        assert_eq!(events[0]["message"], "사용자 지표 수동 집계 요청");
    }

    // UserMetricsAggregationRunnerTest#start_alreadyRunning_throwsInProgressWithoutSecondAggregation
    #[test]
    fn 집계가_진행_중일_때_다시_시작하면_진행_중_에러_코드로_거절하고_두_번_집계하지_않는다() {
        let f = fixture(false, false);
        f.runner.start(admin(), yesterday()).unwrap();

        let error = f.runner.start(admin(), yesterday()).unwrap_err();

        assert_eq!(
            code(&error),
            ErrorCode::USER_METRICS_AGGREGATION_IN_PROGRESS
        );
        assert_eq!(error.status(), 400);
        assert_eq!(error.message(), "이미 사용자 지표 집계가 진행 중입니다.");
        assert_eq!(f.executor.len(), 1);
    }

    // UserMetricsAggregationRunnerTest#start_afterSuccess_releasesGuard
    #[test]
    fn 집계가_성공하면_다시_시작할_수_있다() {
        let f = fixture(false, false);
        f.runner.start(admin(), yesterday()).unwrap();
        f.executor.run_first();

        f.runner.start(admin(), yesterday()).unwrap();

        assert_eq!(f.executor.len(), 1);
    }

    // UserMetricsAggregationRunnerTest#start_afterFailure_logsErrorAndReleasesGuard
    #[test]
    fn 백그라운드_집계가_실패하면_type_error_로그를_남기고_다시_시작할_수_있다() {
        let f = fixture(true, false);
        f.runner.start(admin(), yesterday()).unwrap();

        f.executor.run_first();

        let events = f.lines.events();
        let event = events.last().unwrap();
        assert_eq!(event["level"], "ERROR");
        assert_eq!(event["message"], "사용자 지표 집계 실패");
        assert_eq!(event["type"], "error");
        assert_eq!(event["metricDate"], "2030-01-02");
        assert!(event["stack_trace"].as_str().unwrap().contains("db down"));
        f.runner.start(admin(), yesterday()).unwrap();
        assert_eq!(f.executor.len(), 1);
    }

    // UserMetricsAggregationRunnerTest#start_executorRejects_rethrowsAndReleasesGuard
    #[test]
    fn 실행기가_작업을_거절하면_오류를_돌려주고_다시_시작할_수_있다() {
        let f = fixture(false, true);

        for _ in 0..2 {
            let error = f.runner.start(admin(), yesterday()).unwrap_err();
            assert!(matches!(error, ApiError::Internal(ref m) if m == "queue full"));
        }
    }

    // UserMetricsAggregationRunnerTest#start_todayAndFuture_throwsBusinessError
    #[test]
    fn 오늘과_미래_날짜는_거절하고_어제는_허용한다() {
        let f = fixture(false, false);

        for day in ["2030-01-03", "2030-01-04"] {
            let error = f.runner.start(admin(), day.parse().unwrap()).unwrap_err();
            assert_eq!(code(&error), ErrorCode::BUSINESS_ERROR);
            assert_eq!(
                error.message(),
                "아직 끝나지 않은 날짜는 집계할 수 없습니다."
            );
        }
        f.runner.start(admin(), yesterday()).unwrap();
        assert_eq!(f.executor.len(), 1);
    }

    // UserMetricsAggregationRunnerTest#start_futureDate_doesNotHoldGuard
    #[test]
    fn 거절된_날짜_요청은_진행_중_표시를_남기지_않는다() {
        let f = fixture(false, false);
        assert!(
            f.runner
                .start(admin(), "2030-01-03".parse().unwrap())
                .is_err()
        );

        f.runner.start(admin(), yesterday()).unwrap();

        assert_eq!(f.executor.len(), 1);
    }

    // UserMetricsAggregationRunnerTest#run_idle_aggregatesAndReleasesGuard
    #[test]
    fn 동기_집계는_호출_스레드에서_집계하고_끝나면_다시_실행할_수_있다() {
        let f = fixture(false, false);

        f.runner.run(yesterday()).unwrap();
        f.runner.run(yesterday()).unwrap();

        // 집계마다 dau·wau 두 번 센다.
        assert_eq!(f.repository.post_author_calls.lock().unwrap().len(), 4);
    }

    // UserMetricsAggregationRunnerTest#run_aggregationFails_logsErrorAndRethrows
    #[test]
    fn 동기_집계가_실패하면_type_error_로그를_남기고_오류를_그대로_돌려준다() {
        let f = fixture(true, false);

        let error = f.runner.run(yesterday()).unwrap_err();
        assert!(matches!(error, ApiError::Internal(ref m) if m == "db down"));
        let events = f.lines.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["level"], "ERROR");
        assert_eq!(events[0]["type"], "error");
        assert_eq!(events[0]["metricDate"], "2030-01-02");
        // 가드가 풀려 다시 실행되고 같은 오류를 돌려준다.
        assert!(matches!(
            f.runner.run(yesterday()).unwrap_err(),
            ApiError::Internal(ref m) if m == "db down"
        ));
    }

    // UserMetricsAggregationRunnerTest#run_whileBackgroundRunning_logsErrorAndThrowsInProgress
    #[test]
    fn 백그라운드_집계가_진행_중일_때_동기_집계는_로그를_남기고_진행_중_에러_코드로_거절한다() {
        let f = fixture(false, false);
        f.runner.start(admin(), yesterday()).unwrap();

        let error = f.runner.run(yesterday()).unwrap_err();

        assert_eq!(
            code(&error),
            ErrorCode::USER_METRICS_AGGREGATION_IN_PROGRESS
        );
        assert!(f.repository.post_author_calls.lock().unwrap().is_empty());
        let events = f.lines.events();
        // 요청 로그 하나와 실패 로그 하나
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["level"], "ERROR");
        assert_eq!(events[1]["type"], "error");
    }

    // UserMetricsAggregationRunnerTest#start_concurrentRequests_onlyOneAggregates
    #[test]
    fn 요청이_동시에_시작하면_하나만_집계하고_나머지는_진행_중_에러_코드로_거절한다() {
        const REQUESTS: usize = 8;
        let f = fixture(false, false);
        let barrier = Arc::new(std::sync::Barrier::new(REQUESTS));
        let rejected = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let handles: Vec<_> = (0..REQUESTS)
            .map(|_| {
                let (runner, barrier, rejected) = (
                    f.runner.clone(),
                    Arc::clone(&barrier),
                    Arc::clone(&rejected),
                );
                std::thread::spawn(move || {
                    barrier.wait();
                    if let Err(error) = runner.start(admin(), yesterday()) {
                        assert_eq!(
                            error.code(),
                            ErrorCode::USER_METRICS_AGGREGATION_IN_PROGRESS
                        );
                        rejected.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(f.executor.len(), 1);
        assert_eq!(rejected.load(Ordering::SeqCst), REQUESTS - 1);
    }

    #[test]
    fn 스레드_실행기는_task_번호_이름의_스레드에서_돌린다() {
        let executor = ThreadTaskExecutor::default();
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..2 {
            let tx = tx.clone();
            executor
                .execute(Box::new(move || {
                    tx.send(std::thread::current().name().map(str::to_owned))
                        .unwrap();
                }))
                .unwrap();
        }
        let mut names: Vec<_> = (0..2).map(|_| rx.recv().unwrap().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["task-1", "task-2"]);
    }
}
