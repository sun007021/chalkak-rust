//! FCM 발송이 원본 JAR(Firebase Admin SDK 9.11.0, google-auth 1.52.0)과 같은지 기록 전체로 확인한다.
//!
//! `fixtures/expected_fcm.tsv`는 `fixtures/FcmFixture.java`를 원본 `app.jar`에 돌려 얻은 기록이다(`run_fcm.sh`). 한 행이 입력(상태·헤더·
//! 본문)과 결과(상태·오류 코드·재시도 간격·FCM 호출 수·토큰 교환 수)를 담는다. 여기서는 같은 입력을 가짜 전송에 넣어 Rust 구현의
//! 결과가 행과 같은지 본다. 대기 시간(`seconds`)과 `x-goog-api-client`(자바 버전 문자열)는 비교하지 않는다.
//!
//! 제외한 행:
//! - `@DATE+N` 헤더: 실제 시각에 의존한다. 날짜 해석은 `retry_after`의 단위 시험이 같은 규칙을 확인한다.
//! - `retry_after_arabic_digits`: JDK 시험 서버가 비ASCII 헤더를 깨뜨려 기록이 SDK 동작이 아니다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalkak_app::notification::domain::{NotificationSourceType, NotificationType};
use chalkak_app::notification::fcm::FcmDevicePushSender;
use chalkak_app::notification::fcm::auth::{ServiceAccount, TokenProvider};
use chalkak_app::notification::fcm::client::FirebaseMessagingClient;
use chalkak_app::notification::fcm::transport::{
    ExchangeError, ExchangeRequest, ExchangeResponse, HttpExchange,
};
use chalkak_app::notification::push_sender::{DevicePushRequest, DevicePushSender};
use chalkak_app::notification::sqs_push::PushMessage;
use chalkak_core::{Clock, FixedClock};
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

const EXPECTED: &str = include_str!("fixtures/expected_fcm.tsv");
const KEY: &str = include_str!("fixtures/fcm-test-key.pem");
const ACCEPTED: &str = r#"{"name":"projects/test/messages/accepted"}"#;
const TOKEN_OK: &str =
    r#"{"access_token":"fixture-access-token","expires_in":3600,"token_type":"Bearer"}"#;

fn now() -> Timestamp {
    "2026-10-07T00:00:00Z".parse().unwrap()
}

fn plus_secs(seconds: i64) -> Timestamp {
    now()
        .checked_add(SignedDuration::from_secs(seconds))
        .unwrap()
}

fn plus_nanos(nanos: i64) -> Timestamp {
    now()
        .checked_add(SignedDuration::from_nanos(nanos))
        .unwrap()
}

/// TSV 한 칸의 이스케이프(`\\`, `\n`, `\t`)를 푼다.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 시험 서버가 정해 둔 응답. 자바 `FcmFixture`의 서버 상태와 같다.
#[derive(Clone)]
struct Server {
    fcm_status: u16,
    fcm_body: String,
    fcm_headers: Vec<(String, String)>,
    accept_after: usize,
    token_status: u16,
    token_body: String,
    /// FCM 호출을 이 오류로 끝낸다(전송 오류 행).
    fcm_error: Option<ExchangeError>,
    /// 토큰 호출을 이 오류로 끝낸다.
    token_error: Option<ExchangeError>,
}

impl Server {
    fn new() -> Self {
        Self {
            fcm_status: 200,
            fcm_body: ACCEPTED.to_owned(),
            fcm_headers: Vec::new(),
            accept_after: usize::MAX,
            token_status: 200,
            token_body: TOKEN_OK.to_owned(),
            fcm_error: None,
            token_error: None,
        }
    }
}

struct Transport {
    server: Server,
    fcm: Mutex<Vec<ExchangeRequest>>,
    tokens: Mutex<Vec<ExchangeRequest>>,
}

