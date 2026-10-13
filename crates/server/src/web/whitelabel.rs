//! Spring Boot `Whitelabel Error Page`와 Tomcat 오류 페이지.
//!
//! - Whitelabel: `BasicErrorController`가 `text/html`을 받는 요청에 돌려주는 기본 오류 페이지.
//!   `Content-Language`는 `Accept-Language`로 정한 로캘이고, 본문의 시각은 `Date.toString()` 모양이다.
//! - Tomcat: 컨테이너가 요청을 파싱하다 끊을 때 쓰는 `ErrorReportValve` 페이지(보고서·서버 정보 없음).
//!   `Content-Language`는 항상 `en`이다.
//!
//! 원본 응답은 java-a에 직접 보내 받은 것이다.

use chalkak_core::Clock;
use chalkak_http::Response;
use jiff::Timestamp;

/// 상태 코드의 영어 이유 문구(`HttpStatus.getReasonPhrase`). 알 수 없는 코드는 `None`.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "None",
    }
}

/// `Date.toString()`: `Tue Oct 13 10:02:35 UTC 2026`. 서버 기본 시간대를 UTC로 본다.
pub fn java_date_string(at: Timestamp) -> String {
    at.strftime("%a %b %d %H:%M:%S UTC %Y").to_string()
}

/// Whitelabel 페이지. `error`는 `type=` 자리에 들어가는 상태 이유 문구다.
pub fn whitelabel(clock: &dyn Clock, language: &str, status: u16, error: &str) -> Response {
    let body = format!(
        "<html><body><h1>Whitelabel Error Page</h1><p>This application has no explicit mapping for /error, so you are seeing this as a fallback.</p><div id='created'>{}</div><div>There was an unexpected error (type={error}, status={status}).</div></body></html>",
        java_date_string(clock.now())
    );
    Response::new(status)
        .with_header("Content-Type", "text/html;charset=UTF-8")
        .with_header("Content-Language", language)
        .with_body(body.into_bytes())
}

/// Tomcat `ErrorReportValve` 페이지(`showReport=false`, `showServerInfo=false`).
pub fn tomcat_page(status: u16) -> Response {
    let reason = reason_phrase(status);
    let title = format!("HTTP Status {status} – {reason}");
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><title>{title}</title><style type=\"text/css\">body {{font-family:Tahoma,Arial,sans-serif;}} h1, h2, h3, b {{color:white;background-color:#525D76;}} h1 {{font-size:22px;}} h2 {{font-size:16px;}} h3 {{font-size:14px;}} p {{font-size:12px;}} a {{color:black;}} .line {{height:1px;background-color:#525D76;border:none;}}</style></head><body><h1>{title}</h1></body></html>"
    );
    Response::new(status)
        .with_header("Content-Type", "text/html;charset=utf-8")
        .with_header("Content-Language", "en")
        .with_body(body.into_bytes())
}

/// `Accept-Language` 값들에서 응답 `Content-Language`를 정한다(Tomcat `Request.getLocale` +
/// `Locale.toLanguageTag`). 품질값 내림차순(같으면 입력 순서)으로 첫 요소를 쓰고 `q=0`은 건너뛴다.
/// 헤더가 없거나 쓸 요소가 없으면 서버 기본 로캘 `en-US`다.
pub fn content_language(values: &[&str]) -> String {
    let mut ranges: Vec<(f64, &str)> = Vec::new();
    for value in values {
        for part in value.split(',') {
            let mut pieces = part.split(';');
            let tag = pieces.next().unwrap_or("").trim();
            if tag.is_empty() {
                continue;
            }
            let mut q = 1.0;
            for param in pieces {
                if let Some(v) = param.trim().strip_prefix("q=") {
                    q = v.trim().parse().unwrap_or(0.0);
                }
            }
            if q > 0.0 {
                ranges.push((q, tag));
            }
        }
    }
    // 안정 정렬: 같은 품질값은 입력 순서를 지킨다.
    ranges.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    match ranges.first() {
        Some((_, tag)) => language_tag(tag),
        None => "en-US".to_owned(),
    }
}

