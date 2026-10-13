//! `ForwardedHeaderFilter`(`server.forward-headers-strategy: framework`, dev 프로필만).
//!
//! 프록시가 붙인 `Forwarded`·`X-Forwarded-*` 헤더로 요청의 스킴·호스트를 다시 정한다. 이 서버에서 눈에 보이는 영향은 둘이다.
//! - 스킴이 `https`(또는 `wss`)면 보안 요청으로 보고 Spring Security가 `Strict-Transport-Security`를 쓴다.
//! - 호스트·포트가 잘못되면 필터가 보안 필터보다 앞에서 본문 없는 400으로 끊는다.
//!
//! 규칙은 dev 프로필로 띄운 원본(Tomcat 위의 Spring Framework 6)에 헤더를 바꿔 보내 얻은 값이다.
//! - `Forwarded`가 비어 있지 않으면 그 헤더만 보고 `X-Forwarded-*`는 무시한다. 여러 줄이면 첫 줄.
//!   `proto`·`host`는 정규식 `(?i:key)="?([^;,"]+)"?`의 첫 일치다(`proto=http, proto=https`면 http).
//! - 아니면 `X-Forwarded-Proto`(첫 줄의 첫 토큰), 없을 때만 `X-Forwarded-Ssl: on`(대소문자 무시), `X-Forwarded-Host`,
//!   `X-Forwarded-Port`를 본다.
//! - 스킴 비교는 소문자 `https`·`wss`만 보안이다(`HTTPS`는 아니다).
//! - 포트가 정수가 아니거나(`abc`) 호스트의 `:`가 문법에 맞지 않으면(`[::1`, `a:b:c`) 400이다. 범위는 보지 않는다.
//!
//! 구현하지 않은 것: `X-Forwarded-Prefix`(경로 접두사를 바꿔 방화벽·라우팅에 영향. 원본은 `../x`면 500, `//`면 400).
//! prod 프로필에는 이 필터가 없고(`application-prod.yml`에 `forward-headers-strategy` 없음) local도 없다.

use chalkak_http::{Handler, Middleware, Request, Response};

/// 보안 요청에 Spring Security가 쓰는 HSTS 값(`max-age=31536000 ; includeSubDomains`, 공백 위치까지 원본 그대로).
pub const HSTS: &str = "max-age=31536000 ; includeSubDomains";

/// 헤더 해석 결과.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// 필터가 정한 스킴. 헤더가 없으면 `None`(요청 자체의 `http`).
    Scheme(Option<String>),
    /// 호스트·포트가 잘못되어 필터가 400으로 끊는다.
    Invalid,
}

impl Resolved {
    /// 보안 요청인가(`ForwardedHeaderExtractingRequest.isSecure`).
    pub fn secure(&self) -> bool {
        matches!(self, Resolved::Scheme(Some(s)) if s == "https" || s == "wss")
    }
}

/// 요청 헤더에서 스킴을 정하고 호스트·포트를 검사한다.
pub fn resolve(req: &Request) -> Resolved {
    let forwarded = req
        .headers
        .get_all("forwarded")
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    match forwarded {
        Some(value) => {
            if let Some(host) = pattern_value(value, "host=")
                && !valid_host(&host)
            {
                return Resolved::Invalid;
            }
            Resolved::Scheme(pattern_value(value, "proto="))
        }
        None => from_x_headers(req),
    }
}

fn from_x_headers(req: &Request) -> Resolved {
    let first_token = |name: &str| -> Option<String> {
        let value = req.headers.get_all(name).next()?;
        let token = value.split(',').next().unwrap_or("").trim();
        (!token.is_empty()).then(|| token.to_owned())
    };
    if let Some(host) = first_token("x-forwarded-host")
        && !valid_host(&host)
    {
        return Resolved::Invalid;
    }
    if let Some(port) = first_token("x-forwarded-port")
        && port.parse::<i32>().is_err()
    {
        return Resolved::Invalid;
    }
    let scheme = first_token("x-forwarded-proto").or_else(|| {
        first_token("x-forwarded-ssl")
            .filter(|v| v.eq_ignore_ascii_case("on"))
            .map(|_| "https".to_owned())
    });
    Resolved::Scheme(scheme)
}

/// `UriComponentsBuilder.adaptForwardedHost`의 검사: 마지막 `:`가 `]` 뒤에 있으면 그 뒤는 정수 포트여야 하고,
/// 대괄호가 없는데 `:`가 둘 이상이면 잘못된 주소다.
fn valid_host(host: &str) -> bool {
    let Some(colon) = host.rfind(':') else {
        return true;
    };
    let bracket = host.rfind(']');
    if bracket.is_some_and(|b| b > colon) {
        return true;
    }
    if bracket.is_none() && host.find(':') != Some(colon) {
        return false;
    }
    host[colon + 1..].parse::<i32>().is_ok()
}

/// 정규식 `(?i:key)="?([^;,"]+)"?`의 첫 일치에서 값(앞뒤 공백 제거)을 꺼낸다. `key`는 소문자 `이름=`.
fn pattern_value(header: &str, key: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(key) {
        let start = from + found + key.len();
        let rest = &header[start..];
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        let end = rest.find([';', ',', '"']).unwrap_or(rest.len());
        if end > 0 {
            return Some(rest[..end].trim().to_owned());
        }
        from = start;
    }
    None
}

/// 호스트·포트가 잘못된 요청을 끊는다. 응답은 본문 없는 400이고 연결을 닫는다.
/// `enabled`가 거짓이면(dev가 아닌 프로필) 아무것도 하지 않는다.
pub struct ForwardedHeaders {
    enabled: bool,
}

impl ForwardedHeaders {
    pub fn new(enabled: bool) -> Self {
        ForwardedHeaders { enabled }
    }
}

