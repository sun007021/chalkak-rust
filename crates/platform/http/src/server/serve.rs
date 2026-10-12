//! `Server`: 소켓을 열고 워커 스레드 풀로 연결을 처리한다.
//!
//! 구조: `serve`를 부른 스레드가 accept 루프를 돌고, 받은 연결을 유한 큐로 워커에 넘긴다.
//! 워커 하나가 연결 하나를 닫힐 때까지(keep-alive 포함) 맡는다. 동시 연결 수가 `max_connections`를
//! 넘으면 새 연결은 503으로 즉시 닫는다.
//!
//! 종료: [`ShutdownSignal::trigger`]가 호출되면 accept를 멈추고, 처리 중인 요청은 끝까지 응답한 뒤
//! `Connection: close`로 닫으며, 요청을 기다리던 유휴 연결은 바로 닫는다. 모든 워커가 끝나면 `serve`가 돌아온다.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::handler::Handler;
use super::limits::Limits;
use super::method::Method;
use super::parse::{Conn, Head, ParseError, Version};
use super::request::Request;
use super::response::{Response, WriteOptions, serialize};

/// 소켓 읽기를 이 간격으로 끊어서 종료 신호·마감 시각을 확인한다.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// 오류 응답 뒤 연결을 닫기 전에 상대가 보내는 나머지를 버리며 기다리는 최대 시간·바이트.
const LINGER: Duration = Duration::from_millis(200);
const LINGER_MAX_BYTES: usize = 256 * 1024;

/// 서버 종료 신호. 복제해서 다른 스레드(시그널 핸들러 등)에서 `trigger`할 수 있다.
#[derive(Clone, Default)]
pub struct ShutdownSignal {
    inner: Arc<SignalInner>,
}

#[derive(Default)]
struct SignalInner {
    triggered: AtomicBool,
    wake: Mutex<Option<SocketAddr>>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// 종료를 요청한다. 여러 번 불러도 된다.
    pub fn trigger(&self) {
        self.inner.triggered.store(true, Ordering::SeqCst);
        let addr = *self.inner.wake.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(addr) = addr {
            // accept에서 막혀 있는 서버 스레드를 깨운다.
            let _ = TcpStream::connect_timeout(&loopback(addr), Duration::from_secs(1));
        }
    }

    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::SeqCst)
    }

    fn register(&self, addr: SocketAddr) {
        *self.inner.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(addr);
    }
}

/// `0.0.0.0`·`::`에 묶인 소켓은 루프백 주소로 접속해 깨운다.
fn loopback(mut addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            Ipv4Addr::LOCALHOST.into()
        } else {
            Ipv6Addr::LOCALHOST.into()
        });
    }
    addr
}

pub struct Server {
    listener: TcpListener,
    workers: usize,
    limits: Limits,
}

impl Server {
    /// 주소에 바인딩한다. `workers`가 0이면 1로 본다. 포트 0을 주면 [`Server::local_addr`]로 실제 포트를 알 수 있다.
    pub fn bind(addr: impl ToSocketAddrs, workers: usize, limits: Limits) -> io::Result<Server> {
        Ok(Server {
            listener: TcpListener::bind(addr)?,
            workers: workers.max(1),
            limits,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 종료 신호가 올 때까지 요청을 처리한다. 신호 뒤 진행 중이던 요청을 마무리하고 돌아온다.
    pub fn serve(self, handler: Arc<dyn Handler>, shutdown: ShutdownSignal) -> io::Result<()> {
        let Server {
            listener,
            workers,
            limits,
        } = self;
        let limits = Arc::new(limits);
        let max_connections = limits.max_connections.max(1);

        shutdown.register(listener.local_addr()?);
        // register 뒤에 확인해야 trigger와 엇갈려도 깨어난다.
        if shutdown.is_triggered() {
            return Ok(());
        }

        let (tx, rx) = mpsc::sync_channel::<TcpStream>(max_connections);
        let rx = Arc::new(Mutex::new(rx));
        let active = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(workers);
        for i in 0..workers {
            let worker = Worker {
                rx: Arc::clone(&rx),
                active: Arc::clone(&active),
                handler: Arc::clone(&handler),
                limits: Arc::clone(&limits),
                shutdown: shutdown.clone(),
            };
            handles.push(
                thread::Builder::new()
                    .name(format!("chalkak-http-{i}"))
                    .spawn(move || worker.run())?,
            );
        }

        for incoming in listener.incoming() {
            if shutdown.is_triggered() {
                break;
            }
            let stream = match incoming {
                Ok(s) => s,
                Err(e) => {
                    // 연결 하나의 실패(중단된 handshake)나 fd 고갈로 서버 전체를 내리지 않는다.
                    if !matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::Interrupted
                    ) {
                        thread::sleep(Duration::from_millis(50));
                    }
                    continue;
                }
            };
            if active.fetch_add(1, Ordering::SeqCst) >= max_connections {
                active.fetch_sub(1, Ordering::SeqCst);
                reject_busy(stream, &limits);
                continue;
            }
            if tx.send(stream).is_err() {
                active.fetch_sub(1, Ordering::SeqCst);
                break;
            }
        }

        drop(tx);
        drop(listener);
        for h in handles {
            let _ = h.join();
        }
        Ok(())
    }
}

/// 연결 상한 초과: 503을 쓰고 닫는다(쓰기가 막혀도 accept 루프를 오래 붙잡지 않도록 짧게).
fn reject_busy(mut stream: TcpStream, limits: &Limits) {
    let _ = stream.set_write_timeout(Some(limits.read_timeout.min(Duration::from_secs(1))));
    let bytes = serialize(
        &Response::new(503),
        WriteOptions {
            close: true,
            ..Default::default()
        },
    );
    let _ = stream.write_all(&bytes);
    let _ = stream.shutdown(Shutdown::Both);
}

struct Worker {
    rx: Arc<Mutex<Receiver<TcpStream>>>,
    active: Arc<AtomicUsize>,
    handler: Arc<dyn Handler>,
    limits: Arc<Limits>,
    shutdown: ShutdownSignal,
}

impl Worker {
    fn run(self) {
        loop {
            let next = {
                let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
                rx.recv()
            };
            let Ok(stream) = next else { break };
            // 연결 처리 중 패닉은 handle_connection 안에서 잡는다. 여기서는 개수만 맞춘다.
            serve_connection(stream, self.handler.as_ref(), &self.limits, &self.shutdown);
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 다음 요청의 첫 바이트를 기다리는 중. 마감은 keep-alive 타임아웃이고, 종료 신호가 오면 EOF처럼 끝낸다.
    Idle,
    /// 요청을 읽는 중. 새 바이트가 `read_timeout` 안에 와야 한다.
    Active,
}

/// 소켓 읽기에 유휴/활성 타임아웃과 종료 신호 확인을 입힌다.
struct PollReader {
    stream: TcpStream,
    shutdown: ShutdownSignal,
    read_timeout: Duration,
    phase: Phase,
    deadline: Instant,
}

impl PollReader {
    fn wait_for_request(&mut self, idle_timeout: Duration) {
        self.phase = Phase::Idle;
        self.deadline = Instant::now() + idle_timeout;
    }

    fn continue_request(&mut self) {
        self.phase = Phase::Active;
        self.deadline = Instant::now() + self.read_timeout;
    }
}

impl Read for PollReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.stream.read(buf) {
                Ok(n) => {
                    if n > 0 {
                        self.continue_request();
                    }
                    return Ok(n);
                }
                Err(e) => match e.kind() {
                    io::ErrorKind::Interrupted => {}
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                        if Instant::now() >= self.deadline {
                            return Err(io::ErrorKind::TimedOut.into());
                        }
                        if self.phase == Phase::Idle && self.shutdown.is_triggered() {
                            return Ok(0);
                        }
                    }
                    _ => return Err(e),
                },
            }
        }
    }
}

