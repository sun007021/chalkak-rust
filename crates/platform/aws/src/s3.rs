//! S3 요청 서명: presigned PUT URL과 HeadObject 헤더 서명.
//!
//! 결과는 원본 백엔드가 쓰는 AWS SDK for Java v2(2.54.1) `S3Presigner`·`S3Client`와 바이트 단위로 같다
//! (`tests/java_sdk_parity.rs`). 눈여겨볼 점:
//! - presigned URL 쿼리 순서는 `[X-Amz-Security-Token]`, `X-Amz-Algorithm`, `X-Amz-Date`, `X-Amz-SignedHeaders`,
//!   `X-Amz-Credential`, `X-Amz-Expires`, `X-Amz-Signature`이다(정규 요청 안에서는 이름순으로 정렬).
//! - 서명 헤더는 `host`와 요청에 건 `content-type`(·`cache-control`·`if-none-match`)이다. `content-length`는
//!   서명 대상이 아니라서 업로드 용량은 이 서명으로 강제되지 않는다(원본 `S3PostImageUploadIssuer` 주석 참고).
//! - 본문 해시는 항상 `UNSIGNED-PAYLOAD`다. S3는 경로를 이중 인코딩하지 않는다.
//! - 기본 엔드포인트는 가상 호스트 스타일(`<bucket>.s3.<region>.amazonaws.com`)이고, 버킷 이름에 점이 있으면
//!   경로 스타일이다. `AWS_ENDPOINT_URL_S3`(없으면 `AWS_ENDPOINT_URL`)를 주면 SDK처럼 `<bucket>.<host>[:port]`
//!   가상 호스트로 간다(점이 든 버킷은 http일 때만 가상 호스트, https면 경로 스타일). 경로 스타일 강제는 [`S3Endpoint::custom`]의 옵션으로만 켠다.

use std::time::Duration;

use jiff::Timestamp;

use crate::credentials::Credentials;
use crate::error::AwsError;
use crate::sigv4::{self, Request, Scope, UriMode};

const SERVICE: &str = "s3";
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
/// S3 presigned URL 최대 유효 기간(7일).
const MAX_EXPIRES_SECS: u64 = 604_800;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Endpoint(Kind);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Aws,
    Custom {
        scheme: String,
        authority: String,
        path_style: bool,
    },
}

impl S3Endpoint {
    /// 리전별 기본 엔드포인트.
    pub fn aws() -> Self {
        Self(Kind::Aws)
    }

    /// `http(s)://host[:port]` 형태의 사용자 지정 엔드포인트(로컬 스택, 하네스 가짜 서버 등).
    pub fn custom(url: &str, path_style: bool) -> Result<Self, AwsError> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            AwsError::InvalidInput(format!("endpoint must be http(s)://host[:port]: {url}"))
        })?;
        if scheme != "http" && scheme != "https" {
            return Err(AwsError::InvalidInput(format!(
                "unsupported endpoint scheme: {scheme}"
            )));
        }
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err(AwsError::InvalidInput(format!(
                "endpoint must be http(s)://host[:port]: {url}"
            )));
        }
        Ok(Self(Kind::Custom {
            scheme: scheme.to_owned(),
            authority: authority.to_owned(),
            path_style,
        }))
    }

    /// `AWS_ENDPOINT_URL_S3`, 없으면 `AWS_ENDPOINT_URL`을 읽는다(비어 있으면 없는 것으로 본다). 둘 다 없으면 기본 엔드포인트.
    pub fn from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, AwsError> {
        let value = ["AWS_ENDPOINT_URL_S3", "AWS_ENDPOINT_URL"]
            .iter()
            .find_map(|name| lookup(name).filter(|v| !v.trim().is_empty()));
        match value {
            Some(url) => Self::custom(url.trim(), false),
            None => Ok(Self::aws()),
        }
    }

    pub fn from_process_env() -> Result<Self, AwsError> {
        Self::from_env(&|name| std::env::var(name).ok())
    }

    fn resolve(&self, region: &str, bucket: &str, key: &str) -> Result<Target, AwsError> {
        if bucket.is_empty() || key.is_empty() {
            return Err(AwsError::InvalidInput(
                "bucket and key must not be empty".into(),
            ));
        }
        let encoded_key = sigv4::percent_encode(key, true);
        let (scheme, authority, virtual_host) = match &self.0 {
            Kind::Aws => {
                if region.is_empty()
                    || !region
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                {
                    return Err(AwsError::InvalidInput(format!("invalid region: {region}")));
                }
                let suffix = if region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                (
                    "https".to_owned(),
                    format!("s3.{region}.{suffix}"),
                    is_dns_bucket(bucket, false),
                )
            }
            Kind::Custom {
                scheme,
                authority,
                path_style,
            } => (
                scheme.clone(),
                authority.clone(),
                !path_style && is_dns_bucket(bucket, scheme == "http"),
            ),
        };
        Ok(if virtual_host {
            Target {
                scheme,
                authority: format!("{bucket}.{authority}"),
                path: format!("/{encoded_key}"),
            }
        } else {
            Target {
                scheme,
                authority,
                path: format!("/{}/{encoded_key}", sigv4::percent_encode(bucket, false)),
            }
        })
    }
}

