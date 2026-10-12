//! Spring `CronExpression` 6필드(초 분 시 일 월 요일)의 파싱과 다음 실행 시각 계산.
//!
//! 원본이 쓰는 식(`@Scheduled(cron = "...", zone = "Asia/Seoul")`):
//!
//! | 식 | 작업 |
//! | --- | --- |
//! | `0 30 4 * * *` | `RefreshTokenCleanupScheduler` |
//! | `0 40 4 * * *` | `ConsumedSignupTokenCleanupScheduler` |
//! | `0 50 4 * * *` | `NotificationCleanupScheduler` |
//! | `0 0 * * * *`  | `PendingAppleAuthorizationCleanupScheduler` |
//!
//! Spring Boot 4.1.0(Spring Framework 7.0.8)의 `CronExpression`을 직접 실행해 얻은 값으로 검증한다
//! (`tests/spring_cron_parity.rs`). 같은 규칙:
//!
//! - 필드: `*`, `?`(일·요일만), 값, 범위 `a-b`, 목록 `a,b`, 간격 `*/n`·`a/n`·`a-b/n`, 월·요일 영문 이름(대소문자 무시).
//! - 요일은 `0`과 `7`이 일요일, `1`이 월요일. 범위 시작이 `7`이면 `0`으로 본다(`7-1`은 일·월).
//! - 일·요일을 모두 제한하면 둘 다 맞아야 한다(유닉스 cron의 OR가 아니라 AND).
//! - 일: `L`(말일), `L-n`, `nW`(가장 가까운 평일), `LW`. 요일: `nL`(그 달 마지막 해당 요일), `n#m`(m번째).
//! - 매크로: `@yearly`·`@annually`, `@monthly`, `@weekly`, `@daily`·`@midnight`, `@hourly`(대소문자 무시).
//! - 다음 시각은 기준 시각 **다음 초부터** 찾는다(기준과 같은 초는 포함하지 않는다). 하위 초는 버린다.
//! - 필드 사이 공백은 여러 개여도 되고 앞뒤 공백은 무시한다.
//!
//! 알려진 차이: 앞으로 30년 안에 맞는 날이 없으면 `None`이다(Spring은 내부 시도 횟수에서 멈춘다).
//! 일·요일 `L-n`의 n은 1~30, `nW`에서 n이 그 달 일수보다 크면 그 달은 건너뛴다.

use std::fmt;

use jiff::civil::{Date, DateTime, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

/// 원본 `@Scheduled(zone = "Asia/Seoul")`.
pub const SEOUL: &str = "Asia/Seoul";

/// 앞으로 이 햇수 안에서만 다음 실행을 찾는다.
const SEARCH_YEARS: i16 = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CronError {
    Empty,
    /// 6필드가 아니다.
    FieldCount {
        found: usize,
    },
    /// 필드 하나가 올바르지 않다.
    Invalid {
        field: &'static str,
        token: String,
        reason: &'static str,
    },
    /// 알 수 없는 시간대 이름.
    UnknownZone(String),
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("cron 식이 비어 있습니다"),
            Self::FieldCount { found } => {
                write!(f, "cron 식은 6개 필드여야 합니다(현재 {found}개)")
            }
            Self::Invalid {
                field,
                token,
                reason,
            } => {
                write!(f, "cron {field} 필드 {token:?}: {reason}")
            }
            Self::UnknownZone(zone) => write!(f, "알 수 없는 시간대 {zone:?}"),
        }
    }
}

impl std::error::Error for CronError {}

/// 일 필드. `bits`는 1..=31 비트, 나머지는 `L`·`W` 계열.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DayOfMonth {
    bits: u32,
    /// `L`(0), `L-n`(n)
    last_offsets: Vec<u8>,
    /// `nW`
    near_weekday: Vec<u8>,
    /// `LW`
    last_weekday: bool,
}