/// `Locale.forLanguageTag(tag).toLanguageTag()`의 근사. 앞에서부터 올바른 하위 태그만 취하고 첫 하위 태그가 틀리면 `und`.
fn language_tag(tag: &str) -> String {
    // 공백 이후는 쓰지 않는다(Tomcat이 토큰 하나만 읽는다).
    let tag = tag.split_whitespace().next().unwrap_or("");
    let mut subtags = tag.split('-');
    let Some(language) = subtags.next() else {
        return "und".to_owned();
    };
    let alpha = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic());
    let alnum = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if !(alpha(language) && matches!(language.len(), 2 | 3 | 5..=8)) {
        return "und".to_owned();
    }
    let mut out = language.to_ascii_lowercase();
    let (mut has_script, mut has_region) = (false, false);
    for sub in subtags {
        if !has_script && !has_region && sub.len() == 4 && alpha(sub) {
            let mut chars = sub.chars();
            let first = chars.next().map(|c| c.to_ascii_uppercase()).unwrap_or(' ');
            out.push('-');
            out.push(first);
            out.push_str(&chars.as_str().to_ascii_lowercase());
            has_script = true;
        } else if !has_region
            && ((sub.len() == 2 && alpha(sub))
                || (sub.len() == 3 && sub.bytes().all(|b| b.is_ascii_digit())))
        {
            out.push('-');
            out.push_str(&sub.to_ascii_uppercase());
            has_region = true;
        } else if alnum(sub)
            && ((5..=8).contains(&sub.len())
                || (sub.len() == 4 && sub.as_bytes()[0].is_ascii_digit()))
        {
            out.push('-');
            out.push_str(&sub.to_ascii_lowercase());
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;

    // java-a에 `Accept-Language`를 바꿔 보낸 결과(Whitelabel의 Content-Language)
    #[test]
    fn 로캘은_accept_language_첫_항목이다() {
        let cases = [
            (vec!["ko"], "ko"),
            (vec!["ko-KR"], "ko-KR"),
            (vec!["en-GB"], "en-GB"),
            (vec!["fr, en;q=0.5"], "fr"),
            (vec!["*"], "und"),
            (vec!["xx-YY"], "xx-YY"),
            (vec!["bad lang!!"], "bad"),
            (vec!["ko;q=0"], "en-US"),
            (vec!["zh-Hant-TW"], "zh-Hant-TW"),
            (vec!["en_US"], "und"),
            (vec![], "en-US"),
            (vec!["en;q=0.3, ko;q=0.9"], "ko"),
        ];
        for (values, expected) in cases {
            assert_eq!(content_language(&values), expected, "{values:?}");
        }
    }

    // java-a: GET /api/v1/nothing + Accept: text/html (2026-10-13 10:02:35 UTC)
    #[test]
    fn whitelabel_본문은_원본과_같다() {
        let clock = FixedClock::new("2026-10-13T10:02:35Z".parse().unwrap());
        let r = whitelabel(&clock, "en-US", 404, "Not Found");
        assert_eq!(r.status, 404);
        assert_eq!(r.headers.get("content-type"), Some("text/html;charset=UTF-8"));
        assert_eq!(r.headers.get("content-language"), Some("en-US"));
        let body = String::from_utf8(r.body).unwrap();
        assert_eq!(
            body,
            "<html><body><h1>Whitelabel Error Page</h1><p>This application has no explicit mapping for /error, so you are seeing this as a fallback.</p><div id='created'>Tue Oct 13 10:02:35 UTC 2026</div><div>There was an unexpected error (type=Not Found, status=404).</div></body></html>"
        );
        assert_eq!(body.len(), 275);
    }

    #[test]
    fn tomcat_페이지_길이는_원본과_같다() {
        // java-a: %2F 경로 400 -> Content-Length 435, 505 -> 모양이 같고 제목만 다르다
        let r = tomcat_page(400);
        assert_eq!(r.body.len(), 435);
        assert_eq!(r.headers.get("content-language"), Some("en"));
        assert_eq!(
            r.headers.get("content-type"),
            Some("text/html;charset=utf-8")
        );
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains("<title>HTTP Status 400 – Bad Request</title>"));
        assert!(text.ends_with("<body><h1>HTTP Status 400 – Bad Request</h1></body></html>"));
    }
}
