//! 원본 Spring Framework 7.0.8(Spring Boot 4.1.0) `CronExpression`과 같은 규칙인지 확인한다.
//!
//! fixture는 원본이 쓰는 라이브러리의 `CronExpression.parse(expr).next(ZonedDateTime)`을 직접 실행해 얻었다.
//! 모든 계산은 `Asia/Seoul` 기준이다(원본 `@Scheduled(zone = "Asia/Seoul")`).
//!
//! - `spring_cron_next.tsv`: `식<TAB>기준 시각(UTC)<TAB>다음 실행 시각(UTC) 또는 none`
//! - `spring_cron_parse.tsv`: `식<TAB>OK|ERR` (Spring이 받아들이는지)
//!
//! 원본이 실제로 쓰는 식: `0 30 4 * * *`, `0 40 4 * * *`, `0 50 4 * * *`, `0 0 * * * *`.

use chalkak_core::scheduler::{CronExpression, CronSchedule, SEOUL};
use jiff::Timestamp;

const NEXT: &str = include_str!("fixtures/spring_cron_next.tsv");
const PARSE: &str = include_str!("fixtures/spring_cron_parse.tsv");

/// 원본 `@Scheduled` 4종.
const ORIGINAL_CRONS: [&str; 4] = [
    "0 30 4 * * *", // RefreshTokenCleanupScheduler
    "0 0 * * * *",  // PendingAppleAuthorizationCleanupScheduler
    "0 40 4 * * *", // ConsumedSignupTokenCleanupScheduler
    "0 50 4 * * *", // NotificationCleanupScheduler
];

#[test]
fn 다음_실행_시각이_spring과_같다() {
    let mut checked = 0;
    let mut original = 0;
    for line in NEXT.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let [expr, from, expected] = cols[..] else {
            panic!("fixture 형식 오류: {line:?}");
        };
        let schedule = CronSchedule::parse(expr, SEOUL)
            .unwrap_or_else(|e| panic!("Spring이 받는 식 {expr:?}를 거절했다: {e}"));
        let actual = schedule
            .next_after(from.parse::<Timestamp>().unwrap())
            .map_or_else(|| "none".to_owned(), |t| t.to_string());
        assert_eq!(actual, expected, "식 {expr:?}, 기준 {from}");
        checked += 1;
        if ORIGINAL_CRONS.contains(&expr) {
            original += 1;
        }
    }
    assert!(checked >= 300, "fixture 줄 수 {checked}");
    assert!(original >= 4 * 8, "원본 식 검증 수 {original}");
}

#[test]
fn 받아들이는_식과_거절하는_식이_spring과_같다() {
    let mut checked = 0;
    for line in PARSE.lines() {
        let (expr, verdict) = line.split_once('\t').expect("탭으로 나뉜 줄");
        let ours = CronExpression::parse(expr).is_ok();
        match verdict {
            "OK" => assert!(ours, "Spring은 받지만 거절했다: {expr:?}"),
            "ERR" => assert!(!ours, "Spring은 거절하지만 받았다: {expr:?}"),
            other => panic!("알 수 없는 판정 {other}"),
        }
        checked += 1;
    }
    assert!(checked >= 60, "fixture 줄 수 {checked}");
}

#[test]
fn 원본_식_네_개의_연속_실행_시각() {
    // 2026-10-12 00:00:00Z(= 서울 09:00)부터 사흘간. 서울 기준 04:30/04:40/04:50은 UTC 19:30/19:40/19:50.
    let from: Timestamp = "2026-10-12T00:00:00Z".parse().unwrap();
    let cases = [
        ("0 30 4 * * *", "19:30:00"),
        ("0 40 4 * * *", "19:40:00"),
        ("0 50 4 * * *", "19:50:00"),
    ];
    for (expr, utc_time) in cases {
        let schedule = CronSchedule::parse(expr, SEOUL).unwrap();
        let mut at = from;
        for day in 12..15 {
            at = schedule.next_after(at).unwrap();
            assert_eq!(
                at.to_string(),
                format!("2026-10-{day}T{utc_time}Z"),
                "{expr}"
            );
        }
    }
    // 매시 정각
    let hourly = CronSchedule::parse("0 0 * * * *", SEOUL).unwrap();
    let mut at = from;
    for hour in 1..=3 {
        at = hourly.next_after(at).unwrap();
        assert_eq!(at.to_string(), format!("2026-10-12T0{hour}:00:00Z"));
    }
}
