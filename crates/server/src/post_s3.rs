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
