//! SQL 문장 서명: Hibernate가 만든 SQL과 손으로 쓴 SQL을 같은 모양으로 비교하려고 문장 텍스트를 줄인다.
//!
//! 서명 = 종류(SELECT/INSERT/UPDATE/DELETE/UPSERT) + 대상 테이블 + 잠금(FOR UPDATE 등, pg_advisory_xact_lock).
//! 열 목록·별칭·WHERE 모양·파라미터는 버린다. 풀 검증(`SELECT 1`), SET/SHOW, pgjdbc 타입 조회처럼 앱 동작과
//! 상관없는 문장은 `signature`가 None을 돌려준다(잡음).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Select,
    Insert,
    Update,
    Delete,
    /// `INSERT ... ON CONFLICT`
    Upsert,
    /// 위에 없는 쓰기성 명령(TRUNCATE, LOCK TABLE 등)
    Other,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Select => "SELECT",
            Kind::Insert => "INSERT",
            Kind::Update => "UPDATE",
            Kind::Delete => "DELETE",
            Kind::Upsert => "UPSERT",
            Kind::Other => "OTHER",
        }
    }
}

/// 문장 하나의 서명.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sig {
    pub kind: Kind,
    /// 쓰기면 쓴 테이블, 읽기면 읽은 테이블(정렬·중복 없음).
    pub tables: BTreeSet<String>,
    /// 잠금 표기. 예: `FOR UPDATE`, `FOR NO KEY UPDATE OF posts`, `advisory_xact_lock/2`. 문장 안의 등장 순서.
    pub locks: Vec<String>,
}

impl Sig {
    /// 행이나 트랜잭션 범위의 잠금을 잡는 문장인가.
    pub fn is_lock(&self) -> bool {
        !self.locks.is_empty()
    }

    /// 데이터를 바꾸는 문장인가.
    pub fn is_write(&self) -> bool {
        self.kind != Kind::Select
    }

    /// 읽기만 하고 잠그지도 않는 문장인가.
    pub fn is_plain_read(&self) -> bool {
        !self.is_write() && !self.is_lock()
    }
}

