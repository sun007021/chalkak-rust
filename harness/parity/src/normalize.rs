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
        self.replace_uuids(s)
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
    fn base64url_왕복() {
        for s in ["", "a", "ab", "abc", "{\"alg\":\"HS256\"}"] {
            assert_eq!(
                base64url_decode(&base64url_encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
    }
}
