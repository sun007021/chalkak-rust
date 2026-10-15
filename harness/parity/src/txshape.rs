//! 트랜잭션 모양 비교: Java(Spring @Transactional + Hibernate + pgjdbc)와 Rust가 같은 요청에 대해
//! 같은 트랜잭션 경계·잠금으로 DB를 쓰는지 판정하고 표로 낸다.
//!
//! 판정(단계 = 시나리오의 요청 하나):
//! - 같음: 쓰기·잠금 문장 집합이 같은 트랜잭션 묶음에 들어 있고 잠금 순서·격리 수준·종료(COMMIT/ROLLBACK)가 같으며,
//!   읽기 문장도 같은 순서로 같은 묶음 모양이다.
//! - 동등(설명 필요): 쓰기·잠금은 같고 읽기만 다르다. 읽기 전용 단계에서 Java가 `BEGIN READ ONLY`로 SELECT 여러 개를
//!   묶고 Rust가 자동 커밋으로 보내는 경우(READ COMMITTED에서는 문장마다 스냅샷이라 결과가 같다. 한 트랜잭션 안의 일관성
//!   차이는 남긴다), SELECT 개수·순서·대상 테이블 차이(Hibernate 지연 로딩 등), 쓰기 한 문장이 BEGIN/COMMIT 대신 자동
//!   커밋으로 나간 경우, 잠금 사이 쓰기 위치 차이.
//! - 다름: 쓰기가 다른 트랜잭션으로 나뉘었거나(원자성 차이), 쓰기·잠금 문장 집합이 다르거나, 잠금 종류·대상·순서가
//!   다르거나, 한쪽만 잠금, 격리 수준이 다르거나, 한쪽만 ROLLBACK.

use crate::sqlsig::Sig;
use crate::txlog::{End, Tx};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Same,
    Equivalent,
    Different,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Same => "같음",
            Verdict::Equivalent => "동등",
            Verdict::Different => "다름",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepCmp {
    pub verdict: Verdict,
    /// 다름·동등의 근거(사람이 읽는 문장)
    pub reasons: Vec<String>,
}

/// 쓰기나 잠금을 하는 트랜잭션(또는 자동 커밋 문장) 하나.
struct Unit<'a> {
    tx: &'a Tx,
    /// 쓰기 문장(정렬: Hibernate는 flush 때 쓰기 순서를 바꾼다)
    writes: Vec<String>,
    /// 잠금 문장(실행 순서)
    locks: Vec<String>,
    /// 쓰기와 잠금을 실행 순서로 늘어놓고, 이어진 쓰기는 정렬해 한 덩어리로 묶은 열
    protocol: Vec<String>,
}

const LOCK_TAG: &str = "잠금:";

fn is_core(s: &Sig) -> bool {
    s.is_write() || s.is_lock()
}

fn unit(tx: &Tx) -> Option<Unit<'_>> {
    let core: Vec<&Sig> = tx.stmts.iter().filter(|s| is_core(s)).collect();
    if core.is_empty() {
        return None;
    }
    let mut writes: Vec<String> = Vec::new();
    let mut locks = Vec::new();
    let mut protocol = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let flush = |run: &mut Vec<String>, protocol: &mut Vec<String>| {
        if !run.is_empty() {
            run.sort();
            protocol.push(format!("쓰기{{{}}}", run.join("; ")));
            run.clear();
        }
    };
    for s in core {
        if s.is_write() {
            writes.push(s.to_string());
            run.push(s.to_string());
        } else {
            // 잠금 문장은 잠금 자체(종류·대상)로 식별한다. 같이 조인한 읽기 테이블은 잠금과 상관없다.
            let id = s.locks.join("; ");
            flush(&mut run, &mut protocol);
            locks.push(id.clone());
            protocol.push(id);
        }
    }
    flush(&mut run, &mut protocol);
    writes.sort();
    Some(Unit {
        tx,
        writes,
        locks,
        protocol,
    })
}

