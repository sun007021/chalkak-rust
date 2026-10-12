//! AWS 공식 SigV4 테스트 스위트(aws-sig-v4-test-suite)와 S3 문서 예제로 SigV4 구현을 검증한다.
//! 케이스 이름은 스위트의 디렉터리 이름을 그대로 쓴다.

use chalkak_aws::sigv4::{
    self, CanonicalRequest, Request, Scope, UriMode, authorization_header, canonical_request,
    parse_query, sha256_hex, signing_key, string_to_sign,
};
use jiff::Timestamp;

const SUITE_ACCESS_KEY: &str = "AKIDEXAMPLE";
const SUITE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn suite_time() -> Timestamp {
    "2015-08-30T12:36:00Z".parse().unwrap()
}

struct Outcome {
    canonical: CanonicalRequest,
    string_to_sign: String,
    authorization: String,
}

/// 테스트 스위트 케이스 하나를 서명한다(리전 us-east-1, 서비스 `service`).
fn run_suite_case(
    method: &str,
    path: &str,
    raw_query: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Outcome {
    let query = parse_query(raw_query);
    let query_refs: Vec<(&str, &str)> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let payload_hash = sha256_hex(body);
    let request = Request {
        method,
        path,
        query: &query_refs,
        headers,
        payload_hash: &payload_hash,
        uri_mode: UriMode::DoubleEncodeNormalize,
    };
    let canonical = canonical_request(&request);
    let scope = Scope {
        region: "us-east-1",
        service: "service",
    };
    let sts = string_to_sign(&scope, suite_time(), &canonical.text);
    let key = signing_key(
        SUITE_SECRET,
        &sigv4::date_stamp(suite_time()),
        "us-east-1",
        "service",
    );
    let signature = sigv4::sign(&key, &sts);
    let authorization = authorization_header(
        SUITE_ACCESS_KEY,
        &scope,
        suite_time(),
        &canonical.signed_headers,
        &signature,
    );
    Outcome {
        canonical,
        string_to_sign: sts,
        authorization,
    }
}

#[test]
fn get_vanilla() {
    let out = run_suite_case(
        "GET",
        "/",
        "",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert_eq!(
        out.canonical.text,
        format!(
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{EMPTY_SHA256}"
        )
    );
    assert_eq!(
        out.string_to_sign,
        "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
    );
    assert_eq!(
        out.authorization,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
    );
}

#[test]
fn get_vanilla_query_order_key_case() {
    let out = run_suite_case(
        "GET",
        "/",
        "Param2=value2&Param1=value1",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(
        out.canonical
            .text
            .starts_with("GET\n/\nParam1=value1&Param2=value2\n")
    );
    assert!(
        out.authorization.ends_with(
            "Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        )
    );
}

#[test]
fn get_vanilla_query_order_value() {
    // 같은 키의 값은 값 순서로 정렬한다.
    let out = run_suite_case(
        "GET",
        "/",
        "Param1=value2&Param1=value1",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(
        out.canonical
            .text
            .starts_with("GET\n/\nParam1=value1&Param1=value2\n")
    );
}

#[test]
fn post_x_www_form_urlencoded() {
    let out = run_suite_case(
        "POST",
        "/",
        "",
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"Param1=value1",
    );
    assert!(
        out.authorization
            .contains("SignedHeaders=content-type;host;x-amz-date")
    );
    assert!(
        out.authorization.ends_with(
            "Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        )
    );
}

#[test]
fn get_header_value_multiline_and_trim() {
    // get-header-value-trim: 앞뒤 공백 제거, 내부 연속 공백은 하나로.
    let out = run_suite_case(
        "GET",
        "/",
        "",
        &[
            ("Host", "example.amazonaws.com"),
            ("My-Header1", "  value1"),
            ("My-Header2", "  \"a   b   c\""),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(
        out.canonical
            .text
            .contains("my-header1:value1\nmy-header2:\"a b c\"\nx-amz-date:20150830T123600Z\n")
    );
}

#[test]
fn get_space_normalized_double_encodes_path() {
    // get-space-normalized: 경로의 공백은 %20 으로, 이미 있는 %는 다시 %25 로 인코딩한다(S3 외 서비스 규칙).
    let out = run_suite_case(
        "GET",
        "/example space/",
        "",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(out.canonical.text.starts_with("GET\n/example%20space/\n"));
    let out = run_suite_case(
        "GET",
        "/example%20space/",
        "",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(out.canonical.text.starts_with("GET\n/example%2520space/\n"));
}

#[test]
fn get_relative_and_slashes_are_normalized() {
    // get-relative-normalized, get-slash-normalized, get-slashes-normalized
    let h = [
        ("Host", "example.amazonaws.com"),
        ("X-Amz-Date", "20150830T123600Z"),
    ];
    for (path, expected) in [
        ("/example/..", "/"),
        ("/./", "/"),
        ("//", "/"),
        ("//example//", "/example/"),
        ("/example/./a/../b", "/example/b"),
    ] {
        let out = run_suite_case("GET", path, "", &h, b"");
        assert!(
            out.canonical
                .text
                .starts_with(&format!("GET\n{expected}\n")),
            "{path} -> {}",
            out.canonical.text
        );
    }
}

#[test]
fn get_utf8_path_is_percent_encoded_upper_hex() {
    // get-utf8: "/ሴ" -> "/%E1%88%B4"
    let out = run_suite_case(
        "GET",
        "/ሴ",
        "",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(out.canonical.text.starts_with("GET\n/%E1%88%B4\n"));
}

#[test]
fn get_vanilla_utf8_query_is_encoded() {
    // get-vanilla-utf8-query: "ሴ=bar" -> "%E1%88%B4=bar"
    let out = run_suite_case(
        "GET",
        "/",
        "ሴ=bar",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(out.canonical.text.starts_with("GET\n/\n%E1%88%B4=bar\n"));
}

#[test]
fn get_vanilla_empty_query_key() {
    // get-vanilla-empty-query-key: 값이 있는 키 하나.
    let out = run_suite_case(
        "GET",
        "/",
        "Param1=value1",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(out.canonical.text.starts_with("GET\n/\nParam1=value1\n"));
}

#[test]
fn query_value_less_key_gets_empty_value() {
    // 값 없는 키("Param1")는 "Param1="
    let out = run_suite_case(
        "GET",
        "/",
        "Param1",
        &[
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Date", "20150830T123600Z"),
        ],
        b"",
    );
    assert!(out.canonical.text.starts_with("GET\n/\nParam1=\n"));
}

// ---- S3 문서 예제 (Signature Calculations for the Authorization Header / Query String Auth) ----

const S3_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

fn s3_time() -> Timestamp {
    "2013-05-24T00:00:00Z".parse().unwrap()
}

fn s3_sign(canonical: &CanonicalRequest) -> String {
    let scope = Scope {
        region: "us-east-1",
        service: "s3",
    };
    let sts = string_to_sign(&scope, s3_time(), &canonical.text);
    let key = signing_key(S3_SECRET, "20130524", "us-east-1", "s3");
    sigv4::sign(&key, &sts)
}

#[test]
fn s3_doc_example_presigned_get_object() {
    // https://docs.aws.amazon.com/AmazonS3/latest/API/sigv4-query-string-auth.html
    // GET https://examplebucket.s3.amazonaws.com/test.txt, 만료 86400초, 서명 헤더는 host 하나.
    let query = [
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
        (
            "X-Amz-Credential",
            "AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request",
        ),
        ("X-Amz-Date", "20130524T000000Z"),
        ("X-Amz-Expires", "86400"),
        ("X-Amz-SignedHeaders", "host"),
    ];
    let request = Request {
        method: "GET",
        path: "/test.txt",
        query: &query,
        headers: &[("host", "examplebucket.s3.amazonaws.com")],
        payload_hash: "UNSIGNED-PAYLOAD",
        uri_mode: UriMode::SingleEncodeAsIs,
    };
    let canonical = canonical_request(&request);
    assert_eq!(
        canonical.text,
        "GET\n/test.txt\nX-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\nhost:examplebucket.s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD"
    );
    assert_eq!(
        s3_sign(&canonical),
        "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
    );
}

#[test]
fn s3_doc_example_get_object_with_range_header() {
    // GET Object 예제: Range 헤더 포함 헤더 서명
    let request = Request {
        method: "GET",
        path: "/test.txt",
        query: &[],
        headers: &[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ],
        payload_hash: EMPTY_SHA256,
        uri_mode: UriMode::SingleEncodeAsIs,
    };
    let canonical = canonical_request(&request);
    assert_eq!(
        canonical.signed_headers,
        "host;range;x-amz-content-sha256;x-amz-date"
    );
    assert_eq!(
        s3_sign(&canonical),
        "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
}

#[test]
fn s3_doc_example_put_object_with_dollar_in_key() {
    // PUT Object 예제: 키 "test$file.text" 는 경로에서 %24 로 인코딩되고 S3는 이중 인코딩하지 않는다.
    let body_hash = sha256_hex(b"Welcome to Amazon S3.");
    assert_eq!(
        body_hash,
        "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
    );
    let request = Request {
        method: "PUT",
        path: &sigv4::percent_encode("/test$file.text", true),
        query: &[],
        headers: &[
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("x-amz-content-sha256", &body_hash),
        ],
        payload_hash: &body_hash,
        uri_mode: UriMode::SingleEncodeAsIs,
    };
    let canonical = canonical_request(&request);
    assert!(
        canonical
            .text
            .starts_with("PUT\n/test%24file.text\n\ndate:Fri, 24 May 2013 00:00:00 GMT\n")
    );
    assert_eq!(
        s3_sign(&canonical),
        "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
    );
}

#[test]
fn signing_key_derivation_matches_aws_documented_example() {
    // https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html (iam, 20150830)
    let key = signing_key(
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        "20150830",
        "us-east-1",
        "iam",
    );
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
    );
}
