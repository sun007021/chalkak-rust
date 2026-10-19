//! SQS(JSON 1.0 프로토콜) 가짜. `POST /` 와 `X-Amz-Target: AmazonSQS.<작업>` 헤더로 들어온 요청을 받는다.
//! AWS SDK for Java와 Rust 클라이언트가 같은 `AWS_ENDPOINT_URL_SQS` 로 이 서버를 본다(큐 URL은 본문에만 들어간다).
//!
//! 지원 작업: `SendMessage`(본문 저장, 기본 성공), `ReceiveMessage`, `DeleteMessage`, `ChangeMessageVisibility`.
//! 큐는 하나로 보고 `QueueUrl` 은 기록만 한다. 롱 폴링은 흉내 내지 않는다: `ReceiveMessage` 는 보이는 메시지를 바로 돌려주고
//! 없으면 빈 목록을 돌려준다(`WaitTimeSeconds` 무시).
//!
//! 응답 지정(`PUT /__admin/responses` 의 `sqs` 항목): 규칙 목록이고 위에서부터 처음 맞는 것을 쓴다. 규칙마다
//! `op`(작업 이름, 생략하면 모두), `contains`(요청 본문에 든 문자열, 생략하면 모두), `status`, `content_type`,
//! `body`, `headers`(객체), `delay_ms`(응답 전 대기)를 줄 수 있다. 맞는 규칙이 없으면 정상 응답이다.

use crate::http::{Request, Response};
use crate::rules::{self, Rule};
use chalkak_aws::sqs::md5_hex;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const JSON_1_0: &str = "application/x-amz-json-1.0";

/// 롱 폴링을 흉내 내며 기다리는 최대 시간. 앱은 20초를 요청하지만 시나리오 사이 초기화가 오래 막히지 않게 줄인다.
pub const LONG_POLL_CAP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Stored {
    id: String,
    body: String,
    receive_count: u32,
    /// 이 시각 전에는 `ReceiveMessage` 가 돌려주지 않는다.
    visible_at: Instant,
    receipt: Option<String>,
}

#[derive(Debug, Default)]
pub struct Sqs {
    rules: Vec<Rule>,
    messages: Vec<Stored>,
    next_id: u64,
    next_receipt: u64,
    /// 마지막 `ReceiveMessage`가 빈 목록을 돌려줬는지(규칙 응답은 해당하지 않는다).
    last_receive_empty: bool,
}

impl Sqs {
    /// 요청 대상 작업 이름. SQS 요청이 아니면 `None`.
    pub fn operation(req: &Request) -> Option<String> {
        if req.method != "POST" || req.path != "/" {
            return None;
        }
        req.header("x-amz-target")?
            .strip_prefix("AmazonSQS.")
            .map(str::to_owned)
    }

    pub fn handle(&mut self, op: &str, req: &Request) -> Response {
        self.last_receive_empty = false;
        let body = String::from_utf8_lossy(&req.body).into_owned();
        if let Some(res) = rules::respond(&self.rules, op, &body) {
            return res;
        }
        let input: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        match op {
            "SendMessage" => self.send(&input),
            "ReceiveMessage" => self.receive(&input),
            "DeleteMessage" => self.delete(&input),
            "ChangeMessageVisibility" => self.change_visibility(&input),
            _ => error(
                400,
                "com.amazon.coral.service#UnknownOperationException",
                "unknown operation",
            ),
        }
    }

    fn send(&mut self, input: &Value) -> Response {
        let Some(body) = input.get("MessageBody").and_then(Value::as_str) else {
            return error(
                400,
                "com.amazon.coral.service#MissingParameter",
                "MessageBody is required",
            );
        };
        self.next_id += 1;
        let id = format!("fake-message-{}", self.next_id);
        self.messages.push(Stored {
            id: id.clone(),
            body: body.to_owned(),
            receive_count: 0,
            visible_at: Instant::now(),
            receipt: None,
        });
        ok(&json!({ "MD5OfMessageBody": md5_hex(body.as_bytes()), "MessageId": id }))
    }

