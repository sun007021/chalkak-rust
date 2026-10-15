//! PostgreSQL 문장 로그(`log_statement=all`)를 읽어 앱 세션이 보낸 문장을 트랜잭션 목록으로 자른다.
//!
//! lane DB는 `TXLOG=1`일 때 줄 머리를 `@@세션|앱이름|사용자|xid|vxid|시각|`로 쓴다(harness/lane/compose.yaml).
//! 한 줄 예: `@@68f1.1a|PostgreSQL JDBC Driver|chalkak|0|4/12|2026-10-12 00:00:01.123 UTC|LOG:  execute <unnamed>: BEGIN`
//! 문장 본문이 여러 줄이면 이어지는 줄에는 머리가 없다(앞 기록에 붙인다).
//!
//! 하네스 자신의 문장(`application_name = chalkak-harness`)은 앱 문장과 구분해 뺀다. Rust 앱은 application_name을
//! 보내지 않아 비어 있고, pgjdbc는 `PostgreSQL JDBC Driver`를 보낸다.

use crate::sqlsig::{Sig, signature};
use std::collections::HashMap;

/// 하네스가 DB에 붙을 때 쓰는 application_name.
pub const HARNESS_APP: &str = "chalkak-harness";

pub const DEFAULT_ISOLATION: &str = "READ COMMITTED";

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Stmt(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub session: String,
    pub app: String,
    pub msg: Msg,
}

impl Entry {
    pub fn is_harness(&self) -> bool {
        self.app == HARNESS_APP
    }
}

