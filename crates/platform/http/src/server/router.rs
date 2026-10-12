//! 경로 변수를 지원하는 라우터와 404·405 판정.
//!
//! 원본(Spring Boot 4 기본 설정) 규칙을 따른다:
//! - 경로 끝 슬래시는 다른 경로다(`/posts`와 `/posts/`는 별개). 대소문자도 구분한다.
//!   근거: `harness/golden/platform__unknown-path.json`의 "공개 목록 경로 끝 슬래시"·"대소문자가 다른 경로".
//! - 경로 변수는 비어 있지 않은 한 세그먼트와만 맞는다.
//! - 경로는 맞는데 메서드가 없으면 405(허용 메서드 목록 제공), 경로가 없으면 404.
//!   근거: `platform__method-not-allowed.json`. HEAD는 GET 핸들러로 처리한다(Spring `HEAD` 자동 매핑).
//! - 리터럴 세그먼트가 변수보다 우선한다(Spring의 더 구체적인 패턴 우선).

use std::sync::Arc;

use super::handler::Handler;
use super::method::Method;
use super::request::Request;
use super::response::Response;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Param(String),
}

struct Route {
    method: Method,
    segments: Vec<Segment>,
    handler: Arc<dyn Handler>,
}

/// 조회 결과.
pub enum RouteMatch<'a> {
    Found {
        handler: &'a dyn Handler,
        /// 경로 변수(패턴에 나온 순서).
        params: Vec<(String, String)>,
    },
    /// 경로는 맞지만 메서드가 없다. `allowed`는 메서드 이름순으로 정렬돼 있고 GET이 있으면 HEAD도 든다.
    MethodNotAllowed {
        allowed: Vec<Method>,
    },
    NotFound,
}

type MethodNotAllowedFn = dyn Fn(&[Method], Request) -> Response + Send + Sync;

#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
    not_found: Option<Arc<dyn Handler>>,
    method_not_allowed: Option<Arc<MethodNotAllowedFn>>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// 경로를 등록한다. 패턴은 `/`로 시작하고 세그먼트는 리터럴이거나 `{이름}` 전체여야 한다
    /// (`/api/v1/posts/{postId}`). 잘못된 패턴은 서버 시작 시점의 프로그래밍 오류이므로 패닉한다.
    /// 같은 메서드·같은 모양의 패턴을 두 번 등록해도 패닉한다.
    pub fn route(mut self, method: Method, pattern: &str, handler: impl Handler + 'static) -> Self {
        let segments = parse_pattern(pattern);
        let shape = |s: &[Segment]| -> Vec<Option<String>> {
            s.iter()
                .map(|seg| match seg {
                    Segment::Literal(l) => Some(l.clone()),
                    Segment::Param(_) => None,
                })
                .collect()
        };
        assert!(
            !self
                .routes
                .iter()
                .any(|r| r.method == method && shape(&r.segments) == shape(&segments)),
            "중복 라우트: {method} {pattern}"
        );
        self.routes.push(Route {
            method,
            segments,
            handler: Arc::new(handler),
        });
        self
    }

    /// 어떤 경로에도 안 맞을 때 쓸 핸들러. 기본은 본문 없는 404.
    pub fn not_found(mut self, handler: impl Handler + 'static) -> Self {
        self.not_found = Some(Arc::new(handler));
        self
    }

    /// 경로는 맞지만 메서드가 없을 때 쓸 응답 생성기(허용 메서드 목록을 받는다).
    /// 기본은 `Allow` 헤더가 있는 본문 없는 405.
    pub fn method_not_allowed(
        mut self,
        f: impl Fn(&[Method], Request) -> Response + Send + Sync + 'static,
    ) -> Self {
        self.method_not_allowed = Some(Arc::new(f));
        self
    }

    /// 메서드와 (디코딩된) 경로로 핸들러를 찾는다.
    pub fn lookup(&self, method: &Method, path: &str) -> RouteMatch<'_> {
        let Some(parts) = split_path(path) else {
            return RouteMatch::NotFound;
        };
        let candidates: Vec<(&Route, Vec<(String, String)>)> = self
            .routes
            .iter()
            .filter_map(|r| match_segments(&r.segments, &parts).map(|p| (r, p)))
            .collect();
        if candidates.is_empty() {
            return RouteMatch::NotFound;
        }

        let pick = |wanted: &Method| {
            candidates
                .iter()
                .filter(|(r, _)| r.method == *wanted)
                // 리터럴이 변수보다 앞서는 쪽이 더 구체적이다. 같으면 먼저 등록한 것.
                .min_by_key(|(r, _)| specificity(&r.segments))
        };
        let chosen = pick(method).or_else(|| {
            if *method == Method::Head {
                pick(&Method::Get)
            } else {
                None
            }
        });
        if let Some((route, params)) = chosen {
            return RouteMatch::Found {
                handler: route.handler.as_ref(),
                params: params.clone(),
            };
        }

        let mut allowed: Vec<Method> = Vec::new();
        for (r, _) in &candidates {
            if !allowed.contains(&r.method) {
                allowed.push(r.method.clone());
            }
        }
        if allowed.contains(&Method::Get) && !allowed.contains(&Method::Head) {
            allowed.push(Method::Head);
        }
        allowed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        RouteMatch::MethodNotAllowed { allowed }
    }
}

