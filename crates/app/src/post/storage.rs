//! 게시물 이미지의 스토리지 키 규칙과 업로드 URL 발급. 원본 `S3PostImageStorage`·`S3PostImageUploadIssuer`.
//!
//! S3 호출(HeadObject, presigned PUT)은 [`ObjectStore`] 뒤에 둔다. 실제 구현은 서버 조립이 `chalkak-aws`로 만들고,
//! 시험은 가짜 구현을 쓴다.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::settings::ImageSettings;

const STAGING_PATH: &str = "staging";
const POST_PATH: &str = "posts";
const ORIGINAL_PATH: &str = "original";
const THUMBNAIL_PATH: &str = "thumbnail";
const IMAGE_EXTENSION: &str = ".webp";

pub const CONTENT_TYPE: &str = "image/webp";
/// presigned URL 유효 시간(`SIGNATURE_DURATION`).
pub const SIGNATURE_DURATION: Duration = Duration::from_secs(5 * 60);

/// 스토리지 호출 실패. 원본에서는 SDK 예외가 그대로 올라가 500이 된다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageError(pub String);

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StorageError {}

/// presigned PUT에 서명으로 묶는 속성(원본 `PutObjectRequest`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresignPut {
    pub bucket: String,
    pub key: String,
    pub content_type: String,
    /// 처리 람다용 업로드에서만 건다.
    pub cache_control: Option<String>,
    /// 처리 람다용 업로드에서만 `"*"`를 건다.
    pub if_none_match: Option<String>,
    pub expires: Duration,
}

pub trait ObjectStore: Send + Sync {
    /// HeadObject. 키 없음(`NoSuchKey`)과 403·404는 `false`, 그 밖의 실패는 오류다.
    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool, StorageError>;

    fn presign_put(&self, request: &PresignPut) -> Result<String, StorageError>;
}

/// `PresignedPostImageUpload`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresignedPostImageUpload {
    pub upload_url: String,
    pub expires_in_seconds: u64,
    pub content_type: String,
    pub max_bytes: u64,
}

/// `PostProcessingImageUpload`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostProcessingImageUpload {
    pub original_upload_url: String,
    pub thumbnail_upload_url: String,
    pub content_type: String,
    pub cache_control: String,
}

#[derive(Clone)]
pub struct PostImageStorage {
    store: Arc<dyn ObjectStore>,
    image: ImageSettings,
}

impl PostImageStorage {
    pub fn new(store: Arc<dyn ObjectStore>, image: ImageSettings) -> Self {
        PostImageStorage { store, image }
    }

    /// `existsUploadedImage`: staging 객체가 있는지.
    pub fn exists_uploaded_image(&self, upload_id: Uuid) -> Result<bool, StorageError> {
        self.store
            .object_exists(&self.image.bucket, &self.to_staging_storage_key(upload_id))
    }

    pub fn to_staging_storage_key(&self, upload_id: Uuid) -> String {
        self.create_key(STAGING_PATH, &self.image.environment, POST_PATH, upload_id)
    }

    pub fn to_original_storage_key(&self, upload_id: Uuid) -> String {
        self.create_key(POST_PATH, &self.image.environment, ORIGINAL_PATH, upload_id)
    }

    pub fn to_thumbnail_storage_key(&self, upload_id: Uuid) -> String {
        self.create_key(
            POST_PATH,
            &self.image.environment,
            THUMBNAIL_PATH,
            upload_id,
        )
    }

    fn create_key(&self, first: &str, second: &str, third: &str, upload_id: Uuid) -> String {
        format!(
            "{}/{first}/{second}/{third}/{upload_id}{IMAGE_EXTENSION}",
            self.image.root_prefix
        )
    }

