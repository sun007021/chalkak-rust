//! 인증 도메인. 지금은 `@RequiresExistingUser`가 쓰는 판정만 둔다.

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
}
