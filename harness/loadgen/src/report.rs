//! `report`: 결과 디렉터리(`java/`, `rust/` 하위 JSON과 `sql-*.json`, `startup-*.json`, `race-*.json`)를 읽어
//! 마크다운 보고서와 성능 게이트 판정을 만든다.
//!
//! 게이트(전환 계획 4.5): 모든 엔드포인트에서 Rust p99 <= Java p99, 처리량 >= Java, 요청당 SQL 수 <= Java.
//! 라운드를 여러 번 돌려 **중앙값**으로 비교하고, 라운드 사이 퍼짐(최대-최소)/중앙값을 노이즈로 함께 낸다.
//! 판정은 셋이다: 통과(엄격히 만족), 통과*(엄격히는 어겼지만 차이가 양쪽 노이즈보다 작다), 실패.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::args::Args;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// 엄격한 비교는 어겼지만 라운드 노이즈 안쪽이다.
    PassWithinNoise,
    Fail,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "통과",
            Verdict::PassWithinNoise => "통과*",
            Verdict::Fail => "실패",
        }
    }
}

/// 여러 라운드 값의 요약.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub n: usize,
}

impl Series {
    pub fn of(values: &[f64]) -> Option<Series> {
        if values.is_empty() {
            return None;
        }
        let mut v = values.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = v.len() / 2;
        let median = if v.len() % 2 == 1 {
            v[mid]
        } else {
            (v[mid - 1] + v[mid]) / 2.0
        };
        Some(Series {
            median,
            min: v[0],
            max: v[v.len() - 1],
            n: v.len(),
        })
    }

    /// (최대-최소)/중앙값. 값이 하나뿐이면 0이다(퍼짐을 알 수 없으므로 노이즈로 면제하지 않는다).
    pub fn spread(&self) -> f64 {
        if self.n < 2 {
            return 0.0;
        }
        if self.median == 0.0 {
            return 0.0;
        }
        (self.max - self.min) / self.median
    }
}

/// `lower_is_better`이면 Rust <= Java가 목표, 아니면 Rust >= Java.
pub fn judge(java: &Series, rust: &Series, lower_is_better: bool) -> Verdict {
    let strict = if lower_is_better {
        rust.median <= java.median
    } else {
        rust.median >= java.median
    };
    if strict {
        return Verdict::Pass;
    }
    let noise = java.spread().max(rust.spread());
    let rel = if java.median == 0.0 {
        f64::INFINITY
    } else {
        (rust.median - java.median).abs() / java.median
    };
    if rel <= noise {
        Verdict::PassWithinNoise
    } else {
        Verdict::Fail
    }
}

#[derive(Default)]
struct SideRuns {
    rps: Vec<Value>,
    max: Vec<Value>,
    only: BTreeMap<String, Vec<Value>>,
    soak: Vec<Value>,
}

fn load_side(dir: &Path) -> SideRuns {
    let mut out = SideRuns::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    files.sort();
    for path in files {
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v["kind"] != "run" {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("soak") {
            out.soak.push(v);
        } else if let Some(only) = v["only"].as_str() {
            out.only.entry(only.to_owned()).or_default().push(v);
        } else if v["mode"] == "rps" {
            out.rps.push(v);
        } else {
            out.max.push(v);
        }
    }
    out
}

fn nums<'a>(runs: &'a [Value], f: impl Fn(&Value) -> Option<f64> + 'a) -> Vec<f64> {
    runs.iter().filter_map(f).collect()
}

fn ep_metric(run: &Value, ep: &str, key: &str) -> Option<f64> {
    run["endpoints"][ep][key].as_f64()
}

fn fmt_series(s: &Option<Series>, scale: f64, digits: usize) -> String {
    match s {
        None => "-".to_owned(),
        Some(s) if s.n > 1 => format!(
            "{:.d$} ({:.d$}–{:.d$})",
            s.median / scale,
            s.min / scale,
            s.max / scale,
            d = digits
        ),
        Some(s) => format!("{:.d$}", s.median / scale, d = digits),
    }
}

type Metric = Box<dyn Fn(&Value) -> Option<f64>>;

struct GateRow {
    area: String,
    name: String,
    java: Option<Series>,
    rust: Option<Series>,
    verdict: Option<Verdict>,
}

