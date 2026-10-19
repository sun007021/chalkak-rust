//! 알림 도메인. 원본 `com.chalkak.backend.notification` 패키지.
//!
//! - 알림함(`/api/v1/notifications/**`): [`api`], [`service`], [`repository`].
//! - 검수 결과 알림 생성: [`create_for_moderation`](원본 `UserNotificationService.createForModeration`). 관리자 검수
//!   트랜잭션 안에서 호출한다.
//! - 관리자 Slack 알림(검수 대기): [`slack`].
//! - 보관 기간이 지난 알림 정리 쿼리: [`cleanup`]. 스케줄은 주기 작업 이슈에서 건다.
//! - 공지([`notice`])와 예약 시각이 된 공지의 알림함 배포([`distribution`]), 주제 푸시 발송 시간표([`topic_push_schedule`]).

pub mod api;
pub mod cleanup;
pub mod distribution;
#[cfg(test)]
mod distribution_tests;
pub mod domain;
pub mod notice;
pub mod repository;
pub mod service;
pub mod slack;
pub mod topic_push_schedule;

use std::fmt;

use chalkak_core::error::AppError;
use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::Timestamp;
use uuid::Uuid;

use self::domain::Notification;
use self::repository::NotificationRepository;
use crate::web::ApiError;

/// 검수 결과(`ModerationStatus`의 승인·반려).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModerationDecision {
    Approved,
    Rejected,
}

/// [`create_for_moderation`]의 실패.
#[derive(Debug)]
pub enum NotificationError {
    /// 도메인 규칙 위반(반려 알림에 사유가 없음 등). 원본 `BusinessException`.
    Business(AppError),
    Db(DbError),
}

impl fmt::Display for NotificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotificationError::Business(e) => write!(f, "{}", e.message),
            NotificationError::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for NotificationError {}

impl From<DbError> for NotificationError {
    fn from(e: DbError) -> Self {
        NotificationError::Db(e)
    }
}

impl From<postgres::Error> for NotificationError {
    fn from(e: postgres::Error) -> Self {
        NotificationError::Db(DbError::Postgres(e))
    }
}

impl From<NotificationError> for ApiError {
    fn from(e: NotificationError) -> Self {
        match e {
            NotificationError::Business(e) => ApiError::App(e),
            NotificationError::Db(e) => ApiError::from(e),
        }
    }
}

/// `UserNotificationService.createForModeration`: 검수 결과를 작성자의 알림함에 넣는다.
///
/// 원본은 `Propagation.MANDATORY`라 열린 트랜잭션 안에서만 부른다. `event_key`는 같은 트랜잭션에 쓴 관리자 감사 로그의 식별자로,
/// `(user_id, event_key)`가 유일해 같은 검수 사건의 알림은 한 번만 들어간다(중복이면 DB 제약 오류).
/// `rejection_reason`은 승인일 때 무시하고 반려일 때 비어 있으면 안 된다.
pub fn create_for_moderation(
    tx: &mut impl Db,
    user_id: Uuid,
    post_id: Uuid,
    event_key: Uuid,
    decision: ModerationDecision,
    rejection_reason: Option<&str>,
    occurred_at: Timestamp,
) -> Result<(), NotificationError> {
    let mut notification = match decision {
        ModerationDecision::Approved => {
            Notification::approved(user_id, post_id, event_key, occurred_at)
        }
        ModerationDecision::Rejected => {
            Notification::rejected(user_id, post_id, event_key, rejection_reason, occurred_at)
                .map_err(NotificationError::Business)?
        }
    };
    NotificationRepository.save(tx, &mut notification)?;
    Ok(())
}
