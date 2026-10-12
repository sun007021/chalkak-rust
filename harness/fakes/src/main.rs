//! 하네스용 가짜 외부 서버. 한 프로세스, 한 포트에서 OIDC JWKS, Apple 토큰 교환·철회, Slack Webhook,
//! S3 HeadObject를 흉내 내고 받은 요청을 모두 기록한다. 러너가 관리 경로(`/__admin/*`)로 응답을 지정하고
//! 기록을 가져간다. 경로 표는 `routes.rs` 상단 주석 참고.
//!
//! 환경변수: `FAKES_ADDR`(기본 `0.0.0.0:9000`), `FAKES_S3_BUCKET`(기본 `test-bucket`).

mod http;
mod routes;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn main() {
    let addr = std::env::var("FAKES_ADDR").unwrap_or_else(|_| "0.0.0.0:9000".into());
    let bucket = std::env::var("FAKES_S3_BUCKET").unwrap_or_else(|_| "test-bucket".into());
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{addr} 바인드 실패: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("fakes 시작: {addr} (S3 버킷 {bucket})");
    let state = Arc::new(Mutex::new(routes::Fakes::new(&bucket)));
    for stream in listener.incoming().flatten() {
        let state = Arc::clone(&state);
        thread::spawn(move || serve(stream, &state));
    }
}

fn serve(stream: TcpStream, state: &Mutex<routes::Fakes>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut conn = Duplex {
        reader: BufReader::new(stream),
        writer,
    };
    loop {
        let req = match http::read_request(&mut conn) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                eprintln!("요청 읽기 실패: {e}");
                return;
            }
        };
        let res = match state.lock() {
            Ok(mut s) => s.handle(&req),
            Err(_) => return,
        };
        if http::write_response(&mut conn.writer, &res, req.close).is_err() || req.close {
            return;
        }
    }
}

/// 읽기는 버퍼 리더로, 쓰기는 같은 소켓으로 보내는 한 쌍.
struct Duplex {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl BufRead for Duplex {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.reader.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.reader.consume(amt);
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}
