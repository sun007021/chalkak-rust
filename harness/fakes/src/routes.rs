//! 요청 라우팅, 응답 지정, 기록. 네트워크와 무관한 순수 상태 기계라 단위 시험이 쉽다.
//!
//! | 경로 | 역할 |
//! | --- | --- |
//! | `GET /oidc/apple/keys`, `/oidc/google/certs`, `/oidc/kakao/jwks.json` | 같은 테스트 JWKS (제공자 구분은 기록의 service) |
//! | `POST /apple/auth/token`, `/apple/auth/revoke` | 지정한 응답 |
//! | `POST /slack/...` | Slack Incoming Webhook. 본문 기록 |
//! | `HEAD /<key>` 또는 `HEAD /<bucket>/<key>` | S3 HeadObject. 키별 크기·Content-Type·404 지정 |
//! | `POST /` + `X-Amz-Target: AmazonSQS.*` | SQS JSON 프로토콜(SendMessage 저장, Receive·Delete·ChangeMessageVisibility). `sqs.rs` 참고 |
//! | `POST /oauth2/token` | Google OAuth 토큰 교환(서비스 계정 JWT → 액세스 토큰). 기본은 성공, `oauth` 규칙으로 바꾼다 |
//! | `POST /v1/projects/<프로젝트>/messages:send` | FCM HTTP v1 발송. 기본은 성공, `fcm` 규칙으로 바꾼다 |
//! | `POST /__admin/sqs/messages` | 시나리오가 큐에 미리 넣는 메시지 |
//! | `POST /` + `X-Amz-Target: Logs_20140328.*` | CloudWatch Logs Insights(StartQuery·GetQueryResults·StopQuery). 지표 이름별 결과 행 지정 |
//! | `PUT /__admin/responses`, `GET /__admin/requests`, `POST /__admin/reset` | 관리 |

use crate::http::{Request, Response};
use crate::rules::{self, Rule};
use crate::sqs::Sqs;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const JWKS: &str = include_str!("../../keys/jwks.json");

#[derive(Debug, Clone)]
struct Canned {
    status: u16,
    content_type: String,
    body: String,
}

#[derive(Debug, Clone)]
struct S3Object {
    status: u16,
    size: usize,
    content_type: String,
}

/// CloudWatch Logs Insights 응답 지정. `rows`는 쿼리의 `stats ... as <지표>` 이름 → 결과 행의 필드(값은 문자열).
/// 지정하지 않은 지표는 결과 행이 없다(일치하는 로그가 없는 것과 같다).
#[derive(Debug, Clone)]
struct LogsConfig {
    /// `GetQueryResults`가 돌려줄 상태. 기본 `Complete`.
    status: String,
    rows: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Default)]
pub struct Fakes {
    s3_bucket: String,
    apple_token: Option<Canned>,
    apple_revoke: Option<Canned>,
    slack: Option<Canned>,
    s3: BTreeMap<String, S3Object>,
    sqs: Sqs,
    /// OAuth 토큰 교환 규칙(`oauth`). 규칙이 없으면 성공 응답이다.
    oauth: Vec<Rule>,
    /// FCM 발송 규칙(`fcm`). 규칙이 없으면 성공 응답이다.
    fcm: Vec<Rule>,
    fcm_sent: u64,
    logs: Option<LogsConfig>,
    /// `StartQuery`가 받은 쿼리 문자열. 인덱스 + 1이 쿼리 ID(`fake-query-N`).
    log_queries: Vec<String>,
    records: Vec<Value>,
}

impl Fakes {
    pub fn new(s3_bucket: &str) -> Self {
        Fakes {
            s3_bucket: s3_bucket.into(),
            ..Fakes::default()
        }
    }

    pub fn handle(&mut self, req: &Request) -> Response {
        if let Some(rest) = req.path.strip_prefix("/__admin/") {
            return self.admin(rest, req);
        }
        if let Some(op) = Sqs::operation(req) {
            let mut res = self.sqs.handle(&op, req);
            if op == "ReceiveMessage" && self.sqs.last_receive_empty() {
                // 빈 응답은 기록하지 않고 롱 폴링처럼 잠시 기다린다. 앱은 쉬지 않고 다시 받으므로 횟수가 대상마다 달라
                // 비교에 넣을 수 없다. 메시지를 돌려준 수신만 기록한다.
                res.long_poll_ms = Sqs::long_poll_wait(req).as_millis() as u64;
                return res;
            }
            self.record("sqs", Some(op), req);
            return res;
        }
        let (service, detail, res) = self.route(req);
        self.record(&service, detail, req);
        res
    }

