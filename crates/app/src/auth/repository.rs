//! 인증 저장소 SQL. 원본 `SocialAccountRepository`, `RefreshTokenRepository`(회원용), `ConsumedSignupTokenRepository`,
//! `PendingAppleAuthorizationRepository`, `AppleAuthorizationRepository`.
//!
//! enum 열(`social_provider`)은 `CAST($n::text AS social_provider)`로 쓰고 `::text`로 읽는다.

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::{GenericClient, Row};
use uuid::Uuid;

use super::domain::{RefreshToken, SocialProvider};
use crate::user::domain::User;
use crate::user::repository::{COLUMNS as USER_COLUMNS, user_from_row};

// ---------------------------------------------------------------------------------------------
// 소셜 계정
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocialAccount {
    pub id: Uuid,
    pub user_id: Uuid,
    pub provider: SocialProvider,
    pub subject_hmac: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SocialAccountRepository;

impl SocialAccountRepository {
    /// `findByProviderAndSubjectHmac`(회원을 함께 읽는다).
    pub fn find_by_provider_and_subject_hmac(
        &self,
        db: &mut impl GenericClient,
        provider: SocialProvider,
        subject_hmac: &str,
    ) -> Result<Option<(SocialAccount, User)>, DbError> {
        let sql = format!(
            "SELECT sa.id AS sa_id, sa.subject_hmac AS sa_subject_hmac, {USER_COLUMNS} \
             FROM social_accounts sa JOIN users u ON u.id = sa.user_id \
             WHERE sa.provider = CAST($1::text AS social_provider) AND sa.subject_hmac = $2"
        );
        db.query_opt(&sql, &[&provider.name(), &subject_hmac])?
            .map(|row| {
                let user = user_from_row(&row)?;
                Ok((
                    SocialAccount {
                        id: row.try_get("sa_id")?,
                        user_id: user.id,
                        provider,
                        subject_hmac: row.try_get("sa_subject_hmac")?,
                    },
                    user,
                ))
            })
            .transpose()
    }

    /// `findByUserId`: 회원의 소셜 계정 식별자.
    pub fn find_id_by_user_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
    ) -> Result<Option<Uuid>, DbError> {
        db.query_opt(
            "SELECT id FROM social_accounts WHERE user_id = $1",
            &[&user_id],
        )?
        .map(|row| row.try_get(0).map_err(DbError::from))
        .transpose()
    }

    /// `save`.
    pub fn insert(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
        provider: SocialProvider,
        subject_hmac: &str,
        now: Timestamp,
    ) -> Result<Uuid, DbError> {
        Ok(db
            .query_one(
                "INSERT INTO social_accounts (user_id, provider, subject_hmac, created_at) \
                 VALUES ($1, CAST($2::text AS social_provider), $3, $4) RETURNING id",
                &[&user_id, &provider.name(), &subject_hmac, &now],
            )?
            .try_get(0)?)
    }

    /// `deleteByUserId`.
    pub fn delete_by_user_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM social_accounts WHERE user_id = $1",
            &[&user_id],
        )?)
    }
}

// ---------------------------------------------------------------------------------------------
// 리프레시 토큰
// ---------------------------------------------------------------------------------------------

const REFRESH_COLUMNS: &str = "id, user_id, session_id, token_hash, expires_at, absolute_expires_at, \
     rotated_at, revoked_at";

