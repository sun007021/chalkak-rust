//! 시나리오 하나를 대상 하나에 실행하고, 정규화된 결과(Outcome)를 만든다.

use crate::callback;
use crate::http::{self, Request};
use crate::normalize::Normalizer;
use crate::scenario::FakesSpec;
use crate::scenario::{Auth, Scenario};
use crate::target::{self, Target};
use crate::{fakes, idtoken};
use crate::{time, token};
use serde_json::{Value, json};
use std::collections::HashMap;

/// 비교에서 뺄 응답 헤더. 전송 계층의 세부라 클라이언트가 보는 의미가 같다.
const IGNORED_HEADERS: &[&str] = &[
    "date",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
];

pub const DEFAULT_CLOCK: &str = "2026-10-12T09:00:00+09:00";

/// 앱 `Date` 헤더와 시나리오 시각의 허용 차이(초). 단계 처리 시간과 초 경계를 덮는다.
const CLOCK_TOLERANCE_SECONDS: i64 = 10;

/// 앱이 시계 파일을 반영하지 못한 채 응답하면(부하로 libfaketime이 파일을 못 읽는 경우 등) 토큰 만료처럼
/// 보이는 거짓 차이가 생긴다. 동작 차이와 섞이지 않게 하네스 오류로 따로 알린다.
fn check_app_clock(
    target: &Target,
    step: &str,
    headers: &[(String, String)],
    expected: i64,
) -> Result<(), String> {
    let Some((_, date)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("date")) else {
        return Ok(());
    };
    match time::parse_http_date(date) {
        Some(at) if (at - expected).abs() <= CLOCK_TOLERANCE_SECONDS => Ok(()),
        _ => Err(format!(
            "[{}] {step}: {CLOCK_ERROR_MARK}(Date: {date}). 시계 파일이 반영되지 않았습니다",
            target.name
        )),
    }
}

/// 앱 시계 오류 메시지의 표지. `check_app_clock`이 만든다.
const CLOCK_ERROR_MARK: &str = "앱 시계가 시나리오 시각과 다릅니다";

/// 응답을 한 바이트도 받지 못하고 연결이 닫힌 경우의 오류 표지. `http::read_response`가 만든다.
const EMPTY_RESPONSE_MARK: &str = "잘못된 상태 줄: \"\"";

/// 일시적 하네스 오류(시계 오류, 응답 없이 닫힌 연결)가 나면 시나리오를 처음부터(DB 초기화부터) 다시 돌린다.
/// 시계 오류는 부하로 libfaketime이 시계 파일을 놓친 것이고, 빈 응답은 시각을 뒤로 되돌린 직후 JVM이
/// 연결을 끊는 경우가 있었다(2026-10-12, 같은 단계에서 반복). 이미 틀린 상태가 쌓였을 수 있어 그 단계만
/// 다시 보내지 않고 시나리오 전체를 다시 한다. 세 번 모두 같으면 그 오류를 그대로 낸다(구현이 실제로
/// 응답 없이 연결을 닫는 경우를 가리지 않는다). 다른 오류는 바로 낸다.
pub fn run(target: &Target, scenario: &Scenario) -> Result<Value, String> {
    const ATTEMPTS: usize = 3;
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match run_once(target, scenario) {
            Err(e) if e.contains(CLOCK_ERROR_MARK) || e.contains(EMPTY_RESPONSE_MARK) => {
                eprintln!("경고: {e} (시나리오를 다시 실행합니다, {attempt}/{ATTEMPTS})");
                last = e;
            }
            other => return other,
        }
    }
    Err(last)
}

