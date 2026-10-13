//! `chalkak-loadgen`: 성능·동시성·장시간 시험용 부하 발생기. 사용법은 `harness/perf/README.md`.
//!
//! ```text
//! chalkak-loadgen seed   --db "<접속 문자열>" [--users N --days D --posts-per-topic P ...]
//! chalkak-loadgen run    --base http://127.0.0.1:8080 --secret <hex> --label java [옵션...] --out result.json
//! chalkak-loadgen sql    --base ... --secret <hex> --db "<접속 문자열>" --label java --out sql.json
//! chalkak-loadgen race   --base ... --secret <hex> --db "<접속 문자열>" --label java --out race.json
//! chalkak-loadgen report --dir <결과 디렉터리> [--out report.md]
//! ```

mod args;
mod client;
mod hist;
mod load;
mod mix;
mod race;
mod report;
mod run_cmd;
mod seed;
mod sql;
mod sys;
mod token;

use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = raw.split_first() else {
        eprintln!("{}", USAGE);
        return ExitCode::from(2);
    };
    let args = match args::Args::parse(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("chalkak-loadgen: {e}");
            return ExitCode::from(2);
        }
    };
    let result = match cmd.as_str() {
        "list" => {
            for spec in mix::WEIGHTS {
                println!("{}", spec.name);
            }
            Ok(0)
        }
        "seed" => run_cmd::seed(&args),
        "run" => run_cmd::run(&args),
        "sql" => run_cmd::sql(&args),
        "race" => race::command(&args),
        "report" => report::command(&args),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("알 수 없는 명령: {other}\n{USAGE}")),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("chalkak-loadgen: {e}");
            ExitCode::from(2)
        }
    }
}

const USAGE: &str = "사용법: chalkak-loadgen <seed|run|sql|race|report> [--옵션 값]...\n\
  seed    DB에 시드 데이터를 넣는다\n\
  run     혼합 부하를 건다(고정 RPS 또는 최대 처리량), 지연·CPU·RSS·FD를 JSON으로 남긴다\n\
  sql     엔드포인트별 요청당 SQL 수와 DB 시간을 pg_stat_statements로 잰다\n\
  race    같은 자원에 동시 요청을 보내고 DB 불변식을 확인한다\n\
  report  결과 디렉터리의 Java·Rust 결과를 비교해 게이트를 판정한다";