fn refresh_from_row(row: &Row) -> Result<RefreshToken, DbError> {
    Ok(RefreshToken {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        session_id: row.try_get("session_id")?,
        token_hash: row.try_get("token_hash")?,
        expires_at: row.try_get("expires_at")?,
        absolute_expires_at: row.try_get("absolute_expires_at")?,
        rotated_at: row.try_get("rotated_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// 저장할 새 리프레시 토큰.
pub struct NewRefreshToken<'a> {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub token_hash: &'a str,
    pub expires_at: Timestamp,
    pub absolute_expires_at: Timestamp,
    pub created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RefreshTokenRepository;

impl RefreshTokenRepository {
    /// 계보 잠금 대상을 고르려고 토큰이 속한 계보만 읽는다(잠금 없음, 판단에 쓰지 않는다).
    pub fn find_session_id_by_token_hash(
        &self,
        db: &mut impl GenericClient,
        token_hash: &str,
    ) -> Result<Option<Uuid>, DbError> {
        db.query_opt(
            "SELECT session_id FROM user_refresh_tokens WHERE token_hash = $1",
            &[&token_hash],
        )?
        .map(|row| row.try_get(0).map_err(DbError::from))
        .transpose()
    }

    /// 한 회원의 살아 있는 계보 식별자(`ORDER BY session_id`).
    pub fn find_live_session_ids_by_user_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
    ) -> Result<Vec<Uuid>, DbError> {
        db.query(
            "SELECT DISTINCT session_id FROM user_refresh_tokens \
             WHERE user_id = $1 AND revoked_at IS NULL ORDER BY session_id",
            &[&user_id],
        )?
        .iter()
        .map(|row| row.try_get(0).map_err(DbError::from))
        .collect()
    }

    /// 계보를 트랜잭션이 끝날 때까지 잠근다(`pg_advisory_xact_lock`).
    pub fn lock_session(
        &self,
        db: &mut impl GenericClient,
        session_id: Uuid,
    ) -> Result<(), DbError> {
        db.query_one(
            "SELECT pg_advisory_xact_lock(hashtext(CAST($1::uuid AS text)))",
            &[&session_id],
        )?;
        Ok(())
    }

    pub fn find_by_token_hash(
        &self,
        db: &mut impl GenericClient,
        token_hash: &str,
    ) -> Result<Option<RefreshToken>, DbError> {
        db.query_opt(
            &format!("SELECT {REFRESH_COLUMNS} FROM user_refresh_tokens WHERE token_hash = $1"),
            &[&token_hash],
        )?
        .map(|row| refresh_from_row(&row))
        .transpose()
    }

    /// `save`(새 토큰).
    pub fn insert(
        &self,
        db: &mut impl GenericClient,
        token: &NewRefreshToken<'_>,
    ) -> Result<(), DbError> {
        db.execute(
            "INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, \
             absolute_expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &token.user_id,
                &token.session_id,
                &token.token_hash,
                &token.expires_at,
                &token.absolute_expires_at,
                &token.created_at,
            ],
        )?;
        Ok(())
    }

    /// 회전 시각을 기록한다.
    pub fn mark_rotated(
        &self,
        db: &mut impl GenericClient,
        id: Uuid,
        rotated_at: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE user_refresh_tokens SET rotated_at = $2 WHERE id = $1 AND rotated_at IS NULL",
            &[&id, &rotated_at],
        )?;
        Ok(())
    }

    /// `deleteUnusableBefore`: 절대 만료가 `now` 이전인 행과 `revoked_threshold`보다 먼저 폐기된 행을 지운다.
    /// 두 조건은 서로 다른 컬럼을 보므로 문장을 나눠 각자의 인덱스를 쓴다(원본 주석). 지운 총 개수를 돌려준다.
    pub fn delete_unusable_before(
        &self,
        db: &mut impl GenericClient,
        now: Timestamp,
        revoked_threshold: Timestamp,
    ) -> Result<u64, DbError> {
        let absolutely_expired = db.execute(
            "DELETE FROM user_refresh_tokens WHERE absolute_expires_at < $1",
            &[&now],
        )?;
        let revoked = db.execute(
            "DELETE FROM user_refresh_tokens WHERE revoked_at < $1",
            &[&revoked_threshold],
        )?;
        Ok(absolutely_expired + revoked)
    }

    /// 계보 하나를 통째로 폐기하고 폐기된 토큰 수를 돌려준다.
    pub fn revoke_session(
        &self,
        db: &mut impl GenericClient,
        session_id: Uuid,
        revoked_at: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "UPDATE user_refresh_tokens SET revoked_at = $2 \
             WHERE session_id = $1 AND revoked_at IS NULL",
            &[&session_id, &revoked_at],
        )?)
    }

    /// 한 회원의 모든 기기 세션을 폐기한다.
    pub fn revoke_all_by_user_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
        revoked_at: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "UPDATE user_refresh_tokens SET revoked_at = $2 \
             WHERE user_id = $1 AND revoked_at IS NULL",
            &[&user_id, &revoked_at],
        )?)
    }
}

