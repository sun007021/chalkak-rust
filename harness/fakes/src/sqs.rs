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
use chalkak_aws::sqs::md5_hex;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

const JSON_1_0: &str = "application/x-amz-json-1.0";

#[derive(Debug, Clone)]
pub struct Rule {
    op: Option<String>,
    contains: Option<String>,
    status: u16,
    content_type: String,
    body: String,
    headers: Vec<(String, String)>,
    delay_ms: u64,
}

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
        let body = String::from_utf8_lossy(&req.body).into_owned();
        if let Some(rule) = self
            .rules
            .iter()
            .find(|r| {
                r.op.as_deref().is_none_or(|o| o == op)
                    && r.contains.as_deref().is_none_or(|c| body.contains(c))
            })
            .cloned()
        {
            let mut res = Response::new(rule.status, &rule.content_type, rule.body);
            res.headers.extend(rule.headers);
            res.delay_ms = rule.delay_ms;
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
        ok(&json!({ "Messages": out }))
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
        let list = value.as_array().ok_or("sqs는 규칙 배열이어야 합니다")?;
        list.iter()
            .enumerate()
            .map(|(i, v)| parse_rule(i, v))
            .collect()
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

fn parse_rule(i: usize, v: &Value) -> Result<Rule, String> {
    let name = format!("sqs[{i}]");
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{name}은 객체여야 합니다"))?;
    let status = obj
        .get("status")
        .and_then(Value::as_u64)
        .filter(|s| (100..600).contains(s))
        .ok_or_else(|| format!("{name}.status가 필요합니다"))?;
    let text = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_owned);
    let headers = match obj.get("headers") {
        None => Vec::new(),
        Some(h) => headers_of(&name, h)?,
    };
    Ok(Rule {
        op: text("op"),
        contains: text("contains"),
        status: status as u16,
        content_type: text("content_type").unwrap_or_else(|| JSON_1_0.to_owned()),
        body: text("body").unwrap_or_default(),
        headers,
        delay_ms: obj.get("delay_ms").and_then(Value::as_u64).unwrap_or(0),
    })
}

fn headers_of(name: &str, h: &Value) -> Result<Vec<(String, String)>, String> {
    let map: &Map<String, Value> = h
        .as_object()
        .ok_or_else(|| format!("{name}.headers는 객체여야 합니다"))?;
    map.iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|s| (k.clone(), s.to_owned()))
                .ok_or_else(|| format!("{name}.headers.{k}는 문자열이어야 합니다"))
        })
        .collect()
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
