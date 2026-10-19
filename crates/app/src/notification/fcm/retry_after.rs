//! `Retry-After` 헤더 해석. 원본 `FcmDevicePushSender.parseRetryAfter`와 SDK의 503 재시도.
//!
//! - 초 단위 값은 `Long.parseLong`과 같다: 선택적 부호(`+`·`-`)와 숫자만, 공백·소수·16진수·밑줄은 거절한다.
//! - 날짜 값은 `DateTimeFormatter.RFC_1123_DATE_TIME`과 같다(`FcmFixture`가 원본에 돌려 확인한 규칙):
//!   `[요일, ]일 월 연 시:분[:초] GMT|±HHMM`. 요일은 3글자 약어(있으면 날짜와 맞아야 한다), 월·요일·`GMT`는 대소문자를 가리지
//!   않는다. 일·연·시·분·초는 1~9자리 숫자(느슨한 해석)이고 일이 그 달 길이를 넘으면 마지막 날로 맞춘다(11월 31일은 30일).
//!   시는 24:00:00(또는 24:00)만 다음 날 0시로 받는다. 오프셋은 `±HHMM`이며 18시간 이하다. 단어 사이 공백은 정확히 하나다.

/// `Long.parseLong`: 10진수, 선택적 `+`/`-` 부호. 범위를 벗어나면 `None`.
pub fn parse_seconds(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.strip_prefix('+').unwrap_or(text).parse().ok()
}

const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// `RFC_1123_DATE_TIME` 해석. 에포크 초를 돌려주며(연이 아주 커도 받는다), 해석할 수 없거나 값이 올바르지 않으면 `None`.
pub fn parse_http_date(text: &str) -> Option<i64> {
    let mut rest = text;
    let mut weekday = None;
    if let Some((name, after)) = rest.split_at_checked(3)
        && let Some(index) = WEEKDAYS.iter().position(|w| name.eq_ignore_ascii_case(w))
        && let Some(after) = after.strip_prefix(", ")
    {
        weekday = Some(index as i64);
        rest = after;
    }
    let (day, rest) = number(rest)?;
    let rest = rest.strip_prefix(' ')?;
    let (month_name, rest) = rest.split_at_checked(3)?;
    let month = MONTHS
        .iter()
        .position(|m| month_name.eq_ignore_ascii_case(m))? as i64
        + 1;
    let rest = rest.strip_prefix(' ')?;
    let (year, rest) = number(rest)?;
    let rest = rest.strip_prefix(' ')?;
    let (hour, rest) = number(rest)?;
    let rest = rest.strip_prefix(':')?;
    let (minute, mut rest) = number(rest)?;
    let mut second = 0;
    if let Some(after) = rest.strip_prefix(':') {
        let (value, after) = number(after)?;
        second = value;
        rest = after;
    }
    let zone = rest.strip_prefix(' ')?;
    let offset = if zone.eq_ignore_ascii_case("GMT") {
        0
    } else {
        offset_seconds(zone)?
    };

    if !(1..=31).contains(&day) || minute > 59 || second > 59 {
        return None;
    }
    // 24:00:00 은 다음 날 0시다. 그 밖의 24시는 거절한다.
    let mut extra_day = 0;
    if hour == 24 {
        if minute != 0 || second != 0 {
            return None;
        }
        extra_day = 1;
    } else if hour > 23 {
        return None;
    }
    let day = day.min(days_in_month(year, month));
    let days = days_from_civil(year, month, day);
    if let Some(expected) = weekday
        && (days + 4).rem_euclid(7) != expected
    {
        return None;
    }
    let local = (days + extra_day) * 86_400 + hour % 24 * 3_600 + minute * 60 + second;
    Some(local - offset)
}

/// 1~9자리 10진수. 느슨한 해석이라 앞의 0과 한 자리 값도 받는다.
fn number(text: &str) -> Option<(i64, &str)> {
    let end = text
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(text.len());
    if end == 0 || end > 9 {
        return None;
    }
    Some((text[..end].parse().ok()?, &text[end..]))
}

