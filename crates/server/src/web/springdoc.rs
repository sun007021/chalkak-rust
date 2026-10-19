//! springdoc-openapi(OpenAPI 문서와 Swagger UI). 원본은 `!prod` 프로필에서만 켜진다
//! (`SwaggerSecurityConfig`·`application-prod.yml`의 `springdoc.api-docs.enabled=false`).
//!
//! 원본의 응답을 `harness/scripts/collect-springdoc.sh`로 받아 `assets/springdoc/`에 두고 그대로 내보낸다.
//! 문서 안의 서버 주소(`servers[0].url`, `oauth2RedirectUrl`)는 요청의 `Host`로 만들어지므로 파일에는
//! `{{BASE_URL}}`로 적혀 있고 응답할 때 채운다.
//!
//! 원본은 이 경로를 별도 보안 체인(`permitAll`, 토큰 해석 없음)으로 처리한다. 그래서 이 계층은 인증·CORS 계층 앞에 서서
//! 맡은 경로를 직접 끝낸다.
//!
//! | 경로 | 응답 |
//! | --- | --- |
//! | `/v3/api-docs`, `/v3/api-docs/{user-api,admin-api,internal-api}` | `application/json` 문서 |
//! | `/v3/api-docs.yaml` | `application/vnd.oai.openapi` |
//! | `/v3/api-docs/swagger-config` | Swagger UI 설정 |
//! | `/v3/api-docs/{그 밖의 그룹}` | 500 (springdoc이 없는 그룹에서 던지는 예외) |
//! | `/swagger-ui.html` | 302 `/swagger-ui/index.html` |
//! | `/swagger-ui/{파일}` | 정적 파일, 없으면 404 |
//!
//! 구현하지 않은 것: 회원 토큰이 있을 때만 열리는 그룹 YAML(`/v3/api-docs.yaml/{group}`, 원본은 200, 이 서버는 404),
//! `Range`·조건부 요청(`If-Modified-Since`), 참조되지 않는 번들(`swagger-ui.js`,
//! `swagger-ui-es-bundle*.js`), `Last-Modified`(원본은 jar 빌드 시각이라 값을 맞출 수 없다).

use std::sync::Arc;

use chalkak_app::web::{ApiError, marker};
use chalkak_core::log::Logger;
use chalkak_http::{Handler, Method, Middleware, Request, Response};

pub(super) const API_DOCS: &str = include_str!("../../assets/springdoc/api-docs.json");
const API_DOCS_YAML: &str = include_str!("../../assets/springdoc/api-docs.yaml");
const SWAGGER_CONFIG: &str = include_str!("../../assets/springdoc/swagger-config.json");
const GROUPS: [(&str, &str); 3] = [
    (
        "user-api",
        include_str!("../../assets/springdoc/user-api.json"),
    ),
    (
        "admin-api",
        include_str!("../../assets/springdoc/admin-api.json"),
    ),
    (
        "internal-api",
        include_str!("../../assets/springdoc/internal-api.json"),
    ),
];

/// Swagger UI 정적 파일: (이름, `Content-Type`, 내용).
const UI_FILES: [(&str, &str, &[u8]); 9] = [
    (
        "index.html",
        "text/html",
        include_bytes!("../../assets/springdoc/swagger-ui/index.html"),
    ),
    (
        "index.css",
        "text/css",
        include_bytes!("../../assets/springdoc/swagger-ui/index.css"),
    ),
    (
        "swagger-initializer.js",
        "text/javascript",
        include_bytes!("../../assets/springdoc/swagger-ui/swagger-initializer.js"),
    ),
    (
        "oauth2-redirect.html",
        "text/html",
        include_bytes!("../../assets/springdoc/swagger-ui/oauth2-redirect.html"),
    ),
    (
        "swagger-ui.css",
        "text/css",
        include_bytes!("../../assets/springdoc/swagger-ui/swagger-ui.css"),
    ),
    (
        "swagger-ui-bundle.js",
        "text/javascript",
        include_bytes!("../../assets/springdoc/swagger-ui/swagger-ui-bundle.js"),
    ),
    (
        "swagger-ui-standalone-preset.js",
        "text/javascript",
        include_bytes!("../../assets/springdoc/swagger-ui/swagger-ui-standalone-preset.js"),
    ),
    (
        "favicon-32x32.png",
        "image/png",
        include_bytes!("../../assets/springdoc/swagger-ui/favicon-32x32.png"),
    ),
    (
        "favicon-16x16.png",
        "image/png",
        include_bytes!("../../assets/springdoc/swagger-ui/favicon-16x16.png"),
    ),
];

