//! Flyway 호환 마이그레이터 통합 테스트(DB 필요). 근거는 원본 설정
//! `spring.flyway.*`(validate-on-migrate, out-of-order: false, baseline-on-migrate: false)와
//! Flyway의 `flyway_schema_history` 스키마다. Java 교차 검증은 `CHALKAK_TEST_JAVA_DB_URL`
//! (pg-java-a, 읽기 전용 SELECT만 사용)이 있을 때만 실행한다.

mod common;

use std::path::{Path, PathBuf};
use std::thread;

use chalkak_db::migrate::{HistoryRow, MigrateError, Migrator};
use common::{ENV_JAVA_URL, TestDb, migrations_dir};
use postgres::{Client, NoTls};

fn scratch_dir(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chalkak-migrate-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, sql) in files {
        std::fs::write(dir.join(name), sql).unwrap();
    }
    dir
}

fn history(client: &mut Client) -> Vec<(i32, String, String)> {
    client
        .query(
            "select installed_rank, version, script from flyway_schema_history order by 1",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

fn table_exists(client: &mut Client, name: &str) -> bool {
    client
        .query_one("select to_regclass($1) is not null", &[&name])
        .unwrap()
        .get(0)
}

/// 테스트가 직접 쓰는 읽기 전용 Java 이력 조회.
fn java_history() -> Option<Vec<HistoryRow>> {
    let Ok(url) = std::env::var(ENV_JAVA_URL) else {
        eprintln!("skipped: {ENV_JAVA_URL} is not set");
        return None;
    };
    let mut java = url
        .parse::<postgres::Config>()
        .unwrap()
        .connect(NoTls)
        .unwrap();
    let rows = java
        .query(
            "select installed_rank, version, description, type, script, checksum,
                    installed_by, execution_time, success
               from flyway_schema_history order by installed_rank",
            &[],
        )
        .unwrap();
    Some(
        rows.iter()
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
            .collect(),
    )
}

fn rust_history(client: &mut Client) -> Vec<HistoryRow> {
    client
        .query(
            "select installed_rank, version, description, type, script, checksum,
                    installed_by, execution_time, success
               from flyway_schema_history order by installed_rank",
            &[],
        )
        .unwrap()
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
        .collect()
}

#[test]
fn applies_all_repository_migrations_to_an_empty_database() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let report = Migrator::from_dir(&migrations_dir())
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    assert_eq!(report.applied.len(), 28);
    let rows = rust_history(&mut client);
    assert_eq!(rows.len(), 28);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.installed_rank, i as i32 + 1);
        assert_eq!(row.kind, "SQL");
        assert!(row.success);
        assert_eq!(row.installed_by, "chalkak");
        assert!(row.execution_time >= 0);
    }
    assert_eq!(rows[0].version.as_deref(), Some("202608191752"));
    assert_eq!(rows[0].description, "create initial schema");
    assert_eq!(rows[0].checksum, Some(-62292960));
    assert_eq!(rows[27].checksum, Some(-1538072671));
    assert!(table_exists(&mut client, "notifications"));
}

#[test]
fn second_run_is_a_no_op() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let migrator = Migrator::from_dir(&migrations_dir()).unwrap();
    migrator.migrate(&mut client).unwrap();
    let before = rust_history(&mut client);
    let report = migrator.migrate(&mut client).unwrap();
    assert!(report.applied.is_empty());
    assert_eq!(rust_history(&mut client), before);
    migrator.validate(&mut client).unwrap();
}

