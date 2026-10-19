//! 요청 라우팅, 응답 지정, 기록. 네트워크와 무관한 순수 상태 기계라 단위 시험이 쉽다.
//!
//! | 경로 | 역할 |
//! | --- | --- |
//! | `GET /oidc/apple/keys`, `/oidc/google/certs`, `/oidc/kakao/jwks.json` | 같은 테스트 JWKS (제공자 구분은 기록의 service) |
//! | `POST /apple/auth/token`, `/apple/auth/revoke` | 지정한 응답 |
//! | `POST /slack/...` | Slack Incoming Webhook. 본문 기록 |
//! | `HEAD /<key>` 또는 `HEAD /<bucket>/<key>` | S3 HeadObject. 키별 크기·Content-Type·404 지정 |
//! | `POST /` + `X-Amz-Target: AmazonSQS.*` | SQS JSON 프로토콜(SendMessage 저장, Receive·Delete·ChangeMessageVisibility). `sqs.rs` 참고 |
//! | `PUT /__admin/responses`, `GET /__admin/requests`, `POST /__admin/reset` | 관리 |

use crate::http::{Request, Response};
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

#[derive(Debug, Default)]
pub struct Fakes {
    s3_bucket: String,
    apple_token: Option<Canned>,
    apple_revoke: Option<Canned>,
    slack: Option<Canned>,
    s3: BTreeMap<String, S3Object>,
    sqs: Sqs,
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
            let res = self.sqs.handle(&op, req);
            self.record("sqs", Some(op), req);
            return res;
        }
        let (service, detail, res) = self.route(req);
        self.record(&service, detail, req);
        res
    }

    fn route(&self, req: &Request) -> (String, Option<String>, Response) {
        match (req.method.as_str(), req.path.as_str()) {
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

    /// 지정한 응답을 통째로 바꾼다(이전 지정은 사라진다). 기록은 건드리지 않는다.
    fn set_responses(&mut self, body: &[u8]) -> Result<(), String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| format!("JSON 오류: {e}"))?;
        let obj = v.as_object().ok_or("본문은 JSON 객체여야 합니다")?;
        let mut apple_token = None;
        let mut apple_revoke = None;
        let mut slack = None;
        let mut s3 = BTreeMap::new();
        let mut sqs_rules = Vec::new();
        for (name, value) in obj {
            match name.as_str() {
                "apple_token" => apple_token = Some(parse_canned(name, value)?),
                "apple_revoke" => apple_revoke = Some(parse_canned(name, value)?),
                "slack" => slack = Some(parse_canned(name, value)?),
                "s3" => {
                    let objects = value.as_object().ok_or("s3는 객체여야 합니다")?;
                    for (key, spec) in objects {
                        s3.insert(key.clone(), parse_s3(key, spec)?);
                    }
                }
                "sqs" => sqs_rules = Sqs::parse_rules(value)?,
                other => return Err(format!("알 수 없는 항목: {other}")),
            }
        }
        self.apple_token = apple_token;
        self.apple_revoke = apple_revoke;
        self.slack = slack;
        self.s3 = s3;
        self.sqs.set_rules(sqs_rules);
        Ok(())
    }
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
}
