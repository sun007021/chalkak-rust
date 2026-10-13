//! 콘텐츠 협상. 원본에서 Spring MVC의 메시지 컨버터 선택과 `BasicErrorController`가 하던 일이다.
//!
//! 응답을 만드는 쪽은 `Accept`를 신경 쓰지 않고 JSON을 쓴다. 이 계층이 요청의 `Accept`를 보고 마지막에 바로잡는다.
//!
//! | 응답 | `Accept`가 JSON을 받을 때 | 받지 못할 때 |
//! | --- | --- | --- |
//! | 컨트롤러 성공 응답(본문이 있는 JSON) | `Content-Type`을 선택한 타입으로 | 406 |
//! | Spring 예외 응답(400·404·405·415) | 같음 | 같은 상태 코드의 오류 페이지 |
//! | 도메인·처리되지 않은 예외 응답 | 같음 | 500 오류 페이지(해석 실패면 원래 상태 그대로, 본문 없음) |
//! | Boot 오류 본문(방화벽, TRACE, `/error`) | 같음 | 같은 상태 코드의 오류 페이지 |
//! | 액추에이터 응답 | 액추에이터 타입 중 선택 | 406 |
//!
//! "오류 페이지"는 `Accept`가 `text/html`과 호환되면 Whitelabel 페이지, 아니면 본문 없는 응답이다.
//! 보안 필터가 직접 쓰는 401·403(`application/json;charset=UTF-8`)은 협상하지 않는다.

use std::sync::Arc;

use chalkak_app::web::marker;
use chalkak_core::Clock;
use chalkak_http::{Handler, Middleware, Request, Response};

use super::media_type::{InvalidAccept, MediaType, parse_accept, select};
use super::whitelabel;

/// 컨트롤러가 값을 JSON으로 쓸 때 응답 가능한 타입(컨버터 등록 순서).
fn json_producible() -> Vec<MediaType> {
    vec![
        MediaType::new("application", "json"),
        MediaType::new("application", "*+json"),
    ]
}

const ACTUATOR_V3: &str = "vnd.spring-boot.actuator.v3+json";
const ACTUATOR_V2: &str = "vnd.spring-boot.actuator.v2+json";
/// 액추에이터가 406일 때 `Accept`에 싣는 목록.
const ACTUATOR_ACCEPT: &str = "application/vnd.spring-boot.actuator.v3+json, application/vnd.spring-boot.actuator.v2+json, application/json";
/// 컨트롤러 응답이 406일 때 `Accept`에 싣는 목록.
const JSON_ACCEPT: &str = "application/json, application/*+json, application/yaml";

fn actuator_producible() -> Vec<MediaType> {
    vec![
        MediaType::new("application", ACTUATOR_V3),
        MediaType::new("application", ACTUATOR_V2),
        MediaType::new("application", "json"),
    ]
}

/// 요청에서 협상에 쓰는 값.
pub struct Wanted {
    accept: Result<Vec<MediaType>, InvalidAccept>,
    language: String,
}

impl Wanted {
    pub fn of(req: &Request) -> Wanted {
        let accept: Vec<&str> = req.headers.get_all("accept").collect();
        let languages: Vec<&str> = req.headers.get_all("accept-language").collect();
        Wanted {
            accept: parse_accept(&accept),
            language: whitelabel::content_language(&languages),
        }
    }

    /// 응답 가능한 타입에서 하나 고른다. `Err`는 해석 실패, `Ok(None)`은 받을 수 있는 타입이 없음.
    fn select(&self, producible: &[MediaType]) -> Result<Option<MediaType>, InvalidAccept> {
        match &self.accept {
            Ok(list) => Ok(select(list, producible)),
            Err(e) => Err(*e),
        }
    }

