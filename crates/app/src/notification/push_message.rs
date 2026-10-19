//! SQS 본문을 푸시 메시지로 읽는다. 원본은 `ObjectMapper.readValue(body, PushMessage.class)`(Jackson 3, Spring Boot 기본 설정)다.
//!
//! 발행 쪽 [`PushMessage`](super::sqs_push::PushMessage)는 항상 완전한 값이지만 Worker가 읽는 메시지는 외부 입력이라 필드가 모두
//! 비어 있을 수 있다. 읽을 수 있는 모양과 오류 이름은 Jackson 동작을 원본 JAR에 돌려 기록한 표(`tests/fixtures/expected_push_message.tsv`,
//! 생성은 `PushMessageFixture.java`)에 맞춘다(`tests/push_message_java_parity.rs`).
//!
//! - 첫 JSON 값 뒤에는 공백만 올 수 있다. 값이 더 있으면 `MismatchedInputException`, 읽을 수 없는 글자면 `StreamReadException`이다.
//! - 알 수 없는 속성은 무시하고 같은 속성이 여럿이면 마지막 값을 쓴다. 속성은 문서 순서대로 변환하므로 오류는 앞 속성이 먼저다.
//! - UUID는 앞뒤 공백을 떼고 36자 표기(대소문자 무관)나 24자 base64(16바이트)만 받는다. 빈 문자열은 값 없음이다.
//! - 열거형은 앞뒤 공백을 뗀 이름이나 순서 번호(숫자 또는 숫자 문자열)를 받는다.
//! - `Instant`는 ISO-8601 문자열(`Z` 또는 `±HH:MM[:SS]`), 에포크 초(정수·소수, 숫자 또는 문자열)를 받는다.
//!
//! 알려진 차이: Rust 쪽 시각은 `jiff::Timestamp`라 연도 ±9999를 넘는 시각(원본은 ±10억 년까지)은 읽지 못하고
//! `InvalidFormatException`으로 다룬다. 운영 메시지는 발행기가 만든 값이라 이 범위를 넘지 않는다.

use jiff::Timestamp;
use jiff::civil;
use uuid::Uuid;

use super::domain::{NotificationSourceType, NotificationType};

/// Worker가 읽은 메시지. 모든 필드가 비어 있을 수 있다(`PushMessage` 레코드).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RawPushMessage {
    pub event_id: Option<Uuid>,
    pub notification_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub kind: Option<NotificationType>,
    pub source_type: Option<NotificationSourceType>,
    pub source_id: Option<Uuid>,
    pub occurred_at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
}

const MISMATCHED: &str = "MismatchedInputException";
const INVALID_FORMAT: &str = "InvalidFormatException";
const STREAM_READ: &str = "StreamReadException";
const UNEXPECTED_EOF: &str = "UnexpectedEndOfInputException";
const INPUT_COERCION: &str = "InputCoercionException";
const DATE_TIME_PARSE: &str = "DateTimeParseException";
const CONSTRAINTS: &str = "StreamConstraintsException";

/// Jackson의 기본 중첩 깊이 제한(루트 객체를 1로 센다).
const MAX_DEPTH: usize = 500;
/// `Instant`가 다루는 에포크 초의 한계(±1000000000년).
const INSTANT_MAX_SECONDS: i64 = 31_556_889_864_403_199;

/// `readValue`: 본문이 JSON `null`이면 `Ok(None)`, 읽을 수 없으면 Jackson 예외의 단순 이름을 돌려준다.
///
/// Jackson처럼 토큰을 읽는 대로 변환한다. 그래서 문서 뒤쪽이 깨져 있어도 앞 속성의 변환 오류가 먼저 나오고,
/// 루트가 배열·문자열이면 내용을 읽지 않고 바로 거절한다.
pub fn parse_push_message(body: &str) -> Result<Option<RawPushMessage>, &'static str> {
    let mut reader = Reader {
        bytes: body.as_bytes(),
        text: body,
        position: 0,
    };
    reader.skip_whitespace();
    let message = match reader.peek() {
        None => return Err(MISMATCHED),
        Some(b'{') => Some(reader.root_object()?),
        Some(b'[' | b'"') => return Err(MISMATCHED),
        Some(b'-' | b'0'..=b'9') => {
            reader.number()?;
            return Err(MISMATCHED);
        }
        Some(b't') => {
            reader.literal("true", Json::Null)?;
            return Err(MISMATCHED);
        }
        Some(b'f') => {
            reader.literal("false", Json::Null)?;
            return Err(MISMATCHED);
        }
        Some(b'n') => {
            reader.literal("null", Json::Null)?;
            None
        }
        Some(_) => return Err(STREAM_READ),
    };
    reader.skip_whitespace();
    if reader.position < reader.bytes.len() {
        // 값 뒤의 토큰을 읽어 보고(읽을 수 없으면 읽기 오류) 있으면 거절한다(`FAIL_ON_TRAILING_TOKENS`).
        reader.value_start_token()?;
        return Err(MISMATCHED);
    }
    Ok(message)
}

