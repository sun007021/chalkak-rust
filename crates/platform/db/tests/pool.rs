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

// ---- 빈자리 백그라운드 채우기(HikariCP `closeConnection` -> `fillPool`) ----

/// 관리용 연결(테스트 DB가 아니라 `CHALKAK_TEST_DB_URL`이 가리키는 기본 DB).
fn admin() -> postgres::Client {
    let url = std::env::var(common::ENV_URL).unwrap();
    let pg: postgres::Config = url.parse().unwrap();
    pg.connect(postgres::NoTls).unwrap()
}

/// 풀이 테스트 DB에 맺어 둔 연결 수(관리 연결은 제외).
fn backends(admin: &mut postgres::Client, db: &str) -> i64 {
    admin
        .query_one(
            "select count(*) from pg_stat_activity where datname = $1 and pid <> pg_backend_pid()",
            &[&db],
        )
        .unwrap()
        .get(0)
}

fn terminate_all(admin: &mut postgres::Client, db: &str) {
    admin
        .execute(
            "select pg_terminate_backend(pid) from pg_stat_activity \
             where datname = $1 and pid <> pg_backend_pid()",
            &[&db],
        )
        .unwrap();
}

/// 조건이 될 때까지 `limit` 동안 기다린다.
fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[test]
fn pool_refills_in_background_after_all_connections_are_killed() {
    let Some(db) = TestDb::create() else { return };
    let mut admin = admin();
    let pool = Pool::connect(&db.config, 5).unwrap();
    assert_eq!(backends(&mut admin, &db.name), 5);

    terminate_all(&mut admin, &db.name);
    thread::sleep(Duration::from_millis(700)); // alive_bypass_window를 넘겨 검사가 돌게 한다.
    // 버림을 일으키는 요청 한 번. 끊긴 유휴 연결을 모두 걸러내고 하나는 직접 맺는다.
    drop(pool.get());

    // 추가 요청 없이 풀이 크기만큼 다시 채워져야 한다.
    let refilled = wait_until(Duration::from_secs(5), || {
        let s = pool.stats();
        s.total == 5 && s.idle == 5
    });
    assert!(refilled, "풀이 채워지지 않음: {:?}", pool.stats());
    assert_eq!(backends(&mut admin, &db.name), 5);
}

#[test]
fn pool_refills_by_retrying_while_database_refuses_connections() {
    let Some(db) = TestDb::create() else { return };
    let mut admin = admin();
    let mut config = db.config.clone();
    config.connection_timeout = Duration::from_millis(400);
    let pool = Pool::connect(&config, 3).unwrap();

    admin
        .batch_execute(&format!(
            "alter database {} allow_connections false",
            db.name
        ))
        .unwrap();
    terminate_all(&mut admin, &db.name);
    thread::sleep(Duration::from_millis(700));
    // 접속이 거부되는 동안의 요청은 실패한다(버림만 일으키면 된다).
    assert!(pool.get().is_err());
    thread::sleep(Duration::from_millis(600));
    // 채우기 스레드가 맺기를 시도하는 순간에는 자리를 예약해 total이 1일 수 있다. 유휴 연결은 생기지 않는다.
    assert_eq!(pool.stats().idle, 0, "거부되는 동안에는 채워질 수 없다");
    assert_eq!(backends(&mut admin, &db.name), 0);

    admin
        .batch_execute(&format!(
            "alter database {} allow_connections true",
            db.name
        ))
        .unwrap();
    // 재시도 간격은 250ms에서 시작해 늘어난다. 몇 초 안에 채워져야 한다.
    let refilled = wait_until(Duration::from_secs(8), || {
        let s = pool.stats();
        s.total == 3 && s.idle == 3
    });
    assert!(refilled, "재시도로 채워지지 않음: {:?}", pool.stats());
    assert_eq!(backends(&mut admin, &db.name), 3);
}

