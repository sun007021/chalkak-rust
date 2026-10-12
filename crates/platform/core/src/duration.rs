//! 기간 문자열 파서. 원본이 쓰는 Spring Boot `DurationStyle`(SIMPLE·ISO8601)과 같은 규칙이다.
//!
//! - SIMPLE: `^([+-]?\d+)([a-zA-Z]{0,2})$`. 단위는 대소문자 구분 없이 `ns`·`us`·`ms`·`s`·`m`·`h`·`d`이고,
//!   생략하면 밀리초다. 공백·소수·복합(`1h30m`)은 받지 않는다.
//! - ISO8601: `[+-]?P` 로 시작하면 `java.time.Duration.parse` 규칙(`PnDTnHnMn.nS`, 대소문자 무시, 주·월·년 없음).
//! - 판별: 맨 앞 부호를 뗀 첫 글자가 `P`/`p`이면 ISO8601, 아니면 SIMPLE.
//!
//! 두 규칙 모두 입력을 `trim`하지 않는다. 자릿수 범위도 Java와 같이 초 단위 `i64`를 넘으면 오류다.
//! 다른 점 하나: `std::time::Duration`은 음수를 표현하지 못하므로 음수 결과는 오류로 돌려준다.

use std::time::Duration;

use crate::config::ConfigError;

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// 기간 문자열을 `Duration`으로 바꾼다. `15m`, `1h`, `30d`, `7m`, `PT1H`, `P30D` 등.
pub fn parse_duration(input: &str) -> Result<Duration, ConfigError> {
    let invalid = |reason: &'static str| ConfigError::InvalidDuration {
        input: input.to_owned(),
        reason,
    };
    let unsigned = input.strip_prefix(['+', '-']).unwrap_or(input);
    let nanos = if unsigned.starts_with(['P', 'p']) {
        parse_iso8601(input).ok_or_else(|| invalid("올바른 ISO-8601 기간이 아닙니다"))?
    } else {
        parse_simple(input)
            .ok_or_else(|| invalid("올바른 기간이 아닙니다 (예: 15m, 1h, 30d, PT1H)"))?
    };
    if nanos < 0 {
        return Err(invalid("음수 기간은 사용할 수 없습니다"));
    }
    let secs = u64::try_from(nanos / NANOS_PER_SEC).map_err(|_| invalid("범위를 벗어났습니다"))?;
    // 나머지는 0..1e9라 u32에 들어간다.
    let sub = u32::try_from(nanos % NANOS_PER_SEC).map_err(|_| invalid("범위를 벗어났습니다"))?;
    Ok(Duration::new(secs, sub))
}

/// 총 나노초. Java `Duration`이 담을 수 있는 범위(초 `i64`)를 넘으면 `None`.
fn checked_total(secs: i128, nanos: i128) -> Option<i128> {
    let total = secs.checked_mul(NANOS_PER_SEC)?.checked_add(nanos)?;
    let secs_floor = total.div_euclid(NANOS_PER_SEC);
    i64::try_from(secs_floor).ok()?;
    Some(total)
}

fn parse_simple(input: &str) -> Option<i128> {
    let bytes = input.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    let number: i64 = input[..i].parse().ok()?; // Long.parseLong: "+100"도 허용, 범위 밖은 오류
    let suffix = &input[i..];
    if suffix.len() > 2 || !suffix.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let unit_nanos: i128 = match suffix.to_ascii_lowercase().as_str() {
        "" | "ms" => 1_000_000,
        "ns" => 1,
        "us" => 1_000,
        "s" => NANOS_PER_SEC,
        "m" => 60 * NANOS_PER_SEC,
        "h" => 3_600 * NANOS_PER_SEC,
        "d" => 86_400 * NANOS_PER_SEC,
        _ => return None,
    };
    // Duration.ofDays 등은 초가 i64를 넘으면 ArithmeticException이다.
    checked_total(0, i128::from(number) * unit_nanos)
}

/// `[+-]?[0-9]+` 한 덩어리. 19자리를 넘으면 `Long.parseLong`처럼 실패시킨다.
fn take_signed_int(s: &[u8], pos: &mut usize) -> Option<i64> {
    let start = *pos;
    let mut i = start;
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits_start = i;
    while s.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    let text = std::str::from_utf8(&s[start..i]).ok()?;
    let value = text.parse().ok()?;
    *pos = i;
    Some(value)
}

