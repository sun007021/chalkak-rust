//! 가짜 외부 서버(`harness/fakes`) 관리 클라이언트. 시나리오 시작 시 초기화하고 응답을 지정하며,
//! 끝나면 받은 요청 기록을 가져와 정규화한다.

use crate::http::{self, Request};
use crate::normalize::Normalizer;
use crate::scenario::{CannedSpec, FakesSpec, S3Spec, SqsRuleSpec};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

fn admin(
    base_url: &str,
    method: &str,
    path: &str,
    body: Option<String>,
) -> Result<Vec<u8>, String> {
    let req = Request {
        method: method.into(),
        path: path.into(),
        headers: Vec::new(),
        body: body.map(String::into_bytes),
    };
    let res = http::send(base_url, &req).map_err(|e| format!("fakes {path}: {e}"))?;
    if res.status != 200 {
        return Err(format!(
            "fakes {path}: {} {}",
            res.status,
            String::from_utf8_lossy(&res.body)
        ));
    }
    Ok(res.body)
}

/// 지정과 기록을 모두 비운다.
pub fn reset(base_url: &str) -> Result<(), String> {
    admin(base_url, "POST", "/__admin/reset", Some("{}".into())).map(|_| ())
}

/// 응답을 통째로 바꾼다. `subst`는 문자열 안의 `${변수}`를 채운다.
pub fn configure(
    base_url: &str,
    spec: &FakesSpec,
    subst: &dyn Fn(&str) -> String,
) -> Result<(), String> {
    admin(
        base_url,
        "PUT",
        "/__admin/responses",
        Some(admin_json(spec, subst).to_string()),
    )
    .map(|_| ())
}

pub fn records(base_url: &str) -> Result<Vec<Value>, String> {
    let body = admin(base_url, "GET", "/__admin/requests", None)?;
    serde_json::from_slice(&body).map_err(|e| format!("fakes 기록 해석 실패: {e}"))
}

fn canned_json(c: &CannedSpec, subst: &dyn Fn(&str) -> String) -> Value {
    let mut o = Map::new();
    o.insert("status".into(), json!(c.status));
    if let Some(ct) = &c.content_type {
        o.insert("content_type".into(), json!(ct));
    }
    if let Some(b) = &c.body {
        o.insert("body".into(), json!(subst(b)));
    }
    Value::Object(o)
}

fn s3_json(s: &S3Spec) -> Value {
    let mut o = Map::new();
    if let Some(v) = s.status {
        o.insert("status".into(), json!(v));
    }
    if let Some(v) = s.size {
        o.insert("size".into(), json!(v));
    }
    if let Some(v) = &s.content_type {
        o.insert("content_type".into(), json!(v));
    }
    Value::Object(o)
}

fn sqs_json(rules: &[SqsRuleSpec], subst: &dyn Fn(&str) -> String) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|r| {
                let mut o = Map::new();
                o.insert("status".into(), json!(r.status));
                if let Some(v) = &r.op {
                    o.insert("op".into(), json!(v));
                }
                if let Some(v) = &r.contains {
                    o.insert("contains".into(), json!(subst(v)));
                }
                if let Some(v) = &r.content_type {
                    o.insert("content_type".into(), json!(v));
                }
                if let Some(v) = &r.body {
                    o.insert("body".into(), json!(subst(v)));
                }
                if !r.headers.is_empty() {
                    o.insert("headers".into(), json!(r.headers));
                }
                if let Some(v) = r.delay_ms {
                    o.insert("delay_ms".into(), json!(v));
                }
                Value::Object(o)
            })
            .collect(),
    )
}

