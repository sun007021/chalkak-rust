//! 가짜 서버용 최소 HTTP/1.1 서버 계층. 요청 하나를 읽고 응답 하나를 쓴다.
//! Content-Length와 chunked 본문을 읽고, `Expect: 100-continue`에 응답하며, keep-alive를 지원한다.

use std::io::{BufRead, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// 쿼리를 제외한 경로.
    pub path: String,
    pub query: String,
    /// 이름은 소문자로 바꿔 둔다.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 응답 뒤 연결을 닫아야 하는지.
    pub close: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// HEAD 응답처럼 본문 없이 길이만 알릴 때 쓴다.
    pub content_length: Option<usize>,
    /// 응답을 보내기 전에 기다릴 시간(ms). 호출한 쪽의 시간 제한 시험용이다. 서버 상태 잠금을 놓은 뒤에 기다린다.
    pub delay_ms: u64,
    /// 롱 폴링처럼 메시지가 올 때까지 기다릴 시간(ms). 0이면 기다리지 않는다. 서버 상태 잠금을 놓고 기다린다.
    pub long_poll_ms: u64,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
            content_length: None,
            delay_ms: 0,
            long_poll_ms: 0,
        }
    }

    pub fn json(status: u16, body: &str) -> Self {
        Self::new(status, "application/json", body)
    }
}

/// 연결에서 요청 하나를 읽는다. 상대가 연결을 닫았으면 `Ok(None)`.
pub fn read_request<S: BufRead + Write>(stream: &mut S) -> Result<Option<Request>, String> {
    let mut line = String::new();
    if stream.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("잘못된 요청 줄: {line:?}"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (method, path, query) = (method.to_string(), path.to_string(), query.to_string());
    let http10 = version.eq_ignore_ascii_case("HTTP/1.0");

    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if stream.read_line(&mut h).map_err(|e| e.to_string())? == 0 {
            return Err("헤더 도중 연결이 끊겼습니다".into());
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    if find("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .and_then(|()| stream.flush())
            .map_err(|e| e.to_string())?;
    }

    let mut body = Vec::new();
    if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        loop {
            let mut size_line = String::new();
            stream
                .read_line(&mut size_line)
                .map_err(|e| e.to_string())?;
            let size_hex = size_line.trim().split(';').next().unwrap_or("0");
            let size = usize::from_str_radix(size_hex, 16)
                .map_err(|_| format!("잘못된 chunk 크기: {size_line:?}"))?;
            if size == 0 {
                loop {
                    let mut t = String::new();
                    if stream.read_line(&mut t).map_err(|e| e.to_string())? == 0
                        || t.trim().is_empty()
                    {
                        break;
                    }
                }
                break;
            }
            let mut chunk = vec![0; size];
            stream.read_exact(&mut chunk).map_err(|e| e.to_string())?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0; 2];
            stream.read_exact(&mut crlf).map_err(|e| e.to_string())?;
        }
    } else if let Some(len) = find("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(len, 0);
        stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    }

    let connection = find("connection").map(str::to_ascii_lowercase);
    let close = match connection.as_deref() {
        Some(c) if c.contains("close") => true,
        Some(c) if c.contains("keep-alive") => false,
        _ => http10,
    };
    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
        close,
    }))
}

pub fn write_response<W: Write>(w: &mut W, res: &Response, close: bool) -> Result<(), String> {
    let reason = match res.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", res.status);
    for (k, v) in &res.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\n",
        res.content_length.unwrap_or(res.body.len())
    ));
    head.push_str(if close {
        "Connection: close\r\n\r\n"
    } else {
        "Connection: keep-alive\r\n\r\n"
    });
    w.write_all(head.as_bytes())
        .and_then(|()| w.write_all(&res.body))
        .and_then(|()| w.flush())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    /// 읽기는 입력에서, 쓰기는 버퍼로 가는 시험용 스트림.
    struct Pipe {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl BufRead for Pipe {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.input.fill_buf()
        }
        fn consume(&mut self, amt: usize) {
            self.input.consume(amt);
        }
    }
    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn pipe(raw: &str) -> Pipe {
        Pipe {
            input: Cursor::new(raw.as_bytes().to_vec()),
            output: Vec::new(),
        }
    }

    #[test]
    fn content_length_본문과_쿼리를_읽는다() {
        let mut p = pipe("POST /a/b?x=1&y=2 HTTP/1.1\r\nHost: h\r\nContent-Length: 3\r\n\r\nabc");
        let r = read_request(&mut p).unwrap().unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/a/b");
        assert_eq!(r.query, "x=1&y=2");
        assert_eq!(r.body, b"abc");
        assert_eq!(r.header("host"), Some("h"));
        assert!(!r.close);
    }

    #[test]
    fn chunked_본문을_합친다() {
        let mut p = pipe(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        );
        let r = read_request(&mut p).unwrap().unwrap();
        assert_eq!(r.body, b"abcde");
    }

    #[test]
    fn 닫힌_연결은_none이다() {
        assert_eq!(read_request(&mut pipe("")).unwrap(), None);
    }

    #[test]
    fn expect_100_continue에_응답한다() {
        let mut p = pipe("PUT / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 1\r\n\r\nx");
        read_request(&mut p).unwrap();
        assert!(p.output.starts_with(b"HTTP/1.1 100 Continue"));
    }

    #[test]
    fn connection_close와_http10은_연결을_닫는다() {
        let r = read_request(&mut pipe("GET / HTTP/1.1\r\nConnection: close\r\n\r\n"))
            .unwrap()
            .unwrap();
        assert!(r.close);
        let r = read_request(&mut pipe("GET / HTTP/1.0\r\n\r\n"))
            .unwrap()
            .unwrap();
        assert!(r.close);
    }

    #[test]
    fn head_응답은_본문_없이_길이만_쓴다() {
        let mut res = Response::new(200, "image/webp", Vec::new());
        res.content_length = Some(42);
        let mut out = Vec::new();
        write_response(&mut out, &res, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Content-Length: 42\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }
}