/// 요일 필드. `bits`는 0..=7 비트(0과 7 모두 일요일).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DayOfWeek {
    bits: u8,
    /// `nL`: 요일(0=일..6=토)
    last: Vec<u8>,
    /// `n#m`: (요일 0=일..6=토, 순번)
    nth: Vec<(u8, u8)>,
}

/// 파싱된 cron 식.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpression {
    seconds: u64,
    minutes: u64,
    hours: u32,
    days: DayOfMonth,
    months: u16,
    weekdays: DayOfWeek,
}

type Names = &'static [&'static str];

const MONTH_NAMES: Names = &[
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const DAY_NAMES: Names = &["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

struct FieldSpec {
    name: &'static str,
    min: u32,
    max: u32,
    /// 이름 1이 가리키는 값(월 JAN=1, 요일 MON=1)
    names: Names,
    allow_question: bool,
}

const SECOND: FieldSpec = FieldSpec {
    name: "초",
    min: 0,
    max: 59,
    names: &[],
    allow_question: false,
};
const MINUTE: FieldSpec = FieldSpec {
    name: "분",
    min: 0,
    max: 59,
    names: &[],
    allow_question: false,
};
const HOUR: FieldSpec = FieldSpec {
    name: "시",
    min: 0,
    max: 23,
    names: &[],
    allow_question: false,
};
const DAY: FieldSpec = FieldSpec {
    name: "일",
    min: 1,
    max: 31,
    names: &[],
    allow_question: true,
};
const MONTH: FieldSpec = FieldSpec {
    name: "월",
    min: 1,
    max: 12,
    names: MONTH_NAMES,
    allow_question: false,
};
const WEEKDAY: FieldSpec = FieldSpec {
    name: "요일",
    min: 0,
    max: 7,
    names: DAY_NAMES,
    allow_question: true,
};

impl FieldSpec {
    fn err(&self, token: &str, reason: &'static str) -> CronError {
        CronError::Invalid {
            field: self.name,
            token: token.to_owned(),
            reason,
        }
    }

    /// 값 하나: 숫자(`+` 허용) 또는 3글자 이름.
    fn value(&self, token: &str, whole: &str) -> Result<u32, CronError> {
        let v = if !self.names.is_empty()
            && token.len() == 3
            && token.bytes().all(|b| b.is_ascii_alphabetic())
        {
            let upper = token.to_ascii_uppercase();
            let idx = self
                .names
                .iter()
                .position(|n| *n == upper)
                .ok_or_else(|| self.err(whole, "알 수 없는 이름"))?;
            u32::try_from(idx + 1).unwrap_or(0)
        } else {
            token
                .parse::<u32>()
                .map_err(|_| self.err(whole, "숫자가 아닙니다"))?
        };
        if v < self.min || v > self.max {
            return Err(self.err(whole, "허용 범위를 벗어났습니다"));
        }
        Ok(v)
    }

    /// `*` 범위의 끝. 요일은 1..=7(0은 따로 허용).
    fn full_range(&self) -> (u32, u32) {
        if self.name == WEEKDAY.name {
            (1, 7)
        } else {
            (self.min, self.max)
        }
    }

    /// `*`, `a`, `a-b`, `a/n`, `*/n`, `a-b/n` 한 항목을 (시작, 끝, 간격)으로 푼다.
    fn range(&self, item: &str) -> Result<(u32, u32, u32), CronError> {
        let (range_text, step) = match item.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| self.err(item, "간격이 숫자가 아닙니다"))?;
                if step == 0 {
                    return Err(self.err(item, "간격은 1 이상이어야 합니다"));
                }
                (r, Some(step))
            }
            None => (item, None),
        };
        let (full_min, full_max) = self.full_range();
        let (mut lo, hi) = if range_text == "*" {
            (full_min, full_max)
        } else if let Some((a, b)) = range_text.split_once('-') {
            (self.value(a, item)?, self.value(b, item)?)
        } else {
            let v = self.value(range_text, item)?;
            // "a/n"은 a부터 끝까지, "a"만 있으면 한 값
            (v, if step.is_some() { full_max } else { v })
        };
        if self.name == WEEKDAY.name && range_text.contains('-') && lo == 7 {
            lo = 0; // "7-1"
        }
        if lo > hi {
            return Err(self.err(item, "시작이 끝보다 클 수 없습니다"));
        }
        Ok((lo, hi, step.unwrap_or(1)))
    }

    fn bits(&self, field: &str) -> Result<u64, CronError> {
        let field = if self.allow_question && field == "?" {
            "*"
        } else {
            field
        };
        let mut bits = 0u64;
        for item in field.split(',') {
            let (lo, hi, step) = self.range(item)?;
            let mut v = lo;
            while v <= hi {
                bits |= 1 << v;
                v += step;
            }
        }
        Ok(bits)
    }
}

