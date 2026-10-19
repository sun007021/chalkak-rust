//! 주기 작업: 정리 4종과 런타임 상태 로그. 원본 `SchedulingConfig`와 `@Scheduled` 컴포넌트들.
//!
//! | 작업 | 주기(Asia/Seoul) | 끄는 키 |
//! | --- | --- | --- |
//! | 리프레시 토큰 정리 | 매일 04:30 | `chalkak.auth.refresh-token.cleanup.enabled` |
//! | 회원가입 토큰 소진 기록 정리 | 매일 04:40 | `chalkak.auth.consumed-signup-token.cleanup.enabled` |
//! | 알림 정리 | 매일 04:50 | `chalkak.notification.cleanup.enabled` |
//! | 임시 Apple 인증 정보 폐기·삭제 | 매시 정각 | `chalkak.auth.pending-apple-authorization.cleanup.enabled` |
//! | 사용자 지표 집계 | 매일 01:00 | `chalkak.analytics.aggregation.enabled`(기본 꺼짐, prod만 켬) |
//! | 런타임 상태 로그 | 60초마다(시작 즉시 한 번) | `chalkak.monitoring.runtime-log.enabled` |
//!
//! 스케줄링 기능 전체를 끄는 키는 `chalkak.scheduling.enabled`(기본 켜짐)이고 작업별 키와 서로 독립이다. 환경변수
//! 이름은 Spring 느슨한 바인딩을 따라 하이픈을 없애 `CHALKAK_AUTH_REFRESHTOKEN_CLEANUP_ENABLED`처럼 쓴다.
//! 런타임 상태 로그는 원본처럼 기본 꺼짐이고 dev·prod 프로필에서만 켠다(`application-dev.yml`, `application-prod.yml`).
//!
//! 실행은 `chalkak-core`의 [`Scheduler`]가 한다. 원본의 풀 크기 4 스레드 풀 대신 작업마다 스레드 하나를 쓴다. 같은
//! 작업이 겹쳐 실행되지는 않고(앞 실행이 끝나야 다음 실행), 서로 다른 작업은 시각이 겹치지 않거나(04:30·04:40·04:50·
//! 정각) 가벼워서 풀 크기 4와 결과가 다르지 않다. 서버가 종료하면 새 실행을 시작하지 않고 실행 중인 작업이 끝나기를
//! 기다린다([`Jobs::join`]).

pub mod cleanup;
pub mod runtime;

use std::sync::Arc;
use std::time::Duration;

use chalkak_core::config::{ConfigError, Env};
use chalkak_core::log::Logger;
use chalkak_core::scheduler::{CronSchedule, SEOUL, Scheduler};
use chalkak_core::{Clock, ShutdownSignal};
use chalkak_db::Pool;

use crate::analytics::UserMetricsAggregationRunner;
use crate::auth::apple::AppleTokenClient;
use crate::auth::tokens::AppleCipher;

/// 실패한 예약 작업을 남기는 로거(Spring `TaskUtils$LoggingErrorHandler`)와 메시지.
const ERROR_LOGGER: &str = "org.springframework.scheduling.support.TaskUtils$LoggingErrorHandler";
const ERROR_MESSAGE: &str = "Unexpected error occurred in scheduled task";

/// 런타임 상태 로그 주기(`@Scheduled(fixedRate = 60_000)`).
pub const RUNTIME_LOG_PERIOD: Duration = Duration::from_secs(60);

/// 주기 작업 하나.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    RefreshTokenCleanup,
    ConsumedSignupTokenCleanup,
    NotificationCleanup,
    PendingAppleAuthorizationCleanup,
    UserMetricsAggregation,
    RuntimeSnapshot,
}

/// 작업이 도는 때.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Spring 6필드 cron, 시간대는 Asia/Seoul.
    Cron(&'static str),
    /// 시작 즉시 한 번, 이후 시작 시각 기준으로 이 간격마다.
    FixedRate(Duration),
}

