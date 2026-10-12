//! 사인 이미지 저장소(S3). 원본 `S3SignatureImageStorage`, `S3SignatureImageUploadIssuer`, `S3ClientConfig`.
//!
//! - 키 구성: `<root-prefix>/signatures/<env>/{original|thumbnail}/<uploadId>.png`, 업로드 직후는
//!   `<root-prefix>/staging/<env>/signatures/<uploadId>.png`.
//! - 업로드 URL: presigned PUT(유효 5분, `Content-Type: image/png`). 처리 람다용 URL은 `cache-control`과
//!   `if-none-match: *`도 서명한다.
//! - 존재 확인: `HeadObject`. 404·403은 "없음"이고(권한이 없으면 S3가 존재를 숨기려고 403을 준다) 5xx·연결 실패는 SDK
//!   기본 재시도(총 4번, 지수 백오프 + 전체 지터, 기본 100ms)를 거친 뒤 오류다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_aws::credentials::{HttpRequest, HttpResponse, HttpTransport};
use chalkak_aws::{
    AwsError, ChainCredentialsProvider, Credentials, CredentialsProvider, PutObject, presign_put,
    sign_head_object,
};
use chalkak_core::Clock;
use chalkak_http::client::{HttpClient, Method as ClientMethod};
use uuid::Uuid;

use super::domain::{SignatureStorageKeys, StoredImageMetadata};
use crate::common::random_bytes;
use crate::config::ImageSettings;
use crate::web::ApiError;

const STAGING_PATH: &str = "staging";
const SIGNATURE_PATH: &str = "signatures";
const ORIGINAL_PATH: &str = "original";
const THUMBNAIL_PATH: &str = "thumbnail";
const IMAGE_EXTENSION: &str = ".png";
const CONTENT_TYPE: &str = "image/png";
/// `S3SignatureImageUploadIssuer.SIGNATURE_DURATION`.
pub const PRESIGN_DURATION: Duration = Duration::from_secs(300);

/// `SignatureImageUpload`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureImageUpload {
    pub upload_id: Uuid,
    pub upload_url: String,
    pub expires_in_seconds: i64,
}

/// `SignatureProcessingImageUpload`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureProcessingImageUpload {
    pub original_upload_url: String,
    pub thumbnail_upload_url: String,
    pub content_type: String,
    pub cache_control: String,
}

/// `SignatureImageStorage`.
pub trait SignatureImageStorage: Send + Sync {
    fn find_uploaded_image(&self, upload_id: Uuid)
    -> Result<Option<StoredImageMetadata>, ApiError>;
    fn to_staging_storage_key(&self, upload_id: Uuid) -> String;
    fn to_storage_keys(&self, upload_id: Uuid) -> SignatureStorageKeys;
    fn is_processing_completed(&self, upload_id: Uuid) -> Result<bool, ApiError>;
    fn to_image_url(&self, storage_key: &str) -> Result<String, ApiError>;
}

/// `SignatureImageUploadIssuer`.
pub trait SignatureImageUploadIssuer: Send + Sync {
    fn issue(&self, upload_id: Uuid) -> Result<SignatureImageUpload, ApiError>;
    fn issue_processing_upload(
        &self,
        upload_id: Uuid,
    ) -> Result<SignatureProcessingImageUpload, ApiError>;
}

/// `HeadObject` 실패. SDK의 `S3Exception`(HTTP 응답이 있는 오류)과 그 밖의 실패(연결·자격증명)를 가른다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadFailure {
    Http(u16),
    Other(String),
}

/// 객체 존재·메타데이터 조회.
pub trait HeadObjects: Send + Sync {
    /// 객체가 없거나(404) 보이지 않으면(403) `Ok(None)`.
    fn head(&self, key: &str) -> Result<Option<StoredImageMetadata>, HeadFailure>;
}

fn head_error(failure: HeadFailure) -> ApiError {
    match failure {
        HeadFailure::Http(status) => {
            ApiError::Internal(format!("S3 HeadObject가 {status}를 돌려줬습니다"))
        }
        HeadFailure::Other(message) => ApiError::Internal(format!("S3 HeadObject 실패: {message}")),
    }
}

