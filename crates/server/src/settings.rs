//! 설정. 원본 `application*.yml`이 읽는 환경변수 이름을 그대로 쓴다.
//!
//! 프로필은 `SPRING_PROFILES_ACTIVE`(기본 `local`)로 고른다. 프로필에 따라 달라지는 것은 원본과 같다.
//! - 허용 CORS Origin: local은 `http://localhost:3000`, dev는 거기에 `ADMIN_CORS_ALLOWED_ORIGIN`을 더하고, prod는
//!   `ADMIN_CORS_ALLOWED_ORIGIN`만.
//! - 개발 관리자 우회(`ADMIN_DEVELOPMENT_BYPASS_ENABLED`): local에서만 읽고(기본 켜짐) dev·prod에서는 항상 꺼짐.
//! - DB 접속: dev·prod는 `DB_HOST`·`DB_PORT`·`DB_NAME`·`DB_USERNAME`·`DB_PASSWORD`. local은 같은 이름을 우선하고, 없으면
//!   원본 local 프로필의 `POSTGRES_*` 기본값.
//! - JSON 파일 로그: dev·prod만.

use std::path::PathBuf;
use std::time::Duration;

use chalkak_app::config::AppSettings;
use chalkak_app::notification::slack::settings::AdminNotificationSettings;
use chalkak_app::post::PostSettings;
use chalkak_core::config::{ConfigError, Env};
use chalkak_db::DbConfig;

/// 액세스 토큰의 발급자·대상. 원본 `application.yml`의 `chalkak.auth.access-token` 고정값이다.
pub const ACCESS_TOKEN_ISSUER: &str = "chalkak-backend";
pub const ACCESS_TOKEN_AUDIENCE: &str = "chalkak-access";

/// Tomcat 기본 최대 스레드 수.
pub const DEFAULT_WORKERS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Local,
    Dev,
    Prod,
}

impl Profile {
    /// 쉼표로 여러 프로필이 올 수 있다. prod, dev 순으로 우선하고 아니면 local이다.
    pub fn parse(raw: &str) -> Profile {
        let names: Vec<&str> = raw.split(',').map(str::trim).collect();
        if names.contains(&"prod") {
            Profile::Prod
        } else if names.contains(&"dev") {
            Profile::Dev
        } else {
            Profile::Local
        }
    }

    /// 프로필 이름(`application-{name}.yml`).
    pub fn name(self) -> &'static str {
        match self {
            Profile::Local => "local",
            Profile::Dev => "dev",
            Profile::Prod => "prod",
        }
    }

    /// JSON 파일 로그를 남기는 프로필(`logback-spring.xml`의 `dev | prod`).
    pub fn writes_json_log_file(self) -> bool {
        matches!(self, Profile::Dev | Profile::Prod)
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub profile: Profile,
    pub port: u16,
    pub workers: usize,
    pub db: DbConfig,
    pub db_pool_size: usize,
    pub migrations_dir: PathBuf,
    /// 액세스 토큰 HS256 키(16진수 문자열을 푼 바이트).
    pub access_token_key: Vec<u8>,
    pub access_token_expiration: Duration,
    pub admin_development_bypass: bool,
    pub admin_cors_allowed_origins: Vec<String>,
    pub log_file: PathBuf,
    /// auth·user 도메인 설정(OIDC, Apple, 이미지 저장소, 콜백).
    pub app: AppSettings,
    pub post: PostSettings,
    /// 관리자 Slack 알림. 전달이 꺼져 있으면 `None`.
    pub admin_notification: Option<AdminNotificationSettings>,
}