impl Middleware for ForwardedHeaders {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        if self.enabled && resolve(&req) == Resolved::Invalid {
            return Response::new(400).with_header("Connection", "close");
        }
        next.handle(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_http::Method;

    fn with(headers: &[(&str, &str)]) -> Resolved {
        let mut req = Request::new(Method::Get, "/actuator/health");
        for (n, v) in headers {
            req.headers.append(*n, *v);
        }
        resolve(&req)
    }

    fn secure(headers: &[(&str, &str)]) -> bool {
        with(headers).secure()
    }

    // 아래 기대값은 dev 프로필 원본에 헤더를 바꿔 보낸 결과다(HSTS 유무와 400).
    #[test]
    fn forwarded_헤더가_있으면_x_forwarded는_무시한다() {
        assert!(!secure(&[
            ("Forwarded", "proto=http"),
            ("X-Forwarded-Proto", "https")
        ]));
        assert!(secure(&[
            ("Forwarded", "proto=https"),
            ("X-Forwarded-Proto", "http")
        ]));
        assert!(!secure(&[
            ("Forwarded", "for=1.2.3.4"),
            ("X-Forwarded-Proto", "https")
        ]));
        assert!(!secure(&[
            ("Forwarded", "garbage"),
            ("X-Forwarded-Proto", "https")
        ]));
    }

    #[test]
    fn forwarded의_proto는_정규식_첫_일치다() {
        assert!(secure(&[("Forwarded", "for=1.2.3.4, proto=https")]));
        assert!(secure(&[("Forwarded", "for=1.2.3.4;proto=https")]));
        assert!(!secure(&[("Forwarded", "proto=http, proto=https")]));
        assert!(secure(&[("Forwarded", "PROTO=https")]));
        assert!(secure(&[("Forwarded", "proto=\"https\"")]));
        assert!(!secure(&[("Forwarded", "proto=ftp")]));
        assert!(secure(&[("Forwarded", "proto=https;proto=http")]));
        assert!(!secure(&[("Forwarded", ";;;")]));
    }

    #[test]
    fn x_forwarded_proto는_첫_값이고_소문자_https와_wss만_보안이다() {
        assert!(secure(&[("X-Forwarded-Proto", "https")]));
        assert!(secure(&[("X-Forwarded-Proto", "https, http")]));
        assert!(!secure(&[("X-Forwarded-Proto", "http, https")]));
        assert!(secure(&[
            ("X-Forwarded-Proto", "https"),
            ("X-Forwarded-Proto", "http")
        ]));
        assert!(!secure(&[("X-Forwarded-Proto", "HTTPS")]));
        assert!(secure(&[("X-Forwarded-Proto", "wss")]));
        assert!(!secure(&[("X-Forwarded-Proto", "ws")]));
        assert!(!secure(&[("X-Forwarded-Proto", "")]));
        assert!(!secure(&[]));
    }

    #[test]
    fn x_forwarded_ssl_on은_proto가_없을_때만_보안이다() {
        assert!(secure(&[("X-Forwarded-Ssl", "on")]));
        assert!(secure(&[("X-Forwarded-Ssl", "ON")]));
        assert!(!secure(&[("X-Forwarded-Ssl", "off")]));
        assert!(!secure(&[
            ("X-Forwarded-Ssl", "on"),
            ("X-Forwarded-Proto", "http")
        ]));
        assert!(!secure(&[
            ("X-Forwarded-Port", "443"),
            ("X-Forwarded-Proto", "http")
        ]));
        assert!(!secure(&[("X-Forwarded-Scheme", "https")]));
    }

    #[test]
    fn 잘못된_호스트와_포트는_400이다() {
        let invalid = |h: &[(&str, &str)]| with(h) == Resolved::Invalid;
        for host in ["a.b:abc", "[::1", "[::1]:abc", "a:b:c"] {
            assert!(invalid(&[("X-Forwarded-Host", host)]), "{host}");
            assert!(invalid(&[("Forwarded", &format!("host={host}"))]), "{host}");
        }
        assert!(invalid(&[("X-Forwarded-Port", "abc")]));
        for host in [
            "a.b:99999",
            "[::1]:80",
            "a.b, c.d",
            "a b",
            "a/b",
            "@",
            "u@h",
            "",
        ] {
            assert!(!invalid(&[("X-Forwarded-Host", host)]), "{host}");
        }
        for port in ["443", "99999", "-1", "80, 81"] {
            assert!(!invalid(&[("X-Forwarded-Port", port)]), "{port}");
        }
        // Forwarded가 있으면 X-Forwarded-*는 검사하지 않는다.
        assert!(!invalid(&[
            ("Forwarded", "proto=https"),
            ("X-Forwarded-Port", "abc")
        ]));
    }

    #[test]
    fn 계층은_잘못된_요청만_본문_없는_400으로_끊는다() {
        let ok = |_: Request| Response::new(200);
        let mut bad = Request::new(Method::Get, "/");
        bad.headers.append("X-Forwarded-Port", "abc");
        let r = ForwardedHeaders::new(true).handle(bad, &ok);
        assert_eq!(r.status, 400);
        assert!(r.body.is_empty());
        assert!(r.headers.has_token("connection", "close"));
        assert_eq!(
            ForwardedHeaders::new(true)
                .handle(Request::new(Method::Get, "/"), &ok)
                .status,
            200
        );
        // 꺼져 있으면 잘못된 헤더도 통과한다.
        let mut off = Request::new(Method::Get, "/");
        off.headers.append("X-Forwarded-Port", "abc");
        assert_eq!(ForwardedHeaders::new(false).handle(off, &ok).status, 200);
    }
}
