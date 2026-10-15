//! `stress` 명령의 판정과 계산(순수 함수). 네트워크·프로세스에 닿지 않으므로 단위 시험으로 기준을 고정한다.
//!
//! 판정 함수는 실패 사유 목록을 돌려준다(비어 있으면 통과). 기준값은 이 파일 위쪽 상수에 모아 두었고
//! `docs/perf/stress.md`의 표와 같다. 기준은 Java(Tomcat+HikariCP)와 Rust 양쪽에 같은 값을 쓴다.

use std::io;

use serde_json::{Value, json};

// ---- 기준값 -----------------------------------------------------------------------------------------------

/// 급증이 끝난 뒤 낮은 부하의 p99가 기준선으로 돌아오는 데 허용하는 시간(초).
pub const RECOVERY_LIMIT_SECS: u64 = 10;
/// 기준선 복귀: p99가 `max(기준선 x 3, 기준선 + 50ms)` 이하인 1초 창이 연속 3개.
pub const RECOVERY_FACTOR: u64 = 3;
pub const RECOVERY_SLACK_US: u64 = 50_000;
pub const RECOVERY_STREAK: usize = 3;
/// 유휴 연결이 있을 때 p99가 `max(없을 때 x 2, 없을 때 + 100ms)`를 넘으면 영향이 있다고 본다.
pub const IDLE_P99_FACTOR: u64 = 2;
pub const IDLE_P99_SLACK_US: u64 = 100_000;
/// 유휴 연결만 붙어 있을 때 서버가 쓰는 CPU 한도(코어). 넘으면 유휴 연결 수에 비례한 낭비로 본다.
pub const IDLE_CPU_LIMIT_CORES: f64 = 0.5;
/// 일반 부하의 p99가 느린 연결(slowloris)에 영향을 받았다고 보는 배수/여유.
pub const SLOW_P99_FACTOR: u64 = 3;
pub const SLOW_P99_SLACK_US: u64 = 100_000;
/// 멈춘 연결을 서버가 끊는 시각이 제한 시간보다 이만큼(초) 넘게 늦으면 실패.
pub const TIMEOUT_MARGIN_SECS: f64 = 5.0;
/// 가장 적게 처리된 연결이 중앙값의 이 비율보다 작으면 굶었다고 본다.
pub const STARVATION_RATIO: f64 = 0.2;
/// 풀 대기 한도(30초). 이 이상 걸린 요청은 500으로 끝난다.
pub const POOL_WAIT_LIMIT_US: u64 = 30_000_000;
/// DB 연결을 모두 끊은 뒤 모든 요청이 다시 성공하기까지 허용하는 시간(초).
pub const KILL_RECOVERY_LIMIT_SECS: f64 = 5.0;
/// DB를 되살린 뒤 모든 요청이 다시 성공하기까지 허용하는 시간(초).
pub const OUTAGE_RECOVERY_LIMIT_SECS: f64 = 15.0;

// ---- 오류 분류 ----------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrKind {
    ConnectRefused,
    ConnectTimeout,
    ConnectOther,
    /// 요청 도중 RST.
    Reset,
    /// 응답 없이 FIN(서버가 조용히 닫음).
    Closed,
    /// 응답을 시간 안에 받지 못함.
    Timeout,
    Status408,
    Status500,
    Status503,
    Status5xx,
    Status4xx,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrCounts {
    pub connect_refused: u64,
    pub connect_timeout: u64,
    pub connect_other: u64,
    pub reset: u64,
    pub closed: u64,
    pub timeout: u64,
    pub status_408: u64,
    pub status_500: u64,
    pub status_503: u64,
    pub status_5xx: u64,
    pub status_4xx: u64,
}

impl ErrCounts {
    pub fn add(&mut self, k: ErrKind) {
        let slot = match k {
            ErrKind::ConnectRefused => &mut self.connect_refused,
            ErrKind::ConnectTimeout => &mut self.connect_timeout,
            ErrKind::ConnectOther => &mut self.connect_other,
            ErrKind::Reset => &mut self.reset,
            ErrKind::Closed => &mut self.closed,
            ErrKind::Timeout => &mut self.timeout,
            ErrKind::Status408 => &mut self.status_408,
            ErrKind::Status500 => &mut self.status_500,
            ErrKind::Status503 => &mut self.status_503,
            ErrKind::Status5xx => &mut self.status_5xx,
            ErrKind::Status4xx => &mut self.status_4xx,
        };
        *slot += 1;
    }

    pub fn total(&self) -> u64 {
        self.connect_failures()
            + self.broken()
            + self.timeout
            + self.server_5xx()
            + self.client_4xx()
    }

    pub fn connect_failures(&self) -> u64 {
        self.connect_refused + self.connect_timeout + self.connect_other
    }

    /// 연결이 끊겨 응답을 못 받은 수(RST와 응답 없는 FIN).
    pub fn broken(&self) -> u64 {
        self.reset + self.closed
    }

    pub fn server_5xx(&self) -> u64 {
        self.status_500 + self.status_503 + self.status_5xx
    }

    /// 408과 그 밖의 4xx.
    pub fn client_4xx(&self) -> u64 {
        self.status_408 + self.status_4xx
    }

    pub fn to_json(&self) -> Value {
        json!({
            "total": self.total(),
            "connect_refused": self.connect_refused,
            "connect_timeout": self.connect_timeout,
            "connect_other": self.connect_other,
            "reset": self.reset,
            "closed": self.closed,
            "timeout": self.timeout,
            "status_408": self.status_408,
            "status_500": self.status_500,
            "status_503": self.status_503,
            "status_5xx": self.status_5xx,
            "status_4xx": self.status_4xx,
        })
    }

