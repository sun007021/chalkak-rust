//! 퍼센트 디코딩과 쿼리 문자열 파싱.
//!
//! 쿼리는 Spring/Tomcat 규칙을 따른다: `+`는 공백, 바이트열은 UTF-8로 해석하고(잘못된 바이트는 U+FFFD),
//! 잘못된 `%xx`가 든 파라미터는 버리고 쿼리가 망가졌다고 표시한다. Tomcat은 그런 요청에서 파라미터를 읽는 순간
//! `IllegalStateException`을 던진다(파라미터를 읽지 않는 경로는 영향이 없다).
//! 경로는 `+`를 그대로 두고, 인코딩된 `/`·`\`·NUL은 거절한다(Tomcat 기본값).

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 엄격한 퍼센트 디코딩. `%` 뒤가 16진수 두 자리가 아니면 `None`.
/// `reject_special`이 참이면 `%2F`·`%5C`·`%00`도 `None`.
fn decode(s: &str, plus_as_space: bool, reject_special: bool) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hi = hex_val(*b.get(i + 1)?)?;
                let lo = hex_val(*b.get(i + 2)?)?;
                let v = hi << 4 | lo;
                if reject_special && matches!(v, b'/' | b'\\' | 0) {
                    return None;
                }
                out.push(v);
                i += 3;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// 쿼리 이름/값 하나를 디코딩한다(`+`는 공백). 잘못된 이스케이프면 `None`.
pub fn decode_query_component(s: &str) -> Option<String> {
    decode(s, true, false).map(|v| String::from_utf8_lossy(&v).into_owned())
}

/// 경로를 디코딩한다. 잘못된 이스케이프·잘못된 UTF-8·인코딩된 `/`·`\`·NUL이면 `None`.
pub fn decode_path(s: &str) -> Option<String> {
    String::from_utf8(decode(s, false, true)?).ok()
}

/// `a=1&b=%EA%B0%80&c` → `[("a","1"),("b","가"),("c","")]`.
/// 빈 조각(`&&`)과 이름이 빈 조각은 건너뛴다. 같은 이름은 등장 순서대로 모두 남는다.
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    parse_query_checked(raw).0
}

/// [`parse_query`]와 같고, 잘못된 퍼센트 인코딩이 있었는지도 돌려준다.
pub fn parse_query_checked(raw: &str) -> (Vec<(String, String)>, bool) {
    let mut out = Vec::new();
    let mut malformed = false;
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (n, v) = pair.split_once('=').unwrap_or((pair, ""));
        // 이름이 빈 조각도 디코딩은 시도한다: Tomcat은 `=%zz`에서도 실패한다.
        let (name, value) = match (decode(n, true, false), decode(v, true, false)) {
            (Some(n), Some(v)) => (n, v),
            _ => {
                malformed = true;
                continue;
            }
        };
        // UTF-8로 읽을 수 없는 바이트열도 Tomcat은 해석 실패로 본다. 값은 대체 문자로 남긴다.
        if std::str::from_utf8(&name).is_err() || std::str::from_utf8(&value).is_err() {
            malformed = true;
        }
        if name.is_empty() {
            continue;
        }
        out.push((
            String::from_utf8_lossy(&name).into_owned(),
            String::from_utf8_lossy(&value).into_owned(),
        ));
    }
    (out, malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Vec<(String, String)> {
        parse_query(s)
    }
    fn p(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn plus_is_space_in_query_only() {
        assert_eq!(q("a=x+y%20z"), [p("a", "x y z")]);
        assert_eq!(q("a%2Bb=1%2B1"), [p("a+b", "1+1")]);
        assert_eq!(decode_path("/a+b").as_deref(), Some("/a+b"));
    }

    #[test]
    fn utf8_and_repeated_names() {
        assert_eq!(q("t=%EA%B0%80&t=%EB%82%98"), [p("t", "가"), p("t", "나")]);
        // 잘못된 UTF-8 바이트는 U+FFFD
        assert_eq!(q("t=%FF"), [p("t", "\u{fffd}")]);
    }

    #[test]
    fn valueless_empty_and_nameless_pairs() {
        assert_eq!(q("a&b=&=c&&d=1"), [p("a", ""), p("b", ""), p("d", "1")]);
        assert_eq!(q(""), []);
        assert_eq!(q("a=b=c"), [p("a", "b=c")]);
    }

    #[test]
    fn invalid_escape_drops_only_that_pair() {
        assert_eq!(q("a=%zz&b=2&c=%4&d=%"), [p("b", "2")]);
    }

    #[test]
    fn invalid_escape_marks_the_query_malformed() {
        assert!(!parse_query_checked("a=1&b=%EA%B0%80&&c").1);
        for raw in ["a=%zz", "%zz=1", "a=%4", "a=%", "ok=1&a=%G0", "%"] {
            assert!(parse_query_checked(raw).1, "{raw}");
        }
        // 이름이 빈 조각도 디코딩에 실패하면 망가진 것으로 본다(java-3: `?=%zz` -> 500).
        assert!(parse_query_checked("=%zz").1);
        assert!(!parse_query_checked("=ok").1);
        // UTF-8이 아닌 바이트열(`%FF`, 잘린 다바이트 문자)도 마찬가지다.
        assert!(parse_query_checked("t=%FF").1);
        assert!(parse_query_checked("t=%e4%b8").1);
        assert!(!parse_query_checked("t=%EA%B0%80").1);
    }

    #[test]
    fn path_rejects_encoded_separators_and_nul() {
        assert_eq!(decode_path("/a%2Fb"), None);
        assert_eq!(decode_path("/a%2fb"), None);
        assert_eq!(decode_path("/a%5Cb"), None);
        assert_eq!(decode_path("/a%00b"), None);
        assert_eq!(decode_path("/a%zz"), None);
        assert_eq!(decode_path("/a%FF"), None);
        assert_eq!(decode_path("/%EA%B0%80").as_deref(), Some("/가"));
    }
}
