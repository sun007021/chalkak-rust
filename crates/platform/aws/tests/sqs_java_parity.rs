//! 원본 백엔드가 쓰는 AWS SDK for Java v2(2.54.1) `SqsClient`와 같은 서명·응답 해석을 하는지 본다.
//!
//! 기대값은 `tests/fixtures/SqsFixture.java`를 `tests/fixtures/run_sqs.sh`로 원본 JAR의 SDK에 돌려 만든
//! `expected_sqs.tsv`다. 요청 줄(`REQUEST_*`)은 SDK가 실제로 보낸 요청이고, `CASE`·`RCASE`·`DCASE` 줄은 같은 응답을 준 결과의
//! 분류다. 응답 정의는 `SqsFixture.java`의 표를 그대로 읽어 쓰므로 한 곳에서만 관리한다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalkak_aws::credentials::Credentials;
use chalkak_aws::sqs::sign_request;
use chalkak_aws::{
    AwsError, CredentialsProvider, HttpRequest, ReceiveMessageInput, SqsClient, SqsEndpoint,
    SqsError, SqsHttpResponse, SqsTransport,
};
use jiff::Timestamp;

const FIXTURE: &str = include_str!("fixtures/expected_sqs.tsv");
const JAVA: &str = include_str!("fixtures/SqsFixture.java");
const QUEUE: &str = "https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-dev-push";
const MSG: &str =
    "{\"eventId\":\"0190f3a2-7b1c-7d2e-8f4a-123456789abc\",\"text\":\"한글 \\\" \\u0001\"}";

fn cred() -> Credentials {
    Credentials {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        session_token: None,
    }
}

struct Static(Credentials);
impl CredentialsProvider for Static {
    fn credentials(&self) -> Result<Credentials, AwsError> {
        Ok(self.0.clone())
    }
}

/// 정해 둔 응답을 돌려주고 마지막 요청을 기억하는 전송.
struct Canned {
    response: Mutex<Result<SqsHttpResponse, AwsError>>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl SqsTransport for Canned {
    fn send(&self, request: &HttpRequest, _timeout: Duration) -> Result<SqsHttpResponse, AwsError> {
        self.seen.lock().unwrap().push(request.clone());
        self.response.lock().unwrap().clone()
    }
}

fn client(transport: Arc<Canned>, now: Timestamp, endpoint: &str) -> SqsClient {
    SqsClient::new(
        "ap-northeast-2",
        SqsEndpoint::custom(endpoint).unwrap(),
        Arc::new(Static(cred())),
        transport,
        Arc::new(move || now),
    )
}

fn canned(status: u16, headers: Vec<(String, String)>, body: &str) -> Arc<Canned> {
    Arc::new(Canned {
        response: Mutex::new(Ok(SqsHttpResponse {
            status,
            headers,
            body: body.as_bytes().to_vec(),
        })),
        seen: Mutex::new(Vec::new()),
    })
}

fn parse_amz_date(s: &str) -> Timestamp {
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13],
        &s[13..15]
    )
    .parse()
    .unwrap()
}

// ---- 요청 서명 ----

struct Captured {
    headers: Vec<(String, String)>,
    body: String,
}

fn captured(kind: &str) -> Captured {
    let line = FIXTURE
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{kind}\t")))
        .unwrap_or_else(|| panic!("{kind} 줄이 없다"));
    let text = line.replace("\\n", "\n");
    let (head, body) = text.split_once("\nBODY ").unwrap();
    let headers = head
        .lines()
        .skip(1)
        .map(|l| {
            let (k, v) = l.split_once(": ").unwrap();
            (k.to_owned(), v.to_owned())
        })
        .collect();
    Captured {
        headers,
        body: body.to_owned(),
    }
}

impl Captured {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("{name} 헤더가 없다"))
    }
}

/// SDK가 보낸 요청과 같은 시각·같은 추가 서명 헤더로 서명하면 `authorization`이 한 글자도 다르지 않아야 한다.
fn assert_signature_matches(kind: &str, operation: &str) {
    let sdk = captured(kind);
    let host = sdk.header("host").to_owned();
    let endpoint = SqsEndpoint::custom(&format!("http://{host}")).unwrap();
    let now = parse_amz_date(sdk.header("x-amz-date"));
    let invocation = sdk.header("amz-sdk-invocation-id").to_owned();
    let attempt = sdk.header("amz-sdk-request").to_owned();
    let request = sign_request(
        &cred(),
        "ap-northeast-2",
        &endpoint,
        operation,
        &sdk.body,
        now,
        &[
            ("amz-sdk-invocation-id", &invocation),
            ("amz-sdk-request", &attempt),
        ],
    );
    assert_eq!(String::from_utf8(request.body.clone()).unwrap(), sdk.body);
    for name in [
        "authorization",
        "content-length",
        "content-type",
        "host",
        "x-amz-content-sha256",
        "x-amz-date",
        "x-amz-target",
        "x-amzn-query-mode",
    ] {
        assert_eq!(
            request.header(name).unwrap(),
            sdk.header(name),
            "{kind} {name}"
        );
    }
}

