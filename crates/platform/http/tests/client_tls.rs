//! https 경로: 로컬 TLS 서버를 상대로 한다. `tests/fixtures`의 테스트 전용 CA(`ca.pem`)가 서명한
//! 리프(`localhost.pem`, SAN localhost·127.0.0.1, 유효 100년)를 쓴다. 자체서명 CA:TRUE 인증서는 rustls가 서버 인증서로 거부한다.
//! 실제 인터넷 호출은 하지 않는다.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use chalkak_http::client::{ClientError, HttpClient};
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use rustls::{ServerConfig, ServerConnection, StreamOwned};

const CA_PEM: &[u8] = include_bytes!("fixtures/ca.pem");
const CERT_PEM: &[u8] = include_bytes!("fixtures/localhost.pem");
const KEY_PEM: &[u8] = include_bytes!("fixtures/localhost.key");

/// TLS 서버가 요청 하나를 받고 `response`를 돌려준다. 핸드셰이크가 실패해도 스레드는 조용히 끝난다.
fn tls_server(response: &'static [u8], close_notify: bool) -> u16 {
    let certs = rustls::pki_types::CertificateDer::pem_slice_iter(CERT_PEM)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(KEY_PEM).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(config);
    thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(config).unwrap(), tcp);
        let mut buf = [0u8; 4096];
        let mut seen = Vec::new();
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            match tls.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        let _ = tls.write_all(response);
        let _ = tls.flush();
        if close_notify {
            tls.conn.send_close_notify();
        }
        let _ = tls.flush();
    });
    port
}

fn trusting_client() -> HttpClient {
    HttpClient::builder()
        .add_root_certificates_pem(CA_PEM)
        .unwrap()
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

#[test]
fn 추가한_루트를_신뢰하면_https_응답을_읽는다() {
    let port = tls_server(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", true);
    let resp = trusting_client()
        .get(&format!("https://localhost:{port}/x"), &[])
        .unwrap();
    assert_eq!((resp.status, resp.body_text().as_str()), (200, "hello"));
}

#[test]
fn ip_san_인증서도_검증된다() {
    let port = tls_server(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
        true,
    );
    let resp = trusting_client()
        .get(&format!("https://127.0.0.1:{port}/"), &[])
        .unwrap();
    assert_eq!(resp.body_text(), "ok");
}

#[test]
fn 연결_종료까지_읽기는_close_notify_없이_닫혀도_받는다() {
    let port = tls_server(b"HTTP/1.0 200 OK\r\n\r\nbody", false);
    assert_eq!(
        trusting_client()
            .get(&format!("https://localhost:{port}/"), &[])
            .unwrap()
            .body_text(),
        "body"
    );
}

#[test]
fn 신뢰하지_않는_자체서명_인증서는_거부한다() {
    let port = tls_server(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", true);
    let err = HttpClient::new()
        .unwrap()
        .get(&format!("https://localhost:{port}/"), &[])
        .unwrap_err();
    assert!(matches!(err, ClientError::Tls(_)), "{err}");
}