fn parse_day_of_month(field: &str) -> Result<DayOfMonth, CronError> {
    let spec = &DAY;
    let field = if field == "?" { "*" } else { field };
    let mut days = DayOfMonth::default();
    for item in field.split(',') {
        let upper = item.to_ascii_uppercase();
        if upper == "L" {
            days.last_offsets.push(0);
        } else if upper == "LW" {
            days.last_weekday = true;
        } else if let Some(offset) = upper.strip_prefix("L-") {
            let n: u8 = offset
                .parse()
                .map_err(|_| spec.err(item, "L- 뒤는 숫자여야 합니다"))?;
            if n == 0 || n > 30 {
                return Err(spec.err(item, "L-n의 n은 1~30이어야 합니다"));
            }
            days.last_offsets.push(n);
        } else if let Some(n) = upper.strip_suffix('W') {
            if n.is_empty() {
                return Err(spec.err(item, "W 앞에 일이 필요합니다"));
            }
            let day = spec.value(n, item)?;
            days.near_weekday.push(u8::try_from(day).unwrap_or(0));
        } else {
            let (lo, hi, step) = spec.range(item)?;
            let mut v = lo;
            while v <= hi {
                days.bits |= 1 << v;
                v += step;
            }
        }
    }
    Ok(days)
}

fn parse_day_of_week(field: &str) -> Result<DayOfWeek, CronError> {
    let spec = &WEEKDAY;
    let field = if field == "?" { "*" } else { field };
    let mut weekdays = DayOfWeek::default();
    for item in field.split(',') {
        let upper = item.to_ascii_uppercase();
        if let Some(prefix) = upper.strip_suffix('L') {
            if prefix.is_empty() {
                return Err(spec.err(item, "L 앞에 요일이 필요합니다"));
            }
            weekdays.last.push(sunday_zero(spec.value(prefix, item)?));
        } else if let Some((day, nth)) = upper.split_once('#') {
            let day = sunday_zero(spec.value(day, item)?);
            let nth: u8 = nth
                .parse()
                .map_err(|_| spec.err(item, "# 뒤는 숫자여야 합니다"))?;
            if nth == 0 {
                return Err(spec.err(item, "# 뒤는 1 이상이어야 합니다"));
            }
            weekdays.nth.push((day, nth));
        } else {
            let (lo, hi, step) = spec.range(item)?;
            let mut v = lo;
            while v <= hi {
                weekdays.bits |= 1 << v;
                v += step;
            }
        }
    }
    Ok(weekdays)
}

/// 0..=7(0·7=일) -> 0..=6(0=일)
fn sunday_zero(v: u32) -> u8 {
    u8::try_from(v % 7).unwrap_or(0)
}