    /// `issue`: 사용자가 사진을 올릴 staging 주소. 용량은 서명으로 강제되지 않는다(`maxBytes`는 안내용).
    pub fn issue(&self, upload_id: Uuid) -> Result<PresignedPostImageUpload, StorageError> {
        let upload_url = self.store.presign_put(&PresignPut {
            bucket: self.image.bucket.clone(),
            key: self.to_staging_storage_key(upload_id),
            content_type: CONTENT_TYPE.to_owned(),
            cache_control: None,
            if_none_match: None,
            expires: SIGNATURE_DURATION,
        })?;
        Ok(PresignedPostImageUpload {
            upload_url,
            expires_in_seconds: SIGNATURE_DURATION.as_secs(),
            content_type: CONTENT_TYPE.to_owned(),
            max_bytes: self.image.post_max_bytes,
        })
    }

    /// `issueProcessingUpload`: 이미지 처리 람다가 원본·썸네일을 올릴 주소. 덮어쓰기를 막는다(`If-None-Match: *`).
    pub fn issue_processing_upload(
        &self,
        upload_id: Uuid,
    ) -> Result<PostProcessingImageUpload, StorageError> {
        let original_upload_url =
            self.presign_processing(self.to_original_storage_key(upload_id))?;
        let thumbnail_upload_url =
            self.presign_processing(self.to_thumbnail_storage_key(upload_id))?;
        Ok(PostProcessingImageUpload {
            original_upload_url,
            thumbnail_upload_url,
            content_type: CONTENT_TYPE.to_owned(),
            cache_control: self.image.post_cache_control.clone(),
        })
    }