// ---------------------------------------------------------------------------------------------
// 회원가입 토큰 소진 기록
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct ConsumedSignupTokenRepository;

impl ConsumedSignupTokenRepository {
    /// `createIfAbsent`: 처음 기록했으면 `true`.
    pub fn consume_if_absent(
        &self,
        db: &mut impl GenericClient,
        jti: &str,
        expires_at: Timestamp,
    ) -> Result<bool, DbError> {
        let inserted = db.execute(
            "INSERT INTO consumed_signup_tokens (jti, expires_at) VALUES ($1, $2) \
             ON CONFLICT (jti) DO NOTHING",
            &[&jti, &expires_at],
        )?;
        Ok(inserted > 0)
    }

    /// `deleteAllExpiredBefore`: 만료 시각이 `now`보다 앞선 기록만 지운다(같으면 남긴다).
    pub fn delete_all_expired_before(
        &self,
        db: &mut impl GenericClient,
        now: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM consumed_signup_tokens WHERE expires_at < $1",
            &[&now],
        )?)
    }
}

// ---------------------------------------------------------------------------------------------
// Apple 임시 보관분·정식 보관분
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAppleAuthorization {
    pub id: Uuid,
    pub subject_hmac: String,
    pub encrypted_refresh_token: String,
    pub expires_at: Timestamp,
}

