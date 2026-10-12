//! Bearer 토큰 인증. 원본 Spring Security `BearerTokenAuthenticationFilter`와 `JwtAuthenticationProvider`.
//!
//! 규칙(java-a에 직접 보내 확인):
//! - `Authorization` 헤더가 없거나 `bearer`로 시작하지 않으면(Basic 등) 익명이다. 쿼리의 `access_token`도 무시한다.
//! - `bearer`로 시작하는데 `^Bearer (?<token>[a-zA-Z0-9-._~+/]+=*)$`(대소문자 무시)에 맞지 않으면 401.
//! - 토큰이 서명·클레임 검증을 통과하지 못하면 401. **공개 경로에서도** 마찬가지다(토큰을 보냈으면 검증한다).
//! - 단 `aud`가 없거나 null인 토큰은 원본 디코더가 `NullPointerException`을 던져 401이 아니라 500(Boot 오류 본문,
//!   `Connection: close`)으로 끝난다. 빈 `aud` 배열은 401이다.
//! - 권한은 `scope` 클레임(없으면 `scp`)에서 만든다: 문자열이면 공백으로 나누고, 컬렉션이면 원소마다, 그 밖의 타입이면 없음.
//!   각 값 앞에 `SCOPE_`를 붙인다.

use std::sync::Arc;

use chalkak_app::security::{self, Principal};
use chalkak_core::Clock;
use chalkak_core::error::{ErrorCode, error_body};
use chalkak_core::log::{Logger, fields};
use chalkak_http::{Handler, Middleware, Request, Response};
use chalkak_jwt::{ClaimError, Claims, Validator, hs256_decode};
use serde_json::Value;

use super::boot;

const UNAUTHORIZED_ENTRY_POINT_LOGGER: &str =
    "com.chalkak.backend.auth.api.support.UnauthorizedEntryPoint";
const UNAUTHORIZED_MESSAGE: &str = "유효하지 않은 인증 정보입니다.";

pub struct BearerAuth {
    key: Vec<u8>,
    validator: Validator,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
}

/// 인증 단계의 결과.
#[derive(Debug, PartialEq)]
enum Outcome {
    Anonymous,
    Authenticated(Principal),
    /// 401. 값은 로그의 `exception`(원본 예외 클래스 이름).
    Unauthorized(&'static str),
    /// `aud` 누락으로 원본이 NPE를 내는 경우.
    ServerError,
}

impl BearerAuth {
    pub fn new(
        key: Vec<u8>,
        issuer: &str,
        audience: &str,
        clock: Arc<dyn Clock>,
        logger: Arc<Logger>,
    ) -> Self {
        BearerAuth {
            key,
            validator: Validator::access_token(issuer, audience),
            clock,
            logger,
        }
    }

