//! 원본 `appversion.domain`: `AppPlatform`, `MinimumSupportedVersion`, `StoreUrl`, `ChangeReason`, `AppVersion`.
//!
//! 입력 검증은 값 객체 생성자가 맡는다(원본 record 생성자와 같다). 검증 실패는 모두 `BUSINESS_ERROR` 400이다.
//! 스토어 주소 검증은 원본이 `java.net.URI`로 한 해석을 옮긴 것이다. 허용하는 주소가 아주 좁아서(HTTPS, 정해진 두 호스트,
//! 고정 경로) 일반 URI 해석기를 만들지 않고, 그 좁은 모양에서 `java.net.URI`가 받아들이는 문자 집합과 같은 규칙만 쓴다.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use std::collections::HashMap;
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

/// 원본 `String.strip()`: 앞뒤의 `Character.isWhitespace` 문자를 뗀다.
pub fn java_strip(text: &str) -> &str {
    text.trim_matches(is_java_whitespace)
}

// ---------------------------------------------------------------------------------------------
// AppPlatform
// ---------------------------------------------------------------------------------------------

/// 선언 순서(ANDROID, IOS)가 관리자 목록의 순서다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppPlatform {
    Android,
    Ios,
}

impl AppPlatform {
    pub const ALL: [AppPlatform; 2] = [AppPlatform::Android, AppPlatform::Ios];

    pub const fn as_str(self) -> &'static str {
        match self {
            AppPlatform::Android => "ANDROID",
            AppPlatform::Ios => "IOS",
        }
    }

    /// `AppPlatform.valueOf`: 이름이 정확히 같아야 한다.
    pub fn parse(name: &str) -> Option<AppPlatform> {
        AppPlatform::ALL.into_iter().find(|p| p.as_str() == name)
    }

    /// Spring `StringToEnumConverterFactory`: 앞뒤 공백(`String.trim`)을 떼고 이름이 정확히 같아야 한다.
    /// 공백을 뗀 값이 비어 있으면 값이 없는 것(`Ok(None)`)이다.
    pub fn from_request_text(text: &str) -> Result<Option<AppPlatform>, UnknownPlatform> {
        let trimmed = text.trim_matches(|c: char| c <= ' ');
        if trimmed.is_empty() {
            return Ok(None);
        }
        AppPlatform::parse(trimmed).map(Some).ok_or(UnknownPlatform)
    }
}

/// 요청 값이 알려진 플랫폼 이름이 아니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownPlatform;

// ---------------------------------------------------------------------------------------------
// MinimumSupportedVersion
// ---------------------------------------------------------------------------------------------

const MIN_VERSION_MESSAGE: &str =
    "최소 지원 버전은 앞자리 0 없이 0~2147483647 범위의 major.minor.patch여야 합니다.";

/// `major.minor.patch`. 각 숫자는 0 또는 앞자리 0 없는 10자리 이하, 값은 0~2147483647.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MinimumSupportedVersion(String);