impl Job {
    pub const ALL: [Job; 6] = [
        Job::RefreshTokenCleanup,
        Job::ConsumedSignupTokenCleanup,
        Job::NotificationCleanup,
        Job::PendingAppleAuthorizationCleanup,
        Job::UserMetricsAggregation,
        Job::RuntimeSnapshot,
    ];

    /// 스레드 이름·오류 로그에 쓰는 이름(원본 메서드 이름).
    pub fn name(self) -> &'static str {
        match self {
            Job::RefreshTokenCleanup => "deleteUnusableTokens",
            Job::ConsumedSignupTokenCleanup => "deleteExpiredTokens",
            Job::NotificationCleanup => "deleteExpiredNotifications",
            Job::PendingAppleAuthorizationCleanup => "revokeAndDeleteExpiredAuthorizations",
            Job::UserMetricsAggregation => "aggregateYesterday",
            Job::RuntimeSnapshot => "logSnapshot",
        }
    }

    pub fn schedule(self) -> Schedule {
        match self {
            Job::RefreshTokenCleanup => Schedule::Cron("0 30 4 * * *"),
            Job::ConsumedSignupTokenCleanup => Schedule::Cron("0 40 4 * * *"),
            Job::NotificationCleanup => Schedule::Cron("0 50 4 * * *"),
            Job::PendingAppleAuthorizationCleanup => Schedule::Cron("0 0 * * * *"),
            Job::UserMetricsAggregation => Schedule::Cron("0 0 1 * * *"),
            Job::RuntimeSnapshot => Schedule::FixedRate(RUNTIME_LOG_PERIOD),
        }
    }

    /// cron 작업의 판정기. fixed-rate 작업이면 `None`.
    pub fn cron(self) -> Option<CronSchedule> {
        match self.schedule() {
            Schedule::Cron(expression) => Some(
                CronSchedule::parse(expression, SEOUL)
                    .expect("원본 cron 식은 파싱되는 값이다(시험이 확인한다)"),
            ),
            Schedule::FixedRate(_) => None,
        }
    }
}

/// 작업 켜기·끄기 설정.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobsSettings {
    /// `chalkak.scheduling.enabled`: 꺼져 있으면 어떤 작업도 돌지 않는다.
    pub scheduling_enabled: bool,
    pub refresh_token_cleanup: bool,
    pub consumed_signup_token_cleanup: bool,
    pub notification_cleanup: bool,
    pub pending_apple_authorization_cleanup: bool,
    /// `chalkak.analytics.aggregation.enabled`.
    pub user_metrics_aggregation: bool,
    pub runtime_log: bool,
}

impl JobsSettings {
    /// `runtime_log_default`: 프로필이 정하는 `chalkak.monitoring.runtime-log.enabled`의 기본값(local false, dev·prod true).
    /// `aggregation_default`: 프로필이 정하는 `chalkak.analytics.aggregation.enabled`의 기본값(prod만 true).
    pub fn from_env(
        env: &Env,
        runtime_log_default: bool,
        aggregation_default: bool,
    ) -> Result<Self, ConfigError> {
        Ok(JobsSettings {
            scheduling_enabled: env.bool_or("CHALKAK_SCHEDULING_ENABLED", true)?,
            refresh_token_cleanup: env
                .bool_or("CHALKAK_AUTH_REFRESHTOKEN_CLEANUP_ENABLED", true)?,
            consumed_signup_token_cleanup: env
                .bool_or("CHALKAK_AUTH_CONSUMEDSIGNUPTOKEN_CLEANUP_ENABLED", true)?,
            notification_cleanup: env.bool_or("CHALKAK_NOTIFICATION_CLEANUP_ENABLED", true)?,
            pending_apple_authorization_cleanup: env.bool_or(
                "CHALKAK_AUTH_PENDINGAPPLEAUTHORIZATION_CLEANUP_ENABLED",
                true,
            )?,
            user_metrics_aggregation: env
                .bool_or("CHALKAK_ANALYTICS_AGGREGATION_ENABLED", aggregation_default)?,
            runtime_log: env
                .bool_or("CHALKAK_MONITORING_RUNTIMELOG_ENABLED", runtime_log_default)?,
        })
    }

