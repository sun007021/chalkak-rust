//! 게시물 이미지용 S3 호출 구현. 원본 `S3Client.headObject`와 `S3Presigner.presignPutObject`.
//!
//! 서명은 `chalkak-aws`가 하고, 이 모듈은 자격증명 공급자를 `chalkak-http` 클라이언트에 이어 붙이고 HeadObject를
//! 보내 상태 코드를 해석한다.
//! - 200대: 있음. 403·404: 없음(권한이 없는 키와 없는 키를 구분하지 않는다). 그 밖의 4xx·5xx와 전송 실패는 오류다.
//! - 5xx와 전송 실패는 SDK 기본 재시도처럼 최대 4번(처음 1번 + 재시도 3번) 시도한다.
//! - 익명 자격증명(`chalkak.image.anonymous-access`)으로는 presigned URL을 만들 수 없다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_app::post::ImageSettings;
use chalkak_app::post::storage::{ObjectStore, PresignPut, StorageError};
use chalkak_aws::{
    AwsError, ChainCredentialsProvider, Credentials, CredentialsProvider, HttpRequest,
    HttpResponse, HttpTransport, PutObject, S3Endpoint, presign_put, sign_head_object,
};
use chalkak_core::Clock;
use chalkak_http::client::{ClientError, HttpClient, Method};

/// SDK 기본 재시도 설정(`amz-sdk-request: attempt=1; max=4`).
const MAX_ATTEMPTS: u32 = 4;
const BASE_BACKOFF: Duration = Duration::from_millis(100);

/// `chalkak-aws`의 `HttpTransport`를 `chalkak-http` 클라이언트로 구현한다(IMDS 호출용).
struct ClientTransport(HttpClient);

impl HttpTransport for ClientTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AwsError> {
        let method = match request.method.as_str() {
            "GET" => Method::Get,
            "PUT" => Method::Put,
            "POST" => Method::Post,
            "HEAD" => Method::Head,
            "DELETE" => Method::Delete,
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
            .0
            .request(method, &request.url, &headers, body)
            .map_err(|e: ClientError| AwsError::Transport(e.to_string()))?;
        Ok(HttpResponse {
            status: response.status,
            body: response.body,
        })
    }
}

pub struct S3ObjectStore {
    client: HttpClient,
    /// 익명 모드면 `None`.
    credentials: Option<Arc<dyn CredentialsProvider>>,
    region: String,
    endpoint: S3Endpoint,
    clock: Arc<dyn Clock>,
}

impl S3ObjectStore {
    pub fn new(
        image: &ImageSettings,
        endpoint_url: Option<&str>,
        clock: Arc<dyn Clock>,
    ) -> Result<S3ObjectStore, Box<dyn std::error::Error>> {
        let endpoint = match endpoint_url {
            Some(url) => S3Endpoint::custom(url, false)?,
            None => S3Endpoint::aws(),
        };
        // S3 호출(SDK 기본: 연결 2초, 응답 30초). 자격증명 서버(IMDS)는 짧게 끊는다.
        let client = HttpClient::builder()
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        let credentials: Option<Arc<dyn CredentialsProvider>> = if image.anonymous_access {
            None
        } else {
            let imds = HttpClient::builder()
                .connect_timeout(Duration::from_secs(1))
                .read_timeout(Duration::from_secs(1))
                .build()?;
            let now_clock = Arc::clone(&clock);
            Some(Arc::new(ChainCredentialsProvider::default_chain(
                Arc::new(ClientTransport(imds)),
                Arc::new(move || now_clock.now()),
            )))
        };
        Ok(S3ObjectStore {
            client,
            credentials,
            region: image.region.clone(),
            endpoint,
            clock,
        })
    }

    fn credentials(&self) -> Result<Credentials, StorageError> {
        match &self.credentials {
            Some(provider) => provider
                .credentials()
                .map_err(|e| StorageError(e.to_string())),
            None => Err(StorageError(
                "익명 자격증명으로는 서명할 수 없습니다.".to_owned(),
            )),
        }
    }

