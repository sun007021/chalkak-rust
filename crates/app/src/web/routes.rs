//! 라우트 등록. 핸들러를 감싸 (1) 접근 로그용 경로 템플릿을 기록하고 (2) `Err(ApiError)`를 오류 응답으로 바꾸고
//! (3) 패닉을 500 JSON으로 바꾼다. 어떤 경로에도 안 맞을 때(404)와 경로는 맞는데 메서드가 없을 때(405)의 응답도
//! 여기서 원본 모양으로 정한다.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use chalkak_core::log::Logger;
use chalkak_http::{Method, Request, Response, Router};

use super::{ApiError, ApiResult, context};

pub struct Routes {
    router: Router,
    logger: Arc<Logger>,
}

impl Routes {
    pub fn new(logger: Arc<Logger>) -> Self {
        Routes {
            router: Router::new(),
            logger,
        }
    }

    pub fn route<F>(mut self, method: Method, pattern: &'static str, handler: F) -> Self
    where
        F: Fn(&Request) -> ApiResult + Send + Sync + 'static,
    {
        let logger = Arc::clone(&self.logger);
        self.router = self.router.route(method, pattern, move |req: Request| {
            context::set_route(pattern);
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| handler(&req)));
            match outcome {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => error.into_response(&logger),
                Err(panic) => {
                    ApiError::Internal(panic_message(panic.as_ref())).into_response(&logger)
                }
            }
        });
        self
    }

    pub fn get<F>(self, pattern: &'static str, handler: F) -> Self
    where
        F: Fn(&Request) -> ApiResult + Send + Sync + 'static,
    {
        self.route(Method::Get, pattern, handler)
    }

    pub fn post<F>(self, pattern: &'static str, handler: F) -> Self
    where
        F: Fn(&Request) -> ApiResult + Send + Sync + 'static,
    {
        self.route(Method::Post, pattern, handler)
    }

    pub fn put<F>(self, pattern: &'static str, handler: F) -> Self
    where
        F: Fn(&Request) -> ApiResult + Send + Sync + 'static,
    {
        self.route(Method::Put, pattern, handler)
    }

    pub fn delete<F>(self, pattern: &'static str, handler: F) -> Self
    where
        F: Fn(&Request) -> ApiResult + Send + Sync + 'static,
    {
        self.route(Method::Delete, pattern, handler)
    }

    /// 404·405 응답을 원본 모양으로 정해 라우터를 완성한다.
    pub fn into_router(self) -> Router {
        let logger_404 = Arc::clone(&self.logger);
        let logger_405 = self.logger;
        self.router
            .not_found(move |_req: Request| ApiError::NotFound.into_response(&logger_404))
            .method_not_allowed(move |allowed: &[Method], req: Request| {
                // 원본: 경로에 매핑이 있고 OPTIONS만 없으면 DispatcherServlet이 Allow를 채워 200으로 답한다.
                if req.method == Method::Options {
                    return options_response(allowed);
                }
                // 원본 405 응답에는 Allow 헤더가 없다(harness/golden/platform__method-not-allowed.json).
                ApiError::MethodNotSupported.into_response(&logger_405)
            })
    }
}

/// `Allow: GET,HEAD,OPTIONS` + 빈 `Accept-Patch`. Spring `HttpOptionsHandler`의 응답이다.
fn options_response(allowed: &[Method]) -> Response {
    const ORDER: [&str; 8] = [
        "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE",
    ];
    let mut names: Vec<&str> = allowed.iter().map(Method::as_str).collect();
    names.push("OPTIONS");
    names.sort_by_key(|n| ORDER.iter().position(|o| o == n).unwrap_or(usize::MAX));
    names.dedup();
    Response::new(200)
        .with_header("Allow", names.join(","))
        .with_header("Accept-Patch", "")
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::{FixedClock, log::Level};
    use chalkak_http::Handler;

    fn logger() -> Arc<Logger> {
        Arc::new(Logger::new(
            Box::new(std::io::sink()),
            Level::Error,
            Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap())),
        ))
    }

    fn router() -> Router {
        Routes::new(logger())
            .get("/a", |_| Ok(Response::new(200).with_body("ok")))
            .post("/a", |_| Err(ApiError::UnreadableBody))
            .put("/p", |_| Ok(Response::new(204)))
            .delete("/p", |_| Ok(Response::new(204)))
            .get("/boom", |_| panic!("boom"))
            .into_router()
    }

    fn call(router: &Router, method: Method, path: &str) -> Response {
        router.handle(Request::new(method, path))
    }

    #[test]
    fn 핸들러_오류는_json_오류_응답이_된다() {
        let r = call(&router(), Method::Post, "/a");
        assert_eq!(r.status, 400);
        assert_eq!(r.headers.get("content-type"), Some("application/json"));
    }

    #[test]
    fn 없는_경로는_json_404다() {
        let r = call(&router(), Method::Get, "/nope");
        assert_eq!(r.status, 404);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"BUSINESS_ERROR","message":"요청한 API를 찾을 수 없습니다."}"#
        );
    }

    #[test]
    fn 메서드가_없으면_allow_없는_json_405다() {
        let r = call(&router(), Method::Delete, "/a");
        assert_eq!(r.status, 405);
        assert!(!r.headers.contains("allow"));
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"BUSINESS_ERROR","message":"지원하지 않는 요청 방식이거나 형식입니다."}"#
        );
    }

    #[test]
    fn options는_allow를_채워_200으로_답한다() {
        let r = call(&router(), Method::Options, "/a");
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("allow"), Some("GET,HEAD,POST,OPTIONS"));
        assert_eq!(r.headers.get("accept-patch"), Some(""));
        assert!(r.body.is_empty());
    }

    #[test]
    fn put과_delete도_등록한다() {
        let router = router();
        assert_eq!(call(&router, Method::Put, "/p").status, 204);
        assert_eq!(call(&router, Method::Delete, "/p").status, 204);
        assert_eq!(call(&router, Method::Post, "/p").status, 405);
    }

    #[test]
    fn 핸들러_패닉은_500_json이다() {
        let r = call(&router(), Method::Get, "/boom");
        assert_eq!(r.status, 500);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"INTERNAL_ERROR","message":"서버에서 요청을 처리하지 못했습니다."}"#
        );
    }
}
