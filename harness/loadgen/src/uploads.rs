//! `uploads`: 로컬 AWS 환경(`local-aws/`)의 이미지 업로드 흐름 부하. 게시물 사진 업로드 URL 발급(presign) → S3 에 PUT →
//! S3 이벤트 → SQS → esm-poller → Lambda → 백엔드 콜백으로 `post_image_uploads` 가 `ISSUED` 를 벗어날 때까지(콜백 대기)를 잰다.
//!
//! - 열린 루프: 초당 `--rate` 건의 업로드를 정해진 시각에 시작한다(`--duration` 동안). 워커(`--conns`)가 밀려 늦게 시작해도
//!   지연은 실제 시작부터 재고 시작 지연(`lag`)을 따로 낸다. lag 가 크면 발생기가 포화된 것이니 `--conns` 를 올린다.
//! - TLS 라이브러리가 없다(스레드와 블로킹 소켓만). 그래서 HTTPS 인 edge(Caddy)를 거치지 않고, 백엔드 디버그 포트(`--base`,
//!   HTTP)에서 presign 하고, 서버가 서명한 `https://<버킷>.s3.<IP>.sslip.io/<키>?<서명>` 주소를 LocalStack 디버그 포트(`--s3`,
//!   HTTP)에 path-style(`/<버킷>/<키>?<서명>`)로 바꿔 PUT 한다. LocalStack 은 서명을 검증하지 않아 같은 객체가 만들어진다.
//!   Lambda 가 콜백하는 경로(HTTPS, edge 경유)는 그대로 쓰므로 서버 쪽 흐름은 앱과 같다.
//! - 회원은 `--users` 명을 DB 에 직접 만든다(`ON CONFLICT DO NOTHING`, 시드와 같은 UUID 규칙. 기존 데이터를 지우지 않는다).
//!   토큰은 로그인 없이 `ACCESS_TOKEN_SECRET` 으로 서명한다.
//! - 완료 확인은 DB 폴링이다(`--db`). 한 스레드가 진행 중인 업로드 ID 를 묶어 주기적으로 조회한다.
//! - 끝나면 `--timeout`(기본 7분 = `POST_IMAGE_PROCESSING_TIMEOUT`)까지 남은 업로드가 닫히길 기다리고, 닫히지 않은 것을
//!   `stuck` 으로 센다(S2 통과 기준: 모두 완료 또는 실패로 닫힘, 고아 0).

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::args::Args;
use crate::client::{Client, Request, read_response};
use crate::hist::Hist;
use crate::run_cmd::{connect_db, db_err, latency_json};
use crate::token::{Scope, Signer, user_uuid};

/// 기본 시험 이미지(EXIF 가 든 1600x1200 WebP). `--image` 로 바꾼다.
const DEFAULT_IMAGE: &[u8] = include_bytes!("../fixtures/post.webp");
const CACHE_CONTROL: &str = "public, max-age=86400";
const TOKEN_TTL_SECS: i64 = 3600;
const MAX_ERROR_SAMPLES: usize = 10;

/// 서버가 준 presigned URL 을 LocalStack 디버그 포트용 path-style 요청으로 바꾼 결과.
#[derive(Debug, PartialEq, Eq)]
pub struct PutTarget {
    /// 요청 대상(`/<버킷>/<키>?<서명 쿼리>`).
    pub path_and_query: String,
    /// 서명된 헤더 중 우리가 채워야 하는 것(`content-type`, `cache-control`, `if-none-match`).
    pub headers: Vec<(String, String)>,
}

