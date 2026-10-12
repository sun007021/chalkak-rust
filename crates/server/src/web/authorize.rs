//! 경로별 인가. 원본 `SecurityConfig`의 `authorizeHttpRequests` 규칙을 선언 순서대로 옮겼다(먼저 맞는 규칙이 이긴다).
//!
//! 1. `/api/v1/auth/**` 공개.
//! 2. `/api/v1/admin/auth/{login,refresh,logout}` 공개(메서드 무관).
//! 3. `/internal/v1/**` 공개(콜백이 HMAC으로 자체 인증한다).
//! 4. `/actuator/health`, `/actuator/info` 공개.
//! 5. `/error` 공개.
//! 6. 개발 관리자 우회가 켜져 있으면 `/api/v1/admin/**` 공개.
//! 7. `/api/v1/admin/**`은 `SCOPE_ADMIN`.
//! 8. GET `/api/v1/topics`, `/api/v1/posts`(정확히 이 경로)는 익명이거나 `SCOPE_USER`.
//! 9. 나머지는 모두 `SCOPE_USER`. 새 API에 인증을 붙이는 것을 잊어도 기본값이 차단이 되도록.
//!
//! 거부하면 익명은 401(`WWW-Authenticate: Bearer`), 인증된 주체는 403이다. 경로 규칙은 끝 슬래시와 대소문자를 구분한다.

use std::sync::Arc;

use chalkak_app::security::{self, AUTHORITY_ADMIN, AUTHORITY_USER, Principal};
use chalkak_app::web::context;
use chalkak_core::error::{ErrorCode, error_body};
use chalkak_core::log::{Logger, fields};
use chalkak_http::{Handler, Method, Middleware, Request, Response};

use super::bearer;

const FORBIDDEN_LOGGER: &str = "com.chalkak.backend.auth.api.support.ForbiddenAccessDeniedHandler";
const FORBIDDEN_MESSAGE: &str = "접근 권한이 없습니다.";

const PUBLIC_ADMIN_AUTH_PATHS: [&str; 3] = [
    "/api/v1/admin/auth/login",
    "/api/v1/admin/auth/refresh",
    "/api/v1/admin/auth/logout",
];
const PUBLIC_GET_PATHS: [&str; 2] = ["/api/v1/topics", "/api/v1/posts"];

#[derive(Debug, PartialEq, Eq)]
pub enum Access {
    Allow,
    Deny,
}

/// `prefix` 자신이거나 그 아래(`prefix/**`).
fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
}

pub fn decide(
    method: &Method,
    path: &str,
    principal: Option<&Principal>,
    admin_development_bypass: bool,
) -> Access {
    let allow_if = |ok: bool| if ok { Access::Allow } else { Access::Deny };
    let has = |authority: &str| principal.is_some_and(|p| p.has_authority(authority));

    if under(path, "/api/v1/auth")
        || PUBLIC_ADMIN_AUTH_PATHS.contains(&path)
        || under(path, "/internal/v1")
        || path == "/actuator/health"
        || path == "/actuator/info"
        || path == "/error"
    {
        return Access::Allow;
    }
    if under(path, "/api/v1/admin") {
        return allow_if(admin_development_bypass || has(AUTHORITY_ADMIN));
    }
    if *method == Method::Get && PUBLIC_GET_PATHS.contains(&path) {
        return allow_if(principal.is_none() || has(AUTHORITY_USER));
    }
    allow_if(has(AUTHORITY_USER))
}

pub struct Authorize {
    admin_development_bypass: bool,
    logger: Arc<Logger>,
}

impl Authorize {
    pub fn new(admin_development_bypass: bool, logger: Arc<Logger>) -> Self {
        Authorize {
            admin_development_bypass,
            logger,
        }
    }
}

