//! 원본 `AdminRepositoryImpl`·`AdminRefreshTokenRepositoryImpl`: 관리자 계정과 리프레시 토큰 저장.

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::GenericClient;
use uuid::Uuid;

use super::domain::{Admin, RefreshTokenRecord};

#[derive(Clone, Copy, Debug, Default)]
pub struct AdminRepository;

impl AdminRepository {
    pub fn find_by_username(
        &self,
        db: &mut impl GenericClient,
        username: &str,
    ) -> Result<Option<Admin>, DbError> {
        let row = db.query_opt(
            "SELECT id, username, password FROM admins WHERE username = $1",
            &[&username],
        )?;
        row.map(|row| admin_from_row(&row)).transpose()
    }

    pub fn find_by_id(
        &self,
        db: &mut impl GenericClient,
        admin_id: Uuid,
    ) -> Result<Option<Admin>, DbError> {
        let row = db.query_opt(
            "SELECT id, username, password FROM admins WHERE id = $1",
            &[&admin_id],
        )?;
        row.map(|row| admin_from_row(&row)).transpose()
    }

    /// 새 관리자를 넣는다. 시각은 DB 기본값이 채운다.
    pub fn save(&self, db: &mut impl GenericClient, admin: &mut Admin) -> Result<(), DbError> {
        let row = db.query_one(
            "INSERT INTO admins (username, password) VALUES ($1, $2) RETURNING id",
            &[&admin.username, &admin.password_hash],
        )?;
        admin.id = Some(row.try_get("id")?);
        Ok(())
    }
}

fn admin_from_row(row: &postgres::Row) -> Result<Admin, DbError> {
    Ok(Admin {
        id: Some(row.try_get("id")?),
        username: row.try_get("username")?,
        password_hash: row.try_get("password")?,
    })
}

/// 새로 넣을 리프레시 토큰 행.
#[derive(Clone, Debug)]
pub struct NewRefreshToken {
    pub admin_id: Uuid,
    pub session_id: Uuid,
    pub token_hash: String,
    pub expires_at: Timestamp,
    pub absolute_expires_at: Timestamp,
    pub created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AdminRefreshTokenRepository;

impl AdminRefreshTokenRepository {
    pub fn find_session_id_by_token_hash(
        &self,
        db: &mut impl GenericClient,
        token_hash: &str,
    ) -> Result<Option<Uuid>, DbError> {
        let row = db.query_opt(
            "SELECT session_id FROM admin_refresh_tokens WHERE token_hash = $1",
            &[&token_hash],
        )?;
        Ok(row.map(|row| row.try_get(0)).transpose()?)
    }

    /// 계보 하나를 트랜잭션이 끝날 때까지 잠근다(`pg_advisory_xact_lock`).
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
    ) -> Result<Option<RefreshTokenRecord>, DbError> {
        let row = db.query_opt(
            "SELECT id, admin_id, session_id, expires_at, absolute_expires_at, rotated_at, revoked_at \
             FROM admin_refresh_tokens WHERE token_hash = $1",
            &[&token_hash],
        )?;
        row.map(|row| {
            Ok(RefreshTokenRecord {
                id: row.try_get("id")?,
                admin_id: row.try_get("admin_id")?,
                session_id: row.try_get("session_id")?,
                expires_at: row.try_get("expires_at")?,
                absolute_expires_at: row.try_get("absolute_expires_at")?,
                rotated_at: row.try_get("rotated_at")?,
                revoked_at: row.try_get("revoked_at")?,
            })
        })
        .transpose()
    }

    pub fn save(
        &self,
        db: &mut impl GenericClient,
        token: &NewRefreshToken,
    ) -> Result<(), DbError> {
        db.execute(
            "INSERT INTO admin_refresh_tokens \
             (admin_id, session_id, token_hash, expires_at, absolute_expires_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &token.admin_id,
                &token.session_id,
                &token.token_hash,
                &token.expires_at,
                &token.absolute_expires_at,
                &token.created_at,
            ],
        )?;
        Ok(())
    }

    /// `consumed.rotate(now)`: 최초 회전 시각을 덮어쓰지 않는다.
    pub fn mark_rotated(
        &self,
        db: &mut impl GenericClient,
        id: Uuid,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE admin_refresh_tokens SET rotated_at = $2 WHERE id = $1 AND rotated_at IS NULL",
            &[&id, &now],
        )?;
        Ok(())
    }

    /// `deleteUnusableBefore`: 회원 토큰과 같은 기준으로 지운다. 지운 총 개수를 돌려준다.
    pub fn delete_unusable_before(
        &self,
        db: &mut impl GenericClient,
        now: Timestamp,
        revoked_threshold: Timestamp,
    ) -> Result<u64, DbError> {
        let absolutely_expired = db.execute(
            "DELETE FROM admin_refresh_tokens WHERE absolute_expires_at < $1",
            &[&now],
        )?;
        let revoked = db.execute(
            "DELETE FROM admin_refresh_tokens WHERE revoked_at < $1",
            &[&revoked_threshold],
        )?;
        Ok(absolutely_expired + revoked)
    }

    pub fn revoke_session(
        &self,
        db: &mut impl GenericClient,
        session_id: Uuid,
        revoked_at: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "UPDATE admin_refresh_tokens SET revoked_at = $2 \
             WHERE session_id = $1 AND revoked_at IS NULL",
            &[&session_id, &revoked_at],
        )?)
    }
}