/// `https://<버킷>.s3.<호스트>/<키>?X-Amz-SignedHeaders=...` → path-style. 버킷이 호스트 앞 라벨이 아니면(path-style 주소)
/// 경로를 그대로 쓴다.
pub fn put_target(url: &str, content_type: &str) -> Result<PutTarget, String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("presigned URL 형식이 아닙니다: {url}"))?;
    let (host, tail) = rest
        .split_once('/')
        .ok_or_else(|| format!("presigned URL 에 경로가 없습니다: {url}"))?;
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let bucket = host
        .split_once(".s3.")
        .map(|(bucket, _)| bucket)
        .filter(|b| !b.is_empty() && !b.contains('.'));
    let path_and_query = match bucket {
        Some(bucket) => format!("/{bucket}/{path}"),
        None => format!("/{path}"),
    };
    let path_and_query = if query.is_empty() {
        path_and_query
    } else {
        format!("{path_and_query}?{query}")
    };
    let signed = query
        .split('&')
        .find_map(|p| p.strip_prefix("X-Amz-SignedHeaders="))
        .unwrap_or("host");
    // 서명 값은 %3B 로 인코딩된 `;` 구분이다.
    let signed = signed.replace("%3B", ";").replace("%3b", ";");
    let mut headers = Vec::new();
    for name in signed.split(';') {
        match name {
            "content-type" => headers.push((name.to_owned(), content_type.to_owned())),
            "cache-control" => headers.push((name.to_owned(), CACHE_CONTROL.to_owned())),
            "if-none-match" => headers.push((name.to_owned(), "*".to_owned())),
            _ => {}
        }
    }
    Ok(PutTarget {
        path_and_query,
        headers,
    })
}

/// 본문이 있는 PUT 하나(연결을 새로 맺는다). 응답 상태 코드를 돌려준다.
fn http_put(
    addr: SocketAddr,
    target: &PutTarget,
    body: &[u8],
    timeout: Duration,
) -> std::io::Result<u16> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut head = format!(
        "PUT {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        target.path_and_query,
        addr,
        body.len()
    );
    for (name, value) in &target.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut writer = stream.try_clone()?;
    writer.write_all(head.as_bytes())?;
    writer.write_all(body)?;
    let mut reader = BufReader::new(stream);
    let (response, _) = read_response(&mut reader, false)?;
    Ok(response.status)
}

#[derive(Default)]
struct Shared {
    presign: Hist,
    put: Hist,
    /// PUT 완료 → `ISSUED` 를 벗어남(콜백 대기).
    callback: Hist,
    /// 업로드 시작 → 완료(presign + put + 콜백).
    total: Hist,
    lag: Hist,
    started: u64,
    errors: u64,
    error_samples: Vec<String>,
    /// 진행 중: 업로드 ID → (PUT 완료 시각, 시작 시각).
    pending: HashMap<String, (Instant, Instant)>,
    ready: u64,
    rejected: u64,
}

impl Shared {
    fn error(&mut self, what: String) {
        self.errors += 1;
        if self.error_samples.len() < MAX_ERROR_SAMPLES {
            self.error_samples.push(what);
        }
    }
}

fn micros(d: Duration) -> u64 {
    d.as_micros().min(u128::from(u64::MAX)) as u64
}

