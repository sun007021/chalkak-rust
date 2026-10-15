//! lane DB의 문장 로그에서 시나리오 단계별 트랜잭션을 뽑는다.
//!
//! 단계(요청) 경계는 하네스 세션(`application_name = chalkak-harness`)이 요청 앞뒤로 보내는 표식 SQL
//! `SELECT 'txshape:<실행 id>:b<단계>'`로 나눈다. 로그 파일은 한 DB의 모든 세션이 같이 쓰므로 표식과 앱 문장의
//! 로그 순서가 실제 실행 순서다. 표식 사이에 앱 세션이 보낸 문장만 그 단계 것으로 센다.

use crate::target::Target;
use crate::txlog::{self, Entry, Msg, Tx};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Tracer {
    path: PathBuf,
    offset: u64,
    run: String,
}

impl Tracer {
    /// 대상 lane의 로그 파일 끝을 시작 위치로 잡는다. lane이 `TXLOG=1`로 뜨지 않았으면 오류.
    pub fn start(target: &Target) -> Result<Tracer, String> {
        let path = target.pg_log_file.clone().ok_or_else(|| {
            format!(
                "[{}] 대상 설정에 pg_log_file이 없습니다. lane을 TXLOG=1로 띄우세요(harness/scripts/lane.sh)",
                target.name
            )
        })?;
        let offset = std::fs::metadata(&path)
            .map_err(|e| format!("[{}] {}: {e}", target.name, path.display()))?
            .len();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Ok(Tracer {
            path,
            offset,
            run: format!("{nanos:x}"),
        })
    }

    fn marker_sql(&self, tag: &str) -> String {
        format!("SELECT 'txshape:{}:{tag}'", self.run)
    }

    /// 표식 SQL을 하네스 연결로 보낸다.
    pub fn mark(&self, db: &mut postgres::Client, tag: &str) -> Result<(), String> {
        db.batch_execute(&self.marker_sql(tag))
            .map_err(|e| format!("표식 SQL 실패: {e}"))
    }

    /// 마지막 표식을 보내고 로그에 나타날 때까지 기다린 뒤, 단계별 트랜잭션 목록을 낸다.
    pub fn finish(self, db: &mut postgres::Client, steps: usize) -> Result<Vec<Vec<Tx>>, String> {
        self.mark(db, "f")?;
        let end_marker = self.marker_sql("f");
        let deadline = Instant::now() + Duration::from_secs(15);
        let text = loop {
            let text = self.read_new()?;
            if text.contains(&end_marker) {
                break text;
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "{}: 끝 표식이 로그에 나타나지 않습니다(logging_collector 지연?)",
                    self.path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let entries = txlog::parse_log(&text);
        Ok(split_steps(&entries, &self.run, steps))
    }

    /// 시작 위치 이후의 완전한 줄까지 읽는다.
    fn read_new(&self) -> Result<String, String> {
        let mut f =
            std::fs::File::open(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        f.seek(SeekFrom::Start(self.offset))
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// 표식으로 앱 기록을 단계별로 나누고 트랜잭션으로 자른다. 첫 표식 앞과 끝 표식 뒤의 기록은 버린다.
pub fn split_steps(entries: &[Entry], run: &str, steps: usize) -> Vec<Vec<Tx>> {
    let prefix = format!("SELECT 'txshape:{run}:");
    let mut current: Option<usize> = None;
    let mut tagged: Vec<(usize, &Entry)> = Vec::new();
    for e in entries {
        if e.is_harness() {
            if let Msg::Stmt(sql) = &e.msg
                && let Some(rest) = sql.strip_prefix(&prefix)
            {
                let tag = rest.trim_end_matches('\'');
                if tag == "f" {
                    break;
                }
                if let Some(n) = tag.strip_prefix('b').and_then(|n| n.parse().ok()) {
                    current = Some(n);
                }
            }
            continue;
        }
        if let Some(step) = current {
            tagged.push((step, e));
        }
    }
    let mut out: Vec<Vec<Tx>> = (0..steps).map(|_| Vec::new()).collect();
    for tx in txlog::build_txs(&tagged) {
        if let Some(slot) = out.get_mut(tx.step) {
            slot.push(tx);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, app: &str, sql: &str) -> Entry {
        Entry {
            session: session.into(),
            app: app.into(),
            msg: Msg::Stmt(sql.into()),
        }
    }

    #[test]
    fn 표식_사이_앱_문장을_단계별로_나눈다() {
        let h = txlog::HARNESS_APP;
        let es = vec![
            entry("h", h, "SELECT 'txshape:r1:b0'"),
            entry("a", "", "BEGIN"),
            entry("a", "", "update posts set a=1"),
            entry("h", h, "TRUNCATE x"),
            entry("a", "", "COMMIT"),
            entry("h", h, "SELECT 'txshape:r1:b1'"),
            entry("b", "PostgreSQL JDBC Driver", "select * from posts"),
            entry("h", h, "SELECT 'txshape:r1:f'"),
            entry("a", "", "update posts set a=2"),
        ];
        let steps = split_steps(&es, "r1", 2);
        assert_eq!(steps[0].len(), 1);
        assert_eq!(steps[0][0].stmts.len(), 1);
        assert_eq!(steps[1].len(), 1);
        assert!(steps[1][0].auto);
    }

    #[test]
    fn 다른_실행의_표식은_단계를_바꾸지_않는다() {
        let h = txlog::HARNESS_APP;
        let es = vec![
            entry("h", h, "SELECT 'txshape:r1:b0'"),
            entry("h", h, "SELECT 'txshape:other:b1'"),
            entry("a", "", "select * from posts"),
            entry("h", h, "SELECT 'txshape:r1:f'"),
        ];
        let steps = split_steps(&es, "r1", 2);
        assert_eq!(steps[0].len(), 1);
        assert!(steps[1].is_empty());
    }

    #[test]
    fn 단계_경계를_넘는_트랜잭션은_시작_단계에_속한다() {
        let h = txlog::HARNESS_APP;
        let es = vec![
            entry("h", h, "SELECT 'txshape:r:b0'"),
            entry("a", "", "BEGIN"),
            entry("a", "", "update posts set a=1"),
            entry("h", h, "SELECT 'txshape:r:b1'"),
            entry("a", "", "COMMIT"),
            entry("h", h, "SELECT 'txshape:r:f'"),
        ];
        let steps = split_steps(&es, "r", 2);
        assert_eq!(steps[0].len(), 1);
        assert!(steps[1].is_empty());
    }
}
