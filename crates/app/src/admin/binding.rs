//! 관리자 API의 요청 바인딩 보조. Spring이 `@ModelAttribute` 쿼리 객체와 Jackson 본문 DTO에 대해 하던 변환 규칙이다.
//! 규칙은 모두 java-a에 직접 보내 확인한 결과다(시나리오 `admin/*`, `harness/golden/admin__*.json`).
//!
//! 쿼리 파라미터(`@ModelAttribute` 레코드):
//! - 같은 이름이 여러 번 오면 첫 값. 공백뿐인 값은 "없음"(`null`)이다.
//! - 열거형(`StringToEnumConverterFactory`)은 앞뒤 공백을 떼고 이름이 정확히 같아야 한다.
//!   `Converter<String, XxxSort>`는 값을 그대로 쓴다(앞뒤 공백이 있으면 오류).
//! - 정수는 `NumberUtils.parseNumber`: 모든 공백을 지우고, `0x`·`#` 16진수는 `Integer.decode`, 그 밖에는 `Integer.valueOf`.
//! - UUID는 앞뒤 공백을 떼고 `UUID.fromString`(느슨한 형식 허용).
//! - 날짜는 ISO_DATE(오프셋을 받고 버린다), 시각은 `Instant`(아래 [`parse_query_instant`]).
//! - 변환에 실패하면 `이름: 요청 값의 형식이 올바르지 않습니다.`이며 검증보다 먼저다. 여러 개가 틀리면 선언 순서의 첫 번째다.
//!
//! JSON 본문(Jackson):
//! - 열거형은 이름(앞뒤 공백 허용)·서수(정수 또는 숫자 문자열). 빈 문자열·소문자·실수·불리언은 거절한다.
//! - 시각은 ISO 문자열(초 필수, 오프셋 허용, 앞뒤 공백 허용) 또는 에포크 초(정수·소수·숫자 문자열). 빈 문자열은 `null`.
//! - 날짜는 `yyyy-MM-dd`, `T`가 붙은 날짜시각(`Z`로 끝나면 앞 10자), 에포크 일수(정수), `[연,월,일]` 배열.
//! - 빈 값·누락은 Bean Validation으로 넘어가 `must not be null` 같은 기본 메시지가 된다.

use chalkak_http::Request;
use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::{Offset, TimeZone};
use serde_json::Value;
use uuid::Uuid;

use crate::common::java_uuid_from_string;
use crate::web::ApiError;
use crate::web::binding::{JsonObject, LocalDate, is_java_whitespace, parse_iso_date};

/// `StringUtils.hasText`
pub fn has_text(text: &str) -> bool {
    !text.chars().all(is_java_whitespace)
}

/// `String.trim()`
pub fn java_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

/// 자바 `String.length()`(UTF-16 코드 유닛 수). Bean Validation `@Size`가 센다.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 코드 포인트 수(`codePointCount`).
pub fn code_points(text: &str) -> usize {
    text.chars().count()
}

// ---------------------------------------------------------------------------------------------
// 쿼리 파라미터
// ---------------------------------------------------------------------------------------------

pub struct QueryParams<'a> {
    req: &'a Request,
}

fn mismatch(name: &str) -> ApiError {
    ApiError::ArgumentTypeMismatch(name.to_owned())
}

impl<'a> QueryParams<'a> {
    pub fn new(req: &'a Request) -> Self {
        QueryParams { req }
    }