struct Target {
    scheme: String,
    /// `Host` 헤더 값(비표준 포트면 포트 포함).
    authority: String,
    /// 인코딩된 요청 경로.
    path: String,
}

/// S3 가상 호스트에 쓸 수 있는 버킷 이름인가(소문자·숫자·하이픈, 필요하면 점, 3~63자, IP 모양 아님).
fn is_dns_bucket(bucket: &str, allow_dots: bool) -> bool {
    let len_ok = (3..=63).contains(&bucket.len());
    let chars_ok = bucket.bytes().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || (allow_dots && b == b'.')
    });
    let edges_ok = bucket
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bucket
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let labels_ok = bucket.split('.').all(|l| !l.is_empty());
    let looks_like_ip =
        bucket.split('.').count() == 4 && bucket.split('.').all(|l| l.parse::<u8>().is_ok());
    len_ok && chars_ok && edges_ok && labels_ok && !looks_like_ip
}

/// presigned PUT에 서명으로 묶을 객체 속성. 원본 `PutObjectRequest.builder()`에 거는 값과 같다.
#[derive(Clone, Copy, Debug)]
pub struct PutObject<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub content_type: &'a str,
    /// 처리 람다용 업로드에서만 건다(`cacheControl`).
    pub cache_control: Option<&'a str>,
    /// 처리 람다용 업로드에서만 건다(`ifNoneMatch("*")`).
    pub if_none_match: Option<&'a str>,
}

/// presigned PUT URL을 만든다. 클라이언트는 서명된 헤더(`Content-Type` 등)를 같은 값으로 보내야 한다.
pub fn presign_put(
    credentials: &Credentials,
    region: &str,
    endpoint: &S3Endpoint,
    put: &PutObject<'_>,
    expires: Duration,
    now: Timestamp,
) -> Result<String, AwsError> {
    let expires_secs = expires.as_secs();
    if expires_secs == 0 || expires_secs > MAX_EXPIRES_SECS {
        return Err(AwsError::InvalidInput(format!(
            "presign expiry must be 1..={MAX_EXPIRES_SECS} seconds, got {expires_secs}"
        )));
    }
    let target = endpoint.resolve(region, put.bucket, put.key)?;

    let mut headers: Vec<(&str, &str)> = vec![
        ("host", &target.authority),
        ("content-type", put.content_type),
    ];
    if let Some(v) = put.cache_control {
        headers.push(("cache-control", v));
    }
    if let Some(v) = put.if_none_match {
        headers.push(("if-none-match", v));
    }
    let signed_headers = sigv4::signed_headers(&headers);

    let scope = Scope {
        region,
        service: SERVICE,
    };
    let credential = format!(
        "{}/{}",
        credentials.access_key_id,
        sigv4::credential_scope(&scope, now)
    );
    let amz_date = sigv4::amz_date(now);
    let expires_text = expires_secs.to_string();

    // URL에 나오는 순서. 정규 요청에서는 canonical_request가 이름순으로 다시 정렬한다.
    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(token) = &credentials.session_token {
        query.push(("X-Amz-Security-Token", token));
    }
    query.push(("X-Amz-Algorithm", sigv4::ALGORITHM));
    query.push(("X-Amz-Date", &amz_date));
    query.push(("X-Amz-SignedHeaders", &signed_headers));
    query.push(("X-Amz-Credential", &credential));
    query.push(("X-Amz-Expires", &expires_text));

    let canonical = sigv4::canonical_request(&Request {
        method: "PUT",
        path: &target.path,
        query: &query,
        headers: &headers,
        payload_hash: UNSIGNED_PAYLOAD,
        uri_mode: UriMode::SingleEncodeAsIs,
    });
    let signature = compute_signature(credentials, &scope, now, &canonical.text);

    let query_string: String = query
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                sigv4::percent_encode(k, false),
                sigv4::percent_encode(v, false)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    Ok(format!(
        "{}://{}{}?{query_string}&X-Amz-Signature={signature}",
        target.scheme, target.authority, target.path
    ))
}

