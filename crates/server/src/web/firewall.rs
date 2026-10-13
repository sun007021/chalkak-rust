//! 요청 방화벽. 원본 Spring Security `StrictHttpFirewall` 기본값을 따른다.
//!
//! 보안 필터 체인에 들어가기 전에 거절하므로 응답에는 보안 헤더가 없고(요청 ID만 바깥 계층이 붙인다), Boot 오류 본문
//! (`{"timestamp","status":400,"error":"Bad Request","path"}`)과 `Connection: close`로 끝난다.
//!
//! 거절 대상:
//! - 허용 목록(`DELETE GET HEAD OPTIONS PATCH POST PUT`)에 없는 메서드.
//! - 경로의 `;`, 연속 슬래시(`//`), 역슬래시, 퍼센트(`%25`가 풀린 `%`), NUL, `.`·`..` 세그먼트.
//!
//! 경로는 서버가 이미 퍼센트 디코딩해 넘기므로 인코딩된 형태(`%3b`, `%5c`, `%2e`)도 디코딩 뒤의 문자로 잡힌다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_http::{Handler, Method, Middleware, Request, Response};

use super::boot;

pub struct Firewall {
    clock: Arc<dyn Clock>,
}

impl Firewall {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Firewall { clock }
    }
}

fn allowed_method(method: &Method) -> bool {
    matches!(
        method,
        Method::Delete
            | Method::Get
            | Method::Head
            | Method::Options
            | Method::Patch
            | Method::Post
            | Method::Put
    )
}

fn rejected_path(path: &str) -> bool {
    if path.contains([';', '\\', '%', '\0']) || path.contains("//") {
        return true;
    }
    path.split('/')
        .any(|segment| segment == "." || segment == "..")
}

impl Middleware for Firewall {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        if !allowed_method(&req.method) || rejected_path(&req.path) {
            return boot::closing(boot::error_response(
                &*self.clock,
                400,
                "Bad Request",
                &req.raw_path,
            ));
        }
        next.handle(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;

    fn status(method: Method, path: &str) -> Response {
        let clock = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        let ok = |_: Request| Response::new(200);
        Firewall::new(clock).handle(Request::new(method, path), &ok)
    }

    #[test]
    fn 위험한_경로와_허용_밖_메서드는_400이다() {
        for path in [
            "/api/v1/a;b",
            "/api//v1",
            "//api",
            "/api/./v1",
            "/api/../v1",
            "/api/v1/..",
            "/api/v1/.",
            "/api\\v1",
            "/api/%v1",
            "/api/v1/a\0b",
        ] {
            let r = status(Method::Get, path);
            assert_eq!(r.status, 400, "{path}");
            assert_eq!(r.headers.get("connection"), Some("close"), "{path}");
            assert_eq!(r.headers.get("content-type"), Some("application/json"));
            assert!(!r.headers.contains("cache-control"));
        }
        assert_eq!(status(Method::Other("FOO".into()), "/a").status, 400);
        assert_eq!(status(Method::Connect, "/a").status, 400);
    }

    #[test]
    fn 평범한_경로는_통과한다() {
        for path in [
            "/",
            "/api/v1/topics",
            "/api/v1/topics/",
            "/a.b/c..d",
            "/가나다",
        ] {
            assert_eq!(status(Method::Get, path).status, 200, "{path}");
        }
        for m in [
            Method::Delete,
            Method::Head,
            Method::Options,
            Method::Patch,
            Method::Put,
        ] {
            assert_eq!(status(m, "/a").status, 200);
        }
    }

    #[test]
    fn 본문에_요청_경로가_담긴다() {
        let r = status(Method::Get, "/api/v1/a;b");
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.ends_with(r#""status":400,"error":"Bad Request","path":"/api/v1/a;b"}"#));
    }
}
