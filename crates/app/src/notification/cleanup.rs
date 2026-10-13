//! 보관 기간이 지난 알림 정리. 원본 `NotificationCleanupScheduler.deleteExpiredNotifications`의 본문이다.
//!
//! 스케줄(`0 50 4 * * *`, Asia/Seoul)은 주기 작업 이슈에서 건다. 여기에는 쿼리를 호출하는 함수만 둔다.

use std::time::Duration;

use chalkak_db::DbError;
use jiff::{SignedDuration, Timestamp};
use postgres::GenericClient;

use super::repository::NotificationRepository;

/// 탈퇴하지 않은 회원의 알림 보관 기간.
pub const NORMAL_RETENTION: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// 탈퇴한 회원의 알림 보관 기간(탈퇴 시각부터).
pub const WITHDRAWN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 지운 알림 수.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deleted {
    pub active_users: u64,
    pub withdrawn_users: u64,
}

/// `deleteExpiredNotifications`: 두 삭제를 같은 트랜잭션에서 하라는 뜻으로 `tx`를 받는다(원본은 `@Transactional`).
pub fn delete_expired_notifications(
    tx: &mut impl GenericClient,
    now: Timestamp,
) -> Result<Deleted, DbError> {
    let minus = |d: Duration| {
        now.checked_sub(
            SignedDuration::try_from(d).expect("보관 기간은 SignedDuration 범위 안이다"),
        )
        .expect("보관 기간을 뺀 시각은 표현 범위 안이다")
    };
    let repository = NotificationRepository;
    let active_users = repository.delete_expired_for_active_users(tx, minus(NORMAL_RETENTION))?;
    let withdrawn_users =
        repository.delete_expired_for_withdrawn_users(tx, minus(WITHDRAWN_RETENTION))?;
    Ok(Deleted {
        active_users,
        withdrawn_users,
    })
}