fn parse_iso8601(input: &str) -> Option<i128> {
    let s = input.as_bytes();
    let mut pos = 0;
    let negate = match s.first() {
        Some(b'-') => {
            pos += 1;
            true
        }
        Some(b'+') => {
            pos += 1;
            false
        }
        _ => false,
    };
    if !matches!(s.get(pos), Some(b'P' | b'p')) {
        return None;
    }
    pos += 1;

    let mut total_secs: i64 = 0;
    let mut fraction: i128 = 0;
    let mut any = false;

    // 일(D)
    let mut probe = pos;
    if let Some(days) = take_signed_int(s, &mut probe)
        && matches!(s.get(probe), Some(b'D' | b'd'))
    {
        total_secs = total_secs.checked_add(days.checked_mul(86_400)?)?;
        pos = probe + 1;
        any = true;
    }

    // 시간부(T)
    if matches!(s.get(pos), Some(b'T' | b't')) {
        pos += 1;
        let mut time_any = false;

        let mut probe = pos;
        if let Some(hours) = take_signed_int(s, &mut probe)
            && matches!(s.get(probe), Some(b'H' | b'h'))
        {
            total_secs = total_secs.checked_add(hours.checked_mul(3_600)?)?;
            pos = probe + 1;
            time_any = true;
        }

        let mut probe = pos;
        if let Some(minutes) = take_signed_int(s, &mut probe)
            && matches!(s.get(probe), Some(b'M' | b'm'))
        {
            total_secs = total_secs.checked_add(minutes.checked_mul(60)?)?;
            pos = probe + 1;
            time_any = true;
        }

        let mut probe = pos;
        let seconds_negative = s.get(probe) == Some(&b'-');
        if let Some(seconds) = take_signed_int(s, &mut probe) {
            let mut digits = String::new();
            let mut frac_end = probe;
            if matches!(s.get(probe), Some(b'.' | b',')) {
                frac_end = probe + 1;
                while s.get(frac_end).is_some_and(u8::is_ascii_digit) {
                    digits.push(char::from(s[frac_end]));
                    frac_end += 1;
                }
            }
            if digits.len() <= 9 && matches!(s.get(frac_end), Some(b'S' | b's')) {
                while digits.len() < 9 {
                    digits.push('0');
                }
                let nanos: i128 = digits.parse().ok()?;
                total_secs = total_secs.checked_add(seconds)?;
                fraction = if seconds_negative { -nanos } else { nanos };
                pos = frac_end + 1;
                time_any = true;
            }
        }

        if !time_any {
            return None; // "PT", "P1DT"
        }
        any = true;
    }

    if !any || pos != s.len() {
        return None;
    }
    let total = checked_total(i128::from(total_secs), fraction)?;
    Some(if negate { -total } else { total })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn simple_단위_전부() {
        assert_eq!(parse_duration("1ns").unwrap(), Duration::from_nanos(1));
        assert_eq!(parse_duration("1us").unwrap(), Duration::from_micros(1));
        assert_eq!(parse_duration("1ms").unwrap(), Duration::from_millis(1));
        assert_eq!(parse_duration("1s").unwrap(), secs(1));
        assert_eq!(parse_duration("1m").unwrap(), secs(60));
        assert_eq!(parse_duration("1h").unwrap(), secs(3_600));
        assert_eq!(parse_duration("1d").unwrap(), secs(86_400));
    }

    #[test]
    fn simple_단위는_대소문자를_구분하지_않는다() {
        assert_eq!(parse_duration("15M").unwrap(), secs(900));
        assert_eq!(parse_duration("1MS").unwrap(), Duration::from_millis(1));
    }

    #[test]
    fn iso8601_은_대소문자를_구분하지_않는다() {
        assert_eq!(parse_duration("pt1h").unwrap(), secs(3_600));
        assert_eq!(parse_duration("p1d").unwrap(), secs(86_400));
    }

    #[test]
    fn iso8601_일과_시간을_합친다() {
        assert_eq!(parse_duration("P1DT2H").unwrap(), secs(26 * 3_600));
        assert_eq!(parse_duration("PT1H30M").unwrap(), secs(5_400));
        assert_eq!(
            parse_duration("PT1M1.5S").unwrap(),
            Duration::new(61, 500_000_000)
        );
    }

    #[test]
    fn 음수_결과는_오류다() {
        for input in ["-5s", "PT-5M", "-PT5M", "-100"] {
            assert!(parse_duration(input).is_err(), "{input}");
        }
        assert_eq!(parse_duration("-0s").unwrap(), Duration::ZERO);
    }

    #[test]
    fn 오류에_입력이_담긴다() {
        let ConfigError::InvalidDuration { input, .. } = parse_duration("abc").unwrap_err() else {
            panic!("InvalidDuration이어야 한다");
        };
        assert_eq!(input, "abc");
    }
}
