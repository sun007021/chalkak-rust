//! 게시물 API가 요청 값을 읽는 규칙. Spring MVC(`@PathVariable`, `@ModelAttribute`)와 Jackson이 해 주던 변환이다.
//!
//! 규칙은 원본 java-a에 직접 보내 확인한 것이다(시나리오 `post/*`).

use chalkak_core::error::{AppError, ErrorCode};
use serde_json::Value;
use uuid::Uuid;

use crate::common::java_uuid_from_string;
use crate::web::{ApiError, binding::is_java_whitespace};

/// `CanonicalUuidParser.parse`: `UUID.fromString`이 받고, 다시 문자열로 만든 값이 입력과 (대소문자 무시로) 같아야
/// 한다. 그래서 `1-2-3-4-5` 같은 느슨한 표기는 거절된다.
pub fn parse_canonical_uuid(value: &str) -> Result<Uuid, ApiError> {
    let invalid = || {
        ApiError::App(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            "ID 형식이 올바르지 않습니다.",
        ))
    };
    let uuid = java_uuid_from_string(value).ok_or_else(invalid)?;
    if !uuid.to_string().eq_ignore_ascii_case(value) {
        return Err(invalid());
    }
    Ok(uuid)
}

/// Jackson `UUIDDeserializer`가 JSON 값에서 UUID를 읽는 규칙. 없거나 `null`이거나 빈 문자열이면 `None`이다.
/// 문자열은 하이픈이 있는 36자이거나 base64 24자여야 하고, 그 밖의 값과 JSON 타입은 읽을 수 없는 본문(400)이다.
pub fn jackson_uuid(value: Option<&Value>) -> Result<Option<Uuid>, ApiError> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s,
        Some(_) => return Err(ApiError::UnreadableBody),
    };
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() == 36 {
        let b = text.as_bytes();
        if [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
            return Err(ApiError::UnreadableBody);
        }
        return Uuid::parse_str(text)
            .map(Some)
            .map_err(|_| ApiError::UnreadableBody);
    }
    if text.len() == 24 {
        let bytes = decode_base64(text).ok_or(ApiError::UnreadableBody)?;
        let bytes: [u8; 16] = bytes.try_into().map_err(|_| ApiError::UnreadableBody)?;
        return Ok(Some(Uuid::from_bytes(bytes)));
    }
    Err(ApiError::UnreadableBody)
}

/// 표준 알파벳, `=` 패딩 base64.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.len() / 4;
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i + 1 == chunks;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut acc = 0u32;
        for &c in &chunk[..4 - pad] {
            acc = (acc << 6) | value(c)?;
        }
        acc <<= 6 * pad as u32;
        let triple = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&triple[..3 - pad]);
    }
    Some(out)
}

/// 정수로 읽을 수 없는 값(`필드: 요청 값의 형식이 올바르지 않습니다.`가 된다).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotAnInteger;

/// Spring `@ModelAttribute`가 `Integer`로 바꾸는 규칙(`StringToNumber` + `NumberUtils.parseNumber`).
///
/// - 빈 문자열은 `None`(값 없음). 그 밖에는 모든 공백을 지운 뒤 읽는다.
/// - `0x`/`0X`/`#` 접두(앞에 `-` 허용)는 16진수, 나머지는 부호(`+`,`-`)와 10진 숫자다.
/// - 32비트를 넘으면 형식 오류. 오류는 `Err(NotAnInteger)`로 알린다(호출 측이 `필드: 요청 값의 형식이...`를 만든다).
pub fn spring_integer(text: &str) -> Result<Option<i32>, NotAnInteger> {
    if text.is_empty() {
        return Ok(None);
    }
    let trimmed: String = text.chars().filter(|&c| !is_java_whitespace(c)).collect();
    // `isHexNumber`는 맨 앞의 `-` 하나만 건너뛰고 접두를 본다. `+0x1`은 16진수가 아니다.
    let negative = trimmed.starts_with('-');
    let after_sign = trimmed.strip_prefix('-').unwrap_or(&trimmed);
    let hex_digits = ["0x", "0X", "#"]
        .iter()
        .find_map(|prefix| after_sign.strip_prefix(prefix));
    let magnitude: i64 = match hex_digits {
        Some(digits) => parse_digits(digits, 16)?,
        None => {
            // `Integer.valueOf`는 `+`/`-` 하나만 받고 숫자는 10진수다.
            let digits = if negative {
                after_sign
            } else {
                after_sign.strip_prefix('+').unwrap_or(after_sign)
            };
            parse_digits(digits, 10)?
        }
    };
    let value = if negative { -magnitude } else { magnitude };
    i32::try_from(value).map(Some).map_err(|_| NotAnInteger)
}

