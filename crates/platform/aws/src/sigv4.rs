//! AWS Signature Version 4 기본 연산: 정규 요청, 서명 키 파생, 서명.
//!
//! 이 모듈은 서비스를 모른다. S3 전용 규칙(경로 이중 인코딩 안 함, `UNSIGNED-PAYLOAD`, presign 쿼리 순서)은
//! [`crate::s3`]가 얹는다. 근거: AWS 공식 `aws-sig-v4-test-suite`와 S3 문서 예제(`tests/sigv4_vectors.rs`).

use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use jiff::Timestamp;
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// 정규 요청의 경로(CanonicalURI)를 만드는 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UriMode {
    /// 대부분의 AWS 서비스: `.`·`..`·중복 슬래시를 정규화한 뒤 각 세그먼트를 한 번 더 인코딩한다
    /// (이미 `%20`이면 `%2520`). 공식 테스트 스위트의 규칙.
    DoubleEncodeNormalize,
    /// S3: 호출 측이 이미 인코딩해 둔 경로를 그대로 쓴다. 정규화도, 이중 인코딩도 하지 않는다.
    SingleEncodeAsIs,
}

/// 정규 요청을 만들기 위한 입력. 쿼리 값은 디코딩된 상태로, 경로는 요청 줄에 쓰는 그대로 준다.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    /// 서명에 포함할 헤더 전부(이름 대소문자 무관). 여기 준 헤더가 곧 `SignedHeaders`다.
    pub headers: &'a [(&'a str, &'a str)],
    /// 본문 SHA-256 소문자 hex, 또는 `UNSIGNED-PAYLOAD`.
    pub payload_hash: &'a str,
    pub uri_mode: UriMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalRequest {
    pub text: String,
    /// 세미콜론으로 이은 소문자 헤더 이름 목록.
    pub signed_headers: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
}

pub fn canonical_request(request: &Request<'_>) -> CanonicalRequest {
    let canonical_uri = match request.uri_mode {
        UriMode::SingleEncodeAsIs => {
            if request.path.is_empty() {
                "/".to_owned()
            } else {
                request.path.to_owned()
            }
        }
        UriMode::DoubleEncodeNormalize => percent_encode(&normalize_path(request.path), true),
    };

    let mut query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(k, v)| (percent_encode(k, false), percent_encode(v, false)))
        .collect();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    // 이름 소문자화, 값 앞뒤 공백 제거와 내부 연속 공백 축약, 같은 이름은 쉼표로 합친다.
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in request.headers {
        let name = name.to_ascii_lowercase();
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        match headers.iter_mut().find(|(n, _)| *n == name) {
            Some((_, existing)) => {
                existing.push(',');
                existing.push_str(&value);
            }
            None => headers.push((name, value)),
        }
    }
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let text = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        request.payload_hash
    );
    CanonicalRequest {
        text,
        signed_headers,
    }
}

/// 헤더 목록에서 `SignedHeaders` 값(소문자 이름, 중복 제거, 정렬, 세미콜론 연결)만 뽑는다.
pub fn signed_headers(headers: &[(&str, &str)]) -> String {
    let mut names: Vec<String> = headers
        .iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    names.sort();
    names.dedup();
    names.join(";")
}

pub fn string_to_sign(scope: &Scope<'_>, time: Timestamp, canonical_request: &str) -> String {
    format!(
        "{ALGORITHM}\n{}\n{}\n{}",
        amz_date(time),
        credential_scope(scope, time),
        sha256_hex(canonical_request.as_bytes())
    )
}

/// `YYYYMMDD/region/service/aws4_request`
pub fn credential_scope(scope: &Scope<'_>, time: Timestamp) -> String {
    format!(
        "{}/{}/{}/aws4_request",
        date_stamp(time),
        scope.region,
        scope.service
    )
}

/// 서명 키 파생: HMAC 체인 `AWS4<secret>` → 날짜 → 리전 → 서비스 → `aws4_request`.
pub fn signing_key(
    secret_access_key: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
) -> Vec<u8> {
    let k_date = hmac_sha256(
        format!("AWS4{secret_access_key}").as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// 서명 = hex(HMAC(서명 키, string-to-sign))
pub fn sign(signing_key: &[u8], string_to_sign: &str) -> String {
    hex(&hmac_sha256(signing_key, string_to_sign.as_bytes()))
}

/// `Authorization` 헤더 값.
pub fn authorization_header(
    access_key_id: &str,
    scope: &Scope<'_>,
    time: Timestamp,
    signed_headers: &str,
    signature: &str,
) -> String {
    format!(
        "{ALGORITHM} Credential={access_key_id}/{}, SignedHeaders={signed_headers}, Signature={signature}",
        credential_scope(scope, time)
    )
}

/// `20150830T123600Z`
pub fn amz_date(time: Timestamp) -> String {
    time.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// `20150830`
pub fn date_stamp(time: Timestamp) -> String {
    time.strftime("%Y%m%d").to_string()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC은 임의 길이 키를 받으므로 new_from_slice는 실패하지 않는다.
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// RFC 3986 unreserved(`A-Z a-z 0-9 - . _ ~`)만 그대로 두고 나머지는 UTF-8 바이트별 `%XX`(대문자)로 인코딩한다.
/// `keep_slash`가 참이면 `/`도 그대로 둔다(경로용). AWS SDK의 `SdkHttpUtils.urlEncode*`와 같은 규칙.
pub fn percent_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `a=b&c` 형태의 원본 쿼리 문자열을 디코딩된 (이름, 값) 쌍으로 나눈다. 값이 없으면 빈 문자열.
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(part), String::new()),
        })
        .collect()
}

/// 빈 세그먼트와 `.`을 없애고 `..`은 한 단계 올라간다. 끝 슬래시는 유지한다.
fn normalize_path(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    let mut out = format!("/{}", stack.join("/"));
    if path.ends_with('/') && !stack.is_empty() {
        out.push('/');
    }
    out
}