/// 근거: Flyway PostgreSQL이 만드는 이력 테이블 DDL(열 순서·타입·기본값·PK·success 인덱스).
#[test]
fn history_table_has_the_flyway_layout() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    Migrator::new(Vec::new())
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    let cols: Vec<String> = client
        .query(
            "select column_name || ' ' || data_type || coalesce('(' || character_maximum_length || ')', '')
                    || ' ' || is_nullable || ' ' || coalesce(column_default, '')
               from information_schema.columns
              where table_name = 'flyway_schema_history' order by ordinal_position",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        cols,
        [
            "installed_rank integer NO ",
            "version character varying(50) YES ",
            "description character varying(200) NO ",
            "type character varying(20) NO ",
            "script character varying(1000) NO ",
            "checksum integer YES ",
            "installed_by character varying(100) NO ",
            "installed_on timestamp without time zone NO now()",
            "execution_time integer NO ",
            "success boolean NO ",
        ]
    );
    let indexes: Vec<String> = client
        .query(
            "select indexdef from pg_indexes where tablename = 'flyway_schema_history' order by indexname",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        indexes,
        [
            "CREATE UNIQUE INDEX flyway_schema_history_pk ON public.flyway_schema_history USING btree (installed_rank)",
            "CREATE INDEX flyway_schema_history_s_idx ON public.flyway_schema_history USING btree (success)",
        ]
    );
}

#[test]
fn applies_only_new_files_after_a_partial_run() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let first = scratch_dir("partial1", &[("V1__a.sql", "create table a (id int);")]);
    Migrator::from_dir(&first)
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    let both = scratch_dir(
        "partial2",
        &[
            ("V1__a.sql", "create table a (id int);"),
            (
                "V2__b.sql",
                "create table b (id int);\ninsert into b values (1);\n",
            ),
        ],
    );
    let report = Migrator::from_dir(&both)
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.applied[0].script, "V2__b.sql");
    assert_eq!(
        history(&mut client),
        [
            (1, "1".to_string(), "V1__a.sql".to_string()),
            (2, "2".to_string(), "V2__b.sql".to_string())
        ]
    );
}

/// 근거: `validate-on-migrate: true` - 적용된 스크립트가 바뀌면 아무것도 적용하지 않고 실패한다.
#[test]
fn modified_applied_script_fails_before_applying_anything() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let v1 = scratch_dir("tamper1", &[("V1__a.sql", "create table a (id int);")]);
    Migrator::from_dir(&v1)
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    let changed = scratch_dir(
        "tamper2",
        &[
            ("V1__a.sql", "create table a (id bigint);"),
            ("V2__b.sql", "create table b (id int);"),
        ],
    );
    let migrator = Migrator::from_dir(&changed).unwrap();
    assert!(matches!(
        migrator.migrate(&mut client),
        Err(MigrateError::ChecksumMismatch { .. })
    ));
    assert!(matches!(
        migrator.validate(&mut client),
        Err(MigrateError::ChecksumMismatch { .. })
    ));
    assert!(!table_exists(&mut client, "b"));
}

#[test]
fn renamed_description_of_applied_script_fails() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let v1 = scratch_dir("desc1", &[("V1__a.sql", "create table a (id int);")]);
    Migrator::from_dir(&v1)
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    let renamed = scratch_dir(
        "desc2",
        &[("V1__other_name.sql", "create table a (id int);")],
    );
    assert!(matches!(
        Migrator::from_dir(&renamed).unwrap().migrate(&mut client),
        Err(MigrateError::DescriptionMismatch { .. })
    ));
}

/// 근거: `out-of-order: false`.
#[test]
fn lower_version_added_after_a_higher_one_is_rejected() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let v1_v3 = scratch_dir(
        "ooo1",
        &[
            ("V1__a.sql", "create table a (id int);"),
            ("V3__c.sql", "create table c (id int);"),
        ],
    );
    Migrator::from_dir(&v1_v3)
        .unwrap()
        .migrate(&mut client)
        .unwrap();
    let with_v2 = scratch_dir(
        "ooo2",
        &[
            ("V1__a.sql", "create table a (id int);"),
            ("V2__b.sql", "create table b (id int);"),
            ("V3__c.sql", "create table c (id int);"),
        ],
    );
    assert!(matches!(
        Migrator::from_dir(&with_v2).unwrap().migrate(&mut client),
        Err(MigrateError::OutOfOrder { .. })
    ));
    assert!(!table_exists(&mut client, "b"));
}

