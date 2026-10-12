//! 차등 검증 러너.
//!
//! ```text
//! parity compare --left harness/env/java-a.toml --right harness/env/rust.toml [--scenarios DIR] [--filter TAG]
//! parity record  --target harness/env/java-a.toml --out harness/golden [--scenarios DIR] [--filter TAG]
//! parity verify  --target harness/env/rust.toml  --golden harness/golden [--scenarios DIR] [--filter TAG]
//! ```
//!
//! `compare`는 두 대상에 같은 시나리오를 차례로 실행해 정규화 결과를 비교한다. 결과는
//! `harness/out/<시나리오>.{left,right}.json`에도 남는다. 하나라도 다르면 종료 코드 1.

mod http;
mod normalize;
mod runner;
mod scenario;
mod target;
mod time;
mod token;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use target::Target;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
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

    match cmd {
        "compare" => {
            let left = Target::load(Path::new(opt(args, "--left").ok_or("--left가 필요합니다")?))?;
            let right = Target::load(Path::new(
                opt(args, "--right").ok_or("--right가 필요합니다")?,
            ))?;
            let mut failed = 0;
            for s in &scenarios {
                let l = runner::run(&left, s)?;
                let r = runner::run(&right, s)?;
                let key = file_key(&s.path, &scenarios_dir);
                write_json(&out_dir.join(format!("{key}.left.json")), &l)?;
                write_json(&out_dir.join(format!("{key}.right.json")), &r)?;
                report(&s.name, &key, runner::diff(&l, &r), &mut failed);
            }
            summary(scenarios.len(), failed)
        }
        "record" => {
            let target = Target::load(Path::new(
                opt(args, "--target").ok_or("--target이 필요합니다")?,
            ))?;
            let golden = PathBuf::from(opt(args, "--out").unwrap_or("harness/golden"));
            for s in &scenarios {
                let v = runner::run(&target, s)?;
                let key = file_key(&s.path, &scenarios_dir);
                write_json(&golden.join(format!("{key}.json")), &v)?;
                println!("녹화  {key}");
            }
            Ok(true)
        }
        "verify" => {
            let target = Target::load(Path::new(
                opt(args, "--target").ok_or("--target이 필요합니다")?,
            ))?;
            let golden = PathBuf::from(opt(args, "--golden").unwrap_or("harness/golden"));
            let mut failed = 0;
            for s in &scenarios {
                let key = file_key(&s.path, &scenarios_dir);
                let path = golden.join(format!("{key}.json"));
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let expected: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let actual = runner::run(&target, s)?;
                write_json(&out_dir.join(format!("{key}.actual.json")), &actual)?;
                report(&s.name, &key, runner::diff(&expected, &actual), &mut failed);
            }
            summary(scenarios.len(), failed)
        }
        _ => Err("사용법: parity <compare|record|verify> ... (src/main.rs 주석 참고)".into()),
    }
}

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
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

fn report(name: &str, key: &str, diff: Option<String>, failed: &mut usize) {
    match diff {
        None => println!("통과  {key}  ({name})"),
        Some(d) => {
            *failed += 1;
            println!("실패  {key}  ({name})\n{d}");
        }
    }
}

fn summary(total: usize, failed: usize) -> Result<bool, String> {
    println!(
        "\n시나리오 {total}개 중 {}개 통과, {failed}개 실패",
        total - failed
    );
    Ok(failed == 0)
}
