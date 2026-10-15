//! `stress`: 트래픽이 몰릴 때의 서버·커넥션 풀 안정성을 Java(Tomcat+HikariCP)와 Rust에 같은 조건으로 시험한다.
//!
//! 케이스(`docs/perf/stress.md`): spike(급증), storm(연결 폭주), idle(유휴 연결 많음), slow(느린 클라이언트),
//! fairness(과부하 공정성), dbkill(DB 연결 끊기), outage(DB 중단), abort(클라이언트 중단).
//! 케이스마다 끝에 **누수 검사**를 한다: 앱 DB 연결 수가 풀 크기와 같고 `idle in transaction`이 0이고, 앱 스레드·FD가
//! 시작 수준으로 돌아오는지. 판정 기준과 계산은 `stress_judge.rs`(순수 함수), 부하 엔진은 `stress_load.rs`.
//!
//! 결과는 `--out`(예: `$PERF_OUT/stress-rust.json`)에 케이스별로 합쳐 쓴다(같은 케이스는 덮어쓴다).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use postgres::Client as Pg;
use serde_json::{Value, json};

use crate::args::Args;
use crate::client::{Client, Request, read_response};
use crate::mix::{self, Ctx};
use crate::run_cmd::{connect_db, db_err};
use crate::seed::SeedSize;
use crate::stress_judge::{self as judge, CaseKind, ErrCounts, Runtime, Snapshot};
use crate::stress_load::{self as load, MixCfg, Recorder};
use crate::sys;

/// 부하 발생기 자신의 DB 연결에 붙이는 이름. 앱 연결과 구분한다(누수 검사·연결 끊기에서 제외).
const APP_NAME: &str = "chalkak-loadgen";
/// DB를 안 쓰는 가벼운 공개 엔드포인트(두 서버 모두 인증 없이 `{}`를 준다).
const LIGHT: &str = "/actuator/info";
const DB_ENDPOINT: &str = "GET /api/v1/posts/today";

// ---- 작은 도우미 --------------------------------------------------------------------------------------------

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("docker 실행 실패: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!(
            "docker {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// `/proc/<pid>/net/netstat`(TcpExt)에서 항목 하나. 헤더 줄과 값 줄이 짝으로 나온다.
pub fn parse_netstat(text: &str, key: &str) -> Option<u64> {
    let mut lines = text.lines();
    while let Some(head) = lines.next() {
        let Some(vals) = lines.next() else { break };
        if !head.starts_with("TcpExt:") {
            continue;
        }
        let idx = head.split_whitespace().position(|k| k == key)?;
        return vals.split_whitespace().nth(idx)?.parse().ok();
    }
    None
}

fn read_proc(pid: u32, file: &str) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/{file}")).ok()
}

/// 서버 프로세스가 속한 cgroup(v2)의 `pids.max`·`pids.current`.
fn pids_limits(pid: u32) -> (Option<u64>, Option<u64>) {
    let Some(cg) = read_proc(pid, "cgroup") else {
        return (None, None);
    };
    let Some(path) = cg.lines().find_map(|l| l.strip_prefix("0::")) else {
        return (None, None);
    };
    let dir = format!("/sys/fs/cgroup{}", path.trim());
    let read = |f: &str| {
        std::fs::read_to_string(Path::new(&dir).join(f))
            .ok()
            .and_then(|v| judge::parse_cgroup_limit(&v))
    };
    (read("pids.max"), read("pids.current"))
}

fn loadgen_nofile() -> u64 {
    std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|t| judge::parse_nofile(&t))
        .unwrap_or(1024)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sleep(d: Duration) {
    thread::sleep(d);
}

fn ms_of(us: u64) -> f64 {
    us as f64 / 1000.0
}

// ---- 대상(프로세스·컨테이너) ----------------------------------------------------------------------------------

struct Target {
    addr: SocketAddr,
    secret: String,
    label: String,
    rt: Runtime,
    pool: i64,
    db_url: String,
    app_pid: Option<u32>,
    db_container: Option<String>,
    mix_ctx: Ctx,
    quick: bool,
    settle_secs: u64,
    rps: f64,
}

impl Target {
    /// 시간을 줄이는 `--quick`(스모크용)이면 3분의 1로(최소 3초).
    fn secs(&self, n: u64) -> Duration {
        let s = if self.quick { (n / 3).max(3) } else { n };
        Duration::from_secs(s)
    }

    fn mix(&self, interval: Option<Duration>, probe: bool) -> MixCfg {
        MixCfg {
            addr: self.addr,
            secret: self.secret.clone(),
            ctx: self.mix_ctx.clone(),
            only: None,
            keep_alive: true,
            interval,
            io_timeout: Duration::from_secs(60),
            seed: 1,
            probe,
        }
    }

    /// 고정 RPS(`rps`)로 연결 `conns`개가 나눠 보내는 간격.
    fn paced(&self, conns: usize, rps: f64) -> Option<Duration> {
        Some(Duration::from_secs_f64(conns as f64 / rps.max(0.1)))
    }