/// `ForbiddenAccessDeniedHandler`
fn forbidden(logger: &Logger) -> Response {
    logger
        .warn(FORBIDDEN_LOGGER)
        .field(fields::TYPE, fields::TYPE_ERROR)
        .field(fields::ERROR_CODE, ErrorCode::FORBIDDEN.as_str())
        .field(fields::STATUS, 403)
        .field(fields::EXCEPTION, "AuthorizationDeniedException")
        .log("접근 거부");
    Response::new(403)
        .with_header("Content-Type", "application/json;charset=UTF-8")
        .with_body(error_body(ErrorCode::FORBIDDEN, FORBIDDEN_MESSAGE).into_bytes())
}

impl Middleware for Authorize {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        let principal = security::current();
        match decide(
            &req.method,
            &req.path,
            principal.as_ref(),
            self.admin_development_bypass,
        ) {
            Access::Deny if principal.is_none() => {
                bearer::unauthorized(&self.logger, "InsufficientAuthenticationException")
            }
            Access::Deny => forbidden(&self.logger),
            Access::Allow => {
                let response = next.handle(req);
                // 원본 AccessLogUserInterceptor는 핸들러가 매핑된 요청에서만, 회원 토큰일 때만 userId를 남긴다.
                // 메서드가 없어 405로 끝난 요청에는 핸들러가 없다.
                if response.status != 405
                    && let Some(user_id) = principal.as_ref().and_then(access_log_user_id)
                {
                    context::set_user_id(user_id);
                }
                response
            }
        }
    }
}

