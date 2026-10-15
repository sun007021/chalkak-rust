//! `stress`용 부하 엔진. `load.rs`와 달리 단계(phase)별로 통계를 나누고 오류를 종류별로 센다.
//!
//! - 결과를 `Recorder`가 모은다: 단계별 지연 히스토그램·오류 종류, 낮은 부하 프로브의 1초 창별 p99, 오류가 난 시각, 연결(슬롯)별 완료 수.
//! - 워커는 연결마다 스레드 하나(스택을 작게). 최대 처리량(닫힌 루프) 또는 고정 간격(보내기로 한 시각부터 잼)이다.
//! - 지연은 연결을 새로 맺는 시간까지 포함한다(keep-alive를 끈 워커).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::client::{Client, Request};
use crate::hist::Hist;
use crate::load::Tokens;
use crate::mix::{self, Ctx, Rng, WEIGHTS};
use crate::stress_judge::{ErrCounts, ErrKind, PhaseSummary, Window, classify_io, classify_status};

/// 부하 스레드의 스택 크기. 수천 개를 띄우므로 작게 둔다(요청 만들기·토큰 서명은 몇 KB면 된다).
pub const WORKER_STACK: usize = 128 * 1024;

#[derive(Default)]
struct Acc {
    hist: Hist,
    ok: u64,
    errors: ErrCounts,
    over_1s: u64,
    over_5s: u64,
    over_10s: u64,
    begun: Option<Instant>,
    secs: f64,
}

struct Inner {
    phases: Vec<Acc>,
    /// 프로브가 성공한 요청의 1초 창별 지연(기록 시작부터의 초가 인덱스).
    probe: Vec<Hist>,
    errors: Vec<(u64, ErrKind)>,
    slot_ok: Vec<u64>,
}

pub struct Recorder {
    start: Instant,
    phase: AtomicUsize,
    inner: Mutex<Inner>,
}

/// 단계 하나가 `over_*`까지 담은 요약.
pub struct PhaseDetail {
    pub summary: PhaseSummary,
    pub over_1s: u64,
    pub over_5s: u64,
    pub over_10s: u64,
}

impl Recorder {
    pub fn new(phases: usize, slots: usize) -> Arc<Recorder> {
        Arc::new(Recorder {
            start: Instant::now(),
            phase: AtomicUsize::new(0),
            inner: Mutex::new(Inner {
                phases: (0..phases).map(|_| Acc::default()).collect(),
                probe: Vec::new(),
                errors: Vec::new(),
                slot_ok: vec![0; slots],
            }),
        })
    }

    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn phase(&self) -> usize {
        self.phase.load(Ordering::Relaxed)
    }