    fn receive(&mut self, input: &Value) -> Response {
        let max = input
            .get("MaxNumberOfMessages")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 10) as usize;
        let visibility = input
            .get("VisibilityTimeout")
            .and_then(Value::as_u64)
            .unwrap_or(30);
        let now = Instant::now();
        let mut out = Vec::new();
        for m in self.messages.iter_mut().filter(|m| m.visible_at <= now) {
            if out.len() == max {
                break;
            }
            self.next_receipt += 1;
            m.receive_count += 1;
            m.visible_at = now + Duration::from_secs(visibility);
            let receipt = format!("fake-receipt-{}", self.next_receipt);
            m.receipt = Some(receipt.clone());
            out.push(json!({
                "MessageId": m.id,
                "ReceiptHandle": receipt,
                "Body": m.body,
                "MD5OfBody": md5_hex(m.body.as_bytes()),
                "Attributes": { "ApproximateReceiveCount": m.receive_count.to_string() },
            }));
        }
        self.last_receive_empty = out.is_empty();
        ok(&json!({ "Messages": out }))
    }

    /// 마지막 `ReceiveMessage`가 빈 목록이었는지. 비어 있으면 호출한 쪽이 `long_poll_wait`만큼 메시지를 기다린다.
    pub fn last_receive_empty(&self) -> bool {
        self.last_receive_empty
    }

    /// 롱 폴링으로 기다릴 시간: 요청의 `WaitTimeSeconds`를 `LONG_POLL_CAP`으로 줄인 값.
    pub fn long_poll_wait(req: &Request) -> Duration {
        let wait = serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|v| v.get("WaitTimeSeconds").and_then(Value::as_u64))
            .unwrap_or(0);
        Duration::from_secs(wait).min(LONG_POLL_CAP)
    }

    /// 시나리오가 큐에 미리 넣어 두는 메시지(`SendMessage`처럼 저장하지만 기록·규칙은 거치지 않는다).
    pub fn seed(&mut self, body: &str) {
        self.next_id += 1;
        self.messages.push(Stored {
            id: format!("fake-message-{}", self.next_id),
            body: body.to_owned(),
            receive_count: 0,
            visible_at: Instant::now(),
            receipt: None,
        });
    }

    fn delete(&mut self, input: &Value) -> Response {
        let handle = input.get("ReceiptHandle").and_then(Value::as_str);
        match self
            .messages
            .iter()
            .position(|m| handle.is_some() && m.receipt.as_deref() == handle)
        {
            Some(i) => {
                self.messages.remove(i);
                ok(&json!({}))
            }
            None => error(
                400,
                "com.amazon.coral.service#ReceiptHandleIsInvalid",
                "The input receipt handle is invalid.",
            ),
        }
    }

    fn change_visibility(&mut self, input: &Value) -> Response {
        let handle = input.get("ReceiptHandle").and_then(Value::as_str);
        let seconds = input
            .get("VisibilityTimeout")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match self
            .messages
            .iter_mut()
            .find(|m| handle.is_some() && m.receipt.as_deref() == handle)
        {
            Some(m) => {
                m.visible_at = Instant::now() + Duration::from_secs(seconds);
                ok(&json!({}))
            }
            None => error(
                400,
                "com.amazon.coral.service#ReceiptHandleIsInvalid",
                "The input receipt handle is invalid.",
            ),
        }
    }

    /// `sqs` 지정을 읽어 규칙으로 바꾼다. 형식이 틀리면 설명을 돌려준다.
    pub fn parse_rules(value: &Value) -> Result<Vec<Rule>, String> {
        rules::parse_rules("sqs", JSON_1_0, value)
    }

    pub fn set_rules(&mut self, rules: Vec<Rule>) {
        self.rules = rules;
    }
}

fn ok(body: &Value) -> Response {
    Response::new(200, JSON_1_0, body.to_string())
}