    /// HeadObject 한 번. 일시적 실패(5xx, 전송 오류)는 `Err(true)`, 되풀이해도 소용없는 실패는 `Err(false)`.
    fn head_once(&self, bucket: &str, key: &str) -> Result<bool, (bool, StorageError)> {
        let signed = match &self.credentials {
            Some(_) => {
                let credentials = self.credentials().map_err(|e| (false, e))?;
                sign_head_object(
                    &credentials,
                    &self.region,
                    &self.endpoint,
                    bucket,
                    key,
                    self.clock.now(),
                )
            }
            // 익명: 서명 없이 요청한다(공개 읽기 버킷).
            None => sign_head_object(
                &Credentials {
                    access_key_id: "anonymous".to_owned(),
                    secret_access_key: "anonymous".to_owned(),
                    session_token: None,
                },
                &self.region,
                &self.endpoint,
                bucket,
                key,
                self.clock.now(),
            )
            .map(|mut request| {
                request.headers.clear();
                request
            }),
        }
        .map_err(|e| (false, StorageError(e.to_string())))?;

        let headers: Vec<(&str, &str)> = signed
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        match self
            .client
            .request(Method::Head, &signed.url, &headers, None)
        {
            Ok(response) => match response.status {
                200..=299 => Ok(true),
                403 | 404 => Ok(false),
                500..=599 => Err((
                    true,
                    StorageError(format!(
                        "S3 HeadObject가 {}를 돌려줬습니다.",
                        response.status
                    )),
                )),
                status => Err((
                    false,
                    StorageError(format!("S3 HeadObject가 {status}를 돌려줬습니다.")),
                )),
            },
            Err(e) => Err((true, StorageError(e.to_string()))),
        }
    }
}

impl ObjectStore for S3ObjectStore {
    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool, StorageError> {
        let mut attempt = 1;
        loop {
            match self.head_once(bucket, key) {
                Ok(exists) => return Ok(exists),
                Err((retryable, error)) => {
                    if !retryable || attempt >= MAX_ATTEMPTS {
                        return Err(error);
                    }
                    std::thread::sleep(BASE_BACKOFF * 2u32.pow(attempt - 1));
                    attempt += 1;
                }
            }
        }
    }

    fn presign_put(&self, request: &PresignPut) -> Result<String, StorageError> {
        let credentials = self.credentials()?;
        presign_put(
            &credentials,
            &self.region,
            &self.endpoint,
            &PutObject {
                bucket: &request.bucket,
                key: &request.key,
                content_type: &request.content_type,
                cache_control: request.cache_control.as_deref(),
                if_none_match: request.if_none_match.as_deref(),
            },
            request.expires,
            self.clock.now(),
        )
        .map_err(|e| StorageError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    //! 원본 `S3PostImageStorageTest`(HeadObject 해석)와 `S3PostImageUploadIssuerTest`(서명 헤더)의 어댑터 쪽 확인.
    //! HeadObject는 한 줄 응답만 돌려주는 작은 TCP 서버로 받고, 서명은 `chalkak-aws`의 실제 구현으로 만든다.

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use chalkak_core::FixedClock;

    use super::*;

    struct Fixed;

    impl CredentialsProvider for Fixed {
        fn credentials(&self) -> Result<Credentials, AwsError> {
            Ok(Credentials {
                access_key_id: "AKIDEXAMPLE".to_owned(),
                secret_access_key: "secret".to_owned(),
                session_token: None,
            })
        }
    }

    struct Fake {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    /// 요청마다 `statuses`의 다음 상태 코드로 답하는 서버(다 쓰면 마지막 값을 되풀이한다).
    fn serve(statuses: Vec<u16>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or("").to_owned());
                let status = statuses[i.min(statuses.len() - 1)];
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
        });
        Fake { endpoint, requests }
    }

    fn store(endpoint: &str) -> S3ObjectStore {
        S3ObjectStore {
            client: HttpClient::builder().build().unwrap(),
            credentials: Some(Arc::new(Fixed)),
            region: "ap-northeast-2".to_owned(),
            endpoint: S3Endpoint::custom(endpoint, true).unwrap(),
            clock: Arc::new(FixedClock::new("2026-10-12T03:00:00Z".parse().unwrap())),
        }
    }

