//! 이미지 처리 콜백 본문 크기 제한. 원본 `CallbackBodySizeLimitFilter`와 `CallbackBodySizeLimitConfig`.
//!
//! 인증보다 앞에서 `Content-Length`가 상한을 넘는 요청을 413으로 끊는다. 서블릿 URL 패턴 `/internal/v1/*`와 같이
//! `/internal/v1` 자신과 그 아래 경로에만 걸린다. 콜백 핸들러는 본문을 읽지 않으므로 `Content-Length`가 없거나(청크)
//! 상한 이하이면 읽는 도중에 끊길 일도 없다.

use chalkak_core::error::{ErrorCode, error_body};
use chalkak_http::{Handler, Middleware, Request, Response};

const MESSAGE: &str = "이미지 처리 콜백 본문이 너무 큽니다.";

pub struct CallbackBodyLimit {
    max_bytes: u64,
}

impl CallbackBodyLimit {
    pub fn new(max_bytes: u64) -> Self {
        CallbackBodyLimit { max_bytes }
    }
}

fn is_callback_path(path: &str) -> bool {
    path == "/internal/v1" || path.starts_with("/internal/v1/")
}

/// `getContentLengthLong`: 헤더가 없거나 숫자가 아니면 -1이라 거르지 않는다.
fn content_length(req: &Request) -> i64 {
    req.headers
        .get("content-length")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(-1)
}

impl Middleware for CallbackBodyLimit {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        let limit = i64::try_from(self.max_bytes).unwrap_or(i64::MAX);
        if is_callback_path(&req.path) && content_length(&req) > limit {
            return Response::new(413)
                .with_header("Content-Type", "application/json;charset=UTF-8")
                .with_body(error_body(ErrorCode::BUSINESS_ERROR, MESSAGE).into_bytes());
        }
        next.handle(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_http::Method;

    fn request(path: &str, content_length: Option<&str>) -> Request {
        let mut req = Request::new(Method::Post, path);
        if let Some(v) = content_length {
            req.headers.set("Content-Length", v);
        }
        req
    }

    fn run(path: &str, content_length: Option<&str>) -> Response {
        let inner = |_: Request| Response::new(204);
        CallbackBodyLimit::new(10).handle(request(path, content_length), &inner)
    }

    // CallbackBodySizeLimitFilterTest: Content-Length가 상한을 넘으면 413
    #[test]
    fn 상한을_넘는_콜백은_413_json이다() {
        let r = run("/internal/v1/signature-processing/x/complete", Some("11"));
        assert_eq!(r.status, 413);
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"BUSINESS_ERROR","message":"이미지 처리 콜백 본문이 너무 큽니다."}"#
        );
    }

    #[test]
    fn 상한_이하이거나_길이를_모르면_통과한다() {
        assert_eq!(run("/internal/v1/a/b", Some("10")).status, 204);
        assert_eq!(run("/internal/v1/a/b", None).status, 204);
        assert_eq!(run("/internal/v1/a/b", Some("abc")).status, 204);
    }

    #[test]
    fn 콜백_경로에만_건다() {
        assert_eq!(run("/api/v1/auth/refresh", Some("999")).status, 204);
        assert_eq!(run("/internal/v10/x", Some("999")).status, 204);
        assert_eq!(run("/internal/v1", Some("999")).status, 413);
    }
}
