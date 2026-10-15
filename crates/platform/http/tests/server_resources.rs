//! 연결 하나가 쓰는 자원(깨어남, FD, 붙잡는 시간) 시험. `/proc/self`를 세므로 다른 시험과 프로세스를 나눠 두고,
//! 이 파일 안에서도 하나씩 돈다(#36 stress 시험에서 찾은 문제).

use std::io::Write;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use chalkak_http::Limits;

mod support;
use support::*;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

// 그대로 연결 수만큼 곱해진다.

/// 이 프로세스의 `chalkak-conn` 스레드들이 지금까지 자발적으로 잠든 횟수(깨어난 횟수)의 합.
fn conn_thread_wakeups() -> u64 {
    let mut total = 0;
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let path = task.path();
        let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if comm.trim() != "chalkak-conn" {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(path.join("status")) else {
            continue;
        };
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:") {
                total += v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    total
}

#[test]
fn idle_connections_do_not_wake_up_periodically() {
    let _serial = serial();
    let srv = TestServer::start(echo, 2, fast_limits());
    let _idle: Vec<Client> = (0..20)
        .map(|_| {
            let mut c = srv.connect();
            c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
            assert_eq!(c.read_reply(false).status, 200);
            c
        })
        .collect();
    thread::sleep(Duration::from_millis(200));
    let before = conn_thread_wakeups();
    thread::sleep(Duration::from_secs(1));
    let woke = conn_thread_wakeups() - before;
    // 예전에는 유휴 스레드가 100ms마다 깨어나 1초에 연결당 약 10번(20개면 약 200번)이었다.
    assert!(woke <= 20, "유휴 연결 20개가 1초에 {woke}번 깨어났다");
}

#[test]
fn shutdown_still_closes_idle_connections_promptly() {
    let _serial = serial();
    let mut srv = TestServer::start(
        echo,
        2,
        Limits {
            keep_alive_timeout: Duration::from_secs(60),
            ..fast_limits()
        },
    );
    let mut idle = srv.connect();
    idle.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(idle.read_reply(false).status, 200);
    let mut never_sent = srv.connect();
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    srv.stop();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert!(idle.is_closed_by_peer());
    assert!(never_sent.is_closed_by_peer());
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn each_connection_uses_one_server_fd() {
    let _serial = serial();
    let srv = TestServer::start(echo, 2, fast_limits());
    thread::sleep(Duration::from_millis(100));
    let before = open_fds();
    let _idle: Vec<Client> = (0..40)
        .map(|_| {
            let mut c = srv.connect();
            c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
            assert_eq!(c.read_reply(false).status, 200);
            c
        })
        .collect();
    let added = open_fds() - before;
    // 시험 클라이언트는 연결마다 FD 2개(읽기용 복제), 서버는 1개여야 한다: 120개. 예전에는 서버도 쓰기용 복제본(try_clone)을 열어 160개였다.
    assert!(added <= 124, "연결 40개에 FD {added}개");
}

#[test]
fn request_head_trickled_byte_by_byte_is_cut_at_read_timeout() {
    let _serial = serial();
    let srv = TestServer::start(echo, 2, fast_limits());
    let mut c = srv.connect();
    let started = Instant::now();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n");
    // read_timeout(500ms)보다 짧은 간격으로 한 바이트씩 계속 보낸다. 헤더 전체 마감이 없으면 8KB까지 끊기지 않는다.
    let mut closed = false;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(100));
        if c.writer.write_all(b"X").is_err() {
            closed = true;
            break;
        }
        if started.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    assert!(
        closed || c.is_closed_by_peer(),
        "헤더를 한 바이트씩 보내는 연결이 {:?} 동안 끊기지 않았다",
        started.elapsed()
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn slow_body_is_allowed_while_each_gap_is_within_read_timeout() {
    let _serial = serial();
    let srv = TestServer::start(echo, 2, fast_limits());
    let mut c = srv.connect();
    c.send("POST /slow HTTP/1.1\r\nHost: t\r\nContent-Length: 8\r\n\r\n");
    // 본문은 Tomcat처럼 바이트가 오는 동안 마감이 늘어난다: 간격 300ms로 총 1.2초(read_timeout 500ms보다 김).
    for b in ["ab", "cd", "ef", "gh"] {
        thread::sleep(Duration::from_millis(300));
        c.writer.write_all(b.as_bytes()).unwrap();
    }
    assert_eq!(c.read_reply(false).status, 200);
}
