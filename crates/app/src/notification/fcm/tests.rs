//! `FcmDevicePushSender`(원본 `FcmDevicePushSenderTest`)와 SDK 호출(`client`) 시험. 전송은 정해 둔 응답을 돌려주는 가짜다.
//! 원본 JAR의 SDK와 바이트·분류가 같은지는 `tests/fcm_java_parity.rs`가 기록 전체로 확인한다.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalkak_core::{Clock, FixedClock};
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use uuid::Uuid;

use super::FcmDevicePushSender;
use super::auth::{ServiceAccount, TokenProvider};
use super::client::FirebaseMessagingClient;
use super::transport::{ExchangeError, ExchangeRequest, ExchangeResponse, HttpExchange};
use crate::notification::domain::{NotificationSourceType, NotificationType};
use crate::notification::push_sender::{
    DevicePushRequest, DevicePushResult, DevicePushSender, DeviceStatus,
};
use crate::notification::sqs_push::PushMessage;

const KEY: &str = include_str!("../../../tests/fixtures/fcm-test-key.pem");

fn now() -> Timestamp {
    "2026-10-07T00:00:00Z".parse().unwrap()
}

fn plus(seconds: i64) -> Timestamp {
    now()
        .checked_add(SignedDuration::from_secs(seconds))
        .unwrap()
}

type Handler =
    dyn Fn(&ExchangeRequest, usize) -> Result<ExchangeResponse, ExchangeError> + Send + Sync;

/// 요청을 기록하고 `handler`가 정한 응답을 돌려주는 전송. FCM과 토큰 서버를 주소로 구별한다.
struct Scripted {
    fcm: Mutex<Vec<ExchangeRequest>>,
    tokens: Mutex<Vec<ExchangeRequest>>,
    fcm_handler: Box<Handler>,
    token_handler: Box<Handler>,
}

impl HttpExchange for Scripted {
    fn execute(&self, request: &ExchangeRequest) -> Result<ExchangeResponse, ExchangeError> {
        let (list, handler) = if request.url.ends_with("/token") {
            (&self.tokens, &self.token_handler)
        } else {
            (&self.fcm, &self.fcm_handler)
        };
        let mut seen = list.lock().unwrap();
        seen.push(request.clone());
        handler(request, seen.len())
    }
}