fn parse_digits(digits: &str, radix: u32) -> Result<i64, NotAnInteger> {
    if digits.is_empty() {
        return Err(NotAnInteger);
    }
    let mut value: i64 = 0;
    for c in digits.chars() {
        let d = c.to_digit(radix).ok_or(NotAnInteger)?;
        value = value
            .checked_mul(i64::from(radix))
            .and_then(|v| v.checked_add(i64::from(d)))
            .ok_or(NotAnInteger)?;
        if value > i64::from(i32::MAX) + 1 {
            return Err(NotAnInteger);
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // CanonicalUuidParserTest
    #[test]
    fn 표준_uuid만_경로_변수로_받는다() {
        let s = "0198f6c1-62ba-7d30-8b12-0f733b6570b2";
        assert_eq!(parse_canonical_uuid(s).unwrap().to_string(), s);
        assert!(parse_canonical_uuid(&s.to_uppercase()).is_ok());
        for bad in [
            "",
            "abc",
            "1-2-3-4-5",
            "0198f6c1-62ba-7d30-8b12-0f733b6570b",
            " ",
        ] {
            let e = parse_canonical_uuid(bad).unwrap_err();
            assert_eq!(e.status(), 400, "{bad}");
            assert_eq!(e.message(), "ID 형식이 올바르지 않습니다.");
        }
    }

    #[test]
    fn jackson_uuid는_36자_하이픈_형식과_24자_base64를_읽는다() {
        let s = "0198f6c1-62ba-7d30-8b12-0f733b6570b2";
        let id = jackson_uuid(Some(&json!(s))).unwrap().unwrap();
        assert_eq!(id.to_string(), s);
        assert_eq!(
            jackson_uuid(Some(&json!(s.to_uppercase()))).unwrap(),
            Some(id)
        );
        assert_eq!(jackson_uuid(None).unwrap(), None);
        assert_eq!(jackson_uuid(Some(&Value::Null)).unwrap(), None);
        assert_eq!(jackson_uuid(Some(&json!(""))).unwrap(), None);
        // 24자 base64: 16바이트
        assert_eq!(
            jackson_uuid(Some(&json!("AAAAAAAAAAAAAAAAAAAAAA==")))
                .unwrap()
                .unwrap(),
            Uuid::nil()
        );
        for bad in [
            json!("abc"),
            json!("0198f6c1062ba-7d30-8b12-0f733b6570b2x"),
            json!("0198f6c1-62ba-7d30-8b12-0f733b6570bz"),
            json!(" 0198f6c1-62ba-7d30-8b12-0f733b6570b"),
            json!(1),
            json!(true),
            json!([]),
            json!({}),
        ] {
            assert!(
                matches!(jackson_uuid(Some(&bad)), Err(ApiError::UnreadableBody)),
                "{bad}"
            );
        }
    }

    #[test]
    fn spring_integer는_공백을_지우고_16진수와_부호를_읽는다() {
        assert_eq!(spring_integer(""), Ok(None));
        assert_eq!(spring_integer("12"), Ok(Some(12)));
        assert_eq!(spring_integer(" 1 2 "), Ok(Some(12)));
        assert_eq!(spring_integer("+7"), Ok(Some(7)));
        assert_eq!(spring_integer("-7"), Ok(Some(-7)));
        assert_eq!(spring_integer("007"), Ok(Some(7)));
        assert_eq!(spring_integer("0x10"), Ok(Some(16)));
        assert_eq!(spring_integer("#1F"), Ok(Some(31)));
        assert_eq!(spring_integer("-0x1"), Ok(Some(-1)));
        assert_eq!(spring_integer("+0x1"), Err(NotAnInteger));
        assert_eq!(spring_integer("2147483647"), Ok(Some(i32::MAX)));
        assert_eq!(spring_integer("-2147483648"), Ok(Some(i32::MIN)));
        for bad in [
            "2147483648",
            "abc",
            " ",
            "1.5",
            "--1",
            "+-1",
            "-+1",
            "0x",
            "1e3",
        ] {
            assert_eq!(spring_integer(bad), Err(NotAnInteger), "{bad}");
        }
    }
}