    /// 0이 아닌 항목만 `이름 값` 목록으로(실패 사유·콘솔 출력용).
    pub fn describe(&self) -> String {
        let items = [
            ("연결거부", self.connect_refused),
            ("연결시간초과", self.connect_timeout),
            ("연결실패기타", self.connect_other),
            ("RST", self.reset),
            ("응답없이닫힘", self.closed),
            ("응답시간초과", self.timeout),
            ("408", self.status_408),
            ("500", self.status_500),
            ("503", self.status_503),
            ("5xx기타", self.status_5xx),
            ("4xx", self.status_4xx),
        ];
        let parts: Vec<String> = items
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        if parts.is_empty() {
            "없음".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// I/O 오류를 분류한다. `in_connect`는 연결을 맺는 도중 난 오류인지.
pub fn classify_io(kind: io::ErrorKind, in_connect: bool) -> ErrKind {
    use io::ErrorKind as K;
    if in_connect {
        return match kind {
            K::ConnectionRefused => ErrKind::ConnectRefused,
            K::TimedOut | K::WouldBlock => ErrKind::ConnectTimeout,
            _ => ErrKind::ConnectOther,
        };
    }
    match kind {
        K::TimedOut | K::WouldBlock => ErrKind::Timeout,
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => ErrKind::Reset,
        _ => ErrKind::Closed,
    }
}

/// 상태 코드를 분류한다. 2xx·3xx는 `None`(성공).
pub fn classify_status(status: u16) -> Option<ErrKind> {
    match status {
        200..=399 => None,
        408 => Some(ErrKind::Status408),
        400..=499 => Some(ErrKind::Status4xx),
        500 => Some(ErrKind::Status500),
        503 => Some(ErrKind::Status503),
        _ => Some(ErrKind::Status5xx),
    }
}

// ---- 단계 요약 ----------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct PhaseSummary {
    pub name: String,
    pub ok: u64,
    pub errors: ErrCounts,
    pub p50_us: u64,
    pub p99_us: u64,
    pub p999_us: u64,
    pub max_us: u64,
    pub secs: f64,
}

impl PhaseSummary {
    pub fn rps(&self) -> f64 {
        if self.secs > 0.0 {
            self.ok as f64 / self.secs
        } else {
            0.0
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "ok": self.ok,
            "errors": self.errors.to_json(),
            "p50_us": self.p50_us,
            "p99_us": self.p99_us,
            "p999_us": self.p999_us,
            "max_us": self.max_us,
            "secs": self.secs,
            "rps": self.rps(),
        })
    }
}

// ---- 누수 검사 ----------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// 앱이 DB에 연 연결 수(부하 발생기 자신의 연결 제외).
    pub app_db_conns: i64,
    /// `idle in transaction`(중단된 것 포함) 세션 수.
    pub idle_in_tx: i64,
    pub threads: u64,
    pub fds: u64,
}

