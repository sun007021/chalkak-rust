use std::fmt;

/// 마이그레이션 실패 원인. 메시지는 Flyway의 검증 오류와 같은 사실을 담는다.
#[derive(Debug)]
pub enum MigrateError {
    Db(postgres::Error),
    Io {
        path: String,
        source: std::io::Error,
    },
    InvalidName {
        script: String,
        reason: String,
    },
    DuplicateVersion {
        version: String,
        first: String,
        second: String,
    },
    /// 적용된 마이그레이션의 체크섬이 지금 파일과 다르다.
    ChecksumMismatch {
        version: String,
        script: String,
        applied: Option<i32>,
        resolved: i32,
    },
    DescriptionMismatch {
        version: String,
        applied: String,
        resolved: String,
    },
    TypeMismatch {
        version: String,
        applied: String,
    },
    /// 이력에는 있는데 파일이 없다(미래 버전은 Flyway 기본값처럼 무시한다).
    MissingMigration {
        version: String,
        script: String,
    },
    /// 이미 더 높은 버전이 적용됐는데 낮은 버전 파일이 아직 적용되지 않았다(`out-of-order: false`).
    OutOfOrder {
        version: String,
        script: String,
        latest_applied: String,
    },
    /// 이력에 `success = false` 행이 있다.
    FailedMigration {
        version: String,
        script: String,
    },
    /// 이력 테이블 없이 비어 있지 않은 스키마(`baseline-on-migrate: false`).
    NonEmptySchema {
        schema: String,
    },
    /// 스크립트 실행 실패. 그 스크립트의 트랜잭션은 롤백됐다.
    ScriptFailed {
        script: String,
        source: postgres::Error,
    },
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(e) => write!(f, "migration db error: {e}"),
            Self::Io { path, source } => write!(f, "cannot read {path}: {source}"),
            Self::InvalidName { script, reason } => {
                write!(f, "invalid migration name {script}: {reason}")
            }
            Self::DuplicateVersion {
                version,
                first,
                second,
            } => write!(f, "duplicate version {version}: {first} and {second}"),
            Self::ChecksumMismatch {
                version,
                script,
                applied,
                resolved,
            } => write!(
                f,
                "checksum mismatch for version {version} ({script}): applied {applied:?}, resolved {resolved}"
            ),
            Self::DescriptionMismatch {
                version,
                applied,
                resolved,
            } => write!(
                f,
                "description mismatch for version {version}: applied `{applied}`, resolved `{resolved}`"
            ),
            Self::TypeMismatch { version, applied } => {
                write!(
                    f,
                    "type mismatch for version {version}: applied {applied}, resolved SQL"
                )
            }
            Self::MissingMigration { version, script } => {
                write!(
                    f,
                    "applied migration {version} ({script}) is not resolved locally"
                )
            }
            Self::OutOfOrder {
                version,
                script,
                latest_applied,
            } => write!(
                f,
                "migration {version} ({script}) is not applied but a newer version {latest_applied} is (out-of-order is disabled)"
            ),
            Self::FailedMigration { version, script } => {
                write!(
                    f,
                    "failed migration recorded in history: {version} ({script})"
                )
            }
            Self::NonEmptySchema { schema } => write!(
                f,
                "schema {schema} is not empty but has no flyway_schema_history (baseline-on-migrate is disabled)"
            ),
            Self::ScriptFailed { script, source } => {
                write!(f, "migration {script} failed (rolled back): {source}")
            }
        }
    }
}

impl std::error::Error for MigrateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) | Self::ScriptFailed { source: e, .. } => Some(e),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<postgres::Error> for MigrateError {
    fn from(e: postgres::Error) -> Self {
        Self::Db(e)
    }
}
