//! 차등 검증 러너.
//!
//! ```text
//! parity compare --left harness/env/java-a.toml --right harness/env/rust.toml [--scenarios DIR] [--filter TAG]
//! parity record  --target harness/env/java-a.toml --out harness/golden [--scenarios DIR] [--filter TAG]
//! parity verify  --target harness/env/rust.toml  --golden harness/golden [--scenarios DIR] [--filter TAG]
//! ```
//!
//! `compare`는 두 대상에 같은 시나리오를 실행해 정규화 결과를 비교한다. 결과는
//! `harness/out/<시나리오>.{left,right}.json`에도 남는다. 하나라도 다르면 종료 코드 1, 하네스 오류가 있으면 2.
//! 끝나면 대상의 시계 파일을 "+0"(실제 시각)으로 되돌린다.
//!
//! `--target`·`--left`·`--right`에는 대상 설정을 쉼표로 여러 개 줄 수 있다(`harness/scripts/lane.sh targets rust`).
//! 그러면 대상마다 작업 스레드 하나가 시나리오를 나눠 가져가 동시에 실행한다. `compare`는 i번째 left와 i번째
//! right를 짝으로 쓰고, 짝의 두 대상이 가짜 서버를 함께 쓰지 않으면 두 대상도 동시에 돌린다.
//! 대상 설정에 `lock_file`이 있으면 그 대상을 쓰는 동안 잠그고, 다른 작업이 잡고 있는 대상은 건너뛴다
//! (모두 잡혀 있으면 첫 대상이 풀릴 때까지 기다린다).

mod callback;
mod fakes;
mod http;
mod idtoken;
mod normalize;
mod runner;
mod scenario;
mod target;
mod time;
mod token;

use scenario::Scenario;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use target::Target;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(_) if HARNESS_ERROR.load(Ordering::SeqCst) => ExitCode::from(2),
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("오류: {e}");
            ExitCode::from(2)
        }
    }
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
}

fn run(args: &[String]) -> Result<bool, String> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let scenarios_dir = PathBuf::from(opt(args, "--scenarios").unwrap_or("harness/scenarios"));
    let scenarios = scenario::load_dir(&scenarios_dir, opt(args, "--filter"))?;
    let out_dir = PathBuf::from("harness/out");
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let key = |s: &Scenario| file_key(&s.path, &scenarios_dir);
    let started = Instant::now();

    let outcome = match cmd {
        "compare" => {
            let lefts = load_targets(opt(args, "--left").ok_or("--left가 필요합니다")?)?;
            let rights = load_targets(opt(args, "--right").ok_or("--right가 필요합니다")?)?;
            let (lefts, _l) = acquire(&lefts)?;
            let (rights, _r) = acquire(&rights)?;
            let workers: Vec<(&Target, &Target)> = lefts.into_iter().zip(rights).collect();
            let result = pool("통과", &scenarios, &workers, |&(left, right), s| {
                let (l, r) = if left.fakes_url.is_some() && left.fakes_url == right.fakes_url {
                    // 가짜 서버의 요청 기록을 함께 쓰므로 차례로 돌린다.
                    (runner::run(left, s), runner::run(right, s))
                } else {
                    std::thread::scope(|t| {
                        let h = t.spawn(|| runner::run(left, s));
                        let r = runner::run(right, s);
                        (
                            h.join().unwrap_or_else(|_| Err("작업 스레드 패닉".into())),
                            r,
                        )
                    })
                };
                let (l, r) = (l?, r?);
                write_json(&out_dir.join(format!("{}.left.json", key(s))), &l)?;
                write_json(&out_dir.join(format!("{}.right.json", key(s))), &r)?;
                Ok(runner::diff(&l, &r))
            });
            for (l, r) in &workers {
                l.reset_clock();
                r.reset_clock();
            }
            result
        }
        "record" => {
            let targets = load_targets(opt(args, "--target").ok_or("--target이 필요합니다")?)?;
            let (targets, _locks) = acquire(&targets)?;
            let golden = PathBuf::from(opt(args, "--out").unwrap_or("harness/golden"));
            let result = pool("녹화", &scenarios, &targets, |target, s| {
                let v = runner::run(target, s)?;
                write_json(&golden.join(format!("{}.json", key(s))), &v)?;
                Ok(None)
            });
            targets.iter().for_each(|t| t.reset_clock());
            result
        }
        "verify" => {
            let targets = load_targets(opt(args, "--target").ok_or("--target이 필요합니다")?)?;
            let (targets, _locks) = acquire(&targets)?;
            let golden = PathBuf::from(opt(args, "--golden").unwrap_or("harness/golden"));
            let result = pool("통과", &scenarios, &targets, |target, s| {
                let path = golden.join(format!("{}.json", key(s)));
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let mut expected: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| e.to_string())?;
                runner::canonicalize(&mut expected);
                let actual = runner::run(target, s)?;
                write_json(&out_dir.join(format!("{}.actual.json", key(s))), &actual)?;
                Ok(runner::diff(&expected, &actual))
            });
            targets.iter().for_each(|t| t.reset_clock());
            result
        }
        _ => {
            return Err(
                "사용법: parity <compare|record|verify> ... (src/main.rs 주석 참고)".into(),
            );
        }
    };
    Ok(summary(
        cmd,
        &scenarios,
        &outcome,
        key,
        started.elapsed().as_secs_f64(),
    ))
}

