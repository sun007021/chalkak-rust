//! 자격증명 공급자 시험. HTTP는 가짜 `HttpTransport`로 대체한다.
//! 근거: 원본 `S3ClientConfig`는 `DefaultCredentialsProvider`(환경변수 → ... → EC2 인스턴스 프로파일)를 쓴다.
//! IMDSv2 절차는 AWS 문서(`PUT /latest/api/token` → `GET /latest/meta-data/iam/security-credentials/<role>`)를 따른다.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chalkak_aws::AwsError;
use chalkak_aws::credentials::{
    ChainCredentialsProvider, Credentials, CredentialsProvider, EnvCredentialsProvider,
    HttpRequest, HttpResponse, HttpTransport, ImdsCredentialsProvider,
};
use jiff::{SignedDuration, Timestamp};

// ---- 가짜 ----

#[derive(Default)]
struct FakeTransport {
    responses: Mutex<VecDeque<Result<HttpResponse, AwsError>>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    fn push(&self, status: u16, body: &str) {
        self.responses.lock().unwrap().push_back(Ok(HttpResponse {
            status,
            body: body.as_bytes().to_vec(),
        }));
    }
    fn push_err(&self, e: AwsError) {
        self.responses.lock().unwrap().push_back(Err(e));
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpTransport for FakeTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AwsError> {
        self.requests.lock().unwrap().push(request.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(AwsError::Transport("no scripted response".into())))
    }
}

#[derive(Clone)]
struct TestClock(Arc<Mutex<Timestamp>>);
impl TestClock {
    fn new(s: &str) -> Self {
        Self(Arc::new(Mutex::new(s.parse().unwrap())))
    }
    fn advance(&self, secs: i64) {
        let mut t = self.0.lock().unwrap();
        *t = t.checked_add(SignedDuration::from_secs(secs)).unwrap();
    }
    fn now_fn(&self) -> Arc<dyn Fn() -> Timestamp + Send + Sync> {
        let c = self.0.clone();
        Arc::new(move || *c.lock().unwrap())
    }
}

fn role_json(access: &str, token: &str, expiration: &str) -> String {
    format!(
        r#"{{"Code":"Success","LastUpdated":"2026-10-12T00:00:00Z","Type":"AWS-HMAC","AccessKeyId":"{access}","SecretAccessKey":"secret-{access}","Token":"{token}","Expiration":"{expiration}"}}"#
    )
}

fn env_of(pairs: &[(&str, &str)]) -> EnvCredentialsProvider {
    let map: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    EnvCredentialsProvider::new(move |name| {
        map.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    })
}

// ---- 환경변수 ----

#[test]
fn env_provider_reads_key_secret_and_optional_token() {
    let p = env_of(&[
        ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
        ("AWS_SECRET_ACCESS_KEY", "s3cret"),
        ("AWS_SESSION_TOKEN", "tok"),
    ]);
    assert_eq!(
        p.credentials().unwrap(),
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "s3cret".into(),
            session_token: Some("tok".into())
        }
    );
    let p = env_of(&[
        ("AWS_ACCESS_KEY_ID", "AKID"),
        ("AWS_SECRET_ACCESS_KEY", "s"),
    ]);
    assert_eq!(p.credentials().unwrap().session_token, None);
}

#[test]
fn env_provider_treats_blank_values_as_missing() {
    let p = env_of(&[
        ("AWS_ACCESS_KEY_ID", "AKID"),
        ("AWS_SECRET_ACCESS_KEY", "  "),
    ]);
    assert!(matches!(p.credentials(), Err(AwsError::NoCredentials(_))));
    let p = env_of(&[
        ("AWS_ACCESS_KEY_ID", "AKID"),
        ("AWS_SECRET_ACCESS_KEY", "s"),
        ("AWS_SESSION_TOKEN", ""),
    ]);
    assert_eq!(p.credentials().unwrap().session_token, None);
    assert!(matches!(
        env_of(&[]).credentials(),
        Err(AwsError::NoCredentials(_))
    ));
}

// ---- IMDSv2 ----