fn assign(message: &mut RawPushMessage, name: &str, value: &Json) -> Result<(), &'static str> {
    match name {
        "eventId" => message.event_id = uuid(value)?,
        "notificationId" => message.notification_id = uuid(value)?,
        "userId" => message.user_id = uuid(value)?,
        "sourceId" => message.source_id = uuid(value)?,
        "type" => {
            message.kind = enum_value(
                value,
                &[
                    ("POST_APPROVED", NotificationType::PostApproved),
                    ("POST_REJECTED", NotificationType::PostRejected),
                    ("TOPIC", NotificationType::Topic),
                    ("NOTICE", NotificationType::Notice),
                ],
            )?;
        }
        "sourceType" => {
            message.source_type = enum_value(
                value,
                &[
                    ("POST", NotificationSourceType::Post),
                    ("TOPIC", NotificationSourceType::Topic),
                    ("NOTICE", NotificationSourceType::Notice),
                ],
            )?;
        }
        "occurredAt" => message.occurred_at = instant(value)?,
        "expiresAt" => message.expires_at = instant(value)?,
        _ => {}
    }
    Ok(())
}

/// `String.trim()`: U+0020 이하의 앞뒤 글자를 뗀다.
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

fn uuid(value: &Json) -> Result<Option<Uuid>, &'static str> {
    match value {
        Json::Null => Ok(None),
        Json::String(text) => {
            let text = trim(text);
            if text.is_empty() {
                return Ok(None);
            }
            parse_uuid(text).map(Some).ok_or(INVALID_FORMAT)
        }
        Json::Number(_) | Json::Bool(_) => Err(INVALID_FORMAT),
        Json::Array | Json::Object => Err(MISMATCHED),
    }
}

/// `UUIDDeserializer`: 36자 표기이거나, 24자면 base64로 푼 16바이트.
fn parse_uuid(text: &str) -> Option<Uuid> {
    if text.len() == 36 {
        let bytes = text.as_bytes();
        if [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-') {
            return None;
        }
        let hex: String = text.chars().filter(|&c| c != '-').collect();
        if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return Uuid::parse_str(&hex).ok();
    }
    if text.len() == 24 {
        let decoded = base64_decode(text)?;
        let array: [u8; 16] = decoded.try_into().ok()?;
        return Some(Uuid::from_bytes(array));
    }
    None
}

/// 표준 base64(패딩 필수).
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    let mut padding = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            _ => return None,
        };
        if padding > 0 {
            return None;
        }
        buffer = buffer << 6 | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if !text.len().is_multiple_of(4) {
        return None;
    }
    Some(out)
}

fn enum_value<T: Copy>(value: &Json, names: &[(&str, T)]) -> Result<Option<T>, &'static str> {
    let ordinal = |index: i64| -> Result<Option<T>, &'static str> {
        usize::try_from(index)
            .ok()
            .and_then(|i| names.get(i))
            .map(|(_, v)| Some(*v))
            .ok_or(INVALID_FORMAT)
    };
    match value {
        Json::Null => Ok(None),
        Json::String(text) => {
            let text = trim(text);
            if let Some((_, v)) = names.iter().find(|(name, _)| *name == text) {
                return Ok(Some(*v));
            }
            // 숫자 문자열은 순서 번호로 읽는다.
            let digits = text.strip_prefix('-').unwrap_or(text);
            if !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && let Ok(index) = text.parse::<i64>()
            {
                return ordinal(index);
            }
            Err(INVALID_FORMAT)
        }
        Json::Number(raw) if is_integer_literal(raw) => match raw.parse::<i64>() {
            Ok(index) => ordinal(index),
            Err(_) => Err(INVALID_FORMAT),
        },
        Json::Number(_) | Json::Bool(_) | Json::Array | Json::Object => Err(MISMATCHED),
    }
}