fn resolve(host: &str) -> Result<SocketAddr, String> {
    let host = host
        .strip_prefix("http://")
        .unwrap_or(host)
        .trim_end_matches('/');
    host.to_socket_addrs()
        .map_err(|e| format!("주소를 풀 수 없습니다 ({host}): {e}"))?
        .next()
        .ok_or_else(|| format!("주소가 없습니다: {host}"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct Cfg {
    base: SocketAddr,
    s3: SocketAddr,
    rate: f64,
    duration: Duration,
    conns: usize,
    users: u64,
    secret: String,
    image: Arc<Vec<u8>>,
}

/// 워커 하나: 슬롯을 번호순으로 받아 정해진 시각에 업로드를 시작한다.
fn worker(cfg: &Cfg, shared: &Mutex<Shared>, slot: &AtomicU64, start: Instant, run_tag: &str) {
    let Ok(signer) = Signer::from_hex(&cfg.secret) else {
        return;
    };
    let mut client = Client::new(cfg.base, true);
    loop {
        let k = slot.fetch_add(1, Ordering::Relaxed);
        let due = start + Duration::from_secs_f64(k as f64 / cfg.rate);
        if due >= start + cfg.duration {
            return;
        }
        let now = Instant::now();
        if due > now {
            thread::sleep(due - now);
        }
        let began = Instant::now();
        let user = k % cfg.users + 1;
        let token = signer.token(
            &user_uuid(user),
            Scope::User,
            now_unix() - 5,
            TOKEN_TTL_SECS,
            &format!("up-{run_tag}-{k}"),
        );
        let lag = began.saturating_duration_since(due);

        let presign = client.send(&Request {
            method: "POST",
            path: "/api/v1/posts/uploads",
            bearer: Some(&token),
            body: None,
        });
        let presign_done = Instant::now();
        let response = match presign {
            Ok(r) if r.status == 200 => r,
            Ok(r) => {
                let mut s = shared.lock().unwrap();
                s.started += 1;
                s.lag.record(micros(lag));
                s.error(format!(
                    "presign -> {} {}",
                    r.status,
                    String::from_utf8_lossy(&r.body_head[..r.body_head.len().min(160)])
                ));
                continue;
            }
            Err(e) => {
                let mut s = shared.lock().unwrap();
                s.started += 1;
                s.lag.record(micros(lag));
                s.error(format!("presign 연결 오류: {e}"));
                continue;
            }
        };
        let Ok(body) = serde_json::from_slice::<Value>(&response.body_head) else {
            let mut s = shared.lock().unwrap();
            s.started += 1;
            s.error("presign 응답이 JSON 이 아니다(4096바이트 초과?)".into());
            continue;
        };
        let (Some(id), Some(url)) = (body["uploadId"].as_str(), body["uploadUrl"].as_str()) else {
            let mut s = shared.lock().unwrap();
            s.started += 1;
            s.error(format!("presign 응답에 uploadId·uploadUrl 이 없다: {body}"));
            continue;
        };
        let content_type = body["contentType"].as_str().unwrap_or("image/webp");
        let target = match put_target(url, content_type) {
            Ok(t) => t,
            Err(e) => {
                let mut s = shared.lock().unwrap();
                s.started += 1;
                s.error(e);
                continue;
            }
        };
        let put = http_put(cfg.s3, &target, &cfg.image, Duration::from_secs(60));
        let put_done = Instant::now();
        let mut s = shared.lock().unwrap();
        s.started += 1;
        s.lag.record(micros(lag));
        s.presign.record(micros(presign_done - began));
        match put {
            Ok(200) => {
                s.put.record(micros(put_done - presign_done));
                s.pending.insert(id.to_owned(), (put_done, began));
            }
            Ok(status) => s.error(format!("S3 PUT -> {status}")),
            Err(e) => s.error(format!("S3 PUT 연결 오류: {e}")),
        }
    }
}

/// 진행 중인 업로드를 DB 에서 조회해 `ISSUED` 를 벗어난 것을 닫는다. 닫힌 수를 돌려준다.
fn poll_once(db: &mut postgres::Client, shared: &Mutex<Shared>) -> Result<usize, String> {
    let ids: Vec<String> = shared.lock().unwrap().pending.keys().cloned().collect();
    if ids.is_empty() {
        return Ok(0);
    }
    let rows = db
        .query(
            "SELECT id::text, status::text FROM post_image_uploads \
             WHERE id = ANY($1::text[]::uuid[]) AND status <> 'ISSUED'",
            &[&ids],
        )
        .map_err(db_err)?;
    let now = Instant::now();
    let mut s = shared.lock().unwrap();
    let mut closed = 0;
    for row in rows {
        let id: String = row.get(0);
        let status: String = row.get(1);
        if let Some((put_done, began)) = s.pending.remove(&id) {
            s.callback.record(micros(now - put_done));
            s.total.record(micros(now - began));
            if status == "READY" {
                s.ready += 1;
            } else {
                s.rejected += 1;
            }
            closed += 1;
        }
    }
    Ok(closed)
}

pub fn command(args: &Args) -> Result<u8, String> {
    let rate: f64 = args.parsed_or("rate", 2.0)?;
    if rate <= 0.0 {
        return Err("--rate 는 0보다 커야 합니다".into());
    }
    let image = match args.str("image") {
        Some(path) => {
            std::fs::read(path).map_err(|e| format!("--image 를 읽을 수 없습니다 ({path}): {e}"))?
        }
        None => DEFAULT_IMAGE.to_vec(),
    };
    let cfg = Arc::new(Cfg {
        base: resolve(args.require("base")?)?,
        s3: resolve(args.str("s3").unwrap_or("127.0.0.1:4566"))?,
        rate,
        duration: args.duration_or("duration", Duration::from_secs(60))?,
        conns: args.parsed_or("conns", 16usize)?.max(1),
        users: args.parsed_or("users", 50u64)?.max(1),
        secret: args.require("secret")?.to_owned(),
        image: Arc::new(image),
    });
    let timeout = args.duration_or("timeout", Duration::from_secs(7 * 60))?;
    let poll_every = args.duration_or("poll", Duration::from_millis(250))?;
    let label = args.str("label").unwrap_or("local-aws").to_owned();

    // 회원 준비: 시드와 같은 UUID 규칙, 기존 데이터는 그대로 둔다.
    let mut db = connect_db(args.require("db")?)?;
    db.batch_execute(&format!(
        "INSERT INTO users (id, email, status, signature_original_storage_key, signature_thumbnail_storage_key) \
         SELECT ('01990000-0000-7000-8000-' || lpad(to_hex(g), 12, '0'))::uuid, 'load' || g || '@test.chalkak', 'ACTIVE', \
                'chalkak/signatures/load' || g || '/original.png', 'chalkak/signatures/load' || g || '/thumbnail.png' \
         FROM generate_series(1, {}) g ON CONFLICT DO NOTHING",
        cfg.users
    ))
    .map_err(db_err)?;
    let before: i64 = db
        .query_one(
            "SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED'",
            &[],
        )
        .map_err(db_err)?
        .get(0);

    let expected = (rate * cfg.duration.as_secs_f64()).floor() as u64;
    println!(
        "업로드 부하: {rate}건/초 x {}초 (예정 약 {expected}건), 워커 {}, 회원 {}, 이미지 {}바이트, 완료 제한 {}초",
        cfg.duration.as_secs(),
        cfg.conns,
        cfg.users,
        cfg.image.len(),
        timeout.as_secs()
    );
    let shared = Arc::new(Mutex::new(Shared::default()));
    let slot = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let run_tag = format!("{:x}", now_unix());
    let workers: Vec<_> = (0..cfg.conns)
        .map(|_| {
            let (cfg, shared, slot, run_tag) =
                (cfg.clone(), shared.clone(), slot.clone(), run_tag.clone());
            thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || worker(&cfg, &shared, &slot, start, &run_tag))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;

    // 폴링은 이 스레드가 한다: 워커가 도는 동안 진행 상황을 찍고, 끝난 뒤에는 남은 것이 닫힐 때까지.
    let workers_done = Arc::new(AtomicBool::new(false));
    let waiter = {
        let workers_done = workers_done.clone();
        thread::spawn(move || {
            for w in workers {
                let _ = w.join();
            }
            workers_done.store(true, Ordering::Release);
        })
    };
    let mut last_report = Instant::now();
    let mut drain_started: Option<Instant> = None;
    loop {
        thread::sleep(poll_every);
        poll_once(&mut db, &shared)?;
        let done = workers_done.load(Ordering::Acquire);
        if done && drain_started.is_none() {
            drain_started = Some(Instant::now());
        }
        let (started, pending, ready, rejected, errors) = {
            let s = shared.lock().unwrap();
            (s.started, s.pending.len(), s.ready, s.rejected, s.errors)
        };
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            println!(
                "[{:>5.0}s] 시작 {started}  진행 중 {pending}  완료 {ready}  반려 {rejected}  오류 {errors}",
                start.elapsed().as_secs_f64()
            );
        }
        if done && pending == 0 {
            break;
        }
        if drain_started.is_some_and(|t| t.elapsed() >= timeout) {
            break;
        }
    }
    let _ = waiter.join();
    let wall = start.elapsed().as_secs_f64();

    // 닫히지 않은 것: DB 의 현재 상태로 한 번 더 확인(폴링 직후 닫혔을 수 있다).
    poll_once(&mut db, &shared)?;
    let s = shared.lock().unwrap();
    let stuck = s.pending.len();
    let issued_after: i64 = db
        .query_one(
            "SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED'",
            &[],
        )
        .map_err(db_err)?
        .get(0);
    let secs = cfg.duration.as_secs_f64();
    let ms = |h: &Hist, q: f64| h.quantile(q) as f64 / 1000.0;
    println!("---- 결과 ({label}, 전체 {wall:.0}초)");
    println!(
        "시작 {}  완료(READY) {}  반려(REJECTED) {}  닫히지 않음(stuck) {}  오류 {}  (시작 전 ISSUED {before}건 -> 끝 {issued_after}건)",
        s.started, s.ready, s.rejected, stuck, s.errors
    );
    for (name, h) in [
        ("presign      ", &s.presign),
        ("S3 PUT       ", &s.put),
        ("콜백 대기    ", &s.callback),
        ("시작->완료   ", &s.total),
    ] {
        println!(
            "{name} n={:<5} p50 {:>8.1}ms  p95 {:>8.1}ms  p99 {:>8.1}ms  max {:>8.1}ms",
            h.count(),
            ms(h, 0.5),
            ms(h, 0.95),
            ms(h, 0.99),
            h.max() as f64 / 1000.0
        );
    }
    println!(
        "시작 지연(lag) p99 {:.1}ms  max {:.1}ms",
        ms(&s.lag, 0.99),
        s.lag.max() as f64 / 1000.0
    );
    for sample in &s.error_samples {
        println!("  오류 예: {sample}");
    }
    if let Some(out) = args.str("out") {
        let result = json!({
            "label": label,
            "rate": rate,
            "duration_secs": secs,
            "wall_secs": wall,
            "conns": cfg.conns,
            "users": cfg.users,
            "image_bytes": cfg.image.len(),
            "started": s.started,
            "ready": s.ready,
            "rejected": s.rejected,
            "stuck": stuck,
            "errors": s.errors,
            "error_samples": s.error_samples,
            "issued_before": before,
            "issued_after": issued_after,
            "presign": latency_json(&s.presign, 0, secs),
            "put": latency_json(&s.put, 0, secs),
            "callback_wait": latency_json(&s.callback, 0, secs),
            "total": latency_json(&s.total, 0, secs),
            "lag": latency_json(&s.lag, 0, secs),
        });
        std::fs::write(out, serde_json::to_string_pretty(&result).unwrap())
            .map_err(|e| format!("결과를 쓸 수 없습니다 ({out}): {e}"))?;
        println!("결과 JSON: {out}");
    }
    // 통과 기준(S2): 오류 없음, 닫히지 않은 업로드 없음.
    Ok(if s.errors == 0 && stuck == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://techcourse-project-2026.s3.127.0.0.1.sslip.io/chalkak/staging/dev/posts/abc.webp?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-SignedHeaders=cache-control%3Bcontent-type%3Bhost%3Bif-none-match&X-Amz-Signature=ff";

    #[test]
    fn 가상호스트_주소를_path_style로_바꾼다() {
        let t = put_target(URL, "image/webp").unwrap();
        assert!(t.path_and_query.starts_with(
            "/techcourse-project-2026/chalkak/staging/dev/posts/abc.webp?X-Amz-Algorithm="
        ));
        assert!(t.path_and_query.ends_with("X-Amz-Signature=ff"));
    }

    #[test]
    fn 서명된_헤더만_채운다() {
        let t = put_target(URL, "image/webp").unwrap();
        assert_eq!(
            t.headers,
            vec![
                ("cache-control".to_owned(), CACHE_CONTROL.to_owned()),
                ("content-type".to_owned(), "image/webp".to_owned()),
                ("if-none-match".to_owned(), "*".to_owned()),
            ]
        );
        let host_only = "https://b.s3.x.sslip.io/k?X-Amz-SignedHeaders=host";
        assert!(
            put_target(host_only, "image/webp")
                .unwrap()
                .headers
                .is_empty()
        );
    }

    #[test]
    fn path_style_주소는_경로를_그대로_쓴다() {
        let t = put_target(
            "https://s3.127.0.0.1.sslip.io/bucket/key.webp?X-Amz-SignedHeaders=host",
            "image/webp",
        )
        .unwrap();
        assert_eq!(
            t.path_and_query,
            "/bucket/key.webp?X-Amz-SignedHeaders=host"
        );
    }

    #[test]
    fn 이상한_주소는_거절한다() {
        assert!(put_target("ftp://x/y", "image/webp").is_err());
        assert!(put_target("https://hostonly", "image/webp").is_err());
    }

    #[test]
    fn 기본_이미지는_webp다() {
        assert_eq!(&DEFAULT_IMAGE[..4], b"RIFF");
        assert_eq!(&DEFAULT_IMAGE[8..12], b"WEBP");
    }
}