    fn authenticate(&self, req: &Request) -> Outcome {
        let token = match resolve_token(req.header("authorization")) {
            TokenHeader::Absent => return Outcome::Anonymous,
            TokenHeader::Malformed => {
                return Outcome::Unauthorized("OAuth2AuthenticationException");
            }
            TokenHeader::Token(token) => token,
        };
        let Ok(claims) = hs256_decode(token, &self.key) else {
            return Outcome::Unauthorized("InvalidBearerTokenException");
        };
        let now_ms = self.clock.now().as_millisecond();
        match self.validator.validate(&claims, now_ms) {
            Ok(()) => {}
            Err(ClaimError::AudienceMissing) => return Outcome::ServerError,
            Err(_) => return Outcome::Unauthorized("InvalidBearerTokenException"),
        }
        let Some(subject) = claims.subject() else {
            return Outcome::Unauthorized("InvalidBearerTokenException");
        };
        Outcome::Authenticated(Principal {
            subject: subject.to_owned(),
            authorities: authorities(&claims),
        })
    }
}

impl Middleware for BearerAuth {
    fn handle(&self, req: Request, next: &dyn Handler) -> Response {
        match self.authenticate(&req) {
            Outcome::Anonymous => next.handle(req),
            Outcome::Authenticated(principal) => {
                let _guard = security::enter(principal);
                next.handle(req)
            }
            Outcome::Unauthorized(exception) => unauthorized(&self.logger, exception),
            Outcome::ServerError => {
                // 원본은 컨테이너가 예외를 로그로 남기고 오류 페이지(Boot 본문)로 답한다.
                self.logger
                    .error("org.apache.catalina.core.ContainerBase.[Tomcat].[localhost].[/].[dispatcherServlet]")
                    .field(fields::TYPE, fields::TYPE_ERROR)
                    .field(fields::STATUS, 500)
                    .field(fields::EXCEPTION, "NullPointerException")
                    .log("처리되지 않은 예외");
                boot::closing(boot::error_response(
                    &*self.clock,
                    500,
                    "Internal Server Error",
                    &req.path,
                ))
            }
        }
    }
}

/// `UnauthorizedEntryPoint`: 401 + `WWW-Authenticate: Bearer` + 오류 본문, 경고 로그 한 줄.
pub fn unauthorized(logger: &Logger, exception: &str) -> Response {
    logger
        .warn(UNAUTHORIZED_ENTRY_POINT_LOGGER)
        .field(fields::TYPE, fields::TYPE_ERROR)
        .field(fields::ERROR_CODE, ErrorCode::UNAUTHORIZED.as_str())
        .field(fields::STATUS, 401)
        .field(fields::EXCEPTION, exception)
        .log("인증 실패");
    Response::new(401)
        .with_header("WWW-Authenticate", "Bearer")
        .with_header("Content-Type", "application/json;charset=UTF-8")
        .with_body(error_body(ErrorCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_bytes())
}

#[derive(Debug, PartialEq)]
enum TokenHeader<'a> {
    Absent,
    Malformed,
    Token(&'a str),
}

/// `DefaultBearerTokenResolver`.
fn resolve_token(header: Option<&str>) -> TokenHeader<'_> {
    let Some(header) = header else {
        return TokenHeader::Absent;
    };
    let starts_with_bearer = header
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("bearer"));
    if !starts_with_bearer {
        return TokenHeader::Absent;
    }
    let token = match header.get(6..).and_then(|rest| rest.strip_prefix(' ')) {
        Some(token) => token,
        None => return TokenHeader::Malformed,
    };
    let body_len = token
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(b))
        .count();
    let padding = token[body_len..].bytes().take_while(|&b| b == b'=').count();
    if body_len == 0 || body_len + padding != token.len() {
        return TokenHeader::Malformed;
    }
    TokenHeader::Token(token)
}

/// `JwtGrantedAuthoritiesConverter`(접두사 `SCOPE_`, 클레임 이름 `scope` -> `scp`).
fn authorities(claims: &Claims) -> Vec<String> {
    let claim = ["scope", "scp"]
        .into_iter()
        .find_map(|name| claims.get(name).filter(|v| !v.is_null()));
    let values: Vec<String> = match claim {
        Some(Value::String(s)) if s.chars().any(|c| !is_java_whitespace_char(c)) => {
            java_split_on_space(s)
        }
        Some(Value::Array(items)) => items.iter().map(java_to_string).collect(),
        _ => Vec::new(),
    };
    values.into_iter().map(|v| format!("SCOPE_{v}")).collect()
}

fn is_java_whitespace_char(c: char) -> bool {
    chalkak_app::web::binding::is_java_whitespace(c)
}

/// `String.split(" ")`: 중간의 빈 조각은 남기고 끝의 빈 조각은 버린다.
fn java_split_on_space(s: &str) -> Vec<String> {
    let mut parts: Vec<String> = s.split(' ').map(str::to_owned).collect();
    while parts.last().is_some_and(String::is_empty) {
        parts.pop();
    }
    parts
}

