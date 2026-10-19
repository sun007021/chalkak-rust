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
//!
//! 외부 연동(가짜 서버 `harness/fakes`)이 필요한 시나리오는 아래를 더 쓴다. 대상 설정에 `fakes_url`이 있어야 한다.
//!
//! ```toml
//! # 소셜 로그인 ID Token. 시나리오 시작 시 테스트 개인키로 RS256 서명해 변수 ${googleNew}로 쓴다.
//! [[id_token]]
//! name = "googleNew"
//! provider = "google"                  # google | kakao | apple (iss·aud 기본값이 정해진다)
//! sub = "google-sub-1"
//! nonce = "raw-nonce-1"                # 원본 nonce. 토큰의 nonce 클레임은 SHA-256 소문자 hex
//! email = "a@example.com"
//! email_verified = true
//! issued_in = 0                        # iat = 시나리오 시계 + 이 초(기본 0)
//! expires_in = 600                     # exp = 시나리오 시계 + 이 초(기본 600)
//! # aud, iss, kid로 값을 바꾸고, claims로 클레임을 더하거나 덮고, omit로 클레임을 뺀다
//! # tamper_signature = true            # 서명을 깨뜨린다
//!
//! # 가짜 서버 응답. 시나리오 시작 시 설정하며(이전 설정은 초기화), 단계마다 [step.fakes]로 덧씌울 수 있다.
//! # 덧씌운 값은 같은 항목을 다시 덧씌우기 전까지 이후 단계에도 유지된다.
//! [fakes.apple_token]                  # POST /apple/auth/token. 지정하지 않으면 500
//! status = 200
//! body = '''{"access_token":"a","token_type":"Bearer","expires_in":3600,"refresh_token":"r","id_token":"${appleIdToken}"}'''
//! [fakes.apple_revoke]                 # POST /apple/auth/revoke
//! status = 200
//! [fakes.slack]                        # Slack Webhook. 지정하지 않으면 200 ok
//! status = 200
//! [fakes.s3."chalkak/signatures/local/original/${uploadId}.png"]   # HeadObject. 지정하지 않은 키는 404
//! size = 1024
//! content_type = "image/png"
//! ```
//!
//! DB에 무작위 값이 들어가는 컬럼(암호문 등)은 최상위에 `db_opaque = ["encrypted_refresh_token"]`처럼 적어 길이만 비교한다.
//!
//! 시나리오가 끝나면 가짜 서버가 받은 요청(JWKS 조회 제외)을 정규화해 결과의 `fakes`에 넣어 비교한다.
//! `fakes`/`id_token`을 쓰지 않고 받은 요청도 없으면 `fakes` 항목은 생략한다.

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
    /// 시나리오 시작 시 서명해 변수로 만드는 소셜 로그인 ID Token.
    #[serde(default, rename = "id_token")]
    pub id_tokens: Vec<IdTokenSpec>,
    /// 가짜 외부 서버의 시작 응답.
    pub fakes: Option<FakesSpec>,
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
    /// 시나리오 끝에 DB 상태를 비교할지. 기본은 비교한다.
    #[serde(default = "default_true")]
    pub compare_db: bool,
    /// DB 덤프에서 길이만 비교할 컬럼 이름(무작위 IV로 암호화한 값 등).
    #[serde(default)]
    pub db_opaque: Vec<String>,
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
    /// 지정한 바이트 수만큼 `a`로 채운 본문을 보낸다(본문 크기 제한 시나리오). `body`와 함께 쓸 수 없다.
    pub body_size: Option<usize>,
    pub auth: Option<Auth>,
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    #[serde(default)]
    pub opaque: Vec<String>,
    /// 다음 단계 전에 앱 시각을 이 초만큼 앞으로 옮긴다.
    pub advance_seconds: Option<i64>,
    /// 이 단계 요청을 보내기 전에 가짜 서버 응답을 덧씌운다(항목 단위로 덮고 s3 키는 합친다).
    pub fakes: Option<FakesSpec>,
    /// 이미지 처리 콜백 서명 헤더를 붙인다(`/internal/v1/...`). 시각은 단계 시작 시 앱 시각이다.
    pub callback: Option<CallbackSpec>,
    /// 응답을 받은 뒤, 비동기로 도는 일이 가짜 서버를 이만큼 부를 때까지 기다린다(누적 횟수).
    /// 응답 뒤에 외부 호출을 이어 가는 일(사용자 지표 집계)은 앱마다 걸리는 시간이 달라 다음 단계와 겹치지 않게 맞춘다.
    pub await_fakes: Option<AwaitFakes>,
}

