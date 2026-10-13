//! 부하 실행기. 연결마다 스레드 하나가 블로킹 소켓으로 요청을 보낸다.
//!
//! - 최대 처리량 모드(`rps = None`): 연결마다 응답을 받자마자 다음 요청을 보낸다(닫힌 루프).
//! - 고정 RPS 모드: 전체 목표 처리율을 연결에 나눠 정해진 시각에 보낸다. 지연은 **보내기로 한 시각**부터 잰다
//!   (서버가 느려 보내는 게 밀린 시간도 지연에 들어간다. coordinated omission 보정).
//! - 워밍업 동안의 요청은 통계에 넣지 않는다.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::client::{Client, Request};
use crate::hist::Hist;
use crate::mix::{self, Ctx, Rng, WEIGHTS};
use crate::token::{Scope, Signer, user_uuid};

const FLUSH_EVERY: Duration = Duration::from_millis(500);
const TOKEN_TTL_SECS: i64 = 900;
/// 토큰을 이 시간마다 새로 만든다(만료 전에 교체).
const TOKEN_REFRESH: Duration = Duration::from_secs(300);
const MAX_ERROR_SAMPLES: usize = 10;

#[derive(Clone)]
pub struct LoadCfg {
    pub addr: SocketAddr,
    pub conns: usize,
    pub duration: Duration,
    pub warmup: Duration,
    /// `None`이면 최대 처리량.
    pub rps: Option<f64>,
    /// 한 엔드포인트만(`mix::index_of`).
    pub only: Option<usize>,
    pub keep_alive: bool,
    /// 요청 없이 붙어만 있는(가끔 요청 하나만 보내는) 유휴 keep-alive 연결 수.
    pub idle_conns: usize,
    pub secret_hex: String,
    pub ctx: Ctx,
    pub seed: u64,
}

#[derive(Clone, Default)]
pub struct EpStats {
    pub hist: Hist,
    pub errors: u64,
}

#[derive(Clone)]
pub struct Stats {
    pub eps: Vec<EpStats>,
    pub all: Hist,
    pub errors: u64,
    pub connects: u64,
    pub error_samples: Vec<String>,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            eps: vec![EpStats::default(); WEIGHTS.len()],
            all: Hist::default(),
            errors: 0,
            connects: 0,
            error_samples: Vec::new(),
        }
    }
}

impl Stats {
    pub fn merge(&mut self, o: &Stats) {
        for (a, b) in self.eps.iter_mut().zip(&o.eps) {
            a.hist.merge(&b.hist);
            a.errors += b.errors;
        }
        self.all.merge(&o.all);
        self.errors += o.errors;
        self.connects += o.connects;
        for s in &o.error_samples {
            if self.error_samples.len() < MAX_ERROR_SAMPLES {
                self.error_samples.push(s.clone());
            }
        }
    }

    pub fn clear(&mut self) {
        *self = Stats::default();
    }
}

pub struct Shared {
    pub measuring: AtomicBool,
    pub stop: AtomicBool,
    /// 측정 시작부터의 누적.
    pub cum: Mutex<Stats>,
    /// 마지막으로 가져간 뒤의 누적(구간 보고용).
    pub interval: Mutex<Stats>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            measuring: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            cum: Mutex::default(),
            interval: Mutex::default(),
        })
    }

    fn flush(&self, local: &mut Stats) {
        self.cum.lock().unwrap().merge(local);
        self.interval.lock().unwrap().merge(local);
        local.clear();
    }

    pub fn take_interval(&self) -> Stats {
        std::mem::take(&mut *self.interval.lock().unwrap())
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct Tokens {
    signer: Signer,
    cache: Vec<Option<(Instant, String)>>,
}

impl Tokens {
    fn new(secret_hex: &str, users: u64) -> Tokens {
        Tokens {
            signer: Signer::from_hex(secret_hex).expect("ACCESS_TOKEN_SECRET은 hex여야 한다"),
            cache: vec![None; users as usize + 1],
        }
    }

    fn get(&mut self, user: u64) -> &str {
        let slot = &mut self.cache[user as usize];
        if slot
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > TOKEN_REFRESH)
        {
            let t = self.signer.token(
                &user_uuid(user),
                Scope::User,
                now_unix() - 5,
                TOKEN_TTL_SECS,
                &format!("load-{user}-{}", now_unix()),
            );
            *slot = Some((Instant::now(), t));
        }
        &slot.as_ref().unwrap().1
    }
}