    /// 첫 값. 공백뿐이면 `None`.
    fn text(&self, name: &str) -> Option<&'a str> {
        self.req.query_param(name).filter(|v| has_text(v))
    }

    /// `String` 파라미터: 값을 그대로 쓴다(빈 문자열도 그대로).
    pub fn string(&self, name: &str) -> Option<String> {
        self.req.query_param(name).map(str::to_owned)
    }

    /// 열거형 파라미터.
    pub fn enumeration<T>(
        &self,
        name: &str,
        parse: impl Fn(&str) -> Option<T>,
    ) -> Result<Option<T>, ApiError> {
        match self.text(name) {
            None => Ok(None),
            Some(raw) => parse(java_trim(raw))
                .map(Some)
                .ok_or_else(|| mismatch(name)),
        }
    }

    /// 정렬 파라미터(`Converter<String, XxxSort>`): 값을 그대로 비교한다.
    pub fn sort<T>(
        &self,
        name: &str,
        parse: impl Fn(&str) -> Option<T>,
    ) -> Result<Option<T>, ApiError> {
        match self.text(name) {
            None => Ok(None),
            Some(raw) => parse(raw).map(Some).ok_or_else(|| mismatch(name)),
        }
    }

    pub fn integer(&self, name: &str) -> Result<Option<i32>, ApiError> {
        match self.text(name) {
            None => Ok(None),
            Some(raw) => parse_spring_int(raw)
                .map(Some)
                .ok_or_else(|| mismatch(name)),
        }
    }

    pub fn uuid(&self, name: &str) -> Result<Option<Uuid>, ApiError> {
        match self.text(name) {
            None => Ok(None),
            Some(raw) => java_uuid_from_string(java_trim(raw))
                .map(Some)
                .ok_or_else(|| mismatch(name)),
        }
    }

    pub fn date(&self, name: &str) -> Result<Option<Date>, ApiError> {
        match self.text(name) {
            None => Ok(None),
            Some(raw) => match parse_iso_date(raw) {
                Some(LocalDate::Valid(date)) => Ok(Some(date)),
                // jiff는 ±9999년까지만 다룬다. 범위 밖은 어떤 주제 날짜보다 뒤/앞인 끝값으로 근사한다.
                Some(LocalDate::AfterMax) => Ok(Some(Date::MAX)),
                Some(LocalDate::BeforeMin) => Ok(Some(Date::MIN)),
                None => Err(mismatch(name)),
            },
        }
    }

    pub fn instant(&self, name: &str) -> Result<Option<Timestamp>, ApiError> {
        match self.text(name) {
            None => Ok(None),
            Some(raw) => parse_query_instant(raw)
                .map(Some)
                .ok_or_else(|| mismatch(name)),
        }
    }
}

/// `NumberUtils.parseNumber(text, Integer.class)`
pub fn parse_spring_int(text: &str) -> Option<i32> {
    let compact: String = text.chars().filter(|c| !is_java_whitespace(*c)).collect();
    let (negative, unsigned) = match compact.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, compact.strip_prefix('+').unwrap_or(&compact)),
    };
    let hex = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
        .or_else(|| unsigned.strip_prefix('#'));
    // `isHexNumber`는 '-'만 보고 '+'는 보지 않는다: "+0x1"은 16진수가 아니라 10진수 변환으로 가서 실패한다.
    if let Some(digits) = hex.filter(|_| !compact.starts_with('+')) {
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = i64::from_str_radix(digits, 16).ok()?;
        let value = if negative { -value } else { value };
        return i32::try_from(value).ok();
    }
    if unsigned.is_empty() || !unsigned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: i64 = unsigned.parse().ok()?;
    i32::try_from(if negative { -value } else { value }).ok()
}

/// 쿼리 `Instant`(`@DateTimeFormat(iso = DATE_TIME)`): `yyyy-MM-dd'T'HH:mm[:ss[.f{1,9}]]` + `Z`/`±HH:mm[:ss]`.
/// 대소문자를 가리지 않는다. 오프셋이 없으면 거절한다. `24:00:00`은 다음 날 0시, `23:59:60`은 `23:59:59`로 읽는다.
pub fn parse_query_instant(text: &str) -> Option<Timestamp> {
    parse_iso_instant(text, false)
}

fn digits(bytes: &[u8], at: usize, count: usize) -> Option<i64> {
    let slice = bytes.get(at..at + count)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    slice
        .iter()
        .try_fold(0i64, |acc, d| Some(acc * 10 + i64::from(d - b'0')))
}

