//! 응답·DB 값에서 "구현마다 달라도 되는 값"만 기호로 바꾼다. 나머지는 바이트 그대로 비교한다.
//!
//! - UUID: 처음 나온 순서대로 `$uuid1`, `$uuid2`… 같은 값은 같은 기호로 묶여야 통과한다.
//! - 시각: 시나리오 시계 기준 상대 초로 바꾸고 원문 형식(소수 자릿수, 오프셋 표기)은 남긴다.
//! - JWT: 헤더·클레임을 풀어서 위 규칙으로 정규화하고 서명은 버린다.
//! - 불투명 값(시나리오가 지정한 경로): 길이만 남긴다.

use crate::time;
use serde_json::{Map, Value};
use std::collections::HashMap;

const BUCKET_SECONDS: f64 = 5.0;

pub struct Normalizer {
    uuids: HashMap<String, usize>,
    /// 시나리오 시계(epoch 초). 시각을 이 값 기준 상대 초로 표기한다.
    clock: f64,
    /// 이 값으로 끝나지 않는 UUID는 새 기호를 주지 않는다(DB 덤프의 2단계 정규화용).
    freeze: bool,
    /// DB 덤프 값은 PostgreSQL이 문자열로 만든 것이라 소수 자릿수가 앱 구현과 무관하다.
    db: bool,
}

impl Normalizer {
    pub fn new(clock: f64) -> Self {
        Self {
            uuids: HashMap::new(),
            clock,
            freeze: false,
            db: false,
        }
    }

    /// 이후 처음 보는 UUID는 `$uuid?`로 둔다. DB 행처럼 순서가 구현마다 다를 수 있는 값에 쓴다.
    pub fn freeze(&mut self) {
        self.freeze = true;
    }

    /// 이후 시각의 소수 자릿수를 표기하지 않는다.
    pub fn db_mode(&mut self) {
        self.db = true;
    }

