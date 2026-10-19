//! CloudWatch Logs 요청 서명(JSON 1.1 프로토콜). 원본 `CloudWatchLogsClient`(AWS SDK for Java v2)가 보내는 요청과 같은
//! 모양이다.
//!
//! - 모든 호출이 `POST /`이고 동작은 `X-Amz-Target: Logs_20140328.<동작>` 헤더로 고른다. 본문은 JSON이다.
//! - 서명 헤더는 `content-type`, `host`, `x-amz-date`, `x-amz-target`(·`x-amz-security-token`)이다. 본문은 SHA-256으로
//!   서명한다(S3와 달리 `UNSIGNED-PAYLOAD`가 아니다). 경로는 S3와 달리 정규화·이중 인코딩 규칙을 따르는데 `/` 하나라 같다.
//! - 기본 엔드포인트는 `https://logs.<region>.amazonaws.com`이다. `AWS_ENDPOINT_URL_CLOUDWATCH_LOGS`(없으면
//!   `AWS_ENDPOINT_URL`)로 바꾼다(하네스 가짜 서버, 로컬 스택).

use jiff::Timestamp;

use crate::credentials::Credentials;
use crate::error::AwsError;
use crate::s3::SignedRequest;
use crate::sigv4::{self, Request, Scope, UriMode};

const SERVICE: &str = "logs";
const CONTENT_TYPE: &str = "application/x-amz-json-1.1";
const TARGET_PREFIX: &str = "Logs_20140328.";

/// CloudWatch Logs 호출 대상.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogsEndpoint {
    scheme: String,
    /// `Host` 헤더 값(비표준 포트면 포트 포함).
    authority: String,
}

impl LogsEndpoint {
    /// 리전별 기본 엔드포인트.
    pub fn aws(region: &str) -> Result<Self, AwsError> {
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
        Ok(Self {
            scheme: "https".to_owned(),
            authority: format!("logs.{region}.{suffix}"),
        })
    }

    /// `http(s)://host[:port]` 형태의 사용자 지정 엔드포인트.
    pub fn custom(url: &str) -> Result<Self, AwsError> {
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
        Ok(Self {
            scheme: scheme.to_owned(),
            authority: authority.to_owned(),
        })
    }

    /// `AWS_ENDPOINT_URL_CLOUDWATCH_LOGS`, 없으면 `AWS_ENDPOINT_URL`을 읽는다(비어 있으면 없는 것으로 본다).
    pub fn from_env(
        region: &str,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, AwsError> {
        let value = ["AWS_ENDPOINT_URL_CLOUDWATCH_LOGS", "AWS_ENDPOINT_URL"]
            .iter()
            .find_map(|name| lookup(name).filter(|v| !v.trim().is_empty()));
        match value {
            Some(url) => Self::custom(url.trim()),
            None => Self::aws(region),
        }
    }

    pub fn from_process_env(region: &str) -> Result<Self, AwsError> {
        Self::from_env(region, &|name| std::env::var(name).ok())
    }

    pub fn url(&self) -> String {
        format!("{}://{}/", self.scheme, self.authority)
    }
}

/// 동작 하나(`StartQuery`, `GetQueryResults`, `StopQuery`)의 JSON 본문에 서명한다. 호출 측 HTTP 클라이언트는
/// 돌려받은 `headers`를 그대로 실어 `POST url`로 `body`를 보낸다.
pub fn sign_json_request(
    credentials: &Credentials,
    region: &str,
    endpoint: &LogsEndpoint,
    action: &str,
    body: &[u8],
    now: Timestamp,
) -> SignedRequest {
    let amz_date = sigv4::amz_date(now);
    let target = format!("{TARGET_PREFIX}{action}");
    let payload_hash = sigv4::sha256_hex(body);

    let mut headers: Vec<(&str, &str)> = vec![
        ("content-type", CONTENT_TYPE),
        ("host", &endpoint.authority),
        ("x-amz-date", &amz_date),
        ("x-amz-target", &target),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token", token));
    }

    let scope = Scope {
        region,
        service: SERVICE,
    };
    let canonical = sigv4::canonical_request(&Request {
        method: "POST",
        path: "/",
        query: &[],
        headers: &headers,
        payload_hash: &payload_hash,
        uri_mode: UriMode::DoubleEncodeNormalize,
    });
    let string_to_sign = sigv4::string_to_sign(&scope, now, &canonical.text);
    let key = sigv4::signing_key(
        &credentials.secret_access_key,
        &sigv4::date_stamp(now),
        region,
        SERVICE,
    );
    let signature = sigv4::sign(&key, &string_to_sign);
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
    SignedRequest {
        method: "POST",
        url: endpoint.url(),
        headers: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(token: Option<&str>) -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: token.map(str::to_owned),
        }
    }