impl Transport {
    fn new(server: Server) -> Arc<Self> {
        Arc::new(Self {
            server,
            fcm: Mutex::new(Vec::new()),
            tokens: Mutex::new(Vec::new()),
        })
    }
}

impl HttpExchange for Transport {
    fn execute(&self, request: &ExchangeRequest) -> Result<ExchangeResponse, ExchangeError> {
        if request.url.ends_with("/token") {
            self.tokens.lock().unwrap().push(request.clone());
            if let Some(error) = &self.server.token_error {
                return Err(error.clone());
            }
            return Ok(ExchangeResponse {
                status: self.server.token_status,
                headers: Vec::new(),
                body: self.server.token_body.clone().into_bytes(),
            });
        }
        let attempt = {
            let mut seen = self.fcm.lock().unwrap();
            seen.push(request.clone());
            seen.len()
        };
        if let Some(error) = &self.server.fcm_error {
            return Err(error.clone());
        }
        if attempt >= self.server.accept_after {
            return Ok(ExchangeResponse {
                status: 200,
                headers: Vec::new(),
                body: ACCEPTED.as_bytes().to_vec(),
            });
        }
        Ok(ExchangeResponse {
            status: self.server.fcm_status,
            headers: self.server.fcm_headers.clone(),
            body: self.server.fcm_body.clone().into_bytes(),
        })
    }
}

fn sender(transport: &Arc<Transport>) -> FcmDevicePushSender {
    let json = format!(
        r#"{{"type":"service_account","project_id":"test","private_key_id":"fixture-key-id","private_key":{},"client_email":"worker@test.iam.gserviceaccount.com","client_id":"1234567890","token_uri":"http://127.0.0.1:9/token"}}"#,
        serde_json::to_string(KEY).unwrap()
    );
    let account = ServiceAccount::from_json(&json).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(now()));
    let sleep: Arc<dyn Fn(Duration) + Send + Sync> = Arc::new(|_| {});
    let tokens = TokenProvider::new(account, transport.clone(), clock.clone(), sleep.clone());
    let client = FirebaseMessagingClient::new(
        "http://127.0.0.1:9",
        "test",
        tokens,
        transport.clone(),
        clock.clone(),
        sleep,
    );
    FcmDevicePushSender::new(Arc::new(client), clock)
}

fn message(kind: NotificationType, source_type: NotificationSourceType) -> PushMessage {
    PushMessage {
        event_id: "0190f3a2-7b1c-7d2e-8f4a-123456789abc".parse().unwrap(),
        notification_id: Some("0190f3a2-7b1c-7d2e-8f4a-123456789abd".parse().unwrap()),
        user_id: "0190f3a2-7b1c-7d2e-8f4a-123456789abe".parse().unwrap(),
        kind,
        source_type: Some(source_type),
        source_id: Some("0190f3a2-7b1c-7d2e-8f4a-123456789abf".parse().unwrap()),
        occurred_at: now(),
        expires_at: plus_secs(1800),
    }
}

fn request(
    message: PushMessage,
    token: &str,
    title: &str,
    body: &str,
    expires_at: Timestamp,
) -> DevicePushRequest {
    DevicePushRequest {
        message,
        token: token.to_owned(),
        title: title.to_owned(),
        body: body.to_owned(),
        expires_at,
    }
}

fn case_request() -> DevicePushRequest {
    let mut message = message(NotificationType::PostApproved, NotificationSourceType::Post);
    message.event_id = Uuid::new_v4();
    request(message, "fcm-token", "제목", "본문", plus_secs(1800))
}

fn outcome(sender: &FcmDevicePushSender, request: &DevicePushRequest) -> String {
    match sender.send(request) {
        Ok(result) => format!(
            "status={} errorCode={} retryAfter={}",
            result.status.as_str(),
            result.error_code.as_deref().unwrap_or("null"),
            result.retry_after.as_secs()
        ),
        Err(failure) => format!("THROWN {}", failure.exception),
    }
}

