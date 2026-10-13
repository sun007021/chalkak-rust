//! `Accept` 헤더 해석과 응답 미디어 타입 선택. Spring `MediaType`/`MimeTypeUtils`와
//! `AbstractMessageConverterMethodProcessor.writeWithMessageConverters`의 선택 규칙을 따른다.
//!
//! 규칙(java-a에 `Accept` 값을 바꿔 보내 확인한 것과 Spring 소스의 일치 여부):
//! - 요소는 `,`로 나누되 따옴표 안의 `,`는 나누지 않고, 빈 요소는 무시한다. 요소가 하나도 없으면 `*/*`다.
//! - 타입·서브타입은 소문자로 바꾼다. 매개변수 이름도 소문자다. `*` 하나는 `*/*`이고, `*/json`처럼 타입만 와일드카드면 오류다.
//! - `q`는 0 이상 1 이하의 수여야 하고, `charset` 값은 알려진 문자셋 이름이어야 한다(Java `Charset.forName`).
//! - 한 요소라도 해석에 실패하면 전체가 "해석 실패"다(요소가 50개를 넘어도 같다).
//! - 후보 만들기: 요청 타입마다 응답 가능 타입과 호환되는지 보고(품질값은 무시한다. `q=0`도 호환이다),
//!   호환되면 둘 중 더 구체적인 쪽을 후보로 둔다. 후보를 구체성 순으로 정렬해 처음 만나는 구체 타입(와일드카드 아님)을 고른다.

/// 해석된 미디어 타입.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaType {
    pub ty: String,
    pub sub: String,
    /// 이름은 소문자, 값은 따옴표를 벗긴 형태(`charset`만 벗긴다. 나머지는 입력 그대로).
    pub params: Vec<(String, String)>,
}

/// `Accept`를 해석하지 못했다(`InvalidMediaTypeException`). 응답은 406이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidAccept;

impl MediaType {
    pub fn new(ty: &str, sub: &str) -> MediaType {
        MediaType {
            ty: ty.to_owned(),
            sub: sub.to_owned(),
            params: Vec::new(),
        }
    }

    pub fn is_wildcard_type(&self) -> bool {
        self.ty == "*"
    }

    pub fn is_wildcard_subtype(&self) -> bool {
        self.sub == "*" || self.sub.starts_with("*+")
    }

    pub fn is_concrete(&self) -> bool {
        !self.is_wildcard_type() && !self.is_wildcard_subtype()
    }