    fn get_status(&self, path: &str) -> Option<u16> {
        let mut c = Client::new(self.addr, false)
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(40));
        c.send(&Request {
            method: "GET",
            path,
            bearer: None,
            body: None,
        })
        .ok()
        .map(|r| r.status)
    }

    fn health_up(&self) -> bool {
        self.get_status("/actuator/health") == Some(200)
    }

    fn server_alive(&self) -> bool {
        self.get_status(LIGHT) == Some(200)
    }

    /// DB를 쓰는 요청 하나가 성공하는가.
    fn db_request_ok(&self) -> bool {
        let token = load::user_token(&self.secret, 3);
        let mut c = Client::new(self.addr, false)
            .with_timeouts(Duration::from_secs(5), Duration::from_secs(40));
        c.send(&Request {
            method: "GET",
            path: "/api/v1/posts/today",
            bearer: Some(&token),
            body: None,
        })
        .is_ok_and(|r| r.status == 200)
    }

    /// 앱 연결 수·idle in transaction·스레드·FD 스냅샷.
    fn snapshot(&self, obs: &mut Obs) -> Snapshot {
        let (conns, idle_tx) = obs.app_conns().unwrap_or((-1, -1));
        let (threads, fds) = self
            .app_pid
            .and_then(|p| sys::sample(p).ok())
            .map_or((0, 0), |s| (s.threads, s.fds));
        Snapshot {
            app_db_conns: conns,
            idle_in_tx: idle_tx,
            threads,
            fds,
        }
    }

    /// 스냅샷이 누수 기준을 통과할 때까지(또는 `settle_secs`) 1초마다 다시 본다.
    fn settle(&self, obs: &mut Obs, before: &Snapshot) -> (Snapshot, Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(self.settle_secs);
        loop {
            let after = self.snapshot(obs);
            let f = judge::leak_failures(before, &after, self.pool, self.rt);
            if f.is_empty() || Instant::now() >= deadline {
                return (after, f);
            }
            sleep(Duration::from_secs(1));
        }
    }

    /// 풀을 가득 채운다: DB 연결을 쓰는 요청 64개를 동시에 보낸다. Hikari는 알아서 채우지만 Rust 풀은 필요할 때만 맺으므로
    /// (최소 유휴 유지 없음) 끊기·중단 뒤 `풀 크기와 같다`를 보려면 먼저 동시 요청으로 풀을 키워야 한다.
    fn refill_pool(&self) -> usize {
        let ok = Arc::new(AtomicU64::new(0));
        let handles: Vec<_> = (0..64u64)
            .filter_map(|i| {
                let (secret, addr, ok) = (self.secret.clone(), self.addr, Arc::clone(&ok));
                thread::Builder::new()
                    .stack_size(load::WORKER_STACK)
                    .spawn(move || {
                        let token = load::user_token(&secret, 1 + i);
                        let mut c = Client::new(addr, false)
                            .with_timeouts(Duration::from_secs(5), Duration::from_secs(40));
                        let good = (0..3).all(|_| {
                            c.send(&Request {
                                method: "GET",
                                path: "/api/v1/posts/today",
                                bearer: Some(&token),
                                body: None,
                            })
                            .is_ok_and(|r| r.status == 200)
                        });
                        if good {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                    .ok()
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
        ok.load(Ordering::Relaxed) as usize
    }

    /// 서버 컨테이너의 한도(nofile, pids)와 현재 사용량. 연결 수가 많은 시험이 한도에 걸렸는지 가리는 근거다.
    fn server_limits(&self) -> Value {
        let Some(pid) = self.app_pid else {
            return Value::Null;
        };
        let nofile = read_proc(pid, "limits").and_then(|t| judge::parse_nofile(&t));
        let (pmax, pcur) = pids_limits(pid);
        let s = sys::sample(pid).ok();
        json!({
            "nofile": nofile,
            "fds": s.map(|s| s.fds),
            "threads": s.map(|s| s.threads),
            "pids_max": pmax,
            "pids_current": pcur,
            "loadgen_nofile": loadgen_nofile(),
        })
    }

    fn netstat(&self, key: &str) -> Option<u64> {
        self.app_pid
            .and_then(|p| read_proc(p, "net/netstat"))
            .and_then(|t| parse_netstat(&t, key))
    }
}

/// 관찰용 DB 연결(이름이 `chalkak-loadgen`이라 앱 연결에서 제외된다). 끊기면 다시 맺는다.
struct Obs {
    url: String,
    db: Option<Pg>,
}

impl Obs {
    fn new(url: &str) -> Result<Obs, String> {
        let url = format!("{url} application_name={APP_NAME}");
        let db = connect_db(&url)?;
        Ok(Obs { url, db: Some(db) })
    }

    fn db(&mut self) -> Result<&mut Pg, String> {
        if self.db.as_ref().is_none_or(Pg::is_closed) {
            self.db = Some(connect_db(&self.url)?);
        }
        Ok(self.db.as_mut().expect("방금 맺음"))
    }

    fn reconnect(&mut self) {
        self.db = None;
    }

    /// (앱 연결 수, idle in transaction 수).
    fn app_conns(&mut self) -> Result<(i64, i64), String> {
        let r = self
            .db()?
            .query_one(
                "SELECT count(*)::bigint, count(*) FILTER (WHERE state LIKE 'idle in transaction%')::bigint \
                 FROM pg_stat_activity \
                 WHERE datname = current_database() AND backend_type = 'client backend' \
                   AND pid <> pg_backend_pid() AND application_name <> $1",
                &[&APP_NAME],
            )
            .map_err(db_err);
        match r {
            Ok(row) => Ok((row.get(0), row.get(1))),
            Err(e) => {
                self.reconnect();
                Err(e)
            }
        }
    }

    fn scalar(&mut self, sql: &str) -> Result<i64, String> {
        self.db()?
            .query_one(sql, &[])
            .and_then(|r| r.try_get(0))
            .map_err(db_err)
    }

    /// 앱 연결을 모두 끊는다(부하 발생기 자신의 연결 제외). 끊은 수를 돌려준다.
    fn kill_app_conns(&mut self) -> Result<i64, String> {
        self.scalar(
            "SELECT count(pg_terminate_backend(pid))::bigint FROM pg_stat_activity \
             WHERE usename = 'chalkak' AND pid <> pg_backend_pid() \
               AND datname = current_database() AND application_name <> 'chalkak-loadgen'",
        )
    }
}

// ---- 표본 수집(DB 포화·서버 자원) ---------------------------------------------------------------------------

#[derive(Default)]
struct SamplerOut {
    ticks: u64,
    saturated_ticks: u64,
    peak_active: i64,
    threads_max: u64,
    fds_max: u64,
    rss_mb_max: f64,
    cpu_cores: f64,
}

impl SamplerOut {
    fn to_json(&self) -> Value {
        json!({
            "peak_active_db": self.peak_active,
            "pool_saturated_ratio": if self.ticks > 0 { self.saturated_ticks as f64 / self.ticks as f64 } else { 0.0 },
            "threads_max": self.threads_max,
            "fds_max": self.fds_max,
            "rss_mb_max": self.rss_mb_max,
            "cpu_cores_avg": self.cpu_cores,
        })
    }
}

/// 0.25초마다 앱의 활성 DB 세션 수와 서버 프로세스 자원을 읽는 스레드. DB 연결이 막혀 있어도 요청을 막지 않게 따로 둔다.
struct Sampler {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<SamplerOut>,
}

impl Sampler {
    fn start(t: &Target) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let (url, pid, pool) = (
            format!("{} application_name={APP_NAME}", t.db_url),
            t.app_pid,
            t.pool,
        );
        let handle = thread::spawn(move || {
            let mut out = SamplerOut::default();
            let mut db = connect_db(&url).ok();
            let first = pid.and_then(|p| sys::sample(p).ok());
            let started = Instant::now();
            let mut last = first;
            while !s.load(Ordering::Relaxed) {
                if let Some(d) = db.as_mut() {
                    let active = d
                        .query_one(
                            "SELECT count(*)::bigint FROM pg_stat_activity WHERE datname = current_database() \
                             AND backend_type = 'client backend' AND application_name <> $1 AND state = 'active'",
                            &[&APP_NAME],
                        )
                        .map(|r| r.get::<_, i64>(0));
                    match active {
                        Ok(a) => {
                            out.ticks += 1;
                            out.peak_active = out.peak_active.max(a);
                            out.saturated_ticks += u64::from(a >= pool);
                        }
                        Err(_) => db = connect_db(&url).ok(),
                    }
                }
                if let Some(smp) = pid.and_then(|p| sys::sample(p).ok()) {
                    out.threads_max = out.threads_max.max(smp.threads);
                    out.fds_max = out.fds_max.max(smp.fds);
                    out.rss_mb_max = out.rss_mb_max.max(smp.rss_kb as f64 / 1024.0);
                    last = Some(smp);
                }
                sleep(Duration::from_millis(250));
            }
            if let (Some(a), Some(b)) = (first, last) {
                let secs = started.elapsed().as_secs_f64();
                if secs > 0.0 {
                    out.cpu_cores = b.cpu_ticks.saturating_sub(a.cpu_ticks) as f64 / 100.0 / secs;
                }
            }
            out
        });
        Sampler { stop, handle }
    }

    fn finish(self) -> SamplerOut {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or_default()
    }
}

/// `secs` 동안 서버 프로세스가 쓴 CPU(코어 수)와 그때의 RSS(MB)·스레드·FD.
fn cpu_window(pid: Option<u32>, secs: Duration) -> Value {
    let Some(pid) = pid else {
        sleep(secs);
        return Value::Null;
    };
    let a = sys::sample(pid).ok();
    let t0 = Instant::now();
    sleep(secs);
    let b = sys::sample(pid).ok();
    let el = t0.elapsed().as_secs_f64();
    match (a, b) {
        (Some(a), Some(b)) if el > 0.0 => json!({
            "cpu_cores": b.cpu_ticks.saturating_sub(a.cpu_ticks) as f64 / 100.0 / el,
            "rss_mb": b.rss_kb as f64 / 1024.0,
            "threads": b.threads,
            "fds": b.fds,
            "secs": el,
        }),
        _ => Value::Null,
    }
}

fn cores_of(v: &Value) -> f64 {
    v["cpu_cores"].as_f64().unwrap_or(0.0)
}

// ---- 케이스 결과 --------------------------------------------------------------------------------------------

struct CaseResult {
    name: String,
    case: &'static str,
    size: Option<u64>,
    failures: Vec<String>,
    phases: Vec<judge::PhaseSummary>,
    extra: Value,
    before: Snapshot,
    after: Snapshot,
    millis: u128,
}

impl CaseResult {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "case": self.case,
            "size": self.size,
            "passed": self.failures.is_empty(),
            "failures": self.failures,
            "phases": self.phases.iter().map(judge::PhaseSummary::to_json).collect::<Vec<_>>(),
            "extra": self.extra,
            "leak": { "before": self.before.to_json(), "after": self.after.to_json() },
            "millis": self.millis,
        })
    }
}