/// 기록의 입력 칸(`IN` 이후)으로 서버 상태를 만든다. 토큰 행이면 토큰 서버에, 아니면 FCM에 건다.
struct Row<'a> {
    kind: &'a str,
    label: &'a str,
    outcome: &'a str,
    fcm_calls: usize,
    token_calls: usize,
    input: Vec<&'a str>,
}

fn parse_rows() -> Vec<Row<'static>> {
    EXPECTED
        .lines()
        .filter(|line| {
            ["CASE", "RETRY", "AUTH", "TOKEN"]
                .iter()
                .any(|kind| line.starts_with(&format!("{kind}\t")))
        })
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            let calls = |col: &str, prefix: &str| -> usize {
                col.strip_prefix(prefix).unwrap().parse().unwrap()
            };
            let input_at = cols.iter().position(|c| *c == "IN").unwrap();
            Row {
                kind: cols[0],
                label: cols[1],
                outcome: cols[2],
                fcm_calls: calls(cols[3], "fcmCalls="),
                token_calls: calls(cols[4], "tokenCalls="),
                input: cols[input_at + 1..].to_vec(),
            }
        })
        .collect()
}

fn server_for(row: &Row) -> Option<Server> {
    let mut server = Server::new();
    let input = &row.input;
    match row.label {
        "read_timeout" => {
            server.fcm_error = Some(ExchangeError::Timeout);
            return Some(server);
        }
        "connection_refused" => {
            server.fcm_error = Some(ExchangeError::Other);
            return Some(server);
        }
        "unknown_host" => {
            server.fcm_error = Some(ExchangeError::UnknownHost);
            return Some(server);
        }
        "token_connection_refused" => {
            server.token_error = Some(ExchangeError::Other);
            return Some(server);
        }
        _ => {}
    }
    let status: u16 = input[0].parse().unwrap();
    let header_name = input[1];
    let header_value = input[2];
    let accept_after = input[3];
    let body = unescape(input[4]);
    if row.kind == "TOKEN" {
        server.token_status = status;
        server.token_body = body;
        return Some(server);
    }
    if header_value.starts_with("@DATE") || row.label == "retry_after_arabic_digits" {
        return None;
    }
    server.fcm_status = status;
    server.fcm_body = body;
    if !accept_after.is_empty() {
        server.accept_after = accept_after.parse().unwrap();
    }
    if !header_name.is_empty() {
        // 한 헤더에 값이 여럿(`|`)이면 줄을 나눠 보낸다.
        for value in header_value.split('|') {
            server
                .fcm_headers
                .push((header_name.to_owned(), value.to_owned()));
        }
    }
    Some(server)
}