/// 키 규칙과 조회 정책(`S3SignatureImageStorage`).
pub struct S3SignatureImageStorage {
    root_prefix: String,
    environment: String,
    base_url: String,
    head: Arc<dyn HeadObjects>,
}

impl S3SignatureImageStorage {
    pub fn new(settings: &ImageSettings, head: Arc<dyn HeadObjects>) -> S3SignatureImageStorage {
        S3SignatureImageStorage {
            root_prefix: settings.root_prefix.clone(),
            environment: settings.environment.clone(),
            base_url: settings.base_url.clone(),
            head,
        }
    }

    fn key(&self, first: &str, second: &str, third: &str, upload_id: Uuid) -> String {
        format!(
            "{}/{first}/{second}/{third}/{upload_id}{IMAGE_EXTENSION}",
            self.root_prefix
        )
    }

    fn find_image(&self, key: &str) -> Result<Option<StoredImageMetadata>, HeadFailure> {
        self.head.head(key)
    }
}

impl SignatureImageStorage for S3SignatureImageStorage {
    fn find_uploaded_image(
        &self,
        upload_id: Uuid,
    ) -> Result<Option<StoredImageMetadata>, ApiError> {
        // 업로드 직후는 staging, 처리가 끝나면 original에 있다. staging 조회가 S3 오류로 실패하면 original을 먼저
        // 확인하고, original에서도 못 찾았을 때만 그 오류를 던진다(연결 실패는 바로 던진다).
        let mut staging_failure = None;
        match self.find_image(&self.to_staging_storage_key(upload_id)) {
            Ok(Some(image)) => return Ok(Some(image)),
            Ok(None) => {}
            Err(failure @ HeadFailure::Http(_)) => staging_failure = Some(failure),
            Err(other) => return Err(head_error(other)),
        }
        let original = self
            .find_image(&self.to_storage_keys(upload_id).original)
            .map_err(head_error)?;
        match (original, staging_failure) {
            (None, Some(failure)) => Err(head_error(failure)),
            (original, _) => Ok(original),
        }
    }

    fn to_staging_storage_key(&self, upload_id: Uuid) -> String {
        self.key(STAGING_PATH, &self.environment, SIGNATURE_PATH, upload_id)
    }

    fn to_storage_keys(&self, upload_id: Uuid) -> SignatureStorageKeys {
        SignatureStorageKeys {
            original: self.key(SIGNATURE_PATH, &self.environment, ORIGINAL_PATH, upload_id),
            thumbnail: self.key(SIGNATURE_PATH, &self.environment, THUMBNAIL_PATH, upload_id),
        }
    }

    fn is_processing_completed(&self, upload_id: Uuid) -> Result<bool, ApiError> {
        self.find_image(&self.to_storage_keys(upload_id).thumbnail)
            .map(|image| image.is_some())
            .map_err(head_error)
    }

    /// CloudFront 오리진이 `{bucket}/{root-prefix}`를 가리키므로 공개 URL에는 root-prefix가 들어가지 않는다.
    fn to_image_url(&self, storage_key: &str) -> Result<String, ApiError> {
        let prefix = format!("{}/", self.root_prefix);
        let public_path = storage_key.strip_prefix(&prefix).ok_or_else(|| {
            ApiError::Internal(format!(
                "스토리지 키는 root-prefix로 시작해야 합니다: {}",
                self.root_prefix
            ))
        })?;
        Ok(if self.base_url.ends_with('/') {
            format!("{}{public_path}", self.base_url)
        } else {
            format!("{}/{public_path}", self.base_url)
        })
    }
}

/// 재시도 대상(SDK 기본 재시도 조건): 500·502·503·504·429.
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// 기본 재시도: 첫 시도 + 3번, 지수 백오프(기본 100ms, 상한 20초)에 전체 지터.
const MAX_ATTEMPTS: u32 = 4;
const BASE_DELAY_MS: u64 = 100;
const MAX_DELAY_MS: u64 = 20_000;

fn backoff(attempt: u32) -> Duration {
    let ceiling = (BASE_DELAY_MS << attempt.min(20)).min(MAX_DELAY_MS);
    let jitter = u64::from_le_bytes(random_bytes::<8>()) % (ceiling + 1);
    Duration::from_millis(jitter)
}

