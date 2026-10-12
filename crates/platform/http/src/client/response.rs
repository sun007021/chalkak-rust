//! HTTP/1.1 응답 읽기: 상태줄, 헤더, 본문(Content-Length, chunked, 연결 종료까지).

use std::io::{self, BufRead, Read};

use super::error::{ClientError, from_io};

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 200;

/// 읽은 응답.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientResponse {
    pub status: u16,
    /// 수신 순서 그대로. 이름은 서버가 보낸 대소문자를 유지한다.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ClientResponse {
    /// 이름이 같은(대소문자 무시) 첫 헤더 값.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 본문을 UTF-8로 해석한다(잘못된 바이트는 대체 문자).
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 한 줄(CRLF 또는 LF 종료)을 읽는다. 연결이 먼저 끊기면 `None`.
fn read_line<R: BufRead>(r: &mut R, budget: &mut usize) -> Result<Option<String>, ClientError> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(from_io)?;
    if n == 0 {
        return Ok(None);
    }
    if n > *budget {
        return Err(ClientError::Protocol("응답 헤더가 너무 크다".into()));
    }
    if buf.last() != Some(&b'\n') {
        return Err(ClientError::Protocol("연결이 헤더 도중 끊겼다".into()));
    }
    *budget -= n;
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| ClientError::Protocol("헤더가 UTF-8이 아니다".into()))
}

/// 응답 하나를 읽는다. `head_request`가 참이면 본문을 읽지 않는다.
/// `100 Continue` 같은 1xx 중간 응답은 건너뛴다.
pub(crate) fn read_response<R: BufRead>(
    r: &mut R,
    head_request: bool,
    max_body: usize,
) -> Result<ClientResponse, ClientError> {
    loop {
        let mut budget = MAX_HEAD_BYTES;
        let status_line = read_line(r, &mut budget)?
            .ok_or_else(|| ClientError::Protocol("응답 없이 연결이 닫혔다".into()))?;
        let status = parse_status(&status_line)?;

        let mut headers = Vec::new();
        loop {
            let line = read_line(r, &mut budget)?
                .ok_or_else(|| ClientError::Protocol("연결이 헤더 도중 끊겼다".into()))?;
            if line.is_empty() {
                break;
            }
            if headers.len() >= MAX_HEADERS {
                return Err(ClientError::Protocol("헤더가 너무 많다".into()));
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| ClientError::Protocol(format!("헤더 형식 오류: {line}")))?;
            if name.is_empty() || name.contains(|c: char| c.is_whitespace()) {
                return Err(ClientError::Protocol(format!(
                    "헤더 이름 형식 오류: {name}"
                )));
            }
            headers.push((name.to_string(), value.trim().to_string()));
        }

        if (100..200).contains(&status) && status != 101 {
            continue;
        }

        let get = |n: &str| {
            headers
                .iter()
                .find(|(h, _)| h.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.as_str())
        };
        let no_body = head_request || status == 101 || status == 204 || status == 304;
        let body = if no_body {
            Vec::new()
        } else if get("transfer-encoding").is_some_and(|v| {
            v.split(',')
                .next_back()
                .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"))
        }) {
            read_chunked(r, max_body)?
        } else if let Some(len) = get("content-length") {
            let len: usize = len
                .parse()
                .map_err(|_| ClientError::Protocol(format!("Content-Length 형식 오류: {len}")))?;
            read_exact_limited(r, len, max_body)?
        } else {
            read_to_close(r, max_body)?
        };
        return Ok(ClientResponse {
            status,
            headers,
            body,
        });
    }
}

fn parse_status(line: &str) -> Result<u16, ClientError> {
    let bad = || ClientError::Protocol(format!("상태줄 형식 오류: {line}"));
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().ok_or_else(bad)?;
    if !version.starts_with("HTTP/1.") {
        return Err(bad());
    }
    let code = parts.next().ok_or_else(bad)?;
    if code.len() != 3 {
        return Err(bad());
    }
    code.parse().map_err(|_| bad())
}

fn read_exact_limited<R: BufRead>(
    r: &mut R,
    len: usize,
    max: usize,
) -> Result<Vec<u8>, ClientError> {
    if len > max {
        return Err(ClientError::BodyTooLarge { limit: max });
    }
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    let n = r
        .by_ref()
        .take(len as u64)
        .read_to_end(&mut body)
        .map_err(from_io)?;
    if n < len {
        return Err(ClientError::Protocol(format!(
            "본문이 {n}/{len}바이트에서 끊겼다"
        )));
    }
    Ok(body)
}