impl Snapshot {
    pub fn to_json(self) -> Value {
        json!({
            "app_db_conns": self.app_db_conns,
            "idle_in_tx": self.idle_in_tx,
            "threads": self.threads,
            "fds": self.fds,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    Java,
    Rust,
}

impl Runtime {
    /// 결과 이름(`java`, `rust`)에서 정한다. 알 수 없으면 Rust 기준(엄격한 쪽)으로 본다.
    pub fn from_label(label: &str) -> Runtime {
        if label.to_ascii_lowercase().contains("java") {
            Runtime::Java
        } else {
            Runtime::Rust
        }
    }

    /// 요청을 읽는 중인 연결을 서버가 끊는 한도(초). Rust `read_timeout` 20초, Tomcat `connectionTimeout` 60초.
    pub fn read_timeout_secs(self) -> f64 {
        match self {
            Runtime::Java => 60.0,
            Runtime::Rust => 20.0,
        }
    }
}

/// 시험 뒤 스레드 허용 증가분. Rust는 연결 스레드가 모두 끝나야 하므로 작게, Java는 JVM 스레드(Tomcat 워커 최대 200,
/// 유휴 워커는 60초 뒤에 줄어든다)라 풀 상한만큼 허용한다(상대 비교).
pub const THREAD_SLACK_RUST: u64 = 8;
pub const THREAD_SLACK_JAVA: u64 = 210;
pub const FD_SLACK: u64 = 16;

/// 누수 검사: 앱 DB 연결은 풀 크기와 같고 `idle in transaction`은 0, 스레드·FD는 시작 수준으로 돌아와야 한다.
pub fn leak_failures(before: &Snapshot, after: &Snapshot, pool: i64, rt: Runtime) -> Vec<String> {
    let mut f = Vec::new();
    if after.app_db_conns != pool {
        f.push(format!(
            "누수 검사: 앱 DB 연결 {}개(풀 크기 {pool})",
            after.app_db_conns
        ));
    }
    if after.idle_in_tx != 0 {
        f.push(format!(
            "누수 검사: idle in transaction {}개",
            after.idle_in_tx
        ));
    }
    let slack = match rt {
        Runtime::Java => THREAD_SLACK_JAVA,
        Runtime::Rust => THREAD_SLACK_RUST,
    };
    if after.threads > before.threads + slack {
        f.push(format!(
            "누수 검사: 스레드 {} -> {} (허용 +{slack})",
            before.threads, after.threads
        ));
    }
    if after.fds > before.fds + FD_SLACK {
        f.push(format!(
            "누수 검사: FD {} -> {} (허용 +{FD_SLACK})",
            before.fds, after.fds
        ));
    }
    f
}

// ---- 시계열 계산 --------------------------------------------------------------------------------------------

/// 1초 창의 낮은 부하(프로브) 지연.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub second: u64,
    pub count: u64,
    pub p99_us: u64,
}

/// 기준선 복귀 기준(`max(기준선 x 3, 기준선 + 50ms)`).
pub fn recovery_threshold_us(baseline_p99_us: u64) -> u64 {
    (baseline_p99_us * RECOVERY_FACTOR).max(baseline_p99_us + RECOVERY_SLACK_US)
}

/// `after_sec`(급증이 끝난 시각) 뒤 처음으로 기준 이하인 창이 `RECOVERY_STREAK`개 이어지는 지점까지의 초.
/// 표본이 없는 창(요청이 하나도 끝나지 않음)은 기준 초과로 센다. 끝까지 못 돌아오면 `None`.
pub fn recovery_secs(windows: &[Window], after_sec: u64, baseline_p99_us: u64) -> Option<u64> {
    let limit = recovery_threshold_us(baseline_p99_us);
    let last = windows.iter().map(|w| w.second).max()?;
    let ok_at = |s: u64| {
        windows
            .iter()
            .find(|w| w.second == s)
            .is_some_and(|w| w.count > 0 && w.p99_us <= limit)
    };
    let streak = RECOVERY_STREAK as u64;
    (after_sec..=last.saturating_sub(streak - 1))
        .find(|&s| (s..s + streak).all(ok_at))
        .map(|s| s - after_sec)
}

/// 사건(`event_ms`) 뒤 `horizon_ms` 안에서 마지막 오류까지의 초. 오류가 없으면 0.
pub fn outage_secs(event_ms: u64, error_ms: &[u64], horizon_ms: u64) -> f64 {
    error_ms
        .iter()
        .filter(|t| **t >= event_ms && **t < event_ms + horizon_ms)
        .max()
        .map_or(0.0, |t| (t - event_ms) as f64 / 1000.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fairness {
    pub min: u64,
    pub median: u64,
    pub max: u64,
    /// `min / median`. 중앙값이 0이면 0.
    pub ratio: f64,
}

/// 연결마다 끝낸 요청 수의 분포. 연결이 없으면 `None`.
pub fn fairness(counts: &[u64]) -> Option<Fairness> {
    if counts.is_empty() {
        return None;
    }
    let mut v = counts.to_vec();
    v.sort_unstable();
    let median = v[v.len() / 2];
    let min = v[0];
    Some(Fairness {
        min,
        median,
        max: v[v.len() - 1],
        ratio: if median == 0 {
            0.0
        } else {
            min as f64 / median as f64
        },
    })
}

// ---- 케이스별 판정 ------------------------------------------------------------------------------------------

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// 급증: 낮은 부하(앞·뒤)에는 오류가 없고, 급증 구간에도 연결 실패·끊김·5xx·시간 초과가 없고, 기준선으로 돌아온다.
pub fn judge_spike(
    low_before: &PhaseSummary,
    burst: &PhaseSummary,
    low_after: &PhaseSummary,
    recovery: Option<u64>,
) -> Vec<String> {
    let mut f = Vec::new();
    for p in [low_before, low_after] {
        if p.errors.total() > 0 {
            f.push(format!("{}에 오류: {}", p.name, p.errors.describe()));
        }
    }
    if burst.errors.total() > 0 {
        f.push(format!("급증 구간 오류: {}", burst.errors.describe()));
    }
    match recovery {
        None => f.push("급증 뒤 지연이 기준선으로 돌아오지 않음".to_owned()),
        Some(s) if s > RECOVERY_LIMIT_SECS => {
            f.push(format!("기준선 복귀 {s}초(허용 {RECOVERY_LIMIT_SECS}초)"))
        }
        _ => {}
    }
    f
}

/// 연결 폭주: 성공 비율(연결 1000개까지 99%, 그 이상 95%)과 폭주 뒤 서버가 바로 응답하는지.
pub fn storm_min_ratio(conns: u64) -> f64 {
    if conns <= 1000 { 0.99 } else { 0.95 }
}

pub fn judge_storm(conns: u64, ok: u64, errors: &ErrCounts, probe_ok: bool) -> Vec<String> {
    let mut f = Vec::new();
    let total = ok + errors.total();
    let ratio = if total == 0 {
        0.0
    } else {
        ok as f64 / total as f64
    };
    let min = storm_min_ratio(conns);
    if ratio < min {
        f.push(format!(
            "성공 {ok}/{total} ({:.1}%, 기준 {:.0}% 이상): {}",
            ratio * 100.0,
            min * 100.0,
            errors.describe()
        ));
    }
    if !probe_ok {
        f.push("폭주 뒤 서버가 요청에 응답하지 않음".to_owned());
    }
    f
}

pub struct IdleInput {
    pub requested: u64,
    pub opened: u64,
    pub base_p99_us: u64,
    pub load_p99_us: u64,
    pub load_errors: u64,
    pub idle_cpu_cores: f64,
}

/// 유휴 연결: 모두 붙고, 부하 중 오류가 없고, 지연이 크게 늘지 않고, 유휴 CPU가 한도 안이다.
pub fn judge_idle(i: &IdleInput) -> Vec<String> {
    let mut f = Vec::new();
    if i.opened < i.requested {
        f.push(format!("유휴 연결 {}/{}개만 붙음", i.opened, i.requested));
    }
    if i.load_errors > 0 {
        f.push(format!(
            "유휴 연결이 붙은 채 부하 중 오류 {}건",
            i.load_errors
        ));
    }
    let limit = (i.base_p99_us * IDLE_P99_FACTOR).max(i.base_p99_us + IDLE_P99_SLACK_US);
    if i.load_p99_us > limit {
        f.push(format!(
            "p99 {:.1}ms > 유휴 연결 없을 때 {:.1}ms 기준 한도 {:.1}ms",
            ms(i.load_p99_us),
            ms(i.base_p99_us),
            ms(limit)
        ));
    }
    if i.idle_cpu_cores > IDLE_CPU_LIMIT_CORES {
        f.push(format!(
            "유휴 연결만으로 CPU {:.2}코어(한도 {IDLE_CPU_LIMIT_CORES}코어)",
            i.idle_cpu_cores
        ));
    }
    f
}

pub struct SlowInput {
    pub normal_errors: u64,
    pub base_p99_us: u64,
    pub normal_p99_us: u64,
    /// 헤더를 한 번 보내고 멈춘 연결 수와, 그중 서버가 끊은 수·끊은 시각(초)의 최댓값.
    pub stalled: u64,
    pub stalled_closed: u64,
    pub stalled_max_close_secs: f64,
    pub timeout_limit_secs: f64,
}

/// 느린 클라이언트: 일반 부하가 정상이고, 멈춘 연결은 서버의 읽기 제한 시간 안에 끊긴다.
/// (한 바이트씩 계속 보내는 연결은 바이트마다 타이머가 갱신되어 끊기지 않는 것이 두 서버의 공통 동작이라 판정하지 않고 기록만 한다.)
pub fn judge_slow(i: &SlowInput) -> Vec<String> {
    let mut f = Vec::new();
    if i.normal_errors > 0 {
        f.push(format!(
            "느린 연결이 붙은 채 일반 부하 오류 {}건",
            i.normal_errors
        ));
    }
    let limit = (i.base_p99_us * SLOW_P99_FACTOR).max(i.base_p99_us + SLOW_P99_SLACK_US);
    if i.normal_p99_us > limit {
        f.push(format!(
            "일반 부하 p99 {:.1}ms > 한도 {:.1}ms(기준선 {:.1}ms)",
            ms(i.normal_p99_us),
            ms(limit),
            ms(i.base_p99_us)
        ));
    }
    if i.stalled_closed < i.stalled {
        f.push(format!(
            "멈춘 연결 {}개 중 {}개만 끊김",
            i.stalled, i.stalled_closed
        ));
    } else if i.stalled_max_close_secs > i.timeout_limit_secs + TIMEOUT_MARGIN_SECS {
        f.push(format!(
            "멈춘 연결이 {:.1}초에 끊김(제한 {}초 + 여유 {TIMEOUT_MARGIN_SECS}초)",
            i.stalled_max_close_secs, i.timeout_limit_secs
        ));
    }
    f
}

/// 과부하 공정성: 오류(풀 대기 한도를 넘긴 500 포함)가 없고, 굶는 연결이 없고, 최대 지연이 풀 대기 한도 안이다.
pub fn judge_fairness(errors: &ErrCounts, fair: Option<Fairness>, max_us: u64) -> Vec<String> {
    let mut f = Vec::new();
    if errors.total() > 0 {
        f.push(format!("오류: {}", errors.describe()));
    }
    match fair {
        None => f.push("처리된 요청이 없음".to_owned()),
        Some(x) if x.ratio < STARVATION_RATIO => f.push(format!(
            "굶는 연결: 가장 적은 연결 {}건, 중앙값 {}건(비율 {:.2} < {STARVATION_RATIO})",
            x.min, x.median, x.ratio
        )),
        _ => {}
    }
    if max_us >= POOL_WAIT_LIMIT_US {
        f.push(format!(
            "최대 지연 {:.1}초가 풀 대기 한도(30초) 이상",
            max_us as f64 / 1e6
        ));
    }
    f
}

pub struct KillRound {
    pub errors: u64,
    pub recovery_secs: f64,
}

/// DB 연결 끊기: 라운드마다 오류가 (풀 + 연결 수) 이하이고 복구 시간이 한도 안이며, 끝나면 풀이 다시 풀 크기가 된다.
pub fn judge_dbkill(
    rounds: &[KillRound],
    pool: i64,
    load_conns: u64,
    final_ok: bool,
    pool_recovered: bool,
) -> Vec<String> {
    let mut f = Vec::new();
    let max_errors = pool as u64 + load_conns;
    for (i, r) in rounds.iter().enumerate() {
        if r.errors > max_errors {
            f.push(format!(
                "{}번째 끊기: 오류 {}건(연결을 한 번씩 만난다고 봐도 {max_errors}건 이하여야 함)",
                i + 1,
                r.errors
            ));
        }
        if r.recovery_secs > KILL_RECOVERY_LIMIT_SECS {
            f.push(format!(
                "{}번째 끊기: 모든 요청이 성공하기까지 {:.1}초(허용 {KILL_RECOVERY_LIMIT_SECS}초)",
                i + 1,
                r.recovery_secs
            ));
        }
    }
    if !final_ok {
        f.push("끊기를 마친 뒤에도 요청이 실패함".to_owned());
    }
    if !pool_recovered {
        f.push(format!("풀이 {pool}개로 회복하지 않음"));
    }
    f
}

pub struct OutageInput {
    pub recovery_secs: f64,
    pub health_up_after: bool,
    pub data_lost: bool,
}

/// DB 중단: 되살린 뒤 모든 요청이 한도 안에 다시 성공하고, 헬스체크가 UP이고, 데이터가 남아 있다.
pub fn judge_outage(i: &OutageInput) -> Vec<String> {
    let mut f = Vec::new();
    if i.data_lost {
        f.push("DB 데이터가 사라짐(tmpfs 볼륨은 stop/start에 지워진다: PERF_PG_PERSIST=1로 스택을 다시 띄워야 함)".to_owned());
    }
    if i.recovery_secs > OUTAGE_RECOVERY_LIMIT_SECS {
        f.push(format!(
            "되살린 뒤 모든 요청이 성공하기까지 {:.1}초(허용 {OUTAGE_RECOVERY_LIMIT_SECS}초)",
            i.recovery_secs
        ));
    }
    if !i.health_up_after {
        f.push("회복 뒤 /actuator/health가 UP이 아님".to_owned());
    }
    f
}

pub struct AbortInput {
    /// 보낸 요청 수와, 잠금을 푼 뒤 DB에 실제로 들어간 행 수.
    pub sent: u64,
    pub rows_inserted: i64,
    pub probe_ok: bool,
}

/// 클라이언트 중단: 서버가 살아 있고(프로브 성공) 누수 검사를 통과해야 한다. 끊긴 요청의 처리 결과(행 수)는 기록한다:
/// 응답을 못 받는 클라이언트가 있어도 요청은 끝까지 처리되는 것이 원본 동작(Tomcat은 취소하지 않는다)이다.
pub fn judge_abort(i: &AbortInput) -> Vec<String> {
    let mut f = Vec::new();
    if !i.probe_ok {
        f.push("끊긴 요청을 모두 처리한 뒤 서버가 새 요청에 응답하지 않음".to_owned());
    }
    if i.rows_inserted < 0 || i.rows_inserted as u64 > i.sent {
        f.push(format!(
            "삽입된 행 {}개가 보낸 요청 {}개와 맞지 않음",
            i.rows_inserted, i.sent
        ));
    }
    f
}

// ---- 사례 이름과 필요 자원 ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseKind {
    Spike,
    Storm,
    Idle,
    Slow,
    Fairness,
    DbKill,
    Outage,
    Abort,
}

pub const ALL_CASES: [CaseKind; 8] = [
    CaseKind::Spike,
    CaseKind::Storm,
    CaseKind::Idle,
    CaseKind::Slow,
    CaseKind::Fairness,
    CaseKind::DbKill,
    CaseKind::Outage,
    CaseKind::Abort,
];

impl CaseKind {
    pub fn name(self) -> &'static str {
        match self {
            CaseKind::Spike => "spike",
            CaseKind::Storm => "storm",
            CaseKind::Idle => "idle",
            CaseKind::Slow => "slow",
            CaseKind::Fairness => "fairness",
            CaseKind::DbKill => "dbkill",
            CaseKind::Outage => "outage",
            CaseKind::Abort => "abort",
        }
    }

    /// 크기 인자를 주지 않았을 때 도는 크기들(없으면 빈 목록).
    pub fn default_sizes(self) -> &'static [u64] {
        match self {
            CaseKind::Spike => &[50, 200, 400, 800],
            CaseKind::Storm => &[1000, 3000],
            CaseKind::Idle => &[2000, 5000],
            CaseKind::Slow => &[400],
            CaseKind::Fairness => &[400, 1000],
            CaseKind::DbKill | CaseKind::Outage | CaseKind::Abort => &[],
        }
    }
}

/// `spike`, `spike:200`, `all`. 이름과 크기(선택)를 돌려준다. `all`은 `None`.
pub fn parse_case(s: &str) -> Result<(Option<CaseKind>, Option<u64>), String> {
    let (name, size) = match s.split_once(':') {
        Some((n, z)) => (
            n,
            Some(
                z.parse::<u64>()
                    .map_err(|_| format!("크기가 숫자가 아닙니다: {z}"))?,
            ),
        ),
        None => (s, None),
    };
    if name == "all" {
        return if size.is_some() {
            Err("all에는 크기를 줄 수 없습니다".to_owned())
        } else {
            Ok((None, None))
        };
    }
    ALL_CASES
        .iter()
        .find(|c| c.name() == name)
        .map(|c| (Some(*c), size))
        .ok_or_else(|| {
            let names: Vec<&str> = ALL_CASES.iter().map(|c| c.name()).collect();
            format!("알 수 없는 케이스: {name} ({} 또는 all)", names.join(", "))
        })
}

/// 부하 발생기가 열어 둘 동시 소켓 수 + 여유로 필요한 FD 수. 모자라면 원인이 부하 발생기 쪽 `ulimit -n`이다.
pub fn loadgen_fds_needed(sockets: u64) -> u64 {
    sockets + 256
}

/// `/proc/<pid>/limits`에서 `Max open files`의 소프트 한도. `unlimited`는 `u64::MAX`.
pub fn parse_nofile(limits: &str) -> Option<u64> {
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    let v = line.split_whitespace().nth(3)?;
    if v == "unlimited" {
        Some(u64::MAX)
    } else {
        v.parse().ok()
    }
}

/// cgroup 파일 값(`max` 또는 숫자). `max`는 `None`(무제한).
pub fn parse_cgroup_limit(v: &str) -> Option<u64> {
    v.trim().parse().ok()
}

/// 연결 붙이기 실패의 원인을 가른다(부하 발생기 한도, 서버 FD 한도, 서버 pids 한도, 그 밖).
pub fn diagnose_open_failure(
    opened: u64,
    requested: u64,
    loadgen_nofile: u64,
    server_nofile: u64,
    server_fds: u64,
    pids_max: Option<u64>,
    pids_current: Option<u64>,
) -> Option<String> {
    if opened >= requested {
        return None;
    }
    if loadgen_nofile < loadgen_fds_needed(requested) {
        return Some(format!(
            "부하 발생기 쪽 한도: nofile {loadgen_nofile} < 필요 {}",
            loadgen_fds_needed(requested)
        ));
    }
    if server_fds + 64 >= server_nofile {
        return Some(format!(
            "서버 컨테이너 nofile 한도: FD {server_fds} / {server_nofile}"
        ));
    }
    if let (Some(max), Some(cur)) = (pids_max, pids_current)
        && cur + 16 >= max
    {
        return Some(format!("서버 컨테이너 pids 한도: {cur} / {max}"));
    }
    Some("부하 발생기·서버 한도는 아님(서버 거부·백로그·기타)".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(name: &str, errors: ErrCounts) -> PhaseSummary {
        PhaseSummary {
            name: name.to_owned(),
            errors,
            ..Default::default()
        }
    }

    fn errs(k: ErrKind, n: u64) -> ErrCounts {
        let mut e = ErrCounts::default();
        for _ in 0..n {
            e.add(k);
        }
        e
    }

    #[test]
    fn io_오류를_연결_단계와_요청_단계로_나눈다() {
        use io::ErrorKind as K;
        assert_eq!(
            classify_io(K::ConnectionRefused, true),
            ErrKind::ConnectRefused
        );
        assert_eq!(classify_io(K::TimedOut, true), ErrKind::ConnectTimeout);
        assert_eq!(classify_io(K::ConnectionReset, true), ErrKind::ConnectOther);
        assert_eq!(classify_io(K::TimedOut, false), ErrKind::Timeout);
        assert_eq!(classify_io(K::WouldBlock, false), ErrKind::Timeout);
        assert_eq!(classify_io(K::ConnectionReset, false), ErrKind::Reset);
        assert_eq!(classify_io(K::BrokenPipe, false), ErrKind::Reset);
        assert_eq!(classify_io(K::UnexpectedEof, false), ErrKind::Closed);
    }

    #[test]
    fn 상태_코드를_분류한다() {
        assert_eq!(classify_status(200), None);
        assert_eq!(classify_status(204), None);
        assert_eq!(classify_status(408), Some(ErrKind::Status408));
        assert_eq!(classify_status(404), Some(ErrKind::Status4xx));
        assert_eq!(classify_status(500), Some(ErrKind::Status500));
        assert_eq!(classify_status(503), Some(ErrKind::Status503));
        assert_eq!(classify_status(502), Some(ErrKind::Status5xx));
    }

    #[test]
    fn 오류_합계와_설명() {
        let mut e = ErrCounts::default();
        assert_eq!(e.describe(), "없음");
        e.add(ErrKind::ConnectRefused);
        e.add(ErrKind::Reset);
        e.add(ErrKind::Status503);
        e.add(ErrKind::Status503);
        assert_eq!(e.total(), 4);
        assert_eq!(
            (e.connect_failures(), e.broken(), e.server_5xx()),
            (1, 1, 2)
        );
        assert_eq!(e.describe(), "연결거부 1, RST 1, 503 2");
        assert_eq!(e.to_json()["total"], 4);
        assert_eq!(e.to_json()["status_503"], 2);
    }

    #[test]
    fn 누수_검사는_풀_크기와_idle_in_transaction을_본다() {
        let before = Snapshot {
            app_db_conns: 10,
            idle_in_tx: 0,
            threads: 40,
            fds: 60,
        };
        let ok = Snapshot {
            threads: 44,
            fds: 70,
            ..before
        };
        assert!(leak_failures(&before, &ok, 10, Runtime::Rust).is_empty());
        let few = Snapshot {
            app_db_conns: 7,
            ..ok
        };
        assert!(leak_failures(&before, &few, 10, Runtime::Rust)[0].contains("7개"));
        let tx = Snapshot {
            idle_in_tx: 2,
            ..ok
        };
        assert!(
            leak_failures(&before, &tx, 10, Runtime::Rust)[0].contains("idle in transaction 2")
        );
    }

    #[test]
    fn 누수_검사는_스레드와_fd_증가를_본다() {
        let before = Snapshot {
            app_db_conns: 10,
            idle_in_tx: 0,
            threads: 40,
            fds: 60,
        };
        let after = Snapshot {
            threads: 400,
            fds: 61,
            ..before
        };
        let rust = leak_failures(&before, &after, 10, Runtime::Rust);
        assert!(rust.iter().any(|m| m.contains("스레드")), "{rust:?}");
        assert!(
            leak_failures(&before, &after, 10, Runtime::Java)
                .iter()
                .any(|m| m.contains("스레드")),
            "Java도 풀 상한(+210)을 넘으면 실패"
        );
        let java_ok = Snapshot {
            threads: 40 + 200,
            ..before
        };
        assert!(
            leak_failures(&before, &java_ok, 10, Runtime::Java).is_empty(),
            "Tomcat 워커 200개는 허용"
        );
        let fd = Snapshot { fds: 200, ..before };
        assert!(leak_failures(&before, &fd, 10, Runtime::Rust)[0].contains("FD"));
    }

    #[test]
    fn 라벨로_런타임을_고른다() {
        assert_eq!(Runtime::from_label("java"), Runtime::Java);
        assert_eq!(Runtime::from_label("Rust"), Runtime::Rust);
        assert_eq!(Runtime::from_label(""), Runtime::Rust);
        assert_eq!(Runtime::Java.read_timeout_secs(), 60.0);
        assert_eq!(Runtime::Rust.read_timeout_secs(), 20.0);
    }

    fn windows(p99s: &[(u64, u64)]) -> Vec<Window> {
        p99s.iter()
            .map(|(s, p)| Window {
                second: *s,
                count: 40,
                p99_us: *p,
            })
            .collect()
    }

    #[test]
    fn 기준선_복귀_기준은_3배와_50ms_중_큰_값() {
        assert_eq!(recovery_threshold_us(10_000), 60_000);
        assert_eq!(recovery_threshold_us(100_000), 300_000);
    }

    #[test]
    fn 급증_뒤_복귀_시간을_잰다() {
        // 급증은 10초에 끝남. 10~12초는 느리고 13초부터 3개 연속 기준(60ms) 이하.
        let w = windows(&[
            (8, 5_000),
            (9, 5_000),
            (10, 900_000),
            (11, 400_000),
            (12, 70_000),
            (13, 20_000),
            (14, 25_000),
            (15, 22_000),
            (16, 20_000),
        ]);
        assert_eq!(recovery_secs(&w, 10, 10_000), Some(3));
    }

    #[test]
    fn 한_번_내려갔다_다시_오르면_연속_기준을_못_채운다() {
        let w = windows(&[
            (10, 20_000),
            (11, 20_000),
            (12, 500_000),
            (13, 20_000),
            (14, 20_000),
            (15, 20_000),
        ]);
        assert_eq!(recovery_secs(&w, 10, 10_000), Some(3));
    }

    #[test]
    fn 끝까지_못_돌아오면_none이고_빈_창은_초과로_센다() {
        let w = windows(&[(10, 900_000), (11, 900_000), (12, 900_000)]);
        assert_eq!(recovery_secs(&w, 10, 10_000), None);
        // 11초에 표본이 없으면(요청이 하나도 안 끝남) 연속이 끊긴다.
        let w = windows(&[(10, 1_000), (12, 1_000), (13, 1_000), (14, 1_000)]);
        assert_eq!(recovery_secs(&w, 10, 10_000), Some(2));
        assert_eq!(recovery_secs(&[], 0, 1), None);
    }

    #[test]
    fn 오류가_멈춘_시각까지의_초() {
        let errs = [900, 5_200, 5_900, 6_100, 30_000];
        assert!((outage_secs(5_000, &errs, 10_000) - 1.1).abs() < 1e-9);
        assert_eq!(outage_secs(40_000, &errs, 10_000), 0.0);
        assert_eq!(outage_secs(0, &[], 1000), 0.0);
    }

    #[test]
    fn 공정성_분포() {
        let f = fairness(&[10, 20, 30, 40, 50]).unwrap();
        assert_eq!((f.min, f.median, f.max), (10, 30, 50));
        assert!((f.ratio - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(fairness(&[]), None);
        assert_eq!(fairness(&[0, 0, 5]).unwrap().ratio, 0.0);
    }

    #[test]
    fn 급증_판정() {
        let ok = phase("낮은 부하", ErrCounts::default());
        let burst = phase("급증", ErrCounts::default());
        assert!(judge_spike(&ok, &burst, &ok, Some(2)).is_empty());
        let bad = phase("급증", errs(ErrKind::Status503, 3));
        let f = judge_spike(&ok, &bad, &ok, Some(2));
        assert!(f[0].contains("503 3"), "{f:?}");
        assert!(judge_spike(&ok, &burst, &ok, None)[0].contains("돌아오지 않음"));
        assert!(judge_spike(&ok, &burst, &ok, Some(11))[0].contains("11초"));
        let low_err = phase("낮은 부하(뒤)", errs(ErrKind::Timeout, 1));
        assert!(judge_spike(&ok, &burst, &low_err, Some(1))[0].contains("낮은 부하(뒤)"));
    }

    #[test]
    fn 폭주_판정은_성공_비율과_프로브를_본다() {
        assert_eq!(storm_min_ratio(1000), 0.99);
        assert_eq!(storm_min_ratio(3000), 0.95);
        assert!(judge_storm(1000, 995, &errs(ErrKind::ConnectTimeout, 5), true).is_empty());
        let f = judge_storm(1000, 900, &errs(ErrKind::ConnectRefused, 100), true);
        assert!(
            f[0].contains("90.0%") && f[0].contains("연결거부 100"),
            "{f:?}"
        );
        assert!(
            judge_storm(3000, 2900, &errs(ErrKind::Reset, 100), true).is_empty(),
            "96.7% >= 95%"
        );
        assert!(judge_storm(10, 10, &ErrCounts::default(), false)[0].contains("응답하지 않음"));
        assert!(
            !judge_storm(10, 0, &ErrCounts::default(), true).is_empty(),
            "요청이 하나도 없으면 실패"
        );
    }

    #[test]
    fn 유휴_연결_판정() {
        let base = IdleInput {
            requested: 2000,
            opened: 2000,
            base_p99_us: 10_000,
            load_p99_us: 25_000,
            load_errors: 0,
            idle_cpu_cores: 0.1,
        };
        assert!(judge_idle(&base).is_empty());
        let few = IdleInput {
            opened: 1500,
            ..base
        };
        assert!(judge_idle(&few)[0].contains("1500/2000"));
        let slow = IdleInput {
            load_p99_us: 200_000,
            ..base
        };
        assert!(judge_idle(&slow)[0].contains("p99"));
        let cpu = IdleInput {
            idle_cpu_cores: 0.9,
            ..base
        };
        assert!(judge_idle(&cpu)[0].contains("0.90코어"));
        let err = IdleInput {
            load_errors: 4,
            ..base
        };
        assert!(judge_idle(&err)[0].contains("오류 4건"));
    }

    #[test]
    fn 느린_클라이언트_판정() {
        let base = SlowInput {
            normal_errors: 0,
            base_p99_us: 10_000,
            normal_p99_us: 20_000,
            stalled: 100,
            stalled_closed: 100,
            stalled_max_close_secs: 21.0,
            timeout_limit_secs: 20.0,
        };
        assert!(judge_slow(&base).is_empty());
        let late = SlowInput {
            stalled_max_close_secs: 26.5,
            ..base
        };
        assert!(judge_slow(&late)[0].contains("26.5초"));
        let open = SlowInput {
            stalled_closed: 60,
            ..base
        };
        assert!(judge_slow(&open)[0].contains("100개 중 60개"));
        let hurt = SlowInput {
            normal_errors: 2,
            normal_p99_us: 500_000,
            ..base
        };
        assert_eq!(judge_slow(&hurt).len(), 2);
        let java = SlowInput {
            stalled_max_close_secs: 61.0,
            timeout_limit_secs: 60.0,
            ..base
        };
        assert!(judge_slow(&java).is_empty());
    }

    #[test]
    fn 공정성_판정() {
        let fair = fairness(&[100, 110, 120]);
        assert!(judge_fairness(&ErrCounts::default(), fair, 5_000_000).is_empty());
        let starved = fairness(&[1, 100, 110, 120, 130]);
        assert!(judge_fairness(&ErrCounts::default(), starved, 1)[0].contains("굶는 연결"));
        let f = judge_fairness(&errs(ErrKind::Status500, 7), fair, 31_000_000);
        assert!(
            f[0].contains("500 7") && f[1].contains("풀 대기 한도"),
            "{f:?}"
        );
        assert!(judge_fairness(&ErrCounts::default(), None, 0)[0].contains("처리된 요청이 없음"));
    }

    #[test]
    fn db_끊기_판정() {
        let ok = [
            KillRound {
                errors: 3,
                recovery_secs: 0.4,
            },
            KillRound {
                errors: 0,
                recovery_secs: 0.0,
            },
        ];
        assert!(judge_dbkill(&ok, 10, 16, true, true).is_empty());
        let bad = [KillRound {
            errors: 40,
            recovery_secs: 7.0,
        }];
        let f = judge_dbkill(&bad, 10, 16, false, false);
        assert_eq!(f.len(), 4, "{f:?}");
        assert!(f[2].contains("끝난 뒤") || f[2].contains("마친 뒤"));
    }

    #[test]
    fn db_중단_판정() {
        let ok = OutageInput {
            recovery_secs: 3.0,
            health_up_after: true,
            data_lost: false,
        };
        assert!(judge_outage(&ok).is_empty());
        let bad = OutageInput {
            recovery_secs: 30.0,
            health_up_after: false,
            data_lost: true,
        };
        assert_eq!(judge_outage(&bad).len(), 3);
    }

    #[test]
    fn 클라이언트_중단_판정() {
        assert!(
            judge_abort(&AbortInput {
                sent: 50,
                rows_inserted: 50,
                probe_ok: true
            })
            .is_empty()
        );
        assert!(
            judge_abort(&AbortInput {
                sent: 50,
                rows_inserted: 0,
                probe_ok: true
            })
            .is_empty(),
            "취소되어도 정상"
        );
        assert!(
            !judge_abort(&AbortInput {
                sent: 50,
                rows_inserted: 51,
                probe_ok: true
            })
            .is_empty()
        );
        assert!(
            !judge_abort(&AbortInput {
                sent: 50,
                rows_inserted: 50,
                probe_ok: false
            })
            .is_empty()
        );
    }

    #[test]
    fn 케이스_이름을_읽는다() {
        assert_eq!(parse_case("spike").unwrap(), (Some(CaseKind::Spike), None));
        assert_eq!(
            parse_case("storm:1000").unwrap(),
            (Some(CaseKind::Storm), Some(1000))
        );
        assert_eq!(parse_case("all").unwrap(), (None, None));
        assert!(parse_case("all:3").is_err());
        assert!(parse_case("spike:x").is_err());
        assert!(parse_case("nope").unwrap_err().contains("dbkill"));
        assert_eq!(CaseKind::Spike.default_sizes(), &[50, 200, 400, 800]);
        assert!(CaseKind::Outage.default_sizes().is_empty());
    }

    #[test]
    fn 한도에서_원인을_가른다() {
        let diag = |lg, sv, fds, pm, pc| diagnose_open_failure(4000, 5000, lg, sv, fds, pm, pc);
        assert_eq!(diagnose_open_failure(5000, 5000, 1, 1, 1, None, None), None);
        assert!(
            diag(1024, 1_000_000, 10, None, None)
                .unwrap()
                .contains("부하 발생기")
        );
        assert!(
            diag(20_000, 8192, 8150, None, None)
                .unwrap()
                .contains("nofile")
        );
        assert!(
            diag(20_000, 1_000_000, 500, Some(4096), Some(4090))
                .unwrap()
                .contains("pids")
        );
        assert!(
            diag(20_000, 1_000_000, 500, Some(4096), Some(100))
                .unwrap()
                .contains("한도는 아님")
        );
    }

    #[test]
    fn proc_limits에서_nofile을_읽는다() {
        let text = "Limit                     Soft Limit           Hard Limit           Units     \nMax cpu time              unlimited            unlimited            seconds   \nMax open files            20000                20000                files    \n";
        assert_eq!(parse_nofile(text), Some(20_000));
        assert_eq!(
            parse_nofile(
                "Max open files            unlimited            unlimited            files"
            ),
            Some(u64::MAX)
        );
        assert_eq!(parse_nofile("x"), None);
        assert_eq!(parse_cgroup_limit("max\n"), None);
        assert_eq!(parse_cgroup_limit("4096\n"), Some(4096));
        assert_eq!(loadgen_fds_needed(5000), 5256);
    }
}
