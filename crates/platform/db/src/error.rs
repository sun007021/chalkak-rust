use std::fmt;
use std::time::Duration;

#[derive(Debug)]
pub enum DbError {
    Postgres(postgres::Error),
    /// 풀에서 `connection_timeout` 안에 연결을 빌리지 못했다(Hikari `SQLTransientConnectionException`).
    PoolTimeout {
        waited: Duration,
    },
    PoolClosed,
    Config(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Postgres(e) => write!(f, "postgres error: {e}"),
            Self::PoolTimeout { waited } => {
                write!(
                    f,
                    "connection is not available, request timed out after {waited:?}"
                )
            }
            Self::PoolClosed => f.write_str("connection pool is closed"),
            Self::Config(msg) => write!(f, "invalid database config: {msg}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Postgres(e) => Some(e),
            _ => None,
        }
    }
}

impl From<postgres::Error> for DbError {
    fn from(e: postgres::Error) -> Self {
        Self::Postgres(e)
    }
}