/// 실제 S3 `HeadObject`(SigV4 헤더 서명, 익명이면 서명 없음).
pub struct HttpHeadObjects {
    settings: ImageSettings,
    credentials: Option<Arc<dyn CredentialsProvider>>,
    client: HttpClient,
    clock: Arc<dyn Clock>,
    sleep: fn(Duration),
}

impl HttpHeadObjects {
    pub fn new(
        settings: &ImageSettings,
        credentials: Option<Arc<dyn CredentialsProvider>>,
        clock: Arc<dyn Clock>,
    ) -> Result<HttpHeadObjects, String> {
        // SDK 기본 HTTP 클라이언트 제한 시간(연결 2초, 읽기 30초)을 따른다.
        let client = HttpClient::builder()
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(HttpHeadObjects {
            settings: settings.clone(),
            credentials,
            client,
            clock,
            sleep: std::thread::sleep,
        })
    }

    fn attempt(&self, key: &str) -> Result<Option<StoredImageMetadata>, HeadFailure> {
        let (url, headers): (String, Vec<(String, String)>) = match &self.credentials {
            Some(provider) => {
                let credentials = provider
                    .credentials()
                    .map_err(|e| HeadFailure::Other(e.to_string()))?;
                let signed = sign_head_object(
                    &credentials,
                    &self.settings.region,
                    &self.settings.endpoint,
                    &self.settings.bucket,
                    key,
                    self.clock.now(),
                )
                .map_err(|e| HeadFailure::Other(e.to_string()))?;
                (signed.url, signed.headers)
            }
            None => {
                // 익명: 서명 없이 같은 주소로 요청한다. 주소 계산만 서명 함수를 빌린다.
                let anonymous = Credentials {
                    access_key_id: "anonymous".to_owned(),
                    secret_access_key: "anonymous".to_owned(),
                    session_token: None,
                };
                let signed = sign_head_object(
                    &anonymous,
                    &self.settings.region,
                    &self.settings.endpoint,
                    &self.settings.bucket,
                    key,
                    self.clock.now(),
                )
                .map_err(|e| HeadFailure::Other(e.to_string()))?;
                (signed.url, Vec::new())
            }
        };
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("host"))
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let response = self
            .client
            .head(&url, &header_refs)
            .map_err(|e| HeadFailure::Other(e.to_string()))?;
        match response.status {
            200..=299 => {
                let byte_size = response
                    .header("content-length")
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .ok_or_else(|| HeadFailure::Other("Content-Length가 없습니다".to_owned()))?;
                Ok(Some(StoredImageMetadata {
                    content_type: response.header("content-type").map(str::to_owned),
                    byte_size,
                }))
            }
            404 | 403 => Ok(None),
            status => Err(HeadFailure::Http(status)),
        }
    }
}

impl HeadObjects for HttpHeadObjects {
    fn head(&self, key: &str) -> Result<Option<StoredImageMetadata>, HeadFailure> {
        let mut attempt = 0;
        loop {
            let result = self.attempt(key);
            let retry = match &result {
                Err(HeadFailure::Http(status)) => retryable_status(*status),
                // 연결·시간 초과 같은 입출력 실패는 재시도하지만 자격증명 실패는 그렇지 않다.
                Err(HeadFailure::Other(message)) => !message.starts_with("no AWS credentials"),
                Ok(_) => false,
            };
            attempt += 1;
            if !retry || attempt >= MAX_ATTEMPTS {
                return result;
            }
            (self.sleep)(backoff(attempt - 1));
        }
    }
}

/// 업로드 URL 발급(`S3SignatureImageUploadIssuer`). 익명 접근 설정이면 서명을 만들 수 없어 오류다.
pub struct S3SignatureImageUploadIssuer {
    settings: ImageSettings,
    credentials: Option<Arc<dyn CredentialsProvider>>,
    storage: Arc<dyn SignatureImageStorage>,
    clock: Arc<dyn Clock>,
}

impl S3SignatureImageUploadIssuer {
    pub fn new(
        settings: &ImageSettings,
        credentials: Option<Arc<dyn CredentialsProvider>>,
        storage: Arc<dyn SignatureImageStorage>,
        clock: Arc<dyn Clock>,
    ) -> S3SignatureImageUploadIssuer {
        S3SignatureImageUploadIssuer {
            settings: settings.clone(),
            credentials,
            storage,
            clock,
        }
    }