/// `seconds_required`: Jackson 본문은 초를 생략할 수 없다.
fn parse_iso_instant(text: &str, seconds_required: bool) -> Option<Timestamp> {
    let b = text.as_bytes();
    let year = digits(b, 0, 4)?;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    if !matches!(b.get(10), Some(b'T' | b't')) {
        return None;
    }
    let hour = digits(b, 11, 2)?;
    if b.get(13) != Some(&b':') {
        return None;
    }
    let minute = digits(b, 14, 2)?;
    let mut at = 16;
    let mut second = 0;
    let mut nano = 0i64;
    if b.get(at) == Some(&b':') {
        second = digits(b, at + 1, 2)?;
        at += 3;
        if b.get(at) == Some(&b'.') {
            let start = at + 1;
            let mut end = start;
            while b.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            let count = end - start;
            if !(1..=9).contains(&count) {
                return None;
            }
            nano =
                text[start..end].parse::<i64>().ok()? * 10i64.pow(u32::try_from(9 - count).ok()?);
            at = end;
        }
    } else if seconds_required {
        return None;
    }
    let offset_seconds = parse_offset(&text[at..])?;

    if minute > 59 || month == 0 || month > 12 || day == 0 {
        return None;
    }
    let (mut days_to_add, mut hour) = (0i64, hour);
    match (hour, minute, second, nano) {
        (24, 0, 0, 0) => {
            hour = 0;
            days_to_add = 1;
        }
        (h, ..) if h > 23 => return None,
        _ => {}
    }
    if second == 60 {
        if hour == 23 && minute == 59 {
            second = 59;
        } else {
            return None;
        }
    }
    if second > 59 {
        return None;
    }
    let date = Date::new(
        i16::try_from(year).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
    )
    .ok()?;
    let datetime = DateTime::from_parts(
        date,
        jiff::civil::Time::new(
            i8::try_from(hour).ok()?,
            i8::try_from(minute).ok()?,
            i8::try_from(second).ok()?,
            i32::try_from(nano).ok()?,
        )
        .ok()?,
    );
    let offset = Offset::from_seconds(i32::try_from(offset_seconds).ok()?).ok()?;
    let zoned = datetime.to_zoned(TimeZone::fixed(offset)).ok()?;
    let timestamp = zoned.timestamp();
    if days_to_add == 0 {
        Some(timestamp)
    } else {
        timestamp
            .checked_add(jiff::SignedDuration::from_hours(24 * days_to_add))
            .ok()
    }
}

/// `Z` 또는 `±HH:mm[:ss]`(최대 ±18:00). 초 단위 오프셋.
fn parse_offset(text: &str) -> Option<i64> {
    if text == "Z" || text == "z" {
        return Some(0);
    }
    let b = text.as_bytes();
    let sign = match b.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours = digits(b, 1, 2)?;
    if b.get(3) != Some(&b':') {
        return None;
    }
    let minutes = digits(b, 4, 2)?;
    let seconds = match b.len() {
        6 => 0,
        9 if b[6] == b':' => digits(b, 7, 2)?,
        _ => return None,
    };
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let total = hours * 3600 + minutes * 60 + seconds;
    (total <= 18 * 3600).then_some(sign * total)
}

// ---------------------------------------------------------------------------------------------
// JSON 본문
// ---------------------------------------------------------------------------------------------

/// Jackson 열거형. `names`는 선언 순서(서수 순서)다. 없는 필드·`null`은 `None`.
pub fn body_enum<T: Copy>(
    body: &JsonObject,
    field: &str,
    names: &[(&str, T)],
) -> Result<Option<T>, ApiError> {
    match body.value(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = java_trim(text);
            if let Some((_, value)) = names.iter().find(|(name, _)| *name == trimmed) {
                return Ok(Some(*value));
            }
            // 숫자 문자열은 서수로 본다.
            if !trimmed.is_empty()
                && trimmed.bytes().all(|d| d.is_ascii_digit())
                && let Some((_, value)) = trimmed
                    .parse::<usize>()
                    .ok()
                    .and_then(|ordinal| names.get(ordinal))
            {
                return Ok(Some(*value));
            }
            Err(ApiError::UnreadableBody)
        }
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|ordinal| usize::try_from(ordinal).ok())
            .and_then(|ordinal| names.get(ordinal))
            .map(|(_, value)| Some(*value))
            .ok_or(ApiError::UnreadableBody),
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