/// 문자열 연결(`"SCOPE_" + element`)이 쓰는 `toString`에 가깝게.
fn java_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;
    use chalkak_core::log::Level;
    use chalkak_http::Method;
    use serde_json::json;

    const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const NOW: &str = "2026-10-12T00:00:00Z";

    fn token_with(payload: serde_json::Value) -> String {
        let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
        chalkak_jwt::hs256_encode(&json!({"typ":"JWT","alg":"HS256"}), &payload, &key)
    }

    fn payload(extra: serde_json::Value) -> serde_json::Value {
        let now = 1_791_763_200_i64; // 2026-10-12T00:00:00Z
        let mut base = json!({
            "iss": "chalkak-backend",
            "aud": ["chalkak-access"],
            "sub": "01990000-0000-7000-8000-000000000001",
            "iat": now - 5,
            "exp": now + 900,
            "jti": "j",
            "purpose": "ACCESS",
            "scope": "USER",
        });
        for (k, v) in extra.as_object().unwrap() {
            if v == "__DEL__" {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = v.clone();
            }
        }
        base
    }

    fn auth() -> BearerAuth {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
        let logger = Arc::new(Logger::new(
            Box::new(std::io::sink()),
            Level::Error,
            clock.clone(),
        ));
        BearerAuth::new(
            chalkak_jwt::decode_hex(SECRET_HEX).unwrap(),
            "chalkak-backend",
            "chalkak-access",
            clock,
            logger,
        )
    }

    fn request(authorization: Option<&str>) -> Request {
        let mut req = Request::new(Method::Get, "/api/v1/topics");
        if let Some(a) = authorization {
            req.headers.append("Authorization", a);
        }
        req
    }

    fn outcome(authorization: Option<&str>) -> Outcome {
        auth().authenticate(&request(authorization))
    }

    #[test]
    fn 헤더가_없거나_bearer가_아니면_익명이다() {
        assert_eq!(outcome(None), Outcome::Anonymous);
        assert_eq!(outcome(Some("Basic dXNlcjpwYXNz")), Outcome::Anonymous);
        assert_eq!(outcome(Some("")), Outcome::Anonymous);
        assert_eq!(outcome(Some("Bear")), Outcome::Anonymous);
        let mut req = request(None);
        req.query
            .push(("access_token".into(), token_with(payload(json!({})))));
        assert_eq!(auth().authenticate(&req), Outcome::Anonymous);
    }

    #[test]
    fn 정규식에_맞지_않는_bearer는_401이다() {
        for h in [
            "Bearer",
            "Bearer ",
            "Bearer  abc",
            "Bearerabc",
            "Bearer a b",
            "Bearer a=b",
            "Bearer =",
            "bearer a$b",
            "BearerToken x",
        ] {
            assert_eq!(
                outcome(Some(h)),
                Outcome::Unauthorized("OAuth2AuthenticationException"),
                "{h}"
            );
        }
    }

    #[test]
    fn 토큰_해석은_bearer_접두사의_대소문자를_가리지_않는다() {
        assert_eq!(
            resolve_token(Some("bEaReR abc.def-ghi_jkl~+/==")),
            TokenHeader::Token("abc.def-ghi_jkl~+/==")
        );
    }

    // 하네스 시나리오 platform/auth-matrix
    #[test]
    fn 유효한_토큰은_주체와_권한을_만든다() {
        let t = token_with(payload(json!({})));
        let Outcome::Authenticated(p) = outcome(Some(&format!("Bearer {t}"))) else {
            panic!("인증되어야 한다");
        };
        assert_eq!(p.subject, "01990000-0000-7000-8000-000000000001");
        assert_eq!(p.authorities, ["SCOPE_USER"]);
    }

    #[test]
    fn 검증에_실패한_토큰은_401이다() {
        let bad = |payload| outcome(Some(&format!("Bearer {}", token_with(payload))));
        let expected = Outcome::Unauthorized("InvalidBearerTokenException");
        assert_eq!(
            bad(payload(json!({"exp": 1_791_763_200_i64 - 31}))),
            expected,
            "만료"
        );
        assert_eq!(bad(payload(json!({"iss": "other"}))), expected);
        assert_eq!(bad(payload(json!({"aud": ["x"]}))), expected);
        assert_eq!(
            bad(payload(json!({"aud": []}))),
            expected,
            "빈 aud 배열은 401"
        );
        assert_eq!(bad(payload(json!({"purpose": "REFRESH"}))), expected);
        assert_eq!(bad(payload(json!({"sub": "not-a-uuid"}))), expected);
        assert_eq!(outcome(Some("Bearer abc.def.ghi")), expected);
        // 서명 위조
        let t = token_with(payload(json!({})));
        let forged = format!("{}x", t);
        assert_eq!(outcome(Some(&format!("Bearer {forged}"))), expected);
    }

    #[test]
    fn 허용_오차_30초_안의_만료는_통과한다() {
        let t = token_with(payload(
            json!({"iat": 1_791_763_200_i64 - 100, "exp": 1_791_763_200_i64 - 29}),
        ));
        assert!(matches!(
            outcome(Some(&format!("Bearer {t}"))),
            Outcome::Authenticated(_)
        ));
    }

    #[test]
    fn aud가_없거나_null이면_서버_오류다() {
        for p in [
            payload(json!({"aud": "__DEL__"})),
            payload(json!({"aud": null})),
        ] {
            let t = token_with(p);
            assert_eq!(outcome(Some(&format!("Bearer {t}"))), Outcome::ServerError);
        }
    }

    #[test]
    fn 서버_오류_응답은_boot_본문과_연결_종료다() {
        let t = token_with(payload(json!({"aud": "__DEL__"})));
        let mut req = request(Some(&format!("Bearer {t}")));
        req.path = "/api/v1/topics".into();
        let next = |_: Request| Response::new(200);
        let r = auth().handle(req, &next);
        assert_eq!(r.status, 500);
        assert_eq!(r.headers.get("connection"), Some("close"));
        assert_eq!(r.headers.get("content-type"), Some("application/json"));
        let body = String::from_utf8(r.body).unwrap();
        assert!(
            body.contains(
                r#""status":500,"error":"Internal Server Error","path":"/api/v1/topics""#
            ),
            "{body}"
        );
    }

    #[test]
    fn 인증_성공_뒤_안쪽_계층은_주체를_본다() {
        let t = token_with(payload(json!({})));
        let seen = |_: Request| {
            Response::new(if security::current().is_some() {
                200
            } else {
                500
            })
        };
        let r = auth().handle(request(Some(&format!("Bearer {t}"))), &seen);
        assert_eq!(r.status, 200);
        assert!(security::current().is_none(), "응답 뒤에는 비워진다");
        let r = auth().handle(request(None), &seen);
        assert_eq!(r.status, 500);
    }

    #[test]
    fn 거절_응답의_모양() {
        let logger = Logger::new(
            Box::new(std::io::sink()),
            Level::Error,
            Arc::new(FixedClock::new(NOW.parse().unwrap())),
        );
        let r = unauthorized(&logger, "X");
        assert_eq!(r.status, 401);
        assert_eq!(r.headers.get("www-authenticate"), Some("Bearer"));
        assert_eq!(
            r.headers.get("content-type"),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"errorCode":"UNAUTHORIZED","message":"유효하지 않은 인증 정보입니다."}"#
        );
    }

    fn authorities_of(extra: serde_json::Value) -> Vec<String> {
        let claims = Claims::from_slice(payload(extra).to_string().as_bytes()).unwrap();
        authorities(&claims)
    }

    // JwtGrantedAuthoritiesConverter
    #[test]
    fn 권한은_scope_다음_scp에서_만든다() {
        assert_eq!(
            authorities_of(json!({"scope": "USER ADMIN"})),
            ["SCOPE_USER", "SCOPE_ADMIN"]
        );
        assert_eq!(
            authorities_of(json!({"scope": ["USER", "ADMIN"]})),
            ["SCOPE_USER", "SCOPE_ADMIN"]
        );
        assert_eq!(
            authorities_of(json!({"scope": "__DEL__", "scp": "ADMIN"})),
            ["SCOPE_ADMIN"]
        );
        assert_eq!(
            authorities_of(json!({"scope": "__DEL__", "scp": ["A"]})),
            ["SCOPE_A"]
        );
        // scope가 있으면 scp는 보지 않는다.
        assert_eq!(
            authorities_of(json!({"scope": "USER", "scp": "ADMIN"})),
            ["SCOPE_USER"]
        );
    }

    #[test]
    fn 권한이_없는_경우() {
        assert!(authorities_of(json!({"scope": "__DEL__"})).is_empty());
        assert!(authorities_of(json!({"scope": ""})).is_empty());
        assert!(authorities_of(json!({"scope": "   "})).is_empty());
        assert!(authorities_of(json!({"scope": 5})).is_empty());
        assert!(authorities_of(json!({"scope": {"a": 1}})).is_empty());
        assert!(authorities_of(json!({"scope": true, "scp": "ADMIN"})).is_empty());
    }

    #[test]
    fn 공백_분리는_자바_split처럼_동작한다() {
        assert_eq!(java_split_on_space("a  b "), ["a", "", "b"]);
        assert_eq!(java_split_on_space(" a"), ["", "a"]);
    }
}