fn units(txs: &[Tx]) -> Vec<Unit<'_>> {
    txs.iter().filter_map(unit).collect()
}

fn multiset(items: impl IntoIterator<Item = String>) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for i in items {
        *m.entry(i).or_insert(0) += 1;
    }
    m
}

fn only_in(a: &BTreeMap<String, i64>, b: &BTreeMap<String, i64>) -> Vec<String> {
    let mut out = Vec::new();
    for (k, n) in a {
        let m = b.get(k).copied().unwrap_or(0);
        for _ in 0..(n - m).max(0) {
            out.push(k.clone());
        }
    }
    out
}

fn is_single_plain_write(u: &Unit<'_>) -> bool {
    u.tx.auto || (u.writes.len() == 1 && u.locks.is_empty())
}

/// 한 단계의 Java 쪽, Rust 쪽 트랜잭션 목록을 비교한다.
pub fn compare_step(java: &[Tx], rust: &[Tx]) -> StepCmp {
    let (ju, ru) = (units(java), units(rust));
    let mut different: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // 1) 쓰기·잠금 문장 집합
    // 잠금은 "잠금:" 표지를 붙여 쓰기와 한 집합에서 구분한다.
    let items = |us: &[Unit<'_>]| -> BTreeMap<String, i64> {
        multiset(us.iter().flat_map(|u| {
            u.writes
                .iter()
                .cloned()
                .chain(u.locks.iter().map(|l| format!("{LOCK_TAG}{l}")))
        }))
    };
    let (jset, rset) = (items(&ju), items(&ru));
    let set_equal = jset == rset;
    if !set_equal {
        let (jo, ro) = (only_in(&jset, &rset), only_in(&rset, &jset));
        let lock_word = |s: &String| s.starts_with(LOCK_TAG);
        let strip = |v: &[String]| -> Vec<String> {
            v.iter()
                .filter(|s| lock_word(s))
                .map(|s| s.trim_start_matches(LOCK_TAG).to_string())
                .collect()
        };
        let (jl, rl) = (strip(&jo), strip(&ro));
        if !jl.is_empty() || !rl.is_empty() {
            let msg = match (jl.is_empty(), rl.is_empty()) {
                (false, true) => format!("한쪽만 잠금(Java만): {}", join(&jl)),
                (true, false) => format!("한쪽만 잠금(Rust만): {}", join(&rl)),
                _ => format!(
                    "잠금 종류·대상 다름: Java {} / Rust {}",
                    join(&jl),
                    join(&rl)
                ),
            };
            different.push(msg);
        }
        let jw: Vec<&String> = jo.iter().filter(|s| !lock_word(s)).collect();
        let rw: Vec<&String> = ro.iter().filter(|s| !lock_word(s)).collect();
        if !jw.is_empty() || !rw.is_empty() {
            different.push(format!(
                "쓰기 문장 집합 다름: Java만 [{}] / Rust만 [{}]",
                join(&jw),
                join(&rw)
            ));
        }
    } else {
        // 2) 묶음(트랜잭션 분할)
        let key = |u: &Unit<'_>| {
            let mut all: Vec<String> = u.writes.iter().chain(&u.locks).cloned().collect();
            all.sort();
            all.join(" | ")
        };
        let jk: Vec<String> = ju.iter().map(key).collect();
        let rk: Vec<String> = ru.iter().map(key).collect();
        if jk != rk {
            if multiset(jk.iter().cloned()) == multiset(rk.iter().cloned()) {
                different.push(format!(
                    "쓰기 트랜잭션 순서 다름: Java {} / Rust {}",
                    show_keys(&jk),
                    show_keys(&rk)
                ));
            } else {
                different.push(format!(
                    "쓰기가 다른 트랜잭션으로 나뉨(원자성 차이): Java {} / Rust {}",
                    show_keys(&jk),
                    show_keys(&rk)
                ));
            }
        } else {
            // 3) 같은 묶음끼리 짝지어 세부 비교
            for (j, r) in ju.iter().zip(&ru) {
                compare_unit(j, r, &mut different, &mut notes);
            }
        }
    }

    // 4) 한쪽만 ROLLBACK(쓰기·잠금 트랜잭션 기준. 읽기만 한 트랜잭션의 롤백은 결과에 영향이 없다)
    let jr = ju.iter().filter(|u| u.tx.end == End::Rollback).count();
    let rr = ru.iter().filter(|u| u.tx.end == End::Rollback).count();
    if jr != rr && set_equal && different.iter().all(|d| !d.contains("ROLLBACK")) {
        let msg = if jr > 0 && rr == 0 {
            "한쪽만 ROLLBACK(Java만)".to_string()
        } else if rr > 0 && jr == 0 {
            "한쪽만 ROLLBACK(Rust만)".to_string()
        } else {
            format!("ROLLBACK 횟수 다름: Java {jr} / Rust {rr}")
        };
        different.push(msg);
    }

    // 5) 읽기
    compare_reads(java, rust, &mut notes);

    let verdict = if !different.is_empty() {
        Verdict::Different
    } else if !notes.is_empty() {
        Verdict::Equivalent
    } else {
        Verdict::Same
    };
    let mut reasons = different;
    reasons.extend(notes);
    StepCmp { verdict, reasons }
}