/// Jackson `Instant` 필드.
pub fn body_instant(body: &JsonObject, field: &str) -> Result<Option<Timestamp>, ApiError> {
    match body.value(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = java_trim(text);
            if trimmed.is_empty() {
                return Ok(None);
            }
            if let Some(seconds) = parse_epoch_string(trimmed) {
                return epoch_seconds(seconds).map(Some);
            }
            parse_iso_instant(trimmed, true)
                .map(Some)
                .ok_or(ApiError::UnreadableBody)
        }
        Some(Value::Number(n)) => {
            let seconds = if let Some(i) = n.as_i64() {
                EpochSeconds::Whole(i)
            } else if let Some(f) = n.as_f64() {
                EpochSeconds::Fractional(f)
            } else {
                return Err(ApiError::UnreadableBody);
            };
            epoch_seconds(seconds).map(Some)
        }
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

enum EpochSeconds {
    Whole(i64),
    Fractional(f64),
}

/// 숫자 문자열(`-?\d+(\.\d+)?([eE][+-]?\d+)?`)을 에포크 초로 읽는다.
fn parse_epoch_string(text: &str) -> Option<EpochSeconds> {
    let body = text.strip_prefix('-').unwrap_or(text);
    let mut parts = body.splitn(2, ['e', 'E']);
    let mantissa = parts.next()?;
    let exponent = parts.next();
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|d| d.is_ascii_digit()) {
        return None;
    }
    if let Some(f) = frac_part
        && (f.is_empty() || !f.bytes().all(|d| d.is_ascii_digit()))
    {
        return None;
    }
    if let Some(e) = exponent {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        if digits.is_empty() || !digits.bytes().all(|d| d.is_ascii_digit()) {
            return None;
        }
    }
    if frac_part.is_none() && exponent.is_none() {
        return text.parse::<i64>().ok().map(EpochSeconds::Whole).or(Some(
            // 범위를 넘는 정수는 시각으로 만들 수 없다.
            EpochSeconds::Fractional(f64::INFINITY),
        ));
    }
    text.parse::<f64>().ok().map(EpochSeconds::Fractional)
}

fn epoch_seconds(seconds: EpochSeconds) -> Result<Timestamp, ApiError> {
    let invalid = || ApiError::UnreadableBody;
    match seconds {
        EpochSeconds::Whole(s) => Timestamp::from_second(s).map_err(|_| invalid()),
        EpochSeconds::Fractional(f) => {
            if !f.is_finite() {
                return Err(invalid());
            }
            // DB는 마이크로초까지만 저장하므로 마이크로초로 맞춘다.
            let micros = (f * 1_000_000.0).round();
            if micros.abs() > 9.0e18 {
                return Err(invalid());
            }
            #[allow(clippy::cast_possible_truncation)]
            Timestamp::from_microsecond(micros as i64).map_err(|_| invalid())
        }
    }
}

/// Jackson `LocalDate` 해석 결과. jiff가 못 다루는 범위(±9999년 밖)는 값 없이 표시한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonDate {
    Valid(Date),
    OutOfRange,
}

impl JsonDate {
    /// 범위 밖은 어떤 주제 날짜와도 같지 않은 끝값으로 근사한다.
    pub fn date(self) -> Date {
        match self {
            JsonDate::Valid(date) => date,
            JsonDate::OutOfRange => Date::MAX,
        }
    }
}