impl Handler for Router {
    fn handle(&self, mut req: Request) -> Response {
        match self.lookup(&req.method, &req.path) {
            RouteMatch::Found { handler, params } => {
                req.params = params;
                handler.handle(req)
            }
            RouteMatch::MethodNotAllowed { allowed } => match &self.method_not_allowed {
                Some(f) => f(&allowed, req),
                None => {
                    let list: Vec<&str> = allowed.iter().map(Method::as_str).collect();
                    Response::new(405).with_header("Allow", list.join(", "))
                }
            },
            RouteMatch::NotFound => match &self.not_found {
                Some(h) => h.handle(req),
                None => Response::new(404),
            },
        }
    }
}

fn parse_pattern(pattern: &str) -> Vec<Segment> {
    let rest = pattern
        .strip_prefix('/')
        .unwrap_or_else(|| panic!("패턴은 /로 시작해야 한다: {pattern:?}"));
    rest.split('/')
        .map(|seg| {
            if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                assert!(
                    !inner.is_empty() && !inner.contains(['{', '}']),
                    "잘못된 경로 변수: {pattern:?}"
                );
                Segment::Param(inner.to_string())
            } else {
                assert!(
                    !seg.contains(['{', '}']),
                    "경로 변수는 세그먼트 전체여야 한다: {pattern:?}"
                );
                Segment::Literal(seg.to_string())
            }
        })
        .collect()
}

/// `/a/b/` → ["a","b",""]. 슬래시로 시작하지 않으면 `None`.
fn split_path(path: &str) -> Option<Vec<&str>> {
    path.strip_prefix('/').map(|rest| rest.split('/').collect())
}

fn match_segments(pattern: &[Segment], parts: &[&str]) -> Option<Vec<(String, String)>> {
    if pattern.len() != parts.len() {
        return None;
    }
    let mut params = Vec::new();
    for (seg, part) in pattern.iter().zip(parts) {
        match seg {
            Segment::Literal(l) if l == part => {}
            Segment::Param(name) if !part.is_empty() => {
                params.push((name.clone(), (*part).to_string()));
            }
            _ => return None,
        }
    }
    Some(params)
}

