//! SQS 본문을 푸시 메시지로 읽는 규칙이 원본 JAR(Jackson 3)과 같은지 기록 전체로 확인한다.
//!
//! `fixtures/expected_push_message.tsv`는 `fixtures/PushMessageFixture.java`를 원본 `app.jar`에 돌려 얻은 기록이다
//! (`run_push_message.sh`). 한 행이 본문과 결과(읽은 레코드 또는 예외의 단순 이름)를 담는다.
//! 연도가 ±9999를 넘는 시각(`jiff::Timestamp`의 한계)과 음수 연도 표기는 다르게 다루므로 제외한다.

use chalkak_app::notification::push_message::{RawPushMessage, parse_push_message};
use chalkak_app::web::json::format_instant;

const EXPECTED: &str = include_str!("fixtures/expected_push_message.tsv");

/// 기록에서 제외하는 행: 시각이 `jiff` 범위를 넘거나 연도가 음수다.
const SKIPPED: &[&str] = &[
    "occurredAt_year_plus",
    "occurredAt_min_year",
    "occurredAt_very_early",
    "occurredAt_max",
    "expiresAt_year_plus",
    "expiresAt_min_year",
    "expiresAt_very_early",
    "expiresAt_max",
];

fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn render(message: &RawPushMessage) -> String {
    let text = |value: Option<String>| value.unwrap_or_else(|| "null".to_owned());
    format!(
        "PushMessage[eventId={}, notificationId={}, userId={}, type={}, sourceType={}, sourceId={}, occurredAt={}, expiresAt={}]",
        text(message.event_id.map(|v| v.to_string())),
        text(message.notification_id.map(|v| v.to_string())),
        text(message.user_id.map(|v| v.to_string())),
        text(message.kind.map(|v| v.as_str().to_owned())),
        text(message.source_type.map(|v| v.as_str().to_owned())),
        text(message.source_id.map(|v| v.to_string())),
        text(message.occurred_at.map(format_instant)),
        text(message.expires_at.map(format_instant)),
    )
}

#[test]
fn 본문_해석은_원본_기록과_같다() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for line in EXPECTED.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let (label, body, expected) = (cols[0], unescape(cols[1]), cols[2]);
        if SKIPPED.contains(&label) {
            continue;
        }
        let actual = match parse_push_message(&body) {
            Ok(Some(message)) => format!("OK {}", render(&message)),
            Ok(None) => "OK null".to_owned(),
            Err(name) => format!("EXC {name}"),
        };
        if actual != expected {
            failures.push(format!("{label}: 기대 {expected}\n    실제 {actual}"));
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(checked > 170, "비교한 행이 너무 적다: {checked}");
}