fn is_integer_literal(raw: &str) -> bool {
    !raw.contains(['.', 'e', 'E'])
}

fn instant(value: &Json) -> Result<Option<Timestamp>, &'static str> {
    match value {
        Json::Null => Ok(None),
        Json::String(text) => {
            let text = trim(text);
            if text.is_empty() {
                return Ok(None);
            }
            if is_plain_decimal(text) {
                return epoch_decimal(text).map(Some);
            }
            parse_iso_instant(text).map(Some).ok_or(INVALID_FORMAT)
        }
        Json::Number(raw) => epoch_number(raw).map(Some),
        Json::Bool(_) | Json::Array | Json::Object => Err(MISMATCHED),
    }
}

/// `-?숫자+` 또는 `-?숫자+.숫자+`
fn is_plain_decimal(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    let mut parts = body.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

fn epoch_number(raw: &str) -> Result<Timestamp, &'static str> {
    if is_plain_decimal(raw) {
        return epoch_decimal(raw);
    }
    // 지수 표기는 부동소수점으로 읽는다.
    let number: f64 = raw.parse().map_err(|_| INVALID_FORMAT)?;
    if !number.is_finite() || number.abs() >= INSTANT_MAX_SECONDS as f64 {
        return Err(DATE_TIME_PARSE);
    }
    let seconds = number.floor();
    let nanos = ((number - seconds) * 1e9).round() as i128;
    from_nanos(i128::from(seconds as i64) * 1_000_000_000 + nanos)
}

fn epoch_decimal(text: &str) -> Result<Timestamp, &'static str> {
    let negative = text.starts_with('-');
    let body = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    let seconds: i64 = whole.parse().map_err(|_| INPUT_COERCION)?;
    if seconds > INSTANT_MAX_SECONDS {
        return Err(DATE_TIME_PARSE);
    }
    // 소수부는 9자리까지(나노초) 쓴다.
    let mut digits: String = fraction.chars().take(9).collect();
    while digits.len() < 9 {
        digits.push('0');
    }
    let nanos: i128 = digits.parse().map_err(|_| INVALID_FORMAT)?;
    let total = i128::from(seconds) * 1_000_000_000 + nanos;
    from_nanos(if negative { -total } else { total })
}

fn from_nanos(total: i128) -> Result<Timestamp, &'static str> {
    // jiff 범위(±9999년)를 넘는 값은 읽지 못한다(모듈 설명 참고).
    Timestamp::from_nanosecond(total).map_err(|_| INVALID_FORMAT)
}