pub fn admin_json(spec: &FakesSpec, subst: &dyn Fn(&str) -> String) -> Value {
    let mut o = Map::new();
    for (name, c) in [
        ("apple_token", &spec.apple_token),
        ("apple_revoke", &spec.apple_revoke),
        ("slack", &spec.slack),
    ] {
        if let Some(c) = c {
            o.insert(name.into(), canned_json(c, subst));
        }
    }
    if let Some(logs) = &spec.logs {
        let mut l = Map::new();
        if let Some(status) = &logs.status {
            l.insert("status".into(), json!(status));
        }
        l.insert("rows".into(), json!(logs.rows));
        o.insert("logs".into(), Value::Object(l));
    }
    let s3: Map<String, Value> = spec
        .s3
        .iter()
        .map(|(k, v)| (subst(k), s3_json(v)))
        .collect();
    o.insert("s3".into(), Value::Object(s3));
    if let Some(rules) = &spec.sqs {
        o.insert("sqs".into(), sqs_json(rules, subst));
    }
    Value::Object(o)
}

/// 기록을 비교용 값으로 바꾼다. 비교에서 빼거나 줄이는 것:
/// - JWKS 조회: 앱의 공개키 캐시 상태에 따라 횟수가 달라 동작 차이가 아니다.
/// - 요청 헤더: Content-Type만 남긴다(User-Agent, Host, SDK 서명 헤더는 구현 세부).
/// - S3: 경로·가상 호스트 방식 차이를 버리고 `버킷/키`만 남긴다.
/// - SQS: 작업 이름과 큐 URL, 메시지 본문(JSON이면 풀어서 정규화)만 남긴다. 서명·SDK 헤더는 구현 세부다.
/// - Apple 요청 본문: 폼 필드로 풀어 이름순으로 비교하고, `client_secret`(JWT)은 클레임을 풀어 정규화한다.
pub fn normalize(records: &[Value], norm: &mut Normalizer) -> Value {
    let mut out = Vec::new();
    for r in records {
        let service = r["service"].as_str().unwrap_or("");
        if service.starts_with("oidc_jwks") {
            continue;
        }
        let body = r["body"].as_str().unwrap_or("");
        let mut o = Map::new();
        o.insert("service".into(), json!(service));
        o.insert("method".into(), r["method"].clone());
        if let Some(detail) = r["detail"].as_str() {
            o.insert("detail".into(), json!(norm.string(detail)));
        } else {
            let path = r["path"].as_str().unwrap_or("");
            o.insert("path".into(), json!(norm.string(path)));
            if let Some(q) = r["query"].as_str().filter(|q| !q.is_empty()) {
                o.insert("query".into(), json!(norm.string(q)));
            }
        }
        if let Some(ct) = r["headers"]["content-type"].as_str() {
            o.insert("contentType".into(), json!(ct));
        }
        if !body.is_empty() {
            let v = match service {
                "apple_token" | "apple_revoke" => form_value(body, norm),
                "sqs" => sqs_body(body, norm),
                "slack" | "cloudwatch_logs" => match serde_json::from_str::<Value>(body) {
                    Ok(v) => norm.value(&v, &[], ""),
                    Err(_) => json!(norm.string(body)),
                },
                _ => json!(norm.string(body)),
            };
            o.insert("body".into(), v);
        }
        out.push(Value::Object(o));
    }
    Value::Array(out)
}

/// SQS 요청 본문(`{"QueueUrl":..., "MessageBody":"<JSON 문자열>"}`)에서 `MessageBody`를 JSON으로 풀어 정규화한다.
/// 키 순서는 비교하지 않으므로(정규화 결과가 정렬된다) 바이트 동일 여부는 cargo 시험이 확인한다.
fn sqs_body(body: &str, norm: &mut Normalizer) -> Value {
    let Ok(Value::Object(mut req)) = serde_json::from_str::<Value>(body) else {
        return json!(norm.string(body));
    };
    if let Some(Value::String(message)) = req.get("MessageBody").cloned() {
        let inner = match serde_json::from_str::<Value>(&message) {
            Ok(v) => norm.value(&v, &[], ""),
            Err(_) => json!(norm.string(&message)),
        };
        req.insert("MessageBody".into(), inner);
    }
    norm.value(&Value::Object(req), &[], "")
}

