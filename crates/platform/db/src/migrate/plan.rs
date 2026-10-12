//! 이력과 파일을 대조해 적용할 목록을 정한다(DB 없이 검증 가능한 순수 로직).
//!
//! 원본 설정: `validate-on-migrate: true`, `out-of-order: false`, `baseline-on-migrate: false`.
//! Flyway 기본 `ignoreMigrationPatterns=*:future` 도 따른다(이력에만 있고 파일 중 가장 높은
//! 버전보다 큰 행은 롤링 배포를 위해 무시한다).

use super::error::MigrateError;
use super::resolve::{Migration, Version};

/// `flyway_schema_history` 한 행.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub installed_rank: i32,
    pub version: Option<String>,
    pub description: String,
    pub kind: String,
    pub script: String,
    pub checksum: Option<i32>,
    pub installed_by: String,
    pub execution_time: i32,
    pub success: bool,
}

/// `migrations`는 버전 오름차순이어야 한다. 검증에 실패하면 오류, 통과하면 적용할 마이그레이션.
pub fn plan<'a>(
    history: &[HistoryRow],
    migrations: &'a [Migration],
) -> Result<Vec<&'a Migration>, MigrateError> {
    let newest_resolved = migrations.last().map(|m| &m.version);
    let mut applied: Vec<Version> = Vec::new();
    let mut baseline: Option<Version> = None;

    for row in history {
        let version = row.version.as_deref().and_then(Version::parse);
        if !row.success {
            return Err(MigrateError::FailedMigration {
                version: row.version.clone().unwrap_or_default(),
                script: row.script.clone(),
            });
        }
        // BASELINE·SCHEMA 같은 SQL이 아닌 행은 파일과 대조하지 않는다.
        if row.kind != "SQL" {
            if row.kind == "BASELINE" {
                baseline = version.clone().into_iter().chain(baseline).max();
            }
            if let Some(v) = version {
                applied.push(v);
            }
            continue;
        }
        let Some(version) = version else { continue };
        match migrations.iter().find(|m| m.version == version) {
            Some(m) => {
                if m.checksum_differs(row.checksum) {
                    return Err(MigrateError::ChecksumMismatch {
                        version: version.to_string(),
                        script: row.script.clone(),
                        applied: row.checksum,
                        resolved: m.checksum,
                    });
                }
                if m.description != row.description {
                    return Err(MigrateError::DescriptionMismatch {
                        version: version.to_string(),
                        applied: row.description.clone(),
                        resolved: m.description.clone(),
                    });
                }
            }
            None => {
                let future = newest_resolved.is_none_or(|newest| version > *newest);
                if !future {
                    return Err(MigrateError::MissingMigration {
                        version: version.to_string(),
                        script: row.script.clone(),
                    });
                }
            }
        }
        applied.push(version);
    }

    let latest_applied = applied.iter().max();
    let mut pending = Vec::new();
    for m in migrations {
        if applied.contains(&m.version) {
            continue;
        }
        if baseline.as_ref().is_some_and(|b| m.version <= *b) {
            continue;
        }
        if let Some(latest) = latest_applied
            && m.version < *latest
        {
            return Err(MigrateError::OutOfOrder {
                version: m.version.to_string(),
                script: m.script.clone(),
                latest_applied: latest.to_string(),
            });
        }
        pending.push(m);
    }
    Ok(pending)
}

impl Migration {
    fn checksum_differs(&self, applied: Option<i32>) -> bool {
        applied != Some(self.checksum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mig(name: &str, sql: &str) -> Migration {
        Migration::from_source(name, sql.as_bytes()).unwrap()
    }

    fn row(m: &Migration, rank: i32) -> HistoryRow {
        HistoryRow {
            installed_rank: rank,
            version: Some(m.version.to_string()),
            description: m.description.clone(),
            kind: "SQL".to_string(),
            script: m.script.clone(),
            checksum: Some(m.checksum),
            installed_by: "chalkak".to_string(),
            execution_time: 1,
            success: true,
        }
    }

    fn three() -> Vec<Migration> {
        vec![
            mig("V1__a.sql", "select 1;"),
            mig("V2__b.sql", "select 2;"),
            mig("V3__c.sql", "select 3;"),
        ]
    }

    #[test]
    fn empty_history_applies_everything_in_version_order() {
        let ms = three();
        let pending = plan(&[], &ms).unwrap();
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].script, "V1__a.sql");
    }