fn response(status: u16, headers: &[(&str, &str)], body: &str) -> ExchangeResponse {
    ExchangeResponse {
        status,
        headers: headers
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

const ACCEPTED: &str = r#"{"name":"projects/test/messages/accepted"}"#;
const TOKEN_OK: &str = r#"{"access_token":"test-token","expires_in":3600}"#;

fn error_body(code: u16, status: &str, detail: &str) -> String {
    format!(
        r#"{{"error":{{"code":{code},"status":"{status}","message":"failure","details":[{{"@type":"type.googleapis.com/google.firebase.fcm.v1.FcmError","errorCode":"{detail}"}}]}}}}"#
    )
}

struct Fixture {
    clock: Arc<FixedClock>,
    transport: Arc<Scripted>,
    sleeps: Arc<Mutex<Vec<Duration>>>,
    sender: FcmDevicePushSender,
}

fn fixture(
    fcm: impl Fn(&ExchangeRequest, usize) -> Result<ExchangeResponse, ExchangeError>
    + Send
    + Sync
    + 'static,
) -> Fixture {
    fixture_with(fcm, |_, _| Ok(response(200, &[], TOKEN_OK)))
}

fn fixture_with(
    fcm: impl Fn(&ExchangeRequest, usize) -> Result<ExchangeResponse, ExchangeError>
    + Send
    + Sync
    + 'static,
    token: impl Fn(&ExchangeRequest, usize) -> Result<ExchangeResponse, ExchangeError>
    + Send
    + Sync
    + 'static,
) -> Fixture {
    let clock = Arc::new(FixedClock::new(now()));
    let transport = Arc::new(Scripted {
        fcm: Mutex::new(Vec::new()),
        tokens: Mutex::new(Vec::new()),
        fcm_handler: Box::new(fcm),
        token_handler: Box::new(token),
    });
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let sleep: Arc<dyn Fn(Duration) + Send + Sync> = {
        let sleeps = Arc::clone(&sleeps);
        Arc::new(move |d| sleeps.lock().unwrap().push(d))
    };
    let json = format!(
        r#"{{"type":"service_account","project_id":"test","private_key_id":"key-id","private_key":{},"client_email":"worker@test.iam.gserviceaccount.com","client_id":"1","token_uri":"http://127.0.0.1/token"}}"#,
        serde_json::to_string(KEY).unwrap()
    );
    let account = ServiceAccount::from_json(&json).unwrap();
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let tokens = TokenProvider::new(account, transport.clone(), dyn_clock.clone(), sleep.clone());
    let client = FirebaseMessagingClient::new(
        "http://127.0.0.1:1",
        "test",
        tokens,
        transport.clone(),
        dyn_clock.clone(),
        sleep,
    );
    Fixture {
        clock,
        transport,
        sleeps,
        sender: FcmDevicePushSender::new(Arc::new(client), dyn_clock),
    }
}

fn request() -> DevicePushRequest {
    let message = PushMessage {
        event_id: Uuid::new_v4(),
        notification_id: Some(Uuid::new_v4()),
        user_id: Uuid::new_v4(),
        kind: NotificationType::PostApproved,
        source_type: Some(NotificationSourceType::Post),
        source_id: Some(Uuid::new_v4()),
        occurred_at: now(),
        expires_at: plus(1800),
    };
    DevicePushRequest {
        expires_at: message.expires_at,
        message,
        token: "fcm-test-token".to_owned(),
        title: "승인되었습니다".to_owned(),
        body: "사진을 확인해주세요".to_owned(),
    }
}

fn sent_body(fixture: &Fixture, index: usize) -> Value {
    let seen = fixture.transport.fcm.lock().unwrap();
    serde_json::from_slice(&seen[index].body).unwrap()
}

fn fcm_calls(fixture: &Fixture) -> usize {
    fixture.transport.fcm.lock().unwrap().len()
}

fn ok(_: &ExchangeRequest, _: usize) -> Result<ExchangeResponse, ExchangeError> {
    Ok(response(200, &[], ACCEPTED))
}

// FcmDevicePushSenderTest#send_accepted_sendsPlatformExpirationAndNavigation
#[test]
fn 수락된_요청은_화면_이동_정보와_android_ttl_apns_절대_만료를_담는다() {
    let fixture = fixture(ok);
    let request = request();

    let result = fixture.sender.send(&request).unwrap();

    assert_eq!(result.status, DeviceStatus::Accepted);
    let body = sent_body(&fixture, 0);
    let message = &body["message"];
    assert_eq!(message["android"]["ttl"], "1800s");
    assert_eq!(
        message["apns"]["headers"]["apns-expiration"],
        request.expires_at.as_second().to_string()
    );
    assert_eq!(
        message["data"]["notificationId"],
        request.message.notification_id.unwrap().to_string()
    );
    assert!(message["data"].get("userId").is_none());
    assert!(message["data"].get("rejectionReason").is_none());
    assert_eq!(message["token"], "fcm-test-token");
}

// FcmDevicePushSenderTest#send_topicMessage_omitsInboxIdAndPreservesExpiration
#[test]
fn 주제_요청은_알림함_id_없이_주제_종류와_대상을_보낸다() {
    let fixture = fixture(ok);
    let topic_id = Uuid::new_v4();
    let message = PushMessage {
        event_id: Uuid::new_v4(),
        notification_id: None,
        user_id: Uuid::new_v4(),
        kind: NotificationType::Topic,
        source_type: Some(NotificationSourceType::Topic),
        source_id: Some(topic_id),
        occurred_at: now(),
        expires_at: plus(1800),
    };
    let request = DevicePushRequest {
        expires_at: message.expires_at,
        message: message.clone(),
        token: "fcm-test-token".to_owned(),
        title: "오늘의 주제".to_owned(),
        body: "빛과 그림자".to_owned(),
    };

    assert_eq!(
        fixture.sender.send(&request).unwrap().status,
        DeviceStatus::Accepted
    );

    let body = sent_body(&fixture, 0);
    let sent = &body["message"];
    assert_eq!(sent["notification"]["title"], "오늘의 주제");
    assert_eq!(sent["notification"]["body"], "빛과 그림자");
    assert_eq!(sent["data"]["type"], "TOPIC");
    assert_eq!(sent["data"]["sourceType"], "TOPIC");
    assert_eq!(sent["data"]["sourceId"], topic_id.to_string());
    assert_eq!(sent["data"]["eventId"], message.event_id.to_string());
    assert!(sent["data"].get("notificationId").is_none());
    assert!(sent["data"].get("userId").is_none());
    assert_eq!(sent["android"]["ttl"], "1800s");
    assert_eq!(
        sent["apns"]["headers"]["apns-expiration"],
        message.expires_at.as_second().to_string()
    );
}

// FcmDevicePushSenderTest#send_fcmError_classifiesResult
#[test]
fn fcm_응답을_무효_토큰_영구_오류_일시_실패로_분류한다() {
    for (code, status, detail, expected) in [
        (404, "NOT_FOUND", "UNREGISTERED", DeviceStatus::InvalidToken),
        (
            400,
            "INVALID_ARGUMENT",
            "INVALID_ARGUMENT",
            DeviceStatus::PermanentFailure,
        ),
        (
            403,
            "PERMISSION_DENIED",
            "SENDER_ID_MISMATCH",
            DeviceStatus::PermanentFailure,
        ),
        (
            401,
            "UNAUTHENTICATED",
            "THIRD_PARTY_AUTH_ERROR",
            DeviceStatus::PermanentFailure,
        ),
        (
            429,
            "RESOURCE_EXHAUSTED",
            "QUOTA_EXCEEDED",
            DeviceStatus::Retryable,
        ),
        (500, "INTERNAL", "INTERNAL", DeviceStatus::Retryable),
    ] {
        let body = error_body(code, status, detail);
        let fixture = fixture(move |_, _| Ok(response(code, &[], &body)));

        let result = fixture.sender.send(&request()).unwrap();

        assert_eq!(result.status, expected, "{code} {status} {detail}");
        assert!(fcm_calls(&fixture) >= 1);
    }
}

// FcmDevicePushSenderTest#send_unavailableThenAccepted_preservesInitialExpiration
#[test]
fn 재시도에도_최초_계산한_android_ttl과_apns_만료를_유지한다() {
    let clock_slot: Arc<Mutex<Option<Arc<FixedClock>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&clock_slot);
    let body = error_body(503, "UNAVAILABLE", "UNAVAILABLE");
    let fixture = fixture(move |_, attempt| {
        if attempt == 1 {
            // 첫 응답 뒤 시계가 10초 간다.
            if let Some(clock) = slot.lock().unwrap().as_ref() {
                clock.set(plus(10));
            }
            return Ok(response(503, &[], &body));
        }
        Ok(response(200, &[], ACCEPTED))
    });
    *clock_slot.lock().unwrap() = Some(Arc::clone(&fixture.clock));
    let request = request();

    assert_eq!(
        fixture.sender.send(&request).unwrap().status,
        DeviceStatus::Accepted
    );

    assert_eq!(fcm_calls(&fixture), 2);
    let last = sent_body(&fixture, 1);
    assert_eq!(last["message"]["android"]["ttl"], "1800s");
    assert_eq!(
        last["message"]["apns"]["headers"]["apns-expiration"],
        request.expires_at.as_second().to_string()
    );
    // 503은 0.5초 기다린 뒤 다시 보낸다.
    assert_eq!(
        *fixture.sleeps.lock().unwrap(),
        vec![Duration::from_millis(500)]
    );
}

fn quota_exceeded(headers: &'static [(&'static str, &'static str)]) -> Fixture {
    let body = error_body(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED");
    fixture(move |_, _| Ok(response(429, headers, &body)))
}

// FcmDevicePushSenderTest#send_longRetryAfter_preservesDelay
#[test]
fn 긴_retry_after는_재시도_지연으로_전달한다() {
    let fixture = quota_exceeded(&[("Retry-After", "120")]);

    assert_eq!(
        fixture.sender.send(&request()).unwrap().retry_after,
        Duration::from_secs(120)
    );
}

// FcmDevicePushSenderTest#send_retryAfterHeader_parsesDateOrUsesMinimum
#[test]
fn http_날짜_retry_after는_해석하고_잘못된_값은_최소_간격을_유지한다() {
    for (header, seconds) in [
        ("Wed, 07 Oct 2026 00:02:00 GMT", 120),
        ("invalid-header", 60),
        ("-1", 60),
        ("59", 60),
        ("60", 60),
        ("61", 61),
    ] {
        let header: &'static str = header;
        let headers: &'static [(&'static str, &'static str)] =
            Box::leak(Box::new([("Retry-After", header)]));
        let fixture = quota_exceeded(headers);

        assert_eq!(
            fixture.sender.send(&request()).unwrap().retry_after,
            Duration::from_secs(seconds),
            "{header}"
        );
    }
}

// FcmDevicePushSenderTest#send_deadlineBoundary_acceptsBeforeAndSkipsAtDeadline
#[test]
fn fcm_요청_직전_기한을_확인하여_정각부터는_생략한다() {
    for seconds in [1799, 1800, 1801] {
        let fixture = fixture(ok);
        fixture.clock.set(plus(seconds));

        let result = fixture.sender.send(&request()).unwrap();

        if seconds < 1800 {
            assert_eq!(result.status, DeviceStatus::Accepted);
            assert_eq!(fcm_calls(&fixture), 1);
        } else {
            assert_eq!(result, DevicePushResult::skipped("EXPIRED"));
            assert_eq!(fcm_calls(&fixture), 0);
            // 건너뛰면 토큰도 받지 않는다.
            assert!(fixture.transport.tokens.lock().unwrap().is_empty());
        }
    }
}

// FcmDevicePushSenderTest#send_deadlineDuringSdkRetry_finishesStartedSend
#[test]
fn 기한_직전에_시작한_재시도는_기한이_지나도_수락_결과로_완료한다() {
    let clock_slot: Arc<Mutex<Option<Arc<FixedClock>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&clock_slot);
    let body = error_body(503, "UNAVAILABLE", "UNAVAILABLE");
    let fixture = fixture(move |_, attempt| {
        if attempt == 1 {
            if let Some(clock) = slot.lock().unwrap().as_ref() {
                clock.set(plus(1800));
            }
            return Ok(response(503, &[], &body));
        }
        Ok(response(200, &[], ACCEPTED))
    });
    *clock_slot.lock().unwrap() = Some(Arc::clone(&fixture.clock));
    fixture.clock.set(plus(1799));

    assert_eq!(
        fixture.sender.send(&request()).unwrap().status,
        DeviceStatus::Accepted
    );

    assert_eq!(fcm_calls(&fixture), 2);
    assert_eq!(sent_body(&fixture, 1)["message"]["android"]["ttl"], "1s");
}

// ---- SDK 호출 ----

// ServiceAccountCredentials: 토큰은 한 번만 받고 캐시한다. 만료 3분 45초 전부터는 새로 받는다.
#[test]
fn 토큰은_캐시하고_만료_직전에_다시_받는다() {
    let fixture = fixture(ok);

    fixture.sender.send(&request()).unwrap();
    fixture.sender.send(&request()).unwrap();
    assert_eq!(fixture.transport.tokens.lock().unwrap().len(), 1);

    // 3600초짜리 토큰: 남은 시간이 225초 이하가 되면 새로 받는다.
    fixture.clock.set(plus(3600 - 226));
    fixture
        .sender
        .send(&DevicePushRequest {
            expires_at: plus(7200),
            ..request()
        })
        .unwrap();
    assert_eq!(fixture.transport.tokens.lock().unwrap().len(), 1);
    fixture.clock.set(plus(3600 - 225));
    fixture
        .sender
        .send(&DevicePushRequest {
            expires_at: plus(7200),
            ..request()
        })
        .unwrap();
    assert_eq!(fixture.transport.tokens.lock().unwrap().len(), 2);
}

#[test]
fn 토큰_요청은_서비스_계정_jwt를_form으로_보낸다() {
    let fixture = fixture(ok);

    fixture.sender.send(&request()).unwrap();

    let tokens = fixture.transport.tokens.lock().unwrap();
    let body = String::from_utf8(tokens[0].body.clone()).unwrap();
    let assertion = body
        .strip_prefix("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=")
        .expect("form 본문");
    let parts: Vec<&str> = assertion.split('.').collect();
    assert_eq!(parts.len(), 3);
    let decode = |part: &str| -> Value {
        // base64url 해석은 서명 크레이트 안에 있어 여기서는 표준 라이브러리로 간단히 푼다.
        serde_json::from_slice(&base64url(part)).unwrap()
    };
    assert_eq!(
        decode(parts[0]),
        serde_json::json!({"alg": "RS256", "kid": "key-id", "typ": "JWT"})
    );
    let claims = decode(parts[1]);
    assert_eq!(claims["iss"], "worker@test.iam.gserviceaccount.com");
    assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
    assert_eq!(claims["iat"], now().as_second());
    assert_eq!(claims["exp"], now().as_second() + 3600);
    assert!(claims["scope"].as_str().unwrap().contains("cloud-platform"));
    assert_eq!(
        tokens[0]
            .headers
            .iter()
            .find(|(n, _)| n == "Content-Type")
            .unwrap()
            .1,
        "application/x-www-form-urlencoded; charset=UTF-8"
    );
}

fn base64url(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("base64url이 아님"),
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

#[test]
fn 토큰_교환이_일시_오류면_1_2_4초_간격으로_세_번_다시_보내고_실패하면_unknown이다() {
    for status in [408, 429, 500, 503] {
        let fixture = fixture_with(ok, move |_, _| Ok(response(status, &[], "{}")));

        let result = fixture.sender.send(&request()).unwrap();

        assert_eq!(
            result,
            DevicePushResult::retryable_after("UNKNOWN", Duration::from_secs(60))
        );
        assert_eq!(
            fixture.transport.tokens.lock().unwrap().len(),
            4,
            "{status}"
        );
        assert_eq!(
            *fixture.sleeps.lock().unwrap(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4)
            ]
        );
        assert_eq!(fcm_calls(&fixture), 0);
    }
    // 재시도 대상이 아닌 상태는 바로 실패한다.
    for status in [400, 401, 403, 404, 501, 502, 504] {
        let fixture = fixture_with(ok, move |_, _| Ok(response(status, &[], "{}")));
        fixture.sender.send(&request()).unwrap();
        assert_eq!(
            fixture.transport.tokens.lock().unwrap().len(),
            1,
            "{status}"
        );
    }
}

#[test]
fn 토큰_교환_응답이_이상하면_unknown_또는_예외다() {
    for (body, expected) in [
        ("<html>", "UNKNOWN"),
        (r#"{"expires_in":3600}"#, "UNKNOWN"),
        (r#"{"access_token":"t"}"#, "UNKNOWN"),
        (r#"{"access_token":"t","expires_in":"3600"}"#, "UNKNOWN"),
        (r#"{"access_token":5,"expires_in":3600}"#, "UNKNOWN"),
    ] {
        let fixture = fixture_with(ok, move |_, _| Ok(response(200, &[], body)));
        let result = fixture.sender.send(&request()).unwrap();
        assert_eq!(result.error_code.as_deref(), Some(expected), "{body}");
        assert!(result.is_retryable());
    }
    // 만료 시간이 실수면 SDK가 ArithmeticException 을 던진다.
    let fixture = fixture_with(ok, |_, _| {
        Ok(response(
            200,
            &[],
            r#"{"access_token":"t","expires_in":3600.5}"#,
        ))
    });
    let failure = fixture.sender.send(&request()).unwrap_err();
    assert_eq!(failure.exception, "ArithmeticException");
    // 3600.0 은 정수로 읽힌다. 빈 토큰도 받는다.
    let fixture = fixture_with(ok, |_, _| {
        Ok(response(
            200,
            &[],
            r#"{"access_token":"","expires_in":3600.0}"#,
        ))
    });
    assert!(fixture.sender.send(&request()).unwrap().is_accepted());
}

#[test]
fn 응답을_받지_못하면_예외_종류로_오류_코드를_정한다() {
    for (error, code) in [
        (ExchangeError::Timeout, "DEADLINE_EXCEEDED"),
        (ExchangeError::UnknownHost, "UNAVAILABLE"),
        (ExchangeError::Other, "UNKNOWN"),
    ] {
        let fixture = fixture(move |_, _| Err(error.clone()));

        let result = fixture.sender.send(&request()).unwrap();

        assert_eq!(
            result,
            DevicePushResult::retryable_after(code, Duration::from_secs(60))
        );
        // 연결 오류는 다시 보내지 않는다.
        assert_eq!(fcm_calls(&fixture), 1);
    }
}

#[test]
fn 일부_오류_응답은_sdk가_예외를_던진다() {
    for body in [
        r#"{"error":"nope"}"#,
        r#"{"error":[1,2]}"#,
        r#"{"error":{"status":"FOO_BAR"}}"#,
        r#"{"error":{"status":"invalid_argument"}}"#,
        r#"{"error":{"status":"OK"}}"#,
    ] {
        let fixture = fixture(move |_, _| Ok(response(400, &[], body)));
        let failure = fixture.sender.send(&request()).unwrap_err();
        assert_eq!(failure.exception, "IllegalArgumentException", "{body}");
    }
    let body = r#"{"error":{"status":"NOT_FOUND","details":[{"@type":"type.googleapis.com/google.firebase.fcm.v1.FcmError","errorCode":5}]}}"#;
    let fixture = fixture(move |_, _| Ok(response(404, &[], body)));
    assert_eq!(
        fixture.sender.send(&request()).unwrap_err().exception,
        "ClassCastException"
    );
}

#[test]
fn 성공_응답_본문이_비었거나_객체가_아니면_예외이고_json이_아니면_unknown이다() {
    for body in ["", "[1]", "\"x\""] {
        let fixture = fixture(move |_, _| Ok(response(200, &[], body)));
        assert_eq!(
            fixture.sender.send(&request()).unwrap_err().exception,
            "IllegalArgumentException",
            "{body:?}"
        );
    }
    let fixture = fixture(|_, _| Ok(response(200, &[], "not json")));
    assert_eq!(
        fixture.sender.send(&request()).unwrap(),
        DevicePushResult::retryable_after("UNKNOWN", Duration::from_secs(60))
    );
}

#[test]
fn 오류_본문이_없으면_http_상태로_코드를_정한다() {
    for (status, code, retryable) in [
        (400, "INVALID_ARGUMENT", false),
        (403, "PERMISSION_DENIED", false),
        (404, "NOT_FOUND", false),
        (409, "CONFLICT", false),
        (412, "UNKNOWN", true),
        (429, "RESOURCE_EXHAUSTED", false),
        (500, "INTERNAL", true),
        (502, "UNKNOWN", true),
        (504, "DEADLINE_EXCEEDED", true),
    ] {
        let fixture = fixture(move |_, _| Ok(response(status, &[], "")));
        let result = fixture.sender.send(&request()).unwrap();
        assert_eq!(result.error_code.as_deref(), Some(code), "{status}");
        assert_eq!(result.is_retryable(), retryable, "{status}");
    }
}

#[test]
fn 세부_코드는_처음_만난_fcm_오류만_쓴다() {
    let fcm = |code: &str| {
        format!(
            r#"{{"@type":"type.googleapis.com/google.firebase.fcm.v1.FcmError","errorCode":"{code}"}}"#
        )
    };
    for (details, expected) in [
        (
            format!("[{},{}]", fcm("SENDER_ID_MISMATCH"), fcm("UNREGISTERED")),
            "SENDER_ID_MISMATCH",
        ),
        (
            format!("[{},{}]", fcm("NEW_CODE"), fcm("UNREGISTERED")),
            "NOT_FOUND",
        ),
        (
            format!(r#"[null,{{"@type":"other"}},{}]"#, fcm("UNREGISTERED")),
            "UNREGISTERED",
        ),
        (r#"[{"errorCode":"UNREGISTERED"}]"#.to_owned(), "NOT_FOUND"),
        (
            format!("[{}]", fcm("APNS_AUTH_ERROR")),
            "THIRD_PARTY_AUTH_ERROR",
        ),
        ("{}".to_owned(), "NOT_FOUND"),
    ] {
        let body = format!(r#"{{"error":{{"status":"NOT_FOUND","details":{details}}}}}"#);
        let fixture = fixture(move |_, _| Ok(response(404, &[], &body)));
        let result = fixture.sender.send(&request()).unwrap();
        assert_eq!(result.error_code.as_deref(), Some(expected), "{details}");
    }
}

#[test]
fn 응답_헤더의_retry_after는_503_재시도에도_쓴다() {
    let body = error_body(503, "UNAVAILABLE", "UNAVAILABLE");
    // 1~60초는 그만큼, 0·잘못된 값·지난 날짜·음수는 0.5초 백오프, 60초 초과는 재시도하지 않는다.
    for (header, expected) in [
        ("1", Some(Duration::from_secs(1))),
        ("+1", Some(Duration::from_secs(1))),
        ("60", Some(Duration::from_secs(60))),
        ("0", Some(Duration::from_millis(500))),
        ("-5", Some(Duration::from_millis(500))),
        ("later", Some(Duration::from_millis(500))),
        ("1.5", Some(Duration::from_millis(500))),
        (
            "Tue, 06 Oct 2026 00:00:00 GMT",
            Some(Duration::from_millis(500)),
        ),
        (
            "Wed, 07 Oct 2026 00:00:02 GMT",
            Some(Duration::from_secs(2)),
        ),
        ("Wed, 07 Oct 2026 00:01:40 GMT", None),
        ("61", None),
    ] {
        let body = body.clone();
        let headers: &'static [(&'static str, &'static str)] =
            Box::leak(Box::new([("Retry-After", header)]));
        let fixture = fixture(move |_, attempt| {
            if attempt == 1 {
                Ok(response(503, headers, &body))
            } else {
                Ok(response(200, &[], ACCEPTED))
            }
        });

        let result = fixture.sender.send(&request()).unwrap();

        match expected {
            Some(delay) => {
                assert!(result.is_accepted(), "{header}");
                assert_eq!(*fixture.sleeps.lock().unwrap(), vec![delay], "{header}");
            }
            None => {
                assert!(result.is_retryable(), "{header}");
                assert!(fixture.sleeps.lock().unwrap().is_empty());
                assert_eq!(fcm_calls(&fixture), 1);
            }
        }
    }
}

#[test]
fn 계속_503이면_네_번_다시_보내고_0_5_1_2_4초를_기다린다() {
    let body = error_body(503, "UNAVAILABLE", "UNAVAILABLE");
    let fixture = fixture(move |_, _| Ok(response(503, &[], &body)));

    let result = fixture.sender.send(&request()).unwrap();

    assert_eq!(
        result,
        DevicePushResult::retryable_after("UNAVAILABLE", Duration::from_secs(60))
    );
    assert_eq!(fcm_calls(&fixture), 5);
    assert_eq!(
        *fixture.sleeps.lock().unwrap(),
        vec![
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4)
        ]
    );
}

#[test]
fn 일시_오류_500_429는_다시_보내지_않는다() {
    for status in [500, 429] {
        let fixture = fixture(move |_, _| Ok(response(status, &[], "")));
        fixture.sender.send(&request()).unwrap();
        assert_eq!(fcm_calls(&fixture), 1);
        assert!(fixture.sleeps.lock().unwrap().is_empty());
    }
}

#[test]
fn 응답_401은_토큰을_새로_받고_바로_다시_보낸다() {
    let body = error_body(401, "UNAUTHENTICATED", "THIRD_PARTY_AUTH_ERROR");
    // WWW-Authenticate 가 없거나 Bearer 가 아니거나 invalid_token 이면 갱신하고, 다른 Bearer 오류는 갱신하지 않는다.
    for (www, refreshes) in [
        (None, true),
        (Some(r#"Basic realm="x""#), true),
        (Some(r#"Bearer error="invalid_token""#), true),
        (Some("bearer error = invalid_token"), true),
        (Some(r#"Bearer realm="x""#), false),
        (Some(r#"Bearer error="insufficient_scope""#), false),
    ] {
        let body = body.clone();
        let headers: &'static [(&'static str, &'static str)] = match www {
            Some(value) => Box::leak(Box::new([("WWW-Authenticate", value)])),
            None => &[],
        };
        let fixture = fixture(move |_, attempt| {
            if attempt == 1 {
                Ok(response(401, headers, &body))
            } else {
                Ok(response(200, &[], ACCEPTED))
            }
        });

        let result = fixture.sender.send(&request()).unwrap();

        if refreshes {
            assert!(result.is_accepted(), "{www:?}");
            assert_eq!(fcm_calls(&fixture), 2);
            assert_eq!(fixture.transport.tokens.lock().unwrap().len(), 2);
        } else {
            assert_eq!(
                result,
                DevicePushResult::permanent_failure("UNAUTHENTICATED"),
                "{www:?}"
            );
            assert_eq!(fcm_calls(&fixture), 1);
        }
        assert!(fixture.sleeps.lock().unwrap().is_empty());
    }
}

#[test]
fn 계속_401이면_다섯_번_보내고_토큰은_여섯_번_받는다() {
    // 401 본문의 status·세부 코드는 쓰지 않는다.
    let body = error_body(401, "PERMISSION_DENIED", "UNREGISTERED");
    let fixture = fixture(move |_, _| Ok(response(401, &[], &body)));

    let result = fixture.sender.send(&request()).unwrap();

    assert_eq!(
        result,
        DevicePushResult::permanent_failure("UNAUTHENTICATED")
    );
    assert_eq!(fcm_calls(&fixture), 5);
    assert_eq!(fixture.transport.tokens.lock().unwrap().len(), 6);
}

#[test]
fn 요청_헤더와_주소() {
    let fixture = fixture(ok);

    fixture.sender.send(&request()).unwrap();

    let seen = fixture.transport.fcm.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "http://127.0.0.1:1/v1/projects/test/messages:send"
    );
    let header = |name: &str| {
        seen[0]
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("authorization"), Some("Bearer test-token"));
    assert_eq!(
        header("content-type"),
        Some("application/json; charset=UTF-8")
    );
    assert_eq!(header("x-firebase-client"), Some("fire-admin-java/9.11.0"));
    assert_eq!(header("x-goog-api-format-version"), Some("2"));
}

#[test]
fn 서비스_계정_json은_필수_필드와_키를_기동_때_확인한다() {
    let key = serde_json::to_string(KEY).unwrap();
    let valid = |extra: &str| {
        format!(
            r#"{{"type":"service_account","private_key_id":"k","private_key":{key},"client_email":"e@x","client_id":"1"{extra}}}"#
        )
    };
    assert!(ServiceAccount::from_json(&valid("")).is_ok());
    assert!(
        ServiceAccount::from_json(&valid(r#","token_uri":"https://oauth2.example/token""#)).is_ok()
    );
    for bad in [
        "not json".to_owned(),
        "[]".to_owned(),
        valid("").replace("service_account", "authorized_user"),
        valid("").replace(r#""type":"service_account","#, ""),
        valid("").replace(r#""client_id":"1""#, r#""client_id2":"1""#),
        valid("").replace(r#""private_key_id":"k","#, ""),
        valid(r#","token_uri":"ftp://x""#),
        valid("").replace(&key, r#""not a pem""#),
    ] {
        assert!(ServiceAccount::from_json(&bad).is_err(), "{bad}");
    }
}
