//! 요청마다 requestId를 붙이고 접근 로그를 한 줄 남긴다. 원본 `RequestIdFilter`.
//!
//! 가장 바깥 계층이라 보안 단계에서 거절된 요청도 같은 requestId로 기록된다. 접근 로그의 route는 라우트가 매칭한 경로
//! 템플릿뿐이다(원본 URI에는 식별자와 쿼리 값이 섞여 있다). 라우트에 닿기 전에 거절된 요청은 `UNMATCHED`다.
//! 회원 식별자(userId)는 인증 뒤 안쪽 계층이 [`chalkak_app::web::context`]에 올려 둔 것이 있을 때만 남긴다.
//! actuator 경로는 로그를 남기지 않는다.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use chalkak_app::web::context;
use chalkak_core::log::{Logger, fields};
use chalkak_core::request_id;
use chalkak_http::{Handler, Middleware, Request, Response};

const ACCESS_LOGGER: &str = "chalkak.access";

pub struct AccessLog {
    logger: Arc<Logger>,
}

impl AccessLog {
    pub fn new(logger: Arc<Logger>) -> Self {
        AccessLog { logger }
    }

    fn log(&self, method: &str, scope: &context::RequestScope, status: u16, started: Instant) {
        let info = scope.info();
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let mut event = self
            .logger
            .info(ACCESS_LOGGER)
            .field(fields::TYPE, fields::TYPE_ACCESS)
            .field(fields::METHOD, method)
            .field(
                fields::ROUTE,
                info.route.as_deref().unwrap_or(fields::UNMATCHED_ROUTE),
            )
            .field(fields::STATUS, i64::from(status))
            .field(fields::DURATION_MS, duration_ms);
        if let Some(user_id) = info.user_id {
            event = event.field(fields::USER_ID, user_id);
        }
        event.log("access");
    }
}

fn is_actuator_path(path: &str) -> bool {
    path == "/actuator" || path.starts_with("/actuator/")
}

impl Middleware for AccessLog {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        let started = Instant::now();
        let id = request_id::generate();
        let _request_id = request_id::enter(id);
        let scope = context::begin();
        let method = req.method.as_str().to_owned();
        let log_it = !is_actuator_path(&req.path);

        let outcome = panic::catch_unwind(AssertUnwindSafe(|| next.handle(req)));
        match outcome {
            Ok(mut response) => {
                response.headers.set(request_id::HEADER, id.to_string());
                if log_it {
                    self.log(&method, &scope, response.status, started);
                }
                response
            }
            Err(payload) => {
                // 체인 밖으로 예외가 새면 컨테이너가 500으로 응답한다. 서버가 패닉을 500으로 바꾸므로 같은 상태로 남긴다.
                if log_it {
                    self.log(&method, &scope, 500, started);
                }
                panic::resume_unwind(payload)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;
    use chalkak_core::log::Level;
    use chalkak_http::Method;
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn setup() -> (AccessLog, Buf) {
        let buf = Buf::default();
        let logger = Logger::new(
            Box::new(buf.clone()),
            Level::Info,
            Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap())),
        );
        (AccessLog::new(Arc::new(logger)), buf)
    }

    fn lines(buf: &Buf) -> Vec<String> {
        String::from_utf8(buf.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    // AccessLogTest#accessLog_matchedRoute_logsTemplateWithRequestId
    #[test]
    fn access_log_matched_route_logs_template_with_request_id() {
        let (mw, buf) = setup();
        let endpoint = |_: Request| {
            context::set_route("/api/v1/topics");
            Response::new(200)
        };
        let r = mw.handle(Request::new(Method::Get, "/api/v1/topics"), &endpoint);
        let id = r.headers.get("x-request-id").expect("X-Request-Id");
        assert!(uuid::Uuid::parse_str(id).is_ok());
        let out = lines(&buf);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("\"route\":\"/api/v1/topics\""),
            "{}",
            out[0]
        );
        assert!(out[0].contains("\"type\":\"access\""), "{}", out[0]);
        assert!(out[0].contains("\"status\":200"), "{}", out[0]);
        assert!(
            out[0].contains(&format!("\"requestId\":\"{id}\"")),
            "{}",
            out[0]
        );
        assert!(!out[0].contains("userId"), "{}", out[0]);
    }

    // AccessLogTest#accessLog_rejectedBySecurity_logsUnmatchedRoute
    #[test]
    fn access_log_rejected_by_security_logs_unmatched_route() {
        let (mw, buf) = setup();
        let endpoint = |_: Request| Response::new(401);
        let r = mw.handle(Request::new(Method::Get, "/api/v1/posts/1"), &endpoint);
        assert!(r.headers.contains("x-request-id"));
        let out = lines(&buf);
        assert!(out[0].contains("\"route\":\"UNMATCHED\""), "{}", out[0]);
        assert!(out[0].contains("\"status\":401"), "{}", out[0]);
    }

    // AccessLogTest#accessLog_memberRequest_logsUserId
    #[test]
    fn access_log_member_request_logs_user_id() {
        let (mw, buf) = setup();
        let endpoint = |_: Request| {
            context::set_user_id("01990000-0000-7000-8000-000000000001".to_owned());
            Response::new(200)
        };
        mw.handle(Request::new(Method::Get, "/api/v1/topics"), &endpoint);
        let out = lines(&buf);
        assert_eq!(out[0].matches("\"userId\"").count(), 1, "{}", out[0]);
        assert!(
            out[0].contains("\"userId\":\"01990000-0000-7000-8000-000000000001\""),
            "{}",
            out[0]
        );
    }

    // AccessLogTest#accessLog_actuatorPath_logsNothing
    #[test]
    fn access_log_actuator_path_logs_nothing() {
        let (mw, buf) = setup();
        let endpoint = |_: Request| Response::new(200);
        let r = mw.handle(Request::new(Method::Get, "/actuator/health"), &endpoint);
        assert!(r.headers.contains("x-request-id"));
        assert!(lines(&buf).is_empty());
        mw.handle(Request::new(Method::Get, "/actuator"), &endpoint);
        assert!(lines(&buf).is_empty());
        mw.handle(Request::new(Method::Get, "/actuatorx"), &endpoint);
        assert_eq!(lines(&buf).len(), 1);
    }

    // AccessLogTest#requestId_twoRequests_areDifferent
    #[test]
    fn request_id_two_requests_are_different() {
        let (mw, _buf) = setup();
        let endpoint = |_: Request| Response::new(200);
        let a = mw.handle(Request::new(Method::Get, "/a"), &endpoint);
        let b = mw.handle(Request::new(Method::Get, "/a"), &endpoint);
        assert_ne!(a.headers.get("x-request-id"), b.headers.get("x-request-id"));
    }

    #[test]
    fn 패닉해도_접근_로그를_500으로_남기고_패닉을_다시_던진다() {
        let (mw, buf) = setup();
        let endpoint = |_: Request| -> Response { panic!("boom") };
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            mw.handle(Request::new(Method::Get, "/a"), &endpoint)
        }));
        assert!(result.is_err());
        assert!(lines(&buf)[0].contains("\"status\":500"));
    }
}
