//! 사용자 활동 지표 집계(원본 `analytics` 패키지).
//!
//! 매일 새벽 1시(KST)에 전날의 DAU·WAU·게시물 수·유지율을 DB에서 세고, 접근 로그 지표(방문자·요청 수·지연 등)는
//! CloudWatch Logs Insights에서 세어 `chalkak.aggregation` 로그 한 줄로 남긴다. 지표 알람과 대시보드가 이 줄을 읽는다.
//! 관리자는 `POST /api/v1/admin/analytics/aggregations`로 끝난 날짜를 다시 집계할 수 있다.
//!
//! | 모듈 | 원본 |
//! | --- | --- |
//! | [`settings`] | `AnalyticsProperties`, `chalkak.analytics.*` 설정(팀 계정 제외 목록 포함) |
//! | [`repository`] | `UserMetricsRepository`, `UserMetricsRepositoryImpl`(네이티브 SQL 집계) |
//! | [`service`] | `UserMetricsAggregationService`, `DailyUserMetrics`, `AccessLogMetrics`, `WeeklyRetention` |
//! | [`runner`] | `UserMetricsAggregationRunner`(동시 집계 방지) |
//! | [`cloudwatch`] | `CloudWatchLogsAccessLogMetricsReader`, `CloudWatchLogsConfig` |

pub mod cloudwatch;
#[cfg(test)]
mod db_tests;
pub mod repository;
pub mod runner;
pub mod service;
pub mod settings;
#[cfg(test)]
pub(crate) mod testing;

pub use repository::PgUserMetricsRepository;
pub use runner::{TaskExecutor, ThreadTaskExecutor, UserMetricsAggregationRunner};
pub use service::{
    AccessLogMetrics, AccessLogMetricsReader, DailyUserMetrics, UserMetricsAggregationService,
    UserMetricsRepository, WeeklyRetention,
};
pub use settings::AnalyticsSettings;

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::Pool;

/// 서버 조립에서 쓰는 러너 생성. 스케줄러와 관리자 API가 같은 러너(같은 진행 중 표시)를 써야 한다.
pub fn build_runner(
    settings: &AnalyticsSettings,
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
) -> Result<Arc<UserMetricsAggregationRunner>, String> {
    let repository = Arc::new(repository::PgUserMetricsRepository::new(
        pool,
        settings.excluded_user_ids.clone(),
    ));
    let reader = Arc::new(
        cloudwatch::CloudWatchLogsAccessLogMetricsReader::from_settings(
            settings,
            Arc::clone(&clock),
        )?,
    );
    let service = Arc::new(UserMetricsAggregationService::new(
        repository,
        reader,
        Arc::clone(&logger),
    ));
    Ok(Arc::new(UserMetricsAggregationRunner::new(
        service,
        Arc::new(ThreadTaskExecutor::default()),
        clock,
        logger,
    )))
}

/// 접근 로그 지표가 없는 러너. CloudWatch를 부르지 않는 시험과 로컬 조립이 쓴다.
pub fn build_runner_without_access_logs(
    excluded_user_ids: Vec<uuid::Uuid>,
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
) -> Arc<UserMetricsAggregationRunner> {
    struct NoAccessLogs;
    impl AccessLogMetricsReader for NoAccessLogs {
        fn read(
            &self,
            _: jiff::civil::Date,
        ) -> Result<Option<AccessLogMetrics>, crate::web::ApiError> {
            Ok(None)
        }
    }
    let repository = Arc::new(repository::PgUserMetricsRepository::new(
        pool,
        excluded_user_ids,
    ));
    let service = Arc::new(UserMetricsAggregationService::new(
        repository,
        Arc::new(NoAccessLogs),
        Arc::clone(&logger),
    ));
    Arc::new(UserMetricsAggregationRunner::new(
        service,
        Arc::new(ThreadTaskExecutor::default()),
        clock,
        logger,
    ))
}