/// 가짜 서버 호출 대기 지정. `service`가 같은 기록이 시나리오 시작부터 `count`개 이상 쌓일 때까지 기다린다.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AwaitFakes {
    pub service: String,
    pub count: usize,
}

/// 콜백 서명 지정. 필드를 생략하면 올바른 서명(비밀키·audience는 하네스 기본값)이다.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CallbackSpec {
    /// 타임스탬프를 앱 시각에서 이 초만큼 옮긴다(음수면 과거).
    pub timestamp_offset: Option<i64>,
    pub secret: Option<String>,
    pub audience: Option<String>,
    /// 서명 대상 경로를 실제 요청 경로 대신 이 값으로 한다(다른 콜백 경로의 서명 재사용 시나리오).
    pub sign_path: Option<String>,
    /// 서명 대상 본문을 실제 본문 대신 이 문자열로 한다(본문 변조 시나리오).
    pub sign_body: Option<String>,
    /// 서명 헤더 값을 통째로 덮는다.
    pub signature: Option<String>,
    /// 보내지 않을 헤더(`timestamp`, `signature`).
    #[serde(default)]
    pub omit: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdTokenSpec {
    /// 변수 이름. 이후 `${이름}`으로 쓴다.
    pub name: String,
    pub provider: String,
    pub sub: String,
    /// 원본 nonce. 있으면 nonce 클레임에 SHA-256 소문자 hex를 넣는다.
    pub nonce: Option<String>,
    pub email: Option<String>,
    /// bool 또는 문자열("true")로 쓴다. Apple은 두 표현을 모두 보낸다.
    pub email_verified: Option<serde_json::Value>,
    pub issued_in: Option<i64>,
    pub expires_in: Option<i64>,
    pub aud: Option<String>,
    pub iss: Option<String>,
    pub kid: Option<String>,
    /// 클레임을 더하거나 덮어쓴다.
    #[serde(default)]
    pub claims: BTreeMap<String, serde_json::Value>,
    /// 만든 뒤 지울 클레임 이름.
    #[serde(default)]
    pub omit: Vec<String>,
    #[serde(default)]
    pub tamper_signature: bool,
}

/// 가짜 서버 응답 지정. 필드를 생략하면 가짜 서버의 기본 동작이다.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FakesSpec {
    pub apple_token: Option<CannedSpec>,
    pub apple_revoke: Option<CannedSpec>,
    pub slack: Option<CannedSpec>,
    /// CloudWatch Logs Insights. 쿼리는 즉시 끝난다.
    pub logs: Option<LogsSpec>,
    /// 객체 키 → HeadObject 응답.
    #[serde(default)]
    pub s3: BTreeMap<String, S3Spec>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CannedSpec {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Option<String>,
}

/// 쿼리의 `stats ... as <지표>` 이름 → 결과 행의 필드(값은 Logs Insights처럼 문자열).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LogsSpec {
    /// `GetQueryResults` 상태. 기본 `Complete`.
    pub status: Option<String>,
    #[serde(default)]
    pub rows: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct S3Spec {
    /// 기본 200. 404·403 등을 쓰면 본문 없는 오류 응답이다.
    pub status: Option<u16>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
}