fn worker(cfg: &LoadCfg, shared: &Shared, index: usize) {
    let mut client = Client::new(cfg.addr, cfg.keep_alive);
    let mut rng = Rng::new(cfg.seed.wrapping_add(index as u64 * 7919 + 1));
    let mut tokens = Tokens::new(&cfg.secret_hex, cfg.ctx.users);
    let mut local = Stats::default();
    let interval = cfg
        .rps
        .map(|r| Duration::from_secs_f64(cfg.conns as f64 / r.max(0.001)));
    let start = Instant::now();
    // 연결마다 시작 시각을 어긋나게 해서 한꺼번에 몰리지 않게 한다.
    let mut next = start
        + interval
            .map(|i| i.mul_f64(index as f64 / cfg.conns as f64))
            .unwrap_or_default();
    let mut last_flush = Instant::now();
    let mut connects_reported = 0;

    while !shared.stop.load(Ordering::Relaxed) {
        let scheduled = match interval {
            Some(step) => {
                let at = next;
                next += step;
                let now = Instant::now();
                if at > now {
                    thread::sleep(at - now);
                }
                at
            }
            None => Instant::now(),
        };
        let spec_idx = mix::pick(&mut rng, cfg.only);
        let spec = &WEIGHTS[spec_idx];
        let built = (spec.build)(&mut rng, &cfg.ctx);
        let bearer = built.user.map(|u| tokens.get(u).to_owned());
        let req = Request {
            method: built.method,
            path: &built.path,
            bearer: bearer.as_deref(),
            body: built.body.as_deref(),
        };
        let result = client.send(&req);
        let micros = scheduled.elapsed().as_micros() as u64;
        if shared.measuring.load(Ordering::Relaxed) {
            match result {
                Ok(r) if (200..300).contains(&r.status) => {
                    local.eps[spec_idx].hist.record(micros);
                    local.all.record(micros);
                }
                other => {
                    local.eps[spec_idx].errors += 1;
                    local.errors += 1;
                    if local.error_samples.len() < MAX_ERROR_SAMPLES {
                        local.error_samples.push(match other {
                            Ok(r) => format!(
                                "{} {} -> {} {}",
                                built.method,
                                built.path,
                                r.status,
                                String::from_utf8_lossy(&r.body_head)
                            ),
                            Err(e) => format!("{} {} -> {e}", built.method, built.path),
                        });
                    }
                }
            }
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            local.connects += client.connects - connects_reported;
            connects_reported = client.connects;
            shared.flush(&mut local);
            last_flush = Instant::now();
        }
    }
    local.connects += client.connects - connects_reported;
    shared.flush(&mut local);
}

