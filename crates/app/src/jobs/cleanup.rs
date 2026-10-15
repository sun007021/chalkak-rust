//! 정리 작업 4종의 본문. 원본 `RefreshTokenCleanupScheduler`, `ConsumedSignupTokenCleanupScheduler`,
//! `NotificationCleanupScheduler`, `PendingAppleAuthorizationCleanupScheduler`의 `@Scheduled` 메서드와 같다.
//!
//! 스케줄(언제 부를지)은 [`super`]가 정한다. 여기에는 시각을 받아 쿼리를 실행하는 함수만 둔다.

use std::time::Duration;

use chalkak_core::ShutdownSignal;
use chalkak_core::log::Logger;
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};

use crate::admin::auth::repository::AdminRefreshTokenRepository;
use crate::auth::apple::AppleTokenClient;
use crate::auth::repository::{
    ConsumedSignupTokenRepository, PendingAppleAuthorizationRepository, RefreshTokenRepository,
};
use crate::auth::tokens::AppleCipher;
use crate::common::transact;
use crate::notification::cleanup::{Deleted, delete_expired_notifications};
use crate::web::ApiError;

/// 폐기된 행을 남겨 두는 기간(`RefreshTokenCleanupScheduler.REVOKED_RETENTION`). 탈취로 계보를 끊은 직후에는 어떤 토큰이
/// 언제 회전·폐기됐는지가 사고를 들여다보는 유일한 기록이라 일주일은 남긴다.
pub const REVOKED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 폐기 실패를 남기는 로거 이름(원본 `PendingAppleAuthorizationCleanupScheduler`의 `@Slf4j`).
const PENDING_APPLE_LOGGER: &str =
    "com.chalkak.backend.auth.infrastructure.infra.apple.PendingAppleAuthorizationCleanupScheduler";

fn minus(now: Timestamp, retention: Duration) -> Timestamp {
    now.checked_sub(
        SignedDuration::try_from(retention).expect("보관 기간은 SignedDuration 범위 안이다"),
    )
    .expect("보관 기간을 뺀 시각은 표현 범위 안이다")
}

/// `RefreshTokenCleanupScheduler.deleteUnusableTokens`(`@Transactional`): 회원·관리자 토큰을 같은 트랜잭션에서 지운다.
/// 지운 (회원, 관리자) 토큰 수를 돌려준다.
pub fn delete_unusable_refresh_tokens(pool: &Pool, now: Timestamp) -> Result<(u64, u64), ApiError> {
    let revoked_threshold = minus(now, REVOKED_RETENTION);
    transact(pool, |tx| {
        let users = RefreshTokenRepository.delete_unusable_before(tx, now, revoked_threshold)?;
        let admins =
            AdminRefreshTokenRepository.delete_unusable_before(tx, now, revoked_threshold)?;
        Ok((users, admins))
    })
}

/// `ConsumedSignupTokenCleanupScheduler.deleteExpiredTokens`(`@Transactional`).
pub fn delete_expired_signup_tokens(pool: &Pool, now: Timestamp) -> Result<u64, ApiError> {
    transact(pool, |tx| {
        Ok(ConsumedSignupTokenRepository.delete_all_expired_before(tx, now)?)
    })
}

/// `NotificationCleanupScheduler.deleteExpiredNotifications`(`@Transactional`).
pub fn delete_expired_notification_rows(pool: &Pool, now: Timestamp) -> Result<Deleted, ApiError> {
    transact(pool, |tx| Ok(delete_expired_notifications(tx, now)?))
}

/// 만료된 임시 Apple 인증 정보를 폐기하고 지운 개수를 돌려준다
/// (`PendingAppleAuthorizationCleanupScheduler.revokeAndDeleteExpiredAuthorizations`, 트랜잭션 없음).
///
/// 만료 목록을 뽑은 뒤 폐기까지 시간이 걸리므로 그 사이 만료가 연장된 행도 목록에 남아 폐기된다. 연장 트랜잭션이 만료 시각을
/// 걸치는 수 ms에 이 조회가 겹치고 그 사이 가입까지 끝나야 하므로 원본처럼 감수한다(주기를 크게 줄이거나 보관 기간을
/// 짧게 바꾸면 다시 검토한다).
///
/// 폐기(HTTP)는 DB 연결을 쥔 채 하지 않는다: 목록을 읽고 연결을 돌려준 뒤, 행마다 폐기하고 새 연결로 지운다. 한 행이
/// 실패하면(복호화·폐기 오류) 경고를 남기고 그 행은 남긴 채 다음 행으로 간다. 종료 신호가 오면 남은 행은 건드리지 않고
/// 멈춘다(원본은 종료 때 실행 중인 작업을 인터럽트하고 기다리지 않는다. 남은 행은 다음 기동의 주기 실행이 처리한다).
pub fn revoke_and_delete_expired_authorizations(
    pool: &Pool,
    now: Timestamp,
    cipher: &AppleCipher,
    client: &dyn AppleTokenClient,
    logger: &Logger,
    shutdown: &ShutdownSignal,
) -> Result<u64, ApiError> {
    let repository = PendingAppleAuthorizationRepository;
    let expired = {
        let mut conn = pool.get()?;
        repository.find_all_expired_at_or_before(&mut conn, now)?
    };
    let mut deleted = 0;
    for authorization in expired {
        if shutdown.is_triggered() {
            break;
        }
        let outcome = (|| -> Result<(), ApiError> {
            let refresh_token = cipher
                .decrypt(&authorization.encrypted_refresh_token)
                .map_err(ApiError::Internal)?;
            client.revoke_refresh_token(&refresh_token)?;
            let mut conn = pool.get()?;
            repository.delete(&mut conn, authorization.id)?;
            Ok(())
        })();
        match outcome {
            Ok(()) => deleted += 1,
            Err(error) => logger
                .warn(PENDING_APPLE_LOGGER)
                .error(&super::JobFailure::of(&error))
                .log(&format!(
                    "만료된 임시 Apple 인증 정보 폐기에 실패했습니다. id={}",
                    authorization.id
                )),
        }
    }
    Ok(deleted)
}