impl CronExpression {
    /// 6필드 식 또는 `@daily` 같은 매크로를 해석한다.
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        let expanded = expand_macro(expression.trim());
        let text = expanded.as_deref().unwrap_or_else(|| expression.trim());
        if text.is_empty() {
            return Err(CronError::Empty);
        }
        // StringUtils.tokenizeToStringArray(expression, " "): 공백 문자 ' '만 구분자
        let fields: Vec<&str> = text.split(' ').filter(|s| !s.is_empty()).collect();
        if fields.len() != 6 {
            return Err(CronError::FieldCount {
                found: fields.len(),
            });
        }
        Ok(Self {
            seconds: SECOND.bits(fields[0])?,
            minutes: MINUTE.bits(fields[1])?,
            hours: u32::try_from(HOUR.bits(fields[2])?).unwrap_or(0),
            days: parse_day_of_month(fields[3])?,
            months: u16::try_from(MONTH.bits(fields[4])?).unwrap_or(0),
            weekdays: parse_day_of_week(fields[5])?,
        })
    }

    /// `after` **다음 초부터** 처음 맞는 시각. 30년 안에 없으면 `None`.
    pub fn next_after(&self, after: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
        let mut civil = tz
            .to_datetime(after)
            .with()
            .subsec_nanosecond(0)
            .build()
            .ok()?
            .checked_add(1.second())
            .ok()?;
        // 시간대 전환(겹치는 시각 등)으로 해석한 시각이 기준보다 앞서면 한 초씩 넘겨 다시 찾는다.
        for _ in 0..4 {
            let candidate = self.next_civil(civil)?;
            let resolved = tz.to_ambiguous_zoned(candidate).compatible().ok()?;
            let ts = resolved.timestamp();
            if ts > after {
                return Some(ts);
            }
            civil = candidate.checked_add(1.second()).ok()?;
        }
        None
    }

    /// `start` 이상에서 처음 맞는 시간대 없는 시각.
    fn next_civil(&self, start: DateTime) -> Option<DateTime> {
        let end_year = start.year().saturating_add(SEARCH_YEARS);
        let mut date = start.date();
        let mut first = true;
        while date.year() <= end_year {
            if self.months & (1 << date.month()) == 0 {
                date = date.last_of_month().tomorrow().ok()?;
                first = false;
                continue;
            }
            if self.day_matches(date) {
                let from = if first {
                    start.time()
                } else {
                    jiff::civil::time(0, 0, 0, 0)
                };
                if let Some((h, m, s)) =
                    self.first_time_from(from.hour(), from.minute(), from.second())
                {
                    return Some(date.at(h, m, s, 0));
                }
            }
            date = date.tomorrow().ok()?;
            first = false;
        }
        None
    }

    fn first_time_from(&self, h0: i8, m0: i8, s0: i8) -> Option<(i8, i8, i8)> {
        for h in h0..24 {
            if self.hours & (1 << h) == 0 {
                continue;
            }
            let m_start = if h == h0 { m0 } else { 0 };
            for m in m_start..60 {
                if self.minutes & (1 << m) == 0 {
                    continue;
                }
                let s_start = if h == h0 && m == m0 { s0 } else { 0 };
                for s in s_start..60 {
                    if self.seconds & (1 << s) != 0 {
                        return Some((h, m, s));
                    }
                }
            }
        }
        None
    }

    fn day_matches(&self, date: Date) -> bool {
        self.dom_matches(date) && self.dow_matches(date)
    }

    fn dom_matches(&self, date: Date) -> bool {
        let d = &self.days;
        let day = date.day();
        let last = date.days_in_month();
        if d.bits & (1 << day) != 0 {
            return true;
        }
        if d.last_offsets
            .iter()
            .any(|n| i8::try_from(*n).is_ok_and(|n| last - n == day))
        {
            return true;
        }
        if d.near_weekday
            .iter()
            .any(|n| i8::try_from(*n).is_ok_and(|n| n <= last && nearest_weekday(date, n) == day))
        {
            return true;
        }
        d.last_weekday && last_weekday(date) == day
    }

    fn dow_matches(&self, date: Date) -> bool {
        let w = &self.weekdays;
        let dow = u8::try_from(date.weekday().to_sunday_zero_offset()).unwrap_or(0);
        let sunday = dow == 0;
        if w.bits & (1 << dow) != 0 || (sunday && w.bits & (1 << 7) != 0) {
            return true;
        }
        let day = date.day();
        if w.last
            .iter()
            .any(|d| *d == dow && day + 7 > date.days_in_month())
        {
            return true;
        }
        w.nth
            .iter()
            .any(|(d, nth)| *d == dow && i16::from((day - 1) / 7 + 1) == i16::from(*nth))
    }
}