/// 근거: Flyway PostgreSQL은 트랜잭션 DDL - 실패한 스크립트는 변경도 이력도 남기지 않는다.
#[test]
fn failing_script_rolls_back_its_changes_and_history_row() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let dir = scratch_dir(
        "fail",
        &[
            ("V1__ok.sql", "create table ok (id int);"),
            ("V2__bad.sql", "create table half (id int);\nselect 1 / 0;"),
            ("V3__never.sql", "create table never (id int);"),
        ],
    );
    match Migrator::from_dir(&dir).unwrap().migrate(&mut client) {
        Err(MigrateError::ScriptFailed { script, .. }) => assert_eq!(script, "V2__bad.sql"),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(table_exists(&mut client, "ok"));
    assert!(!table_exists(&mut client, "half"));
    assert!(!table_exists(&mut client, "never"));
    assert_eq!(history(&mut client).len(), 1);
}

/// 근거: `baseline-on-migrate: false`.
#[test]
fn non_empty_schema_without_history_is_rejected() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    client
        .batch_execute("create table legacy (id int)")
        .unwrap();
    let dir = scratch_dir("nonempty", &[("V1__a.sql", "create table a (id int);")]);
    assert!(matches!(
        Migrator::from_dir(&dir).unwrap().migrate(&mut client),
        Err(MigrateError::NonEmptySchema { .. })
    ));
    assert!(!table_exists(&mut client, "flyway_schema_history"));
}

#[test]
fn concurrent_migrators_apply_each_script_once() {
    let Some(db) = TestDb::create() else { return };
    let dir: &Path = &scratch_dir(
        "concurrent",
        &[
            ("V1__a.sql", "create table a (id int);"),
            ("V2__b.sql", "create table b (id int);"),
        ],
    );
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let mut client = db.client();
            let migrator = Migrator::from_dir(dir).unwrap();
            thread::spawn(move || migrator.migrate(&mut client).unwrap().applied.len())
        })
        .collect();
    let applied: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(applied, 2);
    assert_eq!(history(&mut db.client()).len(), 2);
}

/// 완료 조건: Java(Flyway)가 만든 이력을 Rust 검증이 통과한다. pg-java-a의 이력을 복사해
/// (읽기만 한다) 새 DB에 놓고, 같은 스키마 위에서 validate·migrate가 통과하고 아무것도
/// 적용하지 않는지 본다.
#[test]
fn rust_validation_accepts_the_history_java_flyway_wrote() {
    let Some(java_rows) = java_history() else {
        return;
    };
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let migrator = Migrator::from_dir(&migrations_dir()).unwrap();
    migrator.migrate(&mut client).unwrap();

    // 반대 방향: Rust가 기록한 이력은 Java 이력과 (설치 시각·소요 시간을 빼고) 같다.
    let strip = |rows: &[HistoryRow]| -> Vec<HistoryRow> {
        rows.iter()
            .map(|r| HistoryRow {
                execution_time: 0,
                ..r.clone()
            })
            .collect()
    };
    assert_eq!(strip(&rust_history(&mut client)), strip(&java_rows));

    // Rust 이력을 Java 이력 복사본으로 바꾼 뒤 검증한다.
    let mut tx = client.transaction().unwrap();
    tx.batch_execute("delete from flyway_schema_history")
        .unwrap();
    for r in &java_rows {
        tx.execute(
            "insert into flyway_schema_history
               (installed_rank, version, description, type, script, checksum, installed_by, execution_time, success)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &r.installed_rank,
                &r.version,
                &r.description,
                &r.kind,
                &r.script,
                &r.checksum,
                &r.installed_by,
                &r.execution_time,
                &r.success,
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    migrator.validate(&mut client).unwrap();
    assert!(migrator.migrate(&mut client).unwrap().applied.is_empty());
}
