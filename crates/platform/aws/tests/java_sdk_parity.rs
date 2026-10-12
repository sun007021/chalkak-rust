//! 원본 백엔드가 쓰는 AWS SDK for Java v2(2.54.1)가 만든 presigned PUT URL·HeadObject 서명과 바이트 단위로 같은지 본다.
//!
//! 기대값은 `tests/fixtures/PresignFixture.java`(원본 `S3PostImageUploadIssuer`·`S3SignatureImageUploadIssuer`·
//! `S3ClientConfig`·`S3*ImageStorage.headObject`와 같은 호출)를 `tests/fixtures/run.sh`로 원본 JAR의 SDK에 돌려 만든
//! `expected_*.tsv`다. SDK는 서명 시각을 `Instant.now()`로 고정해 쓰므로, 각 줄의 `X-Amz-Date`가 그 줄의 서명 시각이다.
//! 자격증명은 AKIDEXAMPLE / wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY, 리전 ap-northeast-2.

use std::time::Duration;

use chalkak_aws::credentials::Credentials;
use chalkak_aws::s3::{self, PutObject, S3Endpoint};
use jiff::Timestamp;

const REGION: &str = "ap-northeast-2";
const TOKEN: &str = "IQoJb3JpZ2luX2VjEXAMPLE//token+with/chars==";

fn cred(with_token: bool) -> Credentials {
    Credentials {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        session_token: with_token.then(|| TOKEN.to_owned()),
    }
}

struct Line {
    kind: String,
    bucket: String,
    key: String,
    value: String,
}

fn lines(tsv: &str) -> Vec<Line> {
    tsv.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.splitn(4, '\t');
            Line {
                kind: it.next().unwrap().into(),
                bucket: it.next().unwrap().into(),
                key: it.next().unwrap().into(),
                value: it.next().unwrap().into(),
            }
        })
        .collect()
}

/// `20261012T163924Z` 형태를 Timestamp로.
fn parse_amz_date(s: &str) -> Timestamp {
    let iso = format!(
        "{}-{}-{}T{}:{}:{}Z",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13],
        &s[13..15]
    );
    iso.parse().unwrap()
}

fn query_param<'a>(url: &'a str, name: &str) -> &'a str {
    let query = url.split_once('?').unwrap().1;
    query
        .split('&')
        .find_map(|p| p.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{name} missing in {url}"))
}

fn check_puts(tsv: &str, with_token: bool, endpoint: &S3Endpoint) {
    let puts: Vec<_> = lines(tsv)
        .into_iter()
        .filter(|l| l.kind.starts_with("PUT_"))
        .collect();
    assert_eq!(puts.len(), 6);
    for line in puts {
        let now = parse_amz_date(query_param(&line.value, "X-Amz-Date"));
        let request = match line.kind.as_str() {
            // S3SignatureImageUploadIssuer.issue: contentType 하나만 건 요청
            "PUT_SIMPLE" => PutObject {
                bucket: &line.bucket,
                key: &line.key,
                content_type: "image/png",
                cache_control: None,
                if_none_match: None,
            },
            // S3PostImageUploadIssuer.presignProcessingUpload: contentType + cacheControl + ifNoneMatch("*")
            "PUT_PROCESSING" => PutObject {
                bucket: &line.bucket,
                key: &line.key,
                content_type: "image/webp",
                cache_control: Some("public, max-age=31536000, immutable"),
                if_none_match: Some("*"),
            },
            other => panic!("unknown kind {other}"),
        };
        let url = s3::presign_put(
            &cred(with_token),
            REGION,
            endpoint,
            &request,
            Duration::from_secs(300),
            now,
        )
        .unwrap();
        assert_eq!(
            url, line.value,
            "{} {} {}",
            line.kind, line.bucket, line.key
        );
    }
}

/// SDK가 실제로 보낸 HEAD 요청 줄에서 헤더를 꺼낸다. `amz-sdk-invocation-id`는 호출마다 무작위라 그대로 가져와 서명에 넣는다.
fn sdk_header(value: &str, name: &str) -> String {
    let needle = format!(" | {name}: ");
    let start = value
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} not in {value}"))
        + needle.len();
    let rest = &value[start..];
    rest.split(" | ").next().unwrap().to_owned()
}

fn check_heads(tsv: &str, with_token: bool, endpoint: &S3Endpoint) {
    let heads: Vec<_> = lines(tsv)
        .into_iter()
        .filter(|l| l.kind == "HEAD")
        .collect();
    assert_eq!(heads.len(), 3);
    for line in heads {
        let now = parse_amz_date(&sdk_header(&line.value, "X-Amz-Date"));
        let invocation = sdk_header(&line.value, "amz-sdk-invocation-id");
        let attempt = sdk_header(&line.value, "amz-sdk-request");
        let signed = s3::sign_head_object_with_headers(
            &cred(with_token),
            REGION,
            endpoint,
            &line.bucket,
            &line.key,
            now,
            &[
                ("amz-sdk-invocation-id", &invocation),
                ("amz-sdk-request", &attempt),
            ],
        )
        .unwrap();

        let expected_url = line
            .value
            .strip_prefix("HEAD ")
            .unwrap()
            .split(" | ")
            .next()
            .unwrap();
        assert_eq!(signed.url, expected_url);
        for header in [
            "Authorization",
            "Host",
            "X-Amz-Date",
            "x-amz-content-sha256",
        ] {
            assert_eq!(
                signed
                    .header(header)
                    .unwrap_or_else(|| panic!("{header} missing")),
                sdk_header(&line.value, header),
                "{header} for {} {}",
                line.bucket,
                line.key
            );
        }
        if with_token {
            assert_eq!(signed.header("X-Amz-Security-Token").unwrap(), TOKEN);
        } else {
            assert!(signed.header("X-Amz-Security-Token").is_none());
        }
    }
}