/// 케이스 끝: 풀을 채우고(필요하면) 누수 검사를 해서 결과를 만든다.
#[allow(clippy::too_many_arguments)]
fn finish(
    t: &Target,
    obs: &mut Obs,
    kind: CaseKind,
    size: Option<u64>,
    started: Instant,
    before: Snapshot,
    phases: Vec<judge::PhaseSummary>,
    extra: Value,
    mut failures: Vec<String>,
    refill: bool,
) -> CaseResult {
    if refill {
        t.refill_pool();
    }
    let (after, leaks) = t.settle(obs, &before);
    failures.extend(leaks);
    CaseResult {
        name: size.map_or_else(
            || kind.name().to_owned(),
            |n| format!("{}-{n}", kind.name()),
        ),
        case: kind.name(),
        size,
        failures,
        phases,
        extra,
        before,
        after,
        millis: started.elapsed().as_millis(),
    }
}

fn failed_early(t: &Target, kind: CaseKind, size: Option<u64>, why: String) -> CaseResult {
    CaseResult {
        name: size.map_or_else(
            || kind.name().to_owned(),
            |n| format!("{}-{n}", kind.name()),
        ),
        case: kind.name(),
        size,
        failures: vec![why],
        phases: Vec::new(),
        extra: json!({ "limits": t.server_limits() }),
        before: Snapshot::default(),
        after: Snapshot::default(),
        millis: 0,
    }
}

/// 부하 발생기 쪽 한도가 모자라면 원인을 적고 시작하지 않는다.
fn need_fds(t: &Target, sockets: u64) -> Result<(), String> {
    let have = loadgen_nofile();
    let need = judge::loadgen_fds_needed(sockets);
    if have < need {
        return Err(format!(
            "부하 발생기 쪽 한도: nofile {have} < 필요 {need} (ulimit -n을 올려 다시 실행)"
        ));
    }
    let _ = t;
    Ok(())
}

// ---- 1. spike ----------------------------------------------------------------------------------------------

fn spike(t: &Target, obs: &mut Obs, n: u64) -> CaseResult {
    let started = Instant::now();
    if let Err(e) = need_fds(t, n) {
        return failed_early(t, CaseKind::Spike, Some(n), e);
    }
    let before = t.snapshot(obs);
    let low = 8usize;
    let rec = Recorder::new(4, n as usize);
    let probe_cfg = t.mix(t.paced(low, 40.0), true);
    let sampler = Sampler::start(t);

    rec.begin_phase(0); // 예열(기록 안 함)
    let probes = load::spawn_mix(&probe_cfg, low, 0, &rec);
    sleep(Duration::from_secs(3));
    rec.begin_phase(1);
    sleep(t.secs(8));
    rec.begin_phase(2);
    let burst_cfg = t.mix(None, false);
    let burst = load::spawn_mix(&burst_cfg, (n as usize).saturating_sub(low), low, &rec);
    let spawn_failures = burst.spawn_failures;
    sleep(t.secs(20));
    burst.signal();
    let burst_end_ms = rec.now_ms();
    rec.begin_phase(3);
    burst.join();
    sleep(t.secs(15));
    rec.end_phase();
    probes.stop();
    let sample = sampler.finish();

    let (p_before, p_burst, p_after) = (
        rec.summary(1, "낮은 부하(앞)"),
        rec.summary(2, "급증"),
        rec.summary(3, "낮은 부하(뒤)"),
    );
    let windows = rec.windows();
    let after_sec = burst_end_ms.div_ceil(1000);
    let recovery = judge::recovery_secs(&windows, after_sec, p_before.p99_us);
    let mut failures = judge::judge_spike(&p_before, &p_burst, &p_after, recovery);
    if spawn_failures > 0 {
        failures.push(format!("부하 발생기가 스레드 {spawn_failures}개를 못 만듦"));
    }
    let extra = json!({
        "low_conns": low,
        "burst_conns": n,
        "burst_rps": p_burst.rps(),
        "recovery_secs": recovery,
        "recovery_threshold_us": judge::recovery_threshold_us(p_before.p99_us),
        // 풀 대기 시간은 앱이 노출하지 않아 직접 못 잰다. 급증 구간의 최대 지연이 풀 대기의 상한이다.
        "pool_wait_upper_bound_us": p_burst.max_us,
        // 급증이 끝난 시점(0초) 기준 프로브 1초 창별 (초, 표본 수, p99 ms). 급증 앞 5초부터 보인다.
        "probe_timeline": windows
            .iter()
            .filter(|w| w.second + 5 >= after_sec)
            .map(|w| json!([w.second as i64 - after_sec as i64, w.count, w.p99_us as f64 / 1000.0]))
            .collect::<Vec<_>>(),
        "sampler": sample.to_json(),
        "loadgen_spawn_failures": spawn_failures,
    });
    finish(
        t,
        obs,
        CaseKind::Spike,
        Some(n),
        started,
        before,
        vec![p_before, p_burst, p_after],
        extra,
        failures,
        false,
    )
}

// ---- 2. connect storm --------------------------------------------------------------------------------------

