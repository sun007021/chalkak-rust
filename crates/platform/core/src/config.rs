//! 환경변수 설정 로더.
//!
//! 원본은 `application.yml`의 `${ACCESS_TOKEN_EXPIRATION}` 같은 자리표시자로 환경변수를 읽고, 기본값이 없는
//! 항목이 비어 있으면 기동하지 않는다. 같은 동작을 위해 누락·형식 오류를 [`ConfigError`]로 돌려주고, 호출 측이
//! 기동 단계에서 한꺼번에 보고하고 끝낼 수 있게 한다.
//!
//! 오류 메시지에는 키 이름과 이유만 담는다. 값은 시크릿일 수 있어 [`ConfigError::Invalid`]에는 넣지 않는다.
//! (기간 값은 시크릿이 아니라서 [`ConfigError::InvalidDuration`]에만 입력을 담는다.)
//!
//! 원본의 `spring.config.import: optional:file:.env[.properties]`처럼 작업 디렉터리의 `.env` 파일(properties
//! 형식)도 읽을 수 있다. 같은 키가 프로세스 환경변수에 있으면 환경변수가 이긴다(Spring의 우선순위와 같다).

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use crate::duration::parse_duration;

/// 설정 오류.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// 필수 키가 없다.
    Missing { key: String },
    /// 값 형식이 올바르지 않다. 값은 시크릿일 수 있어 담지 않는다.
    Invalid { key: String, reason: String },
    /// 기간 문자열 형식이 올바르지 않다. [`parse_duration`]이 돌려준다.
    InvalidDuration { input: String, reason: &'static str },
    /// 설정 파일을 읽지 못했다.
    File { path: PathBuf, reason: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { key } => write!(f, "필수 설정 {key}이(가) 없습니다"),
            Self::Invalid { key, reason } => {
                write!(f, "설정 {key}의 값이 올바르지 않습니다: {reason}")
            }
            Self::InvalidDuration { input, reason } => {
                write!(f, "기간 {input:?}을(를) 해석할 수 없습니다: {reason}")
            }
            Self::File { path, reason } => {
                write!(
                    f,
                    "설정 파일 {}을(를) 읽지 못했습니다: {reason}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// 키-값 설정 묶음. 프로세스 환경변수와 선택적인 `.env` 파일에서 만든다.
#[derive(Clone, Default)]
pub struct Env {
    vars: HashMap<String, String>,
}

// 시크릿이 들어 있을 수 있어 Debug 출력에는 키만 남긴다.
impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&String> = self.vars.keys().collect();
        keys.sort();
        f.debug_struct("Env").field("keys", &keys).finish()
    }
}

impl Env {
    /// 프로세스 환경변수만 읽는다. UTF-8이 아닌 값은 건너뛴다.
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        }
    }

    /// `.env` 파일(properties 형식, 없으면 무시)을 바탕으로 깔고 프로세스 환경변수를 위에 덮는다.
    pub fn from_process_with_dotenv(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut env = Self::from_dotenv_file(path)?;
        env.vars.extend(Self::from_process().vars);
        Ok(env)
    }

    /// `.env` 파일만 읽는다. 파일이 없으면 빈 설정이다(Spring `optional:`).
    pub fn from_dotenv_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::from_properties(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::File {
                path: path.to_owned(),
                reason: e.to_string(),
            }),
        }
    }

    /// java.util.Properties 형식 본문을 읽는다. 지원 범위: 빈 줄, `#`·`!` 주석, `key=value`·`key:value`·
    /// `key value`, 값 앞 공백 제거(뒤 공백은 유지), `\n \t \r \f \\ \uXXXX` 이스케이프. 줄 이어쓰기(`\` 끝)는
    /// 지원하지 않는다.
    pub fn from_properties(text: &str) -> Self {
        let mut vars = HashMap::new();
        for raw in text.lines() {
            let line = raw.trim_start_matches([' ', '\t', '\u{c}']);
            if line.is_empty() || line.starts_with(['#', '!']) {
                continue;
            }
            let key_end = line
                .char_indices()
                .find(|&(_, c)| matches!(c, '=' | ':' | ' ' | '\t' | '\u{c}'))
                .map_or(line.len(), |(i, _)| i);
            let (key, rest) = line.split_at(key_end);
            let rest = rest.trim_start_matches([' ', '\t', '\u{c}']);
            let rest = rest
                .strip_prefix(['=', ':'])
                .map_or(rest, |r| r.trim_start_matches([' ', '\t', '\u{c}']));
            vars.insert(unescape_properties(key), unescape_properties(rest));
        }
        Self { vars }
    }

    /// 테스트·조립용으로 직접 만든다.
    pub fn from_pairs<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// 값이 있으면 돌려준다(빈 문자열도 값이다).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    pub fn required(&self, key: &str) -> Result<&str, ConfigError> {
        self.get(key).ok_or_else(|| ConfigError::Missing {
            key: key.to_owned(),
        })
    }

    /// `${KEY:default}` 자리표시자와 같다.
    pub fn string_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    pub fn required_duration(&self, key: &str) -> Result<Duration, ConfigError> {
        let raw = self.required(key)?;
        parse_duration(raw).map_err(|e| invalid_from(key, &e))
    }

    pub fn duration_or(&self, key: &str, default: Duration) -> Result<Duration, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some(raw) => parse_duration(raw).map_err(|e| invalid_from(key, &e)),
        }
    }

    /// `FromStr`로 해석되는 필수 값(숫자 등). 오류 이유에 값은 담지 않는다.
    pub fn required_parsed<T: FromStr>(&self, key: &str) -> Result<T, ConfigError> {
        let raw = self.required(key)?;
        raw.parse().map_err(|_| ConfigError::Invalid {
            key: key.to_owned(),
            reason: format!("{}(으)로 해석할 수 없습니다", std::any::type_name::<T>()),
        })
    }

    pub fn parsed_or<T: FromStr>(&self, key: &str, default: T) -> Result<T, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some(raw) => raw.parse().map_err(|_| ConfigError::Invalid {
                key: key.to_owned(),
                reason: format!("{}(으)로 해석할 수 없습니다", std::any::type_name::<T>()),
            }),
        }
    }

    /// `true`/`false`(대소문자 무시)만 받는다. Spring의 불리언 바인딩과 같다.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, ConfigError> {
        match self.get(key) {
            None => Ok(default),
            Some(raw) if raw.eq_ignore_ascii_case("true") => Ok(true),
            Some(raw) if raw.eq_ignore_ascii_case("false") => Ok(false),
            Some(_) => Err(ConfigError::Invalid {
                key: key.to_owned(),
                reason: "true 또는 false여야 합니다".to_owned(),
            }),
        }
    }
}