impl PendingAppleAuthorization {
    /// 만료를 앞으로만 민다(`extendTo`). 바뀌었으면 `true`.
    pub fn extend_to(&mut self, candidate: Timestamp) -> bool {
        if candidate > self.expires_at {
            self.expires_at = candidate;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PendingAppleAuthorizationRepository;

impl PendingAppleAuthorizationRepository {
    /// `findLatestUnexpiredBySubjectHmacForUpdate`.
    pub fn find_latest_unexpired_for_update(
        &self,
        db: &mut impl GenericClient,
        subject_hmac: &str,
        now: Timestamp,
    ) -> Result<Option<PendingAppleAuthorization>, DbError> {
        db.query_opt(
            "SELECT id, subject_hmac, encrypted_refresh_token, expires_at \
             FROM pending_apple_authorizations \
             WHERE subject_hmac = $1 AND expires_at > $2 \
             ORDER BY created_at DESC, id DESC LIMIT 1 FOR NO KEY UPDATE",
            &[&subject_hmac, &now],
        )?
        .map(|row| {
            Ok(PendingAppleAuthorization {
                id: row.try_get("id")?,
                subject_hmac: row.try_get("subject_hmac")?,
                encrypted_refresh_token: row.try_get("encrypted_refresh_token")?,
                expires_at: row.try_get("expires_at")?,
            })
        })
        .transpose()
    }

    /// `findAllExpiredAtOrBefore`: 만료 시각이 `now` 이하인 행을 만료가 이른 순서로.
    pub fn find_all_expired_at_or_before(
        &self,
        db: &mut impl GenericClient,
        now: Timestamp,
    ) -> Result<Vec<PendingAppleAuthorization>, DbError> {
        db.query(
            "SELECT id, subject_hmac, encrypted_refresh_token, expires_at \
             FROM pending_apple_authorizations WHERE expires_at <= $1 ORDER BY expires_at ASC",
            &[&now],
        )?
        .iter()
        .map(|row| {
            Ok(PendingAppleAuthorization {
                id: row.try_get("id")?,
                subject_hmac: row.try_get("subject_hmac")?,
                encrypted_refresh_token: row.try_get("encrypted_refresh_token")?,
                expires_at: row.try_get("expires_at")?,
            })
        })
        .collect()
    }

    pub fn insert(
        &self,
        db: &mut impl GenericClient,
        subject_hmac: &str,
        encrypted_refresh_token: &str,
        expires_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "INSERT INTO pending_apple_authorizations (subject_hmac, encrypted_refresh_token, \
             expires_at, created_at) VALUES ($1, $2, $3, $4)",
            &[&subject_hmac, &encrypted_refresh_token, &expires_at, &now],
        )?;
        Ok(())
    }

    pub fn update_expires_at(
        &self,
        db: &mut impl GenericClient,
        id: Uuid,
        expires_at: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE pending_apple_authorizations SET expires_at = $2 WHERE id = $1",
            &[&id, &expires_at],
        )?;
        Ok(())
    }

    pub fn delete(&self, db: &mut impl GenericClient, id: Uuid) -> Result<(), DbError> {
        db.execute(
            "DELETE FROM pending_apple_authorizations WHERE id = $1",
            &[&id],
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppleAuthorization {
    pub id: Uuid,
    pub encrypted_refresh_token: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AppleAuthorizationRepository;

impl AppleAuthorizationRepository {
    pub fn find_all_by_social_account_id(
        &self,
        db: &mut impl GenericClient,
        social_account_id: Uuid,
    ) -> Result<Vec<AppleAuthorization>, DbError> {
        db.query(
            "SELECT id, encrypted_refresh_token FROM apple_authorizations \
             WHERE social_account_id = $1 ORDER BY id",
            &[&social_account_id],
        )?
        .iter()
        .map(|row| {
            Ok(AppleAuthorization {
                id: row.try_get("id")?,
                encrypted_refresh_token: row.try_get("encrypted_refresh_token")?,
            })
        })
        .collect()
    }

    pub fn insert(
        &self,
        db: &mut impl GenericClient,
        social_account_id: Uuid,
        encrypted_refresh_token: &str,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "INSERT INTO apple_authorizations (social_account_id, encrypted_refresh_token, \
             created_at, updated_at) VALUES ($1, $2, $3, $3)",
            &[&social_account_id, &encrypted_refresh_token, &now],
        )?;
        Ok(())
    }

    pub fn delete_all_by_social_account_id(
        &self,
        db: &mut impl GenericClient,
        social_account_id: Uuid,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM apple_authorizations WHERE social_account_id = $1",
            &[&social_account_id],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PendingAppleAuthorizationTest#extendTo_atCurrentExpiry_movesOnlyForward
    #[test]
    fn extend_to_at_current_expiry_moves_only_forward() {
        let ts = |s: &str| -> Timestamp { s.parse().unwrap() };
        let expires_at = ts("2026-09-04T00:05:00Z");
        let one_ms = jiff::SignedDuration::from_millis(1);
        let mut p = PendingAppleAuthorization {
            id: Uuid::nil(),
            subject_hmac: "a".repeat(64),
            encrypted_refresh_token: "encrypted-refresh-token".into(),
            expires_at,
        };
        p.extend_to(expires_at.checked_sub(one_ms).unwrap());
        assert_eq!(p.expires_at, expires_at);
        p.extend_to(expires_at);
        assert_eq!(p.expires_at, expires_at);
        p.extend_to(expires_at.checked_add(one_ms).unwrap());
        assert_eq!(p.expires_at, expires_at.checked_add(one_ms).unwrap());
    }

    // PendingAppleAuthorization#extendTo: 앞으로만 간다.
    #[test]
    fn 임시_보관분의_만료는_앞으로만_민다() {
        let ts = |s: &str| -> Timestamp { s.parse().unwrap() };
        let mut p = PendingAppleAuthorization {
            id: Uuid::nil(),
            subject_hmac: "a".repeat(64),
            encrypted_refresh_token: "x".into(),
            expires_at: ts("2026-01-01T00:10:00Z"),
        };
        assert!(!p.extend_to(ts("2026-01-01T00:05:00Z")));
        assert!(!p.extend_to(ts("2026-01-01T00:10:00Z")));
        assert_eq!(p.expires_at, ts("2026-01-01T00:10:00Z"));
        assert!(p.extend_to(ts("2026-01-01T00:20:00Z")));
        assert_eq!(p.expires_at, ts("2026-01-01T00:20:00Z"));
    }
}