    /// 단계를 시작한다(앞 단계가 열려 있으면 닫는다).
    pub fn begin_phase(&self, i: usize) {
        let prev = self.phase.swap(i, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        if prev != i
            && let Some(acc) = g.phases.get_mut(prev)
            && let Some(b) = acc.begun
            && acc.secs == 0.0
        {
            acc.secs = b.elapsed().as_secs_f64();
        }
        if let Some(acc) = g.phases.get_mut(i) {
            acc.begun = Some(Instant::now());
            acc.secs = 0.0;
        }
    }

    /// 지금 단계를 닫는다(길이를 확정).
    pub fn end_phase(&self) {
        let i = self.phase();
        let mut g = self.inner.lock().unwrap();
        if let Some(acc) = g.phases.get_mut(i)
            && let Some(b) = acc.begun
            && acc.secs == 0.0
        {
            acc.secs = b.elapsed().as_secs_f64();
        }
    }

    pub fn record(
        &self,
        slot: usize,
        probe: bool,
        phase: usize,
        micros: u64,
        outcome: Result<(), ErrKind>,
    ) {
        let at_ms = self.now_ms();
        let mut g = self.inner.lock().unwrap();
        let Some(acc) = g.phases.get_mut(phase) else {
            return;
        };
        match outcome {
            Ok(()) => {
                acc.ok += 1;
                acc.hist.record(micros);
                acc.over_1s += u64::from(micros > 1_000_000);
                acc.over_5s += u64::from(micros > 5_000_000);
                acc.over_10s += u64::from(micros > 10_000_000);
                if probe {
                    let sec = (at_ms / 1000) as usize;
                    if g.probe.len() <= sec {
                        g.probe.resize_with(sec + 1, Hist::default);
                    }
                    g.probe[sec].record(micros);
                }
                if let Some(c) = g.slot_ok.get_mut(slot) {
                    *c += 1;
                }
            }
            Err(kind) => {
                acc.errors.add(kind);
                g.errors.push((at_ms, kind));
            }
        }
    }

    pub fn detail(&self, i: usize, name: &str) -> PhaseDetail {
        let g = self.inner.lock().unwrap();
        let empty = Acc::default();
        let acc = g.phases.get(i).unwrap_or(&empty);
        let secs = if acc.secs > 0.0 {
            acc.secs
        } else {
            acc.begun.map_or(0.0, |b| b.elapsed().as_secs_f64())
        };
        PhaseDetail {
            summary: PhaseSummary {
                name: name.to_owned(),
                ok: acc.ok,
                errors: acc.errors.clone(),
                p50_us: acc.hist.quantile(0.50),
                p99_us: acc.hist.quantile(0.99),
                p999_us: acc.hist.quantile(0.999),
                max_us: acc.hist.max(),
                secs,
            },
            over_1s: acc.over_1s,
            over_5s: acc.over_5s,
            over_10s: acc.over_10s,
        }
    }

    pub fn summary(&self, i: usize, name: &str) -> PhaseSummary {
        self.detail(i, name).summary
    }

    /// 프로브 1초 창(표본이 없는 초는 건너뛴다).
    pub fn windows(&self) -> Vec<Window> {
        let g = self.inner.lock().unwrap();
        g.probe
            .iter()
            .enumerate()
            .filter(|(_, h)| h.count() > 0)
            .map(|(s, h)| Window {
                second: s as u64,
                count: h.count(),
                p99_us: h.quantile(0.99),
            })
            .collect()
    }

    pub fn errors(&self) -> Vec<(u64, ErrKind)> {
        self.inner.lock().unwrap().errors.clone()
    }

    pub fn error_times(&self) -> Vec<u64> {
        self.errors().iter().map(|(t, _)| *t).collect()
    }

    /// `[from_ms, to_ms)`의 오류를 종류별로 센다.
    pub fn errors_between(&self, from_ms: u64, to_ms: u64) -> ErrCounts {
        let mut c = ErrCounts::default();
        for (t, k) in self.errors() {
            if t >= from_ms && t < to_ms {
                c.add(k);
            }
        }
        c
    }

    pub fn slot_counts(&self, from: usize, to: usize) -> Vec<u64> {
        let g = self.inner.lock().unwrap();
        g.slot_ok[from.min(g.slot_ok.len())..to.min(g.slot_ok.len())].to_vec()
    }
}

/// 한 응답/실패를 결과 종류로.
pub fn outcome_of(
    result: &std::io::Result<crate::client::Response>,
    connect_failed: bool,
) -> Result<(), ErrKind> {
    match result {
        Ok(r) => classify_status(r.status).map_or(Ok(()), Err),
        Err(e) => Err(classify_io(e.kind(), connect_failed)),
    }
}

#[derive(Clone)]
pub struct MixCfg {
    pub addr: SocketAddr,
    pub secret: String,
    pub ctx: Ctx,
    /// 한 엔드포인트만(`mix::index_of`).
    pub only: Option<usize>,
    pub keep_alive: bool,
    /// 워커 하나가 요청을 보내는 간격. `None`이면 응답을 받자마자 다음 요청(최대 처리량).
    pub interval: Option<Duration>,
    pub io_timeout: Duration,
    pub seed: u64,
    /// 프로브(낮은 부하 기준선) 워커인가: 1초 창 통계를 남긴다.
    pub probe: bool,
}

/// 같이 멈추는 워커 묶음.
pub struct Group {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    /// 스레드를 못 만든 수(부하 발생기 쪽 자원 한도).
    pub spawn_failures: u64,
}

impl Group {
    /// 멈추라고 알리기만 한다(새 요청을 더 보내지 않는다). 끝나길 기다리려면 `join`.
    pub fn signal(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn join(self) {
        for h in self.handles {
            let _ = h.join();
        }
    }

    /// 멈추라고 알리고 모두 끝나길 기다린다.
    pub fn stop(self) {
        self.signal();
        self.join();
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 워커 `count`개를 띄운다. `first_slot`부터 슬롯 번호(연결별 완료 수 기록)를 쓴다.
/// 고정 간격이면 시작 시각을 어긋나게 해 한꺼번에 몰리지 않게 한다.
pub fn spawn_mix(cfg: &MixCfg, count: usize, first_slot: usize, rec: &Arc<Recorder>) -> Group {
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(count);
    let mut spawn_failures = 0;
    for i in 0..count {
        let (cfg, rec, stop2) = (cfg.clone(), Arc::clone(rec), Arc::clone(&stop));
        let slot = first_slot + i;
        let stagger = cfg
            .interval
            .map(|d| d.mul_f64(i as f64 / count.max(1) as f64))
            .unwrap_or_default();
        match thread::Builder::new()
            .stack_size(WORKER_STACK)
            .spawn(move || worker(&cfg, &rec, &stop2, slot, stagger))
        {
            Ok(h) => handles.push(h),
            Err(_) => spawn_failures += 1,
        }
    }
    Group {
        stop,
        handles,
        spawn_failures,
    }
}

fn worker(cfg: &MixCfg, rec: &Recorder, stop: &AtomicBool, slot: usize, stagger: Duration) {
    let mut client = Client::new(cfg.addr, cfg.keep_alive)
        .with_timeouts(Duration::from_secs(10), cfg.io_timeout);
    let mut rng = Rng::new(cfg.seed.wrapping_add(slot as u64 * 7919 + 1));
    let mut tokens = Tokens::new(&cfg.secret, cfg.ctx.users);
    let mut next = Instant::now() + stagger;
    while !stop.load(Ordering::Relaxed) {
        let scheduled = match cfg.interval {
            Some(step) => {
                let at = next;
                next += step;
                let now = Instant::now();
                if at > now {
                    // 길게 자지 않고 쪼개서 멈춤 신호를 본다.
                    let mut left = at - now;
                    while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
                        let nap = left.min(Duration::from_millis(100));
                        thread::sleep(nap);
                        left = left.saturating_sub(nap);
                    }
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                }
                at
            }
            None => Instant::now(),
        };
        let spec = &WEIGHTS[mix::pick(&mut rng, cfg.only)];
        let built = (spec.build)(&mut rng, &cfg.ctx);
        let bearer = built.user.map(|u| tokens.get(u).to_owned());
        let phase = rec.phase();
        let result = client.send(&Request {
            method: built.method,
            path: &built.path,
            bearer: bearer.as_deref(),
            body: built.body.as_deref(),
        });
        let micros = scheduled.elapsed().as_micros() as u64;
        let outcome = outcome_of(&result, client.connect_failed);
        rec.record(slot, cfg.probe, phase, micros, outcome);
        if outcome.is_err() && cfg.interval.is_none() {
            // 닫힌 루프가 실패 응답을 빠르게 돌며 서버를 두들기지 않게 잠깐 쉰다.
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// 요청 하나를 새 연결로 보내고(keep-alive 없음) 결과를 센다: 연결 폭주용.
pub fn one_shot(
    addr: SocketAddr,
    path: &str,
    connect_timeout: Duration,
    io_timeout: Duration,
) -> (Result<(), ErrKind>, u64) {
    let mut client = Client::new(addr, false).with_timeouts(connect_timeout, io_timeout);
    let t0 = Instant::now();
    let result = client.send(&Request {
        method: "GET",
        path,
        bearer: None,
        body: None,
    });
    let micros = t0.elapsed().as_micros() as u64;
    (outcome_of(&result, client.connect_failed), micros)
}

/// 일회용 토큰 헬퍼(요청 하나만 만들 때).
pub fn user_token(secret: &str, user: u64) -> String {
    let signer =
        crate::token::Signer::from_hex(secret).expect("ACCESS_TOKEN_SECRET은 hex여야 한다");
    signer.token(
        &crate::token::user_uuid(user),
        crate::token::Scope::User,
        unix_now() - 5,
        900,
        &format!("stress-{user}-{}", unix_now()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn 단계별로_성공과_오류를_센다() {
        let rec = Recorder::new(2, 4);
        rec.begin_phase(0);
        rec.record(0, false, 0, 1_000, Ok(()));
        rec.record(1, false, 0, 2_000, Ok(()));
        rec.record(1, false, 0, 9, Err(ErrKind::Status503));
        rec.begin_phase(1);
        rec.record(2, false, 1, 3_000, Ok(()));
        rec.record(2, false, 1, 9, Err(ErrKind::Reset));
        rec.end_phase();
        let a = rec.summary(0, "a");
        assert_eq!((a.ok, a.errors.status_503, a.errors.total()), (2, 1, 1));
        assert!(a.secs >= 0.0);
        let b = rec.summary(1, "b");
        assert_eq!((b.ok, b.errors.reset), (1, 1));
        assert_eq!(rec.errors().len(), 2);
        assert_eq!(rec.slot_counts(0, 4), vec![1, 1, 1, 0]);
    }

    #[test]
    fn 느린_요청_수를_센다() {
        let rec = Recorder::new(1, 1);
        rec.begin_phase(0);
        for us in [500_000, 1_500_000, 6_000_000, 11_000_000] {
            rec.record(0, false, 0, us, Ok(()));
        }
        let d = rec.detail(0, "x");
        assert_eq!((d.over_1s, d.over_5s, d.over_10s), (3, 2, 1));
        assert_eq!(d.summary.max_us, 11_000_000);
    }

    #[test]
    fn 프로브만_1초_창에_남긴다() {
        let rec = Recorder::new(1, 2);
        rec.begin_phase(0);
        rec.record(0, true, 0, 5_000, Ok(()));
        rec.record(1, false, 0, 900_000, Ok(()));
        let w = rec.windows();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].second, w[0].count), (0, 1));
        assert!(w[0].p99_us <= 5_000);
    }

    #[test]
    fn 구간의_오류를_종류별로_센다() {
        let rec = Recorder::new(1, 1);
        rec.begin_phase(0);
        rec.record(0, false, 0, 1, Err(ErrKind::Timeout));
        std::thread::sleep(Duration::from_millis(30));
        let mid = rec.now_ms();
        rec.record(0, false, 0, 1, Err(ErrKind::Status500));
        let after = rec.errors_between(mid, mid + 1000);
        assert_eq!((after.status_500, after.timeout), (1, 0));
        assert_eq!(rec.errors_between(0, mid + 1000).total(), 2);
    }

    #[test]
    fn 단계_길이는_다음_단계_시작이나_끝에서_정해진다() {
        let rec = Recorder::new(2, 1);
        rec.begin_phase(0);
        std::thread::sleep(Duration::from_millis(60));
        rec.begin_phase(1);
        let a = rec.summary(0, "a");
        assert!(a.secs >= 0.05 && a.secs < 1.0, "{}", a.secs);
        std::thread::sleep(Duration::from_millis(30));
        rec.end_phase();
        assert!(rec.summary(1, "b").secs >= 0.02);
    }

    /// 요청마다 상태 코드를 주고 닫지 않는 가짜 서버.
    fn fake(status: &'static str) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while s.read(&mut buf).is_ok_and(|n| n > 0) {
                        let r = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
                        if s.write_all(r.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    fn cfg(addr: SocketAddr) -> MixCfg {
        MixCfg {
            addr,
            secret: "00ff00ff".into(),
            ctx: Ctx::new(300, 60, 150, 1_791_774_000),
            only: None,
            keep_alive: true,
            interval: None,
            io_timeout: Duration::from_secs(5),
            seed: 1,
            probe: false,
        }
    }

    #[test]
    fn 워커가_성공과_오류를_단계에_기록한다() {
        let rec = Recorder::new(1, 3);
        rec.begin_phase(0);
        let g = spawn_mix(&cfg(fake("200 OK")), 3, 0, &rec);
        thread::sleep(Duration::from_millis(300));
        g.stop();
        let s = rec.summary(0, "x");
        assert!(
            s.ok > 10 && s.errors.total() == 0,
            "{} {:?}",
            s.ok,
            s.errors
        );
        assert!(rec.slot_counts(0, 3).iter().all(|c| *c > 0));

        let rec = Recorder::new(1, 2);
        rec.begin_phase(0);
        let g = spawn_mix(&cfg(fake("503 Service Unavailable")), 2, 0, &rec);
        thread::sleep(Duration::from_millis(300));
        g.stop();
        let s = rec.summary(0, "x");
        assert!(s.ok == 0 && s.errors.status_503 > 3, "{:?}", s.errors);
    }

    #[test]
    fn 연결이_거부되면_연결_실패로_센다() {
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let (outcome, _) = one_shot(closed, "/", Duration::from_secs(1), Duration::from_secs(1));
        assert_eq!(outcome, Err(ErrKind::ConnectRefused));
        let ok = fake("200 OK");
        let (outcome, micros) = one_shot(ok, "/", Duration::from_secs(1), Duration::from_secs(1));
        assert_eq!(outcome, Ok(()));
        assert!(micros < 1_000_000);
    }

    #[test]
    fn 고정_간격_워커는_간격을_지킨다() {
        let rec = Recorder::new(1, 2);
        rec.begin_phase(0);
        let mut c = cfg(fake("200 OK"));
        c.interval = Some(Duration::from_millis(100)); // 워커 2개 -> 약 20 rps
        let g = spawn_mix(&c, 2, 0, &rec);
        thread::sleep(Duration::from_millis(1000));
        g.stop();
        let n = rec.summary(0, "x").ok;
        assert!((14..=26).contains(&n), "{n}");
    }
}