fn serve_connection(
    stream: TcpStream,
    handler: &dyn Handler,
    limits: &Limits,
    shutdown: &ShutdownSignal,
) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(limits.read_timeout));
    if stream.set_read_timeout(Some(POLL_SLICE)).is_err() {
        return;
    }
    let (Ok(peer), Ok(mut writer)) = (stream.peer_addr(), stream.try_clone()) else {
        return;
    };
    let mut conn = Conn::new(PollReader {
        stream,
        shutdown: shutdown.clone(),
        read_timeout: limits.read_timeout,
        phase: Phase::Idle,
        deadline: Instant::now() + limits.keep_alive_timeout,
    });

    loop {
        if conn.has_buffered() {
            conn.reader_mut().continue_request();
        } else {
            conn.reader_mut()
                .wait_for_request(limits.keep_alive_timeout);
        }
        let head = match conn.read_head(limits) {
            Ok(Some(head)) => head,
            Ok(None) => break,
            Err(e) => {
                reject(&mut conn, &mut writer, &e);
                break;
            }
        };

        if head.expects_continue && writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
            break;
        }
        let body = match conn.read_body(&head, limits) {
            Ok(b) => b,
            Err(e) => {
                reject(&mut conn, &mut writer, &e);
                break;
            }
        };

        let keep_alive_requested = head.keep_alive;
        let version = head.version;
        let head_only = head.method == Method::Head;
        let request = into_request(head, body, peer);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| handler.handle(request)));
        let (response, panicked) = match outcome {
            Ok(r) => (r, false),
            Err(_) => (Response::new(500), true),
        };

        let handler_closes = response.headers.has_token("connection", "close");
        let close = !keep_alive_requested || handler_closes || panicked || shutdown.is_triggered();
        let bytes = serialize(
            &response,
            WriteOptions {
                head_only,
                close,
                keep_alive_header: version == Version::Http10 && keep_alive_requested,
            },
        );
        if writer.write_all(&bytes).is_err() || close {
            break;
        }
    }
    let _ = writer.shutdown(Shutdown::Write);
}

fn into_request(head: Head, body: Vec<u8>, remote_addr: SocketAddr) -> Request {
    Request {
        method: head.method,
        path: head.path,
        query: head.query,
        headers: head.headers,
        body,
        remote_addr,
        params: Vec::new(),
    }
}

/// 잘못된 요청에 오류 응답을 쓰고 연결을 정리한다. 응답할 수 없는 오류(연결 끊김)는 조용히 닫는다.
fn reject(conn: &mut Conn<PollReader>, writer: &mut TcpStream, err: &ParseError) {
    let Some(status) = err.status() else { return };
    let bytes = serialize(
        &Response::new(status),
        WriteOptions {
            close: true,
            ..Default::default()
        },
    );
    if writer.write_all(&bytes).is_err() {
        return;
    }
    // 읽지 않은 요청 바이트가 남은 채 닫으면 RST가 나가 상대가 응답을 못 읽을 수 있다.
    // 쓰기 쪽을 닫아 응답을 먼저 내보내고, 상대가 보내는 나머지를 잠깐 버린다.
    let _ = writer.shutdown(Shutdown::Write);
    let reader = conn.reader_mut();
    let until = Instant::now() + LINGER;
    let mut drained = 0usize;
    let mut sink = [0u8; 4096];
    while Instant::now() < until && drained < LINGER_MAX_BYTES {
        match reader.stream.read(&mut sink) {
            Ok(0) => break,
            Ok(n) => drained += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
    }
}
