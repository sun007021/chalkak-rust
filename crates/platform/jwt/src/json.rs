//! 원본(Nimbus + Gson)과 같은 규칙의 컴팩트 JSON 직렬화.
//!
//! `serde_json::to_string`과 다른 점은 문자열 이스케이프 하나다. Nimbus는 U+2028·U+2029를 ` `·` `로
//! 쓰고, serde_json은 그대로 둔다. 나머지(`"` `\` `\b` `\f` `\n` `\r` `\t`, 그 밖의 제어 문자는 `\u00xx` 소문자,
//! `/`·`<`·`>`·`&`·`'`·`=`·비ASCII는 그대로)는 같다. 객체 키는 `Value`가 가진 순서 그대로 쓴다.

use serde_json::Value;

pub(crate) fn to_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
