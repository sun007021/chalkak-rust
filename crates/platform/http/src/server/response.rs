//! 응답과 직렬화.

use std::time::{SystemTime, UNIX_EPOCH};

use super::headers::Headers;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// 순서를 보존하고 같은 이름을 여러 개 허용한다.
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    /// 헤더를 끝에 추가한다(같은 이름도 덮어쓰지 않는다).
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.append(name, value);
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

/// 직렬화 옵션.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WriteOptions {
    /// HEAD 응답: 본문을 보내지 않는다. `Content-Length`는 GET이었다면 나갈 값을 유지한다.
    pub head_only: bool,
    /// 응답 뒤에 연결을 닫는다(`Connection: close`를 붙인다).
    pub close: bool,
    /// HTTP/1.0 클라이언트가 keep-alive를 요청했다(`Connection: keep-alive`를 붙인다).
    pub keep_alive_header: bool,
}

pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// 본문(과 `Content-Length`)이 없어야 하는 상태 코드.
fn bodyless_status(status: u16) -> bool {
    status < 200 || status == 204 || status == 304
}

/// 헤더 인젝션을 막는다: CR·LF·NUL은 공백으로 바꾼다.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\0') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// 상태 줄 + 헤더 + (조건부) 본문을 바이트로 만든다. 항상 `HTTP/1.1`로 응답한다(Tomcat과 같다).
pub(crate) fn serialize(resp: &Response, opts: WriteOptions) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + resp.body.len());
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            resp.status,
            reason_phrase(resp.status)
        )
        .as_bytes(),
    );

    let no_body = bodyless_status(resp.status);
    let mut has_date = false;
    let mut has_connection_close = false;
    let mut has_connection = false;
    let mut handler_content_length: Option<&str> = None;
    for (name, value) in resp.headers.iter() {
        if name.eq_ignore_ascii_case("content-length") {
            handler_content_length.get_or_insert(value);
            continue; // 아래에서 한 번만 쓴다
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            continue; // 청크 응답은 지원하지 않는다. 항상 Content-Length로 보낸다.
        }
        if name.eq_ignore_ascii_case("date") {
            has_date = true;
        }
        if name.eq_ignore_ascii_case("connection") {
            has_connection = true;
            if value
                .split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("close"))
            {
                has_connection_close = true;
            }
        }
        push_header(&mut out, name, value);
    }
    if !has_date {
        push_header(&mut out, "Date", &http_date(SystemTime::now()));
    }
    if opts.close && !has_connection_close {
        push_header(&mut out, "Connection", "close");
    } else if opts.keep_alive_header && !opts.close && !has_connection {
        push_header(&mut out, "Connection", "keep-alive");
    }
    if !no_body {
        let len = if opts.head_only {
            handler_content_length
                .filter(|v| v.trim().bytes().all(|b| b.is_ascii_digit()) && !v.trim().is_empty())
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|| resp.body.len().to_string())
        } else {
            resp.body.len().to_string()
        };
        push_header(&mut out, "Content-Length", &len);
    }
    out.extend_from_slice(b"\r\n");
    if !opts.head_only && !no_body {
        out.extend_from_slice(&resp.body);
    }
    out
}

fn push_header(out: &mut Vec<u8>, name: &str, value: &str) {
    out.extend_from_slice(sanitize(name).as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(sanitize(value).as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// RFC 9110 IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub(crate) fn http_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant의 civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WD[days.rem_euclid(7) as usize],
        d,
        MO[(mo - 1) as usize],
        y,
        h,
        m,
        s
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn text(b: &[u8]) -> String {
        String::from_utf8(b.to_vec()).unwrap()
    }

    #[test]
    fn imf_fixdate() {
        let t = UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        // 윤일
        let t = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(http_date(t), "Thu, 29 Feb 2024 00:00:00 GMT");
    }

    #[test]
    fn keeps_header_order_and_duplicate_names() {
        let r = Response::new(200)
            .with_header("Vary", "Origin")
            .with_header("X-Mid", "1")
            .with_header("Vary", "Access-Control-Request-Method")
            .with_header("Vary", "Access-Control-Request-Headers")
            .with_body(b"hi".to_vec());
        let s = text(&serialize(&r, WriteOptions::default()));
        let (head, body) = s.split_once("\r\n\r\n").unwrap();
        assert_eq!(body, "hi");
        let lines: Vec<_> = head.split("\r\n").collect();
        assert_eq!(lines[0], "HTTP/1.1 200 OK");
        assert_eq!(
            &lines[1..4],
            [
                "Vary: Origin",
                "X-Mid: 1",
                "Vary: Access-Control-Request-Method"
            ]
        );
        assert_eq!(lines[4], "Vary: Access-Control-Request-Headers");
        assert!(lines.contains(&"Content-Length: 2"));
        assert!(lines.iter().any(|l| l.starts_with("Date: ")));
    }

    #[test]
    fn head_keeps_content_length_but_drops_body() {
        let r = Response::new(200).with_body(b"hello".to_vec());
        let s = text(&serialize(
            &r,
            WriteOptions {
                head_only: true,
                ..Default::default()
            },
        ));
        assert!(s.contains("Content-Length: 5\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
        // 핸들러가 정한 Content-Length(GET이었다면 나갈 길이)를 유지한다.
        let r = Response::new(200).with_header("Content-Length", "42");
        let s = text(&serialize(
            &r,
            WriteOptions {
                head_only: true,
                ..Default::default()
            },
        ));
        assert!(s.contains("Content-Length: 42\r\n"));
    }

    #[test]
    fn handler_content_length_is_corrected_for_non_head() {
        let r = Response::new(200)
            .with_header("content-length", "999")
            .with_body(b"abc".to_vec());
        let s = text(&serialize(&r, WriteOptions::default()));
        assert_eq!(s.matches("ontent-").count(), 1);
        assert!(s.contains("Content-Length: 3\r\n"));
    }

    #[test]
    fn no_body_statuses() {
        for status in [204u16, 304] {
            let r = Response::new(status).with_body(b"x".to_vec());
            let s = text(&serialize(&r, WriteOptions::default()));
            assert!(!s.contains("Content-Length"), "{status}");
            assert!(s.ends_with("\r\n\r\n"), "{status}");
        }
    }

    #[test]
    fn connection_headers() {
        let r = Response::new(200);
        let s = text(&serialize(
            &r,
            WriteOptions {
                close: true,
                ..Default::default()
            },
        ));
        assert!(s.contains("Connection: close\r\n"));
        let s = text(&serialize(
            &r,
            WriteOptions {
                keep_alive_header: true,
                ..Default::default()
            },
        ));
        assert!(s.contains("Connection: keep-alive\r\n"));
        let s = text(&serialize(&r, WriteOptions::default()));
        assert!(!s.contains("Connection"));
    }

    #[test]
    fn header_injection_is_neutralised() {
        let r = Response::new(200).with_header("X-A", "a\r\nSet-Cookie: x=1");
        let s = text(&serialize(&r, WriteOptions::default()));
        assert!(!s.contains("\r\nSet-Cookie"));
    }

    #[test]
    fn unknown_status_has_empty_reason() {
        let s = text(&serialize(&Response::new(299), WriteOptions::default()));
        assert!(s.starts_with("HTTP/1.1 299 \r\n"));
    }
}