    fn presign(&self, put: &PutObject<'_>) -> Result<String, ApiError> {
        let provider = self.credentials.as_ref().ok_or_else(|| {
            ApiError::Internal("익명 자격증명으로는 업로드 URL을 서명할 수 없습니다".to_owned())
        })?;
        let credentials = provider
            .credentials()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        presign_put(
            &credentials,
            &self.settings.region,
            &self.settings.endpoint,
            put,
            PRESIGN_DURATION,
            self.clock.now(),
        )
        .map_err(|e| ApiError::Internal(e.to_string()))
    }

    fn presign_processing_upload(&self, key: &str) -> Result<String, ApiError> {
        self.presign(&PutObject {
            bucket: &self.settings.bucket,
            key,
            content_type: CONTENT_TYPE,
            cache_control: Some(&self.settings.signature_cache_control),
            if_none_match: Some("*"),
        })
    }
}

impl SignatureImageUploadIssuer for S3SignatureImageUploadIssuer {
    fn issue(&self, upload_id: Uuid) -> Result<SignatureImageUpload, ApiError> {
        let key = self.storage.to_staging_storage_key(upload_id);
        let upload_url = self.presign(&PutObject {
            bucket: &self.settings.bucket,
            key: &key,
            content_type: CONTENT_TYPE,
            cache_control: None,
            if_none_match: None,
        })?;
        Ok(SignatureImageUpload {
            upload_id,
            upload_url,
            expires_in_seconds: i64::try_from(PRESIGN_DURATION.as_secs()).unwrap_or(300),
        })
    }

    fn issue_processing_upload(
        &self,
        upload_id: Uuid,
    ) -> Result<SignatureProcessingImageUpload, ApiError> {
        let keys = self.storage.to_storage_keys(upload_id);
        Ok(SignatureProcessingImageUpload {
            original_upload_url: self.presign_processing_upload(&keys.original)?,
            thumbnail_upload_url: self.presign_processing_upload(&keys.thumbnail)?,
            content_type: CONTENT_TYPE.to_owned(),
            cache_control: self.settings.signature_cache_control.clone(),
        })
    }
}

/// `HttpTransport` 어댑터(IMDS 자격증명 조회용). 호출이 드물고 짧아 제한 시간을 1초로 둔다.
pub struct ClientTransport {
    client: HttpClient,
}

impl ClientTransport {
    pub fn new() -> Result<ClientTransport, String> {
        let client = HttpClient::builder()
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(1))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(ClientTransport { client })
    }
}

impl HttpTransport for ClientTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AwsError> {
        let method = match request.method.as_str() {
            "GET" => ClientMethod::Get,
            "PUT" => ClientMethod::Put,
            other => {
                return Err(AwsError::InvalidInput(format!(
                    "지원하지 않는 메서드: {other}"
                )));
            }
        };
        let headers: Vec<(&str, &str)> = request
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        let body = (!request.body.is_empty()).then_some(request.body.as_slice());
        let response = self
            .client
            .request(method, &request.url, &headers, body)
            .map_err(|e| AwsError::Transport(e.to_string()))?;
        Ok(HttpResponse {
            status: response.status,
            body: response.body,
        })
    }
}

