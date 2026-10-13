//! 요청 바인딩: 쿼리 파라미터와 JSON 본문. Spring MVC의 `@RequestParam`, `@RequestBody`(+ Jackson)가 하던 일이다.
//!
//! 규칙은 원본 java-a에 직접 보내 확인한 것이다(`harness/golden`, 시나리오 `topic/get-by-date`,
//! `platform/request-body-errors`).
//!
//! 쿼리 파라미터:
//! - 이름은 대소문자를 구분한다. 같은 이름이 여러 번 오면 첫 값을 쓴다(`date=a&date=b`는 `a`).
//! - 값이 없거나 비었거나 공백뿐이면 "없음"(`필수 요청 값이 없습니다.`), 값이 있는데 해석되지 않으면 형식 오류.
//!
//! JSON 본문:
//! - `Content-Type`이 없고 본문이 비었으면 "본문 없음"(400), 없는데 본문이 있으면 415. JSON이 아닌 타입(`text/json` 포함)은
//!   비어 있어도 415. `*/*`·`application/*`처럼 와일드카드가 든 타입은 원본이 서버 오류(500)로 끝난다.
//! - 본문은 객체여야 한다. 비었거나 객체가 아닌 값(`[]`, `"x"`, `123`, `null`), 뒤에 딸린 토큰, 깨진 JSON은 모두 400.
//! - 알 수 없는 필드는 무시하고, 같은 필드가 반복되면 마지막 값을 쓴다. UTF-8 BOM은 허용한다.
//! - 문자열 필드에 숫자·불리언을 보내면 거절하지 않고 문자열로 바꾼다(Jackson 스칼라 강제 변환). 배열·객체는 400.

use chalkak_http::Request;
use serde_json::{Map, Value};

use super::ApiError;

// ---------------------------------------------------------------------------------------------
// 쿼리 파라미터
// ---------------------------------------------------------------------------------------------

/// Java `Character.isWhitespace` (Spring `StringUtils.hasText`가 쓴다). 줄바꿈 없는 공백(U+00A0 등)은 제외한다.
pub fn is_java_whitespace(c: char) -> bool {
    match c {
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | '\u{1c}'..='\u{1f}' => true,
        '\u{a0}' | '\u{2007}' | '\u{202f}' => false,
        c => c.is_whitespace() && c != '\u{85}',
    }
}

/// 필수 쿼리 파라미터의 첫 값. 없거나 공백뿐이면 `MissingRequestParameter`.
pub fn required_query<'a>(req: &'a Request, name: &str) -> Result<&'a str, ApiError> {
    match req.query_param(name) {
        Some(value) if !value.chars().all(is_java_whitespace) => Ok(value),
        _ => Err(ApiError::MissingRequestParameter(name.to_owned())),
    }
}

/// 달력 날짜. Java `LocalDate`는 ±999999999년까지 다루지만 jiff는 ±9999년이라, 범위 밖은 방향만 기억한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalDate {
    Valid(jiff::civil::Date),
    /// 9999년보다 뒤. 어떤 "오늘"보다도 미래다.
    AfterMax,
    /// -9999년보다 앞.
    BeforeMin,
}

/// `@DateTimeFormat(iso = DATE)`: `DateTimeFormatter.ISO_DATE`로 엄격하게 해석한다.
///
/// - 연도 4자리, 또는 `+`와 5~10자리, 또는 `-`와 4~10자리(`+`는 4자리 연도에 못 쓴다). 월·일은 2자리이고 존재하는 날짜여야 한다.
/// - 뒤에 오프셋(`Z`, `+09:00`, `+09:00:30`)이 붙어도 받고 버린다.
/// - 앞뒤 공백, 시각 부분, 한 자리 월·일은 형식 오류다.
pub fn parse_iso_date(text: &str) -> Option<LocalDate> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut negative = false;
    let mut plus = false;
    match bytes.first()? {
        b'+' => {
            plus = true;
            i += 1;
        }
        b'-' => {
            negative = true;
            i += 1;
        }
        _ => {}
    }
    let year_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let digits = i - year_start;
    let ok_width = if plus {
        (5..=10).contains(&digits)
    } else {
        (4..=10).contains(&digits)
    };
    // 부호 없는 5자리 이상 연도는 받지 않는다(SignStyle.EXCEEDS_PAD).
    let unsigned_too_long = !plus && !negative && digits > 4;
    if !ok_width || unsigned_too_long {
        return None;
    }
    let year: i64 = text[year_start..i].parse().ok()?;
    if year > 999_999_999 {
        return None; // `Year.MAX_VALUE`를 넘으면 `LocalDate`로 만들 수 없다.
    }
    if negative && year == 0 {
        return None; // "-0000"은 연도 0의 부호 있는 표기라 거절된다.
    }
    let year = if negative { -year } else { year };

    let two = |at: usize| -> Option<u32> {
        let a = *bytes.get(at)?;
        let b = *bytes.get(at + 1)?;
        (a.is_ascii_digit() && b.is_ascii_digit())
            .then(|| u32::from(a - b'0') * 10 + u32::from(b - b'0'))
    };
    if bytes.get(i) != Some(&b'-') {
        return None;
    }
    let month = two(i + 1)?;
    if bytes.get(i + 3) != Some(&b'-') {
        return None;
    }
    let day = two(i + 4)?;
    let rest = &text[i + 6..];
    if !valid_offset_suffix(rest) {
        return None;
    }
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(match i16::try_from(year) {
        Ok(y) if (-9999..=9999).contains(&y) => {
            LocalDate::Valid(jiff::civil::date(y, month as i8, day as i8))
        }
        _ if year > 0 => LocalDate::AfterMax,
        _ => LocalDate::BeforeMin,
    })
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
    }
}