#[test]
fn 응답_분류와_재시도_횟수가_원본_기록과_같다() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for row in parse_rows() {
        let Some(server) = server_for(&row) else {
            continue;
        };
        let transport = Transport::new(server);
        let sender = sender(&transport);
        let actual = outcome(&sender, &case_request());
        let fcm_calls = transport.fcm.lock().unwrap().len();
        let token_calls = transport.tokens.lock().unwrap().len();
        // 전송 오류 행은 서버에 닿았는지(`fcmCalls`)가 환경 문제라 FCM 호출 수를 비교하지 않는다.
        let is_io = matches!(
            row.label,
            "read_timeout" | "connection_refused" | "unknown_host"
        );
        // 토큰 서버에 닿지 못한 행은 자바 서버가 호출을 세지 못한다(여기서는 재시도 네 번을 센다).
        let calls_match = (is_io || fcm_calls == row.fcm_calls)
            && (row.label == "token_connection_refused" || token_calls == row.token_calls);
        if actual != row.outcome || !calls_match {
            failures.push(format!(
                "{}/{}: 기대 {} fcm={} token={}, 실제 {} fcm={} token={}",
                row.kind,
                row.label,
                row.outcome,
                row.fcm_calls,
                row.token_calls,
                actual,
                fcm_calls,
                token_calls
            ));
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(checked > 180, "비교한 행이 너무 적다: {checked}");
}

#[test]
fn 토큰은_만료_직전에_다시_받는다() {
    let mut checked = 0;
    for line in EXPECTED.lines().filter(|l| l.starts_with("CACHE\t")) {
        let cols: Vec<&str> = line.split('\t').collect();
        let expires_in: u64 = cols[1]
            .strip_prefix("expires_in=")
            .unwrap()
            .parse()
            .unwrap();
        let token_calls: usize = cols[2]
            .strip_prefix("tokenCalls=")
            .unwrap()
            .parse()
            .unwrap();
        let fcm_calls: usize = cols[3].strip_prefix("fcmCalls=").unwrap().parse().unwrap();
        let mut server = Server::new();
        server.token_body =
            format!(r#"{{"access_token":"fixture-access-token","expires_in":{expires_in}}}"#);
        let transport = Transport::new(server);
        let sender = sender(&transport);
        let request = case_request();
        outcome(&sender, &request);
        outcome(&sender, &request);
        assert_eq!(
            transport.tokens.lock().unwrap().len(),
            token_calls,
            "expires_in={expires_in}"
        );
        assert_eq!(transport.fcm.lock().unwrap().len(), fcm_calls);
        checked += 1;
    }
    assert_eq!(checked, 9);
}

fn base64url_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("base64url이 아니다"),
        } as u32;
        buffer = buffer << 6 | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

fn recorded_request(row: &str) -> (String, String) {
    let cols: Vec<&str> = row.split('\t').collect();
    // 요청 기록은 줄 구분만 글자 그대로의 `\n`(역슬래시 n)이고 본문은 이스케이프하지 않은 원문이다.
    let text = *cols.last().unwrap();
    let (head, body) = text.split_once("\\nBODY ").unwrap();
    (head.to_owned(), body.to_owned())
}

#[test]
fn 요청_바이트와_헤더가_원본_기록과_같다() {
    let big = message(NotificationType::PostApproved, NotificationSourceType::Post);
    let expires = plus_secs(1800);
    let mut sweep = String::from("s");
    sweep.extend((0u32..0x80).filter_map(char::from_u32));
    for c in [
        0x80, 0x9F, 0xA0, 0xAD, 0x2028, 0x2029, 0xD7FF, 0xE000, 0xFFFD, 0xFFFE, 0xFFFF, 0x10000,
        0x10FFFF,
    ] {
        sweep.push(char::from_u32(c).unwrap());
    }
    let topic = {
        let mut m = message(NotificationType::Topic, NotificationSourceType::Topic);
        m.notification_id = None;
        m
    };
    let requests = [
        (
            "approved",
            request(
                big.clone(),
                "fcm-token-1",
                "게시물이 승인되었습니다.",
                "내 사진이 피드에 공개되었습니다.",
                expires,
            ),
        ),
        (
            "rejected",
            request(
                message(NotificationType::PostRejected, NotificationSourceType::Post),
                "fcm-token-2",
                "게시물이 반려되었습니다.",
                "반려 사유를 확인해 주세요.",
                expires,
            ),
        ),
        (
            "topic",
            request(topic, "fcm-token-3", "오늘의 주제", "빛과 그림자", expires),
        ),
        (
            "notice",
            request(
                message(NotificationType::Notice, NotificationSourceType::Notice),
                "fcm-token-4",
                "서비스 점검 안내",
                "10월 20일 02:00부터 점검합니다.",
                expires,
            ),
        ),
        (
            "fraction",
            request(
                big.clone(),
                "fcm-token-5",
                "t",
                "b",
                plus_nanos(1_800_123_000_000),
            ),
        ),
        (
            "fraction_micro",
            request(
                big.clone(),
                "fcm-token-5",
                "t",
                "b",
                plus_nanos(1_500_999_999),
            ),
        ),
        (
            "subsecond",
            request(
                big.clone(),
                "fcm-token-5",
                "t",
                "b",
                plus_nanos(500_000_000),
            ),
        ),
        (
            "escapes",
            request(
                big.clone(),
                "tok\"en\\x",
                "제목 \"따옴표\" \\ / <b>&='",
                "줄\n바꿈\t탭\u{1}\u{1f}\u{7f}\u{2028}\u{2029} \u{1F600} é",
                expires,
            ),
        ),
        ("sweep", request(big, "tok", &sweep, "b", expires)),
    ];

    let transport = Transport::new(Server::new());
    let sender = sender(&transport);
    let rows: Vec<&str> = EXPECTED
        .lines()
        .filter(|l| l.starts_with("REQUEST\t"))
        .collect();
    assert_eq!(rows.len(), requests.len());
    for ((name, request), row) in requests.iter().zip(&rows) {
        assert!(row.starts_with(&format!("REQUEST\t{name}\t")), "{name}");
        let result = sender.send(request).unwrap();
        assert_eq!(result.status.as_str(), "ACCEPTED", "{name}");
        let seen = transport.fcm.lock().unwrap().last().cloned().unwrap();
        let (head, expected_body) = recorded_request(row);
        assert_eq!(
            String::from_utf8(seen.body.clone()).unwrap(),
            expected_body,
            "{name}"
        );
        let header = |wanted: &str| -> Option<&str> {
            seen.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(wanted))
                .map(|(_, v)| v.as_str())
        };
        for line in head.split("\\n").skip(1) {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };
            match key {
                "authorization"
                | "content-type"
                | "x-firebase-client"
                | "x-goog-api-format-version" => {
                    assert_eq!(header(key), Some(value), "{name} {key}");
                }
                "content-length" => assert_eq!(seen.body.len().to_string(), value, "{name}"),
                _ => {}
            }
        }
        assert!(head.starts_with("POST /v1/projects/test/messages:send"));
        assert!(seen.url.ends_with("/v1/projects/test/messages:send"));
    }
    // 토큰 교환은 첫 발송에서만 일어나고 이후에는 캐시를 쓴다.
    let expected_calls: usize = EXPECTED
        .lines()
        .find(|l| l.starts_with("TOKEN_CALLS_AFTER_"))
        .unwrap()
        .split('\t')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(transport.tokens.lock().unwrap().len(), expected_calls);
}