fn storm(t: &Target, obs: &mut Obs, n: u64) -> CaseResult {
    let started = Instant::now();
    if let Err(e) = need_fds(t, n) {
        return failed_early(t, CaseKind::Storm, Some(n), e);
    }
    let before = t.snapshot(obs);
    let overflows0 = t.netstat("ListenOverflows");
    let drops0 = t.netstat("ListenDrops");
    let rec = Recorder::new(1, 1);
    rec.begin_phase(0);
    // 출발 신호: 스레드가 조건 변수에서 자다가 한꺼번에 깬다(깨어서 도는 대기는 수천 개일 때 부하 발생기를 먼저 태운다).
    let go = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let mut handles = Vec::new();
    let mut spawn_failures = 0u64;
    for _ in 0..n {
        let (rec, go, addr) = (Arc::clone(&rec), Arc::clone(&go), t.addr);
        match thread::Builder::new()
            .stack_size(load::WORKER_STACK)
            .spawn(move || {
                {
                    let (lock, cv) = &*go;
                    let mut started = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !*started {
                        started = cv.wait(started).unwrap_or_else(|e| e.into_inner());
                    }
                }
                let (outcome, micros) = load::one_shot(
                    addr,
                    LIGHT,
                    Duration::from_secs(10),
                    Duration::from_secs(30),
                );
                rec.record(0, false, 0, micros, outcome);
            }) {
            Ok(h) => handles.push(h),
            Err(_) => spawn_failures += 1,
        }
    }
    sleep(Duration::from_millis(300)); // 스레드가 모두 준비되게
    let t0 = Instant::now();
    {
        let (lock, cv) = &*go;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
    for h in handles {
        let _ = h.join();
    }
    let wall = t0.elapsed().as_secs_f64();
    rec.end_phase();
    let phase = rec.summary(0, "폭주");
    let probe_ok = (0..5).any(|_| {
        let ok = t.server_alive();
        if !ok {
            sleep(Duration::from_secs(1));
        }
        ok
    });
    let mut failures = judge::judge_storm(n, phase.ok, &phase.errors, probe_ok);
    if spawn_failures > 0 {
        failures.push(format!(
            "부하 발생기가 스레드 {spawn_failures}개를 못 만듦(연결 {n}개 중)"
        ));
    }
    let delta = |a: Option<u64>, key: &str| match (a, t.netstat(key)) {
        (Some(a), Some(b)) => json!(b.saturating_sub(a)),
        _ => Value::Null,
    };
    let extra = json!({
        "conns": n,
        "wall_secs": wall,
        "listen_overflows": delta(overflows0, "ListenOverflows"),
        "listen_drops": delta(drops0, "ListenDrops"),
        "note": "리슨 백로그: Rust std 기본 128, Tomcat accept-count 100. 커널 SYN 재전송(1·3·7초)이 연결 지연으로 보인다.",
        "limits": t.server_limits(),
        "loadgen_spawn_failures": spawn_failures,
    });
    finish(
        t,
        obs,
        CaseKind::Storm,
        Some(n),
        started,
        before,
        vec![phase],
        extra,
        failures,
        false,
    )
}

// ---- 3. many idle ------------------------------------------------------------------------------------------

/// 요청 하나를 보내고 응답을 읽는다(keep-alive 연결로).
fn ping(stream: &mut TcpStream) -> io::Result<u16> {
    stream.write_all(
        format!("GET {LIGHT} HTTP/1.1\r\nHost: chalkak\r\nAccept: application/json\r\n\r\n")
            .as_bytes(),
    )?;
    let mut r = std::io::BufReader::with_capacity(1024, &*stream);
    read_response(&mut r, false).map(|(resp, _)| resp.status)
}

struct IdleHolder {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<usize>,
    lost: Arc<AtomicU64>,
}

impl IdleHolder {
    /// 소켓들을 쥐고 20초마다 요청 하나씩 보내 keep-alive 제한(60초)에 안 걸리게 한다.
    fn start(mut streams: Vec<TcpStream>) -> IdleHolder {
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicU64::new(0));
        let (s, l) = (Arc::clone(&stop), Arc::clone(&lost));
        let handle = thread::spawn(move || {
            let mut next = Instant::now() + Duration::from_secs(20);
            while !s.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    let before = streams.len();
                    streams.retain_mut(|st| ping(st).is_ok_and(|c| c == 200));
                    l.fetch_add((before - streams.len()) as u64, Ordering::Relaxed);
                    next = Instant::now() + Duration::from_secs(20);
                }
                sleep(Duration::from_millis(100));
            }
            streams.len()
        });
        IdleHolder { stop, handle, lost }
    }

    fn finish(self) -> (usize, u64) {
        self.stop.store(true, Ordering::Relaxed);
        let alive = self.handle.join().unwrap_or(0);
        (alive, self.lost.load(Ordering::Relaxed))
    }
}

fn idle(t: &Target, obs: &mut Obs, n: u64) -> CaseResult {
    let started = Instant::now();
    if let Err(e) = need_fds(t, n) {
        return failed_early(t, CaseKind::Idle, Some(n), e);
    }
    let before = t.snapshot(obs);
    let conns = 16usize;
    let cfg = t.mix(t.paced(conns, t.rps), false);
    let rec = Recorder::new(3, conns);

    // 1) 유휴 연결 없이 서버만(CPU 기준선), 2) 유휴 연결 없이 일반 부하(지연 기준선)
    let cpu_empty = cpu_window(t.app_pid, t.secs(8));
    rec.begin_phase(0);
    let g = load::spawn_mix(&cfg, conns, 0, &rec);
    sleep(t.secs(15));
    g.stop();
    rec.end_phase();

    // 3) 유휴 연결 N개를 붙인다
    let mut streams = Vec::with_capacity(n as usize);
    let mut open_errors = ErrCounts::default();
    let t_open = Instant::now();
    for _ in 0..n {
        match TcpStream::connect_timeout(&t.addr, Duration::from_secs(5)) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
                streams.push(s);
            }
            Err(e) => open_errors.add(judge::classify_io(e.kind(), true)),
        }
    }
    // 모두 한 번씩 요청해서 실제 keep-alive 연결로 만든다.
    let connected = streams.len();
    streams.retain_mut(|s| ping(s).is_ok_and(|c| c == 200));
    let opened = streams.len() as u64;
    let open_secs = t_open.elapsed().as_secs_f64();
    let open_diag = judge::diagnose_open_failure(
        opened,
        n,
        loadgen_nofile(),
        t.server_limits()["nofile"].as_u64().unwrap_or(u64::MAX),
        t.server_limits()["fds"].as_u64().unwrap_or(0),
        t.server_limits()["pids_max"].as_u64(),
        t.server_limits()["pids_current"].as_u64(),
    );
    let holder = IdleHolder::start(streams);
    sleep(Duration::from_secs(3));
    let after_open = t.server_limits();

    // 4) 유휴 연결만 붙은 채 서버 CPU
    let cpu_idle = cpu_window(t.app_pid, t.secs(10));
    // 5) 같은 일반 부하
    let sampler = Sampler::start(t);
    rec.begin_phase(1);
    let g = load::spawn_mix(&cfg, conns, 0, &rec);
    sleep(t.secs(15));
    g.stop();
    rec.end_phase();
    let sample = sampler.finish();
    let load_phase = rec.summary(1, "유휴 연결 + 일반 부하");
    let base_phase = rec.summary(0, "일반 부하(유휴 연결 없음)");
    let (alive, lost) = holder.finish();

    let idle_cpu = cores_of(&cpu_idle);
    let idle_cpu_net = (idle_cpu - cores_of(&cpu_empty)).max(0.0);
    let mut failures = judge::judge_idle(&judge::IdleInput {
        requested: n,
        opened,
        base_p99_us: base_phase.p99_us,
        load_p99_us: load_phase.p99_us,
        load_errors: load_phase.errors.total(),
        idle_cpu_cores: idle_cpu,
    });
    if lost > 0 {
        failures.push(format!("유휴 연결 {lost}개를 서버가 끊음"));
    }
    if let Some(d) = open_diag {
        failures.push(format!("연결 붙이기 실패 원인: {d}"));
    }
    let extra = json!({
        "idle_conns": n,
        "connected": connected,
        "opened": opened,
        "open_errors": open_errors.to_json(),
        "open_secs": open_secs,
        "still_alive_at_end": alive,
        "lost_during_hold": lost,
        "cpu_empty": cpu_empty,
        "cpu_idle_only": cpu_idle,
        "cpu_idle_net_cores": idle_cpu_net,
        // 유휴 연결 1000개당 서버 CPU(코어). Rust는 유휴 연결 스레드가 POLL_SLICE(100ms)마다 깨어난다.
        "cpu_cores_per_1000_idle": if n > 0 { idle_cpu_net * 1000.0 / n as f64 } else { 0.0 },
        "cpu_under_load": sample.cpu_cores,
        "rss_mb_idle_only": cpu_idle["rss_mb"],
        "threads_idle_only": cpu_idle["threads"],
        "fds_idle_only": cpu_idle["fds"],
        "limits_after_open": after_open,
        "load_rps": t.rps,
        "sampler": sample.to_json(),
    });
    finish(
        t,
        obs,
        CaseKind::Idle,
        Some(n),
        started,
        before,
        vec![base_phase, load_phase],
        extra,
        failures,
        false,
    )
}

// ---- 4. slow clients ---------------------------------------------------------------------------------------

struct SlowOut {
    stalled: bool,
    closed_after: Option<f64>,
    status: Option<u16>,
}