fn specificity(segments: &[Segment]) -> Vec<u8> {
    segments
        .iter()
        .map(|s| match s {
            Segment::Literal(_) => 0,
            Segment::Param(_) => 1,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(tag: &'static str) -> impl Handler + 'static {
        move |r: Request| {
            let params: Vec<String> = r.params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            Response::new(200).with_body(format!("{tag}[{}]", params.join(",")).into_bytes())
        }
    }

    fn router() -> Router {
        Router::new()
            .route(Method::Get, "/api/v1/posts", ok("list"))
            .route(Method::Post, "/api/v1/posts", ok("create"))
            .route(Method::Get, "/api/v1/posts/{postId}", ok("get"))
            .route(Method::Delete, "/api/v1/posts/{postId}", ok("delete"))
            .route(
                Method::Post,
                "/api/v1/posts/{postId}/likes/{likeId}",
                ok("like"),
            )
            .route(Method::Get, "/api/v1/posts/mine", ok("mine"))
            .route(Method::Get, "/", ok("root"))
    }

    fn call(r: &Router, m: Method, path: &str) -> (u16, String, Option<String>) {
        let resp = r.handle(Request::new(m, path));
        (
            resp.status,
            String::from_utf8(resp.body).unwrap(),
            resp.headers.get("allow").map(str::to_string),
        )
    }

    #[test]
    fn matches_and_extracts_path_variables() {
        let r = router();
        assert_eq!(call(&r, Method::Get, "/api/v1/posts").1, "list[]");
        assert_eq!(
            call(&r, Method::Get, "/api/v1/posts/42").1,
            "get[postId=42]"
        );
        assert_eq!(
            call(&r, Method::Post, "/api/v1/posts/7/likes/9").1,
            "like[postId=7,likeId=9]"
        );
        assert_eq!(call(&r, Method::Get, "/").1, "root[]");
    }

    #[test]
    fn path_variables_are_not_percent_decoded_twice() {
        let r = router();
        assert_eq!(
            call(&r, Method::Get, "/api/v1/posts/a b").1,
            "get[postId=a b]"
        );
        assert_eq!(
            call(&r, Method::Get, "/api/v1/posts/가").1,
            "get[postId=가]"
        );
    }

    #[test]
    fn literal_beats_variable() {
        let r = router();
        assert_eq!(call(&r, Method::Get, "/api/v1/posts/mine").1, "mine[]");
        // mine에는 DELETE가 없지만 {postId}에는 있다: 변수 쪽으로 간다.
        assert_eq!(
            call(&r, Method::Delete, "/api/v1/posts/mine").1,
            "delete[postId=mine]"
        );
    }

    #[test]
    fn trailing_slash_is_a_different_path() {
        // platform__unknown-path.json "공개 목록 경로 끝 슬래시"
        let r = router();
        assert_eq!(call(&r, Method::Get, "/api/v1/posts/").0, 404);
        assert_eq!(call(&r, Method::Get, "/api/v1/posts/42/").0, 404);
        assert_eq!(call(&r, Method::Get, "//").0, 404);
    }

    #[test]
    fn case_matters() {
        // platform__unknown-path.json "대소문자가 다른 경로"
        let r = router();
        assert_eq!(call(&r, Method::Get, "/API/v1/posts").0, 404);
        assert_eq!(call(&r, Method::Get, "/api/v1/Posts").0, 404);
    }

    #[test]
    fn variable_needs_a_non_empty_segment() {
        let r = router();
        assert_eq!(call(&r, Method::Get, "/api/v1/posts//likes").0, 404);
        assert_eq!(call(&r, Method::Post, "/api/v1/posts//likes/1").0, 404);
    }

    #[test]
    fn unknown_paths_are_404() {
        let r = router();
        assert_eq!(call(&r, Method::Get, "/nope").0, 404);
        assert_eq!(call(&r, Method::Get, "/api/v1").0, 404);
        assert_eq!(call(&r, Method::Get, "/api/v1/posts/1/extra").0, 404);
        assert_eq!(call(&r, Method::Get, "no-leading-slash").0, 404);
    }

    #[test]
    fn method_not_allowed_lists_allowed_methods() {
        // platform__method-not-allowed.json "회원 토큰으로 DELETE 공개 경로" -> 405
        let r = router();
        let (status, _, allow) = call(&r, Method::Delete, "/api/v1/posts");
        assert_eq!(status, 405);
        assert_eq!(allow.as_deref(), Some("GET, HEAD, POST"));
        let (status, _, allow) = call(&r, Method::Put, "/api/v1/posts/5");
        assert_eq!(status, 405);
        assert_eq!(allow.as_deref(), Some("DELETE, GET, HEAD"));
        let (status, _, allow) = call(&r, Method::Get, "/api/v1/posts/7/likes/9");
        assert_eq!(status, 405);
        assert_eq!(allow.as_deref(), Some("POST"));
        // 모르는 메서드 토큰도 405
        assert_eq!(
            call(&r, Method::Other("PURGE".into()), "/api/v1/posts").0,
            405
        );
    }

    #[test]
    fn head_falls_back_to_get() {
        // platform__method-not-allowed.json "HEAD 공개 경로"
        let r = router();
        assert_eq!(call(&r, Method::Head, "/api/v1/posts/3").1, "get[postId=3]");
        // GET이 없는 경로의 HEAD는 405
        assert_eq!(call(&r, Method::Head, "/api/v1/posts/1/likes/2").0, 405);
    }

    #[test]
    fn explicit_head_route_wins_over_get() {
        let r =
            Router::new()
                .route(Method::Get, "/x", ok("get"))
                .route(Method::Head, "/x", ok("head"));
        assert_eq!(call(&r, Method::Head, "/x").1, "head[]");
        let (_, _, _) = call(&r, Method::Post, "/x");
        match r.lookup(&Method::Post, "/x") {
            RouteMatch::MethodNotAllowed { allowed } => {
                assert_eq!(allowed, [Method::Get, Method::Head])
            }
            _ => panic!("405 expected"),
        }
    }

    #[test]
    fn lookup_reports_params_in_pattern_order() {
        let r = router();
        match r.lookup(&Method::Post, "/api/v1/posts/7/likes/9") {
            RouteMatch::Found { params, .. } => assert_eq!(
                params,
                [
                    ("postId".to_string(), "7".to_string()),
                    ("likeId".to_string(), "9".to_string())
                ]
            ),
            _ => panic!("found expected"),
        }
        assert!(matches!(
            r.lookup(&Method::Get, "/zzz"),
            RouteMatch::NotFound
        ));
    }

    #[test]
    fn custom_fallbacks() {
        let r = Router::new()
            .route(Method::Get, "/a", ok("a"))
            .not_found(|_r: Request| Response::new(404).with_body(b"custom".to_vec()))
            .method_not_allowed(|allowed: &[Method], _r: Request| {
                Response::new(405).with_body(format!("{}", allowed.len()).into_bytes())
            });
        assert_eq!(call(&r, Method::Get, "/zz"), (404, "custom".into(), None));
        assert_eq!(call(&r, Method::Post, "/a"), (405, "2".into(), None));
    }

    #[test]
    #[should_panic(expected = "중복 라우트")]
    fn duplicate_routes_panic() {
        let _ = Router::new().route(Method::Get, "/a/{x}", ok("1")).route(
            Method::Get,
            "/a/{y}",
            ok("2"),
        );
    }

    #[test]
    #[should_panic(expected = "잘못된 경로 변수")]
    fn empty_variable_name_panics() {
        let _ = Router::new().route(Method::Get, "/a/{}", ok("1"));
    }

    #[test]
    #[should_panic(expected = "세그먼트 전체")]
    fn partial_variable_panics() {
        let _ = Router::new().route(Method::Get, "/a/x{id}", ok("1"));
    }

    #[test]
    #[should_panic(expected = "/로 시작")]
    fn pattern_must_start_with_slash() {
        let _ = Router::new().route(Method::Get, "a", ok("1"));
    }
}
