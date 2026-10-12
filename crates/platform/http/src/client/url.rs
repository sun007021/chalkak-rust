//! 최소 URL 해석(`http://`, `https://`만). 리다이렉트는 따르지 않으므로 상대 URL 해석은 없다.

use super::error::ClientError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    pub https: bool,
    /// IPv6는 대괄호를 뗀 형태.
    pub host: String,
    pub port: u16,
    /// 항상 `/`로 시작. 쿼리 포함, 프래그먼트 제외.
    pub path_and_query: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Self, ClientError> {
        let bad = |m: &str| ClientError::InvalidUrl(format!("{m}: {s}"));
        let (scheme, rest) = s.split_once("://").ok_or_else(|| bad("스킴이 없다"))?;
        let https = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            other => return Err(ClientError::UnsupportedScheme(other.to_string())),
        };
        let rest = rest.split('#').next().unwrap_or("");
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(bad("userinfo는 지원하지 않는다"));
        }
        let (host, port_str) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6
                .split_once(']')
                .ok_or_else(|| bad("IPv6 대괄호가 닫히지 않았다"))?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(|| bad("포트 형식 오류"))?),
            };
            (h, port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(bad("호스트가 없다"));
        }
        if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad("호스트에 허용되지 않는 문자"));
        }
        let port = match port_str {
            None | Some("") => {
                if https {
                    443
                } else {
                    80
                }
            }
            Some(p) => p.parse::<u16>().map_err(|_| bad("포트 형식 오류"))?,
        };
        let path_and_query = if path.is_empty() {
            "/".to_string()
        } else if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_string()
        };
        if path_and_query
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(bad("경로에 공백·제어 문자가 있다"));
        }
        Ok(Self {
            https,
            host: host.to_string(),
            port,
            path_and_query,
        })
    }

    /// `Host` 헤더 값. 기본 포트면 포트를 생략한다.
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_기본_포트와_쿼리() {
        let u = Url::parse("https://appleid.apple.com/auth/token?a=b#frag").unwrap();
        assert!(u.https);
        assert_eq!((u.host.as_str(), u.port), ("appleid.apple.com", 443));
        assert_eq!(u.path_and_query, "/auth/token?a=b");
        assert_eq!(u.host_header(), "appleid.apple.com");
    }

    #[test]
    fn http_명시_포트와_빈_경로() {
        let u = Url::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!((u.port, u.path_and_query.as_str()), (8080, "/"));
        assert_eq!(u.host_header(), "127.0.0.1:8080");
        assert_eq!(Url::parse("http://h?x=1").unwrap().path_and_query, "/?x=1");
    }

    #[test]
    fn ipv6_대괄호() {
        let u = Url::parse("http://[::1]:9000/x").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 9000));
        assert_eq!(u.host_header(), "[::1]:9000");
    }

    #[test]
    fn 잘못된_url은_거부한다() {
        assert!(matches!(
            Url::parse("ftp://h/"),
            Err(ClientError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            Url::parse("example.com"),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            Url::parse("http://:80/"),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            Url::parse("http://h:99999/"),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            Url::parse("http://u@h/"),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            Url::parse("http://h/a b"),
            Err(ClientError::InvalidUrl(_))
        ));
        assert!(matches!(
            Url::parse("http://h/a\r\nX: y"),
            Err(ClientError::InvalidUrl(_))
        ));
    }
}