/// 설정에 맞는 자격증명 공급자. 익명 접근이면 `None`(서명하지 않는다), 아니면 환경변수 -> IMDS 순서다.
pub fn credentials_for(
    settings: &ImageSettings,
    clock: Arc<dyn Clock>,
) -> Result<Option<Arc<dyn CredentialsProvider>>, String> {
    if settings.anonymous_access {
        return Ok(None);
    }
    let transport: Arc<dyn HttpTransport> = Arc::new(ClientTransport::new()?);
    let now: chalkak_aws::credentials::NowFn = Arc::new(move || clock.now());
    Ok(Some(Arc::new(ChainCredentialsProvider::default_chain(
        transport, now,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn settings() -> ImageSettings {
        ImageSettings {
            bucket: "test-bucket".into(),
            region: "ap-northeast-2".into(),
            base_url: "https://cdn.test.chalkak".into(),
            root_prefix: "chalkak".into(),
            environment: "local".into(),
            signature_max_bytes: 1_048_576,
            signature_content_types: vec!["image/png".into()],
            signature_cache_control: "public, max-age=86400".into(),
            anonymous_access: false,
            endpoint: chalkak_aws::S3Endpoint::custom("http://fakes:9000", false).unwrap(),
        }
    }

    type Responses = Mutex<Vec<(String, Result<Option<StoredImageMetadata>, HeadFailure>)>>;

    /// 키별 응답을 미리 정해 두는 가짜 `HeadObject`.
    struct FakeHead {
        responses: Responses,
        calls: Mutex<Vec<String>>,
    }

    impl FakeHead {
        fn new(
            responses: Vec<(&str, Result<Option<StoredImageMetadata>, HeadFailure>)>,
        ) -> Arc<FakeHead> {
            Arc::new(FakeHead {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(k, r)| (k.to_owned(), r))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl HeadObjects for FakeHead {
        fn head(&self, key: &str) -> Result<Option<StoredImageMetadata>, HeadFailure> {
            self.calls.lock().unwrap().push(key.to_owned());
            self.responses
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| key.contains(k.as_str()))
                .map(|(_, r)| r.clone())
                .unwrap_or(Ok(None))
        }
    }

    fn png() -> StoredImageMetadata {
        StoredImageMetadata {
            content_type: Some("image/png".into()),
            byte_size: 10,
        }
    }

    fn id() -> Uuid {
        Uuid::parse_str("0190f3a2-7b1c-7d2e-8f4a-123456789abc").unwrap()
    }

    // S3SignatureImageStorageTest: 키 구성
    #[test]
    fn 저장_키는_원본_규칙을_따른다() {
        let storage = S3SignatureImageStorage::new(&settings(), FakeHead::new(vec![]));
        assert_eq!(
            storage.to_staging_storage_key(id()),
            "chalkak/staging/local/signatures/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png"
        );
        let keys = storage.to_storage_keys(id());
        assert_eq!(
            keys.original,
            "chalkak/signatures/local/original/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png"
        );
        assert_eq!(
            keys.thumbnail,
            "chalkak/signatures/local/thumbnail/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png"
        );
    }

    // S3SignatureImageStorageTest#toImageUrl
    #[test]
    fn 이미지_url에서는_root_prefix를_뺀다() {
        let storage = S3SignatureImageStorage::new(&settings(), FakeHead::new(vec![]));
        assert_eq!(
            storage
                .to_image_url("chalkak/signatures/local/original/a.png")
                .unwrap(),
            "https://cdn.test.chalkak/signatures/local/original/a.png"
        );
        let mut s = settings();
        s.base_url = "https://cdn.test.chalkak/".into();
        let storage = S3SignatureImageStorage::new(&s, FakeHead::new(vec![]));
        assert_eq!(
            storage.to_image_url("chalkak/x.png").unwrap(),
            "https://cdn.test.chalkak/x.png"
        );
        assert_eq!(
            storage.to_image_url("withdrawn/abc").unwrap_err().status(),
            500
        );
        assert_eq!(storage.to_image_url("chalkak").unwrap_err().status(), 500);
    }

    // S3SignatureImageStorageTest#findUploadedImage
    #[test]
    fn staging을_먼저_보고_없으면_original을_본다() {
        let head = FakeHead::new(vec![("/original/", Ok(Some(png())))]);
        let storage = S3SignatureImageStorage::new(&settings(), head.clone());
        assert_eq!(storage.find_uploaded_image(id()).unwrap(), Some(png()));
        assert_eq!(head.calls.lock().unwrap().len(), 2);

        let head = FakeHead::new(vec![("/staging/", Ok(Some(png())))]);
        let storage = S3SignatureImageStorage::new(&settings(), head.clone());
        assert_eq!(storage.find_uploaded_image(id()).unwrap(), Some(png()));
        assert_eq!(
            head.calls.lock().unwrap().len(),
            1,
            "staging에서 찾으면 original은 조회하지 않는다"
        );

        let storage = S3SignatureImageStorage::new(&settings(), FakeHead::new(vec![]));
        assert_eq!(storage.find_uploaded_image(id()).unwrap(), None);
    }

    #[test]
    fn staging_s3_오류는_original이_있으면_삼킨다() {
        let head = FakeHead::new(vec![
            ("/staging/", Err(HeadFailure::Http(500))),
            ("/original/", Ok(Some(png()))),
        ]);
        let storage = S3SignatureImageStorage::new(&settings(), head);
        assert_eq!(storage.find_uploaded_image(id()).unwrap(), Some(png()));

        let head = FakeHead::new(vec![("/staging/", Err(HeadFailure::Http(500)))]);
        let storage = S3SignatureImageStorage::new(&settings(), head);
        assert_eq!(storage.find_uploaded_image(id()).unwrap_err().status(), 500);
    }

    #[test]
    fn staging_연결_실패는_original을_보지_않고_바로_던진다() {
        let head = FakeHead::new(vec![
            ("/staging/", Err(HeadFailure::Other("io".into()))),
            ("/original/", Ok(Some(png()))),
        ]);
        let storage = S3SignatureImageStorage::new(&settings(), head.clone());
        assert_eq!(storage.find_uploaded_image(id()).unwrap_err().status(), 500);
        assert_eq!(head.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn 썸네일이_있으면_처리가_끝난_것이다() {
        let head = FakeHead::new(vec![("/thumbnail/", Ok(Some(png())))]);
        let storage = S3SignatureImageStorage::new(&settings(), head);
        assert!(storage.is_processing_completed(id()).unwrap());
        let storage = S3SignatureImageStorage::new(&settings(), FakeHead::new(vec![]));
        assert!(!storage.is_processing_completed(id()).unwrap());
        let head = FakeHead::new(vec![("/thumbnail/", Err(HeadFailure::Http(503)))]);
        let storage = S3SignatureImageStorage::new(&settings(), head);
        assert!(storage.is_processing_completed(id()).is_err());
    }

    #[test]
    fn 재시도_대상_상태() {
        for s in [429, 500, 502, 503, 504] {
            assert!(retryable_status(s));
        }
        for s in [400, 401, 403, 404, 405] {
            assert!(!retryable_status(s));
        }
        for attempt in 0..6 {
            assert!(backoff(attempt) <= Duration::from_millis(100 << attempt));
        }
    }

    struct StaticCredentials;
    impl CredentialsProvider for StaticCredentials {
        fn credentials(&self) -> Result<Credentials, AwsError> {
            Ok(Credentials {
                access_key_id: "AKIAHARNESSTEST00000".into(),
                secret_access_key: "harness/secret/key/for/deterministic/presign".into(),
                session_token: None,
            })
        }
    }

    fn issuer(anonymous: bool) -> S3SignatureImageUploadIssuer {
        let s = settings();
        let storage = Arc::new(S3SignatureImageStorage::new(&s, FakeHead::new(vec![])));
        let clock = Arc::new(chalkak_core::FixedClock::new(
            "2026-10-12T00:00:00Z".parse().unwrap(),
        ));
        let creds: Option<Arc<dyn CredentialsProvider>> = if anonymous {
            None
        } else {
            Some(Arc::new(StaticCredentials))
        };
        S3SignatureImageUploadIssuer::new(&s, creds, storage, clock)
    }

    // S3SignatureImageUploadIssuerTest
    #[test]
    fn 업로드_url은_staging_키를_5분_서명한다() {
        let upload = issuer(false).issue(id()).unwrap();
        assert_eq!(upload.upload_id, id());
        assert_eq!(upload.expires_in_seconds, 300);
        assert!(upload.upload_url.starts_with(
            "http://test-bucket.fakes:9000/chalkak/staging/local/signatures/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png?"
        ));
        assert!(upload.upload_url.contains("X-Amz-Expires=300"));
        assert!(
            upload
                .upload_url
                .contains("X-Amz-SignedHeaders=content-type%3Bhost")
        );
    }

    #[test]
    fn 처리용_업로드_url은_캐시_헤더와_조건부_쓰기를_서명한다() {
        let upload = issuer(false).issue_processing_upload(id()).unwrap();
        assert_eq!(upload.content_type, "image/png");
        assert_eq!(upload.cache_control, "public, max-age=86400");
        for url in [&upload.original_upload_url, &upload.thumbnail_upload_url] {
            assert!(
                url.contains(
                    "X-Amz-SignedHeaders=cache-control%3Bcontent-type%3Bhost%3Bif-none-match"
                ),
                "{url}"
            );
        }
        assert!(
            upload
                .original_upload_url
                .contains("/signatures/local/original/")
        );
        assert!(
            upload
                .thumbnail_upload_url
                .contains("/signatures/local/thumbnail/")
        );
    }

    #[test]
    fn 익명_접근이면_서명할_수_없어_500이다() {
        assert_eq!(issuer(true).issue(id()).unwrap_err().status(), 500);
        assert_eq!(
            issuer(true)
                .issue_processing_upload(id())
                .unwrap_err()
                .status(),
            500
        );
    }

    // ----- S3SignatureImageStorageTest, S3SignatureImageUploadIssuerTest (원본 메서드 이름) -----

    fn dev_settings() -> ImageSettings {
        let mut s = settings();
        s.environment = "dev".into();
        s.base_url = "https://cdn.example.com".into();
        s
    }

    #[test]
    fn to_storage_keys_upload_id_creates_original_and_thumbnail_keys() {
        let upload_id = Uuid::new_v4();
        let storage = S3SignatureImageStorage::new(&dev_settings(), FakeHead::new(vec![]));
        let keys = storage.to_storage_keys(upload_id);
        assert_eq!(
            keys.original,
            format!("chalkak/signatures/dev/original/{upload_id}.png")
        );
        assert_eq!(
            keys.thumbnail,
            format!("chalkak/signatures/dev/thumbnail/{upload_id}.png")
        );
    }

    #[test]
    fn to_storage_keys_prod_environment_creates_prod_keys() {
        let upload_id = Uuid::new_v4();
        let mut s = dev_settings();
        s.environment = "prod".into();
        let storage = S3SignatureImageStorage::new(&s, FakeHead::new(vec![]));
        let keys = storage.to_storage_keys(upload_id);
        assert_eq!(
            keys.original,
            format!("chalkak/signatures/prod/original/{upload_id}.png")
        );
        assert_eq!(
            keys.thumbnail,
            format!("chalkak/signatures/prod/thumbnail/{upload_id}.png")
        );
        assert_eq!(
            storage.to_staging_storage_key(upload_id),
            format!("chalkak/staging/prod/signatures/{upload_id}.png")
        );
    }

    #[test]
    fn to_image_url_original_storage_key_excludes_root_prefix() {
        let upload_id = Uuid::new_v4();
        let storage = S3SignatureImageStorage::new(&dev_settings(), FakeHead::new(vec![]));
        let key = storage.to_storage_keys(upload_id).original;
        assert_eq!(
            storage.to_image_url(&key).unwrap(),
            format!("https://cdn.example.com/signatures/dev/original/{upload_id}.png")
        );
    }

    #[test]
    fn is_processing_completed_existing_thumbnail_returns_true() {
        let upload_id = Uuid::new_v4();
        let head = FakeHead::new(vec![("/thumbnail/", Ok(Some(png())))]);
        let storage = S3SignatureImageStorage::new(&dev_settings(), head.clone());
        assert!(storage.is_processing_completed(upload_id).unwrap());
        assert_eq!(
            *head.calls.lock().unwrap(),
            vec![format!("chalkak/signatures/dev/thumbnail/{upload_id}.png")]
        );
    }

    #[test]
    fn to_image_url_storage_key_without_root_prefix_throws_illegal_argument_exception() {
        let storage = S3SignatureImageStorage::new(&dev_settings(), FakeHead::new(vec![]));
        let key = format!("signatures/original/{}.png", Uuid::new_v4());
        let error = storage.to_image_url(&key).unwrap_err();
        assert!(
            matches!(&error, ApiError::Internal(m) if m == "스토리지 키는 root-prefix로 시작해야 합니다: chalkak")
        );
    }

    #[test]
    fn find_uploaded_image_no_such_key_returns_empty() {
        let storage = S3SignatureImageStorage::new(&dev_settings(), FakeHead::new(vec![]));
        assert_eq!(storage.find_uploaded_image(Uuid::new_v4()).unwrap(), None);
    }

    #[test]
    fn find_uploaded_image_existing_staging_image_uses_signature_staging_path() {
        let upload_id = Uuid::new_v4();
        let head = FakeHead::new(vec![("/staging/", Ok(Some(png())))]);
        let storage = S3SignatureImageStorage::new(&dev_settings(), head.clone());
        storage.find_uploaded_image(upload_id).unwrap();
        assert_eq!(
            *head.calls.lock().unwrap(),
            vec![format!("chalkak/staging/dev/signatures/{upload_id}.png")]
        );
    }

    /// 요청 한 건에 `status` 줄만 돌려주는 한 번짜리 서버. 받은 요청 첫 줄을 넘긴다.
    fn serve_status_once(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let n = stream.read(&mut buffer).unwrap();
            let first_line = String::from_utf8_lossy(&buffer[..n])
                .lines()
                .next()
                .unwrap_or("")
                .to_owned();
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            stream.write_all(response.as_bytes()).unwrap();
            first_line
        });
        (url, handle)
    }

    #[test]
    fn find_uploaded_image_access_denied_returns_empty() {
        // ListBucket 권한이 없으면 S3는 객체 존재를 숨기려고 404 대신 403을 준다.
        for status in ["403 Forbidden", "404 Not Found"] {
            let (url, server) = serve_status_once(status);
            let mut s = dev_settings();
            s.endpoint = chalkak_aws::S3Endpoint::custom(&url, true).unwrap();
            let clock = Arc::new(chalkak_core::FixedClock::new(
                "2026-10-12T00:00:00Z".parse().unwrap(),
            ));
            let head = HttpHeadObjects::new(&s, Some(Arc::new(StaticCredentials)), clock).unwrap();
            assert_eq!(head.head("chalkak/staging/dev/signatures/a.png"), Ok(None));
            let first_line = server.join().unwrap();
            assert!(
                first_line.starts_with("HEAD /test-bucket/chalkak/staging/dev/signatures/a.png"),
                "{first_line}"
            );
        }
    }

    #[test]
    fn find_uploaded_image_server_error_propagates() {
        let head = FakeHead::new(vec![
            ("/staging/", Err(HeadFailure::Http(500))),
            ("/original/", Err(HeadFailure::Http(500))),
        ]);
        let storage = S3SignatureImageStorage::new(&dev_settings(), head);
        assert_eq!(
            storage
                .find_uploaded_image(Uuid::new_v4())
                .unwrap_err()
                .status(),
            500
        );
    }

    #[test]
    fn issue_processing_upload_presigns_original_and_thumbnail_keys() {
        let upload = issuer(false).issue_processing_upload(id()).unwrap();
        assert_eq!(upload.content_type, "image/png");
        assert_eq!(upload.cache_control, "public, max-age=86400");
        assert!(upload.original_upload_url.contains(
            "/chalkak/signatures/local/original/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png?"
        ));
        assert!(upload.thumbnail_upload_url.contains(
            "/chalkak/signatures/local/thumbnail/0190f3a2-7b1c-7d2e-8f4a-123456789abc.png?"
        ));
        for url in [&upload.original_upload_url, &upload.thumbnail_upload_url] {
            assert!(url.contains("X-Amz-Expires=300"), "{url}");
            // 덮어쓰기 방지(`If-None-Match: *`)와 캐시 헤더, 콘텐츠 타입이 서명에 들어 있다.
            assert!(
                url.contains(
                    "X-Amz-SignedHeaders=cache-control%3Bcontent-type%3Bhost%3Bif-none-match"
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn issue_dev_environment_presigns_dev_signature_upload() {
        let s = dev_settings();
        let storage = Arc::new(S3SignatureImageStorage::new(&s, FakeHead::new(vec![])));
        let clock = Arc::new(chalkak_core::FixedClock::new(
            "2026-10-12T00:00:00Z".parse().unwrap(),
        ));
        let issuer = S3SignatureImageUploadIssuer::new(
            &s,
            Some(Arc::new(StaticCredentials)),
            storage,
            clock,
        );
        let upload_id = Uuid::new_v4();
        let upload = issuer.issue(upload_id).unwrap();
        assert_eq!(upload.upload_id, upload_id);
        assert_eq!(upload.expires_in_seconds, 300);
        assert!(
            upload
                .upload_url
                .contains(&format!("/chalkak/staging/dev/signatures/{upload_id}.png?"))
        );
        assert!(upload.upload_url.contains("X-Amz-Expires=300"));
        assert!(
            upload
                .upload_url
                .contains("X-Amz-SignedHeaders=content-type%3Bhost")
        );
    }
}
