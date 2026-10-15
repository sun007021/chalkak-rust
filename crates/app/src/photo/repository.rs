//! 원본 `PhotoRepositoryImpl`/`PhotoJpaRepository`. 게시물 저장에 딸려 가는 쓰기는 게시물 저장소가 이 함수들을 부른다
//! (JPA에서는 `Post.photo`의 `CascadeType.PERSIST`와 변경 감지가 하던 일).

use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::Timestamp;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::domain::Photo;

#[derive(Clone, Copy, Debug, Default)]
pub struct PhotoRepository;

/// JSON 객체를 jsonb 열에 넣을 텍스트로 만든다.
pub fn metadata_text(metadata: &Map<String, Value>) -> String {
    Value::Object(metadata.clone()).to_string()
}

impl PhotoRepository {
    /// `existsByOriginalStorageKey`: 삭제된 사진도 센다(고유 제약이 삭제 여부와 무관하다).
    pub fn exists_by_original_storage_key(
        &self,
        db: &mut impl Db,
        original_storage_key: &str,
    ) -> Result<bool, DbError> {
        Ok(db
            .query_opt(
                "SELECT 1 FROM photos WHERE original_storage_key = $1 LIMIT 1",
                &[&original_storage_key],
            )?
            .is_some())
    }

    /// INSERT. `id`는 DB 기본값(`uuidv7()`)이 만든다.
    pub fn insert(
        &self,
        db: &mut impl Db,
        photo: &mut Photo,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let metadata = metadata_text(&photo.metadata);
        let row = db.query_one(
            "INSERT INTO photos (original_storage_key, thumbnail_storage_key, metadata, \
             created_at, updated_at, deleted_at) \
             VALUES ($1, $2, $3::text::jsonb, $4, $4, $5) RETURNING id",
            &[
                &photo.original_storage_key,
                &photo.thumbnail_storage_key,
                &metadata,
                &now,
                &photo.deleted_at,
            ],
        )?;
        photo.id = Some(row.try_get("id")?);
        Ok(())
    }

    /// 이미지 처리 결과(썸네일 키와 메타데이터)를 반영한다. 변경 감지처럼 `updated_at`을 앱 시각으로 올린다.
    pub fn update_processing(
        &self,
        db: &mut impl Db,
        photo_id: Uuid,
        photo: &Photo,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let metadata = metadata_text(&photo.metadata);
        db.execute(
            "UPDATE photos SET thumbnail_storage_key = $2, metadata = $3::text::jsonb, updated_at = $4 \
             WHERE id = $1",
            &[&photo_id, &photo.thumbnail_storage_key, &metadata, &now],
        )?;
        Ok(())
    }

    /// 소프트 삭제 시각을 반영한다.
    pub fn update_deleted_at(
        &self,
        db: &mut impl Db,
        photo_id: Uuid,
        deleted_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE photos SET deleted_at = $2, updated_at = $3 WHERE id = $1",
            &[&photo_id, &deleted_at, &now],
        )?;
        Ok(())
    }
}
