//! 게시물·좋아요 흐름이 함께 쓰는 보조: 트랜잭션 경계와 회원 상태 판정.
//!
//! 회원 테이블 질의는 이 모듈 안에 직접 둔다(다른 도메인 모듈을 기다리지 않기 위해서다).

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::{Db, Tx};
use chalkak_db::{DbError, Pool};
use uuid::Uuid;

use crate::auth::UsableUserPolicy;
use crate::security::{AuthenticatedUser, login_user};
use crate::web::ApiError;

/// 트랜잭션 안에서 `f`를 실행한다. `Ok`면 커밋, 업무 오류(`ApiError`)든 DB 오류든 `Err`이면 롤백한다.
///
/// 원본 서비스는 `@Transactional`이라 `RuntimeException`(업무 예외 포함)이면 모두 롤백한다. 커밋은 함수가 돌려준 값이
/// 호출자에게 닿기 전에 끝나므로, 커밋 뒤에 할 일(알림)은 호출자가 이 함수가 `Ok`를 돌려준 뒤에 한다.
pub fn in_transaction<T>(
    pool: &Pool,
    f: impl FnOnce(&mut Tx<'_>) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let mut conn = pool.get()?;
    let mut tx = conn.begin().map_err(DbError::from)?;
    match f(&mut tx) {
        Ok(value) => {
            tx.commit().map_err(DbError::from)?;
            Ok(value)
        }
        Err(error) => {
            // 롤백 실패는 원래 오류를 가리지 않는다. 연결이 끊겼다면 반환 시 버려진다.
            let _ = tx.rollback();
            Err(error)
        }
    }
}

pub fn unauthorized() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 인증 정보입니다.",
    ))
}

/// `UserRepository.findActiveById(...).orElseThrow(401)`: 탈퇴하지 않은 회원이어야 한다.
pub fn require_active_user(db: &mut impl Db, user_id: Uuid) -> Result<(), ApiError> {
    let found = db
        .query_opt(
            "SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL",
            &[&user_id],
        )
        .map_err(DbError::from)?;
    if found.is_some() {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

/// 회원 행의 상태(`UsableUserPolicy.getUser`가 읽는 것).
enum Standing {
    Missing,
    Withdrawn,
    Banned,
    Usable,
}

/// `@RequiresUsableUser`/`@RequiresExistingUser`의 판정(`UsableUserPolicy`).
#[derive(Clone)]
pub struct UserGuard {
    pool: Pool,
    existing: UsableUserPolicy,
}

impl UserGuard {
    pub fn new(pool: Pool) -> Self {
        UserGuard {
            existing: UsableUserPolicy::new(pool.clone()),
            pool,
        }
    }

    /// `validateUsable`: 없거나 탈퇴했으면 401, 정지됐으면 403(`차단된 회원입니다.`).
    pub fn validate_usable(&self) -> Result<AuthenticatedUser, ApiError> {
        let user = login_user()?;
        let mut conn = self.pool.get()?;
        let row = conn
            .query_opt(
                "SELECT deleted_at IS NOT NULL AS withdrawn, status::text = 'BANNED' AS banned \
             FROM users WHERE id = $1",
                &[&user.user_id],
            )
            .map_err(DbError::from)?;
        let standing = match row {
            None => Standing::Missing,
            Some(row) => {
                let withdrawn: bool = row.try_get("withdrawn").map_err(DbError::from)?;
                let banned: bool = row.try_get("banned").map_err(DbError::from)?;
                if withdrawn {
                    Standing::Withdrawn
                } else if banned {
                    Standing::Banned
                } else {
                    Standing::Usable
                }
            }
        };
        match standing {
            Standing::Usable => Ok(user),
            Standing::Missing | Standing::Withdrawn => Err(unauthorized()),
            Standing::Banned => Err(ApiError::App(AppError::forbidden(
                ErrorCode::FORBIDDEN,
                "차단된 회원입니다.",
            ))),
        }
    }

    /// `validateExisting`: 회원이 있고 탈퇴하지 않았는지만 본다(정지는 통과).
    pub fn validate_existing(&self) -> Result<AuthenticatedUser, ApiError> {
        self.existing.validate_existing()
    }
}