/// 로그 본문을 기록 목록으로 바꾼다. 문장이나 오류가 아닌 기록(DETAIL, STATEMENT, 접속 로그 등)은 버린다.
pub fn parse_log(text: &str) -> Vec<Entry> {
    // 먼저 기록 단위(머리 + 이어지는 줄)로 묶는다.
    let mut records: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with("@@") {
            records.push(line.to_string());
        } else if let Some(last) = records.last_mut() {
            last.push('\n');
            last.push_str(line);
        }
    }
    let mut out = Vec::new();
    for rec in records {
        let body = &rec[2..];
        let mut parts = body.splitn(7, '|');
        let (
            Some(session),
            Some(app),
            Some(_user),
            Some(_xid),
            Some(_vxid),
            Some(_time),
            Some(msg),
        ) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        )
        else {
            continue;
        };
        let msg = if let Some(rest) = msg.strip_prefix("LOG:  statement: ") {
            Msg::Stmt(rest.to_string())
        } else if let Some(rest) = msg.strip_prefix("LOG:  execute ") {
            // `<이름>: <SQL>`. 이름에는 ": "가 없다.
            match rest.split_once(": ") {
                Some((_, sql)) => Msg::Stmt(sql.to_string()),
                None => continue,
            }
        } else if let Some(rest) = msg.strip_prefix("ERROR:  ") {
            Msg::Error(rest.lines().next().unwrap_or("").to_string())
        } else {
            continue;
        };
        out.push(Entry {
            session: session.to_string(),
            app: app.to_string(),
            msg,
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Commit,
    Rollback,
    /// 시나리오가 끝날 때까지 닫히지 않았다.
    Open,
    /// 명시적 트랜잭션이 아닌 단일 문장(자동 커밋)
    Auto,
}

/// 트랜잭션 하나(또는 자동 커밋 문장 하나).
#[derive(Debug, Clone, PartialEq)]
pub struct Tx {
    /// 이 트랜잭션이 시작된 단계(요청) 번호
    pub step: usize,
    pub session: String,
    pub auto: bool,
    pub read_only: bool,
    pub isolation: String,
    pub end: End,
    /// 잡음을 뺀 문장 서명(실행 순서)
    pub stmts: Vec<Sig>,
    /// 실행 중 오류가 났다(롤백 예정이거나 자동 커밋 문장 실패)
    pub failed: bool,
}

#[derive(Default)]
struct Session {
    /// `SET SESSION CHARACTERISTICS`로 바뀐 기본 격리 수준
    isolation: Option<String>,
    /// 마지막으로 낸 자동 커밋 문장의 `out` 위치(뒤따르는 오류 표시용)
    last_auto: Option<usize>,
}

fn isolation_of(lower: &str) -> Option<String> {
    let at = lower.find("isolation level")?;
    let rest = &lower[at + "isolation level".len()..];
    let words: Vec<&str> = rest
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .collect();
    let level = match words.as_slice() {
        ["read", "committed", ..] => "READ COMMITTED",
        ["read", "uncommitted", ..] => "READ UNCOMMITTED",
        ["repeatable", "read", ..] => "REPEATABLE READ",
        ["serializable", ..] => "SERIALIZABLE",
        _ => return None,
    };
    Some(level.to_string())
}

/// 단계 번호가 붙은 앱 기록을 세션별로 트랜잭션으로 자른다. 입력 순서는 로그 순서다.
/// 문장이 하나도 없는(잡음뿐인) 트랜잭션은 낸 결과에서 뺀다.
pub fn build_txs(entries: &[(usize, &Entry)]) -> Vec<Tx> {
    let mut sessions: HashMap<&str, Session> = HashMap::new();
    // 트랜잭션은 시작 순서로 돌려준다(닫힌 순서가 아니다). 시작할 때 자리를 만들고 닫을 때 채운다.
    let mut slots: Vec<Option<Tx>> = Vec::new();
    let mut open_slot: HashMap<&str, usize> = HashMap::new();

    for (step, e) in entries {
        let s = sessions.entry(e.session.as_str()).or_default();
        match &e.msg {
            Msg::Error(_) => {
                if let Some(&slot) = open_slot.get(e.session.as_str()) {
                    if let Some(tx) = slots[slot].as_mut() {
                        tx.failed = true;
                    }
                } else if let Some(slot) = s.last_auto
                    && let Some(tx) = slots[slot].as_mut()
                {
                    tx.failed = true;
                }
            }
            Msg::Stmt(sql) => {
                let trimmed = sql.trim().trim_end_matches(';').trim();
                let lower = trimmed.to_lowercase();
                let first = lower.split_whitespace().next().unwrap_or("");
                match first {
                    "begin" | "start" if !open_slot.contains_key(e.session.as_str()) => {
                        let tx = Tx {
                            step: *step,
                            session: e.session.clone(),
                            auto: false,
                            read_only: lower.contains("read only"),
                            isolation: isolation_of(&lower)
                                .or_else(|| s.isolation.clone())
                                .unwrap_or_else(|| DEFAULT_ISOLATION.to_string()),
                            end: End::Open,
                            stmts: Vec::new(),
                            failed: false,
                        };
                        slots.push(Some(tx));
                        open_slot.insert(e.session.as_str(), slots.len() - 1);
                        s.last_auto = None;
                    }
                    "commit" | "end" | "rollback" | "abort" => {
                        if let Some(slot) = open_slot.remove(e.session.as_str())
                            && let Some(tx) = slots[slot].as_mut()
                        {
                            // 실패한 트랜잭션의 COMMIT은 PostgreSQL이 ROLLBACK으로 처리한다.
                            tx.end = if matches!(first, "rollback" | "abort") || tx.failed {
                                End::Rollback
                            } else {
                                End::Commit
                            };
                        }
                    }
                    "set" => {
                        if let Some(level) = isolation_of(&lower) {
                            if lower.starts_with("set session characteristics") {
                                s.isolation = Some(level);
                            } else if let Some(&slot) = open_slot.get(e.session.as_str())
                                && let Some(tx) = slots[slot].as_mut()
                            {
                                tx.isolation = level;
                            }
                        }
                        if lower.contains("transaction read only")
                            && !lower.starts_with("set session")
                            && let Some(&slot) = open_slot.get(e.session.as_str())
                            && let Some(tx) = slots[slot].as_mut()
                        {
                            tx.read_only = true;
                        }
                    }
                    _ => {
                        let Some(sig) = signature(sql) else { continue };
                        if let Some(&slot) = open_slot.get(e.session.as_str()) {
                            if let Some(tx) = slots[slot].as_mut() {
                                tx.stmts.push(sig);
                            }
                        } else {
                            slots.push(Some(Tx {
                                step: *step,
                                session: e.session.clone(),
                                auto: true,
                                read_only: false,
                                isolation: s
                                    .isolation
                                    .clone()
                                    .unwrap_or_else(|| DEFAULT_ISOLATION.to_string()),
                                end: End::Auto,
                                stmts: vec![sig],
                                failed: false,
                            }));
                            s.last_auto = Some(slots.len() - 1);
                        }
                    }
                }
            }
        }
    }
    let mut out: Vec<Tx> = Vec::new();
    for tx in slots.into_iter().flatten() {
        if tx.stmts.is_empty() {
            continue;
        }
        out.push(tx);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(session: &str, app: &str, msg: &str) -> String {
        format!("@@{session}|{app}|chalkak|0|1/1|2026-10-12 00:00:00.000 UTC|{msg}")
    }

    fn txs(lines: &[(&str, &str, &str)]) -> Vec<Tx> {
        let text = lines
            .iter()
            .map(|(s, a, m)| line(s, a, m))
            .collect::<Vec<_>>()
            .join("\n");
        let entries = parse_log(&text);
        let refs: Vec<(usize, &Entry)> = entries.iter().map(|e| (0, e)).collect();
        build_txs(&refs)
    }

    #[test]
    fn 단순_프로토콜과_확장_프로토콜_문장을_읽는다() {
        let text = [
            line("a", "", "LOG:  statement: BEGIN"),
            line(
                "a",
                "",
                "LOG:  execute s1: SELECT id FROM users WHERE id = $1",
            ),
            line("a", "", "DETAIL:  parameters: $1 = 'x'"),
            line("a", "", "LOG:  execute <unnamed>: SELECT 1"),
            line("a", "", "LOG:  connection authorized: user=chalkak"),
        ]
        .join("\n");
        let es = parse_log(&text);
        assert_eq!(es.len(), 3);
        assert_eq!(es[0].msg, Msg::Stmt("BEGIN".into()));
        assert_eq!(
            es[1].msg,
            Msg::Stmt("SELECT id FROM users WHERE id = $1".into())
        );
    }

    #[test]
    fn 여러_줄_문장은_앞_기록에_붙인다() {
        let text = format!(
            "{}\n  FROM users\n WHERE id = $1\n{}",
            line("a", "", "LOG:  execute <unnamed>: SELECT id"),
            line("a", "", "LOG:  statement: COMMIT")
        );
        let es = parse_log(&text);
        assert_eq!(es.len(), 2);
        assert_eq!(
            es[0].msg,
            Msg::Stmt("SELECT id\n  FROM users\n WHERE id = $1".into())
        );
    }

    #[test]
    fn 하네스_세션을_구분한다() {
        let es = parse_log(&line("h", HARNESS_APP, "LOG:  statement: SELECT 1"));
        assert!(es[0].is_harness());
        let es = parse_log(&line("a", "", "LOG:  statement: SELECT 1"));
        assert!(!es[0].is_harness());
    }

    #[test]
    fn begin_commit으로_트랜잭션을_자른다() {
        let t = txs(&[
            ("a", "", "LOG:  execute <unnamed>: BEGIN"),
            (
                "a",
                "",
                "LOG:  execute <unnamed>: select * from users where id=$1 for update",
            ),
            (
                "a",
                "",
                "LOG:  execute <unnamed>: update users set a=$1 where id=$2",
            ),
            ("a", "", "LOG:  execute <unnamed>: COMMIT"),
        ]);
        assert_eq!(t.len(), 1);
        assert!(!t[0].auto && !t[0].read_only);
        assert_eq!(t[0].end, End::Commit);
        assert_eq!(t[0].stmts.len(), 2);
        assert_eq!(t[0].isolation, DEFAULT_ISOLATION);
    }

    #[test]
    fn begin_read_only와_격리_수준을_읽는다() {
        let t = txs(&[
            ("a", "", "LOG:  execute <unnamed>: BEGIN READ ONLY"),
            ("a", "", "LOG:  execute <unnamed>: select * from posts"),
            ("a", "", "LOG:  execute <unnamed>: COMMIT"),
            (
                "b",
                "",
                "LOG:  statement: BEGIN ISOLATION LEVEL SERIALIZABLE",
            ),
            ("b", "", "LOG:  execute <unnamed>: select * from posts"),
            ("b", "", "LOG:  statement: ROLLBACK"),
        ]);
        assert!(t[0].read_only);
        assert_eq!(t[1].isolation, "SERIALIZABLE");
        assert_eq!(t[1].end, End::Rollback);
    }

    #[test]
    fn 트랜잭션_밖의_문장은_자동_커밋_하나씩이다() {
        let t = txs(&[
            ("a", "", "LOG:  execute s1: select * from posts"),
            ("a", "", "LOG:  execute s2: select * from users"),
        ]);
        assert_eq!(t.len(), 2);
        assert!(t.iter().all(|x| x.auto && x.end == End::Auto));
    }

    #[test]
    fn 세션이_섞여도_세션별로_자른다() {
        let t = txs(&[
            ("a", "", "LOG:  statement: BEGIN"),
            ("b", "", "LOG:  statement: BEGIN"),
            ("a", "", "LOG:  execute s: update posts set a=1"),
            ("b", "", "LOG:  execute s: delete from likes"),
            ("b", "", "LOG:  statement: COMMIT"),
            ("a", "", "LOG:  statement: ROLLBACK"),
        ]);
        assert_eq!(t.len(), 2);
        // 시작 순서: a, b
        assert_eq!(t[0].session, "a");
        assert_eq!(t[0].end, End::Rollback);
        assert_eq!(t[1].end, End::Commit);
    }

    #[test]
    fn 잡음만_든_트랜잭션은_뺀다() {
        let t = txs(&[
            ("a", "", "LOG:  statement: BEGIN"),
            ("a", "", "LOG:  execute <unnamed>: SELECT 1"),
            ("a", "", "LOG:  statement: COMMIT"),
        ]);
        assert!(t.is_empty());
    }

    #[test]
    fn 오류가_난_트랜잭션의_commit은_롤백이다() {
        let t = txs(&[
            ("a", "", "LOG:  statement: BEGIN"),
            (
                "a",
                "",
                "LOG:  execute s: insert into posts (a) values ($1)",
            ),
            (
                "a",
                "",
                "ERROR:  duplicate key value violates unique constraint",
            ),
            ("a", "", "LOG:  statement: COMMIT"),
        ]);
        assert!(t[0].failed);
        assert_eq!(t[0].end, End::Rollback);
    }

    #[test]
    fn 자동_커밋_문장의_오류를_표시한다() {
        let t = txs(&[
            (
                "a",
                "",
                "LOG:  execute s: insert into posts (a) values ($1)",
            ),
            ("a", "", "ERROR:  duplicate key"),
        ]);
        assert!(t[0].failed && t[0].auto);
    }

    #[test]
    fn 닫히지_않은_트랜잭션은_open이다() {
        let t = txs(&[
            ("a", "", "LOG:  statement: BEGIN"),
            ("a", "", "LOG:  execute s: update posts set a=1"),
        ]);
        assert_eq!(t[0].end, End::Open);
    }

    #[test]
    fn 트랜잭션_안_set_transaction으로_격리_수준이_바뀐다() {
        let t = txs(&[
            ("a", "", "LOG:  statement: BEGIN"),
            (
                "a",
                "",
                "LOG:  statement: SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            ),
            ("a", "", "LOG:  execute s: update posts set a=1"),
            ("a", "", "LOG:  statement: COMMIT"),
        ]);
        assert_eq!(t[0].isolation, "REPEATABLE READ");
    }

    #[test]
    fn 세션_기본_격리_수준은_이후_트랜잭션에_적용된다() {
        let t = txs(&[
            (
                "a",
                "",
                "LOG:  statement: SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            ),
            ("a", "", "LOG:  statement: BEGIN"),
            ("a", "", "LOG:  execute s: update posts set a=1"),
            ("a", "", "LOG:  statement: COMMIT"),
        ]);
        assert_eq!(t[0].isolation, "SERIALIZABLE");
    }
}