    #[test]
    fn partially_applied_history_applies_only_the_rest() {
        let ms = three();
        let history = [row(&ms[0], 1), row(&ms[1], 2)];
        let pending = plan(&history, &ms).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].script, "V3__c.sql");
    }

    #[test]
    fn fully_applied_history_is_a_no_op() {
        let ms = three();
        let history = [row(&ms[0], 1), row(&ms[1], 2), row(&ms[2], 3)];
        assert!(plan(&history, &ms).unwrap().is_empty());
    }

    /// 근거: `validate-on-migrate: true` - 적용된 스크립트가 바뀌면 체크섬이 달라 실패한다.
    #[test]
    fn changed_applied_script_fails_validation() {
        let ms = three();
        let mut history = [row(&ms[0], 1)];
        history[0].checksum = Some(ms[0].checksum.wrapping_add(1));
        assert!(matches!(
            plan(&history, &ms),
            Err(MigrateError::ChecksumMismatch { .. })
        ));
        history[0].checksum = None;
        assert!(matches!(
            plan(&history, &ms),
            Err(MigrateError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn renamed_description_fails_validation() {
        let ms = three();
        let mut history = [row(&ms[0], 1)];
        history[0].description = "something else".to_string();
        assert!(matches!(
            plan(&history, &ms),
            Err(MigrateError::DescriptionMismatch { .. })
        ));
    }

    /// 근거: `out-of-order: false` - 이미 V3가 적용됐다면 새로 생긴 V2는 적용하지 않고 실패한다.
    #[test]
    fn older_pending_migration_is_rejected_when_out_of_order_is_disabled() {
        let ms = three();
        let history = [row(&ms[0], 1), row(&ms[2], 2)];
        match plan(&history, &ms) {
            Err(MigrateError::OutOfOrder {
                version,
                latest_applied,
                ..
            }) => {
                assert_eq!(version, "2");
                assert_eq!(latest_applied, "3");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn applied_but_deleted_file_fails_unless_it_is_newer_than_every_file() {
        let ms = three();
        let removed = mig("V2__b.sql", "select 2;");
        let history = [row(&ms[0], 1), row(&removed, 2), row(&ms[2], 3)];
        let without_v2 = [ms[0].clone(), ms[2].clone()];
        assert!(matches!(
            plan(&history, &without_v2),
            Err(MigrateError::MissingMigration { .. })
        ));
        // Flyway 기본 *:future - 파일에 없는 더 높은 버전은 무시(새 버전이 먼저 배포된 경우).
        let future = mig("V9__future.sql", "select 9;");
        let history = [row(&ms[0], 1), row(&future, 2)];
        assert!(plan(&history, &ms[..1]).unwrap().is_empty());
    }

    #[test]
    fn failed_row_blocks_migration() {
        let ms = three();
        let mut history = [row(&ms[0], 1)];
        history[0].success = false;
        assert!(matches!(
            plan(&history, &ms),
            Err(MigrateError::FailedMigration { .. })
        ));
    }

    #[test]
    fn baseline_row_skips_older_files_and_is_not_compared() {
        let ms = three();
        let baseline = HistoryRow {
            installed_rank: 1,
            version: Some("2".to_string()),
            description: "<< Flyway Baseline >>".to_string(),
            kind: "BASELINE".to_string(),
            script: "<< Flyway Baseline >>".to_string(),
            checksum: None,
            installed_by: "x".to_string(),
            execution_time: 0,
            success: true,
        };
        let pending = plan(&[baseline], &ms).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].script, "V3__c.sql");
    }
}