fn read_to_close<R: BufRead>(r: &mut R, max: usize) -> Result<Vec<u8>, ClientError> {
    let mut body = Vec::new();
    // 연결 종료로 끝나는 본문: TLS close_notify 없이 닫는 서버가 흔하므로 UnexpectedEof는 정상 종료로 본다.
    match r.by_ref().take(max as u64 + 1).read_to_end(&mut body) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(from_io(e)),
    }
    if body.len() > max {
        return Err(ClientError::BodyTooLarge { limit: max });
    }
    Ok(body)
}

fn read_chunked<R: BufRead>(r: &mut R, max: usize) -> Result<Vec<u8>, ClientError> {
    let mut body = Vec::new();
    loop {
        let mut budget = 1024;
        let line = read_line(r, &mut budget)?
            .ok_or_else(|| ClientError::Protocol("연결이 chunk 도중 끊겼다".into()))?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| ClientError::Protocol(format!("chunk 크기 형식 오류: {line}")))?;
        if size == 0 {
            break;
        }
        if body.len().saturating_add(size) > max {
            return Err(ClientError::BodyTooLarge { limit: max });
        }
        let chunk = read_exact_limited(r, size, max)?;
        body.extend_from_slice(&chunk);
        let mut budget = 8;
        match read_line(r, &mut budget)? {
            Some(l) if l.is_empty() => {}
            _ => return Err(ClientError::Protocol("chunk 뒤에 CRLF가 없다".into())),
        }
    }
    // 트레일러는 읽고 버린다.
    let mut budget = MAX_HEAD_BYTES;
    loop {
        match read_line(r, &mut budget)? {
            Some(l) if l.is_empty() => break,
            Some(_) => {}
            None => return Err(ClientError::Protocol("연결이 트레일러 도중 끊겼다".into())),
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8], head: bool) -> Result<ClientResponse, ClientError> {
        read_response(&mut io::BufReader::new(raw), head, 1024)
    }

    #[test]
    fn content_length_본문() {
        let r = parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A:  b \r\n\r\nhelloEXTRA",
            false,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.header("x-a"), Some("b"));
    }

    #[test]
    fn chunked_본문과_트레일러() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nTrailer: x\r\n\r\n";
        assert_eq!(parse(raw, false).unwrap().body, b"abcde");
    }

    #[test]
    fn 연결_종료까지_읽는다() {
        let r = parse(b"HTTP/1.0 200 OK\r\n\r\nuntil close", false).unwrap();
        assert_eq!(r.body, b"until close");
    }

    #[test]
    fn head와_204와_304는_본문이_없다() {
        assert!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n", true)
                .unwrap()
                .body
                .is_empty()
        );
        assert!(
            parse(b"HTTP/1.1 204 No Content\r\n\r\nxx", false)
                .unwrap()
                .body
                .is_empty()
        );
        assert!(
            parse(
                b"HTTP/1.1 304 Not Modified\r\nContent-Length: 3\r\n\r\n",
                false
            )
            .unwrap()
            .body
            .is_empty()
        );
    }

    #[test]
    fn 중간_1xx_응답은_건너뛴다() {
        let r = parse(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 1\r\n\r\nx",
            false,
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (201, &b"x"[..]));
    }

    #[test]
    fn 잘린_본문은_오류() {
        assert!(matches!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc", false),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            parse(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
                false
            ),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Le", false),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(parse(b"", false), Err(ClientError::Protocol(_))));
    }

    #[test]
    fn 본문_상한을_넘으면_오류() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n{}",
            "a".repeat(2000)
        );
        assert!(matches!(
            parse(raw.as_bytes(), false),
            Err(ClientError::BodyTooLarge { limit: 1024 })
        ));
        let raw = format!("HTTP/1.1 200 OK\r\n\r\n{}", "a".repeat(2000));
        assert!(matches!(
            parse(raw.as_bytes(), false),
            Err(ClientError::BodyTooLarge { .. })
        ));
    }

    #[test]
    fn 잘못된_상태줄과_chunk_크기() {
        assert!(matches!(
            parse(b"FOO 200 OK\r\n\r\n", false),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            parse(b"HTTP/1.1 20 OK\r\n\r\n", false),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            parse(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                false
            ),
            Err(ClientError::Protocol(_))
        ));
    }
}
