//! 실제 `TcpStream`으로 서버를 시험한다(keep-alive, HEAD, 100-continue, chunked, 파이프라인, 종료 신호 등).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chalkak_http::{
    Chain, Handler, Limits, Method, Request, Response, Router, Server, ShutdownSignal,
};

struct TestServer {
    addr: SocketAddr,
    shutdown: ShutdownSignal,
    join: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(handler: impl Handler + 'static, workers: usize, limits: Limits) -> TestServer {
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

    fn connect(&self) -> Client {
        Client::new(TcpStream::connect(self.addr).expect("connect"))
    }

    fn stop(&mut self) {
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

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Client {
    fn new(stream: TcpStream) -> Client {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn send(&mut self, raw: &str) {
        self.writer.write_all(raw.as_bytes()).unwrap();
    }

    fn read_reply(&mut self, head_only: bool) -> Reply {
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
    fn is_closed_by_peer(&mut self) -> bool {
        let mut b = [0u8; 1];
        matches!(self.reader.read(&mut b), Ok(0))
    }
}

fn echo(req: Request) -> Response {
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

fn fast_limits() -> Limits {
    Limits {
        read_timeout: Duration::from_millis(500),
        keep_alive_timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}

#[test]
fn keep_alive_serves_sequential_requests_on_one_connection() {
    let srv = TestServer::start(echo, 2, fast_limits());
    let mut c = srv.connect();
    for i in 0..3 {
        c.send(&format!("GET /r{i}?n={i} HTTP/1.1\r\nHost: t\r\n\r\n"));
        let r = c.read_reply(false);
        assert_eq!(r.status, 200);
        assert!(r.text().contains(&format!("GET /r{i} ")), "{}", r.text());
        assert!(
            r.header("connection").is_none(),
            "keep-alive를 유지해야 한다"
        );
        assert!(r.header("date").is_some());
    }
    // 같은 연결로 본문이 있는 요청도 이어진다.
    c.send("POST /p HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\n\r\nabc");
    assert!(c.read_reply(false).text().ends_with("body=abc"));
}

#[test]
fn connection_close_and_http10_semantics() {
    let srv = TestServer::start(echo, 2, fast_limits());

    let mut c = srv.connect();
    c.send("GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.header("connection"), Some("close"));
    assert!(c.is_closed_by_peer());

    // HTTP/1.0 기본은 닫는다.
    let mut c = srv.connect();
    c.send("GET / HTTP/1.0\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("connection"), Some("close"));
    assert!(c.is_closed_by_peer());

    // HTTP/1.0 + keep-alive는 유지하고 응답에도 keep-alive를 알린다.
    let mut c = srv.connect();
    c.send("GET /a HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.header("connection"), Some("keep-alive"));
    c.send("GET /b HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert!(c.read_reply(false).text().contains("/b"));
}

#[test]
fn head_omits_body_but_keeps_content_length_and_connection_stays_usable() {
    // platform__method-not-allowed.json "HEAD 공개 경로" / RFC 9110 9.3.2
    let srv = TestServer::start(echo, 1, fast_limits());
    let mut c = srv.connect();
    c.send("HEAD /head HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_reply(true);
    assert_eq!(r.status, 200);
    let len: usize = r.header("content-length").unwrap().parse().unwrap();
    assert!(len > 0);
    // 본문이 새어 나왔다면 다음 응답 파싱이 깨진다.
    c.send("GET /next HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.status, 200);
    assert!(r.text().contains("GET /next"));
}

#[test]
fn expect_100_continue_sends_interim_response_before_reading_body() {
    let srv = TestServer::start(echo, 1, fast_limits());
    let mut c = srv.connect();
    c.send("POST /up HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n");
    let interim = c.read_reply(true);
    assert_eq!(interim.status, 100);
    c.send("hello");
    let r = c.read_reply(false);
    assert_eq!(r.status, 200);
    assert!(r.text().ends_with("body=hello"));
}

#[test]
fn expect_100_continue_is_not_sent_when_body_is_too_large() {
    let srv = TestServer::start(
        echo,
        1,
        Limits {
            max_body_bytes: 4,
            ..fast_limits()
        },
    );
    let mut c = srv.connect();
    c.send("POST /up HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.status, 413, "100 없이 곧바로 413");
    assert!(c.is_closed_by_peer());
}

#[test]
fn unknown_expectation_is_417() {
    let srv = TestServer::start(echo, 1, fast_limits());
    let mut c = srv.connect();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 1\r\nExpect: nope\r\n\r\nx");
    assert_eq!(c.read_reply(false).status, 417);
}

#[test]
fn chunked_request_body_with_extensions_and_trailers() {
    let srv = TestServer::start(echo, 1, fast_limits());
    let mut c = srv.connect();
    c.send(
        "POST /c HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n\
         5;ext=1\r\nhello\r\n1\r\n \r\n5\r\nworld\r\n0\r\nX-Trailer: v\r\n\r\n",
    );
    let r = c.read_reply(false);
    assert_eq!(r.status, 200);
    assert!(r.text().ends_with("body=hello world"), "{}", r.text());
    // 트레일러까지 소비했으므로 같은 연결로 계속된다.
    c.send("GET /after HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(c.read_reply(false).text().contains("/after"));
}

#[test]
fn chunked_body_arriving_in_small_pieces() {
    let srv = TestServer::start(echo, 1, fast_limits());
    let mut c = srv.connect();
    c.send("POST /c HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n");
    for piece in ["3\r", "\nab", "c\r\n", "0\r\n", "\r\n"] {
        c.send(piece);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(c.read_reply(false).text().ends_with("body=abc"));
}

#[test]
fn pipelined_requests_get_ordered_responses() {
    let srv = TestServer::start(echo, 1, fast_limits());
    let mut c = srv.connect();
    c.send(
        "GET /one HTTP/1.1\r\nHost: t\r\n\r\n\
         POST /two HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\nhi\
         HEAD /three HTTP/1.1\r\nHost: t\r\n\r\n\
         GET /four HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    assert!(c.read_reply(false).text().contains("GET /one"));
    assert!(c.read_reply(false).text().ends_with("body=hi"));
    assert_eq!(c.read_reply(true).status, 200);
    let last = c.read_reply(false);
    assert!(last.text().contains("GET /four"));
    assert!(c.is_closed_by_peer());
}

#[test]
fn malformed_requests_get_400_and_the_connection_is_closed() {
    let srv = TestServer::start(echo, 1, fast_limits());
    for raw in [
        "GARBAGE\r\n\r\n",
        "GET / HTTP/1.1\r\n\r\n", // Host 없음
        "GET / HTTP/1.1\r\nHost: t\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
        "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\nx",
        "POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
        "GET /%zz HTTP/1.1\r\nHost: t\r\n\r\n",
    ] {
        let mut c = srv.connect();
        c.send(raw);
        let r = c.read_reply(false);
        assert_eq!(r.status, 400, "{raw:?}");
        assert_eq!(r.header("connection"), Some("close"));
        assert!(c.is_closed_by_peer(), "{raw:?}");
    }
    // 서버는 계속 정상이다.
    let mut c = srv.connect();
    c.send("GET /ok HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(false).status, 200);
}

#[test]
fn size_limits_map_to_400_413_505_501() {
    let srv = TestServer::start(
        echo,
        1,
        Limits {
            max_header_bytes: 256,
            max_headers: 5,
            max_body_bytes: 8,
            ..fast_limits()
        },
    );
    let cases: Vec<(String, u16)> = vec![
        (
            format!("GET / HTTP/1.1\r\nHost: t\r\nX: {}\r\n\r\n", "a".repeat(400)),
            400,
        ),
        (
            "GET / HTTP/1.1\r\nHost: t\r\nA: 1\r\nB: 2\r\nC: 3\r\nD: 4\r\nE: 5\r\n\r\n".into(),
            400,
        ),
        (format!("GET /{} HTTP/1.1\r\nHost: t\r\n\r\n", "a".repeat(400)), 400),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 9\r\n\r\n123456789".into(),
            413,
        ),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n".into(),
            413,
        ),
        ("GET / HTTP/2.0\r\nHost: t\r\n\r\n".into(), 505),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: gzip\r\n\r\n".into(),
            501,
        ),
    ];
    for (raw, want) in cases {
        let mut c = srv.connect();
        c.send(&raw);
        let r = c.read_reply(false);
        assert_eq!(r.status, want, "{:?}", &raw[..raw.len().min(60)]);
        assert!(c.is_closed_by_peer());
    }
    // 상한 이내는 통과
    let mut c = srv.connect();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 8\r\n\r\n12345678");
    assert_eq!(c.read_reply(false).status, 200);
}

#[test]
fn stalled_request_gets_408_and_idle_connection_is_closed_silently() {
    let srv = TestServer::start(
        echo,
        2,
        Limits {
            read_timeout: Duration::from_millis(200),
            keep_alive_timeout: Duration::from_millis(400),
            ..Limits::default()
        },
    );
    // 요청 도중 멈춤 -> 408
    let mut c = srv.connect();
    c.send("GET / HTTP/1.1\r\nHo");
    let started = Instant::now();
    let r = c.read_reply(false);
    assert_eq!(r.status, 408);
    assert!(started.elapsed() < Duration::from_secs(3));

    // 본문 도중 멈춤 -> 408
    let mut c = srv.connect();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 10\r\n\r\nabc");
    assert_eq!(c.read_reply(false).status, 408);

    // 아무것도 안 보낸 유휴 연결 -> 응답 없이 닫힘
    let mut c = srv.connect();
    let started = Instant::now();
    assert!(c.is_closed_by_peer());
    assert!(started.elapsed() >= Duration::from_millis(300));

    // keep-alive 뒤 유휴도 조용히 닫힌다.
    let mut c = srv.connect();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(false).status, 200);
    assert!(c.is_closed_by_peer());
}

#[test]
fn router_end_to_end_with_params_query_404_405() {
    let router = Router::new()
        .route(Method::Get, "/api/v1/posts/{postId}", |r: Request| {
            Response::new(200).with_body(
                format!("post {} q={:?}", r.param("postId").unwrap(), r.query).into_bytes(),
            )
        })
        .route(Method::Post, "/api/v1/posts", echo);
    let srv = TestServer::start(router, 2, fast_limits());
    let mut c = srv.connect();

    c.send("GET /api/v1/posts/12?tag=a+b%20c&tag=%EA%B0%80 HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.status, 200);
    assert_eq!(
        r.text(),
        "post 12 q=[(\"tag\", \"a b c\"), (\"tag\", \"가\")]"
    );

    c.send("GET /api/v1/posts/12/ HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(false).status, 404);

    c.send("GET /API/v1/posts/12 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(false).status, 404);

    c.send("DELETE /api/v1/posts HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("POST"));

    c.send("HEAD /api/v1/posts/5 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(true).status, 200);
}

#[test]
fn middleware_chain_and_duplicate_response_headers_on_the_wire() {
    let chain = Chain::new(|_r: Request| Response::new(200).with_body(b"ok".to_vec())).layer(
        |r: Request, next: &dyn Handler| {
            next.handle(r)
                .with_header("Vary", "Origin")
                .with_header("X-Between", "1")
                .with_header("Vary", "Access-Control-Request-Method")
                .with_header("Vary", "Access-Control-Request-Headers")
        },
    );
    let srv = TestServer::start(chain, 1, fast_limits());
    let mut c = srv.connect();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_reply(false);
    let vary: Vec<_> = r
        .headers
        .iter()
        .filter(|(n, _)| n == "Vary")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        vary,
        [
            "Origin",
            "Access-Control-Request-Method",
            "Access-Control-Request-Headers"
        ]
    );
    let order: Vec<_> = r.headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(&order[..4], ["Vary", "X-Between", "Vary", "Vary"]);
}

#[test]
fn handler_panic_yields_500_and_the_server_keeps_running() {
    let srv = TestServer::start(
        |r: Request| -> Response {
            if r.path == "/boom" {
                panic!("boom");
            }
            echo(r)
        },
        1,
        fast_limits(),
    );
    let mut c = srv.connect();
    c.send("GET /boom HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.read_reply(false);
    assert_eq!(r.status, 500);
    assert!(c.is_closed_by_peer());
    // 워커가 살아 있다(워커 1개).
    let mut c = srv.connect();
    c.send("GET /fine HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(false).status, 200);
}

#[test]
fn remote_addr_is_the_peer() {
    let srv = TestServer::start(
        |r: Request| Response::new(200).with_body(r.remote_addr.to_string().into_bytes()),
        1,
        fast_limits(),
    );
    let mut c = srv.connect();
    let local = c.writer.local_addr().unwrap();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.read_reply(false).text(), local.to_string());
}

#[test]
fn workers_handle_connections_in_parallel() {
    let slow = |_r: Request| {
        thread::sleep(Duration::from_millis(300));
        Response::new(200)
    };
    let srv = TestServer::start(slow, 4, fast_limits());
    let started = Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let addr = srv.addr;
            thread::spawn(move || {
                let mut c = Client::new(TcpStream::connect(addr).unwrap());
                c.send("GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
                c.read_reply(false).status
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 200);
    }
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "병렬로 처리돼야 한다: {:?}",
        started.elapsed()
    );
}

#[test]
fn connections_over_the_limit_get_503() {
    let srv = TestServer::start(
        echo,
        1,
        Limits {
            max_connections: 1,
            ..fast_limits()
        },
    );
    // 첫 연결이 자리를 차지한다(요청을 한 번 처리해 서버가 받았음을 확인).
    let mut first = srv.connect();
    first.send("GET /a HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(first.read_reply(false).status, 200);

    let mut second = srv.connect();
    let r = second.read_reply(false);
    assert_eq!(r.status, 503);
    assert!(second.is_closed_by_peer());

    // 첫 연결이 끝나면 다시 받는다.
    drop(first);
    thread::sleep(Duration::from_millis(300));
    let mut third = srv.connect();
    third.send("GET /c HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(third.read_reply(false).status, 200);
}

#[test]
fn shutdown_finishes_in_flight_requests_and_closes_idle_connections() {
    let slow = |r: Request| {
        if r.path == "/slow" {
            thread::sleep(Duration::from_millis(400));
        }
        Response::new(200).with_body(b"done".to_vec())
    };
    let mut srv = TestServer::start(slow, 4, fast_limits());

    // 처리 중인 요청
    let mut busy = srv.connect();
    busy.send("GET /slow HTTP/1.1\r\nHost: t\r\n\r\n");
    // 요청 사이 유휴 연결
    let mut idle = srv.connect();
    idle.send("GET /fast HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(idle.read_reply(false).status, 200);
    // 아무것도 안 보낸 새 연결
    let mut silent = srv.connect();

    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    srv.shutdown.trigger();

    // 진행 중이던 요청은 정상 응답을 받고, 연결은 닫힌다고 알린다.
    let r = busy.read_reply(false);
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "done");
    assert_eq!(r.header("connection"), Some("close"));
    assert!(busy.is_closed_by_peer());
    // 유휴 연결은 곧바로 닫힌다.
    assert!(idle.is_closed_by_peer());
    assert!(silent.is_closed_by_peer());

    srv.stop(); // serve가 돌아와야 join이 끝난다
    assert!(started.elapsed() < Duration::from_secs(3));
    // 더 이상 받지 않는다.
    assert!(TcpStream::connect(srv.addr).is_err());
}

#[test]
fn trigger_before_serve_returns_immediately() {
    let server = Server::bind("127.0.0.1:0", 1, Limits::default()).unwrap();
    let shutdown = ShutdownSignal::new();
    shutdown.trigger();
    let handler: Arc<dyn Handler> = Arc::new(echo);
    server.serve(handler, shutdown).unwrap();
}
