//! http 경로 통합 테스트: 로컬 `TcpListener` 서버를 상대로 한다.
//! 호출 형태 근거: AppleHttpTokenClient(form POST), SlackIncomingWebhookSender(JSON POST),
//! S3 HeadObject(HEAD), IMDSv2(PUT 토큰 → GET), Nimbus JWKS(GET).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chalkak_http::client::{ClientError, HttpClient, Method};

/// 요청 하나를 받아 기록하고 `respond`가 만든 바이트를 돌려준 뒤 닫는 서버.
fn serve_once(
    respond: impl FnOnce(&mut TcpStream) + Send + 'static,
) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let req = read_request(&mut s);
        let _ = tx.send(req);
        respond(&mut s);
    });
    (base, rx)
}

fn read_request(s: &mut TcpStream) -> Vec<u8> {
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = s.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            if buf.len() >= pos + 4 + len {
                return buf;
            }
        }
    }
}

fn client() -> HttpClient {
    HttpClient::builder()
        .connect_timeout(Duration::from_millis(500))
        .read_timeout(Duration::from_millis(500))
        .build()
        .unwrap()
}

#[test]
fn content_length_본문을_읽는다() {
    let (base, rx) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"keys\":[]}").unwrap();
    });
    let resp = client()
        .get(
            &format!("{base}/.well-known/jwks.json?x=1"),
            &[("Accept", "application/json")],
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert_eq!(resp.body_text(), "{\"keys\":[]}");
    let req = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(req.starts_with("GET /.well-known/jwks.json?x=1 HTTP/1.1\r\n"));
    assert!(req.contains("Accept: application/json\r\n"));
    assert!(req.contains("Connection: close\r\n"));
}

#[test]
fn chunked_본문을_이어_붙인다() {
    let (base, _) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        for part in [
            "5\r\nhello\r\n",
            "1\r\n \r\n",
            "5\r\nworld\r\n",
            "0\r\n\r\n",
        ] {
            s.write_all(part.as_bytes()).unwrap();
            s.flush().unwrap();
            thread::sleep(Duration::from_millis(20));
        }
    });
    assert_eq!(client().get(&base, &[]).unwrap().body_text(), "hello world");
}

#[test]
fn 연결_종료까지_읽는다() {
    let (base, _) = serve_once(|s| {
        s.write_all(b"HTTP/1.0 200 OK\r\n\r\nno length").unwrap();
    });
    assert_eq!(client().get(&base, &[]).unwrap().body_text(), "no length");
}

#[test]
fn head_응답은_본문이_없고_헤더만_온다() {
    // S3 HeadObject 형태: Content-Length가 있어도 본문은 없다.
    let (base, rx) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12345\r\nContent-Type: image/png\r\n\r\n")
            .unwrap();
    });
    let resp = client()
        .head(
            &format!("{base}/obj"),
            &[("X-Amz-Date", "20260101T000000Z")],
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("Content-Length"), Some("12345"));
    assert!(resp.body.is_empty());
    let req = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(
        req.starts_with("HEAD /obj HTTP/1.1\r\n")
            && req.contains("X-Amz-Date: 20260101T000000Z\r\n")
    );
}

#[test]
fn form_본문은_urlencoded로_보낸다_apple_토큰_교환() {
    let (base, rx) = serve_once(|s| {
        s.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 25\r\n\r\n{\"error\":\"invalid_grant\"}",
        )
        .unwrap();
    });
    let resp = client()
        .post_form(
            &format!("{base}/auth/token"),
            &[("Accept", "application/json")],
            &[
                ("client_id", "com.chalkak"),
                ("code", "a b&c"),
                ("grant_type", "authorization_code"),
            ],
        )
        .unwrap();
    // RestClient와 달리 4xx도 오류로 바꾸지 않고 본문을 돌려준다.
    assert_eq!(resp.status, 400);
    assert!(resp.body_text().contains("invalid_grant"));
    let req = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(req.starts_with("POST /auth/token HTTP/1.1\r\n"));
    assert!(req.contains("Content-Type: application/x-www-form-urlencoded\r\n"));
    assert!(req.contains("Content-Length: 64\r\n"), "{req}");
    assert!(
        req.ends_with("\r\n\r\nclient_id=com.chalkak&code=a+b%26c&grant_type=authorization_code")
    );
}