/// 느린 연결 하나. `stalled`면 헤더 앞부분을 한 번 보내고 멈추고, 아니면 `drip`마다 한 바이트씩 보낸다.
/// 서버가 끊었는지(EOF·RST·응답)를 계속 살핀다.
fn slow_conn(addr: SocketAddr, stalled: bool, drip: Duration, stop: Arc<AtomicBool>) -> SlowOut {
    let mut out = SlowOut {
        stalled,
        closed_after: None,
        status: None,
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(10)) else {
        return out;
    };
    let _ = s.set_nodelay(true);
    let t0 = Instant::now();
    let prefix = format!("GET {LIGHT} HTTP/1.1\r\nHost: slow\r\nX-Slow: ");
    let mut to_send: Vec<u8> = prefix.into_bytes();
    to_send.extend(std::iter::repeat_n(b'a', 4096));
    let mut sent = 0usize;
    let _ = s.set_nonblocking(true);
    let mut buf = [0u8; 512];
    let mut check = |s: &mut TcpStream, out: &mut SlowOut| -> bool {
        match s.read(&mut buf) {
            Ok(0) => {
                out.closed_after = Some(t0.elapsed().as_secs_f64());
                true
            }
            Ok(n) => {
                let head = String::from_utf8_lossy(&buf[..n]);
                out.status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok());
                out.closed_after = Some(t0.elapsed().as_secs_f64());
                true
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(_) => {
                out.closed_after = Some(t0.elapsed().as_secs_f64());
                true
            }
        }
    };
    let mut next_byte = Instant::now();
    if stalled {
        // 헤더 앞부분(요청 줄과 헤더 일부)만 한꺼번에 보내고 멈춘다.
        let head_len = to_send.iter().position(|b| *b == b'a').unwrap_or(40).max(1);
        let mut written = 0;
        while written < head_len {
            match s.write(&to_send[written..head_len]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => sleep(Duration::from_millis(5)),
                Err(_) => {
                    out.closed_after = Some(t0.elapsed().as_secs_f64());
                    return out;
                }
            }
        }
        sent = head_len;
        next_byte = Instant::now() + Duration::from_secs(86_400);
    }
    while !stop.load(Ordering::Relaxed) {
        if check(&mut s, &mut out) {
            return out;
        }
        if Instant::now() >= next_byte && sent < to_send.len() {
            match s.write(&to_send[sent..=sent]) {
                Ok(1) => sent += 1,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    out.closed_after = Some(t0.elapsed().as_secs_f64());
                    return out;
                }
            }
            next_byte = Instant::now() + drip;
        }
        sleep(Duration::from_millis(100));
    }
    out
}

fn slow(t: &Target, obs: &mut Obs, n: u64, drip_secs: f64, slow_secs: Option<u64>) -> CaseResult {
    let started = Instant::now();
    if let Err(e) = need_fds(t, n) {
        return failed_early(t, CaseKind::Slow, Some(n), e);
    }
    let before = t.snapshot(obs);
    let conns = 16usize;
    let cfg = t.mix(t.paced(conns, t.rps), false);
    let rec = Recorder::new(2, conns);
    let limit = t.rt.read_timeout_secs();
    // 멈춘 연결이 끊기는 것을 보려면 서버 제한 시간 + 여유만큼 붙들어야 한다(스케일하지 않는다).
    let hold = Duration::from_secs(slow_secs.unwrap_or(limit as u64 + 10));

    rec.begin_phase(0);
    let g = load::spawn_mix(&cfg, conns, 0, &rec);
    sleep(t.secs(10));
    rec.begin_phase(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stalled_n = n / 4;
    let mut handles = Vec::new();
    let mut spawn_failures = 0u64;
    for i in 0..n {
        let stalled = i < stalled_n;
        let (stop, addr) = (Arc::clone(&stop), t.addr);
        match thread::Builder::new()
            .stack_size(load::WORKER_STACK)
            .spawn(move || slow_conn(addr, stalled, Duration::from_secs_f64(drip_secs), stop))
        {
            Ok(h) => handles.push(h),
            Err(_) => spawn_failures += 1,
        }
    }
    let sampler = Sampler::start(t);
    sleep(hold);
    let during = t.server_limits();
    stop.store(true, Ordering::Relaxed);
    let outs: Vec<SlowOut> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
    g.stop();
    rec.end_phase();
    let sample = sampler.finish();

    let base = rec.summary(0, "일반 부하(느린 연결 없음)");
    let normal = rec.summary(1, "일반 부하(느린 연결 있음)");
    let summarize = |stalled: bool| {
        let group: Vec<&SlowOut> = outs.iter().filter(|o| o.stalled == stalled).collect();
        let closed: Vec<f64> = group.iter().filter_map(|o| o.closed_after).collect();
        let mut statuses = std::collections::BTreeMap::new();
        for o in &group {
            if let Some(s) = o.status {
                *statuses.entry(s.to_string()).or_insert(0u64) += 1;
            }
        }
        (
            group.len() as u64,
            closed.len() as u64,
            closed.iter().copied().fold(0.0, f64::max),
            closed.iter().copied().fold(f64::MAX, f64::min),
            statuses,
        )
    };
    let (st_n, st_closed, st_max, st_min, st_status) = summarize(true);
    let (dr_n, dr_closed, dr_max, dr_min, dr_status) = summarize(false);
    let mut failures = judge::judge_slow(&judge::SlowInput {
        normal_errors: normal.errors.total(),
        base_p99_us: base.p99_us,
        normal_p99_us: normal.p99_us,
        stalled: st_n,
        stalled_closed: st_closed,
        stalled_max_close_secs: st_max,
        timeout_limit_secs: limit,
    });
    if spawn_failures > 0 {
        failures.push(format!("부하 발생기가 스레드 {spawn_failures}개를 못 만듦"));
    }
    let extra = json!({
        "slow_conns": n,
        "drip_interval_secs": drip_secs,
        "hold_secs": hold.as_secs(),
        "timeout_limit_secs": limit,
        "stalled": { "conns": st_n, "closed": st_closed, "first_close_secs": if st_closed > 0 { json!(st_min) } else { Value::Null }, "last_close_secs": st_max, "statuses": st_status },
        // 한 바이트씩 보내는 연결: 바이트마다 읽기 타이머가 갱신되는지가 핵심이라 판정하지 않고 기록한다.
        "drip": { "conns": dr_n, "closed": dr_closed, "first_close_secs": if dr_closed > 0 { json!(dr_min) } else { Value::Null }, "last_close_secs": dr_max, "statuses": dr_status },
        "server_during": during,
        "sampler": sample.to_json(),
        "loadgen_spawn_failures": spawn_failures,
    });
    finish(
        t,
        obs,
        CaseKind::Slow,
        Some(n),
        started,
        before,
        vec![base, normal],
        extra,
        failures,
        false,
    )
}

// ---- 5. overload fairness ----------------------------------------------------------------------------------

fn fairness(t: &Target, obs: &mut Obs, n: u64) -> CaseResult {
    let started = Instant::now();
    if let Err(e) = need_fds(t, n) {
        return failed_early(t, CaseKind::Fairness, Some(n), e);
    }
    let before = t.snapshot(obs);
    let rec = Recorder::new(1, n as usize);
    let mut cfg = t.mix(None, false);
    cfg.only = mix::index_of(DB_ENDPOINT);
    let sampler = Sampler::start(t);
    rec.begin_phase(0);
    let g = load::spawn_mix(&cfg, n as usize, 0, &rec);
    let spawn_failures = g.spawn_failures;
    sleep(t.secs(30));
    g.signal();
    rec.end_phase();
    g.join();
    let sample = sampler.finish();
    let d = rec.detail(0, "DB 엔드포인트 집중");
    let counts = rec.slot_counts(0, n as usize);
    let fair = judge::fairness(&counts);
    let mut failures = judge::judge_fairness(&d.summary.errors, fair, d.summary.max_us);
    if spawn_failures > 0 {
        failures.push(format!("부하 발생기가 스레드 {spawn_failures}개를 못 만듦"));
    }
    let extra = json!({
        "conns": n,
        "endpoint": DB_ENDPOINT,
        "p999_us": d.summary.p999_us,
        "over_1s": d.over_1s,
        "over_5s": d.over_5s,
        "over_10s": d.over_10s,
        "pool_wait_limit_exceeded_500": d.summary.errors.status_500,
        "per_conn_completed": fair.map(|f| json!({ "min": f.min, "median": f.median, "max": f.max, "min_over_median": f.ratio })),
        "sampler": sample.to_json(),
    });
    finish(
        t,
        obs,
        CaseKind::Fairness,
        Some(n),
        started,
        before,
        vec![d.summary],
        extra,
        failures,
        false,
    )
}

// ---- 6. db kill --------------------------------------------------------------------------------------------

fn dbkill(t: &Target, obs: &mut Obs) -> CaseResult {
    let started = Instant::now();
    let before = t.snapshot(obs);
    let conns = 16usize;
    let rounds_n = 3;
    let cfg = t.mix(t.paced(conns, t.rps.max(60.0)), false);
    let rec = Recorder::new(1, conns);
    rec.begin_phase(0);
    let g = load::spawn_mix(&cfg, conns, 0, &rec);
    sleep(Duration::from_secs(3));
    let mut rounds = Vec::new();
    let mut detail = Vec::new();
    let mut failures = Vec::new();
    for r in 0..rounds_n {
        sleep(t.secs(5));
        let (conns_before, _) = obs.app_conns().unwrap_or((-1, -1));
        let at = rec.now_ms();
        let killed = obs.kill_app_conns();
        let killed = match killed {
            Ok(k) => k,
            Err(e) => {
                failures.push(format!("연결 끊기 실패: {e}"));
                -1
            }
        };
        sleep(t.secs(10));
        let window = t.secs(10).as_millis() as u64;
        let errs = rec.errors_between(at, at + window);
        let recovery = judge::outage_secs(at, &rec.error_times(), window);
        let (conns_after, idle_tx) = obs.app_conns().unwrap_or((-1, -1));
        detail.push(json!({
            "round": r + 1,
            "killed": killed,
            "app_conns_before_kill": conns_before,
            "errors": errs.to_json(),
            "recovery_secs": recovery,
            "app_conns_after_low_load": conns_after,
            "idle_in_tx": idle_tx,
        }));
        rounds.push(judge::KillRound {
            errors: errs.total(),
            recovery_secs: recovery,
        });
    }
    g.stop();
    rec.end_phase();
    let conns_low_load = obs.app_conns().map(|c| c.0).unwrap_or(-1);
    let refilled = t.refill_pool();
    sleep(Duration::from_secs(1));
    let conns_after_refill = obs.app_conns().map(|c| c.0).unwrap_or(-1);
    let final_ok = (0..20).all(|_| t.db_request_ok());
    failures.extend(judge::judge_dbkill(
        &rounds,
        t.pool,
        conns as u64,
        final_ok,
        conns_after_refill == t.pool,
    ));
    let extra = json!({
        "load_conns": conns,
        "load_rps": t.rps.max(60.0),
        "rounds": detail,
        "app_conns_low_load_end": conns_low_load,
        "refill_ok_clients": refilled,
        "app_conns_after_refill": conns_after_refill,
        "note": "app_conns_low_load_end가 풀 크기보다 작으면 풀이 필요할 때만 연결을 다시 맺는다(Hikari는 백그라운드로 채움).",
    });
    finish(
        t,
        obs,
        CaseKind::DbKill,
        None,
        started,
        before,
        vec![rec.summary(0, "전체")],
        extra,
        failures,
        true,
    )
}

// ---- 7. db outage ------------------------------------------------------------------------------------------

struct Health {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Vec<(u64, u16, u64)>>,
}

impl Health {
    /// 0.5초마다 /actuator/health를 부른다. (기록 시작부터의 ms, 상태(0이면 실패), 걸린 마이크로초).
    fn start(addr: SocketAddr, rec: Arc<Recorder>) -> Health {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut out = Vec::new();
            while !s.load(Ordering::Relaxed) {
                let at = rec.now_ms();
                let t0 = Instant::now();
                let mut c = Client::new(addr, false)
                    .with_timeouts(Duration::from_secs(5), Duration::from_secs(40));
                let status = c
                    .send(&Request {
                        method: "GET",
                        path: "/actuator/health",
                        bearer: None,
                        body: None,
                    })
                    .map_or(0, |r| r.status);
                out.push((at, status, t0.elapsed().as_micros() as u64));
                sleep(Duration::from_millis(500));
            }
            out
        });
        Health { stop, handle }
    }

    fn finish(self) -> Vec<(u64, u16, u64)> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or_default()
    }
}