/// 쉼표로 이은 대상 설정 경로를 읽는다.
fn load_targets(list: &str) -> Result<Vec<Target>, String> {
    let targets = list
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| Target::load(Path::new(p)))
        .collect::<Result<Vec<_>, _>>()?;
    if targets.is_empty() {
        return Err(format!("대상 설정이 없습니다: {list:?}"));
    }
    Ok(targets)
}

/// 다른 작업이 쓰고 있지 않은 대상만 잠가서 고른다. 모두 쓰는 중이면 첫 대상이 풀릴 때까지 기다린다.
fn acquire(targets: &[Target]) -> Result<(Vec<&Target>, Vec<File>), String> {
    let mut picked = Vec::new();
    let mut locks = Vec::new();
    for t in targets {
        if let Some(lock) = t.lock(false)? {
            picked.push(t);
            locks.push(lock);
        }
    }
    if picked.is_empty() {
        eprintln!(
            "대상이 모두 사용 중이라 {}이(가) 풀리기를 기다립니다",
            targets[0].name
        );
        let lock = targets[0].lock(true)?.ok_or("잠금 실패")?;
        picked.push(&targets[0]);
        locks.push(lock);
    }
    Ok((picked, locks))
}

/// 시나리오 하나의 결과: 차이 설명(같으면 None) 또는 하네스 오류.
type Outcome = Result<Option<String>, String>;

/// 작업자(대상 또는 대상 짝)마다 스레드 하나가 남은 시나리오를 하나씩 가져가 실행한다.
/// 결과는 끝나는 대로 출력하고, 시나리오 순서대로 모아 돌려준다.
fn pool<W: Sync>(
    label: &str,
    scenarios: &[Scenario],
    workers: &[W],
    job: impl Fn(&W, &Scenario) -> Outcome + Sync,
) -> Vec<Outcome> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Outcome>>> =
        Mutex::new((0..scenarios.len()).map(|_| None).collect());
    let print = Mutex::new(());
    std::thread::scope(|t| {
        for w in workers {
            t.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(s) = scenarios.get(i) else { break };
                    let at = Instant::now();
                    let outcome = job(w, s);
                    {
                        let _p = print.lock().unwrap_or_else(|e| e.into_inner());
                        report_one(s, &outcome, at.elapsed().as_secs_f64(), label);
                    }
                    results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(outcome);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|o| o.unwrap_or_else(|| Err("실행되지 않음".into())))
        .collect()
}

fn report_one(s: &Scenario, outcome: &Outcome, secs: f64, ok_label: &str) {
    let name = &s.name;
    let path = s.path.display();
    match outcome {
        Ok(None) => println!("{ok_label}  {path}  ({name}, {secs:.1}s)"),
        Ok(Some(d)) => println!("실패  {path}  ({name}, {secs:.1}s)\n{d}"),
        Err(e) => println!("오류  {path}  ({name})\n    {e}"),
    }
}

/// 전체 결과를 요약한다. 차이가 없으면 참. 하네스 오류는 종료 코드 2로 따로 알린다.
fn summary(
    cmd: &str,
    scenarios: &[Scenario],
    outcomes: &[Outcome],
    key: impl Fn(&Scenario) -> String,
    secs: f64,
) -> bool {
    let failed: Vec<String> = scenarios
        .iter()
        .zip(outcomes)
        .filter(|(_, o)| matches!(o, Ok(Some(_))))
        .map(|(s, _)| key(s))
        .collect();
    let errors: Vec<String> = scenarios
        .iter()
        .zip(outcomes)
        .filter(|(_, o)| o.is_err())
        .map(|(s, _)| key(s))
        .collect();
    let total = scenarios.len();
    if cmd == "record" {
        println!(
            "\n시나리오 {total}개 중 {}개 녹화 ({secs:.0}s)",
            total - errors.len()
        );
    } else {
        println!(
            "\n시나리오 {total}개 중 {}개 통과, {}개 실패, {}개 오류 ({secs:.0}s)",
            total - failed.len() - errors.len(),
            failed.len(),
            errors.len()
        );
    }
    for k in &failed {
        println!("  실패 {k}");
    }
    for k in &errors {
        println!("  오류 {k}");
    }
    if !errors.is_empty() {
        HARNESS_ERROR.store(true, Ordering::SeqCst);
    }
    failed.is_empty() && errors.is_empty()
}

static HARNESS_ERROR: AtomicBool = AtomicBool::new(false);

fn file_key(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .replace(['/', '\\'], "__")
}

fn write_json(path: &Path, v: &serde_json::Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // 키를 정렬해 쓴다. serde_json 빌드 기능에 따라 golden 파일의 키 순서가 달라지지 않게 한다.
    let text = serde_json::to_string_pretty(&runner::sorted_keys(v)).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}
