//! Flyway 호환 마이그레이터.
//!
//! 원본(Spring Boot + Flyway) 설정을 그대로 따른다(`application*.yml`의 `spring.flyway.*`).
//!
//! | 설정 | 값 | 이 모듈의 동작 |
//! | --- | --- | --- |
//! | `enabled` | true | 앱 시작 시 호출하는 쪽이 [`Migrator::migrate`]를 부른다 |
//! | `default-schema` | public | [`Migrator::schema`] 기본값 `public` |
//! | `validate-on-migrate` | true | 적용 전에 항상 이력과 파일을 대조한다 |
//! | `validate-migration-naming` | true | 규칙에 맞지 않는 `.sql` 이름은 실패 |
//! | `out-of-order` | false | 더 높은 버전이 적용된 뒤 생긴 낮은 버전은 실패 |
//! | `baseline-on-migrate` | false | 이력 없는 비어 있지 않은 스키마는 실패 |
//! | `clean-disabled` | true | clean 기능 자체가 없다 |
//!
//! 이력 테이블(`flyway_schema_history`)의 열·타입·인덱스와 CRC32 체크섬이 Flyway와 같으므로
//! Java가 만든 이력을 이 모듈이 검증할 수 있고, 반대도 가능하다.

pub mod checksum;
pub mod error;
mod history;
pub mod plan;
pub mod resolve;

use std::path::Path;
use std::time::Instant;

use postgres::Client;

pub use error::MigrateError;
pub use plan::HistoryRow;
pub use resolve::{Migration, Version, load_dir};

/// 같은 DB를 동시에 마이그레이션하지 않도록 잡는 어드바이저리 락 키(ASCII "chalkak\0").
const ADVISORY_LOCK_KEY: i64 = 0x6368_616c_6b61_6b00;

/// 이번 실행에서 적용한 마이그레이션 하나.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub version: String,
    pub description: String,
    pub script: String,
    pub execution_time_ms: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrateReport {
    pub applied: Vec<Applied>,
}

pub struct Migrator {
    migrations: Vec<Migration>,
    schema: String,
}

impl Migrator {
    /// 버전 순으로 정렬하고 중복 버전을 거른다.
    pub fn new(migrations: Vec<Migration>) -> Result<Migrator, MigrateError> {
        Ok(Migrator {
            migrations: resolve::sort_and_check(migrations)?,
            schema: "public".to_string(),
        })
    }

    /// 디렉터리(예: 저장소 루트 `migrations/`)의 `V<버전>__<설명>.sql`을 읽는다.
    pub fn from_dir(dir: &Path) -> Result<Migrator, MigrateError> {
        Migrator::new(load_dir(dir)?)
    }

    /// 이력 테이블과 스크립트가 쓰는 스키마(`spring.flyway.default-schema`).
    pub fn schema(mut self, schema: &str) -> Migrator {
        self.schema = schema.to_string();
        self
    }

    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// 이력과 파일을 대조만 한다. 아무것도 쓰지 않는다. 이력이 없으면 통과한다.
    pub fn validate(&self, client: &mut Client) -> Result<(), MigrateError> {
        let rows = if history::exists(client, &self.schema)? {
            history::read(client, &self.schema)?
        } else {
            Vec::new()
        };
        plan::plan(&rows, &self.migrations).map(|_| ())
    }

    /// 검증 후 적용되지 않은 마이그레이션을 버전 순으로 하나씩 적용한다.
    ///
    /// 각 스크립트는 자기 트랜잭션 안에서 실행되고 이력 행도 같은 트랜잭션에서 기록되므로
    /// 실패하면 그 스크립트의 변경과 이력이 함께 롤백된다. 이미 적용된 앞쪽 스크립트는 남는다.
    pub fn migrate(&self, client: &mut Client) -> Result<MigrateReport, MigrateError> {
        client.execute("select pg_advisory_lock($1)", &[&ADVISORY_LOCK_KEY])?;
        let result = self.migrate_locked(client);
        let unlock = client.execute("select pg_advisory_unlock($1)", &[&ADVISORY_LOCK_KEY]);
        let report = result?;
        unlock?;
        Ok(report)
    }

    fn migrate_locked(&self, client: &mut Client) -> Result<MigrateReport, MigrateError> {
        if !history::exists(client, &self.schema)? {
            if history::schema_has_objects(client, &self.schema)? {
                return Err(MigrateError::NonEmptySchema {
                    schema: self.schema.clone(),
                });
            }
            history::create(client, &self.schema)?;
        }
        let rows = history::read(client, &self.schema)?;
        let pending = plan::plan(&rows, &self.migrations)?;
        let installed_by: String = client.query_one("select current_user::text", &[])?.get(0);

        let mut report = MigrateReport::default();
        for migration in pending {
            let started = Instant::now();
            let mut tx = client.transaction()?;
            tx.execute(
                "select set_config('search_path', $1 || ', ' || current_setting('search_path'), true)",
                &[&history::quote_ident(&self.schema)],
            )?;
            tx.batch_execute(&migration.sql)
                .map_err(|source| MigrateError::ScriptFailed {
                    script: migration.script.clone(),
                    source,
                })?;
            let millis = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
            history::record(&mut tx, &self.schema, migration, &installed_by, millis)?;
            tx.commit()?;
            report.applied.push(Applied {
                version: migration.version.to_string(),
                description: migration.description.clone(),
                script: migration.script.clone(),
                execution_time_ms: millis,
            });
        }
        Ok(report)
    }
}

/// `dir`의 마이그레이션을 `public` 스키마에 적용한다(앱 시작 시 호출).
pub fn migrate_dir(client: &mut Client, dir: &Path) -> Result<MigrateReport, MigrateError> {
    Migrator::from_dir(dir)?.migrate(client)
}