#[test]
fn 토큰_요청은_원본_기록과_같다() {
    let transport = Transport::new(Server::new());
    let sender = sender(&transport);
    sender.send(&case_request()).unwrap();
    let seen = transport.tokens.lock().unwrap()[0].clone();
    let row = EXPECTED
        .lines()
        .find(|l| l.starts_with("TOKEN_REQUEST\t"))
        .unwrap();
    let (head, expected) = recorded_request(row);
    assert!(head.starts_with("POST /token"));
    assert_eq!(
        seen.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str()),
        Some("application/x-www-form-urlencoded; charset=UTF-8")
    );
    let body = String::from_utf8(seen.body).unwrap();
    let (prefix, assertion) = body.split_once("&assertion=").unwrap();
    assert_eq!(
        prefix,
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
    );
    let parts: Vec<&str> = assertion.split('.').collect();
    let header = String::from_utf8(base64url_decode(parts[0])).unwrap();
    let payload = String::from_utf8(base64url_decode(parts[1])).unwrap();
    let signature = base64url_decode(parts[2]);
    // 기록은 iat·exp를 N으로 바꾸고 차이를 따로 적는다.
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let iat = value["iat"].as_i64().unwrap();
    let exp = value["exp"].as_i64().unwrap();
    let normalized = payload
        .replace(&format!("\"iat\":{iat}"), "\"iat\":N")
        .replace(&format!("\"exp\":{exp}"), "\"exp\":N");
    let rebuilt = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion HEADER={header} PAYLOAD={normalized} exp-iat={} SIGNATURE_BYTES={}",
        exp - iat,
        signature.len()
    );
    assert_eq!(rebuilt, expected);
}
