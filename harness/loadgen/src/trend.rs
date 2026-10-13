//! `trend`: 장시간 시험 로그(`run --interval`이 찍는 줄)에서 메모리·FD·스레드·DB 연결의 추세를 구해 누수 여부를 판정한다.
//!
//! 처음 일부(기본 20%)는 워밍업·JIT·캐시 채우기로 보고 버린 뒤 최소제곱 직선의 기울기(시간당 증가량)를 본다.
//! 한 번 오른 뒤 평평한 계단형은 누수가 아니므로 기울기와 함께 **마지막 값이 처음 이후 최댓값에서 얼마나 올랐는지**도 본다.

use std::fs;

use crate::args::Args;

#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub secs: f64,
    pub rps: f64,
    pub errors: f64,
    pub p99_ms: f64,
    pub rss_mb: f64,
    pub fds: f64,
    pub threads: f64,
    pub db_conns: Option<f64>,
}

fn after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let i = line.find(key)? + key.len();
    Some(line[i..].trim_start())
}

fn number(s: &str) -> Option<f64> {
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

/// `[    60s] rps     150  err    0  p50 ...ms  p99 ...ms  max ...ms  rss 30MB  fd 112  threads 35  db연결 12  load 55.0`
pub fn parse_line(line: &str) -> Option<Point> {
    let rest = line.trim_start().strip_prefix('[')?;
    let secs = number(rest.trim_start())?;
    Some(Point {
        secs,
        rps: number(after(line, "rps ")?)?,
        errors: number(after(line, "err ")?)?,
        p99_ms: number(after(line, "p99 ")?)?,
        rss_mb: number(after(line, "rss ")?)?,
        fds: number(after(line, "fd ")?)?,
        threads: number(after(line, "threads ")?)?,
        db_conns: after(line, "db연결 ").and_then(number),
    })
}

pub fn parse_log(text: &str) -> Vec<Point> {
    text.lines().filter_map(parse_line).collect()
}

#[derive(Debug, PartialEq)]
pub struct Fit {
    pub first: f64,
    pub last: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// 시간당 기울기.
    pub slope_per_hour: f64,
    /// 마지막 값 - (분석 구간에서의 중앙값 근처 기준선: 앞 절반의 최댓값).
    pub rise_over_early_max: f64,
}

pub fn fit(points: &[(f64, f64)]) -> Option<Fit> {
    if points.len() < 3 {
        return None;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mean_x).powi(2)).sum();
    let sxy: f64 = points.iter().map(|p| (p.0 - mean_x) * (p.1 - mean_y)).sum();
    let slope_per_sec = if sxx == 0.0 { 0.0 } else { sxy / sxx };
    let half = points.len() / 2;
    let early_max = points[..half.max(1)]
        .iter()
        .map(|p| p.1)
        .fold(f64::MIN, f64::max);
    let last = points[points.len() - 1].1;
    Some(Fit {
        first: points[0].1,
        last,
        min: points.iter().map(|p| p.1).fold(f64::MAX, f64::min),
        max: points.iter().map(|p| p.1).fold(f64::MIN, f64::max),
        mean: mean_y,
        slope_per_hour: slope_per_sec * 3600.0,
        rise_over_early_max: last - early_max,
    })
}

pub struct Limits {
    pub pool: f64,
}

pub struct Finding {
    pub series: &'static str,
    pub fit: Fit,
    pub leak: bool,
    pub note: String,
}

/// 누수 판정 기준:
/// - RSS: 기울기가 시간당 평균의 5% 또는 10MB 중 큰 값을 넘고, 마지막 값도 앞 절반 최댓값보다 높다.
/// - FD·스레드: 기울기가 시간당 5개를 넘고 마지막 값이 앞 절반 최댓값보다 높다.
/// - DB 연결: 풀 크기를 넘으면 누수(관찰용 연결은 세지 않는다).
pub fn analyze(points: &[Point], skip: f64, limits: &Limits) -> Vec<Finding> {
    let start = ((points.len() as f64) * skip) as usize;
    let used = &points[start.min(points.len())..];
    let series =
        |f: fn(&Point) -> f64| -> Vec<(f64, f64)> { used.iter().map(|p| (p.secs, f(p))).collect() };
    let mut out = Vec::new();
    if let Some(fit) = fit(&series(|p| p.rss_mb)) {
        let allow = (fit.mean * 0.05).max(10.0);
        let leak = fit.slope_per_hour > allow && fit.rise_over_early_max > 0.0;
        let note = format!("허용 기울기 {allow:.1}MB/h");
        out.push(Finding {
            series: "RSS(MB)",
            fit,
            leak,
            note,
        });
    }
    for (name, f) in [
        ("FD", (|p: &Point| p.fds) as fn(&Point) -> f64),
        ("스레드", |p: &Point| p.threads),
    ] {
        if let Some(fit) = fit(&series(f)) {
            let leak = fit.slope_per_hour > 5.0 && fit.rise_over_early_max > 0.0;
            out.push(Finding {
                series: name,
                fit,
                leak,
                note: "허용 기울기 5개/h".to_owned(),
            });
        }
    }
    let db: Vec<(f64, f64)> = used
        .iter()
        .filter_map(|p| Some((p.secs, p.db_conns?)))
        .collect();
    if let Some(fit) = fit(&db) {
        let leak = fit.max > limits.pool;
        out.push(Finding {
            series: "DB 연결",
            fit,
            leak,
            note: format!("풀 크기 {} 이하", limits.pool),
        });
    }
    out
}