/// ISO_DATE의 선택적 오프셋: 없음, `Z`, `+HH:MM`, `+HH:MM:SS`(부호 `+`/`-`).
fn valid_offset_suffix(rest: &str) -> bool {
    if rest.is_empty() || rest == "Z" {
        return true;
    }
    let b = rest.as_bytes();
    if !matches!(b[0], b'+' | b'-') {
        return false;
    }
    let digit2 = |at: usize, max: u8| -> bool {
        match (b.get(at), b.get(at + 1)) {
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                (x - b'0') * 10 + (y - b'0') <= max
            }
            _ => false,
        }
    };
    match b.len() {
        6 => digit2(1, 99) && b[3] == b':' && digit2(4, 59),
        9 => digit2(1, 99) && b[3] == b':' && digit2(4, 59) && b[6] == b':' && digit2(7, 59),
        _ => false,
    }
}

/// 필수 `LocalDate` 쿼리 파라미터(`@RequestParam @DateTimeFormat(iso = DATE) LocalDate`).
pub fn required_date_query(req: &Request, name: &str) -> Result<LocalDate, ApiError> {
    let text = required_query(req, name)?;
    parse_iso_date(text).ok_or_else(|| ApiError::ArgumentTypeMismatch(name.to_owned()))
}

// ---------------------------------------------------------------------------------------------
// JSON 본문
// ---------------------------------------------------------------------------------------------

/// 해석된 JSON 객체 본문. 필드는 Jackson처럼 느슨하게 꺼낸다.
pub struct JsonObject(Map<String, Value>);

impl JsonObject {
    /// 필드의 원본 JSON 값. 문자열 말고 UUID·숫자 필드를 직접 해석해야 하는 DTO가 쓴다.
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    /// 문자열 필드. 없거나 `null`이면 `None`, 숫자·불리언은 문자열로 바꾸고, 배열·객체는 본문 오류(400).
    pub fn string(&self, name: &str) -> Result<Option<String>, ApiError> {
        match self.0.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(Value::Bool(b)) => Ok(Some(b.to_string())),
            Some(Value::Array(_) | Value::Object(_)) => Err(ApiError::UnreadableBody),
        }
    }
}

/// 요청 본문을 JSON 객체로 읽는다(`@RequestBody`).
pub fn read_json_object(req: &Request) -> Result<JsonObject, ApiError> {
    check_content_type(req)?;
    let body = req
        .body
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(&req.body);
    if body
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    {
        return Err(ApiError::UnreadableBody);
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(JsonObject(map)),
        _ => Err(ApiError::UnreadableBody),
    }
}

/// Jackson 변환기가 읽을 수 있는 타입(`application/json`, `application/*+json`)인지 판정한다.
fn check_content_type(req: &Request) -> Result<(), ApiError> {
    let Some(raw) = req.header("content-type") else {
        // 타입이 없고 본문도 없으면 "본문 없음", 본문이 있으면 octet-stream으로 보고 읽을 변환기가 없다.
        return Err(if req.body.is_empty() {
            ApiError::UnreadableBody
        } else {
            ApiError::UnsupportedMediaType
        });
    };
    let Some((kind, subtype)) = parse_media_type(raw) else {
        return Err(ApiError::UnsupportedMediaType);
    };
    if kind == "*" || subtype == "*" {
        return Err(ApiError::Internal(
            "Content-Type cannot contain wildcard type '*'".to_owned(),
        ));
    }
    if kind == "application" && (subtype == "json" || subtype.ends_with("+json")) {
        Ok(())
    } else {
        Err(ApiError::UnsupportedMediaType)
    }
}