    /// 작업 하나가 켜져 있는지(스케줄링 전체 스위치는 보지 않는다. 원본에서 작업 빈은 스위치와 상관없이 남는다).
    pub fn job_enabled(&self, job: Job) -> bool {
        match job {
            Job::RefreshTokenCleanup => self.refresh_token_cleanup,
            Job::ConsumedSignupTokenCleanup => self.consumed_signup_token_cleanup,
            Job::NotificationCleanup => self.notification_cleanup,
            Job::PendingAppleAuthorizationCleanup => self.pending_apple_authorization_cleanup,
            Job::UserMetricsAggregation => self.user_metrics_aggregation,
            Job::RuntimeSnapshot => self.runtime_log,
        }
    }

    /// 실제로 도는 작업. 스케줄링 전체가 꺼져 있으면 비어 있다.
    pub fn planned(&self) -> Vec<Job> {
        if !self.scheduling_enabled {
            return Vec::new();
        }
        Job::ALL
            .into_iter()
            .filter(|job| self.job_enabled(*job))
            .collect()
    }
}

/// Apple 임시 인증 정보 폐기에 필요한 것.
#[derive(Clone)]
pub struct AppleRevocation {
    pub cipher: AppleCipher,
    pub client: Arc<dyn AppleTokenClient>,
}

/// 작업이 쓰는 객체.
#[derive(Clone)]
pub struct JobDeps {
    pub pool: Pool,
    pub clock: Arc<dyn Clock>,
    pub logger: Arc<Logger>,
    /// 임시 Apple 인증 정보 정리가 켜져 있으면 필요하다.
    pub apple: Option<AppleRevocation>,
    /// 사용자 지표 집계가 켜져 있으면 필요하다. 관리자 수동 집계와 같은 러너여야 한다.
    pub analytics: Option<Arc<UserMetricsAggregationRunner>>,
}

/// 돌고 있는 주기 작업들.
pub struct Jobs {
    scheduler: Scheduler,
    started: Vec<Job>,
}

impl Jobs {
    /// 시작한 작업.
    pub fn started(&self) -> &[Job] {
        &self.started
    }

    /// 모든 작업 스레드가 끝나기를 기다린다. 먼저 `shutdown`을 울려야 한다. 실행 중인 작업은 끝까지 마치고(새 실행은
    /// 시작하지 않는다), 임시 Apple 정리는 남은 행을 건드리지 않고 멈춘다.
    pub fn join(self) {
        self.scheduler.join();
    }
}

/// 집계 작업이 집계할 날짜: KST 기준 어제(`LocalDate.now(clock.withZone(KST)).minusDays(1)`).
pub fn yesterday_kst(now: jiff::Timestamp) -> jiff::civil::Date {
    now.to_zoned(crate::analytics::service::KST.clone())
        .date()
        .yesterday()
        .expect("어제 날짜는 표현 가능하다")
}

/// 켜진 작업을 시작한다(스케줄링이 꺼져 있으면 아무것도 시작하지 않는다).
pub fn start(
    deps: &JobDeps,
    settings: &JobsSettings,
    shutdown: &ShutdownSignal,
) -> Result<Jobs, String> {
    let planned = settings.planned();
    let mut scheduler = Scheduler::new(shutdown.clone(), Arc::clone(&deps.clock))
        .with_logger(Arc::clone(&deps.logger));
    for job in &planned {
        register(&mut scheduler, *job, deps, shutdown)?;
    }
    Ok(Jobs {
        scheduler,
        started: planned,
    })
}

fn register(
    scheduler: &mut Scheduler,
    job: Job,
    deps: &JobDeps,
    shutdown: &ShutdownSignal,
) -> Result<(), String> {
    let body = body(job, deps, shutdown)?;
    let name = job.name();
    let result = match job.schedule() {
        Schedule::Cron(_) => {
            scheduler.cron(name, job.cron().expect("cron 작업은 판정기가 있다"), body)
        }
        Schedule::FixedRate(period) => scheduler.fixed_rate(name, period, body),
    };
    result.map_err(|e| format!("주기 작업 {name} 스레드를 시작할 수 없습니다: {e}"))
}

