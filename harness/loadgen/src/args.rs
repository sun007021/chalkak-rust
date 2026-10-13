//! `--키 값`과 `--플래그` 파서.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct Args {
    values: HashMap<String, String>,
    flags: Vec<String>,
}

impl Args {
    pub fn parse(raw: &[String]) -> Result<Args, String> {
        let mut args = Args::default();
        let mut it = raw.iter().peekable();
        while let Some(a) = it.next() {
            let key = a
                .strip_prefix("--")
                .ok_or_else(|| format!("옵션은 --로 시작해야 합니다: {a}"))?;
            if let Some((k, v)) = key.split_once('=') {
                args.values.insert(k.to_owned(), v.to_owned());
            } else if it.peek().is_some_and(|n| !n.starts_with("--")) {
                args.values
                    .insert(key.to_owned(), it.next().unwrap().clone());
            } else {
                args.flags.push(key.to_owned());
            }
        }
        Ok(args)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn require(&self, key: &str) -> Result<&str, String> {
        self.str(key).ok_or_else(|| format!("--{key}가 필요합니다"))
    }

    pub fn flag(&self, key: &str) -> bool {
        self.flags.iter().any(|f| f == key)
    }

    pub fn parsed<T: FromStr>(&self, key: &str) -> Result<Option<T>, String> {
        self.str(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("--{key} 값이 올바르지 않습니다: {v}"))
            })
            .transpose()
    }

    pub fn parsed_or<T: FromStr>(&self, key: &str, default: T) -> Result<T, String> {
        Ok(self.parsed(key)?.unwrap_or(default))
    }

    /// 초 단위 숫자나 `30s`, `5m`, `2h`.
    pub fn duration_or(&self, key: &str, default: Duration) -> Result<Duration, String> {
        match self.str(key) {
            None => Ok(default),
            Some(v) => {
                parse_duration(v).ok_or_else(|| format!("--{key} 기간이 올바르지 않습니다: {v}"))
            }
        }
    }
}

pub fn parse_duration(v: &str) -> Option<Duration> {
    let (num, unit) = match v.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => v.split_at(i),
        None => (v, "s"),
    };
    let n: f64 = num.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        "ms" => n / 1000.0,
        _ => return None,
    };
    (secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Args {
        let v: Vec<String> = s.split_whitespace().map(String::from).collect();
        Args::parse(&v).unwrap()
    }

    #[test]
    fn 값과_플래그를_읽는다() {
        let a = parse("--base http://x --conns=8 --no-keepalive --label java");
        assert_eq!(a.str("base"), Some("http://x"));
        assert_eq!(a.parsed::<usize>("conns").unwrap(), Some(8));
        assert!(a.flag("no-keepalive"));
        assert_eq!(a.str("label"), Some("java"));
        assert_eq!(a.str("none"), None);
    }

    #[test]
    fn 기간을_읽는다() {
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration("5d"), None);
    }

    #[test]
    fn 필수_옵션이_없으면_오류() {
        assert!(parse("--a 1").require("b").is_err());
        assert!(Args::parse(&["oops".to_string()]).is_err());
    }
}