#[test]
fn imds_fetches_token_then_role_name_then_credentials() {
    let http = Arc::new(FakeTransport::default());
    http.push(200, "imds-token-1");
    http.push(200, "chalkak-ec2-role\nother-role\n");
    http.push(
        200,
        &role_json("ASIAROLE1", "session-1", "2026-10-12T06:00:00Z"),
    );
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let imds = ImdsCredentialsProvider::new(http.clone(), clock.now_fn());

    let c = imds.credentials().unwrap();
    assert_eq!(c.access_key_id, "ASIAROLE1");
    assert_eq!(c.secret_access_key, "secret-ASIAROLE1");
    assert_eq!(c.session_token.as_deref(), Some("session-1"));

    let reqs = http.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(
        (reqs[0].method.as_str(), reqs[0].url.as_str()),
        ("PUT", "http://169.254.169.254/latest/api/token")
    );
    assert_eq!(
        reqs[0].header("x-aws-ec2-metadata-token-ttl-seconds"),
        Some("21600")
    );
    assert_eq!(
        (reqs[1].method.as_str(), reqs[1].url.as_str()),
        (
            "GET",
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/"
        )
    );
    assert_eq!(
        reqs[1].header("x-aws-ec2-metadata-token"),
        Some("imds-token-1")
    );
    assert_eq!(
        reqs[2].url,
        "http://169.254.169.254/latest/meta-data/iam/security-credentials/chalkak-ec2-role"
    );
    assert_eq!(
        reqs[2].header("x-aws-ec2-metadata-token"),
        Some("imds-token-1")
    );
}

#[test]
fn imds_caches_until_five_minutes_before_expiry_then_refreshes() {
    let http = Arc::new(FakeTransport::default());
    http.push(200, "t1");
    http.push(200, "role");
    http.push(200, &role_json("ASIA1", "s1", "2026-10-12T06:00:00Z"));
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let imds = ImdsCredentialsProvider::new(http.clone(), clock.now_fn());

    assert_eq!(imds.credentials().unwrap().access_key_id, "ASIA1");
    // 만료 5분 전 직전까지는 캐시를 쓴다(호출 3번 그대로).
    clock.advance(6 * 3600 - 301);
    assert_eq!(imds.credentials().unwrap().access_key_id, "ASIA1");
    assert_eq!(http.requests().len(), 3);

    // 만료 5분 전 안으로 들어가면 새로 가져온다.
    http.push(200, "t2");
    http.push(200, "role");
    http.push(200, &role_json("ASIA2", "s2", "2026-10-12T12:00:00Z"));
    clock.advance(2);
    let c = imds.credentials().unwrap();
    assert_eq!(c.access_key_id, "ASIA2");
    assert_eq!(c.session_token.as_deref(), Some("s2"));
    assert_eq!(http.requests().len(), 6);
}

#[test]
fn imds_refresh_failure_keeps_serving_unexpired_cached_credentials() {
    let http = Arc::new(FakeTransport::default());
    http.push(200, "t1");
    http.push(200, "role");
    http.push(200, &role_json("ASIA1", "s1", "2026-10-12T06:00:00Z"));
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let imds = ImdsCredentialsProvider::new(http.clone(), clock.now_fn());
    imds.credentials().unwrap();

    clock.advance(6 * 3600 - 60); // 갱신 구간이지만 아직 만료 전
    http.push_err(AwsError::Transport("timeout".into()));
    assert_eq!(imds.credentials().unwrap().access_key_id, "ASIA1");

    clock.advance(120); // 이제 만료
    http.push_err(AwsError::Transport("timeout".into()));
    assert!(matches!(imds.credentials(), Err(AwsError::Transport(_))));
}

#[test]
fn imds_errors_are_reported() {
    let clock = TestClock::new("2026-10-12T00:00:00Z");

    let http = Arc::new(FakeTransport::default());
    http.push(403, "forbidden");
    let imds = ImdsCredentialsProvider::new(http, clock.now_fn());
    assert!(matches!(
        imds.credentials(),
        Err(AwsError::Imds { status: 403, .. })
    ));

    let http = Arc::new(FakeTransport::default());
    http.push(200, "t");
    http.push(404, "");
    let imds = ImdsCredentialsProvider::new(http, clock.now_fn());
    assert!(matches!(
        imds.credentials(),
        Err(AwsError::Imds { status: 404, .. })
    ));

    let http = Arc::new(FakeTransport::default());
    http.push(200, "t");
    http.push(200, "role");
    http.push(200, "not json");
    let imds = ImdsCredentialsProvider::new(http, clock.now_fn());
    assert!(matches!(imds.credentials(), Err(AwsError::Parse(_))));

    // Code 가 Success 가 아니면 실패
    let http = Arc::new(FakeTransport::default());
    http.push(200, "t");
    http.push(200, "role");
    http.push(
        200,
        r#"{"Code":"AssumeRoleUnauthorizedAccess","Message":"no"}"#,
    );
    let imds = ImdsCredentialsProvider::new(http, clock.now_fn());
    assert!(imds.credentials().is_err());
}

