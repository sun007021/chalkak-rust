//! 회원 리프레시 토큰의 발급과 회전. 원본 `RefreshTokenService`·`UserRefreshTokenService`.
//!
//! - 토큰은 불투명한 난수이고 저장소에는 SHA-256 해시만 남는다.
//! - 회전과 폐기는 계보(`session_id`) 단위로 어드바이저리 잠금을 먼저 잡고, 잠금 아래에서 토큰을 다시 읽어 판단한다.
//! - 재사용(이미 회전된 토큰)과 만료는 계보를 폐기한 뒤 거절한다. 폐기가 롤백되면 탈취된 계보가 살아남으므로 이 거절만
//!   커밋한다(`noRollbackFor = UnauthorizedException`).
//! - 거절 사유는 모두 같은 응답(401 `REAUTHENTICATION_REQUIRED`)으로 묶는다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode, ErrorKind};
use chalkak_db::Db;
use chalkak_db::Pool;
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::{
    GeneratedRefreshToken, IssuedAccessToken, IssuedRefreshToken, RefreshToken, RefreshTokenPolicy,
    validate_new_refresh_token,
};
use super::repository::{NewRefreshToken, RefreshTokenRepository};
use super::tokens::{AccessTokenIssuer, generate_refresh_token, hash_refresh_token};
use crate::common::{transact, transact_unless};
use crate::web::ApiError;

/// `TokenRefreshResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenRefreshResult {
    pub access_token: IssuedAccessToken,
    pub refresh_token: IssuedRefreshToken,
}

fn reauthentication_required() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::REAUTHENTICATION_REQUIRED,
        "다시 로그인해 주세요.",
    ))
}

fn is_unauthorized(error: &ApiError) -> bool {
    matches!(error, ApiError::App(e) if e.kind == ErrorKind::Unauthorized)
}

#[derive(Clone)]
pub struct UserRefreshTokenService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    policy: RefreshTokenPolicy,
    access_tokens: AccessTokenIssuer,
    repository: RefreshTokenRepository,
}

fn expires_in(now: Timestamp, expires_at: Timestamp) -> i64 {
    expires_at.duration_since(now).as_secs()
}

fn issued(
    generated: GeneratedRefreshToken,
    now: Timestamp,
    expires_at: Timestamp,
) -> IssuedRefreshToken {
    IssuedRefreshToken {
        value: generated.value,
        expires_in_seconds: expires_in(now, expires_at),
    }
}

impl UserRefreshTokenService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        policy: RefreshTokenPolicy,
        access_tokens: AccessTokenIssuer,
    ) -> UserRefreshTokenService {
        UserRefreshTokenService {
            pool,
            clock,
            policy,
            access_tokens,
            repository: RefreshTokenRepository,
        }
    }

    /// 새 회전 계보를 시작한다(`issue`). 호출자의 트랜잭션에 참여한다. 기기마다 계보를 나누려고 세션 식별자를 새로
    /// 만든다.
    pub fn issue(&self, db: &mut impl Db, user_id: Uuid) -> Result<IssuedRefreshToken, ApiError> {
        let now = self.clock.now();
        let absolute_expires_at = self.policy.absolute_expires_at(now);
        let expires_at = self.policy.next_expires_at(now, absolute_expires_at);
        let generated = generate_refresh_token();
        validate_new_refresh_token(&generated.token_hash, expires_at, absolute_expires_at)?;
        self.repository.insert(
            db,
            &NewRefreshToken {
                user_id,
                session_id: Uuid::new_v4(),
                token_hash: &generated.token_hash,
                expires_at,
                absolute_expires_at,
                created_at: now,
            },
        )?;
        Ok(issued(generated, now, expires_at))
    }

    /// 제시된 토큰을 소비해 다음 리프레시 토큰과 액세스 토큰을 함께 내준다(`refresh`).
    pub fn refresh(&self, presented_token: &str) -> Result<TokenRefreshResult, ApiError> {
        transact_unless(&self.pool, is_unauthorized, |tx| {
            let consumed = self
                .lock_lineage_and_read(tx, &hash_refresh_token(presented_token))?
                .ok_or_else(reauthentication_required)?;
            let now = self.clock.now();
            if consumed.is_revoked() {
                return Err(reauthentication_required());
            }
            if consumed.is_reused() || consumed.is_expired(now) {
                self.repository
                    .revoke_session(tx, consumed.session_id, now)?;
                return Err(reauthentication_required());
            }
            self.rotate(tx, &consumed, now)
        })
    }

    /// 한 기기의 로그아웃. 모르는 토큰이나 이미 폐기된 토큰에도 오류를 내지 않는다(`logout`).
    pub fn logout(&self, presented_token: &str) -> Result<(), ApiError> {
        transact(&self.pool, |tx| {
            let token = self.lock_lineage_and_read(tx, &hash_refresh_token(presented_token))?;
            if let Some(token) = token.filter(|t| !t.is_revoked()) {
                self.repository
                    .revoke_session(tx, token.session_id, self.clock.now())?;
            }
            Ok(())
        })
    }

    /// 한 회원의 모든 기기 세션을 끊는다(`revokeAll`). 회전과 같은 계보 잠금을 먼저 잡아, 그 사이 커밋된 후속 토큰이
    /// 폐기를 빠져나가지 않게 한다. 호출자의 트랜잭션에 참여한다.
    pub fn revoke_all(&self, db: &mut impl Db, user_id: Uuid) -> Result<(), ApiError> {
        let session_ids = self
            .repository
            .find_live_session_ids_by_user_id(db, user_id)?;
        for session_id in session_ids {
            self.repository.lock_session(db, session_id)?;
        }
        self.repository
            .revoke_all_by_user_id(db, user_id, self.clock.now())?;
        Ok(())
    }

    /// 계보 잠금 -> 잠금 아래에서 다시 읽기. 첫 조회는 잠금 없이 하므로 곧 낡을 수 있고, 판단은 두 번째 조회로 한다.
    fn lock_lineage_and_read(
        &self,
        db: &mut impl Db,
        token_hash: &str,
    ) -> Result<Option<RefreshToken>, ApiError> {
        let Some(session_id) = self
            .repository
            .find_session_id_by_token_hash(db, token_hash)?
        else {
            return Ok(None);
        };
        self.repository.lock_session(db, session_id)?;
        Ok(self.repository.find_by_token_hash(db, token_hash)?)
    }

    fn rotate(
        &self,
        db: &mut impl Db,
        consumed: &RefreshToken,
        now: Timestamp,
    ) -> Result<TokenRefreshResult, ApiError> {
        let generated = generate_refresh_token();
        let expires_at = self
            .policy
            .next_expires_at(now, consumed.absolute_expires_at);
        validate_new_refresh_token(
            &generated.token_hash,
            expires_at,
            consumed.absolute_expires_at,
        )?;
        self.repository.mark_rotated(db, consumed.id, now)?;
        self.repository.insert(
            db,
            &NewRefreshToken {
                user_id: consumed.user_id,
                session_id: consumed.session_id,
                token_hash: &generated.token_hash,
                expires_at,
                absolute_expires_at: consumed.absolute_expires_at,
                created_at: now,
            },
        )?;
        Ok(TokenRefreshResult {
            access_token: self.access_tokens.issue(consumed.user_id),
            refresh_token: issued(generated, now, expires_at),
        })
    }
}