pub fn command(args: &Args) -> Result<u8, String> {
    let path = args.require("log")?;
    let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let points = parse_log(&text);
    if points.len() < 6 {
        return Err(format!(
            "표본이 너무 적습니다: {}개 (줄 형식은 run --interval이 찍는 것)",
            points.len()
        ));
    }
    let skip: f64 = args.parsed_or("skip", 0.2)?;
    let pool: f64 = args.parsed_or("pool", 10.0)?;
    let findings = analyze(&points, skip, &Limits { pool });
    let total_errors: f64 = points.iter().map(|p| p.errors).sum();
    println!(
        "표본 {}개, {:.0}초 구간, 앞 {:.0}% 제외, 오류 합계 {total_errors:.0}",
        points.len(),
        points.last().map_or(0.0, |p| p.secs),
        skip * 100.0
    );
    println!(
        "{:<8} {:>9} {:>9} {:>9} {:>9} {:>12}  판정",
        "항목", "처음", "마지막", "최소", "최대", "기울기/h"
    );
    let mut leaks = 0;
    for f in &findings {
        println!(
            "{:<8} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>12.2}  {} ({})",
            f.series,
            f.fit.first,
            f.fit.last,
            f.fit.min,
            f.fit.max,
            f.fit.slope_per_hour,
            if f.leak {
                "누수 의심"
            } else {
                "이상 없음"
            },
            f.note
        );
        leaks += usize::from(f.leak);
    }
    Ok(u8::from(leaks > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(secs: u32, rss: u32, fd: u32, threads: u32, db: u32) -> String {
        format!(
            "[{secs:>6}s] rps     150  err    0  p50    3.1ms  p99    40.2ms  max    900.0ms  rss {rss}MB  fd {fd}  threads {threads}  db연결 {db}  load 55.1"
        )
    }

    #[test]
    fn 줄을_읽는다() {
        let p = parse_line(&line(60, 30, 112, 35, 12)).unwrap();
        assert_eq!(
            (p.secs, p.rss_mb, p.fds, p.threads),
            (60.0, 30.0, 112.0, 35.0)
        );
        assert_eq!(p.db_conns, Some(12.0));
        assert_eq!(p.p99_ms, 40.2);
        assert!(parse_line("워밍업 5초...").is_none());
        assert!(parse_line("rust [max] 연결 64: ...").is_none());
    }

    #[test]
    fn db_연결이_없는_줄도_읽는다() {
        let l = "[    60s] rps     150  err    0  p50    3.1ms  p99    40.2ms  max    900.0ms  rss 30MB  fd 112  threads 35  db연결 -  load 55.1";
        assert_eq!(parse_line(l).unwrap().db_conns, None);
    }

    fn series(f: impl Fn(u32) -> (u32, u32, u32, u32)) -> Vec<Point> {
        (1..=60)
            .map(|i| {
                let (rss, fd, th, db) = f(i);
                parse_line(&line(i * 60, rss, fd, th, db)).unwrap()
            })
            .collect()
    }

    #[test]
    fn 평평하면_이상_없다() {
        let pts = series(|_| (30, 112, 35, 10));
        let findings = analyze(&pts, 0.2, &Limits { pool: 10.0 });
        assert!(
            findings.iter().all(|f| !f.leak),
            "{:?}",
            findings.iter().map(|f| f.series).collect::<Vec<_>>()
        );
    }

    #[test]
    fn 계단형_증가_뒤_평평하면_누수가_아니다() {
        // 워밍업(앞 20%)에서 오르고 이후 평평
        let pts = series(|i| (if i < 10 { 20 + i } else { 30 }, 112, 35, 10));
        let findings = analyze(&pts, 0.2, &Limits { pool: 10.0 });
        assert!(findings.iter().all(|f| !f.leak));
    }

    #[test]
    fn 꾸준히_늘면_누수로_본다() {
        // 1분에 1MB씩(시간당 60MB), FD 1개씩(시간당 60개)
        let pts = series(|i| (100 + i, 100 + i, 35, 10));
        let findings = analyze(&pts, 0.2, &Limits { pool: 10.0 });
        let leaks: Vec<_> = findings
            .iter()
            .filter(|f| f.leak)
            .map(|f| f.series)
            .collect();
        assert_eq!(leaks, ["RSS(MB)", "FD"]);
    }

    #[test]
    fn 연결이_풀_크기를_넘으면_누수로_본다() {
        let pts = series(|i| (30, 112, 35, if i == 40 { 11 } else { 10 }));
        let findings = analyze(&pts, 0.2, &Limits { pool: 10.0 });
        assert!(findings.iter().any(|f| f.series == "DB 연결" && f.leak));
    }

    #[test]
    fn 표본이_적으면_맞추지_않는다() {
        assert!(fit(&[(0.0, 1.0), (1.0, 2.0)]).is_none());
    }
}