const BASE_PLACEHOLDER: &str = "{{BASE_URL}}";

pub struct Springdoc {
    enabled: bool,
    logger: Arc<Logger>,
}

impl Springdoc {
    /// `enabled`는 `!prod`일 때 참이다.
    pub fn new(enabled: bool, logger: Arc<Logger>) -> Self {
        Springdoc { enabled, logger }
    }
}

/// 보안 체인이 `permitAll`로 맡는 경로(`SPRINGDOC_PATHS`, Ant 패턴).
fn is_springdoc_path(path: &str) -> bool {
    path == "/swagger-ui.html"
        || path == "/v3/api-docs"
        || path == "/v3/api-docs.yaml"
        || under(path, "/swagger-ui")
        || under(path, "/v3/api-docs")
}

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
}

/// 요청이 가리킨 서버 주소(`http://host:port`).
fn base_url(req: &Request) -> String {
    format!("http://{}", req.header("host").unwrap_or("localhost"))
}

fn text(content_type: &str, body: &str, base: &str) -> Response {
    let origin = match content_type {
        "application/json" => marker::DOCS_JSON,
        _ => marker::DOCS_YAML,
    };
    Response::new(200)
        .with_header("Content-Type", content_type)
        .with_header(marker::ORIGIN, origin)
        .with_body(body.replace(BASE_PLACEHOLDER, base).into_bytes())
}

/// 본문을 `Content-Type`과 함께 내보낸다. 문서·설정은 컨트롤러가 쓰므로 `application/json`이면 협상 계층이 다룬다.
fn api_docs(path: &str, base: &str) -> Option<Response> {
    Some(match path {
        "/v3/api-docs" => text("application/json", API_DOCS, base),
        "/v3/api-docs.yaml" => text("application/vnd.oai.openapi", API_DOCS_YAML, base),
        "/v3/api-docs/swagger-config" => text("application/json", SWAGGER_CONFIG, base),
        _ => {
            let group = path.strip_prefix("/v3/api-docs/")?;
            let (_, body) = GROUPS.iter().find(|(name, _)| *name == group)?;
            text("application/json", body, base)
        }
    })
}

/// `/swagger-ui/**` 정적 파일. 끝 슬래시는 무시한다(리소스 핸들러가 경로를 정리한다).
fn ui_file(path: &str) -> Option<Response> {
    let name = path.strip_prefix("/swagger-ui/")?.trim_end_matches('/');
    let (_, content_type, bytes) = UI_FILES.iter().find(|(n, _, _)| *n == name)?;
    let mut response = Response::new(200)
        .with_header("Content-Type", *content_type)
        .with_header("Accept-Ranges", "bytes")
        .with_body(*bytes);
    if path == "/swagger-ui/index.html" || path == "/swagger-ui/swagger-initializer.js" {
        // springdoc이 설정을 채워 직접 쓰는 파일: 캐시 금지 한 줄만 있고 보안 헤더의 캐시 헤더는 붙지 않는다.
        // (끝 슬래시를 붙인 경로는 평범한 정적 리소스라 해당하지 않는다.)
        response = response.with_header("Cache-Control", "no-store");
    }
    Some(response)
}

