//! 모든 응답에 붙는 보안 헤더. Spring Security `HeaderWriterFilter` 기본값이다(HSTS는 HTTPS에서만이라 없다).

use chalkak_http::{Handler, Middleware, Request, Response};

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

pub struct SecurityHeaders;

impl Middleware for SecurityHeaders {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        let mut response = next.handle(req);
        for (name, value) in HEADERS {
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
        let inner = |_: Request| Response::new(200).with_header("Cache-Control", "max-age=60");
        let r = SecurityHeaders.handle(Request::new(Method::Get, "/"), &inner);
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
}
