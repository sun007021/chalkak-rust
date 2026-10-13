//! 모든 응답에 붙는 보안 헤더. Spring Security `HeaderWriterFilter` 기본값이다.
//! HSTS는 보안 요청에만 붙는데, 이 서버는 평문 HTTP만 받아서 dev 프로필의 프록시 헤더([`super::forwarded`])로만 생긴다.

use chalkak_http::{Handler, Middleware, Request, Response};

use super::forwarded;

const HEADERS: [(&str, &str); 6] = [
    (
        "Cache-Control",
        "no-cache, no-store, max-age=0, must-revalidate",
    ),
    ("Expires", "0"),
    ("Pragma", "no-cache"),
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("X-XSS-Protection", "0"),
];

/// `forwarded`가 참이면(dev 프로필) 프록시 헤더로 보안 요청이라고 판단될 때 HSTS를 더한다.
pub struct SecurityHeaders {
    forwarded: bool,
}

impl SecurityHeaders {
    pub fn new(forwarded: bool) -> Self {
        SecurityHeaders { forwarded }
    }
}

impl Middleware for SecurityHeaders {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        let secure = self.forwarded && forwarded::resolve(&req).secure();
        let mut response = next.handle(req);
        if secure {
            response
                .headers
                .set("Strict-Transport-Security", forwarded::HSTS);
        }
        // `StaticHeadersWriter`는 이미 있는 헤더를 덮어쓰지 않고, `CacheControlHeadersWriter`는 캐시 헤더
        // (Cache-Control·Pragma·Expires)가 하나라도 있으면 셋 모두 쓰지 않는다.
        let has_cache_header = ["cache-control", "pragma", "expires"]
            .iter()
            .any(|n| response.headers.contains(n));
        for (name, value) in HEADERS {
            let is_cache = matches!(name, "Cache-Control" | "Pragma" | "Expires");
            if (is_cache && has_cache_header) || response.headers.contains(name) {
                continue;
            }
            response.headers.set(name, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_http::Method;

    #[test]
    fn 응답에_여섯_헤더를_붙인다() {
        let inner = |_: Request| Response::new(200);
        let r = SecurityHeaders::new(false).handle(Request::new(Method::Get, "/"), &inner);
        assert_eq!(
            r.headers.get("cache-control"),
            Some("no-cache, no-store, max-age=0, must-revalidate")
        );
        assert_eq!(r.headers.get("expires"), Some("0"));
        assert_eq!(r.headers.get("pragma"), Some("no-cache"));
        assert_eq!(r.headers.get("x-content-type-options"), Some("nosniff"));
        assert_eq!(r.headers.get("x-frame-options"), Some("DENY"));
        assert_eq!(r.headers.get("x-xss-protection"), Some("0"));
        assert_eq!(r.headers.get_all("cache-control").count(), 1);
    }

    // dev 프로필 원본: X-Forwarded-Proto: https -> Strict-Transport-Security: max-age=31536000 ; includeSubDomains
    #[test]
    fn dev에서는_프록시가_https라고_알리면_hsts를_붙인다() {
        let inner = |_: Request| Response::new(200);
        let mut https = Request::new(Method::Get, "/");
        https.headers.append("X-Forwarded-Proto", "https");
        let r = SecurityHeaders::new(true).handle(https.clone(), &inner);
        assert_eq!(
            r.headers.get("strict-transport-security"),
            Some("max-age=31536000 ; includeSubDomains")
        );
        // 프로필이 dev가 아니면 헤더를 믿지 않는다.
        let r = SecurityHeaders::new(false).handle(https, &inner);
        assert!(!r.headers.contains("strict-transport-security"));
        let r = SecurityHeaders::new(true).handle(Request::new(Method::Get, "/"), &inner);
        assert!(!r.headers.contains("strict-transport-security"));
    }

    // java-3: /swagger-ui/index.html은 Cache-Control: no-store 한 줄만 나가고 Pragma·Expires가 붙지 않는다.
    #[test]
    fn 캐시_헤더가_이미_있으면_캐시_헤더를_더하지_않는다() {
        let inner = |_: Request| Response::new(200).with_header("Cache-Control", "no-store");
        let r = SecurityHeaders::new(false).handle(Request::new(Method::Get, "/"), &inner);
        assert_eq!(r.headers.get("cache-control"), Some("no-store"));
        assert!(!r.headers.contains("pragma"));
        assert!(!r.headers.contains("expires"));
        assert_eq!(r.headers.get("x-frame-options"), Some("DENY"));
    }
}