#[test]
fn send_message_서명은_sdk와_같다() {
    assert_signature_matches("REQUEST_SEND", "SendMessage");
}

#[test]
fn receive_message_서명은_sdk와_같다() {
    assert_signature_matches("REQUEST_RECEIVE", "ReceiveMessage");
}

#[test]
fn delete_message_서명은_sdk와_같다() {
    assert_signature_matches("REQUEST_DELETE", "DeleteMessage");
}

#[test]
fn change_message_visibility_서명은_sdk와_같다() {
    assert_signature_matches("REQUEST_VISIBILITY", "ChangeMessageVisibility");
}

#[test]
fn 요청_본문은_sdk가_보낸_json과_바이트가_같다() {
    let now = parse_amz_date(captured("REQUEST_SEND").header("x-amz-date"));
    let transport = canned(200, vec![], "{}");
    let c = client(transport.clone(), now, "http://127.0.0.1:1");
    let _ = c.send_message(QUEUE, MSG);
    let body = String::from_utf8(transport.seen.lock().unwrap()[0].body.clone()).unwrap();
    assert_eq!(body, captured("REQUEST_SEND").body);

    let transport = canned(200, vec![], "{}");
    let c = client(transport.clone(), now, "http://127.0.0.1:1");
    let _ = c.receive_message(
        QUEUE,
        &ReceiveMessageInput {
            max_number_of_messages: Some(1),
            visibility_timeout: Some(60),
            wait_time_seconds: Some(20),
            message_system_attribute_names: vec!["All".into()],
        },
    );
    let body = String::from_utf8(transport.seen.lock().unwrap()[0].body.clone()).unwrap();
    assert_eq!(body, captured("REQUEST_RECEIVE").body);

    let transport = canned(200, vec![], "{}");
    let c = client(transport.clone(), now, "http://127.0.0.1:1");
    c.delete_message(QUEUE, "rh-1").unwrap();
    c.change_message_visibility(QUEUE, "rh-1", 60).unwrap();
    let seen = transport.seen.lock().unwrap();
    assert_eq!(
        String::from_utf8(seen[0].body.clone()).unwrap(),
        captured("REQUEST_DELETE").body
    );
    assert_eq!(
        String::from_utf8(seen[1].body.clone()).unwrap(),
        captured("REQUEST_VISIBILITY").body
    );
}

// ---- 응답 해석 ----

/// Java 문자열 식(`"..."`, `qe`, `"A".repeat(N)`, `+` 연결)을 값으로.
fn eval(expr: &str) -> String {
    let mut out = String::new();
    for term in split_top(expr, '+') {
        let term = term.trim();
        if term == "qe" {
            out.push_str("x-amzn-query-error");
        } else if term == "h" {
            out.push_str(&chalkak_aws::sqs::md5_hex("hello 한글".as_bytes()));
        } else if let Some(rest) = term.strip_suffix(')')
            && let Some((lit, count)) = rest.split_once(".repeat(")
        {
            out.push_str(&unquote(lit).repeat(count.parse().unwrap()));
        } else {
            out.push_str(&unquote(term));
        }
    }
    out
}

