//! 시나리오 파일(TOML) 모델.
//!
//! ```toml
//! name = "주제: 날짜로 조회"
//! tags = ["topic"]
//! clock = "2026-10-12T09:00:00+09:00"   # 시나리오 시작 시 앱·DB의 현재 시각
//!
//! [setup]
//! sql = ["INSERT INTO topics (...) VALUES (...)"]
//!
//! [[step]]
//! name = "조회"
//! method = "GET"
//! path = "/api/v1/topics?date=2026-10-12"
//! auth = { user = "11111111-1111-7111-8111-111111111111" }   # 하네스가 액세스 토큰을 발급
//! headers = { "Accept-Language" = "ko" }
//! body = '''{"title": "x"}'''
//! capture = { topicId = "id" }        # 응답 본문 경로 → 변수, 이후 ${topicId}로 사용
//! opaque = ["refreshToken"]           # 무작위 불투명 값: 길이만 비교
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// RFC 3339. 생략하면 기본 시각을 쓴다.
    pub clock: Option<String>,
    #[serde(default)]
    pub setup: Setup,
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
    /// 시나리오 끝에 DB 상태를 비교할지. 기본은 비교한다.
    #[serde(default = "default_true")]
    pub compare_db: bool,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    #[serde(default)]
    pub sql: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub auth: Option<Auth>,
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    #[serde(default)]
    pub opaque: Vec<String>,
    /// 다음 단계 전에 앱 시각을 이 초만큼 앞으로 옮긴다.
    pub advance_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// 회원 액세스 토큰의 subject.
    pub user: Option<String>,
    /// 관리자 액세스 토큰의 subject.
    pub admin: Option<String>,
    /// 원시 Authorization 헤더 값. 위조·형식 오류 토큰 시나리오용.
    pub raw: Option<String>,
    /// 만료 시각을 현재 기준으로 이 초만큼 옮긴다(음수면 이미 만료).
    pub expires_in: Option<i64>,
}

fn default_true() -> bool {
    true
}

pub fn load_dir(dir: &Path, filter: Option<&str>) -> Result<Vec<Scenario>, String> {
    let mut files = Vec::new();
    collect(dir, &mut files)?;
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        let mut s: Scenario = toml::from_str(&text).map_err(|e| format!("{}: {e}", f.display()))?;
        s.path = f.clone();
        let key = f.display().to_string();
        if filter.is_none_or(|flt| key.contains(flt) || s.tags.iter().any(|t| t == flt)) {
            out.push(s);
        }
    }
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let p = entry.map_err(|e| e.to_string())?.path();
        if p.is_dir() {
            collect(&p, out)?;
        } else if p.extension().is_some_and(|e| e == "toml") {
            out.push(p);
        }
    }
    Ok(())
}