/// `DateTimeFormatter.ISO_INSTANT`가 받는 문자열. 연도는 4자리이거나 부호가 있는 5~10자리, `T`·`Z`는 대소문자 무관,
/// 초는 필수이고 소수는 9자리까지, 24:00:00과 23:59:60(윤초)을 받으며 오프셋은 `±HH:MM[:SS]`다.
fn parse_iso_instant(text: &str) -> Option<Timestamp> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let negative = bytes.first() == Some(&b'-');
    let positive = bytes.first() == Some(&b'+');
    if negative || positive {
        at += 1;
    }
    let year_start = at;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    let year_digits = at - year_start;
    if !(4..=10).contains(&year_digits) {
        return None;
    }
    // 4자리 넘는 연도는 `+`가 있어야 하고, `+`는 4자리를 넘을 때만 쓴다.
    if (year_digits > 4 && !negative && !positive) || (positive && year_digits == 4) {
        return None;
    }
    let mut year: i64 = text[year_start..at].parse().ok()?;
    if negative {
        year = -year;
    }
    if year.abs() > 1_000_000_000 {
        return None;
    }
    let two = |at: &mut usize| -> Option<i64> {
        let slice = text.get(*at..*at + 2)?;
        if !slice.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *at += 2;
        slice.parse().ok()
    };
    let expect = |at: &mut usize, chars: &[u8]| -> Option<()> {
        if chars.contains(bytes.get(*at)?) {
            *at += 1;
            Some(())
        } else {
            None
        }
    };
    expect(&mut at, b"-")?;
    let month = two(&mut at)?;
    expect(&mut at, b"-")?;
    let day = two(&mut at)?;
    expect(&mut at, b"Tt")?;
    let hour = two(&mut at)?;
    expect(&mut at, b":")?;
    let minute = two(&mut at)?;
    expect(&mut at, b":")?;
    let second = two(&mut at)?;
    let mut nanos: i64 = 0;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        let digits = at - start;
        if !(1..=9).contains(&digits) {
            return None;
        }
        nanos = format!("{:0<9}", &text[start..at]).parse().ok()?;
    }
    let offset = match bytes.get(at)? {
        b'Z' | b'z' => {
            at += 1;
            0
        }
        sign @ (b'+' | b'-') => {
            at += 1;
            let hours = two(&mut at)?;
            expect(&mut at, b":")?;
            let minutes = two(&mut at)?;
            let mut seconds = 0;
            if bytes.get(at) == Some(&b':') {
                at += 1;
                seconds = two(&mut at)?;
            }
            if hours > 18
                || minutes > 59
                || seconds > 59
                || (hours == 18 && (minutes | seconds) > 0)
            {
                return None;
            }
            let total = hours * 3600 + minutes * 60 + seconds;
            if *sign == b'-' { -total } else { total }
        }
        _ => return None,
    };
    if at != bytes.len() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if minute > 59 {
        return None;
    }
    let (mut hour, mut second, mut extra_days) = (hour, second, 0);
    if hour == 24 {
        if minute != 0 || second != 0 || nanos != 0 {
            return None;
        }
        hour = 0;
        extra_days = 1;
    } else if hour == 23 && minute == 59 && second == 60 {
        second = 59;
    } else if hour > 23 || second > 59 {
        return None;
    }
    let year = i16::try_from(year).ok()?;
    let date = civil::Date::new(year, month as i8, day as i8).ok()?;
    let local = date.at(hour as i8, minute as i8, second as i8, 0);
    let seconds = local
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()?
        .timestamp()
        .as_second()
        + extra_days * 86_400
        - offset;
    Timestamp::from_nanosecond(i128::from(seconds) * 1_000_000_000 + i128::from(nanos)).ok()
}

// ---- JSON 읽기 ----

enum Json {
    Null,
    Bool(#[allow(dead_code)] bool),
    /// 원문 글자 그대로(정수와 소수를 구별하려고 값으로 바꾸지 않는다).
    Number(String),
    String(String),
    Array,
    Object,
}

struct Reader<'a> {
    bytes: &'a [u8],
    text: &'a str,
    position: usize,
}