fn join<S: AsRef<str>>(v: &[S]) -> String {
    v.iter().map(AsRef::as_ref).collect::<Vec<_>>().join("; ")
}

fn show_keys(k: &[String]) -> String {
    format!(
        "[{}]",
        k.iter()
            .map(|s| format!("({s})"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn compare_unit(j: &Unit<'_>, r: &Unit<'_>, different: &mut Vec<String>, notes: &mut Vec<String>) {
    if j.locks != r.locks {
        let (a, b) = (
            multiset(j.locks.iter().cloned()),
            multiset(r.locks.iter().cloned()),
        );
        if a == b {
            different.push(format!(
                "잠금 순서 다름: Java [{}] / Rust [{}]",
                j.locks.join("; "),
                r.locks.join("; ")
            ));
        } else {
            different.push(format!(
                "잠금 종류·대상 다름: Java [{}] / Rust [{}]",
                j.locks.join("; "),
                r.locks.join("; ")
            ));
        }
    } else if j.protocol != r.protocol {
        notes.push(format!(
            "잠금 사이 쓰기 위치 다름(잠금 순서는 같음): Java {} / Rust {}",
            j.protocol.join(" > "),
            r.protocol.join(" > ")
        ));
    }
    if j.tx.isolation != r.tx.isolation {
        different.push(format!(
            "격리 수준 다름: Java {} / Rust {}",
            j.tx.isolation, r.tx.isolation
        ));
    }
    let (je, re) = (j.tx.end, r.tx.end);
    if je != re {
        match (je, re) {
            (End::Rollback, End::Commit | End::Auto) => {
                different.push("한쪽만 ROLLBACK(Java만)".into());
            }
            (End::Commit | End::Auto, End::Rollback) => {
                different.push("한쪽만 ROLLBACK(Rust만)".into());
            }
            (End::Commit, End::Auto) | (End::Auto, End::Commit) => {
                if is_single_plain_write(j) && is_single_plain_write(r) {
                    notes.push(format!(
                        "쓰기 한 문장: Java {} / Rust {} (효과 같음)",
                        start_name(j.tx),
                        start_name(r.tx)
                    ));
                } else {
                    different.push(format!(
                        "트랜잭션 경계 다름: Java {} / Rust {}",
                        start_name(j.tx),
                        start_name(r.tx)
                    ));
                }
            }
            _ => different.push(format!(
                "트랜잭션이 닫히는 방식 다름: Java {je:?} / Rust {re:?}"
            )),
        }
    }
    if j.tx.read_only != r.tx.read_only {
        different.push(format!(
            "쓰기 트랜잭션의 READ ONLY 여부 다름: Java {} / Rust {}",
            j.tx.read_only, r.tx.read_only
        ));
    }
}

fn start_name(tx: &Tx) -> &'static str {
    if tx.auto {
        "자동 커밋"
    } else if tx.read_only {
        "BEGIN READ ONLY"
    } else {
        "BEGIN"
    }
}

/// 읽기만 하는 문장(잠그지 않는 SELECT)의 차이는 동등으로 남긴다.
fn compare_reads(java: &[Tx], rust: &[Tx], notes: &mut Vec<String>) {
    let reads = |txs: &[Tx]| -> Vec<String> {
        txs.iter()
            .flat_map(|t| t.stmts.iter())
            .filter(|s| s.is_plain_read())
            .map(ToString::to_string)
            .collect()
    };
    let (jr, rr) = (reads(java), reads(rust));
    // 읽기 전용 목적의 묶음: 쓰기·잠금이 없고 SELECT가 둘 이상인 트랜잭션. (문장 수, READ ONLY 표시)
    let groups = |txs: &[Tx]| -> Vec<(usize, bool)> {
        txs.iter()
            .filter(|t| !t.auto && t.stmts.iter().all(Sig::is_plain_read) && t.stmts.len() >= 2)
            .map(|t| (t.stmts.len(), t.read_only))
            .collect()
    };
    let (jg, rg) = (groups(java), groups(rust));
    if jg != rg {
        let fmt = |g: &[(usize, bool)]| -> String {
            if g.is_empty() {
                "묶음 없음(자동 커밋)".into()
            } else {
                g.iter()
                    .map(|(n, ro)| {
                        format!(
                            "{}에 SELECT {n}개",
                            if *ro { "BEGIN READ ONLY" } else { "BEGIN" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        notes.push(format!(
            "읽기 묶음 다름: Java {} / Rust {}. READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다",
            fmt(&jg),
            fmt(&rg)
        ));
    }
    if jr != rr {
        let (jm, rm) = (multiset(jr.iter().cloned()), multiset(rr.iter().cloned()));
        if jr.len() != rr.len() {
            let jo = only_in(&jm, &rm);
            let ro = only_in(&rm, &jm);
            notes.push(format!(
                "SELECT 개수 다름: Java {} / Rust {} (Java만 [{}], Rust만 [{}])",
                jr.len(),
                rr.len(),
                join(&jo),
                join(&ro)
            ));
        } else if jm == rm {
            notes.push("SELECT 순서 다름".into());
        } else {
            notes.push(format!(
                "SELECT 대상 다름: Java만 [{}] / Rust만 [{}]",
                join(&only_in(&jm, &rm)),
                join(&only_in(&rm, &jm))
            ));
        }
    }
}

/// 트랜잭션 하나를 한 줄로 쓴다. 이어진 같은 문장은 `×n`으로 줄인다.
pub fn describe(tx: &Tx) -> String {
    let mut parts: Vec<(String, usize)> = Vec::new();
    for s in &tx.stmts {
        let t = s.to_string();
        match parts.last_mut() {
            Some((last, n)) if *last == t => *n += 1,
            _ => parts.push((t, 1)),
        }
    }
    let body = parts
        .iter()
        .map(|(t, n)| {
            if *n > 1 {
                format!("{t} ×{n}")
            } else {
                t.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    if tx.auto {
        return format!("자동커밋: {body}{}", if tx.failed { " (오류)" } else { "" });
    }
    let mut head = String::from("BEGIN");
    if tx.read_only {
        head.push_str(" RO");
    }
    if tx.isolation != crate::txlog::DEFAULT_ISOLATION {
        head.push_str(&format!(" {}", tx.isolation));
    }
    let end = match tx.end {
        End::Commit => "COMMIT",
        End::Rollback => "ROLLBACK",
        End::Open => "(열림)",
        End::Auto => "",
    };
    format!(
        "{head}: {body} → {end}{}",
        if tx.failed && tx.end != End::Rollback {
            " (오류)"
        } else {
            ""
        }
    )
}

pub fn describe_all(txs: &[Tx]) -> String {
    txs.iter().map(describe).collect::<Vec<_>>().join("\n")
}

/// 시나리오 하나의 단계별 결과.
pub struct StepShape {
    pub name: String,
    pub java: Vec<Tx>,
    pub rust: Vec<Tx>,
    pub cmp: StepCmp,
}

pub struct ScenarioShape {
    pub key: String,
    pub name: String,
    /// 같은 시나리오의 두 응답·DB 상태 정규화 결과가 다르다(트랜잭션 모양 차이와 따로 알린다)
    pub response_diff: bool,
    pub steps: Vec<StepShape>,
}

impl ScenarioShape {
    pub fn count(&self, v: Verdict) -> usize {
        self.steps.iter().filter(|s| s.cmp.verdict == v).count()
    }

    /// 디버깅용 원문: 단계마다 Java·Rust 트랜잭션 목록과 판정.
    pub fn raw_text(&self) -> String {
        let mut out = format!("{} ({})\n", self.key, self.name);
        for st in &self.steps {
            out.push_str(&format!(
                "\n[{}] {}\n  Java:\n{}\n  Rust:\n{}\n",
                st.cmp.verdict.label(),
                st.name,
                indent(&describe_all(&st.java)),
                indent(&describe_all(&st.rust))
            ));
            for r in &st.cmp.reasons {
                out.push_str(&format!("  * {r}\n"));
            }
        }
        out
    }
}

fn indent(s: &str) -> String {
    if s.is_empty() {
        return "    (SQL 없음)".into();
    }
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', "<br>")
}

/// 결과 표(마크다운). `header`는 실행 조건 등 머리말에 넣을 줄들이다.
pub fn render_markdown(shapes: &[ScenarioShape], header: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# 트랜잭션 모양 비교: Java(Spring + Hibernate + pgjdbc) 대 Rust\n\n");
    for h in header {
        out.push_str(h);
        out.push('\n');
    }
    out.push('\n');
    let total: usize = shapes.iter().map(|s| s.steps.len()).sum();
    let n = |v: Verdict| shapes.iter().map(|s| s.count(v)).sum::<usize>();
    out.push_str("## 요약\n\n");
    out.push_str(&format!(
        "시나리오 {}개, 단계(요청) {total}개: 같음 {}, 동등 {}, 다름 {}.\n\n",
        shapes.len(),
        n(Verdict::Same),
        n(Verdict::Equivalent),
        n(Verdict::Different)
    ));
    out.push_str("| 시나리오 | 단계 | 같음 | 동등 | 다름 | 응답·DB 일치 |\n| --- | ---: | ---: | ---: | ---: | --- |\n");
    for s in shapes {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            s.key,
            s.steps.len(),
            s.count(Verdict::Same),
            s.count(Verdict::Equivalent),
            s.count(Verdict::Different),
            if s.response_diff {
                "**다름**"
            } else {
                "일치"
            }
        ));
    }
    out.push('\n');

    out.push_str("## 다름\n\n");
    // 같은 사유 묶음(사유 문장이 모두 같은 것)끼리 모아 한 번만 보여 준다. 단계별 원문은 아래 "전체 단계"에 있다.
    type Group<'a> = (Vec<String>, Vec<(&'a ScenarioShape, &'a StepShape)>);
    let mut groups: Vec<Group<'_>> = Vec::new();
    for s in shapes {
        for st in s
            .steps
            .iter()
            .filter(|x| x.cmp.verdict == Verdict::Different)
        {
            match groups.iter_mut().find(|(r, _)| *r == st.cmp.reasons) {
                Some((_, v)) => v.push((s, st)),
                None => groups.push((st.cmp.reasons.clone(), vec![(s, st)])),
            }
        }
    }
    groups.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    if groups.is_empty() {
        out.push_str("없음.\n\n");
    }
    for (i, (reasons, steps)) in groups.iter().enumerate() {
        out.push_str(&format!("### 다름 {}: {}단계\n\n", i + 1, steps.len()));
        for r in reasons {
            out.push_str(&format!("- {r}\n"));
        }
        let mut by_scenario: Vec<(&str, usize)> = Vec::new();
        for (s, _) in steps {
            match by_scenario.iter_mut().find(|(k, _)| *k == s.key) {
                Some((_, n)) => *n += 1,
                None => by_scenario.push((&s.key, 1)),
            }
        }
        out.push_str(&format!(
            "- 시나리오: {}\n",
            by_scenario
                .iter()
                .map(|(k, n)| format!("`{k}` {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let (s, st) = steps[0];
        out.push_str(&format!(
            "\n예: `{}` / {}\n\nJava:\n```\n{}\n```\nRust:\n```\n{}\n```\n\n",
            s.key,
            st.name,
            describe_all(&st.java),
            describe_all(&st.rust)
        ));
    }

    out.push_str("## 동등 사유 집계\n\n");
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for s in shapes {
        for st in s
            .steps
            .iter()
            .filter(|x| x.cmp.verdict == Verdict::Equivalent)
        {
            for r in &st.cmp.reasons {
                let head = r.split([':', '(']).next().unwrap_or(r).trim().to_string();
                *kinds.entry(head).or_insert(0) += 1;
            }
        }
    }
    if kinds.is_empty() {
        out.push_str("없음.\n\n");
    } else {
        out.push_str("| 사유 | 단계 수 |\n| --- | ---: |\n");
        for (k, c) in &kinds {
            out.push_str(&format!("| {} | {c} |\n", cell(k)));
        }
        out.push('\n');
    }

    out.push_str("## 전체 단계\n\n");
    for s in shapes {
        out.push_str(&format!(
            "<details><summary><code>{}</code> {} (같음 {}, 동등 {}, 다름 {})</summary>\n\n",
            s.key,
            s.name,
            s.count(Verdict::Same),
            s.count(Verdict::Equivalent),
            s.count(Verdict::Different)
        ));
        out.push_str("| 단계 | 판정 | Java | Rust | 비고 |\n| --- | --- | --- | --- | --- |\n");
        for st in &s.steps {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                cell(&st.name),
                st.cmp.verdict.label(),
                cell(&describe_all(&st.java)),
                cell(&describe_all(&st.rust)),
                cell(&st.cmp.reasons.join(" / "))
            ));
        }
        out.push_str("\n</details>\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlsig::signature;
    use crate::txlog::DEFAULT_ISOLATION;

    /// `"S posts"`처럼 SQL 문장 목록에서 트랜잭션을 만든다. `auto`면 문장마다 자동 커밋 하나씩.
    fn tx(read_only: bool, end: End, sqls: &[&str]) -> Tx {
        Tx {
            step: 0,
            session: "s".into(),
            auto: false,
            read_only,
            isolation: DEFAULT_ISOLATION.into(),
            end,
            stmts: sqls.iter().filter_map(|s| signature(s)).collect(),
            failed: false,
        }
    }

    fn auto(sql: &str) -> Tx {
        Tx {
            auto: true,
            end: End::Auto,
            ..tx(false, End::Auto, &[sql])
        }
    }

    fn verdict(j: &[Tx], r: &[Tx]) -> Verdict {
        compare_step(j, r).verdict
    }

    const LOCK_U: &str = "select * from users where id=$1 for update";
    const LOCK_NK: &str = "select * from users where id=$1 for no key update";
    const UPD_U: &str = "update users set a=$1 where id=$2";
    const INS_P: &str = "insert into posts (id) values ($1)";
    const LOCK_P: &str = "select * from posts where id=$1 for update";

    #[test]
    fn 잠금은_같이_조인한_읽기_테이블과_무관하게_식별한다() {
        let j = [tx(
            false,
            End::Commit,
            &["select p.id from posts p for no key update of p", UPD_U],
        )];
        let r = [tx(
            false,
            End::Commit,
            &[
                "select p.id from posts p join photos ph on ph.id=p.photo_id for no key update of p",
                UPD_U,
            ],
        )];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Same, "{:?}", c.reasons);
    }

    #[test]
    fn 같은_모양이면_같음() {
        let j = [tx(false, End::Commit, &[LOCK_U, UPD_U])];
        let r = [tx(false, End::Commit, &[LOCK_U, UPD_U])];
        assert_eq!(verdict(&j, &r), Verdict::Same);
    }

    #[test]
    fn 쓰기_순서가_달라도_같은_묶음이면_같음() {
        // Hibernate는 flush에서 쓰기 순서를 바꾼다
        let j = [tx(false, End::Commit, &[INS_P, UPD_U])];
        let r = [tx(false, End::Commit, &[UPD_U, INS_P])];
        assert_eq!(verdict(&j, &r), Verdict::Same);
    }

    #[test]
    fn sql_없는_단계는_같음() {
        assert_eq!(verdict(&[], &[]), Verdict::Same);
    }

    #[test]
    fn 읽기_전용_묶음과_자동_커밋은_동등이다() {
        let j = [tx(
            true,
            End::Commit,
            &["select * from posts", "select * from users"],
        )];
        let r = [auto("select * from posts"), auto("select * from users")];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Equivalent);
        assert!(c.reasons.iter().any(|x| x.contains("읽기 묶음 다름")));
        assert!(c.reasons.iter().any(|x| x.contains("READ COMMITTED")));
    }

    #[test]
    fn 읽기_한_문장은_읽기_전용_묶음이어도_같음() {
        let j = [tx(true, End::Commit, &["select * from posts"])];
        let r = [auto("select * from posts")];
        assert_eq!(verdict(&j, &r), Verdict::Same);
    }

    #[test]
    fn select_개수와_순서_차이는_동등이다() {
        let j = [auto("select * from posts"), auto("select * from users")];
        let r = [auto("select * from posts")];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Equivalent);
        assert!(c.reasons[0].contains("SELECT 개수 다름"));
        let r2 = [auto("select * from users"), auto("select * from posts")];
        let c = compare_step(&j, &r2);
        assert_eq!(c.verdict, Verdict::Equivalent);
        assert!(c.reasons[0].contains("순서"));
    }

    #[test]
    fn 쓰기가_다른_트랜잭션으로_나뉘면_다름() {
        let j = [tx(false, End::Commit, &[UPD_U, INS_P])];
        let r = [
            tx(false, End::Commit, &[UPD_U]),
            tx(false, End::Commit, &[INS_P]),
        ];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(c.reasons[0].contains("원자성"));
    }

    #[test]
    fn 잠금_종류가_다르면_다름() {
        let j = [tx(false, End::Commit, &[LOCK_NK, UPD_U])];
        let r = [tx(false, End::Commit, &[LOCK_U, UPD_U])];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(
            c.reasons[0].contains("잠금 종류·대상 다름"),
            "{:?}",
            c.reasons
        );
    }

    #[test]
    fn 잠금_순서가_다르면_다름() {
        let j = [tx(false, End::Commit, &[LOCK_U, LOCK_P, UPD_U])];
        let r = [tx(false, End::Commit, &[LOCK_P, LOCK_U, UPD_U])];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(c.reasons[0].contains("잠금 순서 다름"));
    }

    #[test]
    fn 한쪽만_잠금이면_다름() {
        let j = [tx(false, End::Commit, &[LOCK_U, UPD_U])];
        let r = [tx(false, End::Commit, &[UPD_U])];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(c.reasons[0].contains("한쪽만 잠금(Java만)"));
        let c = compare_step(&r, &j);
        assert!(c.reasons[0].contains("한쪽만 잠금(Rust만)"));
    }

    #[test]
    fn 쓰기_문장_집합이_다르면_다름() {
        let j = [tx(false, End::Commit, &[UPD_U, INS_P])];
        let r = [tx(false, End::Commit, &[UPD_U])];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(c.reasons[0].contains("쓰기 문장 집합 다름"));
    }

    #[test]
    fn 격리_수준이_다르면_다름() {
        let j = [tx(false, End::Commit, &[UPD_U])];
        let mut r = [tx(false, End::Commit, &[UPD_U])];
        r[0].isolation = "SERIALIZABLE".into();
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(c.reasons[0].contains("격리 수준 다름"));
    }

    #[test]
    fn 한쪽만_롤백이면_다름() {
        let j = [tx(false, End::Rollback, &[UPD_U])];
        let r = [tx(false, End::Commit, &[UPD_U])];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(
            c.reasons
                .iter()
                .any(|x| x.contains("한쪽만 ROLLBACK(Java만)"))
        );
        let c = compare_step(&r, &j);
        assert!(
            c.reasons
                .iter()
                .any(|x| x.contains("한쪽만 ROLLBACK(Rust만)"))
        );
    }

    #[test]
    fn 읽기만_한_트랜잭션의_롤백은_무시한다() {
        let j = [tx(true, End::Rollback, &["select * from posts"])];
        let r = [auto("select * from posts")];
        assert_eq!(verdict(&j, &r), Verdict::Same);
    }

    #[test]
    fn 쓰기_한_문장이_자동_커밋이면_동등이다() {
        let j = [tx(false, End::Commit, &[UPD_U])];
        let r = [auto(UPD_U)];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Equivalent);
        assert!(c.reasons[0].contains("효과 같음"));
    }

    #[test]
    fn 잠금과_쓰기가_자동_커밋으로_나뉘면_다름() {
        let j = [tx(false, End::Commit, &[LOCK_U, UPD_U])];
        let r = [auto(LOCK_U), auto(UPD_U)];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Different);
        assert!(c.reasons[0].contains("원자성"));
    }

    #[test]
    fn 잠금_사이_쓰기_위치만_다르면_동등이다() {
        let j = [tx(false, End::Commit, &[LOCK_U, LOCK_P, UPD_U])];
        let r = [tx(false, End::Commit, &[LOCK_U, UPD_U, LOCK_P])];
        let c = compare_step(&j, &r);
        assert_eq!(c.verdict, Verdict::Equivalent);
        assert!(c.reasons[0].contains("쓰기 위치 다름"));
    }

    #[test]
    fn 쓰기_트랜잭션_안의_읽기_차이는_동등이다() {
        let j = [tx(false, End::Commit, &["select * from posts", UPD_U])];
        let r = [tx(false, End::Commit, &[UPD_U])];
        assert_eq!(verdict(&j, &r), Verdict::Equivalent);
    }

    #[test]
    fn 한_줄_설명() {
        let t = tx(
            true,
            End::Commit,
            &[
                "select * from posts",
                "select * from posts",
                "select * from users",
            ],
        );
        assert_eq!(
            describe(&t),
            "BEGIN RO: SELECT posts ×2; SELECT users → COMMIT"
        );
        assert_eq!(describe(&auto(UPD_U)), "자동커밋: UPDATE users");
        let mut t = tx(false, End::Commit, &[UPD_U]);
        t.isolation = "SERIALIZABLE".into();
        assert_eq!(describe(&t), "BEGIN SERIALIZABLE: UPDATE users → COMMIT");
    }

    #[test]
    fn 표를_그린다() {
        let step = StepShape {
            name: "조회".into(),
            java: vec![tx(false, End::Commit, &[LOCK_NK, UPD_U])],
            rust: vec![tx(false, End::Commit, &[LOCK_U, UPD_U])],
            cmp: compare_step(
                &[tx(false, End::Commit, &[LOCK_NK, UPD_U])],
                &[tx(false, End::Commit, &[LOCK_U, UPD_U])],
            ),
        };
        let md = render_markdown(
            &[ScenarioShape {
                key: "a__b".into(),
                name: "x".into(),
                response_diff: false,
                steps: vec![step],
            }],
            &["조건".into()],
        );
        assert!(md.contains("같음 0, 동등 0, 다름 1"));
        assert!(md.contains("### 다름 1: 1단계"));
        assert!(md.contains("예: `a__b` / 조회"));
        assert!(md.contains("FOR NO KEY UPDATE"));
    }
}