fn invalid_from(key: &str, e: &ConfigError) -> ConfigError {
    match e {
        ConfigError::InvalidDuration { reason, .. } => ConfigError::Invalid {
            key: key.to_owned(),
            reason: (*reason).to_owned(),
        },
        other => other.clone(),
    }
}

fn unescape_properties(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push_str("\\u");
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn 필수_키가_없으면_missing() {
        let err = env(&[]).required("ACCESS_TOKEN_SECRET").unwrap_err();
        assert_eq!(
            err,
            ConfigError::Missing {
                key: "ACCESS_TOKEN_SECRET".into()
            }
        );
        assert!(err.to_string().contains("ACCESS_TOKEN_SECRET"));
    }

    #[test]
    fn 빈_문자열도_값으로_본다_spring_자리표시자와_같다() {
        assert_eq!(env(&[("A", "")]).required("A").unwrap(), "");
    }

    #[test]
    fn 기간_필수값() {
        let e = env(&[("ACCESS_TOKEN_EXPIRATION", "15m")]);
        assert_eq!(
            e.required_duration("ACCESS_TOKEN_EXPIRATION").unwrap(),
            Duration::from_secs(900)
        );
        assert!(matches!(
            e.required_duration("REFRESH_TOKEN_ABSOLUTE_EXPIRATION"),
            Err(ConfigError::Missing { .. })
        ));
    }

    #[test]
    fn 기간_형식_오류는_키와_이유를_담고_값은_담지_않는다() {
        let e = env(&[("POST_IMAGE_PROCESSING_TIMEOUT", "7 minutes")]);
        let err = e
            .required_duration("POST_IMAGE_PROCESSING_TIMEOUT")
            .unwrap_err();
        let ConfigError::Invalid { key, reason } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(key, "POST_IMAGE_PROCESSING_TIMEOUT");
        assert!(!reason.is_empty());
        assert!(!err.to_string().contains("7 minutes"));
    }

    #[test]
    fn 기본값_기간() {
        let e = env(&[]);
        assert_eq!(
            e.duration_or("SLACK_CONNECT_TIMEOUT", Duration::from_secs(2))
                .unwrap(),
            Duration::from_secs(2)
        );
        let e = env(&[("SLACK_CONNECT_TIMEOUT", "PT3S")]);
        assert_eq!(
            e.duration_or("SLACK_CONNECT_TIMEOUT", Duration::from_secs(2))
                .unwrap(),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn 숫자_해석_오류는_값을_담지_않는다() {
        let e = env(&[("CALLBACK_MAX_BODY_BYTES", "12kb-secret")]);
        let err = e
            .required_parsed::<u64>("CALLBACK_MAX_BODY_BYTES")
            .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        assert!(!err.to_string().contains("12kb-secret"));
        let e = env(&[("CALLBACK_MAX_BODY_BYTES", "65536")]);
        assert_eq!(
            e.required_parsed::<u64>("CALLBACK_MAX_BODY_BYTES").unwrap(),
            65_536
        );
    }

    #[test]
    fn 기본값_문자열은_aws_region_자리표시자처럼_쓴다() {
        // application.yml: region: ${AWS_REGION:ap-northeast-2}
        assert_eq!(
            env(&[]).string_or("AWS_REGION", "ap-northeast-2"),
            "ap-northeast-2"
        );
        assert_eq!(
            env(&[("AWS_REGION", "us-east-1")]).string_or("AWS_REGION", "ap-northeast-2"),
            "us-east-1"
        );
    }

    #[test]
    fn 불리언() {
        assert!(env(&[("X", "TRUE")]).bool_or("X", false).unwrap());
        assert!(!env(&[("X", "false")]).bool_or("X", true).unwrap());
        assert!(env(&[]).bool_or("X", true).unwrap());
        assert!(env(&[("X", "yes")]).bool_or("X", true).is_err());
    }

    #[test]
    fn debug_출력에는_값이_없다() {
        let e = env(&[("ACCESS_TOKEN_SECRET", "super-secret")]);
        let shown = format!("{e:?}");
        assert!(shown.contains("ACCESS_TOKEN_SECRET"));
        assert!(!shown.contains("super-secret"));
    }

    #[test]
    fn properties_형식_본문() {
        let text = "# 주석\n! 주석\n\nACCESS_TOKEN_EXPIRATION=15m\n  S3_PREFIX : dev/\nKEY value\nEMPTY=\nPADDED=  x \nESC=a\\tb\\u0041\n";
        let e = Env::from_properties(text);
        assert_eq!(e.get("ACCESS_TOKEN_EXPIRATION"), Some("15m"));
        assert_eq!(e.get("S3_PREFIX"), Some("dev/"));
        assert_eq!(e.get("KEY"), Some("value"));
        assert_eq!(e.get("EMPTY"), Some(""));
        assert_eq!(e.get("PADDED"), Some("x "));
        assert_eq!(e.get("ESC"), Some("a\tbA"));
        assert_eq!(e.get("# 주석"), None);
    }

    #[test]
    fn dotenv_파일이_없으면_빈_설정() {
        let e = Env::from_dotenv_file("/nonexistent/chalkak/.env").unwrap();
        assert!(e.get("ANY").is_none());
    }

    #[test]
    fn dotenv_는_프로세스_환경변수보다_약하다() {
        let dir = std::env::temp_dir().join(format!("chalkak-core-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(".env");
        // PATH는 모든 환경에 있는 변수다. 파일 값이 덮이는지 확인한다.
        std::fs::write(&file, "PATH=from-file\nCHALKAK_ONLY_IN_FILE=1\n").unwrap();
        let e = Env::from_process_with_dotenv(&file).unwrap();
        assert_ne!(e.get("PATH"), Some("from-file"));
        assert_eq!(e.get("CHALKAK_ONLY_IN_FILE"), Some("1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