fn wait_db(obs: &mut Obs, limit: Duration) -> Option<f64> {
    let t0 = Instant::now();
    obs.reconnect();
    while t0.elapsed() < limit {
        if obs.scalar("SELECT 1::bigint").is_ok() {
            return Some(t0.elapsed().as_secs_f64());
        }
        obs.reconnect();
        sleep(Duration::from_millis(500));
    }
    None
}

fn outage(t: &Target, obs: &mut Obs, mode: &str, hold_secs: u64) -> CaseResult {
    let started = Instant::now();
    let name = format!("outage-{mode}");
    let mk = |failures: Vec<String>| CaseResult {
        name: name.clone(),
        case: "outage",
        size: None,
        failures,
        phases: Vec::new(),
        extra: Value::Null,
        before: Snapshot::default(),
        after: Snapshot::default(),
        millis: 0,
    };
    let Some(dbc) = t.db_container.clone() else {
        return mk(vec![
            "--db-container가 필요합니다(앱 DB 컨테이너 ID)".to_owned(),
        ]);
    };
    let before = t.snapshot(obs);
    let rows_before = obs
        .scalar("SELECT count(*)::bigint FROM topics")
        .unwrap_or(-1);
    let conns = 8usize;
    let rps = t.rps.min(30.0);
    let cfg = t.mix(t.paced(conns, rps), false);
    let rec = Recorder::new(4, conns);
    rec.begin_phase(0); // 예열
    let g = load::spawn_mix(&cfg, conns, 0, &rec);
    let health = Health::start(t.addr, Arc::clone(&rec));
    sleep(Duration::from_secs(3));
    rec.begin_phase(1);
    sleep(t.secs(5));

    // 중단
    rec.begin_phase(2);
    let down_at = rec.now_ms();
    let action = match mode {
        "pause" => docker(&["pause", &dbc]),
        _ => docker(&["stop", "-t", "5", &dbc]),
    };
    let mut failures = Vec::new();
    if let Err(e) = action {
        failures.push(format!("DB 중단 명령 실패: {e}"));
    }
    sleep(Duration::from_secs(hold_secs));
    let resume_at = rec.now_ms();
    rec.begin_phase(3);
    let resume = match mode {
        "pause" => docker(&["unpause", &dbc]),
        _ => docker(&["start", &dbc]),
    };
    if let Err(e) = resume {
        failures.push(format!("DB 재개 명령 실패: {e}"));
    }
    let db_back_secs = wait_db(obs, Duration::from_secs(60));
    sleep(t.secs(25));
    rec.end_phase();
    let health_samples = health.finish();
    g.stop();

    let during = rec.summary(2, "DB 중단 중");
    let after = rec.summary(3, "DB 재개 뒤");
    let window = 25_000u64.max(t.secs(25).as_millis() as u64);
    let recovery = judge::outage_secs(resume_at, &rec.error_times(), window);
    let errs_during = rec.errors_between(down_at, resume_at);
    let errs_after = rec.errors_between(resume_at, resume_at + window);
    let h_during: Vec<&(u64, u16, u64)> = health_samples
        .iter()
        .filter(|(at, _, _)| *at >= down_at && *at < resume_at)
        .collect();
    let tally = |v: &[&(u64, u16, u64)]| {
        let mut m = std::collections::BTreeMap::new();
        for (_, s, _) in v {
            *m.entry(s.to_string()).or_insert(0u64) += 1;
        }
        m
    };
    let h_after_up = health_samples
        .iter()
        .find(|(at, s, _)| *at >= resume_at && *s == 200)
        .map(|(at, _, _)| (*at - resume_at) as f64 / 1000.0);
    let h_max_us = h_during.iter().map(|(_, _, us)| *us).max().unwrap_or(0);
    let rows_after = obs
        .scalar("SELECT count(*)::bigint FROM topics")
        .unwrap_or(-1);
    let data_lost = rows_before > 0 && rows_after < rows_before;
    t.refill_pool();
    let health_up = (0..10).any(|_| {
        let up = t.health_up();
        if !up {
            sleep(Duration::from_secs(1));
        }
        up
    });
    failures.extend(judge::judge_outage(&judge::OutageInput {
        recovery_secs: recovery,
        health_up_after: health_up,
        data_lost,
    }));
    let extra = json!({
        "mode": mode,
        "hold_secs": hold_secs,
        "load_conns": conns,
        "load_rps": rps,
        "errors_during": errs_during.to_json(),
        "errors_after_resume": errs_after.to_json(),
        "recovery_secs": recovery,
        "db_ready_after_resume_secs": db_back_secs,
        "health_during": tally(&h_during),
        "health_during_max_us": h_max_us,
        "health_first_up_after_resume_secs": h_after_up,
        "rows_before": rows_before,
        "rows_after": rows_after,
        "data_lost": data_lost,
        "meaning": match mode {
            "pause" => "docker pause: 프로세스를 얼린다. TCP 연결은 그대로 열려 있고 응답만 멈춘다(느려진 DB·망 정지와 비슷). 되살리면 대기하던 질의가 이어진다.",
            _ => "docker stop/start: DB 프로세스가 종료되어 모든 연결이 끊기고 그동안 새 연결은 거부된다(재시작·장애 조치와 비슷). 앱은 끊긴 연결을 버리고 다시 맺어야 한다.",
        },
    });
    let mut r = finish(
        t,
        obs,
        CaseKind::Outage,
        None,
        started,
        before,
        vec![during, after],
        extra,
        failures,
        true,
    );
    r.name = name;
    r
}