/// 서명된 요청: 호출 측 HTTP 클라이언트는 `method`로 `url`에 `headers`를 그대로 실어 보낸다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl SignedRequest {
    /// 대소문자 구분 없이 헤더 값을 찾는다.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// HeadObject 요청에 헤더 서명을 한다. 서명 헤더: `host`, `x-amz-content-sha256`, `x-amz-date`, (`x-amz-security-token`).
pub fn sign_head_object(
    credentials: &Credentials,
    region: &str,
    endpoint: &S3Endpoint,
    bucket: &str,
    key: &str,
    now: Timestamp,
) -> Result<SignedRequest, AwsError> {
    sign_head_object_with_headers(credentials, region, endpoint, bucket, key, now, &[])
}

/// [`sign_head_object`]에 호출 측이 추가로 서명할 헤더를 더한다. Java SDK는 `amz-sdk-invocation-id`·`amz-sdk-request`도
/// 서명에 넣는데, 바이트 비교 시험이 그 값을 그대로 넣어 보려고 쓴다.
pub fn sign_head_object_with_headers(
    credentials: &Credentials,
    region: &str,
    endpoint: &S3Endpoint,
    bucket: &str,
    key: &str,
    now: Timestamp,
    extra_headers: &[(&str, &str)],
) -> Result<SignedRequest, AwsError> {
    let target = endpoint.resolve(region, bucket, key)?;
    let amz_date = sigv4::amz_date(now);

    let mut headers: Vec<(&str, &str)> = vec![
        ("host", &target.authority),
        ("x-amz-content-sha256", UNSIGNED_PAYLOAD),
        ("x-amz-date", &amz_date),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token", token));
    }
    headers.extend_from_slice(extra_headers);

    let scope = Scope {
        region,
        service: SERVICE,
    };
    let canonical = sigv4::canonical_request(&Request {
        method: "HEAD",
        path: &target.path,
        query: &[],
        headers: &headers,
        payload_hash: UNSIGNED_PAYLOAD,
        uri_mode: UriMode::SingleEncodeAsIs,
    });
    let signature = compute_signature(credentials, &scope, now, &canonical.text);
    let authorization = sigv4::authorization_header(
        &credentials.access_key_id,
        &scope,
        now,
        &canonical.signed_headers,
        &signature,
    );

    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    out.push(("authorization".to_owned(), authorization));
    Ok(SignedRequest {
        method: "HEAD",
        url: format!("{}://{}{}", target.scheme, target.authority, target.path),
        headers: out,
    })
}

fn compute_signature(
    credentials: &Credentials,
    scope: &Scope<'_>,
    now: Timestamp,
    canonical_request: &str,
) -> String {
    let string_to_sign = sigv4::string_to_sign(scope, now, canonical_request);
    let key = sigv4::signing_key(
        &credentials.secret_access_key,
        &sigv4::date_stamp(now),
        scope.region,
        scope.service,
    );
    sigv4::sign(&key, &string_to_sign)
}
