//! 원본 `Photo` 엔티티의 규칙.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

#[derive(Clone, Debug, PartialEq)]
pub struct Photo {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub original_storage_key: String,
    pub thumbnail_storage_key: Option<String>,
    /// `photos.metadata`(jsonb, NOT NULL). 비어 있으면 `{}`.
    pub metadata: Map<String, Value>,
    pub deleted_at: Option<Timestamp>,
}

impl Photo {
    /// `Photo.createPhoto`
    pub fn create(original_storage_key: Option<&str>) -> Result<Photo, AppError> {
        match original_storage_key {
            Some(key) if !key.chars().all(is_java_whitespace) => Ok(Photo {
                id: None,
                original_storage_key: key.to_owned(),
                thumbnail_storage_key: None,
                metadata: Map::new(),
                deleted_at: None,
            }),
            _ => Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "사진 저장 정보가 올바르지 않습니다.",
            )),
        }
    }

    /// 이미지 처리가 끝난 뒤에만 썸네일 키를 채운다. 존재하지 않는 객체의 URL이 공개 응답에 나가면 안 된다.
    pub fn complete_processing(
        &mut self,
        thumbnail_storage_key: String,
        metadata: Option<Map<String, Value>>,
    ) {
        self.thumbnail_storage_key = Some(thumbnail_storage_key);
        self.metadata = metadata.unwrap_or_default();
    }

    /// `Photo.delete`: 이미 지워졌으면 최초 시각을 유지한다.
    pub fn delete(&mut self, deleted_at: Option<Timestamp>) -> Result<(), AppError> {
        if self.deleted_at.is_some() {
            return Ok(());
        }
        let Some(at) = deleted_at else {
            return Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "사진 삭제 시각이 필요합니다.",
            ));
        };
        self.deleted_at = Some(at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    // PhotoTest#createPhoto_validOriginalStorageKey_createsPhotoWithoutThumbnail
    #[test]
    fn create_photo_valid_original_storage_key_creates_photo_without_thumbnail() {
        let key = "chalkak/posts/test/original/photo.webp";
        let photo = Photo::create(Some(key)).unwrap();
        assert_eq!(photo.original_storage_key, key);
        assert_eq!(photo.thumbnail_storage_key, None);
        assert!(photo.metadata.is_empty());
    }

    // PhotoTest#createPhoto_blankOriginalStorageKey_throwsBusinessException
    #[test]
    fn create_photo_blank_original_storage_key_throws_business_exception() {
        for key in [Some(" "), Some(""), None] {
            let err = Photo::create(key).unwrap_err();
            assert_eq!(err.message, "사진 저장 정보가 올바르지 않습니다.");
        }
    }

    // PhotoTest#completeProcessing_processedImage_fillsThumbnailKeyAndMetadata
    #[test]
    fn complete_processing_processed_image_fills_thumbnail_key_and_metadata() {
        let mut photo = Photo::create(Some("chalkak/posts/test/original/photo.webp")).unwrap();
        let metadata = json!({"width": 4032, "height": 3024})
            .as_object()
            .unwrap()
            .clone();
        photo.complete_processing(
            "chalkak/posts/test/thumbnail/photo.webp".to_owned(),
            Some(metadata.clone()),
        );
        assert_eq!(
            photo.thumbnail_storage_key.as_deref(),
            Some("chalkak/posts/test/thumbnail/photo.webp")
        );
        assert_eq!(photo.metadata, metadata);
    }

    // PhotoTest#completeProcessing_nullMetadata_keepsEmptyMetadata
    #[test]
    fn complete_processing_null_metadata_keeps_empty_metadata() {
        let mut photo = Photo::create(Some("chalkak/posts/test/original/photo.webp")).unwrap();
        photo.complete_processing("chalkak/posts/test/thumbnail/photo.webp".to_owned(), None);
        assert!(photo.metadata.is_empty());
    }

    // PhotoTest#delete_activePhoto_recordsDeletedAt
    #[test]
    fn delete_active_photo_records_deleted_at() {
        let mut photo = Photo::create(Some("chalkak/posts/test/original/photo.webp")).unwrap();
        let at = ts("2026-08-20T01:00:00Z");
        photo.delete(Some(at)).unwrap();
        assert_eq!(photo.deleted_at, Some(at));
    }

    // PhotoTest#delete_alreadyDeletedPhoto_keepsFirstDeletionTime
    #[test]
    fn delete_already_deleted_photo_keeps_first_deletion_time() {
        let mut photo = Photo::create(Some("chalkak/posts/test/original/photo.webp")).unwrap();
        let first = ts("2026-08-20T01:00:00Z");
        photo.delete(Some(first)).unwrap();
        photo.delete(Some(ts("2026-08-20T01:01:00Z"))).unwrap();
        assert_eq!(photo.deleted_at, Some(first));
    }
}