    /// `/error`로 디스패치된 응답에서 HTML 핸들러(`errorHtml`)가 JSON 핸들러(`error`)보다 먼저 뽑히는지.
    /// `Accept`를 품질값 내림차순(같으면 구체적인 것 먼저)으로 훑어 처음 결정하는 요소를 본다:
    /// `text/html`과 호환되는 요소(`text/*` 포함)면 HTML, `*/*`면 JSON, 그 밖의 타입은 건너뛴다.
    /// 아무도 결정하지 못하면 HTML이 후보일 때(`text/html`을 받는 요소가 있을 때)가 아니므로 JSON이다.
    fn boot_prefers_html(&self) -> bool {
        let Ok(list) = &self.accept else {
            return false;
        };
        let html = MediaType::new("text", "html");
        let mut ordered: Vec<&MediaType> = list.iter().collect();
        ordered.sort_by(|a, b| {
            b.quality()
                .partial_cmp(&a.quality())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for m in ordered {
            if m.is_wildcard_type() {
                return false;
            }
            if m.compatible_with(&html) {
                return true;
            }
        }
        false
    }

    /// `text/html`을 받을 수 있는지(`BasicErrorController.errorHtml`의 `produces`).
    fn accepts_html(&self) -> bool {
        let html = MediaType::new("text", "html");
        match &self.accept {
            Ok(list) => list.iter().any(|m| m.compatible_with(&html)),
            Err(_) => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 컨트롤러가 돌려준 본문 있는 JSON.
    Success,
    Spring,
    Handler,
    Boot,
    Actuator,
}

/// 응답을 요청의 `Accept`에 맞춰 바로잡는다. 표지 헤더는 항상 지운다.
pub fn finish(mut response: Response, wanted: &Wanted, clock: &dyn Clock) -> Response {
    let origin = response.headers.get(marker::ORIGIN).map(str::to_owned);
    response.headers.remove(marker::ORIGIN);
    let carried = take_carried(&mut response);

    let kind = match origin.as_deref() {
        Some(marker::SPRING) => Kind::Spring,
        Some(marker::HANDLER) => Kind::Handler,
        Some(marker::BOOT) => Kind::Boot,
        Some(marker::ACTUATOR) => Kind::Actuator,
        _ if is_controller_json(&response) => Kind::Success,
        _ => return response,
    };

    if kind == Kind::Actuator {
        return finish_actuator(response, wanted, clock);
    }
    // 컨테이너가 끊은 요청은 `/error`로 가서 핸들러를 고른 뒤에야 본문을 쓴다.
    if kind == Kind::Boot && wanted.boot_prefers_html() {
        return fail(kind, response, carried, wanted, false, clock);
    }
    match wanted.select(&json_producible()) {
        Ok(Some(media)) => {
            response.headers.set("Content-Type", media.render());
            response
        }
        failed => {
            let parse_failed = failed.is_err();
            fail(kind, response, carried, wanted, parse_failed, clock)
        }
    }
}

fn finish_actuator(mut response: Response, wanted: &Wanted, clock: &dyn Clock) -> Response {
    match wanted.select(&actuator_producible()) {
        Ok(Some(media)) => {
            response.headers.set("Content-Type", media.render());
            response
        }
        failed => {
            // 406 예외를 처리기가 JSON으로 쓸 수 있으면 JSON 406, 아니면 오류 페이지.
            let parse_failed = failed.is_err();
            if let Ok(Some(media)) = wanted.select(&json_producible()) {
                let body = chalkak_core::error::error_body(
                    chalkak_core::error::ErrorCode::BUSINESS_ERROR,
                    chalkak_core::error::messages::UNSUPPORTED_REQUEST,
                );
                let mut out = Response::new(406)
                    .with_header("Content-Type", media.render())
                    .with_body(body.into_bytes());
                copy_common_headers(&response, &mut out);
                return out;
            }
            let carried = vec![("Accept".to_owned(), ACTUATOR_ACCEPT.to_owned())];
            let carried = if parse_failed { Vec::new() } else { carried };
            let mut base = response;
            base.status = 406;
            fail(Kind::Spring, base, carried, wanted, parse_failed, clock)
        }
    }
}

fn is_controller_json(response: &Response) -> bool {
    !response.body.is_empty() && response.headers.get("content-type") == Some("application/json")
}

/// `x-chalkak-error-*` 헤더를 떼어 `(원래 이름, 값)`으로 돌려준다.
fn take_carried(response: &mut Response) -> Vec<(String, String)> {
    let carried: Vec<(String, String)> = response
        .headers
        .iter()
        .filter_map(|(n, v)| {
            n.to_ascii_lowercase()
                .strip_prefix(marker::CARRY_PREFIX)
                .map(|rest| (capitalize(rest), v.to_owned()))
        })
        .collect();
    let names: Vec<String> = response
        .headers
        .iter()
        .filter(|(n, _)| n.to_ascii_lowercase().starts_with(marker::CARRY_PREFIX))
        .map(|(n, _)| n.to_owned())
        .collect();
    for name in names {
        response.headers.remove(&name);
    }
    carried
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// 컨테이너가 응답을 닫아 버리는 상태 코드(Tomcat `Http11Processor.statusDropsConnection`).
fn drops_connection(status: u16) -> bool {
    matches!(status, 400 | 408 | 411 | 413 | 414 | 500 | 501 | 503)
}

/// JSON을 받을 수 없을 때의 결과.
fn fail(
    kind: Kind,
    original: Response,
    carried: Vec<(String, String)>,
    wanted: &Wanted,
    parse_failed: bool,
    clock: &dyn Clock,
) -> Response {
    let (status, headers): (u16, Vec<(String, String)>) = match kind {
        Kind::Success => {
            let accept = if parse_failed {
                Vec::new()
            } else {
                vec![("Accept".to_owned(), JSON_ACCEPT.to_owned())]
            };
            (406, accept)
        }
        // 해석에 실패한 `Accept`로는 `Allow`·`Accept` 안내 헤더가 붙지 않는다(java-a 확인).
        Kind::Spring | Kind::Boot if parse_failed => (original.status, Vec::new()),
        Kind::Spring | Kind::Boot => (original.status, carried),
        // 처리기가 JSON을 쓰지 못하면 예외가 컨테이너까지 올라가 500이 된다.
        // 해석에 실패한 `Accept`는 오류 디스패치 자체가 실패해 처리기가 정한 상태가 그대로 남는다.
        Kind::Handler if parse_failed => (original.status, Vec::new()),
        Kind::Handler => (500, Vec::new()),
        Kind::Actuator => (406, Vec::new()),
    };

    let mut out = if wanted.accepts_html() {
        let error_status = boot_status(kind, &original).unwrap_or(status);
        let reason = boot_reason(kind, &original).unwrap_or(whitelabel::reason_phrase(status));
        let mut page = whitelabel::whitelabel(clock, &wanted.language, error_status, reason);
        page.status = status;
        page
    } else {
        Response::new(status)
    };
    copy_common_headers(&original, &mut out);
    for (name, value) in headers {
        out.headers.append(name, value);
    }
    if drops_connection(status) && !out.headers.has_token("connection", "close") {
        out.headers.append("Connection", "close");
    }
    out
}

/// 오류 페이지에 적히는 `status`. Boot 오류 본문(`/error`의 999 등)이 있으면 그 값을 쓴다.
fn boot_status(kind: Kind, original: &Response) -> Option<u16> {
    if kind != Kind::Boot {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&original.body).ok()?;
    v.get("status")?.as_u64().and_then(|s| u16::try_from(s).ok())
}

fn boot_reason(kind: Kind, original: &Response) -> Option<&'static str> {
    if kind != Kind::Boot {
        return None;
    }
    boot_status(kind, original).map(whitelabel::reason_phrase)
}

/// 본문·`Content-Type`·`Content-Length`를 뺀 나머지 헤더(보안 헤더, 요청 ID, CORS 등)를 옮긴다.
fn copy_common_headers(from: &Response, to: &mut Response) {
    for (name, value) in from.headers.iter() {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "content-type" | "content-length" | "content-language" | "transfer-encoding"
        ) {
            continue;
        }
        to.headers.append(name, value);
    }
}

pub struct Negotiate {
    clock: Arc<dyn Clock>,
}

impl Negotiate {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Negotiate { clock }
    }
}

impl Middleware for Negotiate {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        let wanted = Wanted::of(&req);
        let response = next.handle(req);
        finish(response, &wanted, &*self.clock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;
    use chalkak_http::Method;

    fn clock() -> FixedClock {
        FixedClock::new("2026-10-13T10:02:35Z".parse().unwrap())
    }

    fn wanted(accept: &str) -> Wanted {
        let mut req = Request::new(Method::Get, "/");
        if !accept.is_empty() {
            req.headers.append("Accept", accept);
        }
        Wanted::of(&req)
    }

    fn json(status: u16, origin: Option<&str>) -> Response {
        let mut r = Response::new(status)
            .with_header("Content-Type", "application/json")
            .with_header("X-Request-Id", "r1")
            .with_body(r#"{"errorCode":"BUSINESS_ERROR","message":"m"}"#);
        if let Some(o) = origin {
            r.headers.append(marker::ORIGIN, o);
        }
        r
    }

    /// 상태, Content-Type, 본문 앞부분
    fn summary(r: &Response) -> (u16, Option<String>, String) {
        let body = String::from_utf8_lossy(&r.body);
        (
            r.status,
            r.headers.get("content-type").map(str::to_owned),
            body.chars().take(25).collect(),
        )
    }

    // 아래 기대값은 java-a에 Accept를 바꿔 보낸 결과다.
    #[test]
    fn 성공_응답은_타입만_바꾸고_받을_수_없으면_406이다() {
        let c = clock();
        let r = finish(json(200, None), &wanted("application/json;charset=UTF-8"), &c);
        assert_eq!(r.headers.get("content-type"), Some("application/json;charset=UTF-8"));
        assert_eq!(r.status, 200);

        let r = finish(json(200, None), &wanted("text/html"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(r.headers.get("content-type"), Some("text/html;charset=UTF-8"));
        assert_eq!(r.headers.get("accept"), Some(JSON_ACCEPT));
        assert_eq!(r.headers.get("x-request-id"), Some("r1"));

        let r = finish(json(200, None), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (406, None, String::new()));
        assert_eq!(r.headers.get("accept"), Some(JSON_ACCEPT));

        let r = finish(json(200, None), &wanted("bad"), &c);
        assert_eq!(summary(&r), (406, None, String::new()));
        assert!(!r.headers.contains("accept"));
    }

    #[test]
    fn spring_예외는_같은_상태의_오류_페이지다() {
        let c = clock();
        let spring = |status| {
            let mut r = json(status, Some(marker::SPRING));
            if status == 405 {
                r.headers.append(marker::CARRY_ALLOW, "GET, PUT");
            }
            r
        };
        let r = finish(spring(400), &wanted("text/html"), &c);
        assert_eq!(r.status, 400);
        assert_eq!(r.headers.get("content-language"), Some("en-US"));
        assert!(r.headers.has_token("connection", "close"));
        assert!(String::from_utf8_lossy(&r.body).contains("type=Bad Request, status=400"));
        assert!(!r.headers.contains(marker::ORIGIN));

        let r = finish(spring(405), &wanted("text/html"), &c);
        assert_eq!(r.status, 405);
        assert_eq!(r.headers.get("allow"), Some("GET, PUT"));
        assert!(!r.headers.has_token("connection", "close"));

        let r = finish(spring(405), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (405, None, String::new()));
        assert_eq!(r.headers.get("allow"), Some("GET, PUT"));

        // JSON을 받으면 오류 JSON 그대로이고 Allow는 나가지 않는다
        let r = finish(spring(405), &wanted("application/json"), &c);
        assert_eq!(r.status, 405);
        assert!(!r.headers.contains("allow"));
        assert!(!r.headers.contains(marker::CARRY_ALLOW));
    }

    #[test]
    fn 처리기_예외는_500_오류_페이지이고_해석_실패면_상태가_남는다() {
        let c = clock();
        let r = finish(json(404, Some(marker::HANDLER)), &wanted("text/html"), &c);
        assert_eq!(r.status, 500);
        assert!(String::from_utf8_lossy(&r.body).contains("type=Internal Server Error, status=500"));
        assert!(r.headers.has_token("connection", "close"));

        let r = finish(json(404, Some(marker::HANDLER)), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (500, None, String::new()));

        let r = finish(json(404, Some(marker::HANDLER)), &wanted("bad"), &c);
        assert_eq!(summary(&r), (404, None, String::new()));
    }

    #[test]
    fn boot_오류는_본문의_상태와_이유를_페이지에_싣는다() {
        let c = clock();
        let mut error = Response::new(500)
            .with_header("Content-Type", "application/json")
            .with_body(r#"{"timestamp":"2026-10-13T10:02:35.000Z","status":999,"error":"None"}"#);
        error.headers.append(marker::ORIGIN, marker::BOOT);
        let r = finish(error, &wanted("text/html"), &c);
        assert_eq!(r.status, 500);
        assert!(String::from_utf8_lossy(&r.body).contains("type=None, status=999"));
    }

    #[test]
    fn 표지_없는_다른_타입은_건드리지_않는다() {
        let c = clock();
        let r = Response::new(401)
            .with_header("Content-Type", "application/json;charset=UTF-8")
            .with_body("{}");
        let r = finish(r, &wanted("text/html"), &c);
        assert_eq!(r.status, 401);
        assert_eq!(r.body, b"{}");
        let empty = finish(Response::new(204), &wanted("text/html"), &c);
        assert_eq!(empty.status, 204);
    }

    #[test]
    fn 액추에이터는_자기_타입_중에서_고른다() {
        let c = clock();
        let health = || {
            let mut r = Response::new(200)
                .with_header("Content-Type", "application/vnd.spring-boot.actuator.v3+json")
                .with_body(r#"{"status":"UP"}"#);
            r.headers.append(marker::ORIGIN, marker::ACTUATOR);
            r
        };
        let ct = |accept: &str| {
            finish(health(), &wanted(accept), &c)
                .headers
                .get("content-type")
                .map(str::to_owned)
        };
        assert_eq!(ct("").as_deref(), Some("application/vnd.spring-boot.actuator.v3+json"));
        assert_eq!(ct("application/*").as_deref(), Some("application/vnd.spring-boot.actuator.v3+json"));
        assert_eq!(ct("application/*+json").as_deref(), Some("application/vnd.spring-boot.actuator.v3+json"));
        assert_eq!(ct("application/json").as_deref(), Some("application/json"));
        assert_eq!(
            ct("application/json;charset=UTF-8").as_deref(),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(ct("text/html,application/json;q=0.5").as_deref(), Some("application/json"));

        // 받을 수 없으면 406. 처리기가 JSON을 쓸 수 있으면(`+json`) JSON 오류, 아니면 오류 페이지
        let r = finish(health(), &wanted("application/vnd.x+json"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(r.headers.get("content-type"), Some("application/vnd.x+json"));
        let r = finish(health(), &wanted("text/html"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(r.headers.get("accept"), Some(ACTUATOR_ACCEPT));
        assert!(String::from_utf8_lossy(&r.body).contains("type=Not Acceptable, status=406"));
        let r = finish(health(), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (406, None, String::new()));
    }
}
