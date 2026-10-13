//! DB 연결 TLS(`sslmode=prefer`, 인증서 검증 없음). 원본 pgjdbc 기본 동작이다.
//!
//! - `CHALKAK_TEST_TLS_DB_URL`: 자체 서명 인증서로 TLS를 켠 Postgres. 있으면 연결이 TLS를 쓰는지 본다.
//! - `CHALKAK_TEST_DB_URL`: TLS가 꺼진 Postgres. 있으면 평문으로 물러나 연결되는지 본다.
//!
//! 해당 환경변수가 없으면 건너뛴다(CI에는 DB가 없다).

use chalkak_db::{DbConfig, Pool};

fn ssl_in_use(config: DbConfig) -> bool {
    let pool = Pool::connect(&config, 1).expect("풀");
    let mut conn = pool.get().expect("연결");
    let row = conn
        .query_one(
            "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .expect("pg_stat_ssl");
    row.get::<_, bool>(0)
}

#[test]
fn tls를_받는_서버에는_tls로_연결한다() {
    let Ok(url) = std::env::var("CHALKAK_TEST_TLS_DB_URL") else {
        eprintln!("skipped: CHALKAK_TEST_TLS_DB_URL is not set");
        return;
    };
    // 자체 서명 인증서여도 검증하지 않으므로 연결된다.
    assert!(ssl_in_use(DbConfig::from_url(&url).unwrap()));
}

#[test]
fn tls를_받지_않는_서버에는_평문으로_연결한다() {
    let Ok(url) = std::env::var("CHALKAK_TEST_DB_URL") else {
        eprintln!("skipped: CHALKAK_TEST_DB_URL is not set");
        return;
    };
    assert!(!ssl_in_use(DbConfig::from_url(&url).unwrap()));
}
