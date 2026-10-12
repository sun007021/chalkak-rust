//! HTTP/1.1 요청 파서. 직접 구현하며 `Read`만 요구하므로 소켓 없이 바이트열로 시험할 수 있다.
//!
//! 두 단계로 나눈다: [`Conn::read_head`]가 요청 줄·헤더를 읽고, 호출 쪽이 `Expect: 100-continue`에
//! 응답한 뒤 [`Conn::read_body`]가 본문을 읽는다. 버퍼에 남은 바이트는 다음 요청(파이프라인)에 쓰인다.
//!
//! 엄격하게 거절하는 것(요청 밀수 방지): 줄 끝은 CRLF만, 헤더 줄 접기(obs-fold) 금지, 제어 문자·NUL 금지,
//! `Content-Length` 중복·`Content-Length`+`Transfer-Encoding` 동시 사용 금지.

use std::io::{self, Read};

use super::headers::Headers;
use super::limits::Limits;
use super::method::{Method, is_token_byte};
use super::query::{decode_path, parse_query};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    Http10,
    Http11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyKind {
    None,
    Length(usize),
    Chunked,
}

/// 요청 줄과 헤더를 해석한 결과.
#[derive(Debug)]
pub(crate) struct Head {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Headers,
    pub version: Version,
    pub body: BodyKind,
    /// 응답 뒤에도 연결을 유지할 수 있는가(`Connection` 헤더와 HTTP 버전 규칙).
    pub keep_alive: bool,
    /// 본문을 받기 전에 `100 Continue`를 보내야 하는가.
    pub expects_continue: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseError {
    BadRequest(&'static str),
    UriTooLong,
    HeaderTooLarge,
    BodyTooLarge,
    VersionNotSupported,
    NotImplemented(&'static str),
    ExpectationFailed,
    /// 요청을 읽는 도중 시간이 지났다.
    Timeout,
    /// 읽기 오류(상대가 중간에 끊음 포함). 응답할 수 없다.
    Io(io::ErrorKind),
}

impl ParseError {
    /// 돌려줄 상태 코드. 응답할 수 없는 오류는 `None`.
    pub fn status(&self) -> Option<u16> {
        Some(match self {
            ParseError::BadRequest(_) => 400,
            ParseError::UriTooLong => 414,
            ParseError::HeaderTooLarge => 431,
            ParseError::BodyTooLarge => 413,
            ParseError::VersionNotSupported => 505,
            ParseError::NotImplemented(_) => 501,
            ParseError::ExpectationFailed => 417,
            ParseError::Timeout => 408,
            ParseError::Io(_) => return None,
        })
    }
}

/// 읽기 버퍼를 가진 연결 쪽 파서.
pub(crate) struct Conn<R> {
    reader: R,
    buf: Vec<u8>,
    start: usize,
}

enum Fill {
    Data,
    Eof,
}

impl<R: Read> Conn<R> {
    pub fn new(reader: R) -> Self {
        Conn {
            reader,
            buf: Vec::new(),
            start: 0,
        }
    }

    pub fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// 아직 해석하지 않은 바이트가 있는가(파이프라인된 다음 요청 등).
    pub fn has_buffered(&self) -> bool {
        self.start < self.buf.len()
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    fn fill(&mut self) -> Result<Fill, ParseError> {
        let mut chunk = [0u8; 8192];
        loop {
            match self.reader.read(&mut chunk) {
                Ok(0) => return Ok(Fill::Eof),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(Fill::Data);
                }
                Err(e) => match e.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                        return Err(ParseError::Timeout);
                    }
                    k => return Err(ParseError::Io(k)),
                },
            }
        }
    }

    /// 요청 줄과 헤더를 읽는다. 요청 사이에 연결이 닫히거나 유휴 시간이 지났으면 `Ok(None)`.
    pub fn read_head(&mut self, limits: &Limits) -> Result<Option<Head>, ParseError> {
        loop {
            // 요청 앞의 빈 줄은 무시한다(RFC 9112 2.2).
            while self.avail().starts_with(b"\r\n") {
                self.consume(2);
            }
            let avail = self.avail();
            // 요청 줄 하나만으로 상한을 넘으면 414, 헤더까지 합쳐 넘으면 431.
            let line_len = find(avail, b"\r\n").unwrap_or(avail.len());
            if line_len + 2 > limits.max_header_bytes {
                return Err(ParseError::UriTooLong);
            }
            if let Some(pos) = find(avail, b"\r\n\r\n") {
                let end = pos + 4;
                if end > limits.max_header_bytes {
                    return Err(ParseError::HeaderTooLarge);
                }
                let head = parse_head(&avail[..pos], limits)?;
                self.consume(end);
                return Ok(Some(head));
            }
            if avail.len() >= limits.max_header_bytes {
                return Err(ParseError::HeaderTooLarge);
            }
            let empty = avail.is_empty();
            match self.fill() {
                Ok(Fill::Data) => {}
                Ok(Fill::Eof) if empty => return Ok(None),
                Ok(Fill::Eof) => return Err(ParseError::Io(io::ErrorKind::UnexpectedEof)),
                Err(ParseError::Timeout) if empty => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// 본문을 읽어 디코딩한 바이트를 돌려준다. `Content-Length`가 한도를 넘는지는 `read_head`가 이미 걸렀다.
    pub fn read_body(&mut self, head: &Head, limits: &Limits) -> Result<Vec<u8>, ParseError> {
        match head.body {
            BodyKind::None => Ok(Vec::new()),
            BodyKind::Length(n) => self.read_exact_vec(n),
            BodyKind::Chunked => self.read_chunked(limits),
        }
    }

    fn read_exact_vec(&mut self, n: usize) -> Result<Vec<u8>, ParseError> {
        while self.avail().len() < n {
            match self.fill()? {
                Fill::Data => {}
                Fill::Eof => return Err(ParseError::Io(io::ErrorKind::UnexpectedEof)),
            }
        }
        let out = self.avail()[..n].to_vec();
        self.consume(n);
        Ok(out)
    }

    /// CRLF로 끝나는 한 줄(CRLF 제외)을 읽는다. 맨 LF나 `max`를 넘는 줄은 거절한다.
    fn read_line(&mut self, max: usize) -> Result<Vec<u8>, ParseError> {
        loop {
            let avail = self.avail();
            if let Some(nl) = avail.iter().position(|&b| b == b'\n') {
                if nl == 0 || avail[nl - 1] != b'\r' {
                    return Err(ParseError::BadRequest("bare LF in chunked framing"));
                }
                if nl - 1 > max {
                    return Err(ParseError::BadRequest("chunk line too long"));
                }
                let line = avail[..nl - 1].to_vec();
                self.consume(nl + 1);
                return Ok(line);
            }
            if avail.len() > max {
                return Err(ParseError::BadRequest("chunk line too long"));
            }
            match self.fill()? {
                Fill::Data => {}
                Fill::Eof => return Err(ParseError::Io(io::ErrorKind::UnexpectedEof)),
            }
        }
    }

    fn read_chunked(&mut self, limits: &Limits) -> Result<Vec<u8>, ParseError> {
        let mut body: Vec<u8> = Vec::new();
        loop {
            let line = self.read_line(limits.max_header_bytes.min(8192))?;
            let size = parse_chunk_size_line(&line)?;
            if size == 0 {
                self.skip_trailers(limits)?;
                return Ok(body);
            }
            let size = usize::try_from(size).map_err(|_| ParseError::BodyTooLarge)?;
            if body
                .len()
                .checked_add(size)
                .is_none_or(|t| t > limits.max_body_bytes)
            {
                return Err(ParseError::BodyTooLarge);
            }
            let data = self.read_exact_vec(size)?;
            body.extend_from_slice(&data);
            let crlf = self.read_exact_vec(2)?;
            if crlf != b"\r\n" {
                return Err(ParseError::BadRequest("missing CRLF after chunk data"));
            }
        }
    }

    /// 마지막 청크 뒤의 트레일러를 문법만 확인하고 버린다(개수·크기는 헤더 한도를 따른다).
    fn skip_trailers(&mut self, limits: &Limits) -> Result<(), ParseError> {
        let mut count = 0usize;
        let mut bytes = 0usize;
        loop {
            let line = self.read_line(limits.max_header_bytes)?;
            if line.is_empty() {
                return Ok(());
            }
            count += 1;
            bytes += line.len() + 2;
            if count > limits.max_headers || bytes > limits.max_header_bytes {
                return Err(ParseError::HeaderTooLarge);
            }
            parse_header_line(&line)?;
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `1a;ext=val;other` → 0x1a. 확장은 문자만 확인하고 무시한다.
fn parse_chunk_size_line(line: &[u8]) -> Result<u64, ParseError> {
    let (size, ext) = match line.iter().position(|&b| b == b';') {
        Some(i) => (&line[..i], &line[i..]),
        None => (line, &line[..0]),
    };
    // 크기와 `;` 사이의 공백은 관용한다(RFC 9112 7.1.1의 BWS).
    let size = trim_ows(size);
    if size.is_empty() || size.len() > 16 || !size.iter().all(u8::is_ascii_hexdigit) {
        return Err(ParseError::BadRequest("invalid chunk size"));
    }
    if ext
        .iter()
        .any(|&b| b == 0 || (b < 0x20 && b != b'\t') || b == 0x7f)
    {
        return Err(ParseError::BadRequest("invalid chunk extension"));
    }
    // 16자리 16진수는 u64에 항상 들어간다.
    let s = std::str::from_utf8(size).map_err(|_| ParseError::BadRequest("invalid chunk size"))?;
    u64::from_str_radix(s, 16).map_err(|_| ParseError::BadRequest("invalid chunk size"))
}

fn trim_ows(mut b: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = b {
        if *first == b' ' || *first == b'\t' {
            b = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = b {
        if *last == b' ' || *last == b'\t' {
            b = rest;
        } else {
            break;
        }
    }
    b
}

/// Latin-1(바이트 하나가 문자 하나)로 해석한다. Tomcat도 헤더를 ISO-8859-1로 다룬다.
fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn parse_header_line(line: &[u8]) -> Result<(String, String), ParseError> {
    if matches!(line.first(), Some(b' ' | b'\t')) {
        return Err(ParseError::BadRequest("obsolete line folding"));
    }
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(ParseError::BadRequest("header without colon"))?;
    let name = &line[..colon];
    if name.is_empty() || !name.iter().copied().all(is_token_byte) {
        return Err(ParseError::BadRequest("invalid header name"));
    }
    let value = trim_ows(&line[colon + 1..]);
    if value
        .iter()
        .any(|&b| b == 0 || (b < 0x20 && b != b'\t') || b == 0x7f)
    {
        return Err(ParseError::BadRequest("invalid header value"));
    }
    Ok((latin1(name), latin1(value)))
}

/// 요청 줄 + 헤더(끝의 빈 줄 제외)를 해석한다.
fn parse_head(raw: &[u8], limits: &Limits) -> Result<Head, ParseError> {
    // 맨 CR·맨 LF가 섞이면 줄 경계 해석이 프록시마다 달라진다. 모두 거절한다.
    for (i, &b) in raw.iter().enumerate() {
        let ok = match b {
            b'\n' => i > 0 && raw[i - 1] == b'\r',
            b'\r' => raw.get(i + 1) == Some(&b'\n'),
            _ => true,
        };
        if !ok {
            return Err(ParseError::BadRequest("bare CR or LF"));
        }
    }
    let mut lines = raw.split(|&b| b == b'\n');
    // 요청 줄을 뺀 헤더 줄 개수 = 개행 수
    if raw.iter().filter(|&&b| b == b'\n').count() > limits.max_headers {
        return Err(ParseError::HeaderTooLarge);
    }

    let request_line = lines.next().unwrap_or_default();
    let request_line = request_line.strip_suffix(b"\r").unwrap_or(request_line);
    let (method, path, query, version) = parse_request_line(request_line)?;

    let mut headers = Headers::new();
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let (n, v) = parse_header_line(line)?;
        headers.append(n, v);
    }
    let info = analyze(&headers, version, limits)?;
    Ok(Head {
        method,
        path,
        query,
        headers,
        version,
        body: info.body,
        keep_alive: info.keep_alive,
        expects_continue: info.expects_continue,
    })
}

type RequestLine = (Method, String, Vec<(String, String)>, Version);

fn parse_request_line(line: &[u8]) -> Result<RequestLine, ParseError> {
    if line
        .iter()
        .any(|&b| b == 0 || b < 0x20 && b != b' ' || b >= 0x7f)
    {
        return Err(ParseError::BadRequest("invalid byte in request line"));
    }
    // 위에서 ASCII만 통과시켰으므로 UTF-8 변환은 실패하지 않는다.
    let line =
        std::str::from_utf8(line).map_err(|_| ParseError::BadRequest("invalid request line"))?;
    let mut parts = line.split(' ');
    let (Some(m), Some(target), Some(ver), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ParseError::BadRequest("malformed request line"));
    };
    let method = Method::parse(m).ok_or(ParseError::BadRequest("invalid method"))?;
    let version = match ver {
        "HTTP/1.1" => Version::Http11,
        "HTTP/1.0" => Version::Http10,
        v if is_http_version(v) => return Err(ParseError::VersionNotSupported),
        _ => return Err(ParseError::BadRequest("invalid HTTP version")),
    };
    let (path, query) = parse_target(&method, target)?;
    Ok((method, path, query, version))
}

fn is_http_version(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 8
        && v.starts_with("HTTP/")
        && b[5].is_ascii_digit()
        && b[6] == b'.'
        && b[7].is_ascii_digit()
}

fn parse_target(
    method: &Method,
    target: &str,
) -> Result<(String, Vec<(String, String)>), ParseError> {
    if target.is_empty() || target.contains('#') {
        return Err(ParseError::BadRequest("invalid request target"));
    }
    let origin_form = if target.starts_with('/') {
        target
    } else if target == "*" && *method == Method::Options {
        return Ok(("*".to_string(), Vec::new()));
    } else if let Some(rest) = strip_scheme(target) {
        // absolute-form: 호스트는 버리고 경로·쿼리만 쓴다.
        match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => &rest[i..],
            Some(i) => &rest[i..], // "?query" 만 있는 경우는 아래에서 "/"를 붙인다
            None => "/",
        }
    } else {
        return Err(ParseError::BadRequest("unsupported request target form"));
    };
    let (raw_path, raw_query) = match origin_form.split_once('?') {
        Some((p, q)) => (p, q),
        None => (origin_form, ""),
    };
    let raw_path = if raw_path.is_empty() { "/" } else { raw_path };
    let path =
        decode_path(raw_path).ok_or(ParseError::BadRequest("invalid percent-encoding in path"))?;
    Ok((path, parse_query(raw_query)))
}

fn strip_scheme(target: &str) -> Option<&str> {
    let lower_prefix = |p: &str| {
        target
            .get(..p.len())
            .filter(|s| s.eq_ignore_ascii_case(p))
            .map(|_| &target[p.len()..])
    };
    lower_prefix("http://").or_else(|| lower_prefix("https://"))
}

struct Semantics {
    body: BodyKind,
    keep_alive: bool,
    expects_continue: bool,
}

/// 헤더 의미 검사: Host, 본문 길이 규칙, Connection, Expect.
fn analyze(headers: &Headers, version: Version, limits: &Limits) -> Result<Semantics, ParseError> {
    let hosts = headers.get_all("host").count();
    if hosts > 1 || (version == Version::Http11 && hosts == 0) {
        return Err(ParseError::BadRequest("missing or duplicate Host"));
    }

    let cl_count = headers.get_all("content-length").count();
    let te_count = headers.get_all("transfer-encoding").count();
    if cl_count > 0 && te_count > 0 {
        return Err(ParseError::BadRequest(
            "both Content-Length and Transfer-Encoding",
        ));
    }
    let body = if te_count > 0 {
        if version == Version::Http10 {
            return Err(ParseError::BadRequest("Transfer-Encoding in HTTP/1.0"));
        }
        // 코딩 목록의 마지막이 chunked여야 하고 chunked 외 코딩은 지원하지 않는다.
        let codings: Vec<String> = headers
            .get_all("transfer-encoding")
            .flat_map(|v| v.split(','))
            .map(|t| t.trim().to_ascii_lowercase())
            .collect();
        if codings.len() == 1 && codings[0] == "chunked" {
            BodyKind::Chunked
        } else if codings.iter().any(|c| c.is_empty()) {
            return Err(ParseError::BadRequest("empty transfer coding"));
        } else {
            return Err(ParseError::NotImplemented("unsupported Transfer-Encoding"));
        }
    } else if cl_count > 0 {
        if cl_count > 1 {
            return Err(ParseError::BadRequest("duplicate Content-Length"));
        }
        let v = headers.get("content-length").unwrap_or_default();
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseError::BadRequest("invalid Content-Length"));
        }
        let n: usize = v
            .parse()
            .map_err(|_| ParseError::BadRequest("invalid Content-Length"))?;
        if n > limits.max_body_bytes {
            return Err(ParseError::BodyTooLarge);
        }
        if n == 0 {
            BodyKind::None
        } else {
            BodyKind::Length(n)
        }
    } else {
        BodyKind::None
    };

    let keep_alive = match version {
        Version::Http11 => !headers.has_token("connection", "close"),
        Version::Http10 => {
            headers.has_token("connection", "keep-alive")
                && !headers.has_token("connection", "close")
        }
    };

    let mut expects_continue = false;
    for v in headers.get_all("expect") {
        if v.trim().eq_ignore_ascii_case("100-continue") {
            // HTTP/1.0 요청의 Expect는 무시한다(RFC 9110 10.1.1).
            expects_continue = version == Version::Http11 && body != BodyKind::None;
        } else {
            return Err(ParseError::ExpectationFailed);
        }
    }

    Ok(Semantics {
        body,
        keep_alive,
        expects_continue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn limits() -> Limits {
        Limits::default()
    }

    fn conn(bytes: &[u8]) -> Conn<Cursor<Vec<u8>>> {
        Conn::new(Cursor::new(bytes.to_vec()))
    }

    fn head(bytes: &str) -> Result<Head, ParseError> {
        conn(bytes.as_bytes())
            .read_head(&limits())
            .map(|h| h.expect("head"))
    }

    fn bad(bytes: &str) -> ParseError {
        head(bytes).expect_err("should fail")
    }

    fn body_of(bytes: &str) -> Result<Vec<u8>, ParseError> {
        let mut c = conn(bytes.as_bytes());
        let h = c.read_head(&limits())?.expect("head");
        c.read_body(&h, &limits())
    }

    // --- 요청 줄 ---

    #[test]
    fn simple_get_with_query() {
        let h = head("GET /api/v1/posts?page=2&q=a+b%20c HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.path, "/api/v1/posts");
        assert_eq!(
            h.query,
            [("page".into(), "2".into()), ("q".into(), "a b c".into())]
        );
        assert_eq!(h.version, Version::Http11);
        assert_eq!(h.body, BodyKind::None);
        assert!(h.keep_alive);
    }

    #[test]
    fn trailing_slash_and_case_are_preserved() {
        // platform__unknown-path.json: 끝 슬래시·대소문자는 다른 경로다.
        assert_eq!(
            head("GET /a/ HTTP/1.1\r\nHost: x\r\n\r\n").unwrap().path,
            "/a/"
        );
        assert_eq!(
            head("GET /A HTTP/1.1\r\nHost: x\r\n\r\n").unwrap().path,
            "/A"
        );
    }

    #[test]
    fn malformed_request_lines_are_400() {
        for line in [
            "GET\r\nHost: x\r\n\r\n",
            "GET /\r\nHost: x\r\n\r\n",
            "GET  / HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET / HTTP/1.1 extra\r\nHost: x\r\n\r\n",
            "G ET / HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET / HTTP/1.1\t\r\nHost: x\r\n\r\n",
            "GET /a b HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET /\u{0} HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET /가 HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET /#frag HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET foo HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET / http/1.1\r\nHost: x\r\n\r\n",
            "GET / HTTP/one\r\nHost: x\r\n\r\n",
            "GET /%zz HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET /a%2Fb HTTP/1.1\r\nHost: x\r\n\r\n",
        ] {
            assert!(matches!(bad(line), ParseError::BadRequest(_)), "{line:?}");
        }
    }

    #[test]
    fn unsupported_http_version_is_505() {
        assert_eq!(
            bad("GET / HTTP/2.0\r\nHost: x\r\n\r\n"),
            ParseError::VersionNotSupported
        );
        assert_eq!(
            bad("GET / HTTP/0.9\r\nHost: x\r\n\r\n"),
            ParseError::VersionNotSupported
        );
    }

    #[test]
    fn absolute_form_and_asterisk() {
        let h =
            head("GET http://example.com/a/b?x=1 HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert_eq!(h.path, "/a/b");
        assert_eq!(h.query, [("x".into(), "1".into())]);
        let h = head("GET http://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert_eq!(h.path, "/");
        let h = head("OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(h.path, "*");
        assert!(matches!(
            bad("GET * HTTP/1.1\r\nHost: x\r\n\r\n"),
            ParseError::BadRequest(_)
        ));
    }

    #[test]
    fn leading_blank_lines_are_ignored() {
        let h = head("\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(h.path, "/");
    }

    #[test]
    fn unknown_method_token_is_kept() {
        let h = head("PURGE / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(h.method, Method::Other("PURGE".into()));
    }

    // --- 헤더 ---

    #[test]
    fn header_values_are_trimmed_and_order_kept() {
        let h = head("GET / HTTP/1.1\r\nHost: x\r\nX-A:   1 \t\r\nX-B:\r\nx-a: 2\r\n\r\n").unwrap();
        let v: Vec<_> = h.headers.iter().collect();
        assert_eq!(v, [("Host", "x"), ("X-A", "1"), ("X-B", ""), ("x-a", "2")]);
    }

    #[test]
    fn malformed_headers_are_400() {
        for raw in [
            "GET / HTTP/1.1\r\nHost: x\r\nNoColon\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\n: v\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nBad Name: v\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nName : v\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\n folded\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nA: b\u{0}c\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nA: b\x01c\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\nA: b\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\rA: b\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: x\r\nA: b\n\r\n\r\n",
        ] {
            assert!(matches!(bad(raw), ParseError::BadRequest(_)), "{raw:?}");
        }
    }

    #[test]
    fn host_rules() {
        assert!(matches!(
            bad("GET / HTTP/1.1\r\n\r\n"),
            ParseError::BadRequest(_)
        ));
        assert!(matches!(
            bad("GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n"),
            ParseError::BadRequest(_)
        ));
        // HTTP/1.0은 Host 없이 허용
        assert!(head("GET / HTTP/1.0\r\n\r\n").is_ok());
    }

    #[test]
    fn header_size_limit_is_431_and_long_request_line_is_414() {
        let l = Limits {
            max_header_bytes: 64,
            ..limits()
        };
        let big = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nX: {}\r\n\r\n",
            "a".repeat(100)
        );
        let r = Conn::new(Cursor::new(big.into_bytes())).read_head(&l);
        assert_eq!(r.unwrap_err(), ParseError::HeaderTooLarge);

        let long_line = format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(100));
        let r = Conn::new(Cursor::new(long_line.into_bytes())).read_head(&l);
        assert_eq!(r.unwrap_err(), ParseError::UriTooLong);

        // 정확히 상한이면 통과한다.
        let exact = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let l = Limits {
            max_header_bytes: exact.len(),
            ..limits()
        };
        assert!(
            Conn::new(Cursor::new(exact.as_bytes().to_vec()))
                .read_head(&l)
                .is_ok()
        );
        let l = Limits {
            max_header_bytes: exact.len() - 1,
            ..limits()
        };
        assert_eq!(
            Conn::new(Cursor::new(exact.as_bytes().to_vec()))
                .read_head(&l)
                .unwrap_err(),
            ParseError::HeaderTooLarge
        );
    }

    #[test]
    fn header_count_limit_is_431() {
        let l = Limits {
            max_headers: 3,
            ..limits()
        };
        let ok = "GET / HTTP/1.1\r\nHost: x\r\nA: 1\r\nB: 2\r\n\r\n";
        assert!(
            Conn::new(Cursor::new(ok.as_bytes().to_vec()))
                .read_head(&l)
                .is_ok()
        );
        let over = "GET / HTTP/1.1\r\nHost: x\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n";
        assert_eq!(
            Conn::new(Cursor::new(over.as_bytes().to_vec()))
                .read_head(&l)
                .unwrap_err(),
            ParseError::HeaderTooLarge
        );
    }

    // --- 본문 길이 규칙 ---

    #[test]
    fn content_length_body() {
        let b = body_of("POST /p HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(b, b"hello");
        let b = body_of("POST /p HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n").unwrap();
        assert!(b.is_empty());
    }

    #[test]
    fn invalid_or_duplicate_content_length_is_400() {
        for cl in [
            "Content-Length: abc\r\n",
            "Content-Length: -1\r\n",
            "Content-Length: +5\r\n",
            "Content-Length: 5 5\r\n",
            "Content-Length: 5, 5\r\n",
            "Content-Length:\r\n",
            "Content-Length: 99999999999999999999999\r\n",
            "Content-Length: 5\r\nContent-Length: 5\r\n",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
        ] {
            let raw = format!("POST / HTTP/1.1\r\nHost: x\r\n{cl}\r\nhello");
            assert!(matches!(bad(&raw), ParseError::BadRequest(_)), "{cl:?}");
        }
    }

    #[test]
    fn content_length_and_transfer_encoding_together_is_400() {
        let raw =
            "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(matches!(bad(raw), ParseError::BadRequest(_)));
    }

    #[test]
    fn transfer_encoding_rules() {
        let h = head("POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: Chunked\r\n\r\n").unwrap();
        assert_eq!(h.body, BodyKind::Chunked);
        assert_eq!(
            bad("POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"),
            ParseError::NotImplemented("unsupported Transfer-Encoding")
        );
        assert_eq!(
            bad("POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\n\r\n"),
            ParseError::NotImplemented("unsupported Transfer-Encoding")
        );
        assert!(matches!(
            bad("POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n"),
            ParseError::BadRequest(_)
        ));
    }

    #[test]
    fn content_length_over_limit_is_413_before_body_is_read() {
        let l = Limits {
            max_body_bytes: 4,
            ..limits()
        };
        let raw = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n";
        assert_eq!(
            Conn::new(Cursor::new(raw.as_bytes().to_vec()))
                .read_head(&l)
                .unwrap_err(),
            ParseError::BodyTooLarge
        );
    }

    #[test]
    fn truncated_body_is_an_io_error() {
        let r = body_of("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nhello");
        assert_eq!(r.unwrap_err(), ParseError::Io(io::ErrorKind::UnexpectedEof));
    }

    // --- chunked ---

    #[test]
    fn chunked_body_with_extensions_and_trailers() {
        let raw = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
                   5;name=val\r\nhello\r\n6 ; a=\"b c\" ;flag\r\n world\r\n0;last\r\nX-Trailer: 1\r\nX-T2: 2\r\n\r\n";
        assert_eq!(body_of(raw).unwrap(), b"hello world");
    }

    #[test]
    fn chunked_hex_sizes_are_case_insensitive() {
        let raw = format!(
            "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nA\r\n{}\r\na\r\n{}\r\n0\r\n\r\n",
            "x".repeat(10),
            "y".repeat(10)
        );
        assert_eq!(body_of(&raw).unwrap().len(), 20);
    }

    #[test]
    fn chunked_malformed_is_400() {
        let pre = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";
        for tail in [
            "zz\r\nab\r\n0\r\n\r\n",           // 16진수 아님
            "\r\nab\r\n0\r\n\r\n",             // 크기 없음
            "-5\r\nhello\r\n0\r\n\r\n",        // 음수
            "0x5\r\nhello\r\n0\r\n\r\n",       // 0x 접두
            "+5\r\nhello\r\n0\r\n\r\n",        // 부호
            "5\r\nhelloXX0\r\n\r\n",           // 데이터 뒤 CRLF 없음
            "5\nhello\r\n0\r\n\r\n",           // 맨 LF
            "10000000000000000\r\n",           // 17자리
            "5;a\u{0}b\r\nhello\r\n0\r\n\r\n", // 확장에 NUL
            "0\r\nBad Trailer\r\n\r\n",        // 트레일러에 콜론 없음
            "0\r\n folded: x\r\n\r\n",         // 트레일러 줄 접기
        ] {
            let raw = format!("{pre}{tail}");
            let r = body_of(&raw);
            assert!(
                matches!(r, Err(ParseError::BadRequest(_))),
                "{tail:?} -> {r:?}"
            );
        }
    }

    #[test]
    fn chunked_size_overflowing_u64_cannot_happen_but_huge_is_413() {
        let pre = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";
        let r = body_of(&format!("{pre}FFFFFFFFFFFFFFFF\r\n"));
        assert_eq!(r.unwrap_err(), ParseError::BodyTooLarge);
    }

    #[test]
    fn chunked_total_over_limit_is_413() {
        let l = Limits {
            max_body_bytes: 8,
            ..limits()
        };
        let raw = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        let mut c = Conn::new(Cursor::new(raw.as_bytes().to_vec()));
        let h = c.read_head(&l).unwrap().unwrap();
        assert_eq!(c.read_body(&h, &l).unwrap_err(), ParseError::BodyTooLarge);
    }

    #[test]
    fn chunked_trailers_over_header_limit_is_431() {
        let l = Limits {
            max_headers: 2,
            ..limits()
        };
        let raw = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n";
        let mut c = Conn::new(Cursor::new(raw.as_bytes().to_vec()));
        let h = c.read_head(&l).unwrap().unwrap();
        assert_eq!(c.read_body(&h, &l).unwrap_err(), ParseError::HeaderTooLarge);
    }

    // --- keep-alive / Expect ---

    #[test]
    fn keep_alive_rules() {
        let ka = |raw: &str| head(raw).unwrap().keep_alive;
        assert!(ka("GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(!ka(
            "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        ));
        assert!(!ka(
            "GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, Close\r\n\r\n"
        ));
        assert!(!ka("GET / HTTP/1.0\r\n\r\n"));
        assert!(ka("GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"));
        assert!(ka("GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n"));
        assert!(!ka(
            "GET / HTTP/1.0\r\nConnection: keep-alive, close\r\n\r\n"
        ));
    }

    #[test]
    fn expect_100_continue() {
        let h =
            head("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nExpect: 100-Continue\r\n\r\n")
                .unwrap();
        assert!(h.expects_continue);
        let h = head("POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n").unwrap();
        assert!(h.expects_continue);
        // 본문이 없으면 100을 보낼 필요가 없다.
        let h =
            head("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nExpect: 100-continue\r\n\r\n")
                .unwrap();
        assert!(!h.expects_continue);
        // HTTP/1.0의 Expect는 무시한다.
        let h =
            head("POST / HTTP/1.0\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n").unwrap();
        assert!(!h.expects_continue);
        assert_eq!(
            bad("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nExpect: foo\r\n\r\n"),
            ParseError::ExpectationFailed
        );
    }

    // --- 파이프라인 / EOF / 타임아웃 ---

    #[test]
    fn pipelined_requests_share_the_buffer() {
        let raw = "POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabcGET /b?x=1 HTTP/1.1\r\nHost: x\r\n\r\n\
                   POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nzz\r\n0\r\n\r\n";
        let mut c = conn(raw.as_bytes());
        let l = limits();
        let h1 = c.read_head(&l).unwrap().unwrap();
        assert_eq!(
            (h1.path.as_str(), c.read_body(&h1, &l).unwrap()),
            ("/a", b"abc".to_vec())
        );
        let h2 = c.read_head(&l).unwrap().unwrap();
        assert_eq!(h2.path, "/b");
        assert!(c.read_body(&h2, &l).unwrap().is_empty());
        let h3 = c.read_head(&l).unwrap().unwrap();
        assert_eq!(
            (h3.path.as_str(), c.read_body(&h3, &l).unwrap()),
            ("/c", b"zz".to_vec())
        );
        assert!(!c.has_buffered());
        assert!(c.read_head(&l).unwrap().is_none());
    }

    #[test]
    fn clean_eof_is_none_and_partial_head_is_an_error() {
        assert!(conn(b"").read_head(&limits()).unwrap().is_none());
        assert!(conn(b"\r\n").read_head(&limits()).unwrap().is_none());
        assert_eq!(
            conn(b"GET / HTTP/1.1\r\nHo")
                .read_head(&limits())
                .unwrap_err(),
            ParseError::Io(io::ErrorKind::UnexpectedEof)
        );
    }

    /// 바이트를 한 번에 하나씩 주고 마지막에 타임아웃을 낸다.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
    }
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos < self.data.len() {
                buf[0] = self.data[self.pos];
                self.pos += 1;
                Ok(1)
            } else {
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }

    #[test]
    fn byte_at_a_time_reads_work() {
        let data = b"POST /t HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3;e\r\nabc\r\n0\r\n\r\n".to_vec();
        let mut c = Conn::new(Trickle { data, pos: 0 });
        let l = limits();
        let h = c.read_head(&l).unwrap().unwrap();
        assert_eq!(c.read_body(&h, &l).unwrap(), b"abc");
    }

    #[test]
    fn timeout_is_silent_when_idle_and_408_mid_request() {
        let l = limits();
        let mut c = Conn::new(Trickle {
            data: Vec::new(),
            pos: 0,
        });
        assert!(c.read_head(&l).unwrap().is_none());
        let mut c = Conn::new(Trickle {
            data: b"GET / HT".to_vec(),
            pos: 0,
        });
        assert_eq!(c.read_head(&l).unwrap_err(), ParseError::Timeout);
        assert_eq!(ParseError::Timeout.status(), Some(408));
        let mut c = Conn::new(Trickle {
            data: b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\n\r\nabc".to_vec(),
            pos: 0,
        });
        let h = c.read_head(&l).unwrap().unwrap();
        assert_eq!(c.read_body(&h, &l).unwrap_err(), ParseError::Timeout);
    }

    #[test]
    fn error_statuses() {
        assert_eq!(ParseError::BadRequest("x").status(), Some(400));
        assert_eq!(ParseError::UriTooLong.status(), Some(414));
        assert_eq!(ParseError::HeaderTooLarge.status(), Some(431));
        assert_eq!(ParseError::BodyTooLarge.status(), Some(413));
        assert_eq!(ParseError::VersionNotSupported.status(), Some(505));
        assert_eq!(ParseError::NotImplemented("x").status(), Some(501));
        assert_eq!(ParseError::ExpectationFailed.status(), Some(417));
        assert_eq!(ParseError::Io(io::ErrorKind::BrokenPipe).status(), None);
    }
}