    fn put_request() -> PresignPut {
        PresignPut {
            bucket: "test-bucket".to_owned(),
            key: "chalkak/posts/dev/original/a.webp".to_owned(),
            content_type: "image/webp".to_owned(),
            cache_control: Some("public, max-age=86400".to_owned()),
            if_none_match: Some("*".to_owned()),
            expires: Duration::from_secs(300),
        }
    }

    // S3PostImageStorageTest#existsUploadedImage_existingStagingObject_returnsTrue
    #[test]
    fn exists_uploaded_image_existing_staging_object_returns_true() {
        let fake = serve(vec![200]);
        assert!(
            store(&fake.endpoint)
                .object_exists("test-bucket", "chalkak/staging/dev/posts/a.webp")
                .unwrap()
        );
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("HEAD /test-bucket/chalkak/staging/dev/posts/a.webp "),
            "{requests:?}"
        );
    }

    // S3PostImageStorageTest#existsUploadedImage_noSuchKey_returnsFalse
    #[test]
    fn exists_uploaded_image_no_such_key_returns_false() {
        let fake = serve(vec![404]);
        assert!(!store(&fake.endpoint).object_exists("b", "k").unwrap());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
    }

    // S3PostImageStorageTest#existsUploadedImage_notFound_returnsFalse
    #[test]
    fn exists_uploaded_image_not_found_returns_false() {
        let fake = serve(vec![404]);
        assert!(!store(&fake.endpoint).object_exists("b", "k").unwrap());
    }

    // S3PostImageStorageTest#existsUploadedImage_accessDenied_returnsFalse
    #[test]
    fn exists_uploaded_image_access_denied_returns_false() {
        let fake = serve(vec![403]);
        assert!(!store(&fake.endpoint).object_exists("b", "k").unwrap());
    }

    // S3PostImageStorageTest#existsUploadedImage_serverError_propagates
    #[test]
    fn exists_uploaded_image_server_error_propagates() {
        let fake = serve(vec![500]);
        let error = store(&fake.endpoint).object_exists("b", "k").unwrap_err();
        assert!(error.0.contains("500"), "{error}");
        // SDK 기본 재시도처럼 처음 한 번과 재시도 세 번까지 시도한다.
        assert_eq!(fake.requests.lock().unwrap().len(), 4);
    }

    // 원본에는 없는 Rust 전용 확인: 일시적 5xx 뒤 성공하면 그 결과를 쓴다.
    #[test]
    fn exists_uploaded_image_retries_transient_server_error() {
        let fake = serve(vec![503, 200]);
        assert!(store(&fake.endpoint).object_exists("b", "k").unwrap());
        assert_eq!(fake.requests.lock().unwrap().len(), 2);
    }

    // 원본에는 없는 Rust 전용 확인: 되풀이해도 소용없는 4xx는 재시도하지 않는다.
    #[test]
    fn exists_uploaded_image_does_not_retry_client_error() {
        let fake = serve(vec![400]);
        assert!(store(&fake.endpoint).object_exists("b", "k").is_err());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
    }

    // S3PostImageUploadIssuerTest#issueProcessingUpload_presignedUrlSignsRequiredHeaders
    #[test]
    fn issue_processing_upload_presigned_url_signs_required_headers() {
        let url = store("http://127.0.0.1:9")
            .presign_put(&put_request())
            .unwrap();
        let signed = url
            .split("X-Amz-SignedHeaders=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .unwrap()
            .replace("%3B", ";");
        for header in ["cache-control", "content-type", "host", "if-none-match"] {
            assert!(signed.split(';').any(|h| h == header), "{signed}");
        }
        assert!(url.contains("X-Amz-Expires=300"));
    }

    // 원본에는 없는 Rust 전용 확인: 익명 모드는 서명 URL을 만들 수 없다.
    #[test]
    fn presign_put_without_credentials_fails() {
        let mut anonymous = store("http://127.0.0.1:9");
        anonymous.credentials = None;
        let error = anonymous.presign_put(&put_request()).unwrap_err();
        assert!(error.0.contains("익명"));
    }
}