impl Springdoc {
    fn respond(&self, req: &Request) -> Response {
        let path = req.path.as_str();
        let base = base_url(req);
        let get_like = matches!(req.method, Method::Get | Method::Head);
        let swagger_ui_tree = under(path, "/swagger-ui") || path == "/swagger-ui.html";

        // `/swagger-ui.html`은 리다이렉트 컨트롤러다(GET·HEAD만).
        if path == "/swagger-ui.html" {
            return match req.method {
                Method::Get | Method::Head => {
                    // 원본은 쿼리 문자열을 그대로 이어 붙인다(`?x=1` -> `/swagger-ui/index.html?x=1`).
                    let mut location = String::from("/swagger-ui/index.html");
                    if !req.raw_query.is_empty() {
                        location.push('?');
                        location.push_str(&req.raw_query);
                    }
                    Response::new(302).with_header("Location", location)
                }
                Method::Options => options(),
                _ => self.method_not_supported("GET, HEAD"),
            };
        }
        // `/swagger-ui/**`는 정적 리소스 핸들러다. 리소스가 없으면 메서드와 무관하게 404이고,
        // 있으면 GET·HEAD·OPTIONS만 받는다(리소스를 찾은 뒤에 메서드를 검사한다).
        if swagger_ui_tree {
            let Some(found) = ui_file(path) else {
                return ApiError::NotFound.into_response(&self.logger);
            };
            return match req.method {
                Method::Get | Method::Head => found,
                Method::Options => options(),
                _ if found.headers.get("content-type") == Some("text/html") => self
                    .method_not_supported("GET, HEAD")
                    .with_header(marker::HTML_RESOURCE, "1"),
                _ => self.method_not_supported("GET, HEAD"),
            };
        }

        // `/v3/api-docs` 아래는 컨트롤러 매핑이다. 그룹 이름 한 칸만 매핑되고 나머지는 404다.
        let known = api_docs(path, &base);
        let group_path = path
            .strip_prefix("/v3/api-docs/")
            .is_some_and(|g| !g.is_empty() && !g.contains('/'));
        match (known, get_like) {
            (Some(response), true) => response,
            (Some(_), false) if req.method == Method::Options => options(),
            (Some(_), false) => self.method_not_supported("GET, HEAD"),
            (None, _) if group_path && path != "/v3/api-docs.yaml" => {
                // springdoc이 없는 그룹 문서를 만들다 던지는 예외 -> 처리되지 않은 예외로 500
                if req.method == Method::Options {
                    return options();
                }
                if !get_like {
                    return self.method_not_supported("GET, HEAD");
                }
                let cause = "존재하지 않는 OpenAPI 그룹입니다.";
                ApiError::Internal(cause.to_owned()).into_response(&self.logger)
            }
            (None, _) => ApiError::NotFound.into_response(&self.logger),
        }
    }

    fn method_not_supported(&self, allow: &str) -> Response {
        ApiError::MethodNotSupported
            .into_response(&self.logger)
            .with_header(marker::CARRY_ALLOW, allow)
    }
}

/// `Allow: GET,HEAD,OPTIONS` + 빈 `Accept-Patch`(Spring `HttpOptionsHandler`).
fn options() -> Response {
    Response::new(200)
        .with_header("Allow", "GET,HEAD,OPTIONS")
        .with_header("Accept-Patch", "")
}

