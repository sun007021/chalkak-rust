//! HTTP 메서드.

use std::fmt;

/// 요청 메서드. 알려지지 않은 토큰은 [`Method::Other`]로 보존한다(Spring은 이를 405로 처리한다).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Patch,
    Options,
    Trace,
    Connect,
    Other(String),
}

impl Method {
    /// 메서드 토큰을 해석한다. RFC 9110 `token`이 아니면 `None`.
    pub fn parse(token: &str) -> Option<Method> {
        if token.is_empty() || !token.bytes().all(is_token_byte) {
            return None;
        }
        Some(match token {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            "OPTIONS" => Method::Options,
            "TRACE" => Method::Trace,
            "CONNECT" => Method::Connect,
            other => Method::Other(other.to_string()),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
            Method::Connect => "CONNECT",
            Method::Other(s) => s,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// RFC 9110 `tchar`.
pub(crate) fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_tokens() {
        assert_eq!(Method::parse("GET"), Some(Method::Get));
        assert_eq!(Method::parse("FOO"), Some(Method::Other("FOO".into())));
        assert_eq!(Method::parse("get"), Some(Method::Other("get".into())));
        assert_eq!(Method::parse(""), None);
        assert_eq!(Method::parse("GE T"), None);
        assert_eq!(Method::parse("GE\u{0}T"), None);
        assert_eq!(Method::Patch.to_string(), "PATCH");
    }
}
