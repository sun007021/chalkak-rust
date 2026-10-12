//! DB 통합 테스트 공통 도구.
//!
//! `CHALKAK_TEST_DB_URL`(예: `postgres://chalkak:test-password@localhost:15434/chalkak`)이 없으면
//! 테스트를 건너뛴다(CI에는 DB가 없다). 있으면 테스트마다 `chalkak_test_*` 데이터베이스를
//! 새로 만들고 끝나면 지운다.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};

use chalkak_db::DbConfig;
use postgres::{Client, NoTls};

pub const ENV_URL: &str = "CHALKAK_TEST_DB_URL";
/// 읽기 전용으로만 쓰는 Java(Flyway) 쪽 DB. 없으면 교차 검증 테스트를 건너뛴다.
pub const ENV_JAVA_URL: &str = "CHALKAK_TEST_JAVA_DB_URL";

pub fn migrations_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations")
}

pub struct TestDb {
    admin: Client,
    pub name: String,
    pub config: DbConfig,
}

impl TestDb {
    pub fn create() -> Option<TestDb> {
        let Ok(url) = std::env::var(ENV_URL) else {
            eprintln!("skipped: {ENV_URL} is not set");
            return None;
        };
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "chalkak_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let base: postgres::Config = url.parse().expect("CHALKAK_TEST_DB_URL");
        let mut admin = base.connect(NoTls).expect("connect admin");
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .unwrap();
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .unwrap();
        let mut pg = base;
        pg.dbname(&name);
        Some(TestDb {
            admin,
            name,
            config: DbConfig::new(pg),
        })
    }

    pub fn client(&self) -> Client {
        self.config.pg.connect(NoTls).expect("connect test db")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = self.admin.batch_execute(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ));
    }
}
