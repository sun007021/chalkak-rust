//! 관리자 API(`/api/v1/admin`, `/api/v1/admin/**`)의 CORS. 원본 `AdminCorsWebMvcConfig`와 Spring `DefaultCorsProcessor`.
//!
//! 허용 설정은 고정이다: 메서드 `GET POST PUT PATCH DELETE OPTIONS`, 요청 헤더 `Authorization Content-Type Accept`, 노출 헤더
//! `X-Request-Id`, 자격 증명 불가, 사전 요청 캐시 3600초. 허용 Origin만 프로필마다 다르다.
//!
//! 이 경로의 모든 응답에는 CORS 요청이든 아니든 `Vary: Origin, Access-Control-Request-Method,
//! Access-Control-Request-Headers`가 붙는다. 다른 경로에는 CORS 처리가 없다.
//!
//! 보안 필터 체인 안에서 인증보다 먼저 실행하므로 사전 요청(preflight)은 토큰 없이 답한다.

use chalkak_http::{Handler, Method, Middleware, Request, Response};

const ADMIN_API_PATH: &str = "/api/v1/admin";

const ALLOWED_METHODS: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
const ALLOWED_HEADERS: [&str; 3] = ["Authorization", "Content-Type", "Accept"];
const EXPOSED_HEADERS: &str = "X-Request-Id";
const MAX_AGE_SECONDS: &str = "3600";

const VARY: [&str; 3] = [
    "Origin",
    "Access-Control-Request-Method",
    "Access-Control-Request-Headers",
];

pub struct AdminCors {
    /// 끝 슬래시를 뗀 허용 Origin.
    allowed_origins: Vec<String>,
}

impl AdminCors {
    pub fn new(allowed_origins: &[String]) -> Self {
        AdminCors {
            allowed_origins: allowed_origins
                .iter()
                .map(|o| trim_trailing_slash(o).to_owned())
                .collect(),
        }
    }

    /// `checkOrigin`: 대소문자 무시, 끝 슬래시 무시. 응답에는 요청에 온 Origin 문자열을 그대로 돌려준다.
    fn check_origin<'a>(&self, origin: &'a str) -> Option<&'a str> {
        if origin.trim().is_empty() {
            return None;
        }
        let candidate = trim_trailing_slash(origin);
        self.allowed_origins
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(candidate))
            .then_some(origin)
    }

    fn process(&self, req: &Request, origin: &str) -> Decision {
        let preflight =
            req.method == Method::Options && req.headers.contains("access-control-request-method");
        let Some(allow_origin) = self.check_origin(origin) else {
            return Decision::Reject;
        };

        let method = if preflight {
            req.header("access-control-request-method")
                .unwrap_or_default()
                .to_owned()
        } else {
            req.method.as_str().to_owned()
        };
        if !ALLOWED_METHODS.contains(&method.as_str()) {
            return Decision::Reject;
        }

        let allow_headers = if preflight {
            match check_request_headers(req) {
                Some(headers) => headers,
                None => return Decision::Reject,
            }
        } else {
            Vec::new()
        };

        Decision::Allow {
            allow_origin: allow_origin.to_owned(),
            preflight,
            allow_headers,
        }
    }
}

enum Decision {
    Reject,
    Allow {
        allow_origin: String,
        preflight: bool,
        allow_headers: Vec<String>,
    },
}

fn trim_trailing_slash(s: &str) -> &str {
    s.strip_suffix('/').unwrap_or(s)
}

fn applies_to(path: &str) -> bool {
    path == ADMIN_API_PATH
        || path
            .strip_prefix(ADMIN_API_PATH)
            .is_some_and(|r| r.starts_with('/'))
}

/// 사전 요청의 `Access-Control-Request-Headers`를 허용 목록과 맞춘다. 없으면 빈 목록(허용),
/// 있는데 허용된 헤더가 하나도 없으면 `None`(거절). 응답에는 요청에 온 표기 그대로 싣는다.
fn check_request_headers(req: &Request) -> Option<Vec<String>> {
    let requested: Vec<&str> = req
        .headers
        .get_all("access-control-request-headers")
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    if requested.is_empty() {
        return Some(Vec::new());
    }
    let allowed: Vec<String> = requested
        .into_iter()
        .filter(|h| ALLOWED_HEADERS.iter().any(|a| a.eq_ignore_ascii_case(h)))
        .map(str::to_owned)
        .collect();
    (!allowed.is_empty()).then_some(allowed)
}

/// `Origin`의 (scheme, host, port). 기본 포트는 scheme에서 채운다. 형식이 틀리면 `None`.
fn parse_origin(origin: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = origin.split_once("://")?;
    if scheme.is_empty() || scheme.contains([':', '/', '?', '#']) {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let (host, port) = split_host_port(authority)?;
    let port = match port {
        Some(p) => p,
        None => default_port(scheme),
    };
    Some((scheme.to_ascii_lowercase(), host.to_ascii_lowercase(), port))
}

fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    // 대괄호로 감싼 IPv6 리터럴은 그 안의 ':'를 포트 구분자로 보지 않는다.
    let host_end = if authority.starts_with('[') {
        authority.find(']')? + 1
    } else {
        authority.find(':').unwrap_or(authority.len())
    };
    let host = &authority[..host_end];
    match authority[host_end..].strip_prefix(':') {
        None if authority.len() == host_end => Some((host, None)),
        None => None,
        Some("") => Some((host, None)),
        Some(p) => Some((host, Some(p.parse().ok()?))),
    }
}