/// `nW`: 그 달 안에서 n일에 가장 가까운 평일(토요일이면 앞 금요일, 단 1일이면 월요일. 일요일이면 뒤 월요일, 단 말일이면 금요일).
fn nearest_weekday(date: Date, n: i8) -> i8 {
    let last = date.days_in_month();
    let Ok(target) = Date::new(date.year(), date.month(), n) else {
        return 0;
    };
    match target.weekday() {
        Weekday::Saturday if n == 1 => 3,
        Weekday::Saturday => n - 1,
        Weekday::Sunday if n == last => n - 2,
        Weekday::Sunday => n + 1,
        _ => n,
    }
}

/// `LW`: 그 달 마지막 평일.
fn last_weekday(date: Date) -> i8 {
    let last_day = date.last_of_month();
    match last_day.weekday() {
        Weekday::Saturday => last_day.day() - 1,
        Weekday::Sunday => last_day.day() - 2,
        _ => last_day.day(),
    }
}

fn expand_macro(text: &str) -> Option<String> {
    if !text.starts_with('@') {
        return None;
    }
    let expanded = match text.to_ascii_lowercase().as_str() {
        "@yearly" | "@annually" => "0 0 0 1 1 *",
        "@monthly" => "0 0 0 1 * *",
        "@weekly" => "0 0 0 * * 0",
        "@daily" | "@midnight" => "0 0 0 * * *",
        "@hourly" => "0 0 * * * *",
        _ => return None,
    };
    Some(expanded.to_owned())
}

/// 식과 시간대를 묶은 실행 일정. `@Scheduled(cron = ..., zone = ...)`에 해당한다.
#[derive(Clone, Debug)]
pub struct CronSchedule {
    expression: CronExpression,
    tz: TimeZone,
}

impl CronSchedule {
    pub fn new(expression: CronExpression, tz: TimeZone) -> Self {
        Self { expression, tz }
    }

