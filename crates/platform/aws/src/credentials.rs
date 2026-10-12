//! 자격증명과 공급자. 환경변수 → IMDSv2 순서(원본 `DefaultCredentialsProvider`가 EC2에서 거치는 길)로 찾는다.
//!
//! HTTP 호출은 [`HttpTransport`] 트레이트 뒤에 둔다. 실제 구현(`chalkak-http` 클라이언트 어댑터)은 조립하는 쪽이 끼우고,
//! 시험은 가짜 구현을 쓴다. 이 crate는 HTTP 클라이언트에 의존하지 않는다.

use std::fmt;
use std::sync::{Arc, Mutex};

use jiff::{SignedDuration, Timestamp};

use crate::error::AwsError;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

// 비밀 값이 로그에 새지 않게 Debug에서 가린다.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

pub trait CredentialsProvider: Send + Sync {
    fn credentials(&self) -> Result<Credentials, AwsError>;
}

// ---- HTTP 추상화 ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET`, `PUT`
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 동기 HTTP 호출. 연결·읽기 타임아웃은 구현이 정한다(IMDS는 짧게: 1~2초 권장).
pub trait HttpTransport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AwsError>;
}

/// 현재 시각 공급 함수. 시험에서 시각을 옮길 수 있게 주입한다.
pub type NowFn = Arc<dyn Fn() -> Timestamp + Send + Sync>;

// ---- 환경변수 ----

