//! FCM HTTP v1 `messages:send` 요청 본문. Firebase Admin SDK 9.11.0(`Message`·`AndroidConfig`·`ApnsConfig`)이 만드는 JSON과
//! 바이트가 같다(`tests/fixtures/expected_fcm.tsv`).
//!
//! - 키 순서는 SDK 직렬화 순서다: `message` 안은 `android`, `apns`, `data`, `notification`, `token`. `data`와 APNs `headers`는
//!   SDK가 `HashMap`으로 들고 있어 키 해시 순서다(`data`: sourceId, eventId, sourceType, notificationId, type /
//!   `headers`: apns-push-type, apns-expiration).
//! - 문자열 이스케이프: `"` `\` `\b` `\t` `\n` `\f` `\r`는 짧은 이스케이프, 그 밖의 U+0020 미만은 `\u00XX`(16진수 대문자),
//!   U+007F~U+FFFF는 그대로, 보조 평면(U+10000 이상)은 대리쌍 `😀`(대문자)로 쓴다. `/`·`<`·`&`·`'`·`=`는 그대로다.
//! - `ttl`은 `<초>s`, 밀리초 부분이 있으면 소수 9자리(`1800.123000000s`)다.

use std::fmt::Write as _;

/// 보낼 메시지 하나. 시각에 의존하는 값(`ttl`, `apns_expiration`)은 만들 때 한 번 정해서 SDK의 재시도에도 바뀌지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FcmMessage {
    pub token: String,
    pub title: String,
    pub body: String,
    /// `eventId` 등 화면 이동 정보. SDK 해시 순서로 이미 정렬되어 있다.
    pub data: Vec<(&'static str, String)>,
    /// Android 수명(밀리초, 0 이상)
    pub ttl_millis: i64,
    /// APNs 절대 만료(Unix 초)
    pub apns_expiration: i64,
}

impl FcmMessage {
    /// 요청 본문(`{"message":{...}}`)
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("{\"message\":{\"android\":{\"ttl\":\"");
        out.push_str(&ttl_text(self.ttl_millis));
        out.push_str(
            "\"},\"apns\":{\"headers\":{\"apns-push-type\":\"alert\",\"apns-expiration\":\"",
        );
        let _ = write!(out, "{}", self.apns_expiration);
        out.push_str("\"},\"payload\":{\"aps\":{\"sound\":\"default\"}}},\"data\":{");
        for (i, (key, value)) in self.data.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_string(&mut out, key);
            out.push(':');
            write_string(&mut out, value);
        }
        out.push_str("},\"notification\":{\"body\":");
        write_string(&mut out, &self.body);
        out.push_str(",\"title\":");
        write_string(&mut out, &self.title);
        out.push_str("},\"token\":");
        write_string(&mut out, &self.token);
        out.push_str("}}");
        out
    }
}

/// `Duration` 문자열: 초 단위 정수, 밀리초가 남으면 소수 9자리.
fn ttl_text(millis: i64) -> String {
    let seconds = millis / 1000;
    let nanos = (millis % 1000) * 1_000_000;
    if nanos == 0 {
        format!("{seconds}s")
    } else {
        format!("{seconds}.{nanos:09}s")
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c if (c as u32) > 0xFFFF => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04X}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(title: &str) -> FcmMessage {
        FcmMessage {
            token: "t".into(),
            title: title.into(),
            body: "b".into(),
            data: vec![("sourceId", "s".into()), ("type", "TOPIC".into())],
            ttl_millis: 1_800_000,
            apns_expiration: 1_791_333_000,
        }
    }

    // FcmDevicePushSenderTest#send_accepted_sendsPlatformExpirationAndNavigation 의 요청 모양
    #[test]
    fn 본문은_sdk_직렬화_순서다() {
        assert_eq!(
            message("제목").to_json(),
            concat!(
                r#"{"message":{"android":{"ttl":"1800s"},"apns":{"headers":{"apns-push-type":"alert","apns-expiration":"1791333000"},"#,
                r#""payload":{"aps":{"sound":"default"}}},"data":{"sourceId":"s","type":"TOPIC"},"#,
                r#""notification":{"body":"b","title":"제목"},"token":"t"}}"#
            )
        );
    }

    #[test]
    fn ttl은_밀리초가_있을_때만_소수_9자리다() {
        for (millis, expected) in [
            (1_800_000, "1800s"),
            (1_800_123, "1800.123000000s"),
            (1_500, "1.500000000s"),
            (500, "0.500000000s"),
            (1_000, "1s"),
            (0, "0s"),
        ] {
            assert_eq!(ttl_text(millis), expected);
        }
    }

    #[test]
    fn 이스케이프는_sdk_규칙이다() {
        let json =
            message("a\"\\/<&='\u{8}\t\n\u{c}\r\u{1}\u{1f}\u{7f}\u{a0}\u{2028}\u{ffff}\u{1f600}")
                .to_json();
        assert!(
            json.contains(
                "\"title\":\"a\\\"\\\\/<&='\\b\\t\\n\\f\\r\\u0001\\u001F\u{7f}\u{a0}\u{2028}\u{ffff}\\uD83D\\uDE00\""
            ),
            "{json}"
        );
    }
}