// ---- 8. client abort ---------------------------------------------------------------------------------------

fn abort(t: &Target, obs: &mut Obs, requests: usize) -> CaseResult {
    let started = Instant::now();
    let before = t.snapshot(obs);
    let marker = format!("stress-abort-{}", unix_now());
    let mut locker = match connect_db(&format!("{} application_name={APP_NAME}", t.db_url)) {
        Ok(c) => c,
        Err(e) => return failed_early(t, CaseKind::Abort, None, e),
    };
    if let Err(e) = locker.batch_execute("BEGIN; LOCK TABLE feedbacks IN ACCESS EXCLUSIVE MODE") {
        return failed_early(t, CaseKind::Abort, None, db_err(e));
    }
    // 요청 N개를 보내고 응답을 읽지 않는다. 쓰기는 잠금에서 멈추고 풀(10) 밖의 요청은 연결을 기다린다.
    let body = format!(r#"{{"content":"{marker}"}}"#);
    let mut socks = Vec::new();
    let mut connect_failures = 0;
    for i in 0..requests {
        let token = load::user_token(&t.secret, 1 + (i as u64 % 300));
        let req = format!(
            "POST /api/v1/feedbacks HTTP/1.1\r\nHost: chalkak\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        match TcpStream::connect_timeout(&t.addr, Duration::from_secs(5))
            .and_then(|mut s| s.write_all(req.as_bytes()).map(|()| s))
        {
            Ok(s) => socks.push(s),
            Err(_) => connect_failures += 1,
        }
    }
    sleep(Duration::from_secs(2));
    let stuck = t.snapshot(obs);
    let stuck_limits = t.server_limits();
    // 응답 전에 소켓을 닫는다.
    let aborted = socks.len();
    drop(socks);
    sleep(Duration::from_secs(2));
    let during_after_close = t.snapshot(obs);
    let _ = locker.batch_execute("COMMIT");
    sleep(Duration::from_secs(2));
    let rows = obs
        .scalar(&format!(
            "SELECT count(*)::bigint FROM feedbacks WHERE content = '{marker}'"
        ))
        .unwrap_or(-1);
    let probe_ok = (0..10).any(|_| {
        let ok = t.db_request_ok();
        if !ok {
            sleep(Duration::from_secs(1));
        }
        ok
    });
    let _ = obs
        .db()
        .map(|d| d.batch_execute(&format!("DELETE FROM feedbacks WHERE content = '{marker}'")));
    let mut failures = judge::judge_abort(&judge::AbortInput {
        sent: aborted as u64,
        rows_inserted: rows,
        probe_ok,
    });
    if connect_failures > 0 {
        failures.push(format!("요청 {connect_failures}개는 연결/전송에 실패"));
    }
    let extra = json!({
        "sent": aborted,
        "connect_failures": connect_failures,
        "while_stuck": stuck.to_json(),
        "after_close_still_locked": during_after_close.to_json(),
        "server_while_stuck": stuck_limits,
        "rows_inserted_after_unlock": rows,
        "note": "rows_inserted_after_unlock: 끊긴 클라이언트의 요청도 서버가 끝까지 처리했는지(Tomcat·Rust 모두 취소하지 않는다).",
    });
    finish(
        t,
        obs,
        CaseKind::Abort,
        None,
        started,
        before,
        Vec::new(),
        extra,
        failures,
        false,
    )
}

// ---- 명령 ---------------------------------------------------------------------------------------------------

fn merge_out(path: &str, label: &str, results: &[CaseResult]) -> Result<(), String> {
    let mut doc: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(|v: &Value| v["kind"] == "stress")
        .unwrap_or_else(|| json!({ "kind": "stress", "label": label, "cases": [] }));
    let cases = doc["cases"]
        .as_array_mut()
        .ok_or("cases가 배열이 아닙니다")?;
    for r in results {
        let v = r.to_json();
        match cases.iter_mut().find(|c| c["name"] == v["name"]) {
            Some(slot) => *slot = v,
            None => cases.push(v),
        }
    }
    doc["label"] = json!(label);
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap())
        .map_err(|e| format!("{path}: {e}"))
}

fn print_result(r: &CaseResult) {
    let mark = if r.failures.is_empty() {
        "통과"
    } else {
        "실패"
    };
    println!("== {mark} {} ({:.0}초)", r.name, r.millis as f64 / 1000.0);
    for p in &r.phases {
        println!(
            "   {:<28} 성공 {:>7}  오류 {:>5} [{}]  p50 {:>8.1}ms  p99 {:>9.1}ms  max {:>9.1}ms",
            p.name,
            p.ok,
            p.errors.total(),
            p.errors.describe(),
            ms_of(p.p50_us),
            ms_of(p.p99_us),
            ms_of(p.max_us)
        );
    }
    println!(
        "   누수 검사: 앱 DB 연결 {} -> {}, idle in tx {}, 스레드 {} -> {}, FD {} -> {}",
        r.before.app_db_conns,
        r.after.app_db_conns,
        r.after.idle_in_tx,
        r.before.threads,
        r.after.threads,
        r.before.fds,
        r.after.fds
    );
    if !r.extra.is_null() {
        println!("   {}", serde_json::to_string(&r.extra).unwrap_or_default());
    }
    for f in &r.failures {
        println!("   - {f}");
    }
}

pub fn command(args: &Args) -> Result<u8, String> {
    let (kind, size_arg) = judge::parse_case(args.str("case").unwrap_or("all"))?;
    let host = args
        .require("base")?
        .trim_start_matches("http://")
        .to_owned();
    let addr = host
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or("주소가 없습니다")?;
    let label = args.str("label").unwrap_or("").to_owned();
    let size = SeedSize::default();
    let app_pid = match (args.str("container"), args.parsed::<u32>("pid")?) {
        (Some(c), _) => docker(&["inspect", "-f", "{{.State.Pid}}", c])
            .ok()
            .and_then(|p| p.parse::<u32>().ok())
            .filter(|p| *p > 0),
        (None, p) => p,
    };
    let t = Target {
        addr,
        secret: args.require("secret")?.to_owned(),
        rt: Runtime::from_label(&label),
        label: label.clone(),
        pool: args.parsed_or("pool", 10i64)?,
        db_url: args.require("db")?.to_owned(),
        app_pid,
        db_container: args.str("db-container").map(str::to_owned),
        mix_ctx: Ctx::new(size.users, size.days, size.posts_per_topic, unix_now()),
        quick: args.flag("quick"),
        settle_secs: args.parsed_or("settle", 20u64)?,
        rps: args.parsed_or("rps", 60.0f64)?,
    };
    if t.app_pid.is_none() {
        eprintln!(
            "경고: 서버 프로세스를 찾지 못했습니다(--container 또는 --pid). 스레드·FD·CPU 검사가 비어 있습니다."
        );
    }
    let mut obs = Obs::new(&t.db_url)?;
    let sizes_override: Option<Vec<u64>> = match args.str("sizes") {
        Some(s) => Some(
            s.split(',')
                .map(|v| {
                    v.trim()
                        .parse::<u64>()
                        .map_err(|_| format!("--sizes 값이 올바르지 않습니다: {v}"))
                })
                .collect::<Result<_, _>>()?,
        ),
        None => None,
    };
    let kinds: Vec<CaseKind> = match kind {
        Some(k) => vec![k],
        None => judge::ALL_CASES.to_vec(),
    };
    let drip: f64 = args.parsed_or("drip-interval", 3.0f64)?;
    let slow_secs: Option<u64> = args.parsed("slow-secs")?;
    let hold: u64 = args.parsed_or("hold-secs", 10u64)?;
    let modes: Vec<&str> = match args.str("outage-mode").unwrap_or("both") {
        "pause" => vec!["pause"],
        "stop" => vec!["stop"],
        "both" => vec!["pause", "stop"],
        other => return Err(format!("--outage-mode는 pause|stop|both: {other}")),
    };

    println!(
        "stress [{}] 풀 {} 서버 PID {:?}  부하 발생기 nofile {}",
        t.label,
        t.pool,
        t.app_pid,
        loadgen_nofile()
    );
    let mut results: Vec<CaseResult> = Vec::new();
    for k in kinds {
        let sizes: Vec<u64> = match (size_arg, &sizes_override) {
            (Some(n), _) => vec![n],
            (None, Some(v)) => v.clone(),
            (None, None) => k.default_sizes().to_vec(),
        };
        let mut run_one = |r: CaseResult| {
            print_result(&r);
            results.push(r);
            if let Some(path) = args.str("out") {
                // 케이스가 끝날 때마다 저장해서 중간에 멈춰도 결과가 남는다.
                let _ = merge_out(path, &label, &results[results.len() - 1..]);
            }
        };
        match k {
            CaseKind::Spike => sizes.iter().for_each(|n| run_one(spike(&t, &mut obs, *n))),
            CaseKind::Storm => sizes.iter().for_each(|n| run_one(storm(&t, &mut obs, *n))),
            CaseKind::Idle => sizes.iter().for_each(|n| run_one(idle(&t, &mut obs, *n))),
            CaseKind::Slow => sizes
                .iter()
                .for_each(|n| run_one(slow(&t, &mut obs, *n, drip, slow_secs))),
            CaseKind::Fairness => sizes
                .iter()
                .for_each(|n| run_one(fairness(&t, &mut obs, *n))),
            CaseKind::DbKill => run_one(dbkill(&t, &mut obs)),
            CaseKind::Outage => modes
                .iter()
                .for_each(|m| run_one(outage(&t, &mut obs, m, hold))),
            CaseKind::Abort => run_one(abort(&t, &mut obs, 50)),
        }
    }
    let failed = results.iter().filter(|r| !r.failures.is_empty()).count();
    println!("\n케이스 {}개 중 실패 {failed}개", results.len());
    Ok(u8::from(failed > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netstat에서_항목을_읽는다() {
        let text = "TcpExt: SyncookiesSent SyncookiesRecv ListenOverflows ListenDrops TCPTimeouts\nTcpExt: 0 0 12 15 3\nIpExt: InNoRoutes InTruncatedPkts\nIpExt: 0 0\n";
        assert_eq!(parse_netstat(text, "ListenOverflows"), Some(12));
        assert_eq!(parse_netstat(text, "ListenDrops"), Some(15));
        assert_eq!(parse_netstat(text, "Nope"), None);
        assert_eq!(parse_netstat("", "x"), None);
    }

    #[test]
    fn 자기_프로세스_한도를_읽을_수_있다() {
        assert!(loadgen_nofile() >= 256);
        assert_eq!(pids_limits(u32::MAX), (None, None));
    }

    #[test]
    fn 시간을_줄이는_quick은_최소_3초() {
        let t = Target {
            addr: "127.0.0.1:1".parse().unwrap(),
            secret: String::new(),
            label: "rust".into(),
            rt: Runtime::Rust,
            pool: 10,
            db_url: String::new(),
            app_pid: None,
            db_container: None,
            mix_ctx: Ctx::new(300, 60, 150, 1_791_774_000),
            quick: true,
            settle_secs: 1,
            rps: 60.0,
        };
        assert_eq!(t.secs(30), Duration::from_secs(10));
        assert_eq!(t.secs(5), Duration::from_secs(3));
        let t = Target { quick: false, ..t };
        assert_eq!(t.secs(30), Duration::from_secs(30));
        assert_eq!(t.paced(8, 40.0), Some(Duration::from_millis(200)));
    }

    #[test]
    fn 느린_연결은_멈춘_서버를_끝까지_붙든다() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let server = thread::spawn(move || {
            // 첫 연결: 헤더 앞부분을 읽고 408을 쓰고 닫는다.
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n");
        });
        let stop = Arc::new(AtomicBool::new(false));
        let out = slow_conn(addr, true, Duration::from_secs(3), Arc::clone(&stop));
        server.join().unwrap();
        assert!(out.closed_after.is_some(), "서버가 끊은 것을 알아챈다");
        assert_eq!(out.status, Some(408));
    }

    #[test]
    fn 서버가_안_끊으면_멈춤_신호까지_열려_있다() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let keep = thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            thread::sleep(Duration::from_millis(900));
            drop(s);
        });
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let h = thread::spawn(move || slow_conn(addr, false, Duration::from_millis(100), s2));
        thread::sleep(Duration::from_millis(400));
        stop.store(true, Ordering::Relaxed);
        let out = h.join().unwrap();
        assert!(out.closed_after.is_none());
        keep.join().unwrap();
    }
}