/// 회원 토큰의 `sub`를 `UUID.fromString(sub).toString()` 모양으로.
fn access_log_user_id(principal: &Principal) -> Option<String> {
    if !principal.has_authority(AUTHORITY_USER) {
        return None;
    }
    chalkak_app::common::java_uuid_from_string(&principal.subject).map(|id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Principal {
        Principal {
            subject: "01990000-0000-7000-8000-000000000001".into(),
            authorities: vec![AUTHORITY_USER.into()],
        }
    }

    fn admin() -> Principal {
        Principal {
            subject: "01990000-0000-7000-8000-0000000000a1".into(),
            authorities: vec![AUTHORITY_ADMIN.into()],
        }
    }

    fn no_scope() -> Principal {
        Principal {
            subject: "01990000-0000-7000-8000-000000000001".into(),
            authorities: vec![],
        }
    }

    fn check(method: Method, path: &str, p: Option<&Principal>) -> Access {
        decide(&method, path, p, false)
    }

    // SecurityFilterChainTest, 하네스 시나리오 platform/auth-matrix
    #[test]
    fn 공개_경로는_누구나_통과한다() {
        for path in [
            "/api/v1/auth/login",
            "/api/v1/auth",
            "/api/v1/auth/social/google/login",
            "/api/v1/admin/auth/login",
            "/api/v1/admin/auth/refresh",
            "/api/v1/admin/auth/logout",
            "/internal/v1/post-images/callback",
            "/actuator/health",
            "/actuator/info",
            "/error",
        ] {
            for method in [Method::Get, Method::Post, Method::Delete] {
                assert_eq!(check(method, path, None), Access::Allow, "{path}");
            }
        }
    }

    #[test]
    fn 공개_목록은_익명과_회원만_통과한다() {
        for path in ["/api/v1/topics", "/api/v1/posts"] {
            assert_eq!(check(Method::Get, path, None), Access::Allow);
            assert_eq!(check(Method::Get, path, Some(&user())), Access::Allow);
            assert_eq!(check(Method::Get, path, Some(&admin())), Access::Deny);
            assert_eq!(check(Method::Get, path, Some(&no_scope())), Access::Deny);
            // GET이 아니면 공개가 아니다. HEAD, OPTIONS 포함.
            for method in [Method::Head, Method::Options, Method::Delete, Method::Post] {
                assert_eq!(
                    check(method.clone(), path, None),
                    Access::Deny,
                    "{path} {method}"
                );
                assert_eq!(check(method, path, Some(&user())), Access::Allow);
            }
        }
    }

    #[test]
    fn 공개_목록의_끝_슬래시와_대소문자와_하위_경로는_공개가_아니다() {
        for path in [
            "/api/v1/topics/",
            "/api/v1/Topics",
            "/api/v1/posts/1",
            "/api/v1/posts/",
        ] {
            assert_eq!(check(Method::Get, path, None), Access::Deny, "{path}");
        }
    }

    #[test]
    fn 나머지는_회원_토큰이어야_한다() {
        for path in [
            "/api/v1/users/me",
            "/actuator/health/liveness",
            "/actuator",
            "/nope",
        ] {
            assert_eq!(check(Method::Get, path, None), Access::Deny, "{path}");
            assert_eq!(
                check(Method::Get, path, Some(&user())),
                Access::Allow,
                "{path}"
            );
            assert_eq!(
                check(Method::Get, path, Some(&admin())),
                Access::Deny,
                "{path}"
            );
        }
        assert_eq!(check(Method::Get, "/actuator/health/", None), Access::Deny);
    }

    #[test]
    fn 관리자_경로는_관리자_토큰이거나_우회가_켜져_있어야_한다() {
        for path in [
            "/api/v1/admin",
            "/api/v1/admin/feedbacks",
            "/api/v1/admin/auth/other",
        ] {
            assert_eq!(check(Method::Get, path, None), Access::Deny, "{path}");
            assert_eq!(
                check(Method::Get, path, Some(&user())),
                Access::Deny,
                "{path}"
            );
            assert_eq!(
                check(Method::Get, path, Some(&admin())),
                Access::Allow,
                "{path}"
            );
            assert_eq!(
                decide(&Method::Get, path, None, true),
                Access::Allow,
                "{path}"
            );
        }
        // 비슷한 이름은 관리자 경로가 아니라 일반 규칙(회원 토큰)을 따른다.
        assert_eq!(
            check(Method::Get, "/api/v1/administrator", Some(&user())),
            Access::Allow
        );
    }

    #[test]
    fn 거부는_익명이면_401_인증됐으면_403이다() {
        let logger = Arc::new(Logger::new(
            Box::new(std::io::sink()),
            chalkak_core::log::Level::Error,
            Arc::new(chalkak_core::FixedClock::new(
                "2026-10-12T00:00:00Z".parse().unwrap(),
            )),
        ));
        let mw = Authorize::new(false, logger);
        let ok = |_: Request| Response::new(200);
        let r = mw.handle(Request::new(Method::Get, "/api/v1/users/me"), &ok);
        assert_eq!(r.status, 401);
        assert_eq!(r.headers.get("www-authenticate"), Some("Bearer"));

        let _guard = security::enter(admin());
        let r = mw.handle(Request::new(Method::Get, "/api/v1/users/me"), &ok);
        assert_eq!(r.status, 403);
        assert!(!r.headers.contains("www-authenticate"));
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"FORBIDDEN","message":"접근 권한이 없습니다."}"#
        );
    }

    // AccessLogTest#accessLog_memberRequest_logsUserId: 관리자 토큰의 sub는 남기지 않는다.
    #[test]
    fn 회원_토큰일_때만_접근_로그에_user_id를_올린다() {
        let logger = Arc::new(Logger::new(
            Box::new(std::io::sink()),
            chalkak_core::log::Level::Error,
            Arc::new(chalkak_core::FixedClock::new(
                "2026-10-12T00:00:00Z".parse().unwrap(),
            )),
        ));
        let mw = Authorize::new(true, logger);
        let ok = |_: Request| Response::new(200);

        let scope = context::begin();
        {
            let _guard = security::enter(user());
            mw.handle(Request::new(Method::Get, "/api/v1/topics"), &ok);
        }
        assert_eq!(
            scope.info().user_id.as_deref(),
            Some("01990000-0000-7000-8000-000000000001")
        );

        let scope = context::begin();
        {
            let _guard = security::enter(admin());
            mw.handle(Request::new(Method::Get, "/api/v1/admin/x"), &ok);
        }
        assert_eq!(scope.info().user_id, None);

        let scope = context::begin();
        {
            let _guard = security::enter(user());
            let not_allowed = |_: Request| Response::new(405);
            mw.handle(Request::new(Method::Delete, "/api/v1/topics"), &not_allowed);
        }
        assert_eq!(
            scope.info().user_id,
            None,
            "핸들러가 없는 405에는 남기지 않는다"
        );
    }
}