/// `±HHMM`(18:00 이하)
fn offset_seconds(text: &str) -> Option<i64> {
    let (sign, digits) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    if minutes > 59 || hours * 60 + minutes > 18 * 60 {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// 1970-01-01부터의 일수(양력을 과거로도 늘린 ISO 달력).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Option<i64> {
        parse_http_date(text)
    }

    const BASE: i64 = 1_791_331_200; // 2026-10-07T00:00:00Z (수요일)

    // FcmDevicePushSenderTest#send_retryAfterHeader_parsesDateOrUsesMinimum
    #[test]
    fn 표준_날짜를_해석한다() {
        assert_eq!(at("Wed, 07 Oct 2026 00:02:00 GMT"), Some(BASE + 120));
    }

    #[test]
    fn 초_값은_long_parse_long과_같다() {
        for (text, expected) in [
            ("120", Some(120)),
            ("+75", Some(75)),
            ("-1", Some(-1)),
            ("0090", Some(90)),
            ("-0", Some(0)),
            ("+", None),
            ("", None),
            ("90.5", None),
            ("1_000", None),
            ("0x50", None),
            (" 90", None),
            ("99999999999", Some(99_999_999_999)),
            ("99999999999999999999", None),
            ("invalid-header", None),
        ] {
            assert_eq!(parse_seconds(text), expected, "{text:?}");
        }
    }

    #[test]
    fn 날짜_규칙은_java_rfc_1123_date_time과_같다() {
        for (text, expected) in [
            // 요일은 선택이고 있으면 날짜와 맞아야 한다.
            ("07 Oct 2026 00:02:00 GMT", Some(BASE + 120)),
            ("Mon, 07 Oct 2026 00:02:00 GMT", None),
            ("Wednesday, 07 Oct 2026 00:05:00 GMT", None),
            ("Wed,07 Oct 2026 00:05:00 GMT", None),
            ("Wed , 07 Oct 2026 00:05:00 GMT", None),
            // 초는 선택, 대소문자는 가리지 않는다.
            ("Wed, 07 Oct 2026 00:03 GMT", Some(BASE + 180)),
            ("wed, 07 oct 2026 00:05:00 gmt", Some(BASE + 300)),
            // 한 자리 값은 받는다(느슨한 해석).
            ("Wed, 7 Oct 2026 00:04:00 GMT", Some(BASE + 240)),
            ("Wed, 07 Oct 2026 0:05:00 GMT", Some(BASE + 300)),
            ("Wed, 07 Oct 2026 00:5:00 GMT", Some(BASE + 300)),
            ("Wed, 07 Oct 2026 00:05:0 GMT", Some(BASE + 300)),
            // 연은 네 자리를 넘어도 된다.
            (
                "Wed, 07 Oct 12026 00:05:00 GMT",
                Some(BASE + 315_569_520_300),
            ),
            // 오프셋
            ("Wed, 07 Oct 2026 09:05:00 +0900", Some(BASE + 300)),
            ("Wed, 07 Oct 2026 00:05:00 +0000", Some(BASE + 300)),
            ("Thu, 08 Oct 2026 18:05:00 +1800", Some(BASE + 86_700)),
            ("Wed, 07 Oct 2026 19:00:00 +1900", None),
            ("Wed, 07 Oct 2026 00:05:00 +0060", None),
            ("Wed, 07 Oct 2026 09:05:00 +09:00", None),
            ("Wed, 07 Oct 2026 00:06:00 UT", None),
            ("Wed, 07 Oct 2026 00:06:00 Z", None),
            ("Wed, 07 Oct 2026 00:06:00 EST", None),
            ("Wed, 07 Oct 2026 00:05:00GMT", None),
            // 24시
            ("Wed, 07 Oct 2026 24:00:00 GMT", Some(BASE + 86_400)),
            ("Wed, 07 Oct 2026 24:00 GMT", Some(BASE + 86_400)),
            ("Wed, 07 Oct 2026 24:00:01 GMT", None),
            ("Wed, 07 Oct 2026 24:01 GMT", None),
            ("Wed, 07 Oct 2026 00:60:00 GMT", None),
            ("Wed, 07 Oct 2026 00:07:60 GMT", None),
            // 그 달 길이를 넘는 일은 마지막 날로 맞춘 뒤 요일을 확인한다.
            ("31 Nov 2026 00:08:00 GMT", Some(BASE + 4_666_080)),
            ("Mon, 31 Nov 2026 00:08:00 GMT", Some(BASE + 4_666_080)),
            ("Tue, 31 Nov 2026 00:08:00 GMT", None),
            ("29 Feb 2027 00:00:00 GMT", Some(BASE + 12_441_600)),
            ("Mon, 30 Feb 2026 00:08:00 GMT", None),
            ("32 Nov 2026 00:00:00 GMT", None),
            ("00 Nov 2026 00:00:00 GMT", None),
            // 공백·글자
            ("Wed, 07 Oct 2026  00:05:00 GMT", None),
            ("Wed, 07 Oct 2026 00:05:00  GMT", None),
            ("Wed, 07 Oct 2026 00:05:00 GMT x", None),
            ("Wed, 07 October 2026 00:07:00 GMT", None),
            ("Wed, 07 Oct 2026 00:06:00.5 GMT", None),
        ] {
            assert_eq!(at(text), expected, "{text:?}");
        }
    }
}