#[test]
fn json_본문을_보낸다_slack_웹훅() {
    let (base, rx) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });
    let resp = client()
        .post_json(
            &format!("{base}/services/T/B/x"),
            &[],
            "{\"text\":\"안녕\"}",
        )
        .unwrap();
    assert!(resp.is_success());
    let req = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(req.contains("Content-Type: application/json\r\n"));
    assert!(req.ends_with("{\"text\":\"안녕\"}"));
    assert!(req.contains(&format!(
        "Content-Length: {}\r\n",
        "{\"text\":\"안녕\"}".len()
    )));
}

#[test]
fn put_본문_없음은_content_length_0_imdsv2_토큰() {
    let (base, rx) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\ntoken")
            .unwrap();
    });
    let resp = client()
        .put(
            &format!("{base}/latest/api/token"),
            &[("X-aws-ec2-metadata-token-ttl-seconds", "21600")],
            None,
        )
        .unwrap();
    assert_eq!(resp.body_text(), "token");
    let req = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(req.starts_with("PUT /latest/api/token HTTP/1.1\r\n"));
    assert!(req.contains("X-aws-ec2-metadata-token-ttl-seconds: 21600\r\n"));
    assert!(req.contains("Content-Length: 0\r\n"));
}

#[test]
fn 리다이렉트는_따르지_않는다() {
    let (base, _) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://elsewhere.invalid/\r\nContent-Length: 0\r\n\r\n").unwrap();
    });
    let resp = client().request(Method::Get, &base, &[], None).unwrap();
    assert_eq!(resp.status, 302);
    assert_eq!(resp.header("location"), Some("http://elsewhere.invalid/"));
}

#[test]
fn 읽기_타임아웃() {
    let (base, _) = serve_once(|s| {
        thread::sleep(Duration::from_millis(1500));
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    });
    let started = Instant::now();
    let err = client().get(&base, &[]).unwrap_err();
    assert!(matches!(err, ClientError::ReadTimeout), "{err}");
    assert!(started.elapsed() < Duration::from_millis(1400));
}

#[test]
fn 본문_도중_멈추면_읽기_타임아웃() {
    let (base, _) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
            .unwrap();
        s.flush().unwrap();
        thread::sleep(Duration::from_millis(1500));
    });
    assert!(matches!(
        client().get(&base, &[]).unwrap_err(),
        ClientError::ReadTimeout
    ));
}

#[test]
fn 연결_거부() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let err = client()
        .get(&format!("http://127.0.0.1:{port}/"), &[])
        .unwrap_err();
    assert!(matches!(err, ClientError::Connect(_)), "{err}");
}

#[test]
fn 응답_없이_닫히면_프로토콜_오류() {
    let (base, _) = serve_once(|_s| {});
    assert!(matches!(
        client().get(&base, &[]).unwrap_err(),
        ClientError::Protocol(_)
    ));
}

#[test]
fn 본문이_상한을_넘으면_거부한다() {
    let (base, _) = serve_once(|s| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
            .unwrap();
        let _ = s.write_all(&[b'a'; 100]);
    });
    let c = HttpClient::builder()
        .max_response_bytes(10)
        .build()
        .unwrap();
    assert!(matches!(
        c.get(&base, &[]).unwrap_err(),
        ClientError::BodyTooLarge { limit: 10 }
    ));
}

#[test]
fn 잘못된_url과_스킴() {
    let c = client();
    assert!(matches!(
        c.get("ftp://x/", &[]).unwrap_err(),
        ClientError::UnsupportedScheme(_)
    ));
    assert!(matches!(
        c.get("nonsense", &[]).unwrap_err(),
        ClientError::InvalidUrl(_)
    ));
}

// 이름 해석에 실패하면 연결 거부와 구별되는 오류다(FCM 발송이 UNAVAILABLE로 분류한다).
#[test]
fn 이름_해석_실패() {
    let err = client()
        .get("http://no-such-host.invalid:80/", &[])
        .unwrap_err();
    assert!(matches!(err, ClientError::Resolve(_)), "{err}");
}