impl MinimumSupportedVersion {
    pub fn new(value: &str) -> Result<MinimumSupportedVersion, AppError> {
        let parts: Vec<&str> = value.split('.').collect();
        let valid = parts.len() == 3
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part.len() <= 10
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && (*part == "0" || !part.starts_with('0'))
                    && part.parse::<u64>().is_ok_and(|n| n <= i32::MAX as u64)
            });
        if valid {
            Ok(MinimumSupportedVersion(value.to_owned()))
        } else {
            Err(business(MIN_VERSION_MESSAGE))
        }
    }

    pub fn value(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------------------------
// ChangeReason
// ---------------------------------------------------------------------------------------------

const REASON_MESSAGE: &str = "변경 사유는 앞뒤 공백을 제외하고 1~500자여야 합니다.";
const MAX_REASON_LENGTH: usize = 500;

/// 앞뒤 공백을 뗀 1~500 code point 사유.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeReason(String);

impl ChangeReason {
    pub fn new(value: &str) -> Result<ChangeReason, AppError> {
        let stripped = java_strip(value);
        if stripped.is_empty() || stripped.chars().count() > MAX_REASON_LENGTH {
            return Err(business(REASON_MESSAGE));
        }
        Ok(ChangeReason(stripped.to_owned()))
    }

    pub fn value(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------------------------
// StoreUrl
// ---------------------------------------------------------------------------------------------

const STORE_URL_MESSAGE: &str = "스토어 주소는 해당 플랫폼의 공식 HTTPS 앱 상세 주소여야 합니다.";
const MAX_STORE_URL_LENGTH: usize = 2_000;
const ANDROID_PREFIX: &str = "https://play.google.com";
const ANDROID_HOST: &str = "play.google.com";
const IOS_HOST: &str = "apps.apple.com";

/// 플랫폼별 공식 HTTPS 앱 상세 주소. 저장된 원문 그대로를 검증하며 값을 바꾸지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreUrl(String);

impl StoreUrl {
    /// 원본 `new StoreUrl(value)`: 이미 정규화된 원문(DB에서 읽은 값 포함)을 검증한다.
    pub fn new(value: &str) -> Result<StoreUrl, AppError> {
        if value.chars().all(is_java_whitespace)
            || java_strip(value) != value
            || value.chars().count() > MAX_STORE_URL_LENGTH
            || !is_valid_store_url(value)
        {
            return Err(invalid_url());
        }
        Ok(StoreUrl(value.to_owned()))
    }

    /// 원본 `StoreUrl.from`: 요청 입력의 앞뒤 공백(`strip`)을 떼고 검증한다.
    pub fn from_input(input: &str) -> Result<StoreUrl, AppError> {
        StoreUrl::new(java_strip(input))
    }

    pub fn value(&self) -> &str {
        &self.0
    }

    /// 주소의 앞부분이 플랫폼을 정한다(`getPlatform`).
    pub fn platform(&self) -> AppPlatform {
        if starts_with_ignore_ascii_case(&self.0, ANDROID_PREFIX) {
            AppPlatform::Android
        } else {
            AppPlatform::Ios
        }
    }

    pub fn validate_platform(&self, platform: AppPlatform) -> Result<(), AppError> {
        if self.platform() == platform {
            Ok(())
        } else {
            Err(invalid_url())
        }
    }
}

fn invalid_url() -> AppError {
    business(STORE_URL_MESSAGE)
}

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// `java.lang.Character.isSpaceChar`가 참인 비ASCII 문자(Zs, Zl, Zp).
fn is_java_space_char(c: char) -> bool {
    matches!(
        c,
        '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `java.net.URI`의 "other" 문자: 비ASCII이면서 공백 문자도 제어 문자도 아닌 것.
fn is_uri_other(c: char) -> bool {
    (c as u32) > 128 && !is_java_space_char(c) && !c.is_control()
}

/// 경로에 쓸 수 있는 ASCII: `L_PATH` = 영숫자, 표시 문자(`-_.!~*'()`), `:@&=+$,`, `;/`.
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_.!~*'():@&=+$,;/".contains(c)
}

/// 쿼리에 쓸 수 있는 ASCII: `L_URIC` = 예약 문자(`;/?:@&=+$,[]`) + 영숫자 + 표시 문자.
fn is_query_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_.!~*'();/?:@&=+$,[]".contains(c)
}

/// 문자 집합 검사. `%` 뒤에는 16진수 두 자리가 와야 한다(`Malformed escape pair`).
fn has_only_legal_chars(text: &str, is_ascii_legal: fn(char) -> bool) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or('\0');
        if c == '%' {
            let hex = bytes.get(i + 1..i + 3);
            if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                return false;
            }
            i += 3;
            continue;
        }
        if !(c.is_ascii() && is_ascii_legal(c) || is_uri_other(c)) {
            return false;
        }
        i += c.len_utf8();
    }
    true
}

/// `%HH`를 풀어 바이트로 만든다(앞서 `has_only_legal_chars`를 통과한 문자열에만 쓴다). `plus_is_space`면 `+`는 공백이다.
fn percent_decode(text: &str, plus_is_space: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 경로 해석: `%2f`(구분자 인코딩), `%5c`(역슬래시), 풀었을 때 `.`·`..`인 조각, 연속 슬래시를 거절한다.
/// 원본은 `getRawPath()`에 `(?i).*%2f.*`, 풀린 경로에 `\`와 `.`·`..` 조각, `normalize()` 불변성을 확인한다.
fn is_valid_path(raw_path: &str) -> bool {
    let lower = raw_path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") || raw_path.contains("//") {
        return false;
    }
    let decoded = percent_decode(raw_path, false);
    !decoded.split('/').any(|part| part == "." || part == "..")
}