impl Settings {
    pub fn from_env(env: &Env) -> Result<Settings, ConfigError> {
        let profile = Profile::parse(env.string_or("SPRING_PROFILES_ACTIVE", "local"));
        let port: u16 = env.parsed_or("SERVER_PORT", 8080)?;
        let db = db_config(env, profile)?;

        let secret = env.required("ACCESS_TOKEN_SECRET")?;
        let access_token_key =
            chalkak_jwt::decode_hex(secret).map_err(|_| ConfigError::Invalid {
                key: "ACCESS_TOKEN_SECRET".to_owned(),
                reason: "16진수 문자열이어야 합니다".to_owned(),
            })?;
        let access_token_expiration = env.required_duration("ACCESS_TOKEN_EXPIRATION")?;

        let admin_development_bypass = match profile {
            Profile::Local => env.bool_or("ADMIN_DEVELOPMENT_BYPASS_ENABLED", true)?,
            Profile::Dev | Profile::Prod => false,
        };

        let post = PostSettings::from_env(env, profile.name())?;
        let app = AppSettings::from_env(
            env,
            &post,
            access_token_key.clone(),
            access_token_expiration,
        )?;
        let admin_notification =
            AdminNotificationSettings::from_env(env, profile == Profile::Local)?;

        Ok(Settings {
            profile,
            port,
            workers: DEFAULT_WORKERS,
            db,
            db_pool_size: DbConfig::DEFAULT_POOL_SIZE,
            migrations_dir: PathBuf::from(env.string_or("CHALKAK_MIGRATIONS_DIR", "migrations")),
            access_token_key,
            access_token_expiration,
            admin_development_bypass,
            admin_cors_allowed_origins: cors_origins(env, profile)?,
            log_file: PathBuf::from(
                env.string_or("CHALKAK_LOGGING_FILE", chalkak_core::log::DEFAULT_LOG_FILE),
            ),
            app,
            post,
            admin_notification,
        })
    }
}

fn db_config(env: &Env, profile: Profile) -> Result<DbConfig, ConfigError> {
    let deployed = profile != Profile::Local;
    let pick = |key: &str, local_key: &str, local_default: &str| -> Result<String, ConfigError> {
        if deployed {
            Ok(env.required(key)?.to_owned())
        } else {
            Ok(env
                .get(key)
                .or_else(|| env.get(local_key))
                .unwrap_or(local_default)
                .to_owned())
        }
    };
    let host = if deployed {
        env.string_or("DB_HOST", "127.0.0.1").to_owned()
    } else {
        env.string_or("DB_HOST", "localhost").to_owned()
    };
    let port: u16 = match env.get("DB_PORT") {
        Some(_) => env.parsed_or("DB_PORT", 5432)?,
        None if deployed => 5432,
        None => env.parsed_or("POSTGRES_PORT", 5432)?,
    };
    let name = pick("DB_NAME", "POSTGRES_DB", "chalkak")?;
    let user = pick("DB_USERNAME", "POSTGRES_USER", "chalkak")?;
    let password = pick("DB_PASSWORD", "POSTGRES_PASSWORD", "local-password")?;

    let mut pg = postgres::Config::new();
    pg.host(&host)
        .port(port)
        .dbname(&name)
        .user(&user)
        .password(password);
    Ok(DbConfig::new(pg))
}

