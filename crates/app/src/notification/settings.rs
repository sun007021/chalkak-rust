//! 알림 수신 설정. 원본 `NotificationSettingsService`와 `NotificationSettingsResult`.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Pool;
use uuid::Uuid;

use crate::common::transact;
use crate::user::UserRepository;
use crate::web::ApiError;

/// `NotificationSettingsResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationSettingsResult {
    pub topic_push_enabled: bool,
    pub moderation_push_enabled: bool,
}

fn invalid_credentials() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 인증 정보입니다.",
    ))
}

#[derive(Clone)]
pub struct NotificationSettingsService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    users: UserRepository,
}

impl NotificationSettingsService {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>) -> NotificationSettingsService {
        NotificationSettingsService {
            pool,
            clock,
            users: UserRepository,
        }
    }

    /// `getSettings`(읽기 전용 트랜잭션): 없는 회원과 탈퇴 회원은 401.
    pub fn get_settings(&self, user_id: Uuid) -> Result<NotificationSettingsResult, ApiError> {
        let mut conn = self.pool.get()?;
        let user = self
            .users
            .find_by_id(&mut conn, user_id)?
            .ok_or_else(invalid_credentials)?;
        user.validate_not_withdrawn()?;
        Ok(NotificationSettingsResult {
            topic_push_enabled: user.topic_push_enabled,
            moderation_push_enabled: user.moderation_push_enabled,
        })
    }

    /// `updateSettings`: 회원 행을 잠그고 전달한 값만 바꾼다(`null`은 기존 값 유지). 값이 그대로면 `UPDATE`하지 않는다.
    /// 서로 다른 설정을 동시에 바꾸는 두 요청은 행 잠금으로 줄을 서서 두 변경이 모두 남는다.
    pub fn update_settings(
        &self,
        user_id: Uuid,
        topic_push_enabled: Option<bool>,
        moderation_push_enabled: Option<bool>,
    ) -> Result<(), ApiError> {
        transact(&self.pool, |tx| {
            let before = self
                .users
                .find_by_id_for_update(tx, user_id)?
                .ok_or_else(invalid_credentials)?;
            let mut after = before.clone();
            after.update_push_preferences(
                topic_push_enabled.unwrap_or(before.topic_push_enabled),
                moderation_push_enabled.unwrap_or(before.moderation_push_enabled),
            )?;
            self.users
                .update_if_changed(tx, &before, &after, self.clock.now())?;
            Ok(())
        })
    }
}
