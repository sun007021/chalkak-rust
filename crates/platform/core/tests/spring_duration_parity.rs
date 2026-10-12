//! 원본 Spring Boot 4.1.0 `DurationStyle.detectAndParse`(SIMPLE·ISO8601)와 같은 규칙인지 확인한다.
//!
//! `tests/fixtures/spring_duration.tsv`는 원본이 쓰는 Spring Boot 버전의 `DurationStyle`을 직접 실행해 뽑은
//! 값이다(`입력<TAB>OK|NEG|ERR [초 나노]`). 원본 설정에서 실제로 쓰는 값(`15m`, `30d`, `90d`, `7m`, `PT1H`,
//! `P30D`, `PT7M` 등 `application.yml`·`deploy/examples/*.env.example`)이 포함돼 있다.
//!
//! Rust의 `std::time::Duration`은 음수를 표현하지 못하므로 Spring이 음수(`NEG`)로 해석한 입력은 오류로 거절한다.
//! 만료 시간·타임아웃에 음수는 의미가 없어 기동 시점에 걸러내는 편이 안전하다.

use std::time::Duration;

use chalkak_core::{ConfigError, parse_duration};

const FIXTURE: &str = include_str!("fixtures/spring_duration.tsv");

#[test]
fn spring_duration_style_결과와_같다() {
    let mut checked = 0;
    for line in FIXTURE.lines() {
        let (input, expected) = line.split_once('\t').expect("탭으로 나뉜 줄");
        let actual = parse_duration(input);
        let parts: Vec<&str> = expected.split(' ').collect();
        match parts[0] {
            "OK" => {
                let secs: u64 = parts[1].parse().unwrap();
                let nanos: u32 = parts[2].parse().unwrap();
                assert_eq!(
                    actual.as_ref().ok(),
                    Some(&Duration::new(secs, nanos)),
                    "입력 {input:?}"
                );
            }
            "NEG" | "ERR" => {
                assert!(
                    matches!(actual, Err(ConfigError::InvalidDuration { .. })),
                    "입력 {input:?}는 거절돼야 한다: {actual:?}"
                );
            }
            other => panic!("알 수 없는 기대값 {other}"),
        }
        checked += 1;
    }
    assert!(checked >= 90, "fixture 줄 수 {checked}");
}

#[test]
fn 원본_설정의_대표_값() {
    // application.yml: social-signup-token.expiration, deploy/examples/*.env.example
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
    assert_eq!(
        parse_duration("30d").unwrap(),
        Duration::from_secs(30 * 86_400)
    );
    assert_eq!(
        parse_duration("90d").unwrap(),
        Duration::from_secs(90 * 86_400)
    );
    assert_eq!(parse_duration("7m").unwrap(), Duration::from_secs(420));
    assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    // application.yml 주석: "7m, 1h, PT7M 형식을 모두 받는다."
    assert_eq!(
        parse_duration("PT7M").unwrap(),
        parse_duration("7m").unwrap()
    );
    // infrastructure-setup.md: "`30m`, `PT1H` 형식을 모두 받는다."
    assert_eq!(
        parse_duration("PT1H").unwrap(),
        parse_duration("1h").unwrap()
    );
    // 2s·3s: chalkak.admin.notification.slack connect-timeout/read-timeout
    assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
    assert_eq!(parse_duration("3s").unwrap(), Duration::from_secs(3));
}

#[test]
fn 단위_없는_숫자는_밀리초다() {
    // Spring DurationStyle.SIMPLE: 단위를 생략하면 기본 단위(ms)
    assert_eq!(parse_duration("100").unwrap(), Duration::from_millis(100));
    assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
}

#[test]
fn 오류는_입력값과_이유를_담는다() {
    let err = parse_duration("1.5h").unwrap_err();
    assert!(err.to_string().contains("1.5h"), "{err}");
}