/// `AdminCorsProperties`: 비어 있으면 안 되고, 공백이거나 `*`가 든 값도 안 된다.
fn cors_origins(env: &Env, profile: Profile) -> Result<Vec<String>, ConfigError> {
    const LOCAL_ORIGIN: &str = "http://localhost:3000";
    let origins = match profile {
        Profile::Local => vec![LOCAL_ORIGIN.to_owned()],
        Profile::Dev => vec![
            LOCAL_ORIGIN.to_owned(),
            env.required("ADMIN_CORS_ALLOWED_ORIGIN")?.to_owned(),
        ],
        Profile::Prod => vec![env.required("ADMIN_CORS_ALLOWED_ORIGIN")?.to_owned()],
    };
    let origins: Vec<String> = origins.iter().map(|o| o.trim().to_owned()).collect();
    if origins.iter().any(|o| o.is_empty() || o.contains('*')) {
        return Err(ConfigError::Invalid {
            key: "ADMIN_CORS_ALLOWED_ORIGIN".to_owned(),
            reason: "관리자 웹 허용 Origin은 정확한 주소여야 합니다".to_owned(),
        });
    }
    Ok(origins)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(extra: &[(&str, &str)]) -> Env {
        let mut pairs = vec![
            ("ACCESS_TOKEN_SECRET", "0011"),
            ("ACCESS_TOKEN_EXPIRATION", "15m"),
            ("S3_PREFIX", "chalkak"),
        ];
        pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
        pairs.extend_from_slice(extra);
        Env::from_pairs(pairs)
    }

    #[test]
    fn 프로필은_prod_dev_local_순으로_고른다() {
        assert_eq!(Profile::parse("local"), Profile::Local);
        assert_eq!(Profile::parse("dev"), Profile::Dev);
        assert_eq!(Profile::parse("dev,prod"), Profile::Prod);
        assert_eq!(Profile::parse("test"), Profile::Local);
    }

    // application-local.yml: 개발 관리자 우회 기본 켜짐, CORS는 localhost:3000만.
    #[test]
    fn local_기본값은_원본_local_프로필과_같다() {
        let s = Settings::from_env(&env(&[])).unwrap();
        assert_eq!(s.profile, Profile::Local);
        assert_eq!(s.port, 8080);
        assert!(s.admin_development_bypass);
        assert_eq!(s.admin_cors_allowed_origins, ["http://localhost:3000"]);
        assert_eq!(s.access_token_key, [0x00, 0x11]);
        assert_eq!(s.access_token_expiration, Duration::from_secs(900));
        assert_eq!(s.db.pg.get_ports(), [5432]);
    }

    #[test]
    fn 우회_설정은_local에서만_읽는다() {
        let local = env(&[("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false")]);
        assert!(!Settings::from_env(&local).unwrap().admin_development_bypass);

        let dev = env(&[
            ("SPRING_PROFILES_ACTIVE", "dev"),
            ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "true"),
            ("ADMIN_CORS_ALLOWED_ORIGIN", "https://admin.example.com"),
            ("S3_PREFIX", "chalkak"),
            ("DB_NAME", "chalkak"),
            ("DB_USERNAME", "u"),
            ("DB_PASSWORD", "p"),
            ("CLOUDFRONT_BASE_URL", "https://cdn.example.com"),
            ("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "false"),
        ]);
        assert!(!Settings::from_env(&dev).unwrap().admin_development_bypass);
    }

    // application-dev.yml / application-prod.yml
    #[test]
    fn dev와_prod의_cors_origin은_프로필마다_다르다() {
        let base = [
            ("ADMIN_CORS_ALLOWED_ORIGIN", "https://admin.example.com"),
            ("S3_PREFIX", "chalkak"),
            ("DB_NAME", "chalkak"),
            ("DB_USERNAME", "u"),
            ("DB_PASSWORD", "p"),
            ("CLOUDFRONT_BASE_URL", "https://cdn.example.com"),
            ("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "false"),
        ];
        let mut dev = vec![("SPRING_PROFILES_ACTIVE", "dev")];
        dev.extend_from_slice(&base);
        assert_eq!(
            Settings::from_env(&env(&dev))
                .unwrap()
                .admin_cors_allowed_origins,
            ["http://localhost:3000", "https://admin.example.com"]
        );
        let mut prod = vec![("SPRING_PROFILES_ACTIVE", "prod")];
        prod.extend_from_slice(&base);
        assert_eq!(
            Settings::from_env(&env(&prod))
                .unwrap()
                .admin_cors_allowed_origins,
            ["https://admin.example.com"]
        );
    }

    // AdminCorsProperties: 와일드카드·빈 값은 기동 실패
    #[test]
    fn cors_origin에_와일드카드가_있으면_거절한다() {
        let e = env(&[
            ("SPRING_PROFILES_ACTIVE", "prod"),
            ("ADMIN_CORS_ALLOWED_ORIGIN", "https://*.example.com"),
            ("DB_NAME", "chalkak"),
            ("DB_USERNAME", "u"),
            ("DB_PASSWORD", "p"),
        ]);
        assert!(Settings::from_env(&e).is_err());
    }

    #[test]
    fn 필수_설정이_없으면_실패한다() {
        assert!(
            Settings::from_env(&Env::from_pairs([("ACCESS_TOKEN_EXPIRATION", "15m")])).is_err()
        );
        assert!(Settings::from_env(&Env::from_pairs([("ACCESS_TOKEN_SECRET", "00")])).is_err());
        assert!(Settings::from_env(&env(&[("ACCESS_TOKEN_SECRET", "zz")])).is_err());
    }

    #[test]
    fn db_접속_정보는_db_접두_변수를_우선한다() {
        let e = env(&[
            ("DB_HOST", "127.0.0.1"),
            ("DB_PORT", "15434"),
            ("DB_NAME", "chalkak"),
            ("DB_USERNAME", "chalkak"),
            ("DB_PASSWORD", "test-password"),
            ("POSTGRES_PORT", "1"),
        ]);
        let s = Settings::from_env(&e).unwrap();
        assert_eq!(s.db.pg.get_ports(), [15434]);
        assert_eq!(s.db.pg.get_user(), Some("chalkak"));
        assert_eq!(s.db.pg.get_dbname(), Some("chalkak"));
    }
}
