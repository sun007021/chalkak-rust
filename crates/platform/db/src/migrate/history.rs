//! `flyway_schema_history` 테이블 읽기·쓰기. DDL은 Flyway PostgreSQL 지원이 만드는 것과 같다.

use postgres::{Client, GenericClient};

use super::error::MigrateError;
use super::plan::HistoryRow;
use super::resolve::Migration;

const TABLE: &str = "flyway_schema_history";

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn table(schema: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(TABLE))
}

pub fn exists(client: &mut Client, schema: &str) -> Result<bool, MigrateError> {
    let row = client.query_one("select to_regclass($1) is not null", &[&table(schema)])?;
    Ok(row.get(0))
}

/// 이력 테이블이 없을 때 스키마가 비어 있는지 본다(Flyway `allEmpty`와 같은 취지).
pub fn schema_has_objects(client: &mut Client, schema: &str) -> Result<bool, MigrateError> {
    let row = client.query_one(
        "select exists (
             select 1 from pg_class c join pg_namespace n on n.oid = c.relnamespace
              where n.nspname = $1 and c.relkind in ('r','v','m','S','f','p'))
          or exists (
             select 1 from pg_proc p join pg_namespace n on n.oid = p.pronamespace
              where n.nspname = $1
                and not exists (select 1 from pg_depend d where d.objid = p.oid and d.deptype = 'e'))
          or exists (
             select 1 from pg_type t join pg_namespace n on n.oid = t.typnamespace
              where n.nspname = $1 and t.typtype in ('e','d','r','m')
                and not exists (select 1 from pg_depend d where d.objid = t.oid and d.deptype = 'e'))",
        &[&schema],
    )?;
    Ok(row.get(0))
}

/// Flyway가 만드는 DDL 그대로(열 순서·타입·제약 이름·인덱스 이름 포함).
pub fn create(client: &mut Client, schema: &str) -> Result<(), MigrateError> {
    let t = table(schema);
    let mut tx = client.transaction()?;
    tx.batch_execute(&format!(
        "CREATE TABLE {t} (
            \"installed_rank\" INT NOT NULL,
            \"version\" VARCHAR(50),
            \"description\" VARCHAR(200) NOT NULL,
            \"type\" VARCHAR(20) NOT NULL,
            \"script\" VARCHAR(1000) NOT NULL,
            \"checksum\" INT,
            \"installed_by\" VARCHAR(100) NOT NULL,
            \"installed_on\" TIMESTAMP NOT NULL DEFAULT now(),
            \"execution_time\" INT NOT NULL,
            \"success\" BOOL NOT NULL
        );
        ALTER TABLE {t} ADD CONSTRAINT \"{TABLE}_pk\" PRIMARY KEY (\"installed_rank\");
        CREATE INDEX \"{TABLE}_s_idx\" ON {t} (\"success\");"
    ))?;
    tx.commit()?;
    Ok(())
}

pub fn read(client: &mut Client, schema: &str) -> Result<Vec<HistoryRow>, MigrateError> {
    let rows = client.query(
        &format!(
            "SELECT installed_rank, version, description, type, script, checksum,
                    installed_by, execution_time, success
               FROM {} ORDER BY installed_rank",
            table(schema)
        ),
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| HistoryRow {
            installed_rank: r.get(0),
            version: r.get(1),
            description: r.get(2),
            kind: r.get(3),
            script: r.get(4),
            checksum: r.get(5),
            installed_by: r.get(6),
            execution_time: r.get(7),
            success: r.get(8),
        })
        .collect())
}

/// 적용한 스크립트를 기록한다. 스크립트와 같은 트랜잭션 안에서 호출한다.
pub fn record(
    tx: &mut impl GenericClient,
    schema: &str,
    migration: &Migration,
    installed_by: &str,
    execution_time_ms: i32,
) -> Result<(), MigrateError> {
    let t = table(schema);
    let version = migration.version.to_string();
    tx.execute(
        &format!(
            "INSERT INTO {t} (installed_rank, version, description, type, script, checksum,
                              installed_by, execution_time, success)
             VALUES ((SELECT COALESCE(MAX(installed_rank), 0) + 1 FROM {t}),
                     $1, $2, 'SQL', $3, $4, $5, $6, TRUE)"
        ),
        &[
            &version,
            &migration.description,
            &migration.script,
            &migration.checksum,
            &installed_by,
            &execution_time_ms,
        ],
    )?;
    Ok(())
}
