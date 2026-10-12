//! 인증 도메인: 소셜 로그인·가입, 토큰 발급과 회전, 이미지 처리 콜백 인증, `@RequiresExistingUser`·
//! `@RequiresUsableUser`가 쓰는 판정.

pub mod api;
pub mod apple;
pub mod callback;
pub mod domain;
pub mod oidc;
pub mod refresh;
pub mod repository;
pub mod revoker;
pub mod service;
pub mod tokens;

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Pool;

use crate::security::{AuthenticatedUser, login_user};
use crate::user::{UserPresence, UserRepository};
use crate::web::ApiError;

/// `UsableUserPolicy`. 상태는 토큰이 아니라 저장소에서 읽는다.
#[derive(Clone)]
pub struct UsableUserPolicy {
    pool: Pool,
    users: UserRepository,
}

impl UsableUserPolicy {
    pub fn new(pool: Pool) -> Self {
        UsableUserPolicy {
            pool,
            users: UserRepository,
        }
    }

    /// `validateExisting`: 회원이 있고 탈퇴하지 않았는지만 본다(정지는 통과). 없거나 탈퇴했으면 401.
    /// 통과하면 로그인 회원(`@LoginUser`)을 돌려준다.
    pub fn validate_existing(&self) -> Result<AuthenticatedUser, ApiError> {
        let user = login_user()?;
        let mut conn = self.pool.get()?;
        match self.users.presence(&mut *conn, user.user_id)? {
            UserPresence::Present => Ok(user),
            UserPresence::Missing | UserPresence::Withdrawn => Err(ApiError::App(
                AppError::unauthorized(ErrorCode::UNAUTHORIZED, "유효하지 않은 인증 정보입니다."),
            )),
        }
    }

    /// `validateUsable`: 회원이 있고 탈퇴하지 않았으며 차단되지 않아야 한다. 없거나 탈퇴했으면 401, 차단이면 403.
    pub fn validate_usable(&self) -> Result<AuthenticatedUser, ApiError> {
        let user = login_user()?;
        let mut conn = self.pool.get()?;
        let found = self.users.find_by_id(&mut *conn, user.user_id)?;
        let Some(found) = found else {
            return Err(ApiError::App(AppError::unauthorized(
                ErrorCode::UNAUTHORIZED,
                "유효하지 않은 인증 정보입니다.",
            )));
        };
        found.validate_accessible()?;
        Ok(user)
    }
}