fn gate(area: &str, name: &str, java: Vec<f64>, rust: Vec<f64>, lower: bool) -> GateRow {
    let (j, r) = (Series::of(&java), Series::of(&rust));
    let verdict = match (&j, &r) {
        (Some(j), Some(r)) => Some(judge(j, r, lower)),
        _ => None,
    };
    GateRow {
        area: area.to_owned(),
        name: name.to_owned(),
        java: j,
        rust: r,
        verdict,
    }
}

fn load_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

pub fn build(dir: &Path) -> (String, bool) {
    let java = load_side(&dir.join("java"));
    let rust = load_side(&dir.join("rust"));
    let mut md = String::new();
    let mut gates: Vec<GateRow> = Vec::new();

    // 엔드포인트 이름 목록: 어느 한쪽이라도 잰 것.
    let mut names: Vec<String> = Vec::new();
    for side in [&java, &rust] {
        for run in side.rps.iter().chain(side.max.iter()) {
            if let Some(m) = run["endpoints"].as_object() {
                for k in m.keys() {
                    if !names.contains(k) {
                        names.push(k.clone());
                    }
                }
            }
        }
        for k in side.only.keys() {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }
    }
    names.sort();

    // 1. 지연 게이트: 고정 RPS 혼합 부하의 엔드포인트별 p99
    for ep in &names {
        let j = nums(&java.rps, |r| ep_metric(r, ep, "p99_us"));
        let r = nums(&rust.rps, |r| ep_metric(r, ep, "p99_us"));
        gates.push(gate("p99(고정 RPS 혼합)", ep, j, r, true));
    }
    // 2. 처리량 게이트: 엔드포인트 하나만 최대로 부른 실행
    for ep in &names {
        let j = nums(java.only.get(ep).map_or(&[][..], Vec::as_slice), |r| {
            r["total"]["rps"].as_f64()
        });
        let r = nums(rust.only.get(ep).map_or(&[][..], Vec::as_slice), |r| {
            r["total"]["rps"].as_f64()
        });
        gates.push(gate("처리량(엔드포인트별 최대)", ep, j, r, false));
    }
    // 3. 혼합 전체
    gates.push(gate(
        "처리량(혼합 최대)",
        "전체",
        nums(&java.max, |r| r["total"]["rps"].as_f64()),
        nums(&rust.max, |r| r["total"]["rps"].as_f64()),
        false,
    ));
    gates.push(gate(
        "p99(고정 RPS 혼합)",
        "전체",
        nums(&java.rps, |r| r["total"]["p99_us"].as_f64()),
        nums(&rust.rps, |r| r["total"]["p99_us"].as_f64()),
        true,
    ));
    // 4. 요청당 SQL
    let sql_j = load_json(&dir.join("sql-java.json"));
    let sql_r = load_json(&dir.join("sql-rust.json"));
    if let (Some(sj), Some(sr)) = (&sql_j, &sql_r) {
        for ep in sj["endpoints"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
        {
            let g = |s: &Value| {
                s["endpoints"][ep]["data_per_request"]
                    .as_f64()
                    .into_iter()
                    .collect::<Vec<_>>()
            };
            gates.push(gate("요청당 SQL(데이터 문장)", ep, g(sj), g(sr), true));
        }
    }

    let _ = writeln!(md, "# 성능 비교 결과 (Java 대 Rust)\n");
    let rounds_j = java.rps.len().max(java.max.len());
    let rounds_r = rust.rps.len().max(rust.max.len());
    let _ = writeln!(
        md,
        "- 라운드: Java {rounds_j}, Rust {rounds_r} (표의 값은 **중앙값 (최소–최대)**)"
    );
    let loads: Vec<f64> = java
        .rps
        .iter()
        .chain(&java.max)
        .chain(&rust.rps)
        .chain(&rust.max)
        .flat_map(|r| [r["load_before"][0].as_f64(), r["load_after"][0].as_f64()])
        .flatten()
        .collect();
    if let Some(l) = Series::of(&loads) {
        let _ = writeln!(
            md,
            "- 측정 중 호스트 부하 평균(1분): 중앙값 {:.1}, 최소 {:.1}, 최대 {:.1} (코어 수 대비 높으면 절대값은 믿을 수 없고 Java와 Rust의 상대 비교만 의미가 있다)\n",
            l.median, l.min, l.max
        );
    }

    // 게이트 요약
    let (mut pass, mut noise, mut fail, mut missing) = (0, 0, 0, 0);
    for g in &gates {
        match g.verdict {
            Some(Verdict::Pass) => pass += 1,
            Some(Verdict::PassWithinNoise) => noise += 1,
            Some(Verdict::Fail) => fail += 1,
            None => missing += 1,
        }
    }
    let _ = writeln!(md, "## 게이트 요약\n");
    let _ = writeln!(
        md,
        "통과 {pass}, 통과*(노이즈 이내) {noise}, **실패 {fail}**, 데이터 없음 {missing}\n"
    );
    let _ = writeln!(
        md,
        "| 항목 | 엔드포인트 | Java | Rust | 판정 |\n| --- | --- | ---: | ---: | --- |"
    );
    for g in &gates {
        let (scale, digits) = match g.area.as_str() {
            a if a.starts_with("p99") => (1000.0, 1),
            a if a.starts_with("처리량") => (1.0, 0),
            _ => (1.0, 2),
        };
        let unit = match g.area.as_str() {
            a if a.starts_with("p99") => " ms",
            a if a.starts_with("처리량") => " req/s",
            _ => "",
        };
        let _ = writeln!(
            md,
            "| {} | {} | {}{unit} | {}{unit} | {} |",
            g.area,
            g.name,
            fmt_series(&g.java, scale, digits),
            fmt_series(&g.rust, scale, digits),
            g.verdict.map_or("-", Verdict::label)
        );
    }

    // 혼합 부하 상세
    let _ = writeln!(md, "\n## 혼합 부하\n");
    let _ = writeln!(
        md,
        "| 모드 | 지표 | Java | Rust |\n| --- | --- | ---: | ---: |"
    );
    for (title, jr, rr) in [
        ("고정 RPS", &java.rps, &rust.rps),
        ("최대 처리량", &java.max, &rust.max),
    ] {
        let rows: Vec<(&str, Metric, f64, usize)> = vec![
            (
                "처리량 req/s",
                Box::new(|r| r["total"]["rps"].as_f64()),
                1.0,
                0,
            ),
            (
                "p50 ms",
                Box::new(|r| r["total"]["p50_us"].as_f64()),
                1000.0,
                1,
            ),
            (
                "p95 ms",
                Box::new(|r| r["total"]["p95_us"].as_f64()),
                1000.0,
                1,
            ),
            (
                "p99 ms",
                Box::new(|r| r["total"]["p99_us"].as_f64()),
                1000.0,
                1,
            ),
            (
                "최대 ms",
                Box::new(|r| r["total"]["max_us"].as_f64()),
                1000.0,
                1,
            ),
            (
                "오류 수",
                Box::new(|r| r["total"]["errors"].as_f64()),
                1.0,
                0,
            ),
            (
                "CPU 코어",
                Box::new(|r| r["system"]["cpu_cores"].as_f64()),
                1.0,
                2,
            ),
            (
                "요청당 CPU ms",
                Box::new(|r| {
                    let secs = r["system"]["cpu_secs"].as_f64()?;
                    let n = r["total"]["count"].as_f64()?;
                    (n > 0.0).then(|| secs * 1000.0 / n)
                }),
                1.0,
                3,
            ),
            (
                "RSS MB(최대)",
                Box::new(|r| r["system"]["rss_mb_max"].as_f64()),
                1.0,
                0,
            ),
            (
                "FD(최대)",
                Box::new(|r| r["system"]["fds_max"].as_f64()),
                1.0,
                0,
            ),
            (
                "스레드(최대)",
                Box::new(|r| r["system"]["threads_max"].as_f64()),
                1.0,
                0,
            ),
            (
                "요청당 DB 문장",
                Box::new(|r| r["db"]["data_statements_per_request"].as_f64()),
                1.0,
                2,
            ),
            (
                "요청당 DB ms",
                Box::new(|r| r["db"]["db_ms_per_request"].as_f64()),
                1.0,
                3,
            ),
        ];
        for (label, f, scale, digits) in rows {
            let j = Series::of(&nums(jr, &f));
            let r = Series::of(&nums(rr, &f));
            if j.is_none() && r.is_none() {
                continue;
            }
            let _ = writeln!(
                md,
                "| {title} | {label} | {} | {} |",
                fmt_series(&j, scale, digits),
                fmt_series(&r, scale, digits)
            );
        }
    }

    // 엔드포인트별 지연 상세 (고정 RPS)
    let _ = writeln!(md, "\n## 엔드포인트별 지연 (고정 RPS 혼합, ms)\n");
    let _ = writeln!(
        md,
        "| 엔드포인트 | Java p50 | Java p99 | Rust p50 | Rust p99 |\n| --- | ---: | ---: | ---: | ---: |"
    );
    for ep in &names {
        let cell = |side: &SideRuns, key: &str| {
            fmt_series(
                &Series::of(&nums(&side.rps, |r| ep_metric(r, ep, key))),
                1000.0,
                1,
            )
        };
        let _ = writeln!(
            md,
            "| {ep} | {} | {} | {} | {} |",
            cell(&java, "p50_us"),
            cell(&java, "p99_us"),
            cell(&rust, "p50_us"),
            cell(&rust, "p99_us")
        );
    }

    // SQL 상세
    if let (Some(sj), Some(sr)) = (&sql_j, &sql_r) {
        let _ = writeln!(md, "\n## 요청당 SQL과 DB 시간 (`pg_stat_statements`)\n");
        let _ = writeln!(
            md,
            "| 엔드포인트 | Java 데이터 | Rust 데이터 | Java 제어 | Rust 제어 | Java DB ms | Rust DB ms |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: |"
        );
        if let Some(eps) = sj["endpoints"].as_object() {
            for ep in eps.keys() {
                let f = |s: &Value, k: &str| s["endpoints"][ep][k].as_f64().unwrap_or(f64::NAN);
                let _ = writeln!(
                    md,
                    "| {ep} | {:.2} | {:.2} | {:.2} | {:.2} | {:.3} | {:.3} |",
                    f(sj, "data_per_request"),
                    f(sr, "data_per_request"),
                    f(sj, "control_per_request"),
                    f(sr, "control_per_request"),
                    f(sj, "db_ms_per_request"),
                    f(sr, "db_ms_per_request"),
                );
            }
        }
        let _ = writeln!(
            md,
            "\n제어 문장은 BEGIN/COMMIT/SET 등이다. JDBC가 트랜잭션 경계를 문장으로 보내는 방식과 `postgres` crate가 보내는 방식이 달라 따로 센다. 게이트는 데이터 문장(SELECT/INSERT/UPDATE/DELETE)으로 판정한다."
        );
    }

    // 기동 시간
    let st_j = load_json(&dir.join("startup-java.json"));
    let st_r = load_json(&dir.join("startup-rust.json"));
    if st_j.is_some() || st_r.is_some() {
        let _ = writeln!(
            md,
            "\n## 기동 시간 (컨테이너 시작 -> /actuator/health UP, ms)\n"
        );
        for (label, v) in [("Java", &st_j), ("Rust", &st_r)] {
            if let Some(v) = v {
                let m: Vec<f64> = v["millis"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_f64).collect())
                    .unwrap_or_default();
                if let Some(s) = Series::of(&m) {
                    let _ = writeln!(
                        md,
                        "- {label}: 중앙값 {:.0}, 최소 {:.0}, 최대 {:.0} ({}회)",
                        s.median, s.min, s.max, s.n
                    );
                }
            }
        }
    }

    // 동시성
    for side in ["java", "rust"] {
        if let Some(v) = load_json(&dir.join(format!("race-{side}.json"))) {
            let _ = writeln!(md, "\n## 동시성 시험: {side}\n");
            let _ = writeln!(
                md,
                "| 시험 | 결과 | 상태 코드 | 비고 |\n| --- | --- | --- | --- |"
            );
            for c in v["cases"].as_array().into_iter().flatten() {
                let ok = c["passed"].as_bool().unwrap_or(false);
                let fails: Vec<&str> = c["failures"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let _ = writeln!(
                    md,
                    "| {} | {} | `{}` | {} |",
                    c["name"].as_str().unwrap_or(""),
                    if ok { "통과" } else { "실패" },
                    c["statuses"],
                    fails.join("; ")
                );
            }
        }
    }

    // Java와 Rust의 같은 시험 결과 대조(상태 코드 분포, DB 최종 상태)
    let mut race_mismatch = false;
    if let (Some(java), Some(rust)) = (
        load_json(&dir.join("race-java.json")),
        load_json(&dir.join("race-rust.json")),
    ) {
        let diffs = race_diffs(&java, &rust);
        let _ = writeln!(
            md,
            "\n## 동시성 시험: Java 대 Rust 대조\n\n| 시험 | 상태 코드 분포 | DB 최종 상태 | 비고 |\n| --- | --- | --- | --- |"
        );
        for d in &diffs {
            race_mismatch |= d.blocks();
            let note = match (&d.known, d.differs()) {
                (Some(k), true) => format!("알려진 차이: {k}"),
                _ => String::new(),
            };
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} |",
                d.name,
                diff_cell(d.statuses),
                diff_cell(d.state),
                note
            );
        }
    }

    stress_section(&mut md, dir);

    let failed = fail > 0 || race_mismatch;
    (md, failed)
}

