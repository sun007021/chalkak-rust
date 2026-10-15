//! 최소 HTTP/1.1 클라이언트: keep-alive 연결 하나를 쥐고 요청을 하나씩 보낸다.
//! 응답 본문은 읽어서 버린다(`Content-Length`, `Transfer-Encoding: chunked`, 연결 닫힘까지 읽기).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub bearer: Option<&'a str>,
    pub body: Option<&'a str>,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// 응답 본문 앞부분(검사용). 전부 읽되 앞 `KEEP`바이트만 남긴다.
    pub body_head: Vec<u8>,
}

const KEEP: usize = 512;

pub struct Client {
    addr: SocketAddr,
    keep_alive: bool,
    conn: Option<BufReader<TcpStream>>,
    /// 지금까지 새로 맺은 연결 수.
    pub connects: u64,
    /// 마지막 `send`가 연결을 맺다가 실패했는가(연결 실패와 요청 실패를 가르는 데 쓴다).
    pub connect_failed: bool,
    connect_timeout: Duration,
    io_timeout: Duration,
}

impl Client {
    pub fn new(addr: SocketAddr, keep_alive: bool) -> Client {
        Client {
            addr,
            keep_alive,
            conn: None,
            connects: 0,
            connect_failed: false,
            connect_timeout: Duration::from_secs(10),
            io_timeout: IO_TIMEOUT,
        }
    }

    /// 연결·읽기/쓰기 제한 시간을 바꾼다(기본 10초·60초).
    pub fn with_timeouts(mut self, connect: Duration, io: Duration) -> Client {
        self.connect_timeout = connect;
        self.io_timeout = io;
        self
    }

    /// 연결만 미리 맺어 둔다(첫 요청이 연결 시간을 먹지 않게). 이미 있으면 그대로 둔다.
    pub fn warm(&mut self) -> io::Result<()> {
        if self.conn.is_none() {
            self.connect()?;
        }
        Ok(())
    }

