//! 공지 배포(원본 `NoticeDistributionService`). 예약 시각이 지난 공지를 회원 알림함에 발행 대기 알림으로 배포한다.
//! 언제 부를지는 [`crate::jobs`]가 정한다(`NoticeDistributionScheduler`, 매분 0초).
//!
//! 한 번 실행하는 동안 대상 공지를 묶음(10건) 단위로 끝까지 시도한다. 한 공지의 배포는 알림 생성과 배포 완료 표시를 한
//! 트랜잭션에 묶어 공지당 한 번만 일어나고, 실패한 공지는 배포되지 않은 채 남아 다음 실행에서 다시 시도한다. 실패한 공지가
//! 묶음 앞을 막아도 커서가 넘어가므로 뒤 공지를 배포한다.

use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::Pool;
use uuid::Uuid;

use super::notice::{DistributableNotice, NoticeRepository};
use crate::common::transact;
use crate::web::ApiError;

const BATCH_SIZE: usize = 10;

const SERVICE_LOGGER: &str = "com.chalkak.backend.notification.service.NoticeDistributionService";
const TYPE_NOTIFICATION: &str = "notification";

/// `NoticeDistributionService.distributeNotices`. 대상 조회가 실패하면 오류를 돌려준다(공지별 실패는 로그만 남기고 잇는다).
pub fn distribute_notices(pool: &Pool, clock: &dyn Clock, logger: &Logger) -> Result<(), ApiError> {
    let repository = NoticeRepository;
    let now = clock.now();
    let mut cursor: Option<DistributableNotice> = None;
    loop {
        let targets = {
            let mut conn = pool.get()?;
            repository.find_distributable_notices(
                &mut conn,
                now,
                cursor,
                i64::try_from(BATCH_SIZE).expect("묶음 크기는 i64 범위 안이다"),
            )?
        };
        for target in &targets {
            distribute_safely(pool, clock, logger, target.notice_id);
        }
        match targets.last() {
            Some(last) if targets.len() >= BATCH_SIZE => cursor = Some(*last),
            _ => return Ok(()),
        }
    }
}

fn distribute_safely(pool: &Pool, clock: &dyn Clock, logger: &Logger, notice_id: Uuid) {
    // 실패한 공지는 배포되지 않은 채 남아 다음 확인에서 다시 시도한다.
    if let Err(error) = distribute(pool, clock, logger, notice_id) {
        logger
            .error(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "notice_distribution_failed")
            .field("noticeId", notice_id.to_string())
            .field("errorCode", error.exception_name())
            .log("공지 배포 실패");
    }
}

/// 회원별 알림 생성과 배포 완료 표시를 한 트랜잭션으로 묶어 공지당 한 번만 배포한다.
fn distribute(
    pool: &Pool,
    clock: &dyn Clock,
    logger: &Logger,
    notice_id: Uuid,
) -> Result<(), ApiError> {
    let now = clock.now();
    let repository = NoticeRepository;
    let recipient_count = transact(pool, |tx| {
        let Some(mut notice) = repository.find_distributable_for_update(tx, notice_id, now)? else {
            return Ok(None);
        };
        let count = repository.create_notice_notifications(tx, &notice, now)?;
        notice.distribute(now)?;
        repository.save_and_flush(tx, &mut notice, now)?;
        Ok(Some(count))
    })?;
    if let Some(count) = recipient_count {
        logger
            .info(SERVICE_LOGGER)
            .field("type", TYPE_NOTIFICATION)
            .field("stage", "notice_distributed")
            .field("noticeId", notice_id.to_string())
            .field(
                "recipientCount",
                i64::try_from(count).expect("수신자 수는 i64 범위 안이다"),
            )
            .log("공지 배포 완료");
    }
    Ok(())
}