fn form_value(body: &str, norm: &mut Normalizer) -> Value {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for pair in body.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        fields.insert(percent_decode(k), percent_decode(v));
    }
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k, Value::String(norm.string(&v))))
            .collect(),
    )
}

/// `application/x-www-form-urlencoded` 값 해석: `+`는 공백, `%XX`는 바이트.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if b.get(i + 1..i + 3).is_some() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::base64url_encode;

    const CLOCK: f64 = 1_791_763_200.0;

    fn fake_jwt(claims: &Value) -> String {
        format!(
            "{}.{}.c2ln",
            base64url_encode(br#"{"alg":"ES256","kid":"k"}"#),
            base64url_encode(claims.to_string().as_bytes())
        )
    }

    #[test]
    fn 지정을_관리_json으로_바꾸고_변수를_채운다() {
        let spec: FakesSpec = toml::from_str(
            "[apple_token]\nstatus = 200\nbody = \"id=${t}\"\n[s3.\"k/${u}.png\"]\nsize = 5\ncontent_type = \"image/png\"\n",
        )
        .unwrap();
        let v = admin_json(&spec, &|s| s.replace("${t}", "T").replace("${u}", "U"));
        assert_eq!(v["apple_token"], json!({"status": 200, "body": "id=T"}));
        assert_eq!(
            v["s3"]["k/U.png"],
            json!({"size": 5, "content_type": "image/png"})
        );
        assert!(v.get("slack").is_none());
    }

    #[test]
    fn jwks_조회는_비교에서_뺀다() {
        let recs = vec![
            json!({"service": "oidc_jwks:google", "method": "GET", "path": "/x", "query": "", "headers": {}, "body": ""}),
        ];
        assert_eq!(normalize(&recs, &mut Normalizer::new(CLOCK)), json!([]));
    }

    #[test]
    fn apple_폼은_필드별로_풀고_client_secret_클레임을_정규화한다() {
        let secret = fake_jwt(&json!({"iss": "team", "iat": 1_791_763_201, "exp": 1_791_763_801}));
        let body = format!("grant_type=authorization_code&code=a%2Bb+c&client_secret={secret}");
        let recs = vec![
            json!({"service": "apple_token", "method": "POST", "path": "/apple/auth/token",
            "query": "", "headers": {"content-type": "application/x-www-form-urlencoded", "user-agent": "x"}, "body": body}),
        ];
        let out = normalize(&recs, &mut Normalizer::new(CLOCK));
        let r = &out[0];
        assert_eq!(r["contentType"], "application/x-www-form-urlencoded");
        assert_eq!(r["body"]["code"], "a+b c");
        assert_eq!(r["body"]["grant_type"], "authorization_code");
        let cs = r["body"]["client_secret"].as_str().unwrap();
        assert!(cs.starts_with("<jwt "), "{cs}");
        assert!(
            cs.contains("<t+0s epoch>") && cs.contains("<t+600s epoch>"),
            "{cs}"
        );
        assert!(r.get("headers").is_none());
    }

    #[test]
    fn s3는_버킷과_키만_남기고_uuid를_기호로_바꾼다() {
        let recs = vec![
            json!({"service": "s3_head", "method": "HEAD", "path": "/p/0199aaaa-0000-7000-8000-000000000001.png",
            "query": "", "headers": {"host": "test-bucket.fakes:9000"}, "body": "",
            "detail": "test-bucket/p/0199aaaa-0000-7000-8000-000000000001.png"}),
        ];
        let out = normalize(&recs, &mut Normalizer::new(CLOCK));
        assert_eq!(
            out,
            json!([{"service": "s3_head", "method": "HEAD", "detail": "test-bucket/p/$uuid1.png"}])
        );
    }

    #[test]
    fn slack_본문은_json으로_풀어_정규화한다() {
        let recs = vec![
            json!({"service": "slack", "method": "POST", "path": "/slack/services/T/B/X", "query": "",
            "headers": {"content-type": "application/json"}, "body": r#"{"text":"t","at":"2026-10-12T00:00:05Z"}"#}),
        ];
        let out = normalize(&recs, &mut Normalizer::new(CLOCK));
        assert_eq!(out[0]["body"], json!({"text": "t", "at": "<t+5s iso Z>"}));
    }

    #[test]
    fn 퍼센트_해석() {
        assert_eq!(percent_decode("a%2Fb+c%"), "a/b c%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn sqs_규칙을_관리_json으로_바꾸고_변수를_채운다() {
        let spec: FakesSpec = toml::from_str(
            "[[sqs]]\nop = \"SendMessage\"\ncontains = \"${n}\"\nstatus = 500\ndelay_ms = 7\nheaders = { a = \"b\" }\n",
        )
        .unwrap();
        let v = admin_json(&spec, &|s| s.replace("${n}", "N1"));
        assert_eq!(
            v["sqs"],
            json!([{"status": 500, "op": "SendMessage", "contains": "N1",
                    "headers": {"a": "b"}, "delay_ms": 7}])
        );
        let none: FakesSpec = toml::from_str("").unwrap();
        assert!(admin_json(&none, &|s| s.into()).get("sqs").is_none());
    }

    #[test]
    fn sqs_기록은_메시지_본문을_풀어_정규화한다() {
        let message =
            r#"{"id":"0199aaaa-0000-7000-8000-000000000001","at":"2026-10-12T00:00:05Z"}"#;
        let body = json!({"QueueUrl": "https://sqs.x/1/q", "MessageBody": message}).to_string();
        let recs = vec![
            json!({"service": "sqs", "method": "POST", "path": "/", "query": "",
            "headers": {"content-type": "application/x-amz-json-1.0", "authorization": "x"},
            "body": body, "detail": "SendMessage"}),
        ];
        let out = normalize(&recs, &mut Normalizer::new(CLOCK));
        assert_eq!(out[0]["detail"], "SendMessage");
        assert_eq!(out[0]["contentType"], "application/x-amz-json-1.0");
        assert_eq!(
            out[0]["body"]["MessageBody"],
            json!({"id": "$uuid1", "at": "<t+5s iso Z>"})
        );
        assert_eq!(out[0]["body"]["QueueUrl"], "https://sqs.x/1/q");
    }

    #[test]
    fn 로그_지정을_관리_json으로_바꾼다() {
        let spec: FakesSpec =
            toml::from_str("[logs]\nstatus = \"Failed\"\n[logs.rows.visitors]\nvisitors = \"7\"\n")
                .unwrap();
        let v = admin_json(&spec, &|s| s.to_string());
        assert_eq!(
            v["logs"],
            json!({"status": "Failed", "rows": {"visitors": {"visitors": "7"}}})
        );
    }

    #[test]
    fn 로그_쿼리_본문은_json으로_풀고_제외_사용자_uuid를_기호로_바꾼다() {
        let body = r#"{"logGroupName":"g","queryString":"filter userId not in [\"01990000-0000-7000-8000-0000000000e1\"]"}"#;
        let recs = vec![
            json!({"service": "cloudwatch_logs", "detail": "StartQuery", "method": "POST", "path": "/",
            "query": "", "headers": {"content-type": "application/x-amz-json-1.1"}, "body": body}),
        ];
        let out = normalize(&recs, &mut Normalizer::new(CLOCK));
        assert_eq!(out[0]["detail"], "StartQuery");
        assert_eq!(out[0]["body"]["logGroupName"], "g");
        let q = out[0]["body"]["queryString"].as_str().unwrap();
        assert!(q.contains("$uuid") && !q.contains("0000-7000"), "{q}");
    }
}