/// 같은 시험의 Java 결과와 Rust 결과를 대조한 한 줄.
#[derive(Debug, PartialEq, Eq)]
pub struct RaceDiff {
    pub name: String,
    /// `Some(true)`: 같다, `Some(false)`: 다르다, `None`: 비교 대상이 아니다(분포가 요청 순서에 달려 있는 시험).
    pub statuses: Option<bool>,
    /// 같은 뜻. DB 최종 상태(`state`)가 비어 있으면 `None`.
    pub state: Option<bool>,
    /// 알려진 차이의 설명(시험이 `known_diff`로 적은 것). 있으면 다르더라도 게이트를 막지 않는다.
    pub known: Option<String>,
}

impl RaceDiff {
    pub fn differs(&self) -> bool {
        self.statuses == Some(false) || self.state == Some(false)
    }

    /// 게이트를 막는 차이인가.
    pub fn blocks(&self) -> bool {
        self.differs() && self.known.is_none()
    }
}

/// 두 쪽 `race-*.json`의 시험을 이름으로 맞춰 상태 코드 분포(시험이 `compare_statuses`를 켠 경우)와 DB 최종 상태(`state`)를
/// 대조한다. 한쪽에만 있는 시험은 두 쪽 모두 다르다고 본다.
pub fn race_diffs(java: &Value, rust: &Value) -> Vec<RaceDiff> {
    let index = |v: &Value| -> BTreeMap<String, Value> {
        v["cases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c["name"].as_str()?.to_owned(), c.clone())))
            .collect()
    };
    let (j, r) = (index(java), index(rust));
    let mut names: Vec<&String> = j.keys().chain(r.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| match (j.get(name), r.get(name)) {
            (Some(a), Some(b)) => RaceDiff {
                known: a["known_diff"]
                    .as_str()
                    .or_else(|| b["known_diff"].as_str())
                    .map(str::to_owned),
                name: name.clone(),
                statuses: (a["compare_statuses"].as_bool() == Some(true)
                    && b["compare_statuses"].as_bool() == Some(true))
                .then(|| a["statuses"] == b["statuses"]),
                state: (!a["state"].is_null() || !b["state"].is_null())
                    .then(|| a["state"] == b["state"]),
            },
            _ => RaceDiff {
                name: name.clone(),
                statuses: Some(false),
                state: Some(false),
                known: None,
            },
        })
        .collect()
}

fn diff_cell(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "같음",
        Some(false) => "**다름**",
        None => "-",
    }
}

