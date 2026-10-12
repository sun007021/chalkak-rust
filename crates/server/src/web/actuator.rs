//! Spring Boot Actuator(`health`, `info`만 노출) 모양의 엔드포인트와 컨테이너 오류 페이지(`/error`).
//!
//! java-a에 직접 보내 확인한 응답이다. 본문 타입은 `application/vnd.spring-boot.actuator.v3+json`.
//! - `/actuator/health`: `{"groups":["liveness","readiness"],"status":"UP"}`. DB에 닿지 않으면 503 `DOWN`.
//! - `/actuator/health/liveness`, `/actuator/health/readiness`: `{"status":"UP"}`.
//! - `/actuator/info`: `{}`.
//! - `/actuator`: `_links` 목록(요청의 Host로 만든 절대 주소).
//! - 그 밖의 `/actuator/health/*`: 본문 없는 404. 다른 `/actuator/*`은 일반 JSON 404로 넘어간다.
//! - GET·HEAD가 아닌 메서드는 405 JSON.
//!
//! 끝 슬래시는 무시한다. 공개 경로는 `/actuator/health`, `/actuator/info`뿐이라 나머지는 인가 단계에서 회원 토큰을 요구한다.
//!
//! `/error`는 모든 메서드에서 500, 본문 `{"timestamp","status":999,"error":"None"}`이고 연결을 닫는다.

use std::sync::Arc;

use chalkak_app::web::ApiError;
use chalkak_core::Clock;
use chalkak_core::log::Logger;
use chalkak_db::Pool;
use chalkak_http::{Method, Request, Response};

use super::boot;

const ACTUATOR_CONTENT_TYPE: &str = "application/vnd.spring-boot.actuator.v3+json";

pub struct Actuator {
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
}

pub fn is_actuator_path(path: &str) -> bool {
    path == "/actuator" || path.starts_with("/actuator/")
}

impl Actuator {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>, logger: Arc<Logger>) -> Self {
        Actuator {
            pool,
            clock,
            logger,
        }
    }

    /// actuator가 맡지 않는 경로면 `None`.
    pub fn handle(&self, req: &Request) -> Option<Response> {
        let path = req.path.strip_suffix('/').unwrap_or(&req.path);
        let known = matches!(
            path,
            "/actuator"
                | "/actuator/health"
                | "/actuator/health/liveness"
                | "/actuator/health/readiness"
                | "/actuator/info"
        );
        if !known && !path.starts_with("/actuator/health/") {
            return None;
        }
        if !matches!(req.method, Method::Get | Method::Head) {
            return Some(ApiError::MethodNotSupported.into_response(&self.logger));
        }
        Some(match path {
            "/actuator" => json(200, links(req)),
            "/actuator/health" => self.health(),
            "/actuator/health/liveness" | "/actuator/health/readiness" => {
                json(200, r#"{"status":"UP"}"#.to_owned())
            }
            "/actuator/info" => json(200, "{}".to_owned()),
            _ => Response::new(404),
        })
    }

    fn health(&self) -> Response {
        let up = self.database_is_up();
        let status = if up { "UP" } else { "DOWN" };
        json(
            if up { 200 } else { 503 },
            format!(r#"{{"groups":["liveness","readiness"],"status":"{status}"}}"#),
        )
    }

    /// 풀에서 연결을 빌려 `SELECT 1`이 되는지 본다(`DataSourceHealthIndicator`).
    fn database_is_up(&self) -> bool {
        match self.pool.get() {
            Ok(mut conn) => conn.simple_query("SELECT 1").is_ok(),
            Err(_) => false,
        }
    }

    /// `/error`
    pub fn error_page(&self) -> Response {
        let body = boot::body(self.clock.now(), 999, "None", None);
        boot::closing(
            Response::new(500)
                .with_header("Content-Type", "application/json")
                .with_body(body.into_bytes()),
        )
    }
}

fn json(status: u16, body: String) -> Response {
    Response::new(status)
        .with_header("Content-Type", ACTUATOR_CONTENT_TYPE)
        .with_body(body.into_bytes())
}

/// Spring Boot가 요청 URL로 만드는 링크 페이지.
fn links(req: &Request) -> String {
    let host = req.header("host").unwrap_or("localhost");
    let base = format!("http://{host}/actuator");
    format!(
        concat!(
            r#"{{"_links":{{"self":{{"href":"{base}","templated":false}},"#,
            r#""info":{{"href":"{base}/info","templated":false}},"#,
            r#""health":{{"href":"{base}/health","templated":false}},"#,
            r#""health-path":{{"href":"{base}/health/{{*path}}","templated":true}}}}}}"#
        ),
        base = base
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 링크_본문은_원본과_같다() {
        let mut req = Request::new(Method::Get, "/actuator");
        req.headers.append("Host", "127.0.0.1:18080");
        assert_eq!(
            links(&req),
            r#"{"_links":{"self":{"href":"http://127.0.0.1:18080/actuator","templated":false},"info":{"href":"http://127.0.0.1:18080/actuator/info","templated":false},"health":{"href":"http://127.0.0.1:18080/actuator/health","templated":false},"health-path":{"href":"http://127.0.0.1:18080/actuator/health/{*path}","templated":true}}}"#
        );
    }

    #[test]
    fn actuator_경로_판정() {
        assert!(is_actuator_path("/actuator"));
        assert!(is_actuator_path("/actuator/health"));
        assert!(!is_actuator_path("/actuatorx"));
        assert!(!is_actuator_path("/api/actuator"));
    }
}