impl Middleware for Springdoc {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        if self.enabled && is_springdoc_path(&req.path) {
            return self.respond(&req);
        }
        next.handle(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::{FixedClock, log::Level};

    fn springdoc(enabled: bool) -> Springdoc {
        let logger = Arc::new(Logger::new(
            Box::new(std::io::sink()),
            Level::Error,
            Arc::new(FixedClock::new("2026-10-13T00:00:00Z".parse().unwrap())),
        ));
        Springdoc::new(enabled, logger)
    }

    fn call(s: &Springdoc, method: Method, path: &str) -> Response {
        let mut req = Request::new(method, path);
        req.headers.append("Host", "docs.test:8080");
        s.handle(req, &|_: Request| Response::new(418))
    }

    // java-3: GET /v3/api-docs -> 200 application/json, servers[0].url은 요청의 Host
    #[test]
    fn 문서의_서버_주소는_요청의_host로_채운다() {
        let r = call(&springdoc(true), Method::Get, "/v3/api-docs");
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("content-type"), Some("application/json"));
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains(
            r#""servers":[{"url":"http://docs.test:8080","description":"Generated server url"}]"#
        ));
        assert!(!body.contains(BASE_PLACEHOLDER));
        let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(doc["openapi"], "3.1.0");
    }

    #[test]
    fn 그룹_문서와_설정과_yaml() {
        let s = springdoc(true);
        for group in ["user-api", "admin-api", "internal-api"] {
            let r = call(&s, Method::Get, &format!("/v3/api-docs/{group}"));
            assert_eq!(r.status, 200, "{group}");
            assert_eq!(r.headers.get("content-type"), Some("application/json"));
        }
        let r = call(&s, Method::Get, "/v3/api-docs/swagger-config");
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains(
            r#""oauth2RedirectUrl":"http://docs.test:8080/swagger-ui/oauth2-redirect.html""#
        ));
        let r = call(&s, Method::Get, "/v3/api-docs.yaml");
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/vnd.oai.openapi")
        );
        assert!(
            String::from_utf8(r.body)
                .unwrap()
                .starts_with("openapi: 3.1.0")
        );
    }

    // AdminAnalyticsOpenApiTest#adminApiDocs_aggregationEndpoint_exposesContract
    #[test]
    fn admin_api_문서에_수동_집계_계약을_노출하고_user_api에서는_제외한다() {
        let s = springdoc(true);
        let json = |group: &str| -> serde_json::Value {
            let r = call(&s, Method::Get, &format!("/v3/api-docs/{group}"));
            assert_eq!(r.status, 200, "{group}");
            serde_json::from_slice(&r.body).unwrap()
        };
        let admin = json("admin-api");
        let op = &admin["paths"]["/api/v1/admin/analytics/aggregations"]["post"];
        assert!(op["security"][0].get("adminAccessToken").is_some());
        assert_eq!(
            op["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/AdminUserMetricsAggregationRequest"
        );
        let request = &admin["components"]["schemas"]["AdminUserMetricsAggregationRequest"];
        assert_eq!(request["required"], serde_json::json!(["metricDate"]));
        assert_eq!(request["properties"]["metricDate"]["format"], "date");
        assert!(op["responses"]["202"].is_object());
        assert!(op["responses"]["202"].get("content").is_none());
        assert!(op["responses"]["400"].is_object());
        assert!(op["responses"]["403"].is_object());
        assert!(
            json("user-api")["paths"]
                .get("/api/v1/admin/analytics/aggregations")
                .is_none()
        );
    }

    // java-3: 없는 그룹 -> 500, /v3/api-docs/ -> 404, 그룹 문서에 POST -> 405
    #[test]
    fn 없는_그룹은_500이고_빈_그룹은_404다() {
        let s = springdoc(true);
        assert_eq!(call(&s, Method::Get, "/v3/api-docs/nope").status, 500);
        assert_eq!(call(&s, Method::Get, "/v3/api-docs/").status, 404);
        assert_eq!(call(&s, Method::Get, "/v3/api-docs/a/b").status, 404);
        assert_eq!(call(&s, Method::Post, "/v3/api-docs").status, 405);
        assert_eq!(
            call(&s, Method::Post, "/v3/api-docs/swagger-config").status,
            405
        );
    }

    // java-3: /swagger-ui.html -> 302, /swagger-ui/index.html -> 200 Cache-Control: no-store
    #[test]
    fn swagger_ui는_리다이렉트와_정적_파일이다() {
        let s = springdoc(true);
        let r = call(&s, Method::Get, "/swagger-ui.html");
        assert_eq!(
            (r.status, r.headers.get("location")),
            (302, Some("/swagger-ui/index.html"))
        );
        let r = call(&s, Method::Get, "/swagger-ui/index.html");
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("content-type"), Some("text/html"));
        assert_eq!(r.headers.get("cache-control"), Some("no-store"));
        assert_eq!(call(&s, Method::Get, "/swagger-ui/index.html/").status, 200);
        let css = call(&s, Method::Get, "/swagger-ui/swagger-ui.css");
        assert_eq!(css.headers.get("content-type"), Some("text/css"));
        assert_eq!(css.body.len(), 178977);
        assert_eq!(call(&s, Method::Get, "/swagger-ui/nope.js").status, 404);
        assert_eq!(call(&s, Method::Get, "/swagger-ui/").status, 404);
        assert_eq!(
            call(&s, Method::Delete, "/swagger-ui/index.html").status,
            405
        );
        assert_eq!(call(&s, Method::Post, "/swagger-ui.html").status, 405);
        let r = call(&s, Method::Options, "/swagger-ui/index.html");
        assert_eq!(
            (r.status, r.headers.get("allow")),
            (200, Some("GET,HEAD,OPTIONS"))
        );
    }

    // java-3: /swagger-ui.html?x=1 -> Location: /swagger-ui/index.html?x=1
    #[test]
    fn 리다이렉트는_쿼리를_이어_붙인다() {
        let mut req = Request::new(Method::Get, "/swagger-ui.html");
        req.raw_query = "x=%EA&y=1".into();
        let r = springdoc(true).handle(req, &|_: Request| Response::new(418));
        assert_eq!(
            r.headers.get("location"),
            Some("/swagger-ui/index.html?x=%EA&y=1")
        );
    }

    // prod: 원본에는 이 설정이 없어 인증 규칙으로 넘어간다
    #[test]
    fn 꺼져_있으면_다음_계층으로_넘긴다() {
        let r = call(&springdoc(false), Method::Get, "/v3/api-docs");
        assert_eq!(r.status, 418);
        let r = call(&springdoc(true), Method::Get, "/api/v1/topics");
        assert_eq!(r.status, 418);
        // 보안 체인이 맡지 않는 변형은 넘긴다
        assert_eq!(
            call(&springdoc(true), Method::Get, "/swagger-ui.html/").status,
            418
        );
        assert_eq!(
            call(&springdoc(true), Method::Get, "/v3/api-docs.yaml/").status,
            418
        );
    }
}