/// 스트레스 케이스 하나의 핵심 수치를 한 줄로(표의 셀). 없는 항목은 건너뛴다.
pub fn stress_metric(c: &Value) -> String {
    let ms = |v: &Value| v.as_f64().map(|u| format!("{:.0}ms", u / 1000.0));
    let phase = |name: &str| {
        c["phases"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["name"] == name))
    };
    let errs = |p: Option<&Value>| p.and_then(|p| p["errors"]["total"].as_u64()).unwrap_or(0);
    let x = &c["extra"];
    let mut parts: Vec<String> = Vec::new();
    match c["case"].as_str().unwrap_or("") {
        "spike" => {
            if let Some(p) = phase("급증") {
                parts.push(format!(
                    "급증 p99 {} max {} 오류 {}",
                    ms(&p["p99_us"]).unwrap_or_default(),
                    ms(&p["max_us"]).unwrap_or_default(),
                    errs(Some(p))
                ));
            }
            parts.push(format!(
                "복귀 {}초",
                x["recovery_secs"]
                    .as_u64()
                    .map_or("못 돌아옴".to_owned(), |s| s.to_string())
            ));
        }
        "storm" => {
            if let Some(p) = c["phases"].get(0) {
                let ok = p["ok"].as_u64().unwrap_or(0);
                let bad = errs(Some(p));
                parts.push(format!(
                    "성공 {ok}/{} 오류 {bad} 전체 {:.1}초",
                    ok + bad,
                    x["wall_secs"].as_f64().unwrap_or(0.0)
                ));
                parts.push(format!("p99 {}", ms(&p["p99_us"]).unwrap_or_default()));
            }
        }
        "idle" => {
            parts.push(format!(
                "붙음 {}/{} 유휴 CPU {:.2}코어(1000개당 {:.3}) RSS {:.0}MB 스레드 {}",
                x["opened"],
                x["idle_conns"],
                x["cpu_idle_only"]["cpu_cores"].as_f64().unwrap_or(0.0),
                x["cpu_cores_per_1000_idle"].as_f64().unwrap_or(0.0),
                x["rss_mb_idle_only"].as_f64().unwrap_or(0.0),
                x["threads_idle_only"]
            ));
            if let (Some(a), Some(b)) = (c["phases"].get(0), c["phases"].get(1)) {
                parts.push(format!(
                    "p99 {} -> {}",
                    ms(&a["p99_us"]).unwrap_or_default(),
                    ms(&b["p99_us"]).unwrap_or_default()
                ));
            }
        }
        "slow" => {
            parts.push(format!(
                "멈춘 연결 끊김 {}/{} (최초 {}초)",
                x["stalled"]["closed"],
                x["stalled"]["conns"],
                x["stalled"]["first_close_secs"]
                    .as_f64()
                    .map_or("-".to_owned(), |v| format!("{v:.1}"))
            ));
            parts.push(format!(
                "한 바이트씩 끊김 {}/{}",
                x["drip"]["closed"], x["drip"]["conns"]
            ));
            if let Some(p) = c["phases"].get(1) {
                parts.push(format!(
                    "일반 p99 {} 오류 {}",
                    ms(&p["p99_us"]).unwrap_or_default(),
                    errs(Some(p))
                ));
            }
        }
        "fairness" => {
            if let Some(p) = c["phases"].get(0) {
                parts.push(format!(
                    "p99 {} p99.9 {} max {} 500 {}",
                    ms(&p["p99_us"]).unwrap_or_default(),
                    ms(&p["p999_us"]).unwrap_or_default(),
                    ms(&p["max_us"]).unwrap_or_default(),
                    p["errors"]["status_500"]
                ));
            }
            parts.push(format!(
                "연결별 최소/중앙 {}/{}",
                x["per_conn_completed"]["min"], x["per_conn_completed"]["median"]
            ));
        }
        "dbkill" => {
            let rounds: Vec<String> = x["rounds"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    format!(
                        "오류 {} 복구 {:.1}초",
                        r["errors"]["total"],
                        r["recovery_secs"].as_f64().unwrap_or(0.0)
                    )
                })
                .collect();
            parts.push(rounds.join(" / "));
            parts.push(format!(
                "저부하 뒤 연결 {} 채운 뒤 {}",
                x["app_conns_low_load_end"], x["app_conns_after_refill"]
            ));
        }
        "outage" => {
            parts.push(format!(
                "{} 복구 {:.1}초 중단 중 오류 {}",
                x["mode"].as_str().unwrap_or(""),
                x["recovery_secs"].as_f64().unwrap_or(0.0),
                x["errors_during"]["total"]
            ));
            parts.push(format!("health {}", x["health_during"]));
        }
        "abort" => parts.push(format!(
            "끊긴 {} 처리된 행 {}",
            x["sent"], x["rows_inserted_after_unlock"]
        )),
        _ => {}
    }
    let leak = &c["leak"]["after"];
    parts.push(format!(
        "누수검사 후 DB연결 {} 스레드 {}",
        leak["app_db_conns"], leak["threads"]
    ));
    parts.join("; ")
}