fn default_port(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

/// `CorsUtils.isCorsRequest`: Origin이 있고 요청 자신의 origin(http + Host)과 다르다.
fn is_cors_request(req: &Request, origin: &str) -> bool {
    let Some((scheme, host, port)) = parse_origin(origin) else {
        return true;
    };
    let same = req
        .header("host")
        .and_then(|h| split_host_port(h))
        .is_some_and(|(req_host, req_port)| {
            scheme == "http"
                && host.eq_ignore_ascii_case(req_host)
                && port == req_port.unwrap_or(80)
        });
    !same
}

fn add_vary(response: &mut Response) {
    for name in VARY {
        let present = response
            .headers
            .get_all("vary")
            .flat_map(|v| v.split(','))
            .any(|v| v.trim().eq_ignore_ascii_case(name));
        if !present {
            response.headers.append("Vary", name);
        }
    }
}

fn reject() -> Response {
    // 본문만 있고 Content-Type은 없다(원본 DefaultCorsProcessor.rejectRequest).
    Response::new(403).with_body("Invalid CORS request")
}

impl Middleware for AdminCors {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        if !applies_to(&req.path) {
            return next.handle(req);
        }
        let origin = req.header("origin").map(str::to_owned);
        let cors_origin = origin.filter(|o| is_cors_request(&req, o));
        let Some(origin) = cors_origin else {
            let mut response = next.handle(req);
            add_vary(&mut response);
            return response;
        };

        match self.process(&req, &origin) {
            Decision::Reject => {
                let mut response = reject();
                add_vary(&mut response);
                response
            }
            Decision::Allow {
                allow_origin,
                preflight,
                allow_headers,
            } => {
                let mut response = if preflight {
                    Response::new(200)
                } else {
                    next.handle(req)
                };
                add_vary(&mut response);
                response
                    .headers
                    .set("Access-Control-Allow-Origin", allow_origin);
                if preflight {
                    response
                        .headers
                        .set("Access-Control-Allow-Methods", ALLOWED_METHODS.join(","));
                    if !allow_headers.is_empty() {
                        response
                            .headers
                            .set("Access-Control-Allow-Headers", allow_headers.join(", "));
                    }
                }
                response
                    .headers
                    .set("Access-Control-Expose-Headers", EXPOSED_HEADERS);
                if preflight {
                    response
                        .headers
                        .set("Access-Control-Max-Age", MAX_AGE_SECONDS);
                }
                response
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors() -> AdminCors {
        AdminCors::new(&["http://localhost:3000".to_owned()])
    }

    fn call(req: Request) -> Response {
        let inner = |_: Request| Response::new(204);
        cors().handle(req, &inner)
    }

    fn preflight(origin: &str, method: &str, headers: Option<&str>) -> Request {
        let mut req = Request::new(Method::Options, "/api/v1/admin/topics");
        req.headers.append("Host", "127.0.0.1:18082");
        req.headers.append("Origin", origin);
        req.headers.append("Access-Control-Request-Method", method);
        if let Some(h) = headers {
            req.headers.append("Access-Control-Request-Headers", h);
        }
        req
    }

    fn vary(r: &Response) -> Vec<&str> {
        r.headers.get_all("vary").collect()
    }

    // platform/cors: 허용 Origin preflight
    #[test]
    fn 허용_origin의_사전_요청은_허용_목록으로_답한다() {
        let r = call(preflight(
            "http://localhost:3000",
            "POST",
            Some("authorization, content-type"),
        ));
        assert_eq!(r.status, 200);
        assert_eq!(
            r.headers.get("access-control-allow-origin"),
            Some("http://localhost:3000")
        );
        assert_eq!(
            r.headers.get("access-control-allow-methods"),
            Some("GET,POST,PUT,PATCH,DELETE,OPTIONS")
        );
        assert_eq!(
            r.headers.get("access-control-allow-headers"),
            Some("authorization, content-type")
        );
        assert_eq!(
            r.headers.get("access-control-expose-headers"),
            Some("X-Request-Id")
        );
        assert_eq!(r.headers.get("access-control-max-age"), Some("3600"));
        assert_eq!(
            vary(&r),
            [
                "Origin",
                "Access-Control-Request-Method",
                "Access-Control-Request-Headers"
            ]
        );
        assert!(r.body.is_empty());
    }

    // platform/cors: 허용하지 않는 Origin preflight
    #[test]
    fn 허용하지_않는_origin은_403이다() {
        let r = call(preflight("http://evil.example", "POST", None));
        assert_eq!(r.status, 403);
        assert_eq!(r.body, b"Invalid CORS request");
        assert!(!r.headers.contains("content-type"));
        assert!(!r.headers.contains("access-control-allow-origin"));
        assert_eq!(vary(&r).len(), 3);
    }

    #[test]
    fn 허용_밖_메서드와_헤더의_사전_요청은_거절한다() {
        assert_eq!(
            call(preflight("http://localhost:3000", "TRACE", None)).status,
            403
        );
        assert_eq!(
            call(preflight("http://localhost:3000", "get", None)).status,
            403
        );
        assert_eq!(
            call(preflight("http://localhost:3000", "GET", Some("x-custom"))).status,
            403
        );
        // 허용된 것이 하나라도 있으면 허용된 것만 돌려준다.
        let r = call(preflight(
            "http://localhost:3000",
            "GET",
            Some("X-Custom, Accept"),
        ));
        assert_eq!(r.status, 200);
        assert_eq!(
            r.headers.get("access-control-allow-headers"),
            Some("Accept")
        );
    }

    #[test]
    fn 요청_헤더가_없는_사전_요청은_허용_헤더를_싣지_않는다() {
        let r = call(preflight("http://localhost:3000", "GET", None));
        assert_eq!(r.status, 200);
        assert!(!r.headers.contains("access-control-allow-headers"));
    }

    // origin 비교: 대소문자 무시, 끝 슬래시 무시, 응답은 요청 표기 그대로
    #[test]
    fn origin_비교는_대소문자와_끝_슬래시를_무시한다() {
        let r = call(preflight("HTTP://LocalHost:3000/", "GET", None));
        assert_eq!(r.status, 200);
        assert_eq!(
            r.headers.get("access-control-allow-origin"),
            Some("HTTP://LocalHost:3000/")
        );
    }

    // platform/cors: 허용 Origin 실제 요청
    #[test]
    fn 실제_요청은_안쪽_응답에_cors_헤더를_붙인다() {
        let mut req = Request::new(Method::Get, "/api/v1/admin/feedbacks");
        req.headers.append("Host", "127.0.0.1:18082");
        req.headers.append("Origin", "http://localhost:3000");
        let r = call(req);
        assert_eq!(r.status, 204);
        assert_eq!(
            r.headers.get("access-control-allow-origin"),
            Some("http://localhost:3000")
        );
        assert_eq!(
            r.headers.get("access-control-expose-headers"),
            Some("X-Request-Id")
        );
        assert!(!r.headers.contains("access-control-allow-methods"));
        assert!(!r.headers.contains("access-control-max-age"));
        assert_eq!(vary(&r).len(), 3);
    }

    #[test]
    fn 허용_밖_origin의_실제_요청도_403이다() {
        let mut req = Request::new(Method::Get, "/api/v1/admin/feedbacks");
        req.headers.append("Origin", "http://evil.example");
        assert_eq!(call(req).status, 403);
        // HEAD는 허용 메서드가 아니다.
        let mut req = Request::new(Method::Head, "/api/v1/admin/feedbacks");
        req.headers.append("Origin", "http://localhost:3000");
        assert_eq!(call(req).status, 403);
    }

    #[test]
    fn 같은_origin_요청은_cors_요청이_아니라_vary만_붙는다() {
        let mut req = Request::new(Method::Get, "/api/v1/admin/feedbacks");
        req.headers.append("Host", "127.0.0.1:18082");
        req.headers.append("Origin", "http://127.0.0.1:18082");
        let r = call(req);
        assert_eq!(r.status, 204);
        assert!(!r.headers.contains("access-control-allow-origin"));
        assert_eq!(vary(&r).len(), 3);
    }

    #[test]
    fn origin이_없으면_vary만_붙는다() {
        let r = call(Request::new(Method::Get, "/api/v1/admin"));
        assert_eq!(r.status, 204);
        assert_eq!(vary(&r).len(), 3);
    }

    // platform/cors: 회원 API에 Origin
    #[test]
    fn 관리자_경로가_아니면_손대지_않는다() {
        for path in ["/api/v1/topics", "/api/v1/administrator", "/api/v1/admins"] {
            let mut req = Request::new(Method::Get, path);
            req.headers.append("Origin", "http://localhost:3000");
            let r = call(req);
            assert_eq!(r.status, 204, "{path}");
            assert!(r.headers.is_empty(), "{path}");
        }
    }

    #[test]
    fn origin_해석() {
        assert_eq!(
            parse_origin("http://localhost:3000"),
            Some(("http".into(), "localhost".into(), 3000))
        );
        assert_eq!(
            parse_origin("https://a.b"),
            Some(("https".into(), "a.b".into(), 443))
        );
        assert_eq!(
            parse_origin("http://[::1]:8080"),
            Some(("http".into(), "[::1]".into(), 8080))
        );
        assert_eq!(parse_origin("null"), None);
        assert_eq!(parse_origin("http://h:x"), None);
    }
}
