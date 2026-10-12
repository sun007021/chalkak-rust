//! 풀·트랜잭션 통합 테스트. 근거는 HikariCP 기본 동작(크기 고정, connectionTimeout,
//! 끊긴 연결 교체, maxLifetime)과 Spring `@Transactional`의 롤백 규칙이다.

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chalkak_db::{DbConfig, DbError, Pool};
use common::TestDb;

fn pid(conn: &mut chalkak_db::Conn) -> i32 {
    conn.query_one("select pg_backend_pid()", &[])
        .unwrap()
        .get(0)
}

#[test]
fn defaults_match_hikari_defaults() {
    // application*.yml에 hikari 설정이 없으므로 HikariCP 기본값이다.
    let c = DbConfig::from_url("postgres://u:p@localhost/db").unwrap();
    assert_eq!(DbConfig::DEFAULT_POOL_SIZE, 10);
    assert_eq!(c.connection_timeout, Duration::from_secs(30));
    assert_eq!(c.validation_timeout, Duration::from_secs(5));
    assert_eq!(c.max_lifetime, Duration::from_secs(1800));
    assert_eq!(c.alive_bypass_window, Duration::from_millis(500));
}

#[test]
fn connect_fails_fast_when_database_is_unreachable() {
    let mut config = DbConfig::from_url("postgres://u:p@127.0.0.1:1/db").unwrap();
    config.connection_timeout = Duration::from_secs(1);
    assert!(matches!(
        Pool::connect(&config, 2),
        Err(DbError::Postgres(_))
    ));
    assert!(matches!(Pool::connect(&config, 0), Err(DbError::Config(_))));
}

#[test]
fn borrowed_connection_returns_to_the_pool_on_drop() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 3).unwrap();
    assert_eq!((pool.stats().idle, pool.stats().active), (3, 0));
    let mut conn = pool.get().unwrap();
    let one: i32 = conn.query_one("select 1", &[]).unwrap().get(0);
    assert_eq!(one, 1);
    assert_eq!((pool.stats().idle, pool.stats().active), (2, 1));
    drop(conn);
    assert_eq!((pool.stats().idle, pool.stats().active), (3, 0));
}

#[test]
fn pool_size_is_fixed_and_wait_times_out() {
    let Some(db) = TestDb::create() else { return };
    let mut config = db.config.clone();
    config.connection_timeout = Duration::from_millis(300);
    let pool = Pool::connect(&config, 2).unwrap();
    let a = pool.get().unwrap();
    let _b = pool.get().unwrap();
    let started = Instant::now();
    assert!(matches!(pool.get(), Err(DbError::PoolTimeout { .. })));
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(pool.stats().total, 2);
    drop(a);
    assert!(pool.get().is_ok());
}

#[test]
fn waiting_borrower_gets_the_connection_released_by_another_thread() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    let held = pool.get().unwrap();
    let (tx, rx) = mpsc::channel();
    let waiter = {
        let pool = pool.clone();
        thread::spawn(move || {
            let mut conn = pool.get().unwrap();
            tx.send(pid(&mut conn)).unwrap();
        })
    };
    thread::sleep(Duration::from_millis(150));
    assert_eq!(pool.stats().waiting, 1);
    drop(held);
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    waiter.join().unwrap();
}

#[test]
fn dead_connection_is_replaced_when_borrowed() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    let old_pid = pid(&mut pool.get().unwrap());
    // 서버 쪽에서 연결을 끊는다(RDS 장애 조치·idle 연결 정리와 같은 상황).
    db.client()
        .execute("select pg_terminate_backend($1)", &[&old_pid])
        .unwrap();
    thread::sleep(Duration::from_millis(700)); // alive_bypass_window(500ms)를 넘겨 검사가 돌게 한다.
    let mut conn = pool.get().unwrap();
    let new_pid = pid(&mut conn);
    assert_ne!(old_pid, new_pid);
    assert_eq!(pool.stats().total, 1);
}

#[test]
fn connection_older_than_max_lifetime_is_replaced() {
    let Some(db) = TestDb::create() else { return };
    let mut config = db.config.clone();
    config.max_lifetime = Duration::from_millis(100);
    let pool = Pool::connect(&config, 1).unwrap();
    let old_pid = pid(&mut pool.get().unwrap());
    thread::sleep(Duration::from_millis(200));
    assert_ne!(old_pid, pid(&mut pool.get().unwrap()));
    assert_eq!(pool.stats().total, 1);
}

#[test]
fn transaction_commits_on_ok_and_rolls_back_on_err() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    let mut conn = pool.get().unwrap();
    conn.batch_execute("create table t (id int primary key)")
        .unwrap();

    let committed = conn.transaction(|tx| {
        tx.execute("insert into t values (1)", &[])?;
        Ok("done")
    });
    assert_eq!(committed.unwrap(), "done");

    let failed: Result<(), DbError> = conn.transaction(|tx| {
        tx.execute("insert into t values (2)", &[])?;
        // 제약 위반: 앞의 insert까지 함께 롤백돼야 한다.
        tx.execute("insert into t values (1)", &[])?;
        Ok(())
    });
    assert!(matches!(failed, Err(DbError::Postgres(_))));

    let count: i64 = conn
        .query_one("select count(*) from t", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 1);

    // 호출 측이 돌려준 오류도 롤백한다.
    let rolled_back: Result<(), DbError> = conn.transaction(|tx| {
        tx.execute("insert into t values (3)", &[])?;
        Err(DbError::Config("business rule".to_string()))
    });
    assert!(matches!(rolled_back, Err(DbError::Config(_))));
    let count: i64 = conn
        .query_one("select count(*) from t", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}

#[test]
fn panic_inside_transaction_rolls_back_and_connection_stays_usable() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    pool.get()
        .unwrap()
        .batch_execute("create table t (id int)")
        .unwrap();
    let p = pool.clone();
    let result = thread::spawn(move || {
        let mut conn = p.get().unwrap();
        let _ = conn.transaction(|tx| {
            tx.execute("insert into t values (1)", &[])?;
            panic!("boom");
            #[allow(unreachable_code)]
            Ok(())
        });
    })
    .join();
    assert!(result.is_err());
    let mut conn = pool.get().unwrap();
    let count: i64 = conn
        .query_one("select count(*) from t", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[test]
fn close_rejects_new_borrows() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    pool.close();
    assert!(matches!(pool.get(), Err(DbError::PoolClosed)));
}
