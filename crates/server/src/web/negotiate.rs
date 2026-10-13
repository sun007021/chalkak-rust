//! 콘텐츠 협상. 원본에서 Spring MVC의 메시지 컨버터 선택과 `BasicErrorController`가 하던 일이다.
//!
//! 응답을 만드는 쪽은 `Accept`를 신경 쓰지 않고 JSON을 쓴다. 이 계층이 요청의 `Accept`를 보고 마지막에 바로잡는다.
//!
//! | 응답 | `Accept`가 JSON을 받을 때 | 받지 못할 때 |
//! | --- | --- | --- |
//! | 컨트롤러 성공 응답(본문이 있는 JSON) | `Content-Type`을 선택한 타입으로 | 406 |
//! | Spring 예외 응답(400·404·405·415) | 같음 | 같은 상태 코드의 오류 페이지 |
//! | 도메인·처리되지 않은 예외 응답 | 같음 | 500 오류 페이지(해석 실패면 원래 상태 그대로, 본문 없음) |
//! | 파라미터 읽기 실패(잘못된 퍼센트 쿼리) | 같음(500) | 400 오류 페이지(해석 실패면 500, 본문 없음) |
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
        MediaType::new("application", "yaml"),
    ]
}

/// springdoc YAML 문서(`/v3/api-docs.yaml`)가 응답 가능한 타입.
const DOCS_YAML: &str = "application/vnd.oai.openapi";

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
#[derive(Clone)]
pub struct Wanted {
    accept: Result<Vec<MediaType>, InvalidAccept>,
    language: String,
    /// 컨버터가 쓰는 응답에 `Content-Disposition: inline;filename=f.txt`를 붙이는 요청 경로인가.
    disposition: bool,
}