impl fmt::Display for Sig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind.name())?;
        if !self.tables.is_empty() {
            let tables = self.tables.iter().cloned().collect::<Vec<_>>().join(",");
            write!(f, " {tables}")?;
        }
        for l in &self.locks {
            write!(f, " {l}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// 따옴표 없는 식별자·키워드(소문자)
    Word(String),
    /// "..." 식별자
    Quoted(String),
    /// 문자열·숫자·달러 인용 본문(내용은 쓰지 않는다)
    Lit,
    Param,
    Punct(char),
}

fn lex(sql: &str) -> Vec<Tok> {
    let b: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && b.get(i + 1) == Some(&'-') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if c == '\'' {
            i += 1;
            while i < b.len() {
                if b[i] == '\'' {
                    if b.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Lit);
        } else if c == '"' {
            let mut s = String::new();
            i += 1;
            while i < b.len() {
                if b[i] == '"' {
                    if b.get(i + 1) == Some(&'"') {
                        s.push('"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                s.push(b[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Quoted(s));
        } else if c == '$' && b.get(i + 1).is_some_and(char::is_ascii_digit) {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            out.push(Tok::Param);
        } else if c == '$' {
            // 달러 인용: $tag$ ... $tag$
            let mut j = i + 1;
            while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            if b.get(j) == Some(&'$') {
                let tag: String = b[i..=j].iter().collect();
                let rest: String = b[j + 1..].iter().collect();
                i = match rest.find(&tag) {
                    Some(p) => j + 1 + rest[..p].chars().count() + tag.chars().count(),
                    None => b.len(),
                };
                out.push(Tok::Lit);
            } else {
                i += 1;
                out.push(Tok::Punct('$'));
            }
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '.') {
                i += 1;
            }
            out.push(Tok::Lit);
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                i += 1;
            }
            out.push(Tok::Word(
                b[start..i].iter().collect::<String>().to_lowercase(),
            ));
        } else if c == '?' {
            i += 1;
            out.push(Tok::Param);
        } else {
            i += 1;
            out.push(Tok::Punct(c));
        }
    }
    out
}

/// 별칭으로 쓸 수 없는(뒤에 오는 구문 시작) 단어.
const NOT_ALIAS: &[&str] = &[
    "where",
    "on",
    "using",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "natural",
    "group",
    "order",
    "limit",
    "offset",
    "having",
    "union",
    "intersect",
    "except",
    "for",
    "returning",
    "set",
    "window",
    "fetch",
    "values",
    "lateral",
    "tablesample",
    "select",
    "when",
    "then",
    "and",
    "or",
    "as",
    "from",
    "with",
];

struct Scan<'a> {
    t: &'a [Tok],
    reads: BTreeSet<String>,
    writes: BTreeSet<String>,
    aliases: BTreeMap<String, String>,
    kind: Kind,
    on_conflict: bool,
    locks: Vec<(String, Vec<String>, String)>,
    advisory: Vec<String>,
}

fn word(t: Option<&Tok>) -> Option<&str> {
    match t {
        Some(Tok::Word(w)) => Some(w),
        _ => None,
    }
}

impl<'a> Scan<'a> {
    fn w(&self, i: usize) -> Option<&str> {
        word(self.t.get(i))
    }

    /// `i`부터 (스키마.)테이블 이름 하나를 읽는다. 함수 호출이나 하위 질의면 None. (이름, 다음 위치)
    fn table_name(&self, mut i: usize, allow_paren: bool) -> Option<(String, usize)> {
        if self.w(i) == Some("only") {
            i += 1;
        }
        let mut parts = Vec::new();
        loop {
            match self.t.get(i)? {
                Tok::Word(w) => parts.push(w.clone()),
                Tok::Quoted(q) => parts.push(q.clone()),
                _ => return None,
            }
            i += 1;
            if self.t.get(i) == Some(&Tok::Punct('.')) {
                i += 1;
            } else {
                break;
            }
        }
        if !allow_paren && self.t.get(i) == Some(&Tok::Punct('(')) {
            return None; // 함수 호출(unnest(...) 등)
        }
        // 키워드가 테이블 자리에 온 경우(`from select` 등)는 버린다.
        if parts.len() == 1 && matches!(parts[0].as_str(), "select" | "lateral") {
            return None;
        }
        let name = parts.pop()?;
        let schema = parts.pop();
        let name = match schema {
            Some(s) if s == "pg_catalog" || s == "information_schema" => format!("{s}.{name}"),
            _ => name,
        };
        Some((name, i))
    }

    /// `FROM`/`JOIN`/`USING` 뒤의 테이블 목록(쉼표로 이어진 것 포함)을 읽는다.
    fn table_list(&mut self, mut i: usize, into_writes: bool) -> usize {
        loop {
            let Some((name, mut j)) = self.table_name(i, into_writes) else {
                return i;
            };
            if into_writes {
                self.writes.insert(name.clone());
            } else {
                self.reads.insert(name.clone());
            }
            // 별칭
            if self.w(j) == Some("as") {
                j += 1;
            }
            let alias = match self.t.get(j) {
                Some(Tok::Word(w)) if !NOT_ALIAS.contains(&w.as_str()) => Some(w.clone()),
                Some(Tok::Quoted(q)) => Some(q.clone()),
                _ => None,
            };
            if let Some(a) = alias {
                self.aliases.insert(a, name);
                j += 1;
            }
            if self.t.get(j) == Some(&Tok::Punct(',')) && !into_writes {
                i = j + 1;
            } else {
                return j;
            }
        }
    }

    fn run(&mut self) {
        let mut i = 0;
        while i < self.t.len() {
            let prev = if i > 0 { self.w(i - 1) } else { None };
            match self.w(i) {
                Some("insert") if self.w(i + 1) == Some("into") => {
                    let next = self.table_list(i + 2, true);
                    self.kind = self.kind.max(Kind::Insert);
                    i = next.max(i + 1);
                    continue;
                }
                Some("update")
                    if !matches!(prev, Some("for" | "key" | "do" | "on" | "of" | "no")) =>
                {
                    let next = self.table_list(i + 1, true);
                    if next > i + 1 {
                        self.kind = self.kind.max(Kind::Update);
                    }
                    i = next.max(i + 1);
                    continue;
                }
                Some("delete") if self.w(i + 1) == Some("from") => {
                    let next = self.table_list(i + 2, true);
                    self.kind = self.kind.max(Kind::Delete);
                    i = next.max(i + 1);
                    continue;
                }
                Some("conflict") if prev == Some("on") => self.on_conflict = true,
                Some("from" | "join" | "using") if prev != Some("delete") => {
                    // `DELETE FROM`은 위에서 처리했다.
                    let next = self.table_list(i + 1, false);
                    i = next.max(i + 1);
                    continue;
                }
                Some("for") => self.lock_clause(i),
                Some(
                    f @ ("pg_advisory_xact_lock"
                    | "pg_advisory_xact_lock_shared"
                    | "pg_try_advisory_xact_lock"
                    | "pg_try_advisory_xact_lock_shared"
                    | "pg_advisory_lock"
                    | "pg_advisory_lock_shared"
                    | "pg_try_advisory_lock"
                    | "pg_try_advisory_lock_shared"),
                ) if self.t.get(i + 1) == Some(&Tok::Punct('(')) => {
                    let args = self.arg_count(i + 1);
                    let short = f
                        .strip_prefix("pg_try_")
                        .or_else(|| f.strip_prefix("pg_"))
                        .unwrap_or(f);
                    self.advisory.push(format!("{short}/{args}"));
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// `(` 위치에서 시작하는 호출의 최상위 인자 개수.
    fn arg_count(&self, open: usize) -> usize {
        let mut depth = 0;
        let mut args = 1;
        let mut empty = true;
        for t in &self.t[open..] {
            match t {
                Tok::Punct('(') => depth += 1,
                Tok::Punct(')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Punct(',') if depth == 1 => args += 1,
                _ => {
                    if depth >= 1 {
                        empty = false;
                    }
                }
            }
        }
        if empty { 0 } else { args }
    }

    /// `FOR UPDATE|NO KEY UPDATE|SHARE|KEY SHARE [OF 테이블들] [NOWAIT|SKIP LOCKED]`.
    fn lock_clause(&mut self, i: usize) {
        let mut j = i + 1;
        let strength = match (self.w(j), self.w(j + 1), self.w(j + 2)) {
            (Some("update"), _, _) => {
                j += 1;
                "UPDATE"
            }
            (Some("no"), Some("key"), Some("update")) => {
                j += 3;
                "NO KEY UPDATE"
            }
            (Some("share"), _, _) => {
                j += 1;
                "SHARE"
            }
            (Some("key"), Some("share"), _) => {
                j += 2;
                "KEY SHARE"
            }
            _ => return, // FOR ORDINALITY, FOR PORTION 등
        };
        let mut of = Vec::new();
        if self.w(j) == Some("of") {
            j += 1;
            while let Some(w) = self.t.get(j).and_then(|t| match t {
                Tok::Word(w) | Tok::Quoted(w) => Some(w.clone()),
                _ => None,
            }) {
                of.push(w);
                j += 1;
                if self.t.get(j) == Some(&Tok::Punct(',')) {
                    j += 1;
                } else {
                    break;
                }
            }
        }
        let modifier = match (self.w(j), self.w(j + 1)) {
            (Some("nowait"), _) => " NOWAIT",
            (Some("skip"), Some("locked")) => " SKIP LOCKED",
            _ => "",
        };
        self.locks
            .push((format!("FOR {strength}"), of, modifier.to_string()));
    }
}

/// 문장을 서명으로 줄인다. 앱 동작과 상관없는 잡음이면 None.
pub fn signature(sql: &str) -> Option<Sig> {
    let toks = lex(sql);
    let first = word(toks.first())?;
    // 트랜잭션 제어·세션 설정·준비문 관리는 잡음이다. 트랜잭션 경계는 호출 측(txlog)이 따로 읽는다.
    if matches!(
        first,
        "set"
            | "show"
            | "reset"
            | "discard"
            | "deallocate"
            | "begin"
            | "start"
            | "commit"
            | "end"
            | "rollback"
            | "abort"
            | "savepoint"
            | "release"
            | "listen"
            | "unlisten"
            | "notify"
            | "prepare"
            | "close"
    ) {
        return None;
    }
    let kind0 = match first {
        "select" | "with" | "values" | "table" => Kind::Select,
        "insert" => Kind::Insert,
        "update" => Kind::Update,
        "delete" => Kind::Delete,
        _ => Kind::Other,
    };
    let mut s = Scan {
        t: &toks,
        reads: BTreeSet::new(),
        writes: BTreeSet::new(),
        aliases: BTreeMap::new(),
        kind: Kind::Select,
        on_conflict: false,
        locks: Vec::new(),
        advisory: Vec::new(),
    };
    s.run();
    if kind0 == Kind::Other && s.kind == Kind::Select {
        // TRUNCATE, LOCK TABLE, COPY 등: 첫 단어를 이름으로 쓰고 이름 뒤 테이블을 모은다.
        let mut tables = BTreeSet::new();
        if let Some((name, _)) = s.table_name(1 + usize::from(s.w(1) == Some("table")), false) {
            tables.insert(name);
        }
        return Some(Sig {
            kind: Kind::Other,
            tables,
            locks: vec![first.to_uppercase()],
        });
    }
    let mut kind = s.kind;
    if kind == Kind::Insert && s.on_conflict {
        kind = Kind::Upsert;
    }
    let tables = if kind == Kind::Select {
        s.reads.clone()
    } else {
        s.writes.clone()
    };
    let mut locks: Vec<String> = Vec::new();
    for (text, of, modifier) in &s.locks {
        if of.is_empty() {
            // OF가 없으면 FROM의 모든 테이블을 잠근다. Hibernate는 `OF p1_0`을 붙이고 손 SQL은 붙이지 않으므로
            // 같은 표기로 맞춘다.
            let all = s.reads.iter().cloned().collect::<Vec<_>>().join(",");
            if all.is_empty() {
                locks.push(format!("{text}{modifier}"));
            } else {
                locks.push(format!("{text} OF {all}{modifier}"));
            }
        } else {
            let targets: BTreeSet<String> = of
                .iter()
                .map(|a| s.aliases.get(a).cloned().unwrap_or_else(|| a.clone()))
                .collect();
            let targets = targets.into_iter().collect::<Vec<_>>().join(",");
            locks.push(format!("{text} OF {targets}{modifier}"));
        }
    }
    locks.extend(s.advisory.iter().cloned());
    if kind == Kind::Select && tables.is_empty() && locks.is_empty() {
        return None; // SELECT 1, SELECT $1, SELECT version() 같은 풀 검증·점검
    }
    // 시스템 카탈로그만 읽는 조회(pgjdbc 타입·메타데이터 조회)
    if kind == Kind::Select
        && locks.is_empty()
        && tables.iter().all(|t| {
            t.starts_with("pg_")
                || t.starts_with("pg_catalog.")
                || t.starts_with("information_schema.")
        })
    {
        return None;
    }
    Some(Sig {
        kind,
        tables,
        locks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(sql: &str) -> String {
        signature(sql).map_or("<잡음>".into(), |s| s.to_string())
    }

    #[test]
    fn 단순_조회는_종류와_테이블만_남긴다() {
        assert_eq!(
            sig("select p1_0.id,p1_0.title from posts p1_0 where p1_0.id=?"),
            "SELECT posts"
        );
        assert_eq!(
            sig("SELECT id, status::text AS status FROM users WHERE id = $1"),
            "SELECT users"
        );
    }

    #[test]
    fn 조인과_쉼표_목록의_테이블을_모두_모은다() {
        assert_eq!(
            sig(
                "select p.id from posts p left join photos ph on ph.post_id=p.id join users u on u.id=p.user_id"
            ),
            "SELECT photos,posts,users"
        );
        assert_eq!(
            sig("select * from a x, b y where x.id = y.id"),
            "SELECT a,b"
        );
    }

    #[test]
    fn 하위_질의와_함수_호출은_테이블이_아니다() {
        assert_eq!(
            sig("select * from (select id from posts) s join unnest($1::uuid[]) u on true"),
            "SELECT posts"
        );
    }

    #[test]
    fn 쓰기_종류를_구분한다() {
        assert_eq!(
            sig("insert into posts (id,title) values (?,?)"),
            "INSERT posts"
        );
        assert_eq!(sig("update posts set title=? where id=?"), "UPDATE posts");
        assert_eq!(sig("UPDATE ONLY public.posts SET a = $1"), "UPDATE posts");
        assert_eq!(
            sig("delete from post_likes where post_id = $1"),
            "DELETE post_likes"
        );
        assert_eq!(
            sig("INSERT INTO post_likes (a,b) VALUES ($1,$2) ON CONFLICT DO NOTHING"),
            "UPSERT post_likes"
        );
        assert_eq!(
            sig("INSERT INTO t (a) VALUES ($1) ON CONFLICT (a) DO UPDATE SET b = 1"),
            "UPSERT t"
        );
    }

    #[test]
    fn 쓰기_문장의_하위_조회_테이블은_대상에_넣지_않는다() {
        assert_eq!(
            sig(
                "update posts set like_count = (select count(*) from post_likes where post_id = $1) where id = $1"
            ),
            "UPDATE posts"
        );
        assert_eq!(
            sig("insert into t (a) select a from s where s.x = $1"),
            "INSERT t"
        );
    }

    #[test]
    fn cte_안의_쓰기를_찾는다() {
        // 쓰기가 둘 이상이면 가장 센 종류(DELETE)에 쓴 테이블을 모두 모은다.
        assert_eq!(
            sig(
                "WITH moved AS (DELETE FROM a WHERE id = $1 RETURNING *) INSERT INTO b SELECT * FROM moved"
            ),
            "DELETE a,b"
        );
        assert_eq!(
            sig("with x as (update a set n = 1 returning id) select id from x"),
            "UPDATE a"
        );
    }

    #[test]
    fn 잠금_종류를_구분한다() {
        assert_eq!(
            sig("select u1_0.id from users u1_0 where u1_0.id=? for no key update"),
            "SELECT users FOR NO KEY UPDATE OF users"
        );
        assert_eq!(
            sig("select * from users where id=$1 FOR UPDATE"),
            "SELECT users FOR UPDATE OF users"
        );
        assert_eq!(
            sig("select * from users for share"),
            "SELECT users FOR SHARE OF users"
        );
        assert_eq!(
            sig("select * from users for key share"),
            "SELECT users FOR KEY SHARE OF users"
        );
        assert_eq!(
            sig("select n.id from notifications n where n.x=1 limit 10 FOR UPDATE SKIP LOCKED"),
            "SELECT notifications FOR UPDATE OF notifications SKIP LOCKED"
        );
    }

    #[test]
    fn of가_없으면_from의_모든_테이블을_잠근다() {
        assert_eq!(
            sig("select * from a join b on a.id=b.id for update"),
            "SELECT a,b FOR UPDATE OF a,b"
        );
        assert_eq!(
            sig("select * from a join b on a.id=b.id for update of a"),
            "SELECT a,b FOR UPDATE OF a"
        );
    }

    #[test]
    fn of_뒤_별칭은_테이블로_바꾼다() {
        assert_eq!(
            sig(
                "SELECT p.id FROM posts p JOIN photos ph ON ph.post_id = p.id WHERE p.id = $1 FOR SHARE OF p, ph"
            ),
            "SELECT photos,posts FOR SHARE OF photos,posts"
        );
        assert_eq!(
            sig(
                "select p1_0.id from posts p1_0 left join photos p2_0 on p2_0.post_id=p1_0.id for update of p1_0"
            ),
            "SELECT photos,posts FOR UPDATE OF posts"
        );
    }

    #[test]
    fn on_update_do_update는_잠금이_아니다() {
        assert_eq!(
            sig("INSERT INTO t (a) VALUES ($1) ON CONFLICT (a) DO UPDATE SET b = 1"),
            "UPSERT t"
        );
    }

    #[test]
    fn 어드바이저리_잠금은_인자_수로_구분한다() {
        assert_eq!(
            sig("SELECT pg_advisory_xact_lock(hashtext(CAST($1::uuid AS text)))"),
            "SELECT advisory_xact_lock/1"
        );
        assert_eq!(
            sig("SELECT pg_advisory_xact_lock(1, hashtext(?))"),
            "SELECT advisory_xact_lock/2"
        );
    }

    #[test]
    fn 풀_검증과_세션_설정은_잡음이다() {
        for s in [
            "SELECT $1",
            "SELECT 1",
            "select 1",
            "",
            ";",
            "SET extra_float_digits = 3",
            "SHOW TRANSACTION ISOLATION LEVEL",
            "BEGIN",
            "COMMIT",
            "DISCARD ALL",
            "select version()",
        ] {
            assert_eq!(sig(s), "<잡음>", "{s}");
        }
    }

    #[test]
    fn 시스템_카탈로그_조회는_잡음이다() {
        assert_eq!(
            sig(
                "SELECT t.typname, t.oid FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON (t.typnamespace = n.oid) WHERE n.nspname != 'pg_toast'"
            ),
            "<잡음>"
        );
        assert_eq!(
            sig(
                "SELECT n.nspname = ANY(current_schemas(true)), n.nspname, t.typname FROM pg_type t JOIN pg_namespace n ON t.typnamespace = n.oid WHERE t.oid = $1"
            ),
            "<잡음>"
        );
        assert_eq!(sig("SELECT * FROM information_schema.tables"), "<잡음>");
    }

    #[test]
    fn 문자열_주석_따옴표_식별자_안의_키워드는_무시한다() {
        assert_eq!(
            sig(
                "select 'for update from nothing' as x, \"from\" from posts -- for update\n where id = $1 /* delete from y */"
            ),
            "SELECT posts"
        );
        assert_eq!(sig("select $$ from evil $$ from posts"), "SELECT posts");
    }

    #[test]
    fn 스키마가_붙은_이름과_따옴표_이름을_정규화한다() {
        assert_eq!(sig("select * from public.\"posts\" p"), "SELECT posts");
    }

    #[test]
    fn 줄바꿈과_공백이_달라도_같은_서명이다() {
        assert_eq!(
            sig("SELECT id\n  FROM users\n WHERE id = $1\n   FOR UPDATE"),
            sig("select u1_0.id from users u1_0 where u1_0.id=? for update")
        );
    }

    #[test]
    fn truncate_같은_명령은_other로_남긴다() {
        assert_eq!(sig("TRUNCATE TABLE posts"), "OTHER posts TRUNCATE");
    }
}
