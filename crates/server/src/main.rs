//! `chalkak-server` 실행 파일. 설정을 읽고 SIGTERM·SIGINT에서 정상 종료한다.

use std::process::ExitCode;

use chalkak_core::{Env, ShutdownSignal};
use chalkak_server::settings::Settings;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

fn main() -> ExitCode {
    // Spring의 `spring.config.import: optional:file:.env[.properties]`.
    let env = match Env::from_process_with_dotenv(".env") {
        Ok(env) => env,
        Err(e) => return fail(&e),
    };
    let settings = match Settings::from_env(&env) {
        Ok(settings) => settings,
        Err(e) => return fail(&e),
    };

    let shutdown = ShutdownSignal::new();
    match Signals::new([SIGTERM, SIGINT]) {
        Ok(mut signals) => {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                if signals.forever().next().is_some() {
                    shutdown.trigger();
                }
            });
        }
        Err(e) => return fail(&e),
    }

    match chalkak_server::run(&settings, shutdown) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&*e),
    }
}

fn fail(error: &dyn std::error::Error) -> ExitCode {
    eprintln!("chalkak-server: 시작하지 못했습니다: {error}");
    ExitCode::FAILURE
}