/// 작업 본문. 오류는 Spring처럼 ERROR 로그를 남기고 삼킨다(일정은 계속된다).
fn body(
    job: Job,
    deps: &JobDeps,
    shutdown: &ShutdownSignal,
) -> Result<Box<dyn FnMut() + Send>, String> {
    let pool = deps.pool.clone();
    let clock = Arc::clone(&deps.clock);
    let logger = Arc::clone(&deps.logger);
    Ok(match job {
        Job::RefreshTokenCleanup => Box::new(move || {
            report(
                &logger,
                cleanup::delete_unusable_refresh_tokens(&pool, clock.now()),
            );
        }),
        Job::ConsumedSignupTokenCleanup => Box::new(move || {
            report(
                &logger,
                cleanup::delete_expired_signup_tokens(&pool, clock.now()),
            );
        }),
        Job::NotificationCleanup => Box::new(move || {
            report(
                &logger,
                cleanup::delete_expired_notification_rows(&pool, clock.now()),
            );
        }),
        Job::PendingAppleAuthorizationCleanup => {
            let apple = deps.apple.clone().ok_or_else(|| {
                "임시 Apple 인증 정보 정리에는 Apple 토큰 클라이언트가 필요합니다".to_owned()
            })?;
            let shutdown = shutdown.clone();
            Box::new(move || {
                report(
                    &logger,
                    cleanup::revoke_and_delete_expired_authorizations(
                        &pool,
                        clock.now(),
                        &apple.cipher,
                        apple.client.as_ref(),
                        &logger,
                        &shutdown,
                    ),
                );
            })
        }
        Job::UserMetricsAggregation => {
            let runner = deps
                .analytics
                .clone()
                .ok_or_else(|| "사용자 지표 집계에는 집계 러너가 필요합니다".to_owned())?;
            // 매일 새벽 전날(KST)을 집계한다. 실패는 러너가 로그를 남기고, 지표 알람이 성공 로그가 없는 것을 감지한다.
            Box::new(move || {
                report(&logger, runner.run(yesterday_kst(clock.now())));
            })
        }
        Job::RuntimeSnapshot => {
            let reader = runtime::RuntimeSnapshotReader::for_process(pool);
            Box::new(move || runtime::log_snapshot(&logger, &reader))
        }
    })
}

fn report<T>(logger: &Logger, result: Result<T, crate::web::ApiError>) {
    if let Err(error) = result {
        logger
            .error(ERROR_LOGGER)
            .error(&JobFailure::of(&error))
            .log(ERROR_MESSAGE);
    }
}

/// 로그에 남길 실패 원인. `ApiError`의 `Display`는 응답용 문구라 `Internal`의 원인 설명이 빠지므로 그 설명을 꺼내 담는다.
#[derive(Debug)]
pub(crate) struct JobFailure(String);

impl JobFailure {
    pub(crate) fn of(error: &crate::web::ApiError) -> Self {
        JobFailure(match error {
            crate::web::ApiError::Internal(cause) => cause.clone(),
            other => other.to_string(),
        })
    }
}