/// 요청을 거의 보내지 않고 연결만 붙들고 있는 클라이언트(유휴 keep-alive).
fn idler(cfg: &LoadCfg, shared: &Shared) {
    let mut client = Client::new(cfg.addr, true);
    let req = Request {
        method: "GET",
        path: "/actuator/health",
        bearer: None,
        body: None,
    };
    while !shared.stop.load(Ordering::Relaxed) {
        let _ = client.send(&req);
        // 같은 연결로 15초에 한 번만 요청한다.
        for _ in 0..150 {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// 부하를 시작한다. 반환된 핸들로 구간 통계를 읽고 `finish`로 끝낸다.
pub struct Running {
    pub shared: Arc<Shared>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl Running {
    pub fn start(cfg: &LoadCfg) -> Running {
        let shared = Shared::new();
        let mut handles = Vec::new();
        for i in 0..cfg.idle_conns {
            let (c, s) = (cfg.clone(), Arc::clone(&shared));
            let _ = i;
            handles.push(thread::spawn(move || idler(&c, &s)));
        }
        for i in 0..cfg.conns {
            let (c, s) = (cfg.clone(), Arc::clone(&shared));
            handles.push(
                thread::Builder::new()
                    .stack_size(256 * 1024)
                    .spawn(move || worker(&c, &s, i))
                    .expect("스레드를 만들 수 없습니다"),
            );
        }
        Running { shared, handles }
    }

    /// 측정을 시작한다(그전 통계는 버린다).
    pub fn begin_measuring(&self) {
        self.shared.cum.lock().unwrap().clear();
        self.shared.interval.lock().unwrap().clear();
        self.shared.measuring.store(true, Ordering::Relaxed);
    }

    pub fn finish(self) -> Stats {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.measuring.store(false, Ordering::Relaxed);
        for h in self.handles {
            let _ = h.join();
        }
        self.shared.cum.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 모든 요청에 200을 주는 가짜 서버(연결마다 스레드). 받은 요청 수를 센다.
    fn fake_server() -> (SocketAddr, Arc<std::sync::atomic::AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = Arc::clone(&count);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let c = Arc::clone(&c);
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let mut pending = Vec::new();
                    loop {
                        let Ok(n) = s.read(&mut buf) else { return };
                        if n == 0 {
                            return;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&pending[..p]).to_string();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("Content-Length: "))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if pending.len() < p + 4 + len {
                                break;
                            }
                            pending.drain(..p + 4 + len);
                            c.fetch_add(1, Ordering::Relaxed);
                            if s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (addr, count)
    }

    fn cfg(addr: SocketAddr) -> LoadCfg {
        LoadCfg {
            addr,
            conns: 4,
            duration: Duration::from_millis(600),
            warmup: Duration::ZERO,
            rps: None,
            only: None,
            keep_alive: true,
            idle_conns: 0,
            secret_hex: "00ff00ff".into(),
            ctx: Ctx::new(300, 60, 150, 1_791_774_000),
            seed: 1,
        }
    }

    #[test]
    fn 최대_처리량_모드는_요청을_계속_보낸다() {
        let (addr, served) = fake_server();
        let c = cfg(addr);
        let run = Running::start(&c);
        run.begin_measuring();
        thread::sleep(c.duration);
        let stats = run.finish();
        assert!(stats.all.count() > 100, "{}", stats.all.count());
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.connects, 4, "keep-alive라 연결당 한 번");
        assert!(served.load(Ordering::Relaxed) >= stats.all.count());
    }

    #[test]
    fn 고정_rps_모드는_목표_처리율에_가깝다() {
        let (addr, _) = fake_server();
        let mut c = cfg(addr);
        c.rps = Some(200.0);
        c.duration = Duration::from_millis(1500);
        let run = Running::start(&c);
        run.begin_measuring();
        thread::sleep(c.duration);
        let stats = run.finish();
        let n = stats.all.count();
        assert!(
            (200..=400).contains(&n),
            "1.5초 x 200rps 근처여야 한다: {n}"
        );
    }

    #[test]
    fn 한_엔드포인트만_보낼_수_있다() {
        let (addr, _) = fake_server();
        let mut c = cfg(addr);
        let only = mix::index_of("GET /api/v1/topics").unwrap();
        c.only = Some(only);
        let run = Running::start(&c);
        run.begin_measuring();
        thread::sleep(Duration::from_millis(300));
        let stats = run.finish();
        for (i, ep) in stats.eps.iter().enumerate() {
            assert_eq!(ep.hist.count() > 0, i == only, "{}", WEIGHTS[i].name);
        }
    }

    #[test]
    fn 응답이_2xx가_아니면_오류로_센다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while s.read(&mut buf).is_ok_and(|n| n > 0) {
                        if s.write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom",
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        let c = cfg(addr);
        let run = Running::start(&c);
        run.begin_measuring();
        thread::sleep(Duration::from_millis(300));
        let stats = run.finish();
        assert!(stats.errors > 0);
        assert_eq!(stats.all.count(), 0);
        assert!(
            stats.error_samples[0].contains("500 boom"),
            "{:?}",
            stats.error_samples
        );
    }

    #[test]
    fn 유휴_연결도_붙어_있다() {
        let (addr, served) = fake_server();
        let mut c = cfg(addr);
        c.conns = 1;
        c.idle_conns = 5;
        c.rps = Some(5.0);
        let run = Running::start(&c);
        thread::sleep(Duration::from_millis(400));
        run.finish();
        assert!(
            served.load(Ordering::Relaxed) >= 5,
            "유휴 연결이 각각 요청 하나를 보낸다"
        );
    }
}
