//! 서버 시험이 함께 쓰는 시험 서버·클라이언트.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chalkak_http::{Handler, Limits, Request, Response, Server, ShutdownSignal};

pub struct TestServer {
    pub addr: SocketAddr,
    pub shutdown: ShutdownSignal,
    pub join: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start(handler: impl Handler + 'static, workers: usize, limits: Limits) -> TestServer {
        let server = Server::bind("127.0.0.1:0", workers, limits).expect("bind");
        let addr = server.local_addr().unwrap();
        let shutdown = ShutdownSignal::new();
        let s = shutdown.clone();
        let handler: Arc<dyn Handler> = Arc::new(handler);
        let join = thread::spawn(move || server.serve(handler, s).expect("serve"));
        TestServer {
            addr,
            shutdown,
            join: Some(join),
        }
    }

    pub fn connect(&self) -> Client {
        Client::new(TcpStream::connect(self.addr).expect("connect"))
    }

    pub fn stop(&mut self) {
        self.shutdown.trigger();
        if let Some(j) = self.join.take() {
            j.join().expect("server thread");
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Client {
    pub reader: BufReader<TcpStream>,
    pub writer: TcpStream,
}

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Client {
    pub fn new(stream: TcpStream) -> Client {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    pub fn send(&mut self, raw: &str) {
        self.writer.write_all(raw.as_bytes()).unwrap();
    }

    pub fn read_reply(&mut self, head_only: bool) -> Reply {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("status line");
        let status: u16 = line
            .split(' ')
            .nth(1)
            .unwrap_or_else(|| panic!("bad status line {line:?}"))
            .parse()
            .unwrap();
        let mut headers = Vec::new();
        loop {
            line.clear();
            self.reader.read_line(&mut line).unwrap();
            let l = line.trim_end_matches("\r\n");
            if l.is_empty() {
                break;
            }
            let (n, v) = l.split_once(": ").expect("header");
            headers.push((n.to_string(), v.to_string()));
        }
        let mut reply = Reply {
            status,
            headers,
            body: Vec::new(),
        };
        if !head_only && status >= 200 && status != 204 && status != 304 {
            let len: usize = reply
                .header("content-length")
                .expect("content-length")
                .parse()
                .unwrap();
            reply.body = vec![0; len];
            self.reader.read_exact(&mut reply.body).unwrap();
        }
        reply
    }

    /// 서버가 연결을 닫았는가(남은 바이트 없이 EOF).
    pub fn is_closed_by_peer(&mut self) -> bool {
        let mut b = [0u8; 1];
        matches!(self.reader.read(&mut b), Ok(0))
    }
}

pub fn echo(req: Request) -> Response {
    let body = format!(
        "{} {} q={:?} body={}",
        req.method,
        req.path,
        req.query,
        String::from_utf8_lossy(&req.body)
    );
    Response::new(200)
        .with_header("Content-Type", "text/plain")
        .with_body(body.into_bytes())
}

pub fn fast_limits() -> Limits {
    Limits {
        read_timeout: Duration::from_millis(500),
        keep_alive_timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}