    /// `+` 뒤 접미사(`vnd.x+json` -> `json`).
    fn suffix(&self) -> Option<&str> {
        self.sub.rfind('+').map(|i| &self.sub[i + 1..])
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// 매개변수 `q`(없으면 1.0).
    pub fn quality(&self) -> f64 {
        self.param("q").and_then(|v| v.parse().ok()).unwrap_or(1.0)
    }

    /// `MimeType.isCompatibleWith`.
    pub fn compatible_with(&self, other: &MediaType) -> bool {
        if self.is_wildcard_type() || other.is_wildcard_type() {
            return true;
        }
        if self.ty != other.ty {
            return false;
        }
        if self.sub == other.sub {
            return true;
        }
        if self.is_wildcard_subtype() || other.is_wildcard_subtype() {
            let (this_suffix, other_suffix) = (self.suffix(), other.suffix());
            if self.is_wildcard_subtype() && this_suffix.is_some() {
                let s = this_suffix.unwrap_or_default();
                return s == other.sub || Some(s) == other_suffix;
            }
            if other.is_wildcard_subtype() && other_suffix.is_some() {
                let s = other_suffix.unwrap_or_default();
                return self.sub == s || Some(s) == this_suffix;
            }
            return true;
        }
        false
    }

    /// `MimeType.isMoreSpecific`.
    fn more_specific_than(&self, other: &MediaType) -> bool {
        if self.is_wildcard_type() && !other.is_wildcard_type() {
            return false;
        }
        if other.is_wildcard_type() && !self.is_wildcard_type() {
            return true;
        }
        if self.ty != other.ty {
            return false;
        }
        if self.is_wildcard_subtype() && !other.is_wildcard_subtype() {
            return false;
        }
        if other.is_wildcard_subtype() && !self.is_wildcard_subtype() {
            return true;
        }
        if self.sub != other.sub {
            return false;
        }
        self.params.len() > other.params.len()
    }

    /// `MimeType.SPECIFICITY_COMPARATOR.compare(self, other) <= 0` : self가 other보다 덜 구체적이지 않다.
    fn at_least_as_specific(&self, other: &MediaType) -> bool {
        if self.is_wildcard_type() && !other.is_wildcard_type() {
            return false;
        }
        if other.is_wildcard_type() && !self.is_wildcard_type() {
            return true;
        }
        if self.ty != other.ty {
            return true;
        }
        if self.is_wildcard_subtype() && !other.is_wildcard_subtype() {
            return false;
        }
        if other.is_wildcard_subtype() && !self.is_wildcard_subtype() {
            return true;
        }
        if self.sub != other.sub {
            return true;
        }
        // 매개변수가 많은 쪽이 더 구체적이고, 같으면 비교 결과가 0이다.
        self.params.len() >= other.params.len()
    }

    pub fn without_quality(&self) -> MediaType {
        MediaType {
            ty: self.ty.clone(),
            sub: self.sub.clone(),
            params: self
                .params
                .iter()
                .filter(|(n, _)| n != "q")
                .cloned()
                .collect(),
        }
    }

    /// `Content-Type` 헤더 값(`MimeType.toString`).
    pub fn render(&self) -> String {
        let mut out = format!("{}/{}", self.ty, self.sub);
        for (n, v) in &self.params {
            out.push(';');
            out.push_str(n);
            out.push('=');
            out.push_str(v);
        }
        out
    }

    /// 타입과 서브타입이 같은지(매개변수는 보지 않는다).
    pub fn is(&self, ty: &str, sub: &str) -> bool {
        self.ty == ty && self.sub == sub
    }

    /// `MimeTypeUtils.parseMimeType` + `MediaType` 검증.
    pub fn parse(raw: &str) -> Result<MediaType, InvalidAccept> {
        let (full, rest) = match raw.find(';') {
            Some(i) => (&raw[..i], Some(&raw[i + 1..])),
            None => (raw, None),
        };
        let mut full = full.trim();
        if full.is_empty() {
            return Err(InvalidAccept);
        }
        if full == "*" {
            full = "*/*";
        }
        let slash = full.find('/').ok_or(InvalidAccept)?;
        if slash == full.len() - 1 {
            return Err(InvalidAccept);
        }
        let (ty, sub) = (&full[..slash], &full[slash + 1..]);
        if ty == "*" && sub != "*" {
            return Err(InvalidAccept);
        }
        check_token(ty)?;
        check_token(sub)?;
        let mut params: Vec<(String, String)> = Vec::new();
        if let Some(rest) = rest {
            for piece in split_params(rest) {
                let piece = piece.trim();
                if piece.is_empty() {
                    continue;
                }
                let Some(eq) = piece.find('=') else { continue };
                let name = piece[..eq].trim();
                let value = piece[eq + 1..].trim();
                check_token(name)?;
                let name = name.to_ascii_lowercase();
                let value = if name == "charset" {
                    let v = unquote(value);
                    if !known_charset(v) {
                        return Err(InvalidAccept);
                    }
                    v.to_owned()
                } else {
                    if !is_quoted(value) {
                        check_token(value)?;
                    }
                    value.to_owned()
                };
                // 같은 이름은 마지막 값이 이긴다(LinkedHashMap.put)
                if let Some(slot) = params.iter_mut().find(|(n, _)| *n == name) {
                    slot.1 = value;
                } else {
                    params.push((name, value));
                }
            }
        }
        let media = MediaType {
            ty: ty.to_ascii_lowercase(),
            sub: sub.to_ascii_lowercase(),
            params,
        };
        if let Some(q) = media.param("q") {
            let q: f64 = unquote(q).parse().map_err(|_| InvalidAccept)?;
            if !(0.0..=1.0).contains(&q) {
                return Err(InvalidAccept);
            }
        }
        Ok(media)
    }
}

fn is_quoted(v: &str) -> bool {
    v.len() >= 2 && v.starts_with('"') && v.ends_with('"')
}

fn unquote(v: &str) -> &str {
    if is_quoted(v) { &v[1..v.len() - 1] } else { v }
}

/// RFC 2616 토큰: 제어 문자·공백·구분자(`()<>@,;:\"/[]?={}`)를 뺀 ASCII.
fn check_token(s: &str) -> Result<(), InvalidAccept> {
    if s.is_empty() {
        return Err(InvalidAccept);
    }
    let ok = s.bytes().all(|b| {
        (0x21..0x7f).contains(&b) && !b"()<>@,;:\\\"/[]?={}".contains(&b)
    });
    if ok { Ok(()) } else { Err(InvalidAccept) }
}

/// 따옴표 밖의 `;`로 매개변수를 나눈다.
fn split_params(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// `MediaType.parseMediaTypes`: 따옴표 밖의 `,`로 나누고 빈 요소는 건너뛴다.
pub fn parse_accept(values: &[&str]) -> Result<Vec<MediaType>, InvalidAccept> {
    let mut out = Vec::new();
    for value in values {
        let mut quoted = false;
        let mut start = 0;
        let mut pieces = Vec::new();
        for (i, c) in value.char_indices() {
            match c {
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    pieces.push(&value[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        pieces.push(&value[start..]);
        for piece in pieces {
            if piece.trim().is_empty() {
                continue;
            }
            out.push(MediaType::parse(piece)?);
        }
    }
    if out.len() > 50 {
        return Err(InvalidAccept);
    }
    sort_by_specificity(&mut out);
    Ok(out)
}

/// `MimeTypeUtils.sortBySpecificity`(버블 정렬: 같은 구체성은 입력 순서를 지킨다).
pub fn sort_by_specificity(list: &mut [MediaType]) {
    let len = list.len();
    for i in 0..len {
        for j in 1..len.saturating_sub(i) {
            // prev가 cur보다 덜 구체적이면(= cur가 더 구체적이면) 자리를 바꾼다.
            if list[j].more_specific_than(&list[j - 1]) {
                list.swap(j - 1, j);
            }
        }
    }
}

/// 응답 가능한 타입 목록에서 하나를 고른다. 없으면 `None`(406).
/// `accept`가 비어 있으면 `*/*`로 본다.
pub fn select(accept: &[MediaType], producible: &[MediaType]) -> Option<MediaType> {
    let all = [MediaType::new("*", "*")];
    let requested: &[MediaType] = if accept.is_empty() { &all } else { accept };
    let mut candidates: Vec<MediaType> = Vec::new();
    for want in requested {
        for have in producible {
            if want.compatible_with(have) {
                candidates.push(most_specific(want, have));
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    sort_by_specificity(&mut candidates);
    candidates
        .iter()
        .find(|m| m.is_concrete())
        .map(MediaType::without_quality)
}

/// `getMostSpecificMediaType(acceptable, producible)`: 응답 가능 타입에 요청의 품질값을 옮겨 비교한다.
fn most_specific(accept: &MediaType, produce: &MediaType) -> MediaType {
    let mut produce = produce.clone();
    produce.params.retain(|(n, _)| n != "q");
    if let Some(q) = accept.param("q") {
        produce.params.push(("q".to_owned(), q.to_owned()));
    }
    if accept.at_least_as_specific(&produce) {
        accept.clone()
    } else {
        produce
    }
}

/// `Charset.forName`이 받는 이름 가운데 흔한 것. 이 밖의 이름은 해석 실패로 본다.
fn known_charset(name: &str) -> bool {
    const KNOWN: &[&str] = &[
        "utf-8",
        "utf8",
        "us-ascii",
        "ascii",
        "iso-8859-1",
        "iso8859_1",
        "latin1",
        "iso-8859-2",
        "iso-8859-15",
        "utf-16",
        "utf-16be",
        "utf-16le",
        "utf-32",
        "utf-32be",
        "utf-32le",
        "windows-1250",
        "windows-1251",
        "windows-1252",
        "euc-kr",
        "ms949",
        "x-windows-949",
        "euc-jp",
        "shift_jis",
        "windows-31j",
        "gbk",
        "gb2312",
        "gb18030",
        "big5",
        "iso-2022-kr",
        "iso-2022-jp",
    ];
    let lower = name.to_ascii_lowercase();
    KNOWN.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_producible() -> Vec<MediaType> {
        vec![
            MediaType::new("application", "json"),
            MediaType::new("application", "*+json"),
            MediaType::new("application", "yaml"),
        ]
    }

    /// 헤더 값 하나로 고른 `Content-Type`. 해석 실패는 `Err`, 406은 `Ok(None)`.
    fn pick(accept: &str) -> Result<Option<String>, InvalidAccept> {
        let list = if accept.is_empty() {
            Vec::new()
        } else {
            parse_accept(&[accept])?
        };
        Ok(select(&list, &json_producible()).map(|m| m.render()))
    }

    // 아래 기대값은 모두 java-a(/api/v1/notifications)에 같은 Accept를 보낸 응답이다.
    #[test]
    fn 자바와_같은_타입을_고른다() {
        let json = Ok(Some("application/json".to_owned()));
        for accept in [
            "",
            "*/*",
            "*",
            "application/*",
            "application/json",
            "application/json;q=0.9",
            "application/json; q=0.5",
            "application/*+json",
            "application/*+json;charset=UTF-8",
            "text/html,application/json;q=0.5",
            "application/json, text/html",
            "text/html, application/json",
            "*/*;q=0.5, text/html",
            "application/*;q=0.1",
            "text/html;q=0.1,*/*;q=0.8",
            "*/*;q=0",
            "application/json;q=0",
            "application/json,",
            ",",
            "APPLICATION/JSON",
        ] {
            assert_eq!(pick(accept), json, "{accept}");
        }
        assert_eq!(
            pick("application/json;charset=UTF-8"),
            Ok(Some("application/json;charset=UTF-8".into()))
        );
        assert_eq!(
            pick("application/JSON;Charset=UTF-8"),
            Ok(Some("application/json;charset=UTF-8".into()))
        );
        assert_eq!(
            pick("application/json;charset=\"utf-8\""),
            Ok(Some("application/json;charset=utf-8".into()))
        );
        assert_eq!(
            pick("application/json;foo=bar"),
            Ok(Some("application/json;foo=bar".into()))
        );
        assert_eq!(
            pick("application/vnd.x+json"),
            Ok(Some("application/vnd.x+json".into()))
        );
        assert_eq!(
            pick("application/problem+json"),
            Ok(Some("application/problem+json".into()))
        );
        assert_eq!(
            pick("application/yaml"),
            Ok(Some("application/yaml".into()))
        );
        assert_eq!(
            pick("application/yaml, text/html"),
            Ok(Some("application/yaml".into()))
        );
        assert_eq!(
            pick("application/yaml;q=0.1, text/html"),
            Ok(Some("application/yaml".into()))
        );
    }

    #[test]
    fn 응답할_수_없는_타입은_406이다() {
        for accept in [
            "text/html",
            "text/*",
            "text/html;q=0",
            "text/html, application/xml",
            "application/xml",
            "application/vnd.x+xml",
            "text/plain",
            "application/octet-stream",
            "application/x-yaml",
            "text/yaml",
            "application/x-www-form-urlencoded",
            "image/png",
            "application/jsonx",
        ] {
            assert_eq!(pick(accept), Ok(None), "{accept}");
        }
    }

    #[test]
    fn 해석할_수_없는_값은_실패다() {
        for accept in [
            "bad",
            "application/",
            "json",
            "application/json;q=2",
            "application/json;q=abc",
            "application/json;charset=bad",
            "\"",
            "*/json",
            "text/html, bad",
        ] {
            assert_eq!(pick(accept), Err(InvalidAccept), "{accept}");
        }
    }

    #[test]
    fn 요소가_쉰_개를_넘으면_실패다() {
        let many = vec!["text/html"; 51].join(",");
        assert_eq!(parse_accept(&[&many]), Err(InvalidAccept));
        let fifty = vec!["text/html"; 50].join(",");
        assert!(parse_accept(&[&fifty]).is_ok());
    }

    #[test]
    fn 호환_규칙() {
        let t = |s: &str| MediaType::parse(s).unwrap();
        assert!(t("application/*+json").compatible_with(&t("application/json")));
        assert!(t("application/vnd.x+json").compatible_with(&t("application/*+json")));
        assert!(!t("application/vnd.x+xml").compatible_with(&t("application/*+json")));
        assert!(!t("application/yaml").compatible_with(&t("application/*+json")));
        assert!(t("*/*").compatible_with(&t("text/html")));
        assert!(!t("text/html").compatible_with(&t("application/json")));
    }
}