fn stress_section(md: &mut String, dir: &Path) {
    let java = load_json(&dir.join("stress-java.json"));
    let rust = load_json(&dir.join("stress-rust.json"));
    if java.is_none() && rust.is_none() {
        return;
    }
    let _ = writeln!(md, "\n## 스트레스 시험 (`perf.sh stress`)\n");
    let _ = writeln!(
        md,
        "기준은 두 구현에 같다(docs/perf/stress.md). 이 표는 통과·실패가 Rust 대 Java 비교가 아니라 케이스별 절대 기준이다. 호스트가 공유일 때 수치는 참고용이다.\n"
    );
    let _ = writeln!(
        md,
        "| 케이스 | Java | Java 핵심 수치 | Rust | Rust 핵심 수치 |\n| --- | --- | --- | --- | --- |"
    );
    let mut names: Vec<String> = Vec::new();
    for v in [&java, &rust].into_iter().flatten() {
        for c in v["cases"].as_array().into_iter().flatten() {
            let n = c["name"].as_str().unwrap_or("").to_owned();
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    let find = |v: &Option<Value>, n: &str| -> Option<Value> {
        v.as_ref()?["cases"]
            .as_array()?
            .iter()
            .find(|c| c["name"] == n)
            .cloned()
    };
    let cell = |c: &Option<Value>| match c {
        None => ("-".to_owned(), "-".to_owned()),
        Some(c) => (
            if c["passed"].as_bool().unwrap_or(false) {
                "통과"
            } else {
                "실패"
            }
            .to_owned(),
            stress_metric(c),
        ),
    };
    let mut failures: Vec<String> = Vec::new();
    for n in &names {
        let (cj, cr) = (find(&java, n), find(&rust, n));
        let (vj, mj) = cell(&cj);
        let (vr, mr) = cell(&cr);
        let _ = writeln!(md, "| {n} | {vj} | {mj} | {vr} | {mr} |");
        for (side, c) in [("Java", &cj), ("Rust", &cr)] {
            for f in c
                .iter()
                .flat_map(|c| c["failures"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
            {
                failures.push(format!("- {side} {n}: {f}"));
            }
        }
    }
    if !failures.is_empty() {
        let _ = writeln!(md, "\n실패 사유:\n\n{}", failures.join("\n"));
    }
}

pub fn command(args: &Args) -> Result<u8, String> {
    let dir = args.require("dir")?;
    let (md, failed) = build(Path::new(dir));
    println!("{md}");
    if let Some(out) = args.str("out") {
        fs::write(out, &md).map_err(|e| format!("{out}: {e}"))?;
    }
    Ok(u8::from(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[f64]) -> Series {
        Series::of(v).unwrap()
    }

    #[test]
    fn 중앙값과_퍼짐() {
        let x = s(&[3.0, 1.0, 2.0]);
        assert_eq!((x.median, x.min, x.max, x.n), (2.0, 1.0, 3.0, 3));
        assert!((x.spread() - 1.0).abs() < 1e-9);
        assert_eq!(s(&[1.0, 3.0]).median, 2.0);
        assert!(Series::of(&[]).is_none());
        assert_eq!(s(&[5.0]).spread(), 0.0);
    }

    #[test]
    fn 낮을수록_좋은_항목은_rust가_작거나_같으면_통과() {
        assert_eq!(judge(&s(&[10.0]), &s(&[9.0]), true), Verdict::Pass);
        assert_eq!(judge(&s(&[10.0]), &s(&[10.0]), true), Verdict::Pass);
    }

    #[test]
    fn 높을수록_좋은_항목은_rust가_크거나_같으면_통과() {
        assert_eq!(judge(&s(&[100.0]), &s(&[120.0]), false), Verdict::Pass);
        assert_eq!(
            judge(&s(&[100.0]), &s(&[99.0]), false),
            Verdict::Fail,
            "라운드 하나뿐이면 노이즈로 면제하지 않는다"
        );
    }

    #[test]
    fn 차이가_노이즈_안이면_통과별표() {
        // Java 라운드 퍼짐 (120-80)/100 = 40%, Rust가 5% 나쁨
        let java = s(&[80.0, 100.0, 120.0]);
        let rust = s(&[104.0, 105.0, 106.0]);
        assert_eq!(judge(&java, &rust, true), Verdict::PassWithinNoise);
        // 노이즈보다 크게 나쁘면 실패
        let rust = s(&[190.0, 200.0, 210.0]);
        assert_eq!(judge(&java, &rust, true), Verdict::Fail);
    }

    #[test]
    fn 데이터가_없으면_판정하지_않는다() {
        let g = gate("x", "y", vec![], vec![1.0], true);
        assert!(g.verdict.is_none());
    }

    #[test]
    fn 스트레스_결과를_표로_만든다() {
        let dir =
            std::env::temp_dir().join(format!("loadgen-stress-report-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let case = |pass: bool| {
            serde_json::json!({ "kind": "stress", "label": "x", "cases": [{
                "name": "spike-200", "case": "spike", "passed": pass,
                "failures": if pass { vec![] } else { vec!["급증 구간 오류: 503 3"] },
                "phases": [{ "name": "급증", "p99_us": 120000, "max_us": 900000, "errors": { "total": 3 } }],
                "extra": { "recovery_secs": 2 },
                "leak": { "after": { "app_db_conns": 10, "threads": 40 } },
            }]})
        };
        fs::write(dir.join("stress-java.json"), case(true).to_string()).unwrap();
        fs::write(dir.join("stress-rust.json"), case(false).to_string()).unwrap();
        let (md, _) = build(&dir);
        assert!(md.contains("## 스트레스 시험"), "{md}");
        assert!(md.contains("| spike-200 | 통과 | 급증 p99 120ms max 900ms 오류 3; 복귀 2초; 누수검사 후 DB연결 10 스레드 40 | 실패 |"), "{md}");
        assert!(
            md.contains("- Rust spike-200: 급증 구간 오류: 503 3"),
            "{md}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 보고서를_만든다() {
        let dir = std::env::temp_dir().join(format!("loadgen-report-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for side in ["java", "rust"] {
            fs::create_dir_all(dir.join(side)).unwrap();
            let p99 = if side == "java" { 20_000.0 } else { 10_000.0 };
            let rps = if side == "java" { 100.0 } else { 150.0 };
            let run = serde_json::json!({
                "kind": "run", "mode": "rps", "only": null,
                "total": { "rps": rps, "p99_us": p99, "p50_us": 1000.0, "p95_us": 5000.0, "max_us": 30000.0, "errors": 0, "count": 100 },
                "endpoints": { "GET /a": { "p99_us": p99, "p50_us": 1000.0, "rps": rps } },
                "load_before": [1.0, 1.0, 1.0], "load_after": [2.0, 1.0, 1.0],
            });
            fs::write(dir.join(side).join("mix-rps-r1.json"), run.to_string()).unwrap();
            let only = serde_json::json!({
                "kind": "run", "mode": "max", "only": "GET /a",
                "total": { "rps": rps, "p99_us": p99, "p50_us": 1000.0, "errors": 0, "count": 100 },
                "endpoints": { "GET /a": { "p99_us": p99, "rps": rps } },
            });
            fs::write(dir.join(side).join("ep-a-r1.json"), only.to_string()).unwrap();
            let sql = serde_json::json!({ "kind": "sql", "endpoints": { "GET /a": { "data_per_request": 3.0, "control_per_request": 2.0, "db_ms_per_request": 0.5 } } });
            fs::write(dir.join(format!("sql-{side}.json")), sql.to_string()).unwrap();
        }
        let (md, failed) = build(&dir);
        assert!(!failed, "{md}");
        assert!(md.contains("| p99(고정 RPS 혼합) | GET /a |"), "{md}");
        assert!(md.contains("통과 4"), "{md}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 동시성_대조는_분포와_최종_상태를_이름으로_맞춘다() {
        let case = |name: &str, st: Value, state: Value, cmp: bool| serde_json::json!({"name": name, "statuses": st, "state": state, "compare_statuses": cmp});
        let known_case = |name: &str, state: Value, note: &str| serde_json::json!({"name": name, "statuses": {}, "state": state, "compare_statuses": false, "known_diff": note});
        let java = serde_json::json!({"cases": [
            case("a", serde_json::json!({"200": 1, "409": 7}), serde_json::json!({"n": 1}), true),
            case("b", serde_json::json!({"200": 3}), serde_json::json!({"n": 1}), false),
            case("c", serde_json::json!({"200": 1}), Value::Null, false),
            case("only-java", serde_json::json!({}), Value::Null, false),
            known_case("k", serde_json::json!({"x": true}), "알려진 차이"),
        ]});
        let rust = serde_json::json!({"cases": [
            case("a", serde_json::json!({"200": 1, "409": 7}), serde_json::json!({"n": 1}), true),
            case("b", serde_json::json!({"200": 2, "404": 1}), serde_json::json!({"n": 2}), false),
            case("c", serde_json::json!({"200": 1}), Value::Null, false),
            known_case("k", serde_json::json!({"x": false}), "알려진 차이"),
        ]});
        let d = race_diffs(&java, &rust);
        let by = |n: &str| d.iter().find(|x| x.name == n).unwrap();
        assert_eq!((by("a").statuses, by("a").state), (Some(true), Some(true)));
        assert!(!by("a").differs());
        assert_eq!(by("b").statuses, None, "분포를 비교하지 않는 시험");
        assert_eq!(by("b").state, Some(false));
        assert!(by("b").blocks());
        assert_eq!((by("c").statuses, by("c").state), (None, None));
        assert!(!by("c").differs());
        assert!(by("only-java").blocks(), "한쪽에만 있으면 다르다");
        assert!(
            by("k").differs() && !by("k").blocks(),
            "알려진 차이는 게이트를 막지 않는다"
        );
    }
}