    fn at() -> Timestamp {
        "2026-10-19T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn 기본_엔드포인트는_리전별_logs_호스트다() {
        assert_eq!(
            LogsEndpoint::aws("ap-northeast-2").unwrap().url(),
            "https://logs.ap-northeast-2.amazonaws.com/"
        );
        assert_eq!(
            LogsEndpoint::aws("cn-north-1").unwrap().url(),
            "https://logs.cn-north-1.amazonaws.com.cn/"
        );
        assert!(LogsEndpoint::aws("AP/../x").is_err());
        assert!(LogsEndpoint::aws("").is_err());
    }

    #[test]
    fn 사용자_지정_엔드포인트는_스킴과_호스트만_받는다() {
        assert_eq!(
            LogsEndpoint::custom("http://fakes:9000/").unwrap().url(),
            "http://fakes:9000/"
        );
        for bad in [
            "fakes:9000",
            "ftp://fakes",
            "http://",
            "http://u@fakes",
            "http://fakes/p",
        ] {
            assert!(LogsEndpoint::custom(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn 환경변수는_서비스별_값이_공통_값보다_앞선다() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let both = env(&[
            ("AWS_ENDPOINT_URL_CLOUDWATCH_LOGS", "http://logs:1"),
            ("AWS_ENDPOINT_URL", "http://common:2"),
        ]);
        assert_eq!(
            LogsEndpoint::from_env("ap-northeast-2", &both)
                .unwrap()
                .url(),
            "http://logs:1/"
        );
        let common = env(&[("AWS_ENDPOINT_URL", "http://common:2")]);
        assert_eq!(
            LogsEndpoint::from_env("ap-northeast-2", &common)
                .unwrap()
                .url(),
            "http://common:2/"
        );
        let blank = env(&[("AWS_ENDPOINT_URL_CLOUDWATCH_LOGS", "  ")]);
        assert_eq!(
            LogsEndpoint::from_env("ap-northeast-2", &blank)
                .unwrap()
                .url(),
            "https://logs.ap-northeast-2.amazonaws.com/"
        );
    }

    #[test]
    fn 서명_헤더는_content_type_host_date_target이고_본문은_해시로_서명한다() {
        let endpoint = LogsEndpoint::aws("ap-northeast-2").unwrap();
        let body = br#"{"queryId":"q"}"#;
        let signed = sign_json_request(
            &credentials(None),
            "ap-northeast-2",
            &endpoint,
            "GetQueryResults",
            body,
            at(),
        );
        assert_eq!(signed.method, "POST");
        assert_eq!(signed.url, "https://logs.ap-northeast-2.amazonaws.com/");
        assert_eq!(
            signed.header("content-type"),
            Some("application/x-amz-json-1.1")
        );
        assert_eq!(
            signed.header("x-amz-target"),
            Some("Logs_20140328.GetQueryResults")
        );
        assert_eq!(signed.header("x-amz-date"), Some("20261019T000000Z"));
        let auth = signed.header("authorization").unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261019/ap-northeast-2/logs/aws4_request, "
        ));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-target, "));

        // 같은 입력으로 정규 요청을 다시 만들어 서명이 본문 해시를 쓰는지 확인한다.
        let hash = sigv4::sha256_hex(body);
        let headers = [
            ("content-type", "application/x-amz-json-1.1"),
            ("host", "logs.ap-northeast-2.amazonaws.com"),
            ("x-amz-date", "20261019T000000Z"),
            ("x-amz-target", "Logs_20140328.GetQueryResults"),
        ];
        let canonical = sigv4::canonical_request(&Request {
            method: "POST",
            path: "/",
            query: &[],
            headers: &headers,
            payload_hash: &hash,
            uri_mode: UriMode::DoubleEncodeNormalize,
        });
        let scope = Scope {
            region: "ap-northeast-2",
            service: "logs",
        };
        let key = sigv4::signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20261019",
            "ap-northeast-2",
            "logs",
        );
        let expected = sigv4::sign(&key, &sigv4::string_to_sign(&scope, at(), &canonical.text));
        assert!(auth.ends_with(&format!("Signature={expected}")));
    }

    #[test]
    fn 본문이_다르면_서명이_다르다() {
        let endpoint = LogsEndpoint::aws("ap-northeast-2").unwrap();
        let sign = |body: &[u8]| {
            sign_json_request(
                &credentials(None),
                "ap-northeast-2",
                &endpoint,
                "StartQuery",
                body,
                at(),
            )
            .header("authorization")
            .unwrap()
            .to_owned()
        };
        assert_ne!(sign(b"{}"), sign(b"{\"a\":1}"));
    }

    #[test]
    fn 세션_토큰이_있으면_서명_헤더에_들어간다() {
        let endpoint = LogsEndpoint::custom("http://fakes:9000").unwrap();
        let signed = sign_json_request(
            &credentials(Some("token")),
            "ap-northeast-2",
            &endpoint,
            "StopQuery",
            b"{}",
            at(),
        );
        assert_eq!(signed.header("x-amz-security-token"), Some("token"));
        assert_eq!(signed.header("host"), Some("fakes:9000"));
        assert!(signed.header("authorization").unwrap().contains(
            "SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target, "
        ));
    }
}
