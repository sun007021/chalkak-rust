//! `new URI(text).parseServerAuthority()`를 통과하고 `https` 스킴·호스트가 있는 주소인지 판단한다(원본 `Banner.normalizeUrl`).
//!
//! `java.net.URI`(RFC 2396 + IPv6 확장)의 문법만 옮겼다. 통과 여부만 필요하므로 구성 요소는 저장하지 않는다.
//! 포트는 `int` 범위에서 읽고 65535를 넘으면 거절한다. 호스트는 IPv4, 호스트 이름(마지막 라벨은 영문자로 시작), 대괄호 IPv6다.

use std::net::Ipv6Addr;

const MAX_PORT: i64 = 65_535;

/// 원본 `Banner.normalizeUrl`의 URI 검사 부분. 어떤 문법 오류든 같은 결과(거절)다.
pub fn is_https_server_uri(text: &str) -> bool {
    parse(text).is_some()
}

fn parse(text: &str) -> Option<()> {
    // fragment는 첫 '#' 뒤다. 그 안의 두 번째 '#'은 허용하지 않는다.
    let (before_fragment, fragment) = match text.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (text, None),
    };
    if let Some(fragment) = fragment
        && !fragment.chars().all(is_uric_or_other_or_escape_checked)
    {
        return None;
    }
    if fragment.is_some_and(|f| !valid_escapes(f)) {
        return None;
    }

    let (scheme, rest) = before_fragment.split_once(':')?;
    if !is_scheme(scheme) || !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    // 계층형 URI만 호스트를 가진다. 불투명(opaque) 형태는 호스트가 없다.
    let rest = rest.strip_prefix("//")?;

    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path_and_query) = rest.split_at(authority_end);
    if authority.is_empty() {
        return None;
    }
    if !valid_server_authority(authority) {
        return None;
    }

    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    if !path.chars().all(is_path_char) || !valid_escapes(path) {
        return None;
    }
    if let Some(query) = query
        && (!query.chars().all(is_uric_or_other_or_escape_checked) || !valid_escapes(query))
    {
        return None;
    }
    Some(())
}

fn is_scheme(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_unreserved(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
}

/// `java.net.URI`의 `other`: ASCII 밖의 공백·제어 문자가 아닌 문자.
fn is_other(c: char) -> bool {
    (c as u32) > 127 && !c.is_whitespace() && !matches!(c as u32, 0x80..=0x9f)
}

/// `%`는 뒤의 두 16진수까지 [`valid_escapes`]가 따로 검사하므로 여기서는 문자 자격만 본다.
fn is_uric_or_other_or_escape_checked(c: char) -> bool {
    is_unreserved(c)
        || matches!(
            c,
            ';' | '/' | '?' | ':' | '@' | '&' | '=' | '+' | '$' | ',' | '[' | ']' | '%'
        )
        || is_other(c)
}

fn is_path_char(c: char) -> bool {
    is_unreserved(c)
        || matches!(c, ':' | '@' | '&' | '=' | '+' | '$' | ',' | ';' | '/' | '%')
        || is_other(c)
}

/// 모든 `%` 뒤에 16진수 두 자리가 있어야 한다.
fn valid_escapes(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return false;
            }
            if !(bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit()) {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// `[userinfo@]host[:port]`
fn valid_server_authority(authority: &str) -> bool {
    let host_port = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            if !userinfo.chars().all(|c| {
                is_unreserved(c)
                    || matches!(c, ';' | ':' | '&' | '=' | '+' | '$' | ',' | '%')
                    || is_other(c)
            }) || !valid_escapes(userinfo)
            {
                return false;
            }
            host_port
        }
        None => authority,
    };
    let port = if let Some(rest) = host_port.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return false;
        };
        let (inner, after) = rest.split_at(close);
        if inner.parse::<Ipv6Addr>().is_err() {
            return false;
        }
        &after[1..]
    } else {
        let host_end = host_port.find(':').unwrap_or(host_port.len());
        let (host, after) = host_port.split_at(host_end);
        if !(is_ipv4(host) || is_hostname(host)) {
            return false;
        }
        after
    };
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some("") => true,
        Some(digits) => {
            digits.bytes().all(|b| b.is_ascii_digit())
                && digits
                    .parse::<i32>()
                    .is_ok_and(|p| i64::from(p) <= MAX_PORT)
        }
    }
}

/// `a.b.c.d`(각 0~255, 앞자리 0 허용 안 함은 없다. `java.net.URI`는 자릿수 1~3만 본다).
fn is_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            (1..=3).contains(&p.len())
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// `domainlabel *("." domainlabel) ["."]` 이고 마지막 라벨은 영문자로 시작한다.
fn is_hostname(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() {
        return false;
    }
    let labels: Vec<&str> = trimmed.split('.').collect();
    let well_formed = labels.iter().all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes[0].is_ascii_alphanumeric()
            && bytes[bytes.len() - 1].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    });
    if !well_formed {
        return false;
    }
    // 완전한 호스트 이름(라벨이 둘 이상)이면 가장 오른쪽 라벨은 영문자로 시작해야 한다. 한 라벨이면 숫자로 시작해도 된다.
    labels.len() == 1 || labels[labels.len() - 1].as_bytes()[0].is_ascii_alphabetic()
}

#[cfg(test)]
mod tests {
    use super::*;

    // BannerTest#create_validHttpsUrl_preservesInput 외 java.net.URI 규칙
    #[test]
    fn 호스트가_있는_https_절대_주소를_허용한다() {
        for url in [
            "https://example.com",
            "https://example.com/event",
            "https://other.example.org/event?id=1",
            "HTTPS://EXAMPLE.COM/event",
            "https://example.com/안내#camera",
            "https://example.com:65535/event",
            "https://example.com:/event",
            "https://user:pw@example.com/event",
            "https://127.0.0.1:8080/x",
            "https://[::1]:8443/x",
            "https://localhost/x",
            "https://example.com./x",
            "https://example.com/a?b=[c]&d=%E3%81%82#frag[1]",
            "https://example.com/%ED%95%9C",
        ] {
            assert!(is_https_server_uri(url), "{url}");
        }
    }

    // BannerTest#create_invalidUrl_throwsBusinessException
    #[test]
    fn 비https_상대_주소_호스트_누락_문법_오류는_거절한다() {
        for url in [
            "http://example.com",
            "/event",
            "//example.com/event",
            "https:///event",
            "javascript:alert(1)",
            "https://",
            "https://exa mple.com",
            "https://example.com/%xx",
            "https://example.com/%4",
            "https://example.com:invalid/event",
            "https://example.com\\event",
            "https://example.com:65536/event",
            "https://example.com:2147483648/event",
            "https://exa_mple.com/x",
            "https://example.com/a b",
            "https://example.com/[x]",
            "https://example.com/a#b#c",
            "https://-example.com/x",
            "https://example-.com/x",
            "https://example.1com/x",
            "https://999.1.1.1/x",
            "https:example.com",
            "https://[::1/x",
            "https://[zz]/x",
            "https://example.com/\u{85}",
            "https://example.com/a\u{3000}b",
            "",
        ] {
            assert!(!is_https_server_uri(url), "{url:?}");
        }
    }
}