type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`). 빈 값은 없는 것으로 본다.
pub struct EnvCredentialsProvider {
    lookup: EnvLookup,
}

impl EnvCredentialsProvider {
    pub fn new(lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            lookup: Box::new(lookup),
        }
    }

    pub fn from_process_env() -> Self {
        Self::new(|name| std::env::var(name).ok())
    }

    fn get(&self, name: &str) -> Option<String> {
        (self.lookup)(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }
}

impl CredentialsProvider for EnvCredentialsProvider {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        match (
            self.get("AWS_ACCESS_KEY_ID"),
            self.get("AWS_SECRET_ACCESS_KEY"),
        ) {
            (Some(access_key_id), Some(secret_access_key)) => Ok(Credentials {
                access_key_id,
                secret_access_key,
                session_token: self.get("AWS_SESSION_TOKEN"),
            }),
            _ => Err(AwsError::NoCredentials(
                "AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY are not set".into(),
            )),
        }
    }
}

// ---- IMDSv2 ----

const IMDS_DEFAULT_ENDPOINT: &str = "http://169.254.169.254";
const IMDS_TOKEN_TTL_SECS: &str = "21600";
/// 만료 5분 전부터 새로 가져온다(SDK의 prefetch 구간과 같은 값).
const REFRESH_BEFORE_EXPIRY: SignedDuration = SignedDuration::from_secs(300);

struct Cached {
    credentials: Credentials,
    expiration: Timestamp,
}

/// EC2 인스턴스 역할 자격증명. IMDSv2: 세션 토큰을 `PUT`으로 받은 뒤 `GET`에 실어 보낸다.
pub struct ImdsCredentialsProvider {
    transport: Arc<dyn HttpTransport>,
    endpoint: String,
    now: NowFn,
    cache: Mutex<Option<Cached>>,
}

impl ImdsCredentialsProvider {
    pub fn new(transport: Arc<dyn HttpTransport>, now: NowFn) -> Self {
        Self {
            transport,
            endpoint: IMDS_DEFAULT_ENDPOINT.to_owned(),
            now,
            cache: Mutex::new(None),
        }
    }

    /// `AWS_EC2_METADATA_SERVICE_ENDPOINT` 처럼 엔드포인트를 바꾼다(하네스의 가짜 IMDS 등).
    pub fn with_endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = endpoint.trim_end_matches('/').to_owned();
        self
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
    ) -> Result<String, AwsError> {
        let response = self.transport.send(&HttpRequest {
            method: method.to_owned(),
            url: format!("{}{path}", self.endpoint),
            headers,
            body: Vec::new(),
        })?;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        if !(200..300).contains(&response.status) {
            return Err(AwsError::Imds {
                status: response.status,
                message: body,
            });
        }
        Ok(body)
    }

    fn fetch(&self) -> Result<Cached, AwsError> {
        const ROLE_PATH: &str = "/latest/meta-data/iam/security-credentials/";
        let token = self.call(
            "PUT",
            "/latest/api/token",
            vec![(
                "X-aws-ec2-metadata-token-ttl-seconds".into(),
                IMDS_TOKEN_TTL_SECS.into(),
            )],
        )?;
        let token_header = || {
            vec![(
                "X-aws-ec2-metadata-token".to_owned(),
                token.trim().to_owned(),
            )]
        };

        let roles = self.call("GET", ROLE_PATH, token_header())?;
        let role = roles
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .ok_or_else(|| AwsError::Parse("IMDS returned no instance profile role".into()))?;

        let body = self.call("GET", &format!("{ROLE_PATH}{role}"), token_header())?;
        parse_role_credentials(&body)
    }
}

fn parse_role_credentials(body: &str) -> Result<Cached, AwsError> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| AwsError::Parse(format!("IMDS credentials JSON: {e}")))?;
    let text = |field: &str| json.get(field).and_then(|v| v.as_str()).map(str::to_owned);

    if let Some(code) = text("Code")
        && code != "Success"
    {
        return Err(AwsError::Parse(format!("IMDS credentials Code={code}")));
    }
    let missing = |field: &str| AwsError::Parse(format!("IMDS credentials missing {field}"));
    let expiration: Timestamp = text("Expiration")
        .ok_or_else(|| missing("Expiration"))?
        .parse()
        .map_err(|e| AwsError::Parse(format!("IMDS Expiration: {e}")))?;
    Ok(Cached {
        credentials: Credentials {
            access_key_id: text("AccessKeyId").ok_or_else(|| missing("AccessKeyId"))?,
            secret_access_key: text("SecretAccessKey").ok_or_else(|| missing("SecretAccessKey"))?,
            session_token: Some(text("Token").ok_or_else(|| missing("Token"))?),
        },
        expiration,
    })
}

impl CredentialsProvider for ImdsCredentialsProvider {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        let now = (self.now)();
        // 갱신하는 동안 락을 쥐어 동시 호출이 IMDS를 중복으로 두드리지 않게 한다.
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref()
            && now
                < cached
                    .expiration
                    .checked_sub(REFRESH_BEFORE_EXPIRY)
                    .unwrap_or(cached.expiration)
        {
            return Ok(cached.credentials.clone());
        }
        match self.fetch() {
            Ok(fresh) => {
                let credentials = fresh.credentials.clone();
                *cache = Some(fresh);
                Ok(credentials)
            }
            // 갱신에 실패해도 아직 만료 전이면 가진 것을 계속 쓴다.
            Err(_) if cache.as_ref().is_some_and(|c| now < c.expiration) => Ok(cache
                .as_ref()
                .map(|c| c.credentials.clone())
                .expect("checked above")),
            Err(e) => Err(e),
        }
    }
}

// ---- 체인 ----

/// 앞에서부터 시도한다. `NoCredentials`면 다음 공급자로, 그 밖의 오류는 그대로 돌려준다.
pub struct ChainCredentialsProvider {
    providers: Vec<Box<dyn CredentialsProvider>>,
}

impl ChainCredentialsProvider {
    pub fn new(providers: Vec<Box<dyn CredentialsProvider>>) -> Self {
        Self { providers }
    }

    /// 환경변수(프로세스 환경) → IMDSv2. 원본 배포(EC2 인스턴스 역할)와 로컬(환경변수)이 모두 걸린다.
    pub fn default_chain(transport: Arc<dyn HttpTransport>, now: NowFn) -> Self {
        Self::new(vec![
            Box::new(EnvCredentialsProvider::from_process_env()),
            Box::new(ImdsCredentialsProvider::new(transport, now)),
        ])
    }
}

impl CredentialsProvider for ChainCredentialsProvider {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        let mut misses = Vec::new();
        for provider in &self.providers {
            match provider.credentials() {
                Ok(c) => return Ok(c),
                Err(AwsError::NoCredentials(m)) => misses.push(m),
                Err(e) => return Err(e),
            }
        }
        Err(AwsError::NoCredentials(if misses.is_empty() {
            "no credentials providers configured".into()
        } else {
            misses.join("; ")
        }))
    }
}