fn run_once(target: &Target, scenario: &Scenario) -> Result<Value, String> {
    let clock = time::parse(scenario.clock.as_deref().unwrap_or(DEFAULT_CLOCK))
        .ok_or("clock 형식이 올바르지 않습니다")?
        .epoch as i64;

    let mut db = target.db()?;
    target.set_clock(clock)?;
    target::reset_db(&mut db, &scenario.setup.sql)?;

    let mut norm = Normalizer::new(clock as f64);
    let mut vars: HashMap<String, String> = HashMap::new();

    // 외부 연동: 가짜 서버를 비우고, ID Token을 서명해 변수로 만든다. 응답 지정은 단계마다 현재 변수로 다시 채워 보낸다.
    let uses_fakes = scenario.fakes.is_some()
        || !scenario.id_tokens.is_empty()
        || scenario.steps.iter().any(|s| s.fakes.is_some());
    let fakes_url = target.fakes_url.as_deref();
    if uses_fakes && fakes_url.is_none() {
        return Err(format!(
            "[{}] {}: 외부 연동 시나리오인데 대상 설정에 fakes_url이 없습니다",
            target.name,
            scenario.path.display()
        ));
    }
    if let Some(url) = fakes_url {
        fakes::reset(url)?;
    }
    for spec in &scenario.id_tokens {
        vars.insert(spec.name.clone(), idtoken::sign(spec, clock)?);
    }
    let mut fakes_spec: FakesSpec = scenario.fakes.clone().unwrap_or_default();
    let mut app_offset = 0i64;
    let mut steps = Vec::new();

    for step in &scenario.steps {
        // 단계마다 시계를 다시 맞춘다. 구현마다 앞 단계 처리 시간이 달라도 각 단계는 같은 시각에서 출발한다.
        target.set_clock(clock + app_offset)?;
        if let Some(over) = &step.fakes {
            fakes_spec.merge(over);
        }
        if let (true, Some(url)) = (uses_fakes, fakes_url) {
            fakes::configure(url, &fakes_spec, &|s| subst(s, &vars))?;
        }
        let mut headers: Vec<(String, String)> = step
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), subst(v, &vars)))
            .collect();
        if let Some(auth) = &step.auth {
            headers.push((
                "Authorization".into(),
                authorization(target, auth, clock + app_offset, &vars)?,
            ));
        }
        let path = subst(&step.path, &vars);
        if let Some(spec) = &step.callback {
            let route = path.split('?').next().unwrap_or(&path);
            headers.extend(callback::headers(spec, clock + app_offset, route));
        }
        let req = Request {
            method: step.method.clone(),
            path,
            headers,
            body: step.body.as_ref().map(|b| subst(b, &vars).into_bytes()),
        };
        let res = http::send(&target.base_url, &req)
            .map_err(|e| format!("[{}] {}: {e}", target.name, step.name))?;

        check_app_clock(target, &step.name, &res.headers, clock + app_offset)?;

        let parsed: Option<Value> = serde_json::from_slice(&res.body).ok();
        for (var, path) in &step.capture {
            let v = parsed
                .as_ref()
                .and_then(|b| lookup(b, path))
                .ok_or_else(|| {
                    format!(
                        "[{}] {}: 응답에서 {path}를 찾을 수 없습니다",
                        target.name, step.name
                    )
                })?;
            vars.insert(
                var.clone(),
                match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
            );
        }

        let mut hs: Vec<(String, String)> = res
            .headers
            .iter()
            .filter(|(k, _)| !IGNORED_HEADERS.contains(&k.to_ascii_lowercase().as_str()))
            .map(|(k, v)| (k.to_ascii_lowercase(), norm.string(v)))
            .collect();
        hs.sort();
        let body = match parsed {
            Some(v) => norm.value(&v, &step.opaque, ""),
            None => Value::String(norm.string(&String::from_utf8_lossy(&res.body))),
        };
        steps.push(json!({
            "step": step.name,
            "status": res.status,
            "headers": hs.into_iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "body": body,
        }));

        app_offset += step.advance_seconds.unwrap_or(0);
    }

    // 가짜 서버가 받은 요청. 응답에서 본 UUID 기호를 이어 쓰도록 DB 덤프보다 먼저 정규화한다.
    let external_calls = fakes_url
        .map(|url| fakes::records(url).map(|r| fakes::normalize(&r, &mut norm)))
        .transpose()?;

    let db_state = if scenario.compare_db {
        db_snapshot(&mut db, &mut norm, &scenario.db_opaque)?
    } else {
        Value::Null
    };
    let mut outcome = json!({ "steps": steps, "db": db_state });
    // 외부 연동을 쓰지 않고 받은 요청도 없는 시나리오는 결과 형식이 이전과 같다(기존 golden 호환).
    if let Some(calls) =
        external_calls.filter(|c| uses_fakes || c.as_array().is_some_and(|a| !a.is_empty()))
    {
        outcome["fakes"] = calls;
    }
    Ok(outcome)
}