/// `type/subtype;param=value` 형태에서 소문자 type·subtype을 꺼낸다. 형식이 틀리면 `None`.
fn parse_media_type(raw: &str) -> Option<(String, String)> {
    let essence = raw.split(';').next()?.trim();
    let (kind, subtype) = essence.split_once('/')?;
    let is_token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    };
    if !is_token(kind) || !is_token(subtype) {
        return None;
    }
    Some((kind.to_ascii_lowercase(), subtype.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_http::Method;

    fn date(s: &str) -> Option<LocalDate> {
        parse_iso_date(s)
    }

    fn valid(y: i16, m: i8, d: i8) -> Option<LocalDate> {
        Some(LocalDate::Valid(jiff::civil::date(y, m, d)))
    }

    // TopicController: @DateTimeFormat(iso = ISO.DATE) LocalDate. 아래 기대값은 java-a에 직접 보낸 결과다.
    #[test]
    fn iso_date는_엄격하게_해석한다() {
        assert_eq!(date("2026-10-12"), valid(2026, 10, 12));
        assert_eq!(date("2026-02-29"), None, "윤년 아님");
        assert_eq!(date("2024-02-29"), valid(2024, 2, 29));
        assert_eq!(date("2026-02-30"), None);
        assert_eq!(date("2026-13-01"), None);
        assert_eq!(date("2026-00-10"), None);
        assert_eq!(date("2026-10-00"), None);
        assert_eq!(date("20261012"), None);
        assert_eq!(date("2026-1-5"), None);
        assert_eq!(date(" 2026-10-12"), None);
        assert_eq!(date("2026-10-12 "), None);
        assert_eq!(date("2026-10-12T00:00:00"), None);
        assert_eq!(date("오늘"), None);
        assert_eq!(date(""), None);
    }

    #[test]
    fn iso_date는_오프셋을_받고_버린다() {
        assert_eq!(date("2026-10-12Z"), valid(2026, 10, 12));
        assert_eq!(date("2026-10-12+09:00"), valid(2026, 10, 12));
        assert_eq!(date("2026-10-12+09:00:30"), valid(2026, 10, 12));
        assert_eq!(date("2026-10-12-05:00"), valid(2026, 10, 12));
        // java-a: +19:00도 통과한다(범위는 해석 단계에서 검사하지 않는다).
        assert_eq!(date("2026-10-12+19:00"), valid(2026, 10, 12));
        assert_eq!(date("2026-10-12+0900"), None);
        assert_eq!(date("2026-10-12+09"), None);
        assert_eq!(date("2026-10-12+09:60"), None);
    }

    #[test]
    fn iso_date_연도_규칙() {
        // 부호 없는 5자리 이상, 4자리 연도의 '+'는 거절. '+'와 5자리 이상, 음수 연도는 허용.
        assert_eq!(date("12345-01-01"), None);
        assert_eq!(date("+2026-10-12"), None);
        assert_eq!(date("+12345-01-01"), Some(LocalDate::AfterMax));
        assert_eq!(date("+999999999-01-01"), Some(LocalDate::AfterMax));
        assert_eq!(date("+1000000000-01-01"), None);
        assert_eq!(date("-0001-01-01"), valid(-1, 1, 1));
        assert_eq!(date("0000-01-01"), valid(0, 1, 1));
        assert_eq!(date("-10000-01-01"), Some(LocalDate::BeforeMin));
        assert_eq!(date("-12345-01-01"), Some(LocalDate::BeforeMin));
        assert_eq!(date("-0000-01-01"), None);
    }

    fn request(query: &[(&str, &str)]) -> Request {
        let mut r = Request::new(Method::Get, "/");
        r.query = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        r
    }

    #[test]
    fn 필수_쿼리는_없거나_공백이면_없음이고_첫_값을_쓴다() {
        let missing = |r: &Request| matches!(required_query(r, "date"), Err(ApiError::MissingRequestParameter(n)) if n == "date");
        assert!(missing(&request(&[])));
        assert!(missing(&request(&[("date", "")])));
        assert!(missing(&request(&[("date", "  \t")])));
        assert!(
            missing(&request(&[("Date", "2026-10-12")])),
            "이름은 대소문자 구분"
        );
        assert!(
            missing(&request(&[("date", ""), ("date", "2026-10-12")])),
            "첫 값이 비면 없음"
        );
        assert_eq!(
            required_query(&request(&[("date", "a"), ("date", "b")]), "date").unwrap(),
            "a"
        );
    }

    #[test]
    fn 날짜_파라미터_형식_오류는_이름이_붙은_400이다() {
        let err = required_date_query(&request(&[("date", "20261012")]), "date").unwrap_err();
        assert_eq!(err.message(), "date: 요청 값의 형식이 올바르지 않습니다.");
        let err = required_date_query(&request(&[]), "date").unwrap_err();
        assert_eq!(err.message(), "date: 필수 요청 값이 없습니다.");
    }

    fn post(content_type: Option<&str>, body: &[u8]) -> Request {
        let mut r = Request::new(Method::Post, "/");
        if let Some(ct) = content_type {
            r.headers.append("Content-Type", ct);
        }
        r.body = body.to_vec();
        r
    }

    fn body_error(r: &Request) -> u16 {
        read_json_object(r).err().expect("오류여야 한다").status()
    }

    // platform/request-body-errors 시나리오와 java-a 직접 확인.
    #[test]
    fn 본문_해석_오류는_400_본문_없음은_400_형식은_415다() {
        let json = Some("application/json");
        assert_eq!(body_error(&post(json, b"")), 400);
        assert_eq!(body_error(&post(json, b"   ")), 400);
        assert_eq!(body_error(&post(json, b"{\"content\": ")), 400);
        for body in [
            &b"[]"[..],
            b"\"x\"",
            b"123",
            b"null",
            b"{} trailing",
            b"{}{}",
        ] {
            assert_eq!(body_error(&post(json, body)), 400, "{body:?}");
        }
        // 타입이 없으면: 본문 없음 400, 본문이 있으면 415
        assert_eq!(body_error(&post(None, b"")), 400);
        assert_eq!(body_error(&post(None, b"{}")), 415);
        for ct in [
            "text/plain",
            "text/json",
            "application/x-www-form-urlencoded",
            "application/json,text/plain",
            "garbage",
        ] {
            assert_eq!(body_error(&post(Some(ct), b"{}")), 415, "{ct}");
            assert_eq!(body_error(&post(Some(ct), b"")), 415, "{ct} 빈 본문");
        }
        for ct in ["*/*", "application/*"] {
            assert_eq!(body_error(&post(Some(ct), b"{}")), 500, "{ct}");
        }
    }

    #[test]
    fn json으로_읽는_타입() {
        for ct in [
            "application/json",
            "APPLICATION/JSON",
            "application/json; charset=utf-8",
            "application/json;charset=ISO-8859-1",
            "application/problem+json",
        ] {
            assert!(read_json_object(&post(Some(ct), b"{}")).is_ok(), "{ct}");
        }
    }

    #[test]
    fn 문자열_필드는_스칼라를_문자열로_바꾼다() {
        let obj = |body: &str| {
            read_json_object(&post(Some("application/json"), body.as_bytes())).unwrap()
        };
        assert_eq!(
            obj(r#"{"c":"좋아요"}"#).string("c").unwrap().as_deref(),
            Some("좋아요")
        );
        assert_eq!(
            obj(r#"{"c":123}"#).string("c").unwrap().as_deref(),
            Some("123")
        );
        assert_eq!(
            obj(r#"{"c":true}"#).string("c").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(obj(r#"{"c":null}"#).string("c").unwrap(), None);
        assert_eq!(obj(r#"{}"#).string("c").unwrap(), None);
        assert_eq!(
            obj(r#"{"C":"x"}"#).string("c").unwrap(),
            None,
            "필드 이름은 대소문자 구분"
        );
        assert_eq!(
            obj(r#"{"c":"a","c":"b"}"#).string("c").unwrap().as_deref(),
            Some("b"),
            "반복되면 마지막 값"
        );
        assert_eq!(
            obj(r#"{"c":"a","unknown":[1]}"#)
                .string("c")
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert!(obj(r#"{"c":["a"]}"#).string("c").is_err());
        assert!(obj(r#"{"c":{}}"#).string("c").is_err());
    }

    #[test]
    fn bom은_허용한다() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice(br#"{"c":"a"}"#);
        let obj = read_json_object(&post(Some("application/json"), &body)).unwrap();
        assert_eq!(obj.string("c").unwrap().as_deref(), Some("a"));
    }

    #[test]
    fn java_공백_판정() {
        assert!(is_java_whitespace(' '));
        assert!(is_java_whitespace('\u{3000}'));
        assert!(!is_java_whitespace('\u{a0}'));
        assert!(!is_java_whitespace('a'));
    }
}
