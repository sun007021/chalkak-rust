//! 비교 대상 서버 하나(앱 + 전용 DB + 전용 시계 파일) 설정과 조작.

use crate::time;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// 시계를 맞춘 직후 요청을 보낼 때 앱 시각이 시나리오 시각보다 앞선 정도(초).
pub const CLOCK_LEAD: f64 = 1.1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub name: String,
    pub base_url: String,
    /// libpq 형식 연결 문자열.
    pub db_url: String,
    /// 앱과 DB 프로세스가 `FAKETIME_TIMESTAMP_FILE`로 읽는 파일. 여기에 오프셋을 쓴다.
    pub clock_file: PathBuf,
    /// 액세스 토큰 HS256 비밀키(hex). 앱의 `ACCESS_TOKEN_SECRET`과 같아야 한다.
    pub access_token_secret: String,
    /// 가짜 외부 서버(`harness/fakes`)의 관리 주소. 외부 연동을 쓰는 시나리오에 필요하다.
    pub fakes_url: Option<String>,
    /// 실행하는 동안 잡는 잠금 파일. 여러 작업이 같은 대상(lane)을 동시에 쓰지 않게 한다. 없으면 잠그지 않는다.
    pub lock_file: Option<PathBuf>,
    /// 시계 파일을 바꾼 뒤 기다리는 시간(ms). 생략하면 `PARITY_CLOCK_SETTLE_MS` 또는 1100.
    pub clock_settle_ms: Option<u64>,
    /// lane DB가 문장 로그를 쓰는 파일(호스트 경로). `TXLOG=1`로 띄운 lane에만 있다(`txshape` 명령이 읽는다).
    pub pg_log_file: Option<PathBuf>,
}

impl Target {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// 앱·DB의 현재 시각을 옮겨, 기다림(`settle`)이 끝나 요청을 보낼 때 앱 시각이 `epoch + CLOCK_LEAD`가 되게 한다.
    /// 오프셋은 소수 초로 쓴다(libfaketime은 `+12.345` 형식을 받는다). 대상마다 기다림이 달라도 요청 시점의 앱
    /// 시각은 같다. `CLOCK_LEAD`는 이전 방식(초 경계에 정수 오프셋을 쓰고 1100ms 기다림)과 같은 값이라 그때 녹화한
    /// golden과 시각이 같다. 반환값은 기다림이 끝난 실제 시각이다.
    pub fn set_clock(&self, epoch: i64) -> Result<f64, String> {
        let settle = self.settle_ms();
        let now = time::now_epoch();
        let offset = epoch as f64 + CLOCK_LEAD - (now + settle as f64 / 1000.0);
        if let Some(dir) = self.clock_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.clock_file.with_extension("tmp");
        std::fs::write(&tmp, format!("{offset:+.3}\n")).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.clock_file).map_err(|e| e.to_string())?;
        std::thread::sleep(std::time::Duration::from_millis(settle));
        Ok(time::now_epoch())
    }

    /// libfaketime이 `FAKETIME_CACHE_DURATION`으로 파일을 캐시하는 경우에 대비해 바뀐 시계가 반영될 때까지 기다린다.
    /// `FAKETIME_NO_CACHE=1`에서도 0으로 줄이면 큰 시각 이동(예: 12시간) 직후 Java가 빈 응답으로 연결을 끊는 경우가
    /// 있었다(2026-10-12). 대상 설정의 `clock_settle_ms`가 우선하고, 없으면 환경변수, 그것도 없으면 1100ms.
    pub fn settle_ms(&self) -> u64 {
        self.clock_settle_ms
            .or_else(|| {
                std::env::var("PARITY_CLOCK_SETTLE_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(1100)
    }

    /// `lock_file`이 있으면 잠금을 잡는다. `wait`가 거짓이면 이미 잡혀 있을 때 None을 낸다.
    /// 잠금은 돌려준 파일을 닫을 때(프로세스가 끝날 때 포함) 풀린다.
    pub fn lock(&self, wait: bool) -> Result<Option<std::fs::File>, String> {
        let Some(path) = &self.lock_file else {
            return Ok(Some(
                std::fs::File::open("/dev/null").map_err(|e| e.to_string())?,
            ));
        };
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if wait {
            file.lock()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            return Ok(Some(file));
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// 시계 파일을 "+0"(실제 시각)으로 되돌린다. 시나리오 시각으로 옮긴 채 두면 이후 DB를 직접 조회하는 사람이나
    /// 도구가 `now()`를 과거 시각으로 보게 된다. 실패해도 검증 결과와 무관하므로 무시한다.
    pub fn reset_clock(&self) {
        let _ = std::fs::write(&self.clock_file, "+0\n");
    }

    pub fn db(&self) -> Result<postgres::Client, String> {
        // 앱이 보낸 문장과 하네스 문장을 DB 문장 로그에서 구분하게 application_name을 붙인다(txshape).
        let url = format!(
            "{} application_name={}",
            self.db_url,
            crate::txlog::HARNESS_APP
        );
        postgres::Client::connect(&url, postgres::NoTls)
            .map_err(|e| format!("[{}] DB 연결 실패: {e}", self.name))
    }
}

/// 마이그레이션 이력을 뺀 모든 테이블을 비우고 시나리오 준비 SQL을 실행한다.
pub fn reset_db(db: &mut postgres::Client, setup: &[String]) -> Result<(), String> {
    let tables = user_tables(db)?;
    if !tables.is_empty() {
        let list = tables
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ");
        db.batch_execute(&format!("TRUNCATE TABLE {list} RESTART IDENTITY CASCADE"))
            .map_err(|e| format!("TRUNCATE 실패: {e}"))?;
    }
    for sql in setup {
        db.batch_execute(sql)
            .map_err(|e| format!("준비 SQL 실패: {e}\n{sql}"))?;
    }
    Ok(())
}

pub fn user_tables(db: &mut postgres::Client) -> Result<Vec<String>, String> {
    let rows = db
        .query(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public' \
             AND tablename <> 'flyway_schema_history' ORDER BY tablename",
            &[],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

/// 테이블별 행을 JSON으로 읽는다. 정렬과 정규화는 호출 측이 한다.
pub fn dump(db: &mut postgres::Client) -> Result<Vec<(String, Vec<serde_json::Value>)>, String> {
    let mut out = Vec::new();
    for t in user_tables(db)? {
        let rows = db
            .query(&format!("SELECT to_jsonb(x)::text FROM \"{t}\" x"), &[])
            .map_err(|e| format!("{t} 덤프 실패: {e}"))?;
        let mut values = Vec::with_capacity(rows.len());
        for r in rows {
            let text: String = r.get(0);
            values.push(serde_json::from_str(&text).map_err(|e| e.to_string())?);
        }
        out.push((t, values));
    }
    Ok(out)
}