    pub fn value(&mut self, v: &Value, opaque: &[String], path: &str) -> Value {
        if opaque.iter().any(|p| p == path) {
            return match v {
                Value::String(s) => Value::String(format!("<opaque len={}>", s.len())),
                Value::Null => Value::Null,
                other => Value::String(format!("<opaque {}>", kind(other))),
            };
        }
        match v {
            Value::String(s) => Value::String(self.string(s)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, x)| self.value(x, opaque, &join(path, &i.to_string())))
                    .collect(),
            ),
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, x) in map {
                    let child = join(path, k);
                    let nv = if is_epoch_claim(k) && x.is_number() {
                        Value::String(self.relative(x.as_f64().unwrap_or(0.0), "epoch"))
                    } else {
                        self.value(x, opaque, &child)
                    };
                    out.insert(k.clone(), nv);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }

    /// 문자열 하나를 정규화한다. 문자열 전체가 UUID·시각·JWT인 경우와, 문자열 안에 UUID가 섞인 경우
    /// (URL, 저장소 키, 오류 메시지)를 모두 처리한다.
    pub fn string(&mut self, s: &str) -> String {
        if let Some(p) = time::parse(s) {
            let fmt = if self.db {
                "db".to_string()
            } else {
                fraction_style(s, p.fraction_digits)
            };
            return self.relative(p.epoch, &format!("{fmt}{}", zone_label(&p.zone)));
        }
        if let Some(jwt) = self.jwt(s) {
            return jwt;
        }
        let s = self.whitelabel_created(s);
        let s = self.yaml_times(&s);
        let s = self.presigned(s.as_str());
        self.replace_uuids(&s)
    }

    /// Spring Boot Whitelabel 페이지의 `<div id='created'>Tue Oct 13 10:02:35 UTC 2026</div>`(`Date.toString()`)를
    /// 시나리오 시계 기준 상대 초로 바꾼다. 페이지를 만든 순간의 시각이라 앱마다 초 단위로 달라진다.
    fn whitelabel_created(&self, s: &str) -> String {
        const OPEN: &str = "<div id='created'>";
        let Some(at) = s.find(OPEN) else {
            return s.to_string();
        };
        let start = at + OPEN.len();
        let Some(len) = s[start..].find("</div>") else {
            return s.to_string();
        };
        match java_date_epoch(&s[start..start + len]) {
            Some(epoch) => {
                let mut out = s.to_string();
                out.replace_range(start..start + len, &self.relative(epoch, "java-date"));
                out
            }
            None => s.to_string(),
        }
    }

    /// `Accept: application/yaml` 응답의 시각 필드. YAML 매퍼는 날짜를 숫자로 쓴다: Boot 오류 본문의 `timestamp`는
    /// 밀리초(13자리), `…At`(Instant)은 `초.나노초`다. 시나리오 시계 기준 상대 초로 바꾼다.
    fn yaml_times(&self, s: &str) -> String {
        if !s.starts_with("---\n") {
            return s.to_string();
        }
        let mut out = Vec::new();
        for line in s.split('\n') {
            let rewritten = line.split_once(": ").and_then(|(key, value)| {
                let epoch = if key == "timestamp" && value.len() == 13 {
                    value.parse::<f64>().ok()? / 1000.0
                } else if key.ends_with("At") && value.contains('.') {
                    let (secs, nanos) = value.split_once('.')?;
                    (secs.len() == 10 && nanos.len() == 9).then(|| value.parse::<f64>().ok())??
                } else {
                    return None;
                };
                Some(format!("{key}: {}", self.relative(epoch, "yaml-time")))
            });
            out.push(rewritten.unwrap_or_else(|| line.to_string()));
        }
        out.join("\n")
    }

    /// presigned URL(SigV4 쿼리 서명)의 `X-Amz-Date`와 `X-Amz-Signature`를 바꾼다. 서명 시각은 앱이 응답을 만든 순간이라
    /// 단계 처리 시간에 따라 초 단위로 달라지고, 서명은 그 시각에 따라 달라진다. 시각은 시나리오 시계 기준 60초 칸의
    /// 상대값으로, 서명은 길이만 남긴다. 호스트·경로·자격증명 범위·서명 헤더·만료 같은 나머지는 그대로 비교한다.
    /// 서명 계산 자체는 AWS 공식 테스트 벡터로 따로 검증한다.
    fn presigned(&self, s: &str) -> String {
        const DATE: &str = "X-Amz-Date=";
        const SIG: &str = "X-Amz-Signature=";
        let mut out = s.to_string();
        if let Some(at) = out.find(DATE) {
            let (start, end) = (at + DATE.len(), at + DATE.len() + 16);
            if let Some(epoch) = out.get(start..end).and_then(amz_date_epoch) {
                let delta = ((epoch - self.clock) / 60.0).floor() as i64 * 60;
                out.replace_range(start..end, &format!("<t{delta:+}s amzdate>"));
            }
        }
        if let Some(at) = out.find(SIG) {
            let start = at + SIG.len();
            let len = out[start..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(out.len() - start);
            out.replace_range(start..start + len, &format!("<sigv4 len={len}>"));
        }
        out
    }

    /// 시나리오 시계와의 차이를 5초 단위로 내림한다. 두 구현이 같은 단계를 처리하는 데 걸린 시간 차(수백 ms)는
    /// 같은 칸에 들어가고, 시나리오가 `advance_seconds`로 옮긴 시간은 그대로 드러난다.
    fn relative(&self, epoch: f64, fmt: &str) -> String {
        let delta = ((epoch - self.clock) / BUCKET_SECONDS).floor() as i64 * BUCKET_SECONDS as i64;
        format!("<t{delta:+}s {fmt}>")
    }

    fn uuid_symbol(&mut self, raw: &str) -> String {
        let key = raw.to_ascii_lowercase();
        if let Some(n) = self.uuids.get(&key) {
            return format!("$uuid{n}");
        }
        if self.freeze {
            return "$uuid?".to_string();
        }
        let n = self.uuids.len() + 1;
        self.uuids.insert(key, n);
        format!("$uuid{n}")
    }

    fn replace_uuids(&mut self, s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if let Some(cand) = s.get(i..i + 36).filter(|c| is_uuid(c)) {
                let sym = self.uuid_symbol(cand);
                out.push_str(&sym);
                i += 36;
            } else {
                let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }

    fn jwt(&mut self, s: &str) -> Option<String> {
        let mut parts = s.split('.');
        let (h, p, sig) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || !h.starts_with("eyJ") || sig.is_empty() {
            return None;
        }
        let header: Value = serde_json::from_slice(&base64url_decode(h)?).ok()?;
        let payload: Value = serde_json::from_slice(&base64url_decode(p)?).ok()?;
        let header = self.value(&header, &[], "");
        let payload = self.value(&payload, &["jti".to_string()], "");
        Some(format!("<jwt {} {}>", header, payload))
    }
}

/// 응답 시각의 소수부 표기 방식. 소수부 길이는 값에 따라 달라지므로(예: .120000 → .12) 길이 대신 규칙을 비교한다.
/// - `iso`: Java `DateTimeFormatter.ISO_INSTANT`처럼 3자리 묶음 중 가장 짧게(0, 3, 6, 9자리) 쓴다.
/// - 그 외에는 실제 자릿수 `fN`을 남겨 차이가 드러나게 한다.
fn fraction_style(s: &str, digits: usize) -> String {
    if digits == 0 {
        return "iso".into();
    }
    let frac = s.split_once('.').map(|(_, r)| &r[..digits]).unwrap_or("");
    if digits.is_multiple_of(3) && !frac.ends_with("000") {
        "iso".into()
    } else {
        format!("f{digits}")
    }
}

/// `Tue Oct 13 10:02:35 UTC 2026`(`Date.toString()`, UTC 기준)을 epoch 초로 바꾼다.
fn java_date_epoch(s: &str) -> Option<f64> {
    let mut parts = s.split_whitespace();
    let (_dow, month, day, clock, zone, year) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if zone != "UTC" || parts.next().is_some() {
        return None;
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? as u32 + 1;
    let mut hms = clock.split(':');
    let (h, m, sec) = (
        hms.next()?.parse::<i64>().ok()?,
        hms.next()?.parse::<i64>().ok()?,
        hms.next()?.parse::<i64>().ok()?,
    );
    let days = time::days_from_civil(year.parse().ok()?, month, day.parse().ok()?);
    Some((days * 86_400 + h * 3600 + m * 60 + sec) as f64)
}

/// `20261012T000005Z` 형식(SigV4 날짜)을 epoch 초로 바꾼다.
fn amz_date_epoch(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = time::days_from_civil(num(0..4)?, num(4..6)? as u32, num(6..8)? as u32);
    Some((days * 86_400 + num(9..11)? * 3600 + num(11..13)? * 60 + num(13..15)?) as f64)
}

fn is_epoch_claim(k: &str) -> bool {
    matches!(k, "iat" | "exp" | "nbf" | "auth_time")
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        _ => "other",
    }
}

fn zone_label(z: &str) -> String {
    if z.is_empty() {
        " local".to_string()
    } else {
        format!(" {z}")
    }
}

fn join(base: &str, key: &str) -> String {
    if base.is_empty() {
        key.to_string()
    } else {
        format!("{base}.{key}")
    }
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub fn base64url_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CLOCK: f64 = 1_791_763_200.0; // 2026-10-12T00:00:00Z

    #[test]
    fn whitelabel_생성_시각을_시계_기준으로_바꾼다() {
        let mut n = Normalizer::new(CLOCK);
        let page = "<html><body><div id='created'>Mon Oct 12 00:00:07 UTC 2026</div><div>x</div></body></html>";
        assert_eq!(
            n.string(page),
            "<html><body><div id='created'><t+5s java-date></div><div>x</div></body></html>"
        );
        // 같은 5초 칸이면 같은 값
        assert_eq!(
            n.string("<div id='created'>Mon Oct 12 00:00:09 UTC 2026</div>"),
            "<div id='created'><t+5s java-date></div>"
        );
    }

    #[test]
    fn 같은_uuid는_같은_기호로_묶는다() {
        let mut n = Normalizer::new(CLOCK);
        let v = json!({"a": "0199aaaa-0000-7000-8000-000000000001",
                       "b": ["0199AAAA-0000-7000-8000-000000000001", "0199aaaa-0000-7000-8000-000000000002"]});
        assert_eq!(
            n.value(&v, &[], ""),
            json!({"a": "$uuid1", "b": ["$uuid1", "$uuid2"]})
        );
    }

    #[test]
    fn 문자열_속_uuid도_바꾼다() {
        let mut n = Normalizer::new(CLOCK);
        assert_eq!(
            n.string("chalkak/posts/0199aaaa-0000-7000-8000-000000000001/original.jpg"),
            "chalkak/posts/$uuid1/original.jpg"
        );
        assert_eq!(
            n.string("주제를 찾을 수 없습니다. 다시 시도해 주세요."),
            "주제를 찾을 수 없습니다. 다시 시도해 주세요."
        );
    }

    #[test]
    fn 시각은_상대초와_형식으로_바꾼다() {
        let mut n = Normalizer::new(CLOCK);
        assert_eq!(n.string("2026-10-12T00:00:05.120Z"), "<t+5s iso Z>");
        assert_eq!(n.string("2026-10-12T00:00:05.12Z"), "<t+5s f2 Z>");
        assert_eq!(n.string("2026-10-12T00:00:05.120000Z"), "<t+5s f6 Z>");
        assert_eq!(n.string("2026-10-12T09:00:00+09:00"), "<t+0s iso +09:00>");
        n.db_mode();
        assert_eq!(
            n.string("2026-10-12T09:00:00.12345+09:00"),
            "<t+0s db +09:00>"
        );
    }

    #[test]
    fn presigned_url의_서명_시각과_서명을_바꾼다() {
        let mut n = Normalizer::new(CLOCK);
        let url = "http://b.fakes:9000/k/0199aaaa-0000-7000-8000-000000000001.png?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                   &X-Amz-Date=20261012T000005Z&X-Amz-Expires=300&X-Amz-Signature=a17659c6d5c876bf";
        assert_eq!(
            n.string(url),
            "http://b.fakes:9000/k/$uuid1.png?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Date=<t+0s amzdate>&X-Amz-Expires=300&X-Amz-Signature=<sigv4 len=16>"
        );
        // 60초 칸이 바뀌면 드러난다.
        let later = url.replace("T000005Z", "T000105Z");
        assert!(n.string(&later).contains("<t+60s amzdate>"));
    }

    #[test]
    fn 고정_뒤의_새_uuid는_물음표로_둔다() {
        let mut n = Normalizer::new(CLOCK);
        n.string("0199aaaa-0000-7000-8000-000000000001");
        n.freeze();
        assert_eq!(n.string("0199aaaa-0000-7000-8000-000000000001"), "$uuid1");
        assert_eq!(n.string("0199aaaa-0000-7000-8000-000000000009"), "$uuid?");
    }

    #[test]
    fn 불투명_값은_길이만_남긴다() {
        let mut n = Normalizer::new(CLOCK);
        let v = json!({"refreshToken": "abcdef"});
        assert_eq!(
            n.value(&v, &["refreshToken".into()], ""),
            json!({"refreshToken": "<opaque len=6>"})
        );
    }

    #[test]
    fn yaml_시각_필드를_상대_초로_바꾼다() {
        let mut n = Normalizer::new(CLOCK);
        let doc = "---\ntimestamp: 1791763201401\nstatus: 400\ncreatedAt: 1791763201.953659000\nnote: 1791763201401\n";
        assert_eq!(
            n.string(doc),
            "---\ntimestamp: <t+0s yaml-time>\nstatus: 400\ncreatedAt: <t+0s yaml-time>\nnote: 1791763201401\n"
        );
    }

    #[test]
    fn base64url_왕복() {
        for s in ["", "a", "ab", "abc", "{\"alg\":\"HS256\"}"] {
            assert_eq!(
                base64url_decode(&base64url_encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
    }
}