    /// 롱 폴링 대기 중 다시 확인한다. 메시지가 있으면 기록하고 응답을, 없으면 `None`을 돌려준다.
    pub fn poll_receive(&mut self, req: &Request) -> Option<Response> {
        let res = self.sqs.handle("ReceiveMessage", req);
        if self.sqs.last_receive_empty() {
            return None;
        }
        self.record("sqs", Some("ReceiveMessage".into()), req);
        Some(res)
    }

    fn route(&mut self, req: &Request) -> (String, Option<String>, Response) {
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/oauth2/token") => {
                let body = String::from_utf8_lossy(&req.body).into_owned();
                let res = rules::respond(&self.oauth, "", &body).unwrap_or_else(|| {
                    Response::json(
                        200,
                        r#"{"access_token":"fake-access-token","expires_in":3600,"token_type":"Bearer"}"#,
                    )
                });
                ("oauth_token".into(), None, res)
            }
            ("POST", p) if p.starts_with("/v1/projects/") && p.ends_with("/messages:send") => {
                let body = String::from_utf8_lossy(&req.body).into_owned();
                let project = p
                    .trim_start_matches("/v1/projects/")
                    .trim_end_matches("/messages:send");
                self.fcm_sent += 1;
                let res = rules::respond(&self.fcm, "", &body).unwrap_or_else(|| {
                    Response::json(
                        200,
                        &json!({"name": format!("projects/{project}/messages/fake-{}", self.fcm_sent)})
                            .to_string(),
                    )
                });
                ("fcm_send".into(), None, res)
            }
            ("GET", "/oidc/apple/keys") => ("oidc_jwks:apple".into(), None, jwks()),
            ("GET", "/oidc/google/certs") => ("oidc_jwks:google".into(), None, jwks()),
            ("GET", "/oidc/kakao/jwks.json") => ("oidc_jwks:kakao".into(), None, jwks()),
            ("POST", "/apple/auth/token") => (
                "apple_token".into(),
                None,
                canned(self.apple_token.as_ref()),
            ),
            ("POST", "/apple/auth/revoke") => (
                "apple_revoke".into(),
                None,
                canned(self.apple_revoke.as_ref()),
            ),
            ("POST", p) if p.starts_with("/slack/") => {
                let res = match &self.slack {
                    Some(c) => canned(Some(c)),
                    None => Response::new(200, "text/plain", "ok"),
                };
                ("slack".into(), None, res)
            }
            ("POST", "/")
                if req
                    .header("x-amz-target")
                    .is_some_and(|t| t.starts_with("Logs_20140328.")) =>
            {
                let action = req
                    .header("x-amz-target")
                    .and_then(|t| t.strip_prefix("Logs_20140328."))
                    .unwrap_or_default()
                    .to_string();
                let res = self.logs_action(&action, &req.body);
                ("cloudwatch_logs".into(), Some(action), res)
            }
            ("HEAD", _) => {
                let (bucket, key) = self.split_s3(req);
                let res = self.head_object(&key);
                ("s3_head".into(), Some(format!("{bucket}/{key}")), res)
            }
            _ => (
                "unmatched".into(),
                None,
                Response::json(404, r#"{"error":"fakes_no_route"}"#),
            ),
        }
    }

    /// CloudWatch Logs Insights. 쿼리는 즉시 끝나고, 결과 행은 쿼리의 지표 이름(`as <지표>`)으로 고른다.
    fn logs_action(&mut self, action: &str, body: &[u8]) -> Response {
        let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        match action {
            "StartQuery" => {
                self.log_queries
                    .push(json["queryString"].as_str().unwrap_or_default().to_string());
                let id = format!("fake-query-{}", self.log_queries.len());
                logs_json(&json!({ "queryId": id }))
            }
            "GetQueryResults" => {
                let query = json["queryId"]
                    .as_str()
                    .and_then(|id| id.strip_prefix("fake-query-"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .and_then(|n| self.log_queries.get(n.wrapping_sub(1)))
                    .cloned()
                    .unwrap_or_default();
                let config = self.logs.clone().unwrap_or(LogsConfig {
                    status: "Complete".into(),
                    rows: BTreeMap::new(),
                });
                // 쿼리 하나는 지표 하나(요약 쿼리는 requests)만 센다. 쿼리 끝의 `as <이름>` 중 지정한 행이 있는 첫 이름을 쓴다.
                let row = config
                    .rows
                    .iter()
                    .find(|(metric, _)| query.contains(&format!(" as {metric}")))
                    .map(|(_, fields)| fields.clone());
                let results: Vec<Value> = row
                    .map(|fields| {
                        vec![Value::Array(
                            fields
                                .into_iter()
                                .map(|(f, v)| json!({ "field": f, "value": v }))
                                .collect(),
                        )]
                    })
                    .unwrap_or_default();
                logs_json(&json!({
                    "results": results,
                    "statistics": {"recordsMatched": 0.0, "recordsScanned": 0.0, "bytesScanned": 0.0},
                    "status": config.status,
                }))
            }
            "StopQuery" => logs_json(&json!({ "success": true })),
            _ => Response::new(
                400,
                "application/x-amz-json-1.1",
                r#"{"__type":"UnknownOperationException"}"#,
            ),
        }
    }

    /// 가상 호스트 방식(`Host: <bucket>.<호스트>`)과 경로 방식(`/<bucket>/<key>`)을 모두 받는다.
    fn split_s3(&self, req: &Request) -> (String, String) {
        let path = req.path.trim_start_matches('/');
        let host_bucket = req
            .header("host")
            .and_then(|h| h.split_once('.'))
            .map(|(first, _)| first)
            .filter(|first| *first == self.s3_bucket);
        if let Some(b) = host_bucket {
            return (b.to_string(), path.to_string());
        }
        match path.split_once('/') {
            Some((b, key)) if b == self.s3_bucket => (b.to_string(), key.to_string()),
            _ => (String::new(), path.to_string()),
        }
    }

    fn head_object(&self, key: &str) -> Response {
        match self.s3.get(key) {
            Some(o) if o.status == 200 => {
                let mut res = Response::new(200, &o.content_type, Vec::new());
                res.content_length = Some(o.size);
                res.headers.push(("ETag".into(), "\"harness\"".into()));
                res.headers.push((
                    "Last-Modified".into(),
                    "Mon, 12 Oct 2026 00:00:00 GMT".into(),
                ));
                res
            }
            Some(o) => {
                let mut res = Response::new(o.status, "application/xml", Vec::new());
                res.content_length = Some(0);
                res
            }
            None => {
                let mut res = Response::new(404, "application/xml", Vec::new());
                res.content_length = Some(0);
                res
            }
        }
    }

    fn record(&mut self, service: &str, detail: Option<String>, req: &Request) {
        let headers: Map<String, Value> = req
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let mut rec = json!({
            "seq": self.records.len() + 1,
            "service": service,
            "method": req.method,
            "path": req.path,
            "query": req.query,
            "headers": headers,
            "body": String::from_utf8_lossy(&req.body),
        });
        if let Some(d) = detail {
            rec["detail"] = Value::String(d);
        }
        self.records.push(rec);
    }

    fn admin(&mut self, rest: &str, req: &Request) -> Response {
        match (req.method.as_str(), rest) {
            ("GET", "health") => Response::json(200, r#"{"status":"UP"}"#),
            ("PUT", "responses") => match self.set_responses(&req.body) {
                Ok(()) => Response::json(200, r#"{"ok":true}"#),
                Err(e) => Response::json(400, &json!({ "error": e }).to_string()),
            },
            ("POST", "sqs/messages") => match self.seed_sqs(&req.body) {
                Ok(()) => Response::json(200, r#"{"ok":true}"#),
                Err(e) => Response::json(400, &json!({ "error": e }).to_string()),
            },
            ("GET", "requests") => {
                Response::json(200, &Value::Array(self.records.clone()).to_string())
            }
            ("POST", "reset") => {
                *self = Fakes::new(&self.s3_bucket.clone());
                Response::json(200, r#"{"ok":true}"#)
            }
            _ => Response::json(404, r#"{"error":"fakes_no_admin_route"}"#),
        }
    }

    /// `{"messages":["본문", ...]}`을 큐에 넣는다.
    fn seed_sqs(&mut self, body: &[u8]) -> Result<(), String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| format!("JSON 오류: {e}"))?;
        let messages = v
            .get("messages")
            .and_then(Value::as_array)
            .ok_or("messages 배열이 필요합니다")?;
        let bodies: Vec<&str> = messages
            .iter()
            .map(|m| m.as_str().ok_or("messages 항목은 문자열이어야 합니다"))
            .collect::<Result<_, _>>()?;
        for body in bodies {
            self.sqs.seed(body);
        }
        Ok(())
    }

    /// 지정한 응답을 통째로 바꾼다(이전 지정은 사라진다). 기록은 건드리지 않는다.
    fn set_responses(&mut self, body: &[u8]) -> Result<(), String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| format!("JSON 오류: {e}"))?;
        let obj = v.as_object().ok_or("본문은 JSON 객체여야 합니다")?;
        let mut apple_token = None;
        let mut apple_revoke = None;
        let mut slack = None;
        let mut s3 = BTreeMap::new();
        let mut sqs_rules = Vec::new();
        let mut oauth = Vec::new();
        let mut fcm = Vec::new();
        let mut logs = None;
        for (name, value) in obj {
            match name.as_str() {
                "apple_token" => apple_token = Some(parse_canned(name, value)?),
                "apple_revoke" => apple_revoke = Some(parse_canned(name, value)?),
                "slack" => slack = Some(parse_canned(name, value)?),
                "logs" => logs = Some(parse_logs(value)?),
                "s3" => {
                    let objects = value.as_object().ok_or("s3는 객체여야 합니다")?;
                    for (key, spec) in objects {
                        s3.insert(key.clone(), parse_s3(key, spec)?);
                    }
                }
                "sqs" => sqs_rules = Sqs::parse_rules(value)?,
                "oauth" => oauth = rules::parse_rules("oauth", "application/json", value)?,
                "fcm" => fcm = rules::parse_rules("fcm", "application/json", value)?,
                other => return Err(format!("알 수 없는 항목: {other}")),
            }
        }
        self.apple_token = apple_token;
        self.apple_revoke = apple_revoke;
        self.slack = slack;
        self.s3 = s3;
        self.sqs.set_rules(sqs_rules);
        self.oauth = oauth;
        self.fcm = fcm;
        self.logs = logs;
        Ok(())
    }
}

fn logs_json(v: &Value) -> Response {
    Response::new(200, "application/x-amz-json-1.1", v.to_string())
}

fn parse_logs(v: &Value) -> Result<LogsConfig, String> {
    if !v.is_object() {
        return Err("logs는 객체여야 합니다".into());
    }
    let mut rows = BTreeMap::new();
    if let Some(r) = v.get("rows") {
        for (metric, fields) in r.as_object().ok_or("logs.rows는 객체여야 합니다")? {
            let fields = fields
                .as_object()
                .ok_or_else(|| format!("logs.rows.{metric}는 객체여야 합니다"))?;
            rows.insert(
                metric.clone(),
                fields
                    .iter()
                    .map(|(k, val)| (k.clone(), val.as_str().unwrap_or_default().to_string()))
                    .collect(),
            );
        }
    }
    Ok(LogsConfig {
        status: v
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("Complete")
            .to_string(),
        rows,
    })
}

fn jwks() -> Response {
    Response::new(200, "application/json", JWKS)
}

fn canned(c: Option<&Canned>) -> Response {
    match c {
        Some(c) => Response::new(c.status, &c.content_type, c.body.clone()),
        None => Response::json(500, r#"{"error":"fakes_not_configured"}"#),
    }
}

fn parse_canned(name: &str, v: &Value) -> Result<Canned, String> {
    let status = v
        .get("status")
        .and_then(Value::as_u64)
        .filter(|s| (100..600).contains(s))
        .ok_or_else(|| format!("{name}.status가 필요합니다"))?;
    Ok(Canned {
        status: status as u16,
        content_type: v
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("application/json")
            .to_string(),
        body: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_s3(key: &str, v: &Value) -> Result<S3Object, String> {
    let status = v.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
    let size = v.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
    if status == 200 && v.get("content_type").and_then(Value::as_str).is_none() {
        return Err(format!("s3.{key}.content_type이 필요합니다"));
    }
    Ok(S3Object {
        status,
        size,
        content_type: v
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: method.into(),
            path: path.into(),
            query: query.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
            close: false,
        }
    }

    fn records(f: &mut Fakes) -> Vec<Value> {
        let res = f.handle(&req("GET", "/__admin/requests", &[], ""));
        serde_json::from_slice::<Vec<Value>>(&res.body).unwrap()
    }

    fn configure(f: &mut Fakes, config: &str) {
        let res = f.handle(&req("PUT", "/__admin/responses", &[], config));
        assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    }

    #[test]
    fn 제공자마다_다른_경로로_같은_jwks를_준다() {
        let mut f = Fakes::new("test-bucket");
        for (path, service) in [
            ("/oidc/apple/keys", "oidc_jwks:apple"),
            ("/oidc/google/certs", "oidc_jwks:google"),
            ("/oidc/kakao/jwks.json", "oidc_jwks:kakao"),
        ] {
            let res = f.handle(&req("GET", path, &[("accept", "application/json")], ""));
            assert_eq!(res.status, 200);
            let v: Value = serde_json::from_slice(&res.body).unwrap();
            assert_eq!(v["keys"][0]["kid"], "harness-test-key");
            assert_eq!(v["keys"][0]["alg"], "RS256");
            assert_eq!(records(&mut f).last().unwrap()["service"], service);
        }
    }

    #[test]
    fn 지정하지_않은_apple_응답은_500이다() {
        let mut f = Fakes::new("b");
        let res = f.handle(&req("POST", "/apple/auth/token", &[], "code=x"));
        assert_eq!(res.status, 500);
        assert!(String::from_utf8_lossy(&res.body).contains("fakes_not_configured"));
    }

    #[test]
    fn apple_응답을_지정하고_본문을_기록한다() {
        let mut f = Fakes::new("b");
        configure(
            &mut f,
            r#"{"apple_token":{"status":400,"body":"{\"error\":\"invalid_grant\"}"},
                "apple_revoke":{"status":200}}"#,
        );
        let res = f.handle(&req(
            "POST",
            "/apple/auth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            "grant_type=authorization_code&code=abc",
        ));
        assert_eq!(res.status, 400);
        assert_eq!(res.body, br#"{"error":"invalid_grant"}"#);
        assert_eq!(
            f.handle(&req("POST", "/apple/auth/revoke", &[], "t=1"))
                .status,
            200
        );

        let rs = records(&mut f);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0]["service"], "apple_token");
        assert_eq!(rs[0]["body"], "grant_type=authorization_code&code=abc");
        assert_eq!(
            rs[0]["headers"]["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(rs[0]["seq"], 1);
        assert_eq!(rs[1]["service"], "apple_revoke");
        assert_eq!(rs[1]["seq"], 2);
    }

    #[test]
    fn slack은_기본으로_ok를_주고_본문을_기록한다() {
        let mut f = Fakes::new("b");
        let res = f.handle(&req(
            "POST",
            "/slack/services/T/B/X",
            &[],
            r#"{"text":"hi"}"#,
        ));
        assert_eq!((res.status, res.body.as_slice()), (200, &b"ok"[..]));
        let rs = records(&mut f);
        assert_eq!(rs[0]["service"], "slack");
        assert_eq!(rs[0]["path"], "/slack/services/T/B/X");
        assert_eq!(rs[0]["body"], r#"{"text":"hi"}"#);

        configure(
            &mut f,
            r#"{"slack":{"status":500,"content_type":"text/plain","body":"no"}}"#,
        );
        assert_eq!(f.handle(&req("POST", "/slack/x", &[], "{}")).status, 500);
    }

    #[test]
    fn s3_head는_가상호스트와_경로_방식_모두_키를_찾는다() {
        let mut f = Fakes::new("test-bucket");
        configure(
            &mut f,
            r#"{"s3":{"p/a.webp":{"size":123,"content_type":"image/webp"},"p/gone":{"status":403}}}"#,
        );
        let res = f.handle(&req(
            "HEAD",
            "/p/a.webp",
            &[("host", "test-bucket.fakes:9000")],
            "",
        ));
        assert_eq!(res.status, 200);
        assert_eq!(res.content_length, Some(123));
        assert!(
            res.headers
                .contains(&("Content-Type".into(), "image/webp".into()))
        );

        let res = f.handle(&req(
            "HEAD",
            "/test-bucket/p/a.webp",
            &[("host", "fakes:9000")],
            "",
        ));
        assert_eq!(res.status, 200);

        assert_eq!(
            f.handle(&req(
                "HEAD",
                "/p/gone",
                &[("host", "test-bucket.fakes")],
                ""
            ))
            .status,
            403
        );
        assert_eq!(
            f.handle(&req(
                "HEAD",
                "/p/none",
                &[("host", "test-bucket.fakes")],
                ""
            ))
            .status,
            404
        );
        let rs = records(&mut f);
        assert_eq!(rs[0]["detail"], "test-bucket/p/a.webp");
        assert_eq!(rs[1]["detail"], "test-bucket/p/a.webp");
        assert_eq!(rs[3]["detail"], "test-bucket/p/none");
    }

    #[test]
    fn 초기화하면_지정과_기록이_모두_사라진다() {
        let mut f = Fakes::new("test-bucket");
        configure(&mut f, r#"{"apple_token":{"status":200,"body":"x"}}"#);
        f.handle(&req("POST", "/apple/auth/token", &[], ""));
        assert_eq!(
            f.handle(&req("POST", "/__admin/reset", &[], "")).status,
            200
        );
        assert!(records(&mut f).is_empty());
        assert_eq!(
            f.handle(&req("POST", "/apple/auth/token", &[], "")).status,
            500
        );
    }

    #[test]
    fn 잘못된_지정은_400이고_기존_지정을_바꾸지_않는다() {
        let mut f = Fakes::new("b");
        configure(&mut f, r#"{"apple_token":{"status":200}}"#);
        for bad in [
            "not json",
            "[]",
            r#"{"nope":{}}"#,
            r#"{"apple_token":{}}"#,
            r#"{"s3":{"k":{"size":1}}}"#,
        ] {
            let res = f.handle(&req("PUT", "/__admin/responses", &[], bad));
            assert_eq!(res.status, 400, "{bad}");
        }
        assert_eq!(
            f.handle(&req("POST", "/apple/auth/token", &[], "")).status,
            200
        );
    }

    #[test]
    fn 모르는_경로는_404로_기록한다() {
        let mut f = Fakes::new("b");
        assert_eq!(f.handle(&req("GET", "/nope", &[], "")).status, 404);
        assert_eq!(records(&mut f)[0]["service"], "unmatched");
    }

    #[test]
    fn sqs_요청은_작업_이름과_본문을_기록하고_지정을_따른다() {
        let mut f = Fakes::new("b");
        configure(
            &mut f,
            r#"{"sqs":[{"contains":"fail","status":500,"body":"{\"__type\":\"InternalError\"}"}]}"#,
        );
        let send = |body: &str| {
            req(
                "POST",
                "/",
                &[("x-amz-target", "AmazonSQS.SendMessage")],
                body,
            )
        };
        assert_eq!(f.handle(&send(r#"{"MessageBody":"ok"}"#)).status, 200);
        assert_eq!(f.handle(&send(r#"{"MessageBody":"fail"}"#)).status, 500);
        let rs = records(&mut f);
        assert_eq!(rs[0]["service"], "sqs");
        assert_eq!(rs[0]["detail"], "SendMessage");
        assert_eq!(rs[1]["body"], r#"{"MessageBody":"fail"}"#);
    }

    #[test]
    fn 관리_요청은_기록하지_않는다() {
        let mut f = Fakes::new("b");
        f.handle(&req("GET", "/__admin/health", &[], ""));
        assert!(records(&mut f).is_empty());
    }

    const LOGS_TARGET: (&str, &str) = ("x-amz-target", "Logs_20140328.");

    fn logs_call(f: &mut Fakes, action: &str, body: &str) -> Value {
        let target = format!("{}{action}", LOGS_TARGET.1);
        let res = f.handle(&req("POST", "/", &[(LOGS_TARGET.0, &target)], body));
        assert_eq!(res.status, 200);
        serde_json::from_slice(&res.body).unwrap()
    }

    #[test]
    fn 로그_쿼리는_순번_id를_주고_지표_이름으로_행을_고른다() {
        let mut f = Fakes::new("b");
        configure(
            &mut f,
            r#"{"logs":{"rows":{"visitors":{"visitors":"7"},"requests":{"requests":"12","latencyAvg":"1.5"}}}}"#,
        );
        let q1 = logs_call(
            &mut f,
            "StartQuery",
            r#"{"queryString":"filter x\n| stats count_distinct(userId) as visitors"}"#,
        );
        assert_eq!(q1["queryId"], "fake-query-1");
        let q2 = logs_call(
            &mut f,
            "StartQuery",
            r#"{"queryString":"filter x\n| stats count(*) as requests, avg(d) as latencyAvg"}"#,
        );
        assert_eq!(q2["queryId"], "fake-query-2");
        let r1 = logs_call(&mut f, "GetQueryResults", r#"{"queryId":"fake-query-1"}"#);
        assert_eq!(r1["status"], "Complete");
        assert_eq!(r1["results"][0][0], json!({"field":"visitors","value":"7"}));
        let r2 = logs_call(&mut f, "GetQueryResults", r#"{"queryId":"fake-query-2"}"#);
        assert_eq!(r2["results"][0].as_array().unwrap().len(), 2);
        // 행을 지정하지 않은 지표는 결과 행이 없다.
        logs_call(
            &mut f,
            "StartQuery",
            r#"{"queryString":"... as server5xx"}"#,
        );
        let r3 = logs_call(&mut f, "GetQueryResults", r#"{"queryId":"fake-query-3"}"#);
        assert_eq!(r3["results"], json!([]));
        let recs = records(&mut f);
        assert_eq!(recs.len(), 6);
        assert_eq!(recs[0]["service"], "cloudwatch_logs");
        assert_eq!(recs[0]["detail"], "StartQuery");
    }

    #[test]
    fn 로그_쿼리_상태를_지정할_수_있고_지정이_없으면_완료다() {
        let mut f = Fakes::new("b");
        logs_call(&mut f, "StartQuery", r#"{"queryString":"as visitors"}"#);
        let r = logs_call(&mut f, "GetQueryResults", r#"{"queryId":"fake-query-1"}"#);
        assert_eq!(r["status"], "Complete");
        configure(&mut f, r#"{"logs":{"status":"Failed"}}"#);
        let r = logs_call(&mut f, "GetQueryResults", r#"{"queryId":"fake-query-1"}"#);
        assert_eq!(r["status"], "Failed");
        // 쿼리 번호는 설정을 바꿔도 이어진다. 중단 요청은 성공으로 답한다.
        assert_eq!(
            logs_call(&mut f, "StopQuery", r#"{"queryId":"fake-query-1"}"#)["success"],
            true
        );
    }

    #[test]
    fn 로그_지정의_형식이_틀리면_거절한다() {
        let mut f = Fakes::new("b");
        for bad in [
            r#"{"logs":[]}"#,
            r#"{"logs":{"rows":[]}}"#,
            r#"{"logs":{"rows":{"v":1}}}"#,
        ] {
            let res = f.handle(&req("PUT", "/__admin/responses", &[], bad));
            assert_eq!(res.status, 400, "{bad}");
        }
    }

    #[test]
    fn 모르는_로그_동작은_400이다() {
        let mut f = Fakes::new("b");
        let res = f.handle(&req(
            "POST",
            "/",
            &[(LOGS_TARGET.0, "Logs_20140328.Nope")],
            "{}",
        ));
        assert_eq!(res.status, 400);
    }

    #[test]
    fn oauth_토큰_교환은_기본_성공이고_규칙으로_바꾼다() {
        let mut f = Fakes::new("test-bucket");
        let form =
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion=a.b.c";
        let res = f.handle(&req("POST", "/oauth2/token", &[], form));
        assert_eq!(res.status, 200);
        let v: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(v["access_token"], "fake-access-token");
        assert_eq!(v["token_type"], "Bearer");

        configure(
            &mut f,
            r#"{"oauth":[{"contains":"assertion=bad","status":400,"body":"{\"error\":\"invalid_grant\"}"}]}"#,
        );
        let res = f.handle(&req("POST", "/oauth2/token", &[], "assertion=bad.x.y"));
        assert_eq!(res.status, 400);
        let res = f.handle(&req("POST", "/oauth2/token", &[], "assertion=good"));
        assert_eq!(res.status, 200);
        let rs = records(&mut f);
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0]["service"], "oauth_token");
        assert_eq!(rs[0]["body"], form);
    }

    #[test]
    fn fcm_발송은_프로젝트별_경로로_받고_기본_성공_응답에_번호를_붙인다() {
        let mut f = Fakes::new("test-bucket");
        let path = "/v1/projects/chalkak-test/messages:send";
        let body = r#"{"message":{"token":"t"}}"#;
        let res = f.handle(&req("POST", path, &[("authorization", "Bearer x")], body));
        assert_eq!(res.status, 200);
        let v: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(v["name"], "projects/chalkak-test/messages/fake-1");
        let v: Value =
            serde_json::from_slice(&f.handle(&req("POST", path, &[], body)).body).unwrap();
        assert_eq!(v["name"], "projects/chalkak-test/messages/fake-2");

        configure(
            &mut f,
            r#"{"fcm":[{"contains":"bad-token","status":404,"headers":{"retry-after":"7"},
                "body":"{\"error\":{\"code\":404,\"status\":\"NOT_FOUND\"}}"}]}"#,
        );
        let res = f.handle(&req(
            "POST",
            path,
            &[],
            r#"{"message":{"token":"bad-token"}}"#,
        ));
        assert_eq!(res.status, 404);
        assert!(
            res.headers
                .iter()
                .any(|(k, v)| k == "retry-after" && v == "7")
        );
        let rs = records(&mut f);
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0]["service"], "fcm_send");
        assert_eq!(rs[0]["path"], path);
        assert_eq!(rs[0]["headers"]["authorization"], "Bearer x");
    }

    #[test]
    fn 잘못된_oauth_fcm_규칙은_거절한다() {
        let mut f = Fakes::new("test-bucket");
        for config in [r#"{"oauth":{}}"#, r#"{"fcm":[{}]}"#] {
            let res = f.handle(&req("PUT", "/__admin/responses", &[], config));
            assert_eq!(res.status, 400, "{config}");
        }
    }

    #[test]
    fn 빈_수신은_기록하지_않고_기다리며_넣어_둔_메시지를_받으면_기록한다() {
        let mut f = Fakes::new("test-bucket");
        let receive = req(
            "POST",
            "/",
            &[("x-amz-target", "AmazonSQS.ReceiveMessage")],
            r#"{"MaxNumberOfMessages":1,"WaitTimeSeconds":20}"#,
        );
        let res = f.handle(&receive);
        assert_eq!(res.status, 200);
        assert_eq!(res.long_poll_ms, 1000);
        assert!(records(&mut f).is_empty());
        assert!(f.poll_receive(&receive).is_none());

        let res = f.handle(&req(
            "POST",
            "/__admin/sqs/messages",
            &[],
            r#"{"messages":["{\"a\":1}","invalid"]}"#,
        ));
        assert_eq!(res.status, 200);
        let res = f.poll_receive(&receive).expect("넣어 둔 메시지");
        let v: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(v["Messages"][0]["Body"], "{\"a\":1}");
        assert_eq!(
            v["Messages"][0]["Attributes"]["ApproximateReceiveCount"],
            "1"
        );
        let rs = records(&mut f);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0]["service"], "sqs");
        assert_eq!(rs[0]["detail"], "ReceiveMessage");

        // 메시지를 바로 돌려주는 수신은 기다리지 않는다.
        let res = f.handle(&receive);
        assert_eq!(res.long_poll_ms, 0);
        assert_eq!(records(&mut f).len(), 2);
    }

    #[test]
    fn 시나리오가_넣는_메시지_형식이_틀리면_거절한다() {
        let mut f = Fakes::new("test-bucket");
        for body in [r#"{}"#, r#"{"messages":[1]}"#, "not json"] {
            let res = f.handle(&req("POST", "/__admin/sqs/messages", &[], body));
            assert_eq!(res.status, 400, "{body}");
        }
    }
}