    fn connect(&mut self) -> io::Result<()> {
        let stream = TcpStream::connect_timeout(&self.addr, self.connect_timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.io_timeout))?;
        stream.set_write_timeout(Some(self.io_timeout))?;
        self.conn = Some(BufReader::with_capacity(16 * 1024, stream));
        self.connects += 1;
        Ok(())
    }

    /// 요청 하나를 보내고 응답을 끝까지 읽는다. 실패하면 연결을 버린다(다음 호출이 새로 맺는다).
    pub fn send(&mut self, req: &Request) -> io::Result<Response> {
        self.connect_failed = false;
        if self.conn.is_none()
            && let Err(e) = self.connect()
        {
            self.connect_failed = true;
            return Err(e);
        }
        let result = self.exchange(req);
        match &result {
            Ok((_, close)) if *close || !self.keep_alive => self.conn = None,
            Err(_) => self.conn = None,
            _ => {}
        }
        result.map(|(r, _)| r)
    }

    fn exchange(&mut self, req: &Request) -> io::Result<(Response, bool)> {
        let conn = self.conn.as_mut().expect("연결이 있어야 한다");
        let mut head = String::with_capacity(256 + req.path.len());
        head.push_str(req.method);
        head.push(' ');
        head.push_str(req.path);
        head.push_str(" HTTP/1.1\r\nHost: chalkak\r\nAccept: application/json\r\n");
        if let Some(t) = req.bearer {
            head.push_str("Authorization: Bearer ");
            head.push_str(t);
            head.push_str("\r\n");
        }
        if !self.keep_alive {
            head.push_str("Connection: close\r\n");
        }
        if let Some(b) = req.body {
            head.push_str("Content-Type: application/json\r\nContent-Length: ");
            head.push_str(&b.len().to_string());
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        if let Some(b) = req.body {
            bytes.extend_from_slice(b.as_bytes());
        }
        conn.get_mut().write_all(&bytes)?;
        read_response(conn, req.method == "HEAD")
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_line(r: &mut impl BufRead, buf: &mut String) -> io::Result<()> {
    buf.clear();
    if r.read_line(buf)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// 응답을 읽는다. 반환값의 bool은 서버가 연결을 닫겠다고 한 것(`Connection: close`, 닫힐 때까지 읽은 본문).
pub fn read_response(r: &mut impl BufRead, head_only: bool) -> io::Result<(Response, bool)> {
    let mut line = String::new();
    read_line(r, &mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("상태 줄"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut close = line.starts_with("HTTP/1.0");
    loop {
        read_line(r, &mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((name, value)) = l.split_once(':') {
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.parse().ok(),
                "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
                "connection" => {
                    if value.eq_ignore_ascii_case("close") {
                        close = true;
                    } else if value.eq_ignore_ascii_case("keep-alive") {
                        close = false;
                    }
                }
                _ => {}
            }
        }
    }
    let mut body_head = Vec::new();
    let bodyless = head_only || status == 204 || status == 304 || (100..200).contains(&status);
    if bodyless {
    } else if chunked {
        loop {
            read_line(r, &mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| bad("청크 크기"))?;
            if size == 0 {
                // 트레일러와 마지막 빈 줄
                loop {
                    read_line(r, &mut line)?;
                    if line.trim_end().is_empty() {
                        break;
                    }
                }
                break;
            }
            copy_n(r, size, &mut body_head)?;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf)?;
        }
    } else if let Some(n) = content_length {
        copy_n(r, n, &mut body_head)?;
    } else {
        // 길이를 모르면 닫힐 때까지.
        let mut rest = Vec::new();
        r.read_to_end(&mut rest)?;
        body_head.extend_from_slice(&rest[..rest.len().min(KEEP)]);
        close = true;
    }
    Ok((Response { status, body_head }, close))
}

fn copy_n(r: &mut impl Read, mut n: usize, keep: &mut Vec<u8>) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    while n > 0 {
        let want = n.min(buf.len());
        let got = r.read(&mut buf[..want])?;
        if got == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if keep.len() < KEEP {
            let take = got.min(KEEP - keep.len());
            keep.extend_from_slice(&buf[..take]);
        }
        n -= got;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(responses: Vec<&'static [u8]>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            for resp in responses {
                // 요청 헤더 끝까지 읽는다.
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        return;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                s.write_all(resp).unwrap();
            }
        });
        addr
    }

    fn get(path: &str) -> Request<'_> {
        Request {
            method: "GET",
            path,
            bearer: None,
            body: None,
        }
    }

    #[test]
    fn content_length_본문을_읽고_연결을_재사용한다() {
        let addr = serve_once(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        ]);
        let mut c = Client::new(addr, true);
        let r = c.send(&get("/a")).unwrap();
        assert_eq!((r.status, r.body_head.as_slice()), (200, &b"hello"[..]));
        let r = c.send(&get("/b")).unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(c.connects, 1, "keep-alive라 연결은 한 번만 맺는다");
    }

    #[test]
    fn 청크_본문을_읽는다() {
        let addr = serve_once(vec![
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        ]);
        let mut c = Client::new(addr, true);
        let r = c.send(&get("/")).unwrap();
        assert_eq!(r.body_head, b"abcde");
    }

    #[test]
    fn 서버가_닫겠다고_하면_다음에_다시_맺는다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
        });
        let mut c = Client::new(addr, true);
        assert_eq!(c.send(&get("/")).unwrap().status, 204);
        assert_eq!(c.send(&get("/")).unwrap().status, 204);
        assert_eq!(c.connects, 2);
    }

    #[test]
    fn keep_alive를_끄면_요청마다_연결한다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for _ in 0..3 {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).contains("Connection: close"));
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
            }
        });
        let mut c = Client::new(addr, false);
        for _ in 0..3 {
            assert_eq!(c.send(&get("/")).unwrap().status, 200);
        }
        assert_eq!(c.connects, 3);
    }

    #[test]
    fn 연결_실패와_요청_실패를_구분한다() {
        // 닫힌 포트: 연결 단계에서 실패
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let mut c = Client::new(closed, true);
        assert!(c.send(&get("/")).is_err());
        assert!(c.connect_failed, "연결 단계 실패");
        // 받자마자 닫는 서버: 연결은 되고 요청 단계에서 실패
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            drop(s);
        });
        let mut c = Client::new(addr, true);
        assert!(c.send(&get("/")).is_err());
        assert!(!c.connect_failed, "요청 단계 실패");
    }

    #[test]
    fn 읽기_제한_시간을_줄일_수_있다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (_s, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(2)); // 응답하지 않는다
        });
        let mut c = Client::new(addr, true)
            .with_timeouts(Duration::from_secs(1), Duration::from_millis(200));
        let t0 = std::time::Instant::now();
        let e = c.send(&get("/")).unwrap_err();
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(!c.connect_failed);
    }

    #[test]
    fn 연결이_끊기면_오류를_내고_다음에_복구한다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            drop(s); // 응답 없이 닫는다
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let mut c = Client::new(addr, true);
        assert!(c.send(&get("/")).is_err());
        assert_eq!(c.send(&get("/")).unwrap().status, 200);
    }
}
