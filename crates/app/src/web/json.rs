//! Jackson 호환 JSON 출력.
//!
//! 응답 본문은 원본과 바이트까지 같아야 한다(하네스가 본문을 그대로 비교한다).
//! - 필드 순서는 구조체 선언 순서(Jackson record와 같다), 공백 없음.
//! - 문자열 이스케이프: `" \ \b \f \n \r \t`는 짧은 이스케이프, 그 밖의 0x20 미만은 `\u00XX`(**대문자** 16진수).
//!   serde_json 기본은 소문자라 [`JacksonFormatter`]로 바꾼다. `/`와 0x7F 이상은 이스케이프하지 않는다.
//! - `Instant`는 `DateTimeFormatter.ISO_INSTANT`: 소수 초는 0, 3, 6, 9자리 중 가장 짧게([`format_instant`]).

use std::io;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Serialize, Serializer};
use serde_json::ser::{CharEscape, Formatter};

struct JacksonFormatter;

impl Formatter for JacksonFormatter {
    fn write_char_escape<W>(&mut self, writer: &mut W, char_escape: CharEscape) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        match char_escape {
            CharEscape::AsciiControl(byte) => write!(writer, "\\u{byte:04X}"),
            CharEscape::Quote => writer.write_all(b"\\\""),
            CharEscape::ReverseSolidus => writer.write_all(b"\\\\"),
            CharEscape::Solidus => writer.write_all(b"\\/"),
            CharEscape::Backspace => writer.write_all(b"\\b"),
            CharEscape::FormFeed => writer.write_all(b"\\f"),
            CharEscape::LineFeed => writer.write_all(b"\\n"),
            CharEscape::CarriageReturn => writer.write_all(b"\\r"),
            CharEscape::Tab => writer.write_all(b"\\t"),
        }
    }
}

/// 값을 Jackson과 같은 모양의 JSON 문자열로 만든다.
pub fn to_json<T: Serialize>(value: &T) -> String {
    let mut out = Vec::with_capacity(128);
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, JacksonFormatter);
    // 메모리에 쓰고 문자열·숫자·구조체만 직렬화하므로 실패하지 않는다(맵 키가 문자열이 아닌 경우 등은 타입이 막는다).
    value
        .serialize(&mut serializer)
        .expect("JSON 직렬화는 실패하지 않아야 한다");
    String::from_utf8(out).expect("serde_json은 UTF-8만 쓴다")
}

/// `Instant`의 JSON 표기(`2026-10-12T00:00:00Z`, `...00.123Z`, `...00.123456Z`).
pub fn format_instant(ts: Timestamp) -> String {
    let z = ts.to_zoned(TimeZone::UTC);
    let nanos = z.subsec_nanosecond();
    let fraction = if nanos == 0 {
        String::new()
    } else if nanos % 1_000_000 == 0 {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}Z",
        z.year(),
        z.month(),
        z.day(),
        z.hour(),
        z.minute(),
        z.second()
    )
}

/// `#[serde(serialize_with = "serialize_instant")]`용.
pub fn serialize_instant<S: Serializer>(ts: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format_instant(*ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Sample<'a> {
        b: &'a str,
        a: u32,
    }

    #[test]
    fn 필드는_선언_순서이고_공백이_없다() {
        assert_eq!(to_json(&Sample { b: "x", a: 1 }), r#"{"b":"x","a":1}"#);
    }

    #[test]
    fn 제어문자_이스케이프는_jackson처럼_대문자_16진수다() {
        // Jackson: "\u001F", serde_json 기본: "\u001f"
        let s = to_json(&Sample {
            b: "a\u{1f}b\u{1}\u{8}\u{c}\n\r\t\"\\/\u{7f}가",
            a: 0,
        });
        assert_eq!(
            s,
            "{\"b\":\"a\\u001Fb\\u0001\\b\\f\\n\\r\\t\\\"\\\\/\u{7f}가\",\"a\":0}"
        );
    }

    #[test]
    fn instant는_iso_instant_자릿수_규칙을_따른다() {
        let t = |s: &str| format_instant(s.parse().unwrap());
        assert_eq!(t("2026-10-12T00:00:00Z"), "2026-10-12T00:00:00Z");
        assert_eq!(t("2026-10-12T00:00:00.5Z"), "2026-10-12T00:00:00.500Z");
        assert_eq!(t("2026-10-12T00:00:00.123Z"), "2026-10-12T00:00:00.123Z");
        assert_eq!(
            t("2026-10-12T00:00:00.123400Z"),
            "2026-10-12T00:00:00.123400Z"
        );
        assert_eq!(
            t("2026-10-12T00:00:00.000001Z"),
            "2026-10-12T00:00:00.000001Z"
        );
        assert_eq!(
            t("2026-10-12T00:00:00.123456789Z"),
            "2026-10-12T00:00:00.123456789Z"
        );
        // 시간대가 있는 입력도 UTC로 쓴다.
        assert_eq!(t("2026-10-12T09:00:00+09:00"), "2026-10-12T00:00:00Z");
    }
}
