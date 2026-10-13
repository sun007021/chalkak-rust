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

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::log::fields;
    use chalkak_core::{Env, FixedClock, request_id};

    fn dev_settings(log_file: &Path) -> Settings {
        let file = log_file.to_str().unwrap().to_owned();
        let mut pairs = vec![
            ("ACCESS_TOKEN_SECRET", "0011"),
            ("ACCESS_TOKEN_EXPIRATION", "15m"),
            ("SPRING_PROFILES_ACTIVE", "dev"),
            ("ADMIN_CORS_ALLOWED_ORIGIN", "https://admin.example.com"),
            ("S3_PREFIX", "chalkak"),
            ("DB_HOST", "h"),
            ("DB_PORT", "5432"),
            ("DB_NAME", "n"),
            ("DB_USERNAME", "u"),
            ("DB_PASSWORD", "p"),
            ("CLOUDFRONT_BASE_URL", "https://cdn.example.com"),
            ("ADMIN_USERNAME", "operator"),
            ("ADMIN_PASSWORD_HASH", "h"),
            ("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "false"),
            ("CHALKAK_LOGGING_FILE", file.as_str()),
        ];
        pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
        Settings::from_env(&Env::from_pairs(pairs)).unwrap()
    }

    // JsonFileLogTest#initialize_devProfile_writesJsonLineToConfiguredFile
    #[test]
    fn initialize_dev_profile_writes_json_line_to_configured_file() {
        let dir = std::env::temp_dir().join(format!("chalkak-json-log-{}", uuid::Uuid::new_v4()));
        let log_file = dir.join("logs").join("application.log");
        let clock: Arc<dyn Clock> =
            Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        let logger = build(&dev_settings(&log_file), clock);
        {
            let _guard = request_id::enter("request-id-1");
            logger
                .info("chalkak.access")
                .field(fields::TYPE, fields::TYPE_ACCESS)
                .field(fields::ROUTE, "/api/v1/posts/{postId}")
                .field(fields::STATUS, 200)
                .log("access");
        }

        let text = std::fs::read_to_string(&log_file).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        let json: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert!(json.get("@timestamp").is_some());
        assert_eq!(json["level"], "INFO");
        assert_eq!(json["message"], "access");
        assert_eq!(json["requestId"], "request-id-1");
        assert_eq!(json["type"], "access");
        assert_eq!(json["route"], "/api/v1/posts/{postId}");
        assert_eq!(json["status"], 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