#[test]
fn refill_never_exceeds_pool_size_while_borrowers_compete() {
    let Some(db) = TestDb::create() else { return };
    let mut admin = admin();
    let pool = Pool::connect(&db.config, 4).unwrap();
    terminate_all(&mut admin, &db.name);
    thread::sleep(Duration::from_millis(700));

    // 채우기와 get의 직접 맺기가 경쟁해도 total은 size를 넘지 않고 모든 get이 연결을 받는다.
    let workers: Vec<_> = (0..12)
        .map(|_| {
            let pool = pool.clone();
            thread::spawn(move || {
                for _ in 0..20 {
                    let mut conn = pool.get().unwrap();
                    let _: i32 = conn.query_one("select 1", &[]).unwrap().get(0);
                    assert!(pool.stats().total <= 4);
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert!(wait_until(Duration::from_secs(5), || pool.stats().idle == 4));
    assert_eq!(backends(&mut admin, &db.name), 4);
}

#[test]
fn dropping_the_pool_stops_the_refill_thread() {
    let Some(db) = TestDb::create() else { return };
    let mut admin = admin();
    let pool = Pool::connect(&db.config, 3).unwrap();
    terminate_all(&mut admin, &db.name);
    thread::sleep(Duration::from_millis(700));
    drop(pool.get());
    pool.close();
    drop(pool);
    // 풀이 사라진 뒤에는 채우기가 연결을 다시 맺지 않는다.
    thread::sleep(Duration::from_secs(1));
    assert_eq!(backends(&mut admin, &db.name), 0);
}

// #36: 같은 SQL을 보낼 때마다 계획을 다시 세우지 않도록 연결마다 준비된 문장을 캐시한다(pgjdbc와 같은 효과).
fn prepared_count(conn: &mut chalkak_db::Conn, like: &str) -> i64 {
    let client: &mut postgres::Client = conn;
    client
        .query_one(
            "SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE $1",
            &[&like],
        )
        .unwrap()
        .get(0)
}

#[test]
fn repeated_queries_reuse_one_prepared_statement_per_connection() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    let sql = "SELECT $1::int + 41 AS answer";
    {
        let mut conn = pool.get().unwrap();
        for _ in 0..3 {
            let row = conn.query_one(sql, &[&1i32]).unwrap();
            assert_eq!(row.get::<_, i32>("answer"), 42);
        }
        assert_eq!(prepared_count(&mut conn, "SELECT $1::int + 41%"), 1);
        assert_eq!(conn.cached_statements(), 1);
    }
    // 풀로 돌아갔다 다시 빌려도(크기 1이라 같은 연결) 캐시가 남고, 트랜잭션 안에서도 같은 문장을 쓴다.
    let mut conn = pool.get().unwrap();
    assert_eq!(conn.cached_statements(), 1);
    conn.transaction(|tx| {
        let row = tx.query_one(sql, &[&2i32])?;
        assert_eq!(row.get::<_, i32>("answer"), 43);
        Ok(())
    })
    .unwrap();
    assert_eq!(prepared_count(&mut conn, "SELECT $1::int + 41%"), 1);
}

#[test]
fn statement_cache_is_bounded() {
    use chalkak_db::STATEMENT_CACHE_CAPACITY;
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    let mut conn = pool.get().unwrap();
    for i in 0..STATEMENT_CACHE_CAPACITY + 10 {
        conn.query_one(&format!("SELECT {i} AS n"), &[]).unwrap();
    }
    assert!(conn.cached_statements() <= STATEMENT_CACHE_CAPACITY);
}

#[test]
fn replaced_connection_starts_with_an_empty_cache() {
    let Some(db) = TestDb::create() else { return };
    let pool = Pool::connect(&db.config, 1).unwrap();
    let victim = {
        let mut conn = pool.get().unwrap();
        conn.query_one("SELECT 1 AS one", &[]).unwrap();
        pid(&mut conn)
    };
    db.client()
        .execute("SELECT pg_terminate_backend($1)", &[&victim])
        .unwrap();
    thread::sleep(Duration::from_millis(600)); // alive_bypass_window(500ms)가 지나야 빌릴 때 검사한다
    let mut conn = pool.get().unwrap();
    assert_eq!(conn.cached_statements(), 0);
    assert_ne!(pid(&mut conn), victim);
    conn.query_one("SELECT 1 AS one", &[]).unwrap();
}