#[test]
fn imds_endpoint_can_be_overridden_for_fakes() {
    let http = Arc::new(FakeTransport::default());
    http.push(200, "t");
    http.push(200, "role");
    http.push(200, &role_json("ASIA1", "s1", "2026-10-12T06:00:00Z"));
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let imds = ImdsCredentialsProvider::new(http.clone(), clock.now_fn())
        .with_endpoint("http://127.0.0.1:9999/");
    imds.credentials().unwrap();
    assert_eq!(
        http.requests()[0].url,
        "http://127.0.0.1:9999/latest/api/token"
    );
}

#[test]
fn imds_endpoint_is_read_from_the_sdk_environment_variable() {
    let http = Arc::new(FakeTransport::default());
    http.push(200, "t");
    http.push(200, "role");
    http.push(200, &role_json("ASIA1", "s1", "2026-10-12T06:00:00Z"));
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let lookup = |name: &str| {
        (name == "AWS_EC2_METADATA_SERVICE_ENDPOINT").then(|| " http://imds-mock:1338/ ".to_owned())
    };
    let imds =
        ImdsCredentialsProvider::new(http.clone(), clock.now_fn()).with_endpoint_from_env(&lookup);
    imds.credentials().unwrap();
    assert_eq!(
        http.requests()[0].url,
        "http://imds-mock:1338/latest/api/token"
    );
}

#[test]
fn imds_endpoint_stays_default_when_the_variable_is_missing_or_blank() {
    for value in [None, Some(""), Some("  ")] {
        let http = Arc::new(FakeTransport::default());
        http.push(200, "t");
        http.push(200, "role");
        http.push(200, &role_json("ASIA1", "s1", "2026-10-12T06:00:00Z"));
        let clock = TestClock::new("2026-10-12T00:00:00Z");
        let lookup = |_: &str| value.map(str::to_owned);
        let imds = ImdsCredentialsProvider::new(http.clone(), clock.now_fn())
            .with_endpoint_from_env(&lookup);
        imds.credentials().unwrap();
        assert_eq!(
            http.requests()[0].url,
            "http://169.254.169.254/latest/api/token"
        );
    }
}

// ---- 체인 ----

#[test]
fn chain_prefers_env_and_never_touches_imds() {
    let http = Arc::new(FakeTransport::default());
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let chain = ChainCredentialsProvider::new(vec![
        Box::new(env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKID"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ])),
        Box::new(ImdsCredentialsProvider::new(http.clone(), clock.now_fn())),
    ]);
    assert_eq!(chain.credentials().unwrap().access_key_id, "AKID");
    assert!(http.requests().is_empty());
}

#[test]
fn chain_falls_back_to_imds_when_env_is_missing() {
    let http = Arc::new(FakeTransport::default());
    http.push(200, "t");
    http.push(200, "role");
    http.push(200, &role_json("ASIA1", "s1", "2026-10-12T06:00:00Z"));
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let chain = ChainCredentialsProvider::new(vec![
        Box::new(env_of(&[])),
        Box::new(ImdsCredentialsProvider::new(http, clock.now_fn())),
    ]);
    assert_eq!(chain.credentials().unwrap().access_key_id, "ASIA1");
}

#[test]
fn chain_reports_imds_failure_and_empty_chain() {
    let http = Arc::new(FakeTransport::default());
    http.push_err(AwsError::Transport("connect timeout".into()));
    let clock = TestClock::new("2026-10-12T00:00:00Z");
    let chain = ChainCredentialsProvider::new(vec![
        Box::new(env_of(&[])),
        Box::new(ImdsCredentialsProvider::new(http, clock.now_fn())),
    ]);
    assert!(matches!(chain.credentials(), Err(AwsError::Transport(_))));
    assert!(matches!(
        ChainCredentialsProvider::new(vec![]).credentials(),
        Err(AwsError::NoCredentials(_))
    ));
}