impl std::fmt::Display for JobFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JobFailure {}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn settings(pairs: &[(&str, &str)], runtime_log_default: bool) -> JobsSettings {
        let env = Env::from_pairs(pairs.iter().copied());
        JobsSettings::from_env(&env, runtime_log_default, false).unwrap()
    }

    // SchedulingConfigurationTest#schedulingConfiguration_defaultSettings_enablesBoth
    #[test]
    fn default_settings_enable_scheduling_and_every_cleanup() {
        let s = settings(&[], false);

        assert!(s.scheduling_enabled);
        assert_eq!(
            s.planned(),
            vec![
                Job::RefreshTokenCleanup,
                Job::ConsumedSignupTokenCleanup,
                Job::NotificationCleanup,
                Job::PendingAppleAuthorizationCleanup,
            ]
        );
    }

    // SchedulingConfigurationTest#schedulingConfiguration_schedulingDisabled_keepsCleanupBean
    #[test]
    fn scheduling_disabled_runs_nothing_but_keeps_job_flags() {
        let s = settings(&[("CHALKAK_SCHEDULING_ENABLED", "false")], true);

        assert!(s.planned().is_empty());
        assert!(s.job_enabled(Job::RefreshTokenCleanup));
        assert!(s.job_enabled(Job::RuntimeSnapshot));
    }

    // SchedulingConfigurationTest#schedulingConfiguration_cleanupDisabled_keepsSchedulingEnabled
    #[test]
    fn one_cleanup_disabled_keeps_scheduling_and_other_jobs() {
        let s = settings(
            &[("CHALKAK_AUTH_REFRESHTOKEN_CLEANUP_ENABLED", "false")],
            false,
        );

        assert!(s.scheduling_enabled);
        assert!(!s.planned().contains(&Job::RefreshTokenCleanup));
        assert_eq!(s.planned().len(), 3);
    }

    #[test]
    fn each_job_flag_disables_only_its_job() {
        for (key, job) in [
            (
                "CHALKAK_AUTH_REFRESHTOKEN_CLEANUP_ENABLED",
                Job::RefreshTokenCleanup,
            ),
            (
                "CHALKAK_AUTH_CONSUMEDSIGNUPTOKEN_CLEANUP_ENABLED",
                Job::ConsumedSignupTokenCleanup,
            ),
            (
                "CHALKAK_NOTIFICATION_CLEANUP_ENABLED",
                Job::NotificationCleanup,
            ),
            (
                "CHALKAK_AUTH_PENDINGAPPLEAUTHORIZATION_CLEANUP_ENABLED",
                Job::PendingAppleAuthorizationCleanup,
            ),
        ] {
            let s = settings(&[(key, "false")], false);
            let planned = s.planned();
            assert!(!planned.contains(&job), "{key}");
            assert_eq!(planned.len(), 3, "{key}");
        }
    }

    // RuntimeSnapshotLogger의 @ConditionalOnProperty: 기본 꺼짐, 프로필(dev·prod)이 켠다, 환경변수가 덮는다.
    #[test]
    fn runtime_log_follows_profile_default_and_env_override() {
        assert!(
            !settings(&[], false)
                .planned()
                .contains(&Job::RuntimeSnapshot)
        );
        assert!(
            settings(&[], true)
                .planned()
                .contains(&Job::RuntimeSnapshot)
        );
        assert!(
            settings(&[("CHALKAK_MONITORING_RUNTIMELOG_ENABLED", "true")], false)
                .planned()
                .contains(&Job::RuntimeSnapshot)
        );
        assert!(
            !settings(&[("CHALKAK_MONITORING_RUNTIMELOG_ENABLED", "false")], true)
                .planned()
                .contains(&Job::RuntimeSnapshot)
        );
    }

    #[test]
    fn invalid_flag_value_is_a_config_error() {
        let env = Env::from_pairs([("CHALKAK_SCHEDULING_ENABLED", "yes")]);
        assert!(JobsSettings::from_env(&env, false, false).is_err());
    }

    /// 각 cron 작업이 KST(UTC+9) 기준 시각에 맞는지: 직전 초는 같은 실행, 정각에 다음 실행.
    #[test]
    fn cron_jobs_fire_at_kst_wall_clock() {
        // 2026-10-12 19:29:59Z = 2026-10-13 04:29:59 KST.
        let cases = [
            (
                Job::RefreshTokenCleanup,
                "2026-10-12T19:29:59Z",
                "2026-10-12T19:30:00Z",
            ),
            (
                Job::ConsumedSignupTokenCleanup,
                "2026-10-12T19:39:59Z",
                "2026-10-12T19:40:00Z",
            ),
            (
                Job::NotificationCleanup,
                "2026-10-12T19:49:59Z",
                "2026-10-12T19:50:00Z",
            ),
            (
                Job::PendingAppleAuthorizationCleanup,
                "2026-10-12T19:59:59Z",
                "2026-10-12T20:00:00Z",
            ),
        ];
        for (job, before, expected) in cases {
            let cron = job.cron().unwrap();
            assert_eq!(cron.next_after(ts(before)), Some(ts(expected)), "{job:?}");
            // 실행 시각 정각 다음 초부터 찾으면 다음 날(정리 3종) 또는 한 시간 뒤(Apple)다.
            let after = cron.next_after(ts(expected)).unwrap();
            let gap = after.duration_since(ts(expected)).as_secs();
            let want = if job == Job::PendingAppleAuthorizationCleanup {
                3600
            } else {
                86_400
            };
            assert_eq!(gap, want, "{job:?}");
        }
    }

    #[test]
    fn cron_kst_day_boundary_uses_previous_utc_day() {
        // 04:30 KST는 UTC로 전날 19:30이다.
        let cron = Job::RefreshTokenCleanup.cron().unwrap();
        assert_eq!(
            cron.next_after(ts("2026-10-12T19:30:00Z")),
            Some(ts("2026-10-13T19:30:00Z"))
        );
        assert_eq!(
            cron.next_after(ts("2026-10-12T00:00:00Z")),
            Some(ts("2026-10-12T19:30:00Z"))
        );
    }

    // UserMetricsAggregationSchedulerTest#aggregateYesterday_fixedClock_aggregatesYesterdayInKst
    #[test]
    fn aggregation_job_targets_yesterday_in_kst() {
        // UTC 2030-01-02 16:30은 KST 2030-01-03 01:30이다.
        assert_eq!(
            yesterday_kst(ts("2030-01-02T16:30:00Z")).to_string(),
            "2030-01-02"
        );
        // KST 자정 직전(UTC 14:59:59)은 아직 같은 날이다.
        assert_eq!(
            yesterday_kst(ts("2030-01-02T14:59:59Z")).to_string(),
            "2030-01-01"
        );
    }

    // @Scheduled(cron = "0 0 1 * * *", zone = "Asia/Seoul")
    #[test]
    fn aggregation_job_fires_at_one_am_kst() {
        let cron = Job::UserMetricsAggregation.cron().unwrap();
        // 01:00 KST는 UTC 전날 16:00이다.
        assert_eq!(
            cron.next_after(ts("2026-10-12T15:59:59Z")),
            Some(ts("2026-10-12T16:00:00Z"))
        );
        assert_eq!(
            cron.next_after(ts("2026-10-12T16:00:00Z")),
            Some(ts("2026-10-13T16:00:00Z"))
        );
    }

    // UserMetricsAggregationConfigurationTest: 기본 꺼짐, prod 프로필이 켜고, 환경변수가 덮는다.
    #[test]
    fn aggregation_follows_profile_default_and_env_override() {
        let planned = |pairs: &[(&str, &str)], default: bool| {
            let env = Env::from_pairs(pairs.iter().copied());
            JobsSettings::from_env(&env, false, default)
                .unwrap()
                .planned()
        };
        assert!(!planned(&[], false).contains(&Job::UserMetricsAggregation));
        assert!(planned(&[], true).contains(&Job::UserMetricsAggregation));
        assert!(
            planned(&[("CHALKAK_ANALYTICS_AGGREGATION_ENABLED", "true")], false)
                .contains(&Job::UserMetricsAggregation)
        );
        assert!(
            !planned(&[("CHALKAK_ANALYTICS_AGGREGATION_ENABLED", "false")], true)
                .contains(&Job::UserMetricsAggregation)
        );
        // 스케줄링 전체가 꺼져 있으면 집계도 돌지 않는다.
        assert!(planned(&[("CHALKAK_SCHEDULING_ENABLED", "false")], true).is_empty());
    }

    #[test]
    fn runtime_snapshot_is_fixed_rate_sixty_seconds() {
        assert_eq!(
            Job::RuntimeSnapshot.schedule(),
            Schedule::FixedRate(Duration::from_secs(60))
        );
        assert!(Job::RuntimeSnapshot.cron().is_none());
    }
}
