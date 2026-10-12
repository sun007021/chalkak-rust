//! 비교 대상 서버 하나(앱 + 전용 DB + 전용 시계 파일) 설정과 조작.

use crate::time;
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
}

impl Target {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// 앱·DB의 현재 시각을 `epoch`로 맞춘다. 실제 시각과의 차이를 초 단위 오프셋으로 쓰고,
    /// 실제 시각의 소수부가 0에 가까울 때 써서 앱 시각이 `epoch + 0.0x`초에서 출발하게 한다.
    pub fn set_clock(&self, epoch: i64) -> Result<(), String> {
        let now = time::now_epoch();
        let wait = 1.0 - now.fract();
        std::thread::sleep(std::time::Duration::from_secs_f64(wait));
        let now = time::now_epoch().floor() as i64;
        let offset = epoch - now;
        if let Some(dir) = self.clock_file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.clock_file.with_extension("tmp");
        std::fs::write(&tmp, format!("{offset:+}\n")).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.clock_file).map_err(|e| e.to_string())?;
        settle();
        Ok(())
    }

    pub fn db(&self) -> Result<postgres::Client, String> {
        postgres::Client::connect(&self.db_url, postgres::NoTls)
            .map_err(|e| format!("[{}] DB 연결 실패: {e}", self.name))
    }
}

/// libfaketime이 `FAKETIME_CACHE_DURATION`으로 파일을 캐시하는 경우에 대비해 바뀐 시계가 반영될 때까지 기다린다.
/// `FAKETIME_NO_CACHE=1`에서도 0으로 줄이면 큰 시각 이동(예: 12시간) 직후 Java가 빈 응답으로 연결을 끊는 경우가
/// 있었다(2026-10-12). 기본값 1100ms를 유지한다.
fn settle() {
    let ms = std::env::var("PARITY_CLOCK_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1100);
    std::thread::sleep(std::time::Duration::from_millis(ms));
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