pub fn body_local_date(body: &JsonObject, field: &str) -> Result<Option<JsonDate>, ApiError> {
    match body.value(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = java_trim(text);
            if trimmed.is_empty() {
                return Ok(None);
            }
            parse_json_local_date(trimmed)
                .map(Some)
                .ok_or(ApiError::UnreadableBody)
        }
        Some(Value::Number(n)) => {
            let days = n.as_i64().ok_or(ApiError::UnreadableBody)?;
            Ok(Some(epoch_day(days)))
        }
        Some(Value::Array(items)) => {
            let nums: Option<Vec<i64>> = items.iter().map(Value::as_i64).collect();
            match nums.as_deref() {
                Some([y, m, d]) => {
                    let year = i16::try_from(*y).ok();
                    let month = i8::try_from(*m).ok();
                    let day = i8::try_from(*d).ok();
                    match (year, month, day) {
                        (Some(y), Some(m), Some(d)) => Date::new(y, m, d)
                            .map(|d| Some(JsonDate::Valid(d)))
                            .map_err(|_| ApiError::UnreadableBody),
                        _ => Err(ApiError::UnreadableBody),
                    }
                }
                _ => Err(ApiError::UnreadableBody),
            }
        }
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

fn epoch_day(days: i64) -> JsonDate {
    match i32::try_from(days).ok().and_then(|d| {
        Date::new(1970, 1, 1)
            .ok()?
            .checked_add(jiff::Span::new().try_days(d).ok()?)
            .ok()
    }) {
        Some(date) => JsonDate::Valid(date),
        None => JsonDate::OutOfRange,
    }
}

fn parse_json_local_date(text: &str) -> Option<JsonDate> {
    if text.len() > 10 && text.as_bytes()[10] == b'T' {
        if text.ends_with('Z') {
            return parse_strict_local_date(text.get(..10)?);
        }
        // ISO_LOCAL_DATE_TIME: 날짜 + 'T' + HH:mm[:ss[.f{1,9}]]
        let (date_part, time_part) = text.split_once('T')?;
        if !is_local_time(time_part) {
            return None;
        }
        return parse_strict_local_date(date_part);
    }
    parse_strict_local_date(text)
}

fn is_local_time(text: &str) -> bool {
    let b = text.as_bytes();
    let (Some(hour), Some(minute)) = (digits(b, 0, 2), digits(b, 3, 2)) else {
        return false;
    };
    if b.get(2) != Some(&b':') || hour > 23 || minute > 59 {
        return false;
    }
    match b.len() {
        5 => true,
        8 | 10..=18 => {
            let Some(second) = digits(b, 6, 2) else {
                return false;
            };
            b[5] == b':'
                && second <= 59
                && (b.len() == 8 || (b[8] == b'.' && b[9..].iter().all(u8::is_ascii_digit)))
        }
        _ => false,
    }
}

/// `ISO_LOCAL_DATE`: 오프셋 없는 `yyyy-MM-dd`.
fn parse_strict_local_date(text: &str) -> Option<JsonDate> {
    let parsed = parse_iso_date(text)?;
    // `parse_iso_date`는 뒤의 오프셋을 받아 주므로, 날짜 부분의 길이와 같은지 확인해 거른다.
    let bytes = text.as_bytes();
    let mut i = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if text.len() != i + 6 {
        return None;
    }
    Some(match parsed {
        LocalDate::Valid(date) => JsonDate::Valid(date),
        LocalDate::AfterMax | LocalDate::BeforeMin => JsonDate::OutOfRange,
    })
}

// ---------------------------------------------------------------------------------------------
// Bean Validation 기본 메시지
// ---------------------------------------------------------------------------------------------

pub const MUST_NOT_BE_NULL: &str = "must not be null";
pub const MUST_NOT_BE_BLANK: &str = "must not be blank";

pub fn validation(message: &str) -> ApiError {
    ApiError::Validation(message.to_owned())
}

pub fn size_message(max: usize) -> String {
    format!("size must be between 0 and {max}")
}

/// `@Min`/`@Max` 쿼리 검증: `page`·`pageSize`의 기본값을 채우고 범위를 검사한다(메시지 `조회 조건이 올바르지 않습니다.`).
pub fn page_params(page: Option<i32>, page_size: Option<i32>) -> Result<(i32, i32), ApiError> {
    let page = page.unwrap_or(1);
    let page_size = page_size.unwrap_or(20);
    if page < 1 || !(1..=100).contains(&page_size) {
        return Err(validation("조회 조건이 올바르지 않습니다."));
    }
    Ok((page, page_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_http::Method;
    use serde_json::json;

    fn req(query: &[(&str, &str)]) -> Request {
        let mut r = Request::new(Method::Get, "/");
        r.query = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        r
    }

    fn body(value: Value) -> JsonObject {
        let mut r = Request::new(Method::Post, "/");
        r.headers.append("Content-Type", "application/json");
        r.body = value.to_string().into_bytes();
        crate::web::binding::read_json_object(&r).unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    // java-a: 쿼리 정수 (users page ...)
    #[test]
    fn 정수는_스프링_규칙으로_읽는다() {
        for (text, expected) in [
            ("1", Some(1)),
            ("+1", Some(1)),
            (" 1", Some(1)),
            ("1 0", Some(10)),
            ("0010", Some(10)),
            ("010", Some(10)),
            ("0x1", Some(1)),
            ("0X1F", Some(31)),
            ("#1", Some(1)),
            ("-0x1", Some(-1)),
            ("-1", Some(-1)),
            ("2147483647", Some(i32::MAX)),
            ("0x7FFFFFFF", Some(i32::MAX)),
            ("2147483648", None),
            ("0xFFFFFFFF", None),
            ("abc", None),
            ("1.5", None),
            ("", None),
            ("0x", None),
            ("--1", None),
            ("+0x1", None),
        ] {
            assert_eq!(parse_spring_int(text), expected, "{text:?}");
        }
    }

    #[test]
    fn 쿼리_값이_공백뿐이면_없음이다() {
        let q = req(&[("page", " "), ("sort", "\t"), ("status", "")]);
        let p = QueryParams::new(&q);
        assert_eq!(p.integer("page").unwrap(), None);
        assert_eq!(p.sort("sort", |_| Some(1)).unwrap(), None);
        assert_eq!(p.enumeration("status", |_| Some(1)).unwrap(), None);
        assert_eq!(p.string("status").as_deref(), Some(""));
    }

    #[test]
    fn 열거형은_앞뒤_공백을_떼고_정렬은_그대로_비교한다() {
        let parse = |s: &str| (s == "ACTIVE").then_some(1);
        let q = req(&[("a", " ACTIVE "), ("b", " ACTIVE"), ("c", "active")]);
        let p = QueryParams::new(&q);
        assert_eq!(p.enumeration("a", parse).unwrap(), Some(1));
        assert_eq!(
            p.sort("b", parse).unwrap_err().message(),
            "b: 요청 값의 형식이 올바르지 않습니다."
        );
        assert_eq!(p.enumeration("c", parse).unwrap_err().status(), 400);
    }

    #[test]
    fn uuid_쿼리는_앞뒤_공백을_떼고_느슨한_형식을_받는다() {
        let q = req(&[
            ("a", "1-2-3-4-5"),
            ("b", " 01990000-0000-7000-8000-000000000001 "),
            ("c", "zzzzzzzz-0000-7000-8000-000000000001"),
            ("d", "01990000-0000-7000-8000-0000000000011"),
            ("e", "+1990000-0000-7000-8000-000000000001"),
        ]);
        let p = QueryParams::new(&q);
        assert!(p.uuid("a").unwrap().is_some());
        assert!(p.uuid("b").unwrap().is_some());
        assert!(p.uuid("c").is_err());
        assert!(p.uuid("d").is_err());
        assert!(p.uuid("e").unwrap().is_some());
    }

    #[test]
    fn 쿼리_시각은_오프셋이_필수이고_대소문자를_가리지_않는다() {
        let ok = |s: &str| parse_query_instant(s);
        assert_eq!(ok("2026-10-11T00:00:00Z"), Some(ts("2026-10-11T00:00:00Z")));
        assert_eq!(ok("2026-10-11t00:00:00z"), Some(ts("2026-10-11T00:00:00Z")));
        assert_eq!(ok("2026-10-11T00:00Z"), Some(ts("2026-10-11T00:00:00Z")));
        assert_eq!(
            ok("2026-10-11T09:00:00+09:00"),
            Some(ts("2026-10-11T00:00:00Z"))
        );
        assert_eq!(
            ok("2026-10-11T09:00:00+09:00:30"),
            Some(ts("2026-10-10T23:59:30Z"))
        );
        assert_eq!(
            ok("2026-10-12T00:00:00.5Z"),
            Some(ts("2026-10-12T00:00:00.5Z"))
        );
        assert_eq!(
            ok("2026-10-12T00:00:00.123456789Z"),
            Some(ts("2026-10-12T00:00:00.123456789Z"))
        );
        assert_eq!(ok("2026-10-11T24:00:00Z"), Some(ts("2026-10-12T00:00:00Z")));
        assert_eq!(ok("2026-06-30T23:59:60Z"), Some(ts("2026-06-30T23:59:59Z")));
        for bad in [
            "2026-10-11T00:00:00",
            "2026-10-11",
            "2026-10-11T00:00:00.1234567890Z",
            "2026-10-11T00:00:60Z",
            "2026-10-11T24:00:01Z",
            "2026-10-11T25:00:00Z",
            " 2026-10-11T00:00:00Z",
            "2026-10-11T00:00:00z ",
            "2026-13-11T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-11T00:00:00+19:00",
            "2026-10-11T00:00:00+0900",
            "2026-10-11T00:00:00.Z",
            "2026-10-11 00:00:00Z",
        ] {
            assert_eq!(ok(bad), None, "{bad}");
        }
    }

    #[test]
    fn 본문_열거형() {
        let names = [("A", 0), ("B", 1), ("C", 2)];
        let get = |v: Value| body_enum(&body(json!({ "x": v })), "x", &names);
        assert_eq!(get(json!("B")).unwrap(), Some(1));
        assert_eq!(get(json!(" B ")).unwrap(), Some(1));
        assert_eq!(get(json!(2)).unwrap(), Some(2));
        assert_eq!(get(json!("2")).unwrap(), Some(2));
        assert_eq!(get(Value::Null).unwrap(), None);
        for bad in [
            json!("b"),
            json!(""),
            json!(3),
            json!(-1),
            json!(1.0),
            json!(true),
            json!(["A"]),
            json!("x"),
        ] {
            assert!(get(bad.clone()).is_err(), "{bad}");
        }
        assert_eq!(body_enum(&body(json!({})), "x", &names).unwrap(), None);
    }

    #[test]
    fn 본문_시각() {
        let get = |v: Value| body_instant(&body(json!({ "t": v })), "t");
        assert_eq!(get(json!(1)).unwrap(), Some(ts("1970-01-01T00:00:01Z")));
        assert_eq!(get(json!(1.5)).unwrap(), Some(ts("1970-01-01T00:00:01.5Z")));
        assert_eq!(get(json!("1")).unwrap(), Some(ts("1970-01-01T00:00:01Z")));
        assert_eq!(
            get(json!("1.5")).unwrap(),
            Some(ts("1970-01-01T00:00:01.5Z"))
        );
        assert_eq!(get(json!("-1")).unwrap(), Some(ts("1969-12-31T23:59:59Z")));
        assert_eq!(get(json!(1e3)).unwrap(), Some(ts("1970-01-01T00:16:40Z")));
        assert_eq!(
            get(json!(" 1970-01-01T00:00:00Z ")).unwrap(),
            Some(ts("1970-01-01T00:00:00Z"))
        );
        assert_eq!(
            get(json!("1970-01-01t00:00:00z")).unwrap(),
            Some(ts("1970-01-01T00:00:00Z"))
        );
        assert_eq!(
            get(json!("2026-10-14T00:00:00+09:00")).unwrap(),
            Some(ts("2026-10-13T15:00:00Z"))
        );
        assert_eq!(get(json!("")).unwrap(), None);
        assert_eq!(get(Value::Null).unwrap(), None);
        for bad in [
            json!("1970-01-01T00:00:00"),
            json!("1970-01-01T00:00Z"),
            json!("1970-01-01"),
            json!(true),
            json!([1]),
            json!({"a": 1}),
            json!(1e30),
            json!(99999999999999999999.0),
            json!("x"),
        ] {
            assert!(get(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn 본문_날짜() {
        let get = |v: Value| body_local_date(&body(json!({ "d": v })), "d");
        let valid = |y, m, d| Some(JsonDate::Valid(jiff::civil::date(y, m, d)));
        assert_eq!(get(json!("2026-10-15")).unwrap(), valid(2026, 10, 15));
        assert_eq!(get(json!(" 2026-10-15")).unwrap(), valid(2026, 10, 15));
        assert_eq!(
            get(json!("2026-10-15T00:00:00")).unwrap(),
            valid(2026, 10, 15)
        );
        assert_eq!(
            get(json!("2026-10-15T00:00:00Z")).unwrap(),
            valid(2026, 10, 15)
        );
        assert_eq!(get(json!([2026, 10, 15])).unwrap(), valid(2026, 10, 15));
        assert_eq!(get(json!(0)).unwrap(), valid(1970, 1, 1));
        assert_eq!(get(json!(20261015)).unwrap(), Some(JsonDate::OutOfRange));
        assert_eq!(
            get(json!("+12026-10-15")).unwrap(),
            Some(JsonDate::OutOfRange)
        );
        assert_eq!(get(json!("")).unwrap(), None);
        for bad in [
            json!("20261015"),
            json!("2026-10-15+09:00"),
            json!("2026-1-15"),
            json!("2026-02-30"),
            json!("2026-10-15T25:00:00"),
            json!([2026, 10]),
            json!(true),
            json!(1.5),
        ] {
            assert!(get(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn 페이지_쿼리_검증() {
        assert_eq!(page_params(None, None).unwrap(), (1, 20));
        assert_eq!(page_params(Some(3), Some(100)).unwrap(), (3, 100));
        for (p, s) in [
            (Some(0), None),
            (None, Some(0)),
            (None, Some(101)),
            (Some(-1), None),
        ] {
            let e = page_params(p, s).unwrap_err();
            assert_eq!(e.message(), "조회 조건이 올바르지 않습니다.");
        }
    }

    #[test]
    fn 크기는_utf16_단위로_센다() {
        assert_eq!(utf16_len("📸"), 2);
        assert_eq!(code_points("📸"), 1);
        assert_eq!(size_message(500), "size must be between 0 and 500");
    }
}