/// 쿼리를 `&`로 나눠 `이름=값` 쌍을 푼다. 값 없는 조각, 중복 이름은 거절한다(`None`).
fn parse_query(raw_query: Option<&str>) -> Option<HashMap<String, String>> {
    let mut values = HashMap::new();
    let Some(raw_query) = raw_query else {
        return Some(values);
    };
    for part in raw_query.split('&') {
        let (key, value) = part.split_once('=')?;
        let key = percent_decode(key, true);
        let value = percent_decode(value, true);
        if values.insert(key, value).is_some() {
            return None;
        }
    }
    Some(values)
}

fn is_language(value: &str) -> bool {
    (2..=10).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'-')
}

fn has_language(query: &HashMap<String, String>, name: &str) -> bool {
    query.get(name).is_none_or(|v| is_language(v))
}

/// `[A-Za-z][A-Za-z0-9_]*(\.[A-Za-z][A-Za-z0-9_]*)+`
fn is_android_package(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|part| {
            let mut bytes = part.bytes();
            bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn is_android(host: &str, raw_path: &str, query: &HashMap<String, String>) -> bool {
    host == ANDROID_HOST
        && raw_path == "/store/apps/details"
        && query.get("id").is_some_and(|id| is_android_package(id))
        && query
            .keys()
            .all(|k| matches!(k.as_str(), "id" | "hl" | "gl"))
        && has_language(query, "hl")
        && has_language(query, "gl")
}

/// `/(?:[a-zA-Z]{2}/)?app/(?:[^/]+/)?id[1-9][0-9]*`
fn is_ios_path(raw_path: &str) -> bool {
    let Some(rest) = raw_path.strip_prefix('/') else {
        return false;
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let is_country = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic());
    let is_id = |s: &str| {
        s.strip_prefix("id").is_some_and(|d| {
            d.bytes().next().is_some_and(|b| (b'1'..=b'9').contains(&b))
                && d.bytes().all(|b| b.is_ascii_digit())
        })
    };
    match segments.as_slice() {
        ["app", id] => is_id(id),
        [a, "app", id] if is_country(a) => is_id(id),
        ["app", name, id] => !name.is_empty() && is_id(id),
        [a, "app", name, id] if is_country(a) => !name.is_empty() && is_id(id),
        _ => false,
    }
}

fn is_ios(host: &str, raw_path: &str, query: &HashMap<String, String>) -> bool {
    host == IOS_HOST
        && is_ios_path(raw_path)
        && query.keys().all(|k| matches!(k.as_str(), "l" | "mt"))
        && has_language(query, "l")
        && query.get("mt").is_none_or(|v| v == "8")
}

/// 스킴·권한·경로·쿼리·조각 검사와 플랫폼별 앱 상세 모양 검사.
fn is_valid_store_url(value: &str) -> bool {
    let Some(rest) = value
        .get(..8)
        .filter(|head| head.eq_ignore_ascii_case("https://"))
        .map(|_| &value[8..])
    else {
        return false;
    };
    // 조각이 있거나 `#` 뒤가 틀린 주소는 모두 거절한다.
    if rest.contains('#') {
        return false;
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    // 사용자 정보, `:443` 아닌 포트, 빈 포트는 모두 이 목록에 없다. 호스트는 대소문자를 구분하지 않는다.
    let authority = authority.to_ascii_lowercase();
    let host = match authority.strip_suffix(":443") {
        Some(host) => host,
        None => authority.as_str(),
    };
    if host != ANDROID_HOST && host != IOS_HOST {
        return false;
    }
    let (raw_path, raw_query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (tail, None),
    };
    if !has_only_legal_chars(raw_path, is_path_char)
        || raw_query.is_some_and(|q| !has_only_legal_chars(q, is_query_char))
        || !is_valid_path(raw_path)
    {
        return false;
    }
    let Some(query) = parse_query(raw_query) else {
        return false;
    };
    is_android(host, raw_path, &query) || is_ios(host, raw_path, &query)
}

// ---------------------------------------------------------------------------------------------
// AppVersion
// ---------------------------------------------------------------------------------------------

/// 플랫폼별 최소 지원 버전 설정 한 행.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppVersion {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub platform: AppPlatform,
    pub minimum_supported_version: MinimumSupportedVersion,
    pub store_url: StoreUrl,
    /// 낙관적 잠금 버전. 최초 저장은 0이고 변경마다 1 늘어난다.
    pub revision: i64,
    pub last_changed_by: Option<Uuid>,
    pub change_reason: Option<String>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl AppVersion {
    /// `AppVersion.create`
    pub fn create(
        platform: AppPlatform,
        minimum_supported_version: MinimumSupportedVersion,
        store_url: StoreUrl,
        admin_id: Uuid,
        reason: &ChangeReason,
    ) -> Result<AppVersion, AppError> {
        store_url.validate_platform(platform)?;
        Ok(AppVersion {
            id: None,
            platform,
            minimum_supported_version,
            store_url,
            revision: 0,
            last_changed_by: Some(admin_id),
            change_reason: Some(reason.value().to_owned()),
            created_at: None,
            updated_at: None,
        })
    }

    /// `AppVersion.update`: 플랫폼이 주소와 맞지 않으면 아무것도 바꾸지 않는다.
    pub fn update(
        &mut self,
        minimum_supported_version: MinimumSupportedVersion,
        store_url: StoreUrl,
        admin_id: Uuid,
        reason: &ChangeReason,
    ) -> Result<(), AppError> {
        store_url.validate_platform(self.platform)?;
        self.minimum_supported_version = minimum_supported_version;
        self.store_url = store_url;
        self.last_changed_by = Some(admin_id);
        self.change_reason = Some(reason.value().to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(result: Result<impl std::fmt::Debug, AppError>) -> String {
        let e = result.unwrap_err();
        assert_eq!(e.code, ErrorCode::BUSINESS_ERROR);
        e.message
    }

    // ----- MinimumSupportedVersionTest -----

    #[test]
    fn 버전_각_자리의_0부터_정수_상한까지_정규_형식을_허용한다() {
        for v in [
            "0.0.0",
            "1.10.0",
            "2147483646.2147483646.2147483646",
            "2147483647.2147483647.2147483647",
        ] {
            assert_eq!(MinimumSupportedVersion::new(v).unwrap().value(), v);
        }
    }

    #[test]
    fn 버전_형식_앞자리_0_정수_상한을_어기면_거절한다() {
        for v in [
            "",
            " ",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "-1.0.0",
            "v1.2.3",
            "1.2.3-beta",
            "1.2.3+4",
            "1.2.3 ",
            "2147483648.0.0",
            "0.2147483648.0",
            "0.0.2147483648",
            "11111111111.0.0",
            "1..3",
            "٣.0.0",
            "1.2.3\n",
        ] {
            assert_eq!(
                kind(MinimumSupportedVersion::new(v)),
                "최소 지원 버전은 앞자리 0 없이 0~2147483647 범위의 major.minor.patch여야 합니다.",
                "{v:?}"
            );
        }
    }

    // ----- ChangeReasonTest -----

    #[test]
    fn 사유는_공백_제거_후_code_point_상한까지_허용한다() {
        for count in [1, 499, 500] {
            let reason = ChangeReason::new(&format!("  {}  ", "😀".repeat(count))).unwrap();
            assert_eq!(reason.value(), "😀".repeat(count));
        }
    }

    #[test]
    fn 사유가_공백뿐이면_거절한다() {
        for r in ["", " ", "\n\t", "\u{2003}"] {
            assert_eq!(
                kind(ChangeReason::new(r)),
                "변경 사유는 앞뒤 공백을 제외하고 1~500자여야 합니다."
            );
        }
    }

    #[test]
    fn 사유_길이_상한을_넘으면_거절한다() {
        for c in ["a", "😀"] {
            assert!(ChangeReason::new(&c.repeat(501)).is_err());
        }
    }

    // ----- StoreUrlTest -----

    #[test]
    fn 공식_스토어_상세_주소의_플랫폼_앱_id_언어_형식을_허용한다() {
        for (platform, value) in [
            (
                AppPlatform::Android,
                "https://play.google.com/store/apps/details?id=com.example.chalkak",
            ),
            (
                AppPlatform::Android,
                "https://play.google.com/store/apps/details?id=com.example.chalkak&hl=ko&gl=KR",
            ),
            (AppPlatform::Ios, "https://apps.apple.com/app/id1234567890"),
            (
                AppPlatform::Ios,
                "https://apps.apple.com/kr/app/chalkak/id1234567890?l=ko&mt=8",
            ),
            (
                AppPlatform::Android,
                "HTTPS://PLAY.GOOGLE.COM:443/store/apps/details?id=com.a.b",
            ),
            (AppPlatform::Ios, "https://APPS.APPLE.COM:443/app/id123"),
        ] {
            let url = StoreUrl::from_input(&format!("  {value}  ")).unwrap();
            url.validate_platform(platform).unwrap();
            assert_eq!(url.value(), value);
            assert_eq!(url.platform(), platform);
        }
    }

    #[test]
    fn 잘못된_스킴_호스트_권한정보_포트_경로_앱_id_쿼리_인코딩을_거절한다() {
        for value in [
            "",
            " ",
            "https:chalkak",
            "https://play.google.com",
            "http://apps.apple.com/app/id123",
            "https://evil.example/app/id123",
            "https://play.google.com.evil.example/store/apps/details?id=com.example.app",
            "https://user@play.google.com/store/apps/details?id=com.example.app",
            "https://apps.apple.com:444/app/id123",
            "https://play.google.com:/store/apps/details?id=com.a.b",
            "https://play.google.com:0443/store/apps/details?id=com.a.b",
            "https://apps.apple.com:0443/app/id123",
            "https://apps.apple.com/app/id123#update",
            "https://apps.apple.com/app/../id123",
            "https://play.google.com/store/apps/details",
            "https://play.google.com/store/apps/details?id=",
            "https://play.google.com/store/apps/details?id=bad",
            "https://play.google.com/store/apps/details?id=com.example.app&id=com.other.app",
            "https://play.google.com/store/apps/details?id=com.example.app&token=secret",
            "https://play.google.com/store/apps/details?id=com.example.app&hl=",
            "https://play.google.com/store/apps/details?id=com.example.app&hl",
            "https://apps.apple.com/app/id0",
            "https://apps.apple.com/app/idabc",
            "https://apps.apple.com/app/id123?mt=7",
            "https://apps.apple.com/app/id123?secret=abc",
            "https://apps.apple.com/app/%ZZ/id123",
            "https://apps.apple.com/app/%2e%2e/id123",
            "https://apps.apple.com/app/name%2fid123",
            "https://apps.apple.com/app/name%5c/id123",
        ] {
            assert_eq!(
                kind(StoreUrl::new(value)),
                "스토어 주소는 해당 플랫폼의 공식 HTTPS 앱 상세 주소여야 합니다.",
                "{value:?}"
            );
        }
    }

    #[test]
    fn 주소_길이_상한_직전과_상한을_허용하고_초과를_거절한다() {
        let prefix = "https://apps.apple.com/app/";
        for length in [1999, 2000] {
            let value = format!("{prefix}{}/id1", "a".repeat(length - prefix.len() - 4));
            assert_eq!(
                StoreUrl::new(&value).unwrap().value().chars().count(),
                length
            );
        }
        let over = format!("{prefix}{}/id1", "a".repeat(2000));
        assert!(StoreUrl::new(&over).is_err());
    }

    #[test]
    fn 주소가_플랫폼과_다르면_거절한다() {
        assert!(
            StoreUrl::new("https://apps.apple.com/app/id123")
                .unwrap()
                .validate_platform(AppPlatform::Android)
                .is_err()
        );
        assert!(
            StoreUrl::new("https://play.google.com/store/apps/details?id=com.example.app")
                .unwrap()
                .validate_platform(AppPlatform::Ios)
                .is_err()
        );
    }

    #[test]
    fn db_원문_생성자는_고정_경로_인코딩과_비정규_공백을_거절한다() {
        for value in [
            "https://play.google.com/store/apps/%64etails?id=com.a.b",
            "https://play.google.com/store/%61pps/details?id=com.a.b",
            "https://apps.apple.com/%61pp/id123",
            "https://apps.apple.com/app/%69d123",
            " https://apps.apple.com/app/id123",
            "https://apps.apple.com/app/id123\n",
            "\u{2000}https://apps.apple.com/app/id123\u{2000}",
        ] {
            assert!(StoreUrl::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn 요청_입력_팩토리는_유니코드_공백과_개행을_제거한다() {
        assert_eq!(
            StoreUrl::from_input("\u{2000}\thttps://apps.apple.com/app/id123\n")
                .unwrap()
                .value(),
            "https://apps.apple.com/app/id123"
        );
    }

    // java.net.URI와 같은 문자 집합·해석을 확인한 추가 경우(원본 oracle에서 확인한 동작)
    #[test]
    fn 쿼리_이름과_값의_퍼센트_인코딩은_풀어서_본다() {
        let url = "https://play.google.com/store/apps/details?%69d=com.a.b&hl=%6bo";
        assert!(StoreUrl::new(url).is_ok());
        assert!(
            StoreUrl::new("https://play.google.com/store/apps/details?id=com.a.b&hl=k+o").is_err()
        );
    }

    #[test]
    fn 경로와_쿼리에_쓸_수_없는_문자는_거절한다() {
        for value in [
            "https://apps.apple.com/app/na me/id123",
            "https://apps.apple.com/app/na<me>/id123",
            "https://apps.apple.com/app/na[me]/id123",
            "https://apps.apple.com/app/na\\me/id123",
            "https://apps.apple.com/app/id123?l=ko|en",
            "https://apps.apple.com/app/id123?l=k{o}",
            "https://apps.apple.com/app/id123?",
            "https://apps.apple.com/app/id123?l=ko&",
            "https://apps.apple.com/app/id123?l",
            "https://apps.apple.com//app/id123",
            "https://apps.apple.com/app//id123",
            "https://apps.apple.com/app/id123/",
            "https://apps.apple.com/app/%/id123",
            "https://apps.apple.com/app/%4/id123",
        ] {
            assert!(StoreUrl::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn 앱_이름_조각에는_비ascii_문자와_허용된_기호를_쓸_수_있다() {
        for value in [
            "https://apps.apple.com/kr/app/착칵-chalkak!/id123",
            "https://apps.apple.com/app/a;b,c=d&e+f@g:h$i/id123",
            "https://apps.apple.com/app/%EC%B0%A9%EC%B9%B5/id123?l=ko",
        ] {
            assert!(StoreUrl::new(value).is_ok(), "{value:?}");
        }
    }

    // ----- AppVersionTest -----

    #[test]
    fn 수정이_플랫폼과_다른_주소를_거절하면_기존_설정과_변경자를_유지한다() {
        let admin = Uuid::new_v4();
        let version = MinimumSupportedVersion::new("1.0.0").unwrap();
        let url = StoreUrl::new("https://apps.apple.com/app/id123").unwrap();
        let reason = ChangeReason::new("최초 입력").unwrap();
        let mut setting = AppVersion::create(
            AppPlatform::Ios,
            version.clone(),
            url.clone(),
            admin,
            &reason,
        )
        .unwrap();
        let other = StoreUrl::new("https://play.google.com/store/apps/details?id=com.a.b").unwrap();
        assert!(
            setting
                .update(
                    MinimumSupportedVersion::new("2.0.0").unwrap(),
                    other,
                    Uuid::new_v4(),
                    &ChangeReason::new("다른 사유").unwrap()
                )
                .is_err()
        );
        assert_eq!(setting.minimum_supported_version, version);
        assert_eq!(setting.store_url, url);
        assert_eq!(setting.last_changed_by, Some(admin));
        assert_eq!(setting.change_reason.as_deref(), Some("최초 입력"));
    }

    #[test]
    fn 플랫폼_이름은_정확히_같아야_하고_요청_값은_공백을_뗀다() {
        assert_eq!(AppPlatform::parse("ANDROID"), Some(AppPlatform::Android));
        assert_eq!(AppPlatform::parse("android"), None);
        assert_eq!(
            AppPlatform::from_request_text(" IOS "),
            Ok(Some(AppPlatform::Ios))
        );
        assert_eq!(AppPlatform::from_request_text(" "), Ok(None));
        assert_eq!(
            AppPlatform::from_request_text("WINDOWS"),
            Err(UnknownPlatform)
        );
    }
}
