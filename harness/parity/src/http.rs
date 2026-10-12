//! 하네스 전용 최소 HTTP/1.1 클라이언트. 요청마다 연결을 새로 열고 `Connection: close`로 받는다.
//! 비교 대상 서버의 동작을 그대로 보려는 도구라 리다이렉트·재시도·압축 해제를 하지 않는다.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// `base_url`은 `http://host:port` 형태만 받는다. 하네스는 로컬 컨테이너와만 통신한다.
pub fn send(base_url: &str, req: &Request) -> Result<Response, String> {
    let authority = base_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("http:// 주소만 지원합니다: {base_url}"))?
        .trim_end_matches('/');
    let mut stream =
        TcpStream::connect(authority).map_err(|e| format!("{authority} 연결 실패: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\n",
        req.method, req.path, authority
    );
    let mut has_content_type = false;
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(body) = &req.body {
        if !has_content_type {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");

    stream
        .write_all(head.as_bytes())
        .map_err(|e| e.to_string())?;
    if let Some(body) = &req.body {
        stream.write_all(body).map_err(|e| e.to_string())?;
    }
    stream.flush().map_err(|e| e.to_string())?;

    let no_body = req.method.eq_ignore_ascii_case("HEAD");
    read_response(BufReader::new(stream), no_body)
}

/// `no_body`: HEAD 응답처럼 헤더에 길이가 있어도 본문이 오지 않는 경우.
fn read_response<R: Read>(mut reader: BufReader<R>, no_body: bool) -> Result<Response, String> {
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| e.to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("잘못된 상태 줄: {status_line:?}"))?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _): &&(String, String)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let chunked = find("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let mut body = Vec::new();
    if no_body || status == 204 || status == 304 || (100..200).contains(&status) {
        // 본문 없음
    } else if chunked {
        loop {
            let mut size_line = String::new();
            reader
                .read_line(&mut size_line)
                .map_err(|e| e.to_string())?;
            let size_hex = size_line.trim().split(';').next().unwrap_or("0");
            let size = usize::from_str_radix(size_hex, 16)
                .map_err(|_| format!("잘못된 chunk 크기: {size_line:?}"))?;
            if size == 0 {
                // trailer는 무시하고 빈 줄까지 읽는다.
                loop {
                    let mut t = String::new();
                    if reader.read_line(&mut t).map_err(|e| e.to_string())? == 0
                        || t.trim().is_empty()
                    {
                        break;
                    }
                }
                break;
            }
            let mut chunk = vec![0; size];
            reader.read_exact(&mut chunk).map_err(|e| e.to_string())?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).map_err(|e| e.to_string())?;
        }
    } else if let Some(len) = find("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(len, 0);
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    } else {
        reader.read_to_end(&mut body).map_err(|e| e.to_string())?;
    }

    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_본문을_합친다() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let res = read_response(BufReader::new(&raw[..]), false).unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.body, b"abcde");
    }

    #[test]
    fn 빈_응답의_오류는_러너가_알아보는_표지를_담는다() {
        // runner::EMPTY_RESPONSE_MARK와 같은 문자열이어야 일시적 오류로 재실행한다.
        let err = read_response(BufReader::new(&b""[..]), false).unwrap_err();
        assert!(err.contains("잘못된 상태 줄: \"\""), "{err}");
    }

    #[test]
    fn content_length만큼_읽는다() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}extra";
        let res = read_response(BufReader::new(&raw[..]), false).unwrap();
        assert_eq!(res.status, 404);
        assert_eq!(res.body, b"{}");
    }
}
