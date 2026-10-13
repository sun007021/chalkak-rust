//! `pg_stat_statements`로 요청당 SQL 수와 DB 시간을 잰다.
//!
//! 구하는 법: 통계를 비우고 한 엔드포인트만 순서대로 K번 부른 뒤 문장별 호출 수와 실행 시간을 모은다.
//! 문장 수를 둘로 센다. `data`는 SELECT/INSERT/UPDATE/DELETE/WITH(실제 일을 하는 문장), `control`은
//! BEGIN/COMMIT/ROLLBACK/SET/SHOW/DISCARD 등(JDBC와 postgres crate가 트랜잭션을 거는 방식이 달라 따로 본다).

use postgres::Client;

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub query: String,
    pub calls: i64,
    pub total_ms: f64,
    pub rows: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Data,
    Control,
}

pub fn classify(query: &str) -> Kind {
    let first = query
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "MERGE" => Kind::Data,
        _ => Kind::Control,
    }
}

/// 이 도구 자신이나 하네스가 보낸 문장인가.
fn is_own(query: &str) -> bool {
    query.contains("pg_stat_statements") || query.contains("pg_stat_activity")
}

pub fn reset(client: &mut Client) -> Result<(), postgres::Error> {
    client.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")?;
    client.batch_execute("SELECT pg_stat_statements_reset()")
}

pub fn snapshot(client: &mut Client) -> Result<Vec<Stmt>, postgres::Error> {
    let rows = client.query(
        "SELECT query, calls, total_exec_time, rows FROM pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
         ORDER BY calls DESC",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| Stmt {
            query: r.get(0),
            calls: r.get(1),
            total_ms: r.get(2),
            rows: r.get(3),
        })
        .filter(|s| !is_own(&s.query))
        .collect())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub data_calls: i64,
    pub control_calls: i64,
    pub db_ms: f64,
}

pub fn totals(stmts: &[Stmt]) -> Totals {
    let mut t = Totals::default();
    for s in stmts {
        match classify(&s.query) {
            Kind::Data => t.data_calls += s.calls,
            Kind::Control => t.control_calls += s.calls,
        }
        t.db_ms += s.total_ms;
    }
    t
}

/// 이 데이터베이스에 붙은 다른 세션 수(풀 크기 확인, 연결 누수 감시용).
pub fn connection_count(client: &mut Client) -> Result<i64, postgres::Error> {
    let row = client.query_one(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND pid <> pg_backend_pid() AND backend_type = 'client backend'",
        &[],
    )?;
    Ok(row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 문장_종류를_나눈다() {
        for q in [
            "select p1_0.id from posts p1_0",
            "  SELECT 1",
            "INSERT INTO t VALUES ($1)",
            "update posts set x = $1",
            "DELETE FROM post_likes",
            "WITH x AS (SELECT 1) SELECT * FROM x",
        ] {
            assert_eq!(classify(q), Kind::Data, "{q}");
        }
        for q in [
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SET extra_float_digits = 3",
            "SHOW TRANSACTION ISOLATION LEVEL",
            "DEALLOCATE ALL",
            "",
        ] {
            assert_eq!(classify(q), Kind::Control, "{q}");
        }
    }

    #[test]
    fn 합계는_문장_종류별로_센다() {
        let stmts = vec![
            Stmt {
                query: "SELECT 1".into(),
                calls: 3,
                total_ms: 1.5,
                rows: 3,
            },
            Stmt {
                query: "BEGIN".into(),
                calls: 2,
                total_ms: 0.1,
                rows: 0,
            },
            Stmt {
                query: "UPDATE t SET a = 1".into(),
                calls: 1,
                total_ms: 2.0,
                rows: 1,
            },
        ];
        let t = totals(&stmts);
        assert_eq!((t.data_calls, t.control_calls), (4, 2));
        assert!((t.db_ms - 3.6).abs() < 1e-9);
    }

    #[test]
    fn 자기_문장은_걸러낸다() {
        assert!(is_own("SELECT query FROM pg_stat_statements"));
        assert!(is_own("SELECT count(*) FROM pg_stat_activity"));
        assert!(!is_own("SELECT * FROM posts"));
    }
}
