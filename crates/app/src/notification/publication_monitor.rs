//! 발행 대기 모니터(원본 `NotificationPublicationMonitor`). Relay가 멈추거나 오래 걸려도 관측할 수 있도록 PENDING 건수와
//! 가장 오래된 대기의 나이를 주기적으로 로그에 남긴다. 60초마다(시작 즉시 한 번) [`crate::jobs`]가 부른다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::{DbError, Pool};

use super::repository::{NotificationRepository, PublicationBacklog};
use crate::common::transact;
use crate::web::ApiError;

const MONITOR_LOGGER: &str =
    "com.chalkak.backend.notification.infrastructure.infra.NotificationPublicationMonitor";
const TYPE_NOTIFICATION: &str = "notification";
/// 조회 제한 시간(원본 `@Transactional(timeout = 5)`).
const QUERY_TIMEOUT_MS: u32 = 5_000;

#[derive(Clone)]
pub struct NotificationPublicationMonitor {
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
}

impl NotificationPublicationMonitor {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>, logger: Arc<Logger>) -> Self {
        Self {
            pool,
            clock,
            logger,
        }
    }

    /// `logBacklog`
    pub fn log_backlog(&self) {
        match self.read_backlog() {
            Ok(backlog) => self
                .logger
                .info(MONITOR_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "publication_snapshot")
                .field("pendingCount", backlog.pending_count)
                .field("oldestPendingAgeSeconds", self.age_seconds(&backlog))
                .log("알림 발행 대기 상태"),
            // 조회 실패를 대기 0건으로 기록하면 정상으로 오인할 수 있다.
            Err(error) => self
                .logger
                .error(MONITOR_LOGGER)
                .field("type", TYPE_NOTIFICATION)
                .field("stage", "publication_snapshot_failed")
                .field("errorCode", error.exception_name())
                .log("알림 발행 대기 조회 실패"),
        }
    }

    fn read_backlog(&self) -> Result<PublicationBacklog, ApiError> {
        transact(&self.pool, |tx| {
            tx.batch_execute(&format!("SET LOCAL statement_timeout = {QUERY_TIMEOUT_MS}"))
                .map_err(DbError::from)?;
            Ok(NotificationRepository.get_publication_backlog(tx)?)
        })
    }

    /// 가장 오래된 대기의 나이(초). 대기가 없거나 생성 시각이 지금보다 뒤면 0.
    fn age_seconds(&self, backlog: &PublicationBacklog) -> i64 {
        backlog.oldest_created_at.map_or(0, |created| {
            self.clock.now().duration_since(created).as_secs().max(0)
        })
    }
}

#[cfg(test)]
#[path = "publication_monitor_tests.rs"]
mod tests;