fn unquote(lit: &str) -> String {
    let inner = lit
        .trim()
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap();
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(chars.next().unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

/// 문자열 리터럴과 괄호 밖의 `sep`에서 나눈다.
fn split_top(text: &str, sep: char) -> Vec<String> {
    let (mut parts, mut cur) = (Vec::new(), String::new());
    let (mut in_str, mut escaped, mut depth) = (false, false, 0);
    for c in text.chars() {
        if in_str {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            c if c == sep && depth == 0 => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

struct Case {
    name: String,
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

/// `String[][] <array> = { {...}, ... };` 블록의 행을 읽는다. `columns`는 한 행의 칸 수.
fn java_cases(array: &str, columns: usize) -> Vec<Case> {
    let start = JAVA.find(&format!("String[][] {array} = {{")).unwrap();
    let mut cases = Vec::new();
    for line in JAVA[start..].lines().skip(1) {
        let line = line.trim();
        if line.starts_with("};") {
            break;
        }
        let Some(row) = line
            .strip_prefix('{')
            .and_then(|l| l.strip_suffix("},"))
            .or_else(|| line.strip_prefix('{').and_then(|l| l.strip_suffix('}')))
        else {
            continue;
        };
        let cells = split_top(row, ',');
        assert_eq!(cells.len(), columns, "{line}");
        let header = (columns == 5 && !eval(&cells[3]).is_empty())
            .then(|| (eval(&cells[3]), eval(&cells[4])));
        cases.push(Case {
            name: eval(&cells[0]),
            status: eval(&cells[1]).parse().unwrap(),
            body: eval(&cells[2]),
            headers: header.into_iter().collect(),
        });
    }
    cases
}

fn expected(kind: &str, name: &str) -> Vec<String> {
    FIXTURE
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{kind}\t{name}\t")))
        .unwrap_or_else(|| panic!("{kind} {name} 기대값이 없다"))
        .split('\t')
        .map(str::to_owned)
        .collect()
}

fn md5_of_message() -> String {
    chalkak_aws::sqs::md5_hex(MSG.as_bytes())
}

fn classify_send(case: &Case) -> Vec<String> {
    let transport = canned(
        case.status,
        case.headers.clone(),
        &case.body.replace("@@MD5@@", &md5_of_message()),
    );
    let c = client(
        transport,
        "2026-10-19T11:41:23Z".parse().unwrap(),
        "http://127.0.0.1:1",
    );
    match c.send_message(QUEUE, MSG) {
        Ok(out) => vec![
            "OK".into(),
            out.message_id
                .map_or("<null>".into(), |id| format!("[{id}]")),
        ],
        Err(SqsError::Service(e)) => vec![
            "SQS".into(),
            format!("status={}", e.status),
            format!("code={}", e.code.as_deref().unwrap_or("null")),
            format!("throttling={}", e.is_throttling()),
        ],
        Err(SqsError::Client(_)) => vec!["CLIENT".into()],
    }
}

#[test]
fn 보내기_응답_분류는_sdk와_같다() {
    let cases = java_cases("cases", 5);
    assert!(cases.len() > 60, "표를 다 읽지 못했다: {}", cases.len());
    let mut failures = Vec::new();
    for case in &cases {
        let want = expected("CASE", &case.name);
        let got = classify_send(case);
        // CLIENT 줄은 SDK 예외 이름·메시지가 뒤따르지만 분류만 같으면 된다(메시지는 SDK 내부 문구).
        if want[..got.len().min(want.len())] != got[..]
            && !(want[0] == "CLIENT" && got[0] == "CLIENT")
        {
            failures.push(format!("{}: sdk={want:?} rust={got:?}", case.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn 수신_응답_분류는_sdk와_같다() {
    for case in java_cases("rcases", 3) {
        let want = expected("RCASE", &case.name);
        let transport = canned(case.status, vec![], &case.body);
        let c = client(
            transport,
            "2026-10-19T11:41:23Z".parse().unwrap(),
            "http://127.0.0.1:1",
        );
        let got: Vec<String> = match c.receive_message(QUEUE, &ReceiveMessageInput::default()) {
            Ok(messages) => {
                let mut v = vec!["OK".to_owned(), format!("count={}", messages.len())];
                for m in &messages {
                    v.push(format!("id={}", m.message_id.as_deref().unwrap()));
                    v.push(format!("handle={}", m.receipt_handle.as_deref().unwrap()));
                    v.push(format!("body={}", m.body));
                    v.push(format!(
                        "attrs={{{}}}",
                        m.attributes
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                v
            }
            Err(SqsError::Service(e)) => vec![
                "SQS".into(),
                format!("status={}", e.status),
                format!("code={}", e.code.as_deref().unwrap_or("null")),
            ],
            Err(SqsError::Client(_)) => vec!["CLIENT".into()],
        };
        // CLIENT 줄은 예외 이름이 한 칸 더 있다.
        let want = if want[0] == "CLIENT" {
            vec!["CLIENT".to_owned()]
        } else {
            want
        };
        assert_eq!(got, want, "{}", case.name);
    }
}

#[test]
fn 삭제와_가시성_변경_응답_분류는_sdk와_같다() {
    for case in java_cases("dcases", 3) {
        let want = expected("DCASE", &case.name);
        let transport = canned(case.status, vec![], &case.body);
        let c = client(
            transport,
            "2026-10-19T11:41:23Z".parse().unwrap(),
            "http://127.0.0.1:1",
        );
        let show = |r: Result<(), SqsError>| match r {
            Ok(()) => "OK".to_owned(),
            Err(SqsError::Service(e)) => {
                format!("SQS:{}:{}", e.status, e.code.as_deref().unwrap_or("null"))
            }
            Err(SqsError::Client(_)) => "CLIENT".to_owned(),
        };
        assert_eq!(
            show(c.delete_message(QUEUE, "rh")),
            want[0],
            "{} delete",
            case.name
        );
        assert_eq!(
            show(c.change_message_visibility(QUEUE, "rh", 60)),
            want[1],
            "{} visibility",
            case.name
        );
    }
}

#[test]
fn 전송_실패는_클라이언트_오류다() {
    let transport = Arc::new(Canned {
        response: Mutex::new(Err(AwsError::Transport("timeout".into()))),
        seen: Mutex::new(Vec::new()),
    });
    let c = client(
        transport,
        "2026-10-19T11:41:23Z".parse().unwrap(),
        "http://127.0.0.1:1",
    );
    assert!(matches!(
        c.send_message(QUEUE, MSG),
        Err(SqsError::Client(_))
    ));
}