impl Wanted {
    pub fn of(req: &Request) -> Wanted {
        let accept: Vec<&str> = req.headers.get_all("accept").collect();
        let languages: Vec<&str> = req.headers.get_all("accept-language").collect();
        Wanted {
            accept: parse_accept(&accept),
            language: whitelabel::content_language(&languages),
            disposition: needs_disposition(&req.raw_path),
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

/// 확장자 때문에 안전하지 않다고 보지 않는 확장자(`AbstractMessageConverterMethodProcessor.SAFE_EXTENSIONS`).
const SAFE_EXTENSIONS: [&str; 16] = [
    "txt",
    "text",
    "yml",
    "properties",
    "csv",
    "json",
    "xml",
    "atom",
    "rss",
    "png",
    "jpe",
    "jpeg",
    "jpg",
    "gif",
    "wbmp",
    "bmp",
];

/// 미디어 타입이 `image`·`audio`·`video`라서 안전한 확장자(`MediaTypeFactory`가 그 타입으로 푸는 것 중 흔한 것).
const SAFE_MEDIA_EXTENSIONS: [&str; 40] = [
    "webp", "svg", "ico", "tif", "tiff", "avif", "heic", "heif", "jfif", "apng", "mp3", "wav",
    "ogg", "oga", "m4a", "flac", "aac", "mid", "midi", "weba", "opus", "mp4", "mpeg", "mpg",
    "webm", "avi", "mov", "mkv", "ogv", "m4v", "3gp", "wmv", "svgz", "jp2", "psd", "mpga", "wma",
    "aif", "aiff", "ra",
];

fn is_safe_extension(extension: &str) -> bool {
    if extension.is_empty() {
        return true;
    }
    let lower = extension.to_ascii_lowercase();
    SAFE_EXTENSIONS.contains(&lower.as_str()) || SAFE_MEDIA_EXTENSIONS.contains(&lower.as_str())
}

/// `StringUtils.getFilenameExtension`: 마지막 `.` 뒤, 그 뒤에 `/`가 없을 때만.
fn filename_extension(path: &str) -> Option<&str> {
    let dot = path.rfind('.')?;
    if path.rfind('/').is_some_and(|slash| slash > dot) {
        return None;
    }
    Some(&path[dot + 1..])
}

/// 퍼센트 디코딩(잘못된 이스케이프는 그대로 둔다).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `addContentDispositionHeader`(반사 파일 다운로드 방어): 요청 URI 마지막 조각의 확장자나 경로 매개변수(`;…`)의
/// 확장자가 안전 목록에 없으면 컨버터가 쓰는 응답에 `Content-Disposition: inline;filename=f.txt`를 붙인다.
/// 확장자가 `.html`이어도 이 서버의 컨트롤러는 `text/html`을 만들지 않아 예외가 없다.
fn needs_disposition(raw_path: &str) -> bool {
    let filename = &raw_path[raw_path.rfind('/').map_or(0, |i| i + 1)..];
    let (name, params) = match filename.find(';') {
        Some(i) => (&filename[..i], &filename[i..]),
        None => (filename, ""),
    };
    let name = percent_decode(name);
    let params = percent_decode(params);
    !is_safe_extension(filename_extension(&name).unwrap_or(""))
        || !is_safe_extension(filename_extension(&params).unwrap_or(""))
}

/// 컨버터가 응답을 쓸 때의 부가 처리: 미디어 타입 설정, YAML 변환, `Content-Disposition`.
fn write_converted(mut response: Response, media: &MediaType, wanted: &Wanted) -> Response {
    if media.is("application", "yaml")
        && let Some(yaml) = super::yaml::from_json(&response.body)
    {
        response.body = yaml;
    }
    response.headers.set("Content-Type", media.render());
    if wanted.disposition {
        response
            .headers
            .set("Content-Disposition", "inline;filename=f.txt");
    }
    response
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 컨트롤러가 돌려준 본문 있는 JSON.
    Success,
    Spring,
    Handler,
    Boot,
    Actuator,
    /// springdoc JSON 문서·설정: 응답 타입이 `application/json`으로 정해져 있어 `Accept`가 그것을 받아야 한다.
    DocsJson,
    /// springdoc YAML 문서([`DOCS_YAML`]만 만든다).
    DocsYaml,
    /// 파라미터를 읽다 컨테이너 예외로 실패한 응답([`marker::PARAM`]).
    Param,
}

/// 응답을 요청의 `Accept`에 맞춰 바로잡는다. 표지 헤더는 항상 지운다.
pub fn finish(mut response: Response, wanted: &Wanted, clock: &dyn Clock) -> Response {
    let origin = response.headers.get(marker::ORIGIN).map(str::to_owned);
    response.headers.remove(marker::ORIGIN);
    // 정적 리소스가 HTML이면 핸들러가 `text/html`을 만들 수 있다고 밝혀 두어 `.html` 확장자가 안전하다.
    let adjusted;
    let wanted = if response.headers.contains(marker::HTML_RESOURCE) {
        response.headers.remove(marker::HTML_RESOURCE);
        adjusted = Wanted {
            disposition: false,
            ..wanted.clone()
        };
        &adjusted
    } else {
        wanted
    };
    let carried = take_carried(&mut response);

    let kind = match origin.as_deref() {
        Some(marker::SPRING) => Kind::Spring,
        Some(marker::HANDLER) => Kind::Handler,
        Some(marker::BOOT) => Kind::Boot,
        Some(marker::ACTUATOR) => Kind::Actuator,
        Some(marker::DOCS_YAML) => Kind::DocsYaml,
        Some(marker::DOCS_JSON) => Kind::DocsJson,
        Some(marker::PARAM) => Kind::Param,
        // 액추에이터는 핸들러 매핑의 `produces` 검사가 먼저라 액추에이터 타입을 받지 못하면 406이다.
        Some(marker::ACTUATOR_PARAM) => {
            if matches!(wanted.select(&actuator_producible()), Ok(Some(_))) {
                Kind::Param
            } else {
                return finish_restricted(
                    response,
                    wanted,
                    clock,
                    &actuator_producible(),
                    ACTUATOR_ACCEPT,
                );
            }
        }
        _ if is_controller_json(&response) => Kind::Success,
        _ => return response,
    };

    if kind == Kind::Actuator {
        return finish_restricted(
            response,
            wanted,
            clock,
            &actuator_producible(),
            ACTUATOR_ACCEPT,
        );
    }
    if kind == Kind::DocsJson {
        let producible = [MediaType::new("application", "json")];
        return finish_restricted(response, wanted, clock, &producible, "application/json");
    }
    if kind == Kind::DocsYaml {
        let producible = [MediaType::new("application", "vnd.oai.openapi")];
        return finish_restricted(response, wanted, clock, &producible, DOCS_YAML);
    }
    // 컨테이너가 끊은 요청은 `/error`로 가서 핸들러를 고른 뒤에야 본문을 쓴다.
    if kind == Kind::Boot && wanted.boot_prefers_html() {
        return fail(kind, response, carried, wanted, false, clock);
    }
    match wanted.select(&json_producible()) {
        Ok(Some(media)) => write_converted(response, &media, wanted),
        failed => {
            let parse_failed = failed.is_err();
            fail(kind, response, carried, wanted, parse_failed, clock)
        }
    }
}

/// 핸들러 매핑의 `produces`가 정해 둔 타입만 응답할 수 있는 응답(액추에이터, springdoc YAML).
/// 받을 수 없으면 406이고, 예외 처리기가 JSON을 쓸 수 있으면 JSON 406, 아니면 오류 페이지다.
fn finish_restricted(
    mut response: Response,
    wanted: &Wanted,
    clock: &dyn Clock,
    producible: &[MediaType],
    accept_header: &str,
) -> Response {
    match wanted.select(producible) {
        Ok(Some(media)) => {
            // springdoc 문서는 컨버터를 거치지 않는지 확장자 방어 헤더가 붙지 않는다(java-3).
            response.headers.set("Content-Type", media.render());
            response
        }
        failed => {
            let parse_failed = failed.is_err();
            if let Ok(Some(media)) = wanted.select(&json_producible()) {
                let body = chalkak_core::error::error_body(
                    chalkak_core::error::ErrorCode::BUSINESS_ERROR,
                    chalkak_core::error::messages::UNSUPPORTED_REQUEST,
                );
                let mut out = Response::new(406).with_body(body.into_bytes());
                out = write_converted(out, &media, wanted);
                copy_common_headers(&response, &mut out);
                return out;
            }
            let carried = vec![("Accept".to_owned(), accept_header.to_owned())];
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
        // 처리기가 쓰지 못하면 컨테이너가 파라미터 해석 실패로 정해 둔 400이 남는다.
        // 해석에 실패한 `Accept`로는 `/error` 디스패치가 실패해 처리기의 500이 그대로 남는다.
        Kind::Param if parse_failed => (original.status, Vec::new()),
        Kind::Param => (400, Vec::new()),
        Kind::Actuator | Kind::DocsYaml | Kind::DocsJson => (406, Vec::new()),
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
    v.get("status")?
        .as_u64()
        .and_then(|s| u16::try_from(s).ok())
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
        let r = finish(
            json(200, None),
            &wanted("application/json;charset=UTF-8"),
            &c,
        );
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(r.status, 200);

        let r = finish(json(200, None), &wanted("text/html"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(
            r.headers.get("content-type"),
            Some("text/html;charset=UTF-8")
        );
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
        assert!(
            String::from_utf8_lossy(&r.body).contains("type=Internal Server Error, status=500")
        );
        assert!(r.headers.has_token("connection", "close"));

        let r = finish(
            json(404, Some(marker::HANDLER)),
            &wanted("application/xml"),
            &c,
        );
        assert_eq!(summary(&r), (500, None, String::new()));

        let r = finish(json(404, Some(marker::HANDLER)), &wanted("bad"), &c);
        assert_eq!(summary(&r), (404, None, String::new()));
    }

    // java-3: GET /api/v1/topics?date=2026-10-12&x=%zz 에 Accept를 바꿔 보낸 결과
    // java-3: 확장자가 안전 목록에 없는 경로의 컨버터 응답에 Content-Disposition: inline;filename=f.txt
    #[test]
    fn 안전하지_않은_확장자_경로에만_content_disposition을_붙인다() {
        for unsafe_path in [
            "/api/v1/nothing.js",
            "/api/v1/nothing.html",
            "/swagger-ui/swagger-ui.css",
            "/a.pdf",
            "/a.zip",
            "/v3/api-docs.yaml",
            "/api/v1/topics;x.js",
            "/a.x",
        ] {
            assert!(needs_disposition(unsafe_path), "{unsafe_path}");
        }
        for safe_path in [
            "/api/v1/nothing",
            "/api/v1/nothing.txt",
            "/a.json",
            "/a.PNG",
            "/a.jpg",
            "/a.webp",
            "/a.svg",
            "/a.mp3",
            "/a.mp4",
            "/a.yml",
            "/a.csv",
            "/api/v1/not.hing/x",
            "/a.",
            "/api/v1/topics;x",
        ] {
            assert!(!needs_disposition(safe_path), "{safe_path}");
        }
        let c = clock();
        let mut req = Request::new(Method::Get, "/api/v1/nothing.js");
        req.raw_path = "/api/v1/nothing.js".into();
        let r = finish(json(404, Some(marker::SPRING)), &Wanted::of(&req), &c);
        assert_eq!(
            r.headers.get("content-disposition"),
            Some("inline;filename=f.txt")
        );
        // 오류 페이지에는 붙지 않는다
        req.headers.append("Accept", "text/html");
        let r = finish(json(404, Some(marker::SPRING)), &Wanted::of(&req), &c);
        assert!(!r.headers.contains("content-disposition"));
    }

    #[test]
    fn 파라미터_읽기_실패는_json이면_500이고_아니면_400_페이지다() {
        let c = clock();
        let failed = || json(500, Some(marker::PARAM));
        let r = finish(failed(), &wanted(""), &c);
        assert_eq!(summary(&r).0, 500);
        assert_eq!(r.headers.get("content-type"), Some("application/json"));
        let r = finish(failed(), &wanted("text/html"), &c);
        assert_eq!(r.status, 400);
        assert!(String::from_utf8_lossy(&r.body).contains("type=Bad Request, status=400"));
        assert!(r.headers.has_token("connection", "close"));
        let r = finish(failed(), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (400, None, String::new()));
        let r = finish(failed(), &wanted("bad"), &c);
        assert_eq!(summary(&r), (500, None, String::new()));
    }

    // java-3: GET /actuator/health?x=%zz 에 Accept를 바꿔 보낸 결과(핸들러 매핑의 produces 검사가 먼저다)
    #[test]
    fn 액추에이터_파라미터_읽기_실패는_액추에이터_타입을_받을_때만_500이다() {
        let c = clock();
        let failed = || json(500, Some(marker::ACTUATOR_PARAM));
        for accept in [
            "",
            "*/*",
            "application/json",
            "text/html, application/json;q=0.5",
        ] {
            let r = finish(failed(), &wanted(accept), &c);
            assert_eq!(r.status, 500, "{accept}");
        }
        let r = finish(failed(), &wanted("text/html"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(r.headers.get("accept"), Some(ACTUATOR_ACCEPT));
        let r = finish(failed(), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (406, None, String::new()));
        let r = finish(failed(), &wanted("application/problem+json, text/html"), &c);
        assert_eq!(r.status, 406);
        assert!(String::from_utf8_lossy(&r.body).contains("BUSINESS_ERROR"));
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
                .with_header(
                    "Content-Type",
                    "application/vnd.spring-boot.actuator.v3+json",
                )
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
        assert_eq!(
            ct("").as_deref(),
            Some("application/vnd.spring-boot.actuator.v3+json")
        );
        assert_eq!(
            ct("application/*").as_deref(),
            Some("application/vnd.spring-boot.actuator.v3+json")
        );
        assert_eq!(
            ct("application/*+json").as_deref(),
            Some("application/vnd.spring-boot.actuator.v3+json")
        );
        assert_eq!(ct("application/json").as_deref(), Some("application/json"));
        assert_eq!(
            ct("application/json;charset=UTF-8").as_deref(),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(
            ct("text/html,application/json;q=0.5").as_deref(),
            Some("application/json")
        );

        // 받을 수 없으면 406. 처리기가 JSON을 쓸 수 있으면(`+json`) JSON 오류, 아니면 오류 페이지
        let r = finish(health(), &wanted("application/vnd.x+json"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/vnd.x+json")
        );
        let r = finish(health(), &wanted("text/html"), &c);
        assert_eq!(r.status, 406);
        assert_eq!(r.headers.get("accept"), Some(ACTUATOR_ACCEPT));
        assert!(String::from_utf8_lossy(&r.body).contains("type=Not Acceptable, status=406"));
        let r = finish(health(), &wanted("application/xml"), &c);
        assert_eq!(summary(&r), (406, None, String::new()));
    }
}
