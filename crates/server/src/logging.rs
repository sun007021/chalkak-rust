//! 로거 조립. 원본 `logback-spring.xml`: 콘솔은 항상, dev·prod는 JSON 파일도 함께. 루트 수준은 INFO.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::log::{Level, Logger};

use crate::settings::Settings;

/// 같은 줄을 두 곳에 쓴다. 파일 쓰기 실패는 콘솔 출력을 막지 않는다.
struct Tee {
    console: io::Stdout,
    file: std::fs::File,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.file.write_all(buf);
        self.console.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.file.flush();
        self.console.flush()
    }
}

fn open_log_file(path: &Path) -> io::Result<std::fs::File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

pub fn build(settings: &Settings, clock: Arc<dyn Clock>) -> Arc<Logger> {
    if settings.profile.writes_json_log_file() {
        match open_log_file(&settings.log_file) {
            Ok(file) => {
                let sink = Tee {
                    console: io::stdout(),
                    file,
                };
                return Arc::new(Logger::new(Box::new(sink), Level::Info, clock));
            }
            Err(e) => {
                // 원본도 파일 로거가 뜨지 않아도 콘솔 로그로 기동한다.
                eprintln!(
                    "JSON 로그 파일을 열지 못했습니다({}): {e}",
                    settings.log_file.display()
                );
            }
        }
    }
    Arc::new(Logger::stdout(Level::Info, clock))
}
