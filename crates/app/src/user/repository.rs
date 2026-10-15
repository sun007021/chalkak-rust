//! 회원 저장소 SQL. 원본 `UserRepository`(`UserJpaRepository`).
//!
//! Hibernate는 상태가 실제로 바뀐 엔티티만 `UPDATE`하고 그때마다 `updated_at`을 갱신한다. 같은 동작을 위해 읽은
//! 시점의 값과 비교하는 [`UserRepository::update_if_changed`]를 쓴다.

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::{GenericClient, Row};
use uuid::Uuid;

use super::UserRepository;
use super::domain::{SignatureProcessingStatus, User, UserStatus};

/// `users` 열 목록(별칭 `u`). enum 열은 문자열로 읽는다.
pub const COLUMNS: &str = "u.id, u.email, u.status::text AS status, u.signature_original_storage_key, \
     u.signature_thumbnail_storage_key, u.app_version, u.pending_signature_upload_id, \
     u.signature_processing_status::text AS signature_processing_status, \
     u.signature_processing_started_at, u.created_at, u.updated_at, u.deleted_at";

fn corrupt(column: &str, value: &str) -> DbError {
    DbError::Config(format!("users.{column} 값을 해석할 수 없습니다: {value}"))
}

pub fn user_from_row(row: &Row) -> Result<User, DbError> {
    let status: String = row.try_get("status")?;
    let processing: Option<String> = row.try_get("signature_processing_status")?;
    Ok(User {
        id: row.try_get("id")?,
        email: row.try_get("email")?,
        status: UserStatus::from_name(&status).ok_or_else(|| corrupt("status", &status))?,
        signature_original_storage_key: row.try_get("signature_original_storage_key")?,
        signature_thumbnail_storage_key: row.try_get("signature_thumbnail_storage_key")?,
        pending_signature_upload_id: row.try_get("pending_signature_upload_id")?,
        signature_processing_status: match processing {
            None => None,
            Some(p) => Some(
                SignatureProcessingStatus::from_name(&p)
                    .ok_or_else(|| corrupt("signature_processing_status", &p))?,
            ),
        },
        signature_processing_started_at: row.try_get("signature_processing_started_at")?,
        app_version: row.try_get("app_version")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        deleted_at: row.try_get("deleted_at")?,
    })
}

impl UserRepository {
    fn find_one(
        &self,
        db: &mut impl GenericClient,
        condition: &str,
        param: &(dyn postgres::types::ToSql + Sync),
        lock: bool,
    ) -> Result<Option<User>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM users u WHERE {condition}{}",
            if lock { " FOR NO KEY UPDATE" } else { "" }
        );
        db.query_opt(&sql, &[param])?
            .map(|row| user_from_row(&row))
            .transpose()
    }

    /// `findById`: 탈퇴 회원도 읽는다.
    pub fn find_by_id(
        &self,
        db: &mut impl GenericClient,
        id: Uuid,
    ) -> Result<Option<User>, DbError> {
        self.find_one(db, "u.id = $1", &id, false)
    }

    /// `findActiveById`.
    pub fn find_active_by_id(
        &self,
        db: &mut impl GenericClient,
        id: Uuid,
    ) -> Result<Option<User>, DbError> {
        self.find_one(db, "u.id = $1 AND u.deleted_at IS NULL", &id, false)
    }

    /// `findActiveByIdForUpdate`.
    pub fn find_active_by_id_for_update(
        &self,
        db: &mut impl GenericClient,
        id: Uuid,
    ) -> Result<Option<User>, DbError> {
        self.find_one(db, "u.id = $1 AND u.deleted_at IS NULL", &id, true)
    }

    /// `findActiveByPendingSignatureUploadIdForUpdate`.
    pub fn find_active_by_pending_signature_upload_id_for_update(
        &self,
        db: &mut impl GenericClient,
        upload_id: Uuid,
    ) -> Result<Option<User>, DbError> {
        self.find_one(
            db,
            "u.pending_signature_upload_id = $1 AND u.deleted_at IS NULL",
            &upload_id,
            true,
        )
    }

    /// `existsBySignatureOriginalStorageKey`.
    pub fn exists_by_signature_original_storage_key(
        &self,
        db: &mut impl GenericClient,
        storage_key: &str,
    ) -> Result<bool, DbError> {
        Ok(db
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM users WHERE signature_original_storage_key = $1)",
                &[&storage_key],
            )?
            .try_get(0)?)
    }

    /// `save`(새 회원). 식별자는 DB 기본값(`uuidv7()`)이 만든다.
    pub fn insert(
        &self,
        db: &mut impl GenericClient,
        user: &User,
        now: Timestamp,
    ) -> Result<User, DbError> {
        let row = db.query_one(
            &format!(
                "WITH u AS (INSERT INTO users (email, status, signature_original_storage_key, \
                 signature_thumbnail_storage_key, created_at, updated_at) \
                 VALUES ($1, CAST($2::text AS user_status), $3, $4, $5, $5) RETURNING *) \
                 SELECT {COLUMNS} FROM u"
            ),
            &[
                &user.email,
                &user.status.name(),
                &user.signature_original_storage_key,
                &user.signature_thumbnail_storage_key,
                &now,
            ],
        )?;
        user_from_row(&row)
    }

    /// 읽은 시점(`before`)과 달라졌을 때만 `UPDATE`하고 `updated_at`을 `now`로 바꾼다. 갱신했으면 `true`.
    pub fn update_if_changed(
        &self,
        db: &mut impl GenericClient,
        before: &User,
        after: &User,
        now: Timestamp,
    ) -> Result<bool, DbError> {
        if before == after {
            return Ok(false);
        }
        db.execute(
            "UPDATE users SET email = $2, status = CAST($3::text AS user_status), \
             signature_original_storage_key = $4, signature_thumbnail_storage_key = $5, \
             app_version = $6, pending_signature_upload_id = $7, \
             signature_processing_status = CAST($8::text AS signature_processing_status), \
             signature_processing_started_at = $9, deleted_at = $10, updated_at = $11 \
             WHERE id = $1",
            &[
                &after.id,
                &after.email,
                &after.status.name(),
                &after.signature_original_storage_key,
                &after.signature_thumbnail_storage_key,
                &after.app_version,
                &after.pending_signature_upload_id,
                &after
                    .signature_processing_status
                    .map(SignatureProcessingStatus::name),
                &after.signature_processing_started_at,
                &after.deleted_at,
                &now,
            ],
        )?;
        Ok(true)
    }
}
