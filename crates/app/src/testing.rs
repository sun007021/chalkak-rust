//! DB 통합 테스트 보조(테스트에서만 쓴다).
//!
//! `CHALKAK_TEST_DB_URL`(예: `postgres://chalkak:test-password@localhost:15434/chalkak`)이 없으면 테스트를 건너뛴다
//! (CI에는 DB가 없다). 있으면 테스트마다 `chalkak_test_*` 데이터베이스를 새로 만들어 마이그레이션을 적용하고,
//! 끝나면 지운다.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use chalkak_db::migrate::Migrator;
use chalkak_db::{DbConfig, Pool};
use postgres::{Client, NoTls};

pub const ENV_URL: &str = "CHALKAK_TEST_DB_URL";

pub struct TestDb {
    admin: Client,
    pub name: String,
    pub config: DbConfig,
}

impl TestDb {
    /// 환경 변수가 없으면 `None`(테스트는 일찍 돌아간다).
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
            .expect("drop");
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .expect("create");
        let mut pg = base;
        pg.dbname(&name);
        let db = TestDb {
            admin,
            name,
            config: DbConfig::new(pg),
        };
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
        Migrator::from_dir(&dir)
            .expect("migrations")
            .migrate(&mut db.client())
            .expect("migrate");
        Some(db)
    }

    pub fn client(&self) -> Client {
        self.config.pg.connect(NoTls).expect("connect test db")
    }

    pub fn pool(&self, size: usize) -> Pool {
        Pool::connect(&self.config, size).expect("pool")
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
