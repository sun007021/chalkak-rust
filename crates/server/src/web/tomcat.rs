//! 서블릿 컨테이너(Tomcat)가 필터 체인 앞에서 처리하는 것. `TRACE`는 요청 ID도 보안 헤더도 접근 로그도 없이 405로 끝난다
//! (java-a에 직접 보낸 결과).

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_http::{Handler, Method, Middleware, Request, Response};

use super::boot;
use super::negotiate::{self, Wanted};

/// Tomcat이 `Allow`에 싣는 고정 목록.
const TRACE_ALLOW: &str = "HEAD, DELETE, POST, GET, OPTIONS, PUT";

pub struct Tomcat {
    clock: Arc<dyn Clock>,
}

impl Tomcat {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Tomcat { clock }
    }
}

impl Middleware for Tomcat {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        if req.method == Method::Trace {
            let wanted = Wanted::of(&req);
            let response = boot::error_response(&*self.clock, 405, "Method Not Allowed", &req.raw_path)
                .with_header("Allow", TRACE_ALLOW);
            return negotiate::finish(response, &wanted, &*self.clock);
        }
        next.handle(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;

    fn run(method: Method) -> Response {
        let clock = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        let ok = |_: Request| Response::new(200);
        Tomcat::new(clock).handle(Request::new(method, "/api/v1/topics"), &ok)
    }

    #[test]
    fn trace는_405_json이고_다른_메서드는_통과한다() {
        let r = run(Method::Trace);
        assert_eq!(r.status, 405);
        assert_eq!(r.headers.get("allow"), Some(TRACE_ALLOW));
        assert_eq!(r.headers.get("content-type"), Some("application/json"));
        assert!(!r.headers.contains("x-request-id"));
        assert_eq!(run(Method::Get).status, 200);
    }
}