fn error(status: u16, kind: &str, message: &str) -> Response {
    Response::new(
        status,
        JSON_1_0,
        json!({ "__type": kind, "message": message }).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(op: &str, body: &str) -> Request {
        Request {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![("x-amz-target".into(), format!("AmazonSQS.{op}"))],
            body: body.as_bytes().to_vec(),
            close: false,
        }
    }

    fn call(sqs: &mut Sqs, op: &str, body: &str) -> (u16, Value) {
        let res = sqs.handle(op, &req(op, body));
        (
            res.status,
            serde_json::from_slice(&res.body).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn 작업_이름은_target_헤더에서만_읽는다() {
        assert_eq!(
            Sqs::operation(&req("SendMessage", "{}")).as_deref(),
            Some("SendMessage")
        );
        let mut other = req("SendMessage", "{}");
        other.path = "/x".into();
        assert_eq!(Sqs::operation(&other), None);
        other.path = "/".into();
        other.headers.clear();
        assert_eq!(Sqs::operation(&other), None);
    }

    #[test]
    fn 보낸_메시지는_md5와_id를_돌려주고_받을_수_있다() {
        let mut sqs = Sqs::default();
        let (status, v) = call(&mut sqs, "SendMessage", r#"{"MessageBody":"hello 한글"}"#);
        assert_eq!(status, 200);
        assert_eq!(v["MD5OfMessageBody"], md5_hex("hello 한글".as_bytes()));
        assert_eq!(v["MessageId"], "fake-message-1");

        let (_, v) = call(&mut sqs, "ReceiveMessage", r#"{"MaxNumberOfMessages":10}"#);
        let m = &v["Messages"][0];
        assert_eq!(m["Body"], "hello 한글");
        assert_eq!(m["Attributes"]["ApproximateReceiveCount"], "1");
        // 가시성 제한 중이라 다시 받을 수 없다.
        let (_, again) = call(&mut sqs, "ReceiveMessage", "{}");
        assert_eq!(again["Messages"].as_array().unwrap().len(), 0);

        let handle = m["ReceiptHandle"].as_str().unwrap().to_owned();
        let (status, _) = call(
            &mut sqs,
            "DeleteMessage",
            &json!({ "ReceiptHandle": handle }).to_string(),
        );
        assert_eq!(status, 200);
        let (status, v) = call(
            &mut sqs,
            "DeleteMessage",
            &json!({ "ReceiptHandle": handle }).to_string(),
        );
        assert_eq!(status, 400);
        assert!(
            v["__type"]
                .as_str()
                .unwrap()
                .contains("ReceiptHandleIsInvalid")
        );
    }

    #[test]
    fn 가시성을_0으로_바꾸면_바로_다시_받는다() {
        let mut sqs = Sqs::default();
        call(&mut sqs, "SendMessage", r#"{"MessageBody":"a"}"#);
        let (_, v) = call(&mut sqs, "ReceiveMessage", "{}");
        let handle = v["Messages"][0]["ReceiptHandle"].clone();
        call(
            &mut sqs,
            "ChangeMessageVisibility",
            &json!({ "ReceiptHandle": handle, "VisibilityTimeout": 0 }).to_string(),
        );
        let (_, v) = call(&mut sqs, "ReceiveMessage", "{}");
        assert_eq!(
            v["Messages"][0]["Attributes"]["ApproximateReceiveCount"],
            "2"
        );
    }

    #[test]
    fn 규칙은_위에서부터_처음_맞는_것을_쓴다() {
        let mut sqs = Sqs::default();
        let rules = Sqs::parse_rules(&json!([
            {"op":"SendMessage","contains":"boom","status":500,
             "body":"{\"__type\":\"InternalError\"}","delay_ms":7,
             "headers":{"x-amzn-query-error":"AWS.SimpleQueueService.X;Sender"}},
            {"contains":"throttle","status":400,"body":"{\"__type\":\"RequestThrottled\"}"}
        ]))
        .unwrap();
        sqs.set_rules(rules);
        let res = sqs.handle(
            "SendMessage",
            &req("SendMessage", r#"{"MessageBody":"boom"}"#),
        );
        assert_eq!((res.status, res.delay_ms), (500, 7));
        assert!(res.headers.iter().any(|(k, _)| k == "x-amzn-query-error"));
        let (status, _) = call(&mut sqs, "SendMessage", r#"{"MessageBody":"throttle"}"#);
        assert_eq!(status, 400);
        let (status, _) = call(&mut sqs, "SendMessage", r#"{"MessageBody":"fine"}"#);
        assert_eq!(status, 200);
        // 규칙에 걸린 메시지는 저장하지 않는다.
        assert_eq!(sqs.messages.len(), 1);
    }

    #[test]
    fn 잘못된_규칙은_설명과_함께_거절한다() {
        for bad in [
            json!({}),
            json!([1]),
            json!([{}]),
            json!([{"status":200,"headers":{"a":1}}]),
        ] {
            assert!(Sqs::parse_rules(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn 모르는_작업은_400이다() {
        let (status, v) = call(&mut Sqs::default(), "PurgeQueue", "{}");
        assert_eq!(status, 400);
        assert!(v["__type"].as_str().unwrap().contains("UnknownOperation"));
    }
}
