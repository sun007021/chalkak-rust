//! 응답 규칙 목록. 위에서부터 처음 맞는 규칙을 쓰고, 맞는 규칙이 없으면 호출한 쪽의 기본 응답을 쓴다.
//! SQS(`sqs`), FCM 발송(`fcm`), OAuth 토큰 교환(`oauth`)이 같은 형식을 쓴다.
//!
//! 규칙마다 `op`(SQS 작업 이름, 생략하면 모두), `contains`(요청 본문에 든 문자열, 생략하면 모두), `status`, `content_type`,
//! `body`, `headers`(객체), `delay_ms`(응답 전 대기)를 줄 수 있다.

use crate::http::Response;
use serde_json::{Map, Value};

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

impl Rule {
    pub fn matches(&self, op: &str, body: &str) -> bool {
        self.op.as_deref().is_none_or(|o| o == op)
            && self.contains.as_deref().is_none_or(|c| body.contains(c))
    }

    pub fn response(&self) -> Response {
        let mut res = Response::new(self.status, &self.content_type, self.body.clone());
        res.headers.extend(self.headers.clone());
        res.delay_ms = self.delay_ms;
        res
    }
}

/// 처음 맞는 규칙의 응답.
pub fn respond(rules: &[Rule], op: &str, body: &str) -> Option<Response> {
    rules
        .iter()
        .find(|r| r.matches(op, body))
        .map(Rule::response)
}

/// `value`(규칙 배열)를 읽는다. `label`은 오류 설명에 쓰는 항목 이름이다.
pub fn parse_rules(
    label: &str,
    default_content_type: &str,
    value: &Value,
) -> Result<Vec<Rule>, String> {
    let list = value
        .as_array()
        .ok_or_else(|| format!("{label}는 규칙 배열이어야 합니다"))?;
    list.iter()
        .enumerate()
        .map(|(i, v)| parse_rule(label, default_content_type, i, v))
        .collect()
}

fn parse_rule(
    label: &str,
    default_content_type: &str,
    i: usize,
    v: &Value,
) -> Result<Rule, String> {
    let name = format!("{label}[{i}]");
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
        content_type: text("content_type").unwrap_or_else(|| default_content_type.to_owned()),
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
