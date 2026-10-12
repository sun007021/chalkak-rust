use std::time::Duration;

use crate::error::DbError;

/// 접속 정보와 풀 동작 설정.
///
/// 원본 `application*.yml`에는 `spring.datasource.hikari.*` 설정이 없으므로 HikariCP 기본값이
/// 그대로 적용된다. 아래 기본값은 그 값이다(HikariCP 7 `HikariConfig`/`PoolBase`).
/// - `maximumPoolSize` 10, `minimumIdle`은 따로 주지 않으면 같은 값 -> 크기 고정 풀
/// - `connectionTimeout` 30초, `validationTimeout` 5초
/// - `maxLifetime` 30분, `aliveBypassWindow` 500ms
///   (`idleTimeout`은 `minimumIdle == maximumPoolSize`이면 적용되지 않는다)
#[derive(Clone, Debug)]
pub struct DbConfig {
    pub pg: postgres::Config,
    /// 빌릴 때와 새 연결을 맺을 때의 대기 한도.
    pub connection_timeout: Duration,
    /// 빌릴 때 유효성 검사(`is_valid`)의 한도.
    pub validation_timeout: Duration,
    /// 연결 수명. 넘으면 반환·대여 시점에 버리고 새로 맺는다.
    pub max_lifetime: Duration,
    /// 이 시간 안에 쓴 연결은 유효성 검사를 건너뛴다.
    pub alive_bypass_window: Duration,
}

impl DbConfig {
    /// HikariCP `maximumPoolSize` 기본값.
    pub const DEFAULT_POOL_SIZE: usize = 10;

    pub fn new(pg: postgres::Config) -> Self {
        Self {
            pg,
            connection_timeout: Duration::from_secs(30),
            validation_timeout: Duration::from_secs(5),
            max_lifetime: Duration::from_secs(30 * 60),
            alive_bypass_window: Duration::from_millis(500),
        }
    }

    /// `postgres://user:password@host:port/dbname` 또는 `key=value` 접속 문자열.
    pub fn from_url(url: &str) -> Result<Self, DbError> {
        let pg = url
            .parse::<postgres::Config>()
            .map_err(|e| DbError::Config(e.to_string()))?;
        Ok(Self::new(pg))
    }
}