    /// `CronSchedule::parse("0 30 4 * * *", "Asia/Seoul")`
    pub fn parse(expression: &str, zone: &str) -> Result<Self, CronError> {
        let tz = TimeZone::get(zone).map_err(|_| CronError::UnknownZone(zone.to_owned()))?;
        Ok(Self::new(CronExpression::parse(expression)?, tz))
    }

    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        self.expression.next_after(after, &self.tz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn next(expr: &str, zone: &str, from: &str) -> Option<String> {
        CronSchedule::parse(expr, zone)
            .unwrap()
            .next_after(ts(from))
            .map(|t| t.to_string())
    }

    // RefreshTokenCleanupScheduler: @Scheduled(cron = "0 30 4 * * *", zone = "Asia/Seoul")
    #[test]
    fn 리프레시_토큰_정리는_매일_서울_0430() {
        // 2026-10-12 09:00 KST 이후 -> 다음은 10-13 04:30 KST = 10-12 19:30Z
        assert_eq!(
            next("0 30 4 * * *", SEOUL, "2026-10-12T00:00:00Z").as_deref(),
            Some("2026-10-12T19:30:00Z")
        );
    }

    #[test]
    fn 기준_시각과_같은_초는_건너뛴다() {
        // 04:30:00 KST 정각이면 다음은 이튿날
        assert_eq!(
            next("0 30 4 * * *", SEOUL, "2026-10-11T19:30:00Z").as_deref(),
            Some("2026-10-12T19:30:00Z")
        );
        // 1초 전이면 같은 시각
        assert_eq!(
            next("0 30 4 * * *", SEOUL, "2026-10-11T19:29:59Z").as_deref(),
            Some("2026-10-11T19:30:00Z")
        );
        // 하위 초는 버린다: 04:29:59.5 -> 04:30:00
        assert_eq!(
            next("0 30 4 * * *", SEOUL, "2026-10-11T19:29:59.5Z").as_deref(),
            Some("2026-10-11T19:30:00Z")
        );
    }

    // PendingAppleAuthorizationCleanupScheduler: "0 0 * * * *"
    #[test]
    fn 매시_정각() {
        assert_eq!(
            next("0 0 * * * *", SEOUL, "2026-10-12T00:00:00Z").as_deref(),
            Some("2026-10-12T01:00:00Z")
        );
        assert_eq!(
            next("0 0 * * * *", SEOUL, "2026-10-12T00:59:59Z").as_deref(),
            Some("2026-10-12T01:00:00Z")
        );
    }

    #[test]
    fn 일과_요일을_모두_제한하면_and다() {
        // 2027-02-01은 월요일. 10월 이후 첫 "1일 또는 15일이면서 월요일"
        assert_eq!(
            next("0 0 9 1,15 * 1", "UTC", "2026-10-12T00:00:00Z").as_deref(),
            Some("2027-02-01T09:00:00Z")
        );
    }

    #[test]
    fn 요일_0과_7은_모두_일요일() {
        let a = next("0 0 0 * * 0", "UTC", "2026-10-12T00:00:00Z");
        let b = next("0 0 0 * * 7", "UTC", "2026-10-12T00:00:00Z");
        assert_eq!(a, b);
        assert_eq!(a.as_deref(), Some("2026-10-18T00:00:00Z"));
    }

    #[test]
    fn 윤일은_4년_뒤에도_찾는다() {
        assert_eq!(
            next("0 0 0 29 2 *", "UTC", "2026-10-12T00:00:00Z").as_deref(),
            Some("2028-02-29T00:00:00Z")
        );
    }

    #[test]
    fn 맞는_날이_없으면_none() {
        assert_eq!(next("0 0 0 31 2 *", "UTC", "2026-10-12T00:00:00Z"), None);
        assert_eq!(next("0 0 12 31 4 *", "UTC", "2026-10-12T00:00:00Z"), None);
    }

    #[test]
    fn 매크로() {
        assert_eq!(
            next("@daily", "UTC", "2026-10-12T10:00:00Z").as_deref(),
            Some("2026-10-13T00:00:00Z")
        );
        assert_eq!(
            next("@HOURLY", "UTC", "2026-10-12T10:30:00Z").as_deref(),
            Some("2026-10-12T11:00:00Z")
        );
    }

    #[test]
    fn 파싱_오류() {
        assert_eq!(CronExpression::parse(""), Err(CronError::Empty));
        assert_eq!(
            CronExpression::parse("* * * * *"),
            Err(CronError::FieldCount { found: 5 })
        );
        assert!(CronExpression::parse("60 * * * * *").is_err());
        assert!(CronExpression::parse("0 0 0 32 * *").is_err());
        assert!(CronExpression::parse("0 0 0 * * FOO").is_err());
        assert!(matches!(
            CronSchedule::parse("0 0 0 * * *", "Mars/Olympus"),
            Err(CronError::UnknownZone(_))
        ));
    }

    #[test]
    fn 시간대가_결과를_바꾼다() {
        // 같은 식이라도 UTC 04:30과 KST 04:30은 9시간 차이
        assert_eq!(
            next("0 30 4 * * *", "UTC", "2026-10-12T00:00:00Z").as_deref(),
            Some("2026-10-12T04:30:00Z")
        );
    }
}
