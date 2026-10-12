//! 내부 콜백(`/internal/v1/*`) 본문 크기 제한. 원본 `CallbackBodySizeLimitFilter`와 `CallbackBodySizeLimitConfig`.
//!
//! 본문은 서명 검증보다 먼저 메모리에 올라오므로, 서명이 없는 요청이 큰 본문만으로 자원을 쓰지 못하게 인증 이전에
//! 끊는다. 접근 로그(requestId) 바로 안쪽, 보안 필터 체인 바깥에서 돈다. 이 경로에서만 건다.
//!
//! 원본과 다른 점 하나: 원본은 본문을 실제로 읽을 때에야 상한을 넘었는지 알아 chunked 전송의 초과를 읽기 중에
//! 끊는다(본문을 읽지 않는 경로는 통과한다). 여기서는 서버가 본문을 모두 읽은 뒤 핸들러를 부르므로 크기를 그대로 본다.

use chalkak_http::{Handler, Middleware, Request, Response};

const TOO_LARGE_BODY: &str =
    r#"{"errorCode":"BUSINESS_ERROR","message":"이미지 처리 콜백 본문이 너무 큽니다."}"#;

/// 서블릿 URL 패턴 `/internal/v1/*`: `/internal/v1` 자신과 그 아래.
fn applies_to(path: &str) -> bool {
    path == "/internal/v1" || path.starts_with("/internal/v1/")
}

pub struct CallbackBodyLimit {
    max_bytes: u64,
}

impl CallbackBodyLimit {
    pub fn new(max_bytes: u64) -> Self {
        CallbackBodyLimit { max_bytes }
    }

    fn too_large(&self) -> Response {
        Response::new(413)
            .with_header("Content-Type", "application/json;charset=UTF-8")
            .with_body(TOO_LARGE_BODY.as_bytes().to_vec())
    }
}

impl Middleware for CallbackBodyLimit {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        if applies_to(&req.path) {
            // `getContentLengthLong() > maxBytes`: 선언된 길이. 선언이 없거나 맞지 않아도 읽은 크기가 상한을 넘으면 막는다.
            let declared = req
                .header("content-length")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let received = u64::try_from(req.body.len()).unwrap_or(u64::MAX);
            if declared > self.max_bytes || received > self.max_bytes {
                return self.too_large();
            }
        }
        next.handle(req)
    }
}

#[cfg(test)]
mod tests {
    use chalkak_http::Method;

    use super::*;

    fn call(max: u64, path: &str, body: Vec<u8>, content_length: Option<&str>) -> Response {
        let mut req = Request::new(Method::Post, path);
        req.body = body;
        if let Some(v) = content_length {
            req.headers.append("Content-Length", v);
        }
        let ok = |_: Request| Response::new(204);
        CallbackBodyLimit::new(max).handle(req, &ok)
    }

    const COMPLETE: &str =
        "/internal/v1/post-image-processing/0198f6c1-62ba-7d30-8b12-0f733b6570d4/complete";

    // CallbackBodySizeLimitFilterTest#oversizedBody_rejectedBeforeAuthentication
    #[test]
    fn oversized_body_rejected_before_authentication() {
        let r = call(65_536, COMPLETE, vec![b'A'; 70_000], None);
        assert_eq!(r.status, 413);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"BUSINESS_ERROR","message":"이미지 처리 콜백 본문이 너무 큽니다."}"#
        );
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json;charset=UTF-8")
        );
    }

    #[test]
    fn 선언된_길이가_상한을_넘으면_본문을_보기_전에_막는다() {
        let r = call(10, COMPLETE, Vec::new(), Some("11"));
        assert_eq!(r.status, 413);
    }

    // CallbackBodySizeLimitFilterTest#normalBody_reachesAuthentication
    #[test]
    fn normal_body_reaches_authentication() {
        let r = call(65_536, COMPLETE, b"{\"width\":4032}".to_vec(), None);
        assert_eq!(r.status, 204);
        let r = call(10, COMPLETE, vec![b'A'; 10], Some("10"));
        assert_eq!(r.status, 204, "상한과 같은 크기는 통과한다");
    }

    #[test]
    fn 내부_콜백_경로에만_건다() {
        assert_eq!(
            call(1, "/api/v1/feedbacks", vec![b'A'; 100], None).status,
            204
        );
        assert_eq!(call(1, "/internal/v1", vec![b'A'; 100], None).status, 413);
        assert_eq!(call(1, "/internal/v1/x", vec![b'A'; 100], None).status, 413);
        assert_eq!(
            call(1, "/internal/v10/x", vec![b'A'; 100], None).status,
            204
        );
    }
}