impl FakesSpec {
    /// 단계 지정을 덧씌운다. 같은 항목은 덮고, s3는 키별로 합친다.
    pub fn merge(&mut self, over: &FakesSpec) {
        if over.apple_token.is_some() {
            self.apple_token.clone_from(&over.apple_token);
        }
        if over.apple_revoke.is_some() {
            self.apple_revoke.clone_from(&over.apple_revoke);
        }
        if over.slack.is_some() {
            self.slack.clone_from(&over.slack);
        }
        if over.logs.is_some() {
            self.logs.clone_from(&over.logs);
        }
        for (k, v) in &over.s3 {
            self.s3.insert(k.clone(), v.clone());
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name = "소셜"
clock = "2026-10-12T09:00:00+09:00"

[[id_token]]
name = "g"
provider = "google"
sub = "s"
nonce = "n"

[fakes.apple_token]
status = 400
body = "x"
[fakes.s3."a/b.png"]
size = 3
content_type = "image/png"

[[step]]
name = "s1"
method = "GET"
path = "/"
[step.fakes.s3."c/d.png"]
status = 404

[[step]]
name = "s2"
method = "GET"
path = "/"
"#;

    #[test]
    fn 외부_연동_항목을_읽는다() {
        let s: Scenario = toml::from_str(SAMPLE).unwrap();
        assert_eq!(s.id_tokens.len(), 1);
        assert_eq!(s.id_tokens[0].provider, "google");
        let f = s.fakes.unwrap();
        assert_eq!(f.apple_token.unwrap().status, 400);
        assert_eq!(f.s3["a/b.png"].size, Some(3));
        assert_eq!(
            s.steps[0].fakes.as_ref().unwrap().s3["c/d.png"].status,
            Some(404)
        );
        assert!(s.steps[1].fakes.is_none());
    }

    #[test]
    fn 기존_시나리오는_외부_연동_없이_읽힌다() {
        let s: Scenario = toml::from_str(
            "name = \"x\"\n[[step]]\nname = \"a\"\nmethod = \"GET\"\npath = \"/\"\n",
        )
        .unwrap();
        assert!(s.id_tokens.is_empty() && s.fakes.is_none());
    }

    #[test]
    fn 단계_지정은_항목을_덮고_s3는_합친다() {
        let mut base: FakesSpec = toml::from_str(
            "[apple_token]\nstatus = 200\n[s3.\"a\"]\nsize = 1\n[s3.\"b\"]\nsize = 2\n",
        )
        .unwrap();
        let over: FakesSpec =
            toml::from_str("[apple_token]\nstatus = 400\n[s3.\"b\"]\nsize = 9\n").unwrap();
        base.merge(&over);
        assert_eq!(base.apple_token.unwrap().status, 400);
        assert_eq!(base.s3["a"].size, Some(1));
        assert_eq!(base.s3["b"].size, Some(9));
    }

    #[test]
    fn 알_수_없는_fakes_항목은_거절한다() {
        assert!(toml::from_str::<FakesSpec>("[nope]\nstatus = 1\n").is_err());
    }

    #[test]
    fn 로그_지정과_호출_대기를_읽는다() {
        let s: Scenario = toml::from_str(
            "name = \"x\"\n[fakes.logs.rows.visitors]\nvisitors = \"7\"\n[[step]]\nname = \"a\"\nmethod = \"POST\"\npath = \"/\"\nawait_fakes = { service = \"cloudwatch_logs\", count = 10 }\n[step.fakes.logs]\nstatus = \"Failed\"\n",
        )
        .unwrap();
        assert_eq!(
            s.fakes.unwrap().logs.unwrap().rows["visitors"]["visitors"],
            "7"
        );
        let wait = s.steps[0].await_fakes.as_ref().unwrap();
        assert_eq!((wait.service.as_str(), wait.count), ("cloudwatch_logs", 10));
        assert_eq!(
            s.steps[0]
                .fakes
                .as_ref()
                .unwrap()
                .logs
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("Failed")
        );
    }
}