    fn presign_processing(&self, key: String) -> Result<String, StorageError> {
        self.store.presign_put(&PresignPut {
            bucket: self.image.bucket.clone(),
            key,
            content_type: CONTENT_TYPE.to_owned(),
            cache_control: Some(self.image.post_cache_control.clone()),
            if_none_match: Some("*".to_owned()),
            expires: SIGNATURE_DURATION,
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::*;

    /// 시험용 스토리지. 있는 키를 정해 두고, 받은 요청을 기록한다.
    #[derive(Default)]
    pub struct FakeObjectStore {
        pub existing: Mutex<HashSet<String>>,
        pub heads: Mutex<Vec<(String, String)>>,
        pub presigns: Mutex<Vec<PresignPut>>,
        pub fail: Mutex<Option<StorageError>>,
    }

    impl ObjectStore for FakeObjectStore {
        fn object_exists(&self, bucket: &str, key: &str) -> Result<bool, StorageError> {
            self.heads
                .lock()
                .unwrap()
                .push((bucket.to_owned(), key.to_owned()));
            if let Some(e) = self.fail.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(self.existing.lock().unwrap().contains(key))
        }

        fn presign_put(&self, request: &PresignPut) -> Result<String, StorageError> {
            self.presigns.lock().unwrap().push(request.clone());
            if let Some(e) = self.fail.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(format!(
                "https://{}.s3.test/{}?X-Amz-Expires={}",
                request.bucket,
                request.key,
                request.expires.as_secs()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeObjectStore;
    use super::*;

    const UPLOAD_ID: Uuid = Uuid::from_u128(0x0198f6c1_62ba_7d30_8b12_0f733b6570d4);

    fn image() -> ImageSettings {
        ImageSettings {
            bucket: "test-bucket".to_owned(),
            region: "ap-northeast-2".to_owned(),
            root_prefix: "chalkak".to_owned(),
            environment: "dev".to_owned(),
            post_max_bytes: 5_242_880,
            post_cache_control: "public, max-age=86400".to_owned(),
            anonymous_access: false,
        }
    }

    fn storage(store: Arc<FakeObjectStore>) -> PostImageStorage {
        PostImageStorage::new(store, image())
    }

    #[test]
    fn 스토리지_키는_접두사_환경_종류_업로드_id로_만든다() {
        let s = storage(Arc::default());
        assert_eq!(
            s.to_staging_storage_key(UPLOAD_ID),
            "chalkak/staging/dev/posts/0198f6c1-62ba-7d30-8b12-0f733b6570d4.webp"
        );
        assert_eq!(
            s.to_original_storage_key(UPLOAD_ID),
            "chalkak/posts/dev/original/0198f6c1-62ba-7d30-8b12-0f733b6570d4.webp"
        );
        assert_eq!(
            s.to_thumbnail_storage_key(UPLOAD_ID),
            "chalkak/posts/dev/thumbnail/0198f6c1-62ba-7d30-8b12-0f733b6570d4.webp"
        );
    }

    // S3PostImageStorageTest#existsUploadedImage_*
    #[test]
    fn exists_uploaded_image_reads_the_staging_key() {
        let store = Arc::new(FakeObjectStore::default());
        let s = storage(store.clone());
        assert!(!s.exists_uploaded_image(UPLOAD_ID).unwrap());
        store
            .existing
            .lock()
            .unwrap()
            .insert(s.to_staging_storage_key(UPLOAD_ID));
        assert!(s.exists_uploaded_image(UPLOAD_ID).unwrap());
        let heads = store.heads.lock().unwrap();
        assert_eq!(heads[0].0, "test-bucket");
        assert_eq!(heads[0].1, s.to_staging_storage_key(UPLOAD_ID));
    }

    #[test]
    fn 스토리지_오류는_그대로_올린다() {
        let store = Arc::new(FakeObjectStore::default());
        *store.fail.lock().unwrap() = Some(StorageError("boom".to_owned()));
        let s = storage(store);
        assert_eq!(
            s.exists_uploaded_image(UPLOAD_ID).unwrap_err(),
            StorageError("boom".to_owned())
        );
    }

    // S3PostImageUploadIssuerTest#issue_*
    #[test]
    fn issue_signs_a_five_minute_webp_put_for_the_staging_key() {
        let store = Arc::new(FakeObjectStore::default());
        let issued = storage(store.clone()).issue(UPLOAD_ID).unwrap();
        assert_eq!(issued.expires_in_seconds, 300);
        assert_eq!(issued.content_type, "image/webp");
        assert_eq!(issued.max_bytes, 5_242_880);
        let presigns = store.presigns.lock().unwrap();
        assert_eq!(presigns.len(), 1);
        assert_eq!(presigns[0].bucket, "test-bucket");
        assert_eq!(
            presigns[0].key,
            "chalkak/staging/dev/posts/0198f6c1-62ba-7d30-8b12-0f733b6570d4.webp"
        );
        assert_eq!(presigns[0].content_type, "image/webp");
        assert_eq!(presigns[0].cache_control, None);
        assert_eq!(presigns[0].if_none_match, None);
        assert_eq!(presigns[0].expires, Duration::from_secs(300));
        assert!(issued.upload_url.contains("X-Amz-Expires=300"));
    }

    // S3PostImageUploadIssuerTest#issueProcessingUpload_*
    #[test]
    fn issue_processing_upload_signs_original_and_thumbnail_without_overwrite() {
        let store = Arc::new(FakeObjectStore::default());
        let issued = storage(store.clone())
            .issue_processing_upload(UPLOAD_ID)
            .unwrap();
        assert_eq!(issued.content_type, "image/webp");
        assert_eq!(issued.cache_control, "public, max-age=86400");
        let presigns = store.presigns.lock().unwrap();
        assert_eq!(presigns.len(), 2);
        assert!(presigns[0].key.contains("/posts/dev/original/"));
        assert!(presigns[1].key.contains("/posts/dev/thumbnail/"));
        for p in presigns.iter() {
            assert_eq!(p.cache_control.as_deref(), Some("public, max-age=86400"));
            assert_eq!(p.if_none_match.as_deref(), Some("*"));
            assert_eq!(p.content_type, "image/webp");
        }
        assert!(issued.original_upload_url.contains("/original/"));
        assert!(issued.thumbnail_upload_url.contains("/thumbnail/"));
    }
}