#[test]
fn java_sdk_default_endpoint_without_session_token() {
    let tsv = include_str!("fixtures/expected_default_notoken.tsv");
    check_puts(tsv, false, &S3Endpoint::aws());
    check_heads(tsv, false, &S3Endpoint::aws());
}

#[test]
fn java_sdk_default_endpoint_with_session_token_query_comes_first() {
    let tsv = include_str!("fixtures/expected_default_token.tsv");
    // X-Amz-Security-Token 이 쿼리 맨 앞에 온다.
    let first = lines(tsv)
        .into_iter()
        .find(|l| l.kind == "PUT_SIMPLE")
        .unwrap();
    assert!(first.value.contains("?X-Amz-Security-Token="));
    check_puts(tsv, true, &S3Endpoint::aws());
    check_heads(tsv, true, &S3Endpoint::aws());
}

#[test]
fn java_sdk_aws_endpoint_url_s3_http_virtual_host_with_port() {
    // AWS_ENDPOINT_URL_S3=http://localhost:4566 → http://<bucket>.localhost:4566/<key> (http에서는 점이 있는 버킷도 가상 호스트)
    let endpoint = S3Endpoint::custom("http://localhost:4566", false).unwrap();
    let tsv = include_str!("fixtures/expected_endpoint_http_token.tsv");
    check_puts(tsv, true, &endpoint);
    check_heads(tsv, true, &endpoint);
}

#[test]
fn java_sdk_aws_endpoint_url_s3_https() {
    // AWS_ENDPOINT_URL_S3=https://s3.custom.example.com → https://<bucket>.s3.custom.example.com/<key>, 점이 있는 버킷은 https://s3.custom.example.com/<bucket>/<key>
    let endpoint = S3Endpoint::custom("https://s3.custom.example.com", false).unwrap();
    let tsv = include_str!("fixtures/expected_endpoint_https_token.tsv");
    check_puts(tsv, true, &endpoint);
    check_heads(tsv, true, &endpoint);
}

#[test]
fn endpoint_from_env_prefers_service_specific_variable() {
    let env = |name: &str| match name {
        "AWS_ENDPOINT_URL_S3" => Some("http://localhost:4566".to_owned()),
        "AWS_ENDPOINT_URL" => Some("http://other:1".to_owned()),
        _ => None,
    };
    assert_eq!(
        S3Endpoint::from_env(&env).unwrap(),
        S3Endpoint::custom("http://localhost:4566", false).unwrap()
    );
    let env = |name: &str| (name == "AWS_ENDPOINT_URL").then(|| "http://other:1".to_owned());
    assert_eq!(
        S3Endpoint::from_env(&env).unwrap(),
        S3Endpoint::custom("http://other:1", false).unwrap()
    );
    assert_eq!(
        S3Endpoint::from_env(&|_: &str| None).unwrap(),
        S3Endpoint::aws()
    );
}

#[test]
fn path_style_option_puts_bucket_in_path() {
    let endpoint = S3Endpoint::custom("http://localhost:9000", true).unwrap();
    let now: Timestamp = "2026-10-12T00:00:00Z".parse().unwrap();
    let url = s3::presign_put(
        &cred(false),
        REGION,
        &endpoint,
        &PutObject {
            bucket: "b",
            key: "k/a.png",
            content_type: "image/png",
            cache_control: None,
            if_none_match: None,
        },
        Duration::from_secs(300),
        now,
    )
    .unwrap();
    assert!(
        url.starts_with("http://localhost:9000/b/k/a.png?X-Amz-Algorithm="),
        "{url}"
    );
}

#[test]
fn presign_rejects_expiry_over_seven_days_and_empty_input() {
    let now: Timestamp = "2026-10-12T00:00:00Z".parse().unwrap();
    let put = PutObject {
        bucket: "b",
        key: "k",
        content_type: "image/png",
        cache_control: None,
        if_none_match: None,
    };
    let e = S3Endpoint::aws();
    assert!(
        s3::presign_put(
            &cred(false),
            REGION,
            &e,
            &put,
            Duration::from_secs(604_801),
            now
        )
        .is_err()
    );
    assert!(s3::presign_put(&cred(false), REGION, &e, &put, Duration::from_secs(0), now).is_err());
    let no_key = PutObject { key: "", ..put };
    assert!(
        s3::presign_put(
            &cred(false),
            REGION,
            &e,
            &no_key,
            Duration::from_secs(300),
            now
        )
        .is_err()
    );
}

#[test]
fn sign_head_object_without_extra_headers_signs_host_content_sha_date_token() {
    let now: Timestamp = "2026-10-12T00:00:00Z".parse().unwrap();
    let signed = s3::sign_head_object(
        &cred(true),
        REGION,
        &S3Endpoint::aws(),
        "chalkak-bucket",
        "a/b.png",
        now,
    )
    .unwrap();
    let auth = signed.header("authorization").unwrap();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261012/ap-northeast-2/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature="),
        "{auth}"
    );
    assert_eq!(
        signed.url,
        "https://chalkak-bucket.s3.ap-northeast-2.amazonaws.com/a/b.png"
    );
    assert_eq!(signed.method, "HEAD");
}