/// 응답에서 본 UUID 기호를 이어서 쓰고, 처음 보는 UUID는 `$uuid?`로 둔 뒤 정렬한다.
/// DB 행 순서와 처음 보는 UUID의 등장 순서는 구현마다 달라도 되기 때문이다.
fn db_snapshot(
    db: &mut postgres::Client,
    norm: &mut Normalizer,
    opaque: &[String],
) -> Result<Value, String> {
    norm.freeze();
    norm.db_mode();
    let mut tables = serde_json::Map::new();
    for (table, rows) in target::dump(db)? {
        let mut rows: Vec<String> = rows
            .iter()
            .map(|r| norm.value(r, opaque, "").to_string())
            .collect();
        rows.sort();
        if !rows.is_empty() {
            tables.insert(
                table,
                Value::Array(rows.into_iter().map(Value::String).collect()),
            );
        }
    }
    Ok(Value::Object(tables))
}

fn authorization(
    target: &Target,
    auth: &Auth,
    now: i64,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    if let Some(raw) = &auth.raw {
        return Ok(subst(raw, vars));
    }
    let exp = now + auth.expires_in.unwrap_or(900);
    let (subject, scope) = match (&auth.user, &auth.admin) {
        (Some(u), None) => (subst(u, vars), token::Scope::User),
        (None, Some(a)) => (subst(a, vars), token::Scope::Admin),
        _ => return Err("auth에는 user, admin, raw 중 하나만 지정합니다".into()),
    };
    let jti = format!("00000000-0000-4000-8000-{:012x}", now);
    Ok(format!(
        "Bearer {}",
        token::access_token(&target.access_token_secret, &subject, scope, now, exp, &jti)?
    ))
}

pub fn subst(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

/// `a.b.0.c` 형태의 경로로 JSON 값을 찾는다.
fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .filter(|p| !p.is_empty())
        .try_fold(v, |cur, key| match cur {
            Value::Array(a) => a.get(key.parse::<usize>().ok()?),
            Value::Object(o) => o.get(key),
            _ => None,
        })
}

/// 두 결과의 첫 차이를 사람이 읽을 수 있게 설명한다. 같으면 None.
pub fn diff(left: &Value, right: &Value) -> Option<String> {
    fn walk(path: &str, l: &Value, r: &Value, out: &mut Vec<String>) {
        if out.len() >= 20 {
            return;
        }
        match (l, r) {
            (Value::Object(a), Value::Object(b)) => {
                let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
                keys.sort();
                keys.dedup();
                for k in keys {
                    let p = format!("{path}.{k}");
                    match (a.get(k), b.get(k)) {
                        (Some(x), Some(y)) => walk(&p, x, y, out),
                        (x, y) => out.push(format!(
                            "{p}\n    left:  {}\n    right: {}",
                            show(x),
                            show(y)
                        )),
                    }
                }
            }
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
                for (i, (x, y)) in a.iter().zip(b).enumerate() {
                    walk(&format!("{path}[{i}]"), x, y, out);
                }
            }
            _ if l != r => out.push(format!("{path}\n    left:  {l}\n    right: {r}")),
            _ => {}
        }
    }
    fn show(v: Option<&Value>) -> String {
        v.map_or("<없음>".into(), |v| v.to_string())
    }
    let mut out = Vec::new();
    walk("$", left, right, &mut out);
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 경로로_값을_찾는다() {
        let v = json!({"a": [{"b": 1}]});
        assert_eq!(lookup(&v, "a.0.b"), Some(&json!(1)));
        assert_eq!(lookup(&v, "a.1.b"), None);
    }

    #[test]
    fn 차이를_경로와_함께_보여준다() {
        let d = diff(
            &json!({"s": 200, "b": [1, 2]}),
            &json!({"s": 200, "b": [1, 3]}),
        )
        .unwrap();
        assert!(d.contains("$.b[1]"));
        assert!(diff(&json!({"a": 1}), &json!({"a": 1})).is_none());
    }
}
