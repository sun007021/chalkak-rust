//! 게시물 이미지 업로드 저장소. 원본 `PostImageUploadRepositoryImpl`·`PostImageUploadJpaRepository`.

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::GenericClient;
use serde_json::Value;
use uuid::Uuid;

use super::domain::{PostImageUpload, PostImageUploadStatus};
use super::repository::parse_metadata;

const COLUMNS: &str = "id, user_id, status::text AS status, rejection_reason, \
     image_metadata::text AS image_metadata, expires_at, claimed_at";

#[derive(Clone, Copy, Debug, Default)]
pub struct PostImageUploadRepository;

fn upload_from_row(row: &postgres::Row) -> Result<PostImageUpload, DbError> {
    let status_text: String = row.try_get("status")?;
    let status = PostImageUploadStatus::parse(&status_text)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 업로드 상태: {status_text}")))?;
    let metadata: Option<String> = row.try_get("image_metadata")?;
    Ok(PostImageUpload {
        id: Some(row.try_get("id")?),
        user_id: row.try_get("user_id")?,
        status,
        rejection_reason: row.try_get("rejection_reason")?,
        image_metadata: metadata.as_deref().map(parse_metadata).transpose()?,
        expires_at: row.try_get("expires_at")?,
        claimed_at: row.try_get("claimed_at")?,
    })
}

fn metadata_text(upload: &PostImageUpload) -> Option<String> {
    upload
        .image_metadata
        .as_ref()
        .map(|m| Value::Object(m.clone()).to_string())
}

impl PostImageUploadRepository {
    /// `save`(새 행): 식별자는 DB가 만든다.
    pub fn insert(
        &self,
        db: &mut impl GenericClient,
        upload: &mut PostImageUpload,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let status = upload.status.as_str();
        let metadata = metadata_text(upload);
        let row = db.query_one(
            "INSERT INTO post_image_uploads (user_id, status, rejection_reason, image_metadata, \
             expires_at, claimed_at, created_at, updated_at) \
             VALUES ($1, $2::text::post_image_upload_status, $3, $4::text::jsonb, $5, $6, $7, $7) \
             RETURNING id",
            &[
                &upload.user_id,
                &status,
                &upload.rejection_reason,
                &metadata,
                &upload.expires_at,
                &upload.claimed_at,
                &now,
            ],
        )?;
        upload.id = Some(row.try_get("id")?);
        Ok(())
    }

    /// `findByIdForUpdate`: 비관적 쓰기 잠금(`FOR NO KEY UPDATE`).
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl GenericClient,
        upload_id: Uuid,
    ) -> Result<Option<PostImageUpload>, DbError> {
        let sql =
            format!("SELECT {COLUMNS} FROM post_image_uploads WHERE id = $1 FOR NO KEY UPDATE");
        db.query_opt(&sql, &[&upload_id])?
            .as_ref()
            .map(upload_from_row)
            .transpose()
    }

    /// `findStatusByIdAndUserId`: 상태만 읽는다(잠그지 않는다).
    pub fn find_status_by_id_and_user_id(
        &self,
        db: &mut impl GenericClient,
        upload_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<PostImageUploadStatus>, DbError> {
        let row = db.query_opt(
            "SELECT status::text AS status FROM post_image_uploads WHERE id = $1 AND user_id = $2",
            &[&upload_id, &user_id],
        )?;
        row.map(|r| {
            let text: String = r.try_get("status")?;
            PostImageUploadStatus::parse(&text)
                .ok_or_else(|| DbError::Config(format!("알 수 없는 업로드 상태: {text}")))
        })
        .transpose()
    }

    /// 변경 감지처럼 바뀔 수 있는 열을 모두 쓴다.
    pub fn update(
        &self,
        db: &mut impl GenericClient,
        upload: &PostImageUpload,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let status = upload.status.as_str();
        let metadata = metadata_text(upload);
        db.execute(
            "UPDATE post_image_uploads SET status = $2::text::post_image_upload_status, \
             rejection_reason = $3, image_metadata = $4::text::jsonb, claimed_at = $5, \
             updated_at = $6 WHERE id = $1",
            &[
                &upload.id,
                &status,
                &upload.rejection_reason,
                &metadata,
                &upload.claimed_at,
                &now,
            ],
        )?;
        Ok(())
    }
}