impl Reader<'_> {
    /// 루트 객체. 속성을 읽는 대로 변환한다(알 수 없는 속성은 내용을 건너뛴다).
    fn root_object(&mut self) -> Result<RawPushMessage, &'static str> {
        let mut message = RawPushMessage::default();
        self.position += 1;
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(message);
        }
        loop {
            let name = self.field_name()?;
            let known = matches!(
                name.as_str(),
                "eventId"
                    | "notificationId"
                    | "userId"
                    | "sourceId"
                    | "type"
                    | "sourceType"
                    | "occurredAt"
                    | "expiresAt"
            );
            self.skip_whitespace();
            if known && matches!(self.peek(), Some(b'{' | b'[')) {
                // 객체·배열은 내용을 읽지 않고 거절한다.
                return Err(MISMATCHED);
            }
            let value = self.value(1)?;
            if known {
                assign(&mut message, &name, &value)?;
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    return Ok(message);
                }
                None => return Err(UNEXPECTED_EOF),
                Some(_) => return Err(STREAM_READ),
            }
        }
    }

    /// 속성 이름(`"..."` 다음 `:`)까지 읽는다.
    fn field_name(&mut self) -> Result<String, &'static str> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => {}
            None => return Err(UNEXPECTED_EOF),
            Some(_) => return Err(STREAM_READ),
        }
        let name = self.string()?;
        self.skip_whitespace();
        match self.peek() {
            Some(b':') => self.position += 1,
            None => return Err(UNEXPECTED_EOF),
            Some(_) => return Err(STREAM_READ),
        }
        Ok(name)
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.position),
            Some(b' ' | b'\t' | b'\n' | b'\r')
        ) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    /// 값 뒤에 이어진 토큰이 읽히는지만 본다. Jackson은 토큰 하나만 읽으므로 `{`·`[`·`"`는 시작만 보고(내용은 읽지 않는다)
    /// 숫자와 `true`·`false`·`null`은 끝까지 읽는다. 읽을 수 없으면 오류.
    fn value_start_token(&mut self) -> Result<(), &'static str> {
        match self.peek() {
            Some(b'{' | b'[' | b'"') => Ok(()),
            Some(b'-' | b'0'..=b'9') => self.number().map(|_| ()),
            Some(b't') => self.literal("true", Json::Null).map(|_| ()),
            Some(b'f') => self.literal("false", Json::Null).map(|_| ()),
            Some(b'n') => self.literal("null", Json::Null).map(|_| ()),
            None => Err(UNEXPECTED_EOF),
            Some(_) => Err(STREAM_READ),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, &'static str> {
        self.skip_whitespace();
        match self.peek() {
            None => Err(UNEXPECTED_EOF),
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string().map(Json::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(_) => Err(STREAM_READ),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, &'static str> {
        let rest = &self.bytes[self.position..];
        // 낱말이 끝까지 맞지 않으면(짧게 끝나도) 알 수 없는 토큰이다.
        if rest.len() < word.len() || &rest[..word.len()] != word.as_bytes() {
            return Err(STREAM_READ);
        }
        self.position += word.len();
        // 이름에 쓸 수 있는 글자가 바로 이어지면 다른 토큰이다(`truex`).
        if self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(STREAM_READ);
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, &'static str> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.position += 1;
                // 앞자리 0 뒤에 숫자가 오면 거절한다.
                if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    return Err(STREAM_READ);
                }
            }
            Some(b'1'..=b'9') => self.digits(),
            None => return Err(UNEXPECTED_EOF),
            Some(_) => return Err(STREAM_READ),
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            self.require_digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            self.require_digits()?;
        }
        // 숫자 바로 뒤에 이름 글자가 오면 잘못된 토큰이다.
        if self
            .peek()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        {
            return Err(STREAM_READ);
        }
        Ok(Json::Number(self.text[start..self.position].to_owned()))
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.position += 1;
        }
    }

    fn require_digits(&mut self) -> Result<(), &'static str> {
        match self.peek() {
            Some(b'0'..=b'9') => {
                self.digits();
                Ok(())
            }
            None => Err(UNEXPECTED_EOF),
            Some(_) => Err(STREAM_READ),
        }
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.position += 1; // 여는 따옴표
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(UNEXPECTED_EOF);
            };
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.position += 1;
                    let Some(escape) = self.peek() else {
                        return Err(UNEXPECTED_EOF);
                    };
                    self.position += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(STREAM_READ),
                    }
                }
                0..=0x1f => return Err(STREAM_READ),
                _ => {
                    // 글자 하나를 그대로 옮긴다(입력이 `&str`이라 경계가 맞다).
                    let rest = &self.text[self.position..];
                    let c = rest.chars().next().ok_or(UNEXPECTED_EOF)?;
                    out.push(c);
                    self.position += c.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, &'static str> {
        let mut value = 0;
        for _ in 0..4 {
            let Some(byte) = self.peek() else {
                return Err(UNEXPECTED_EOF);
            };
            let digit = (byte as char).to_digit(16).ok_or(STREAM_READ)?;
            value = value * 16 + digit;
            self.position += 1;
        }
        Ok(value)
    }

    /// `\uXXXX`. 짝이 맞는 서로게이트 쌍은 한 글자로 합치고, 짝이 없으면 대체 문자로 바꾼다.
    fn unicode_escape(&mut self) -> Result<char, &'static str> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) && self.bytes[self.position..].starts_with(b"\\u") {
            let saved = self.position;
            self.position += 2;
            let second = self.hex4()?;
            if (0xDC00..0xE000).contains(&second) {
                let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                return Ok(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            self.position = saved;
        }
        Ok(char::from_u32(first).unwrap_or('\u{FFFD}'))
    }

    fn array(&mut self, depth: usize) -> Result<Json, &'static str> {
        if depth > MAX_DEPTH {
            return Err(CONSTRAINTS);
        }
        self.position += 1;
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(Json::Array);
        }
        loop {
            self.value(depth)?;
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    return Ok(Json::Array);
                }
                None => return Err(UNEXPECTED_EOF),
                Some(_) => return Err(STREAM_READ),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, &'static str> {
        if depth > MAX_DEPTH {
            return Err(CONSTRAINTS);
        }
        self.position += 1;
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(Json::Object);
        }
        loop {
            self.field_name()?;
            self.value(depth)?;
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    return Ok(Json::Object);
                }
                None => return Err(UNEXPECTED_EOF),
                Some(_) => return Err(STREAM_READ),
            }
        }
    }
}
