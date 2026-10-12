//! 원본 `AdminAuthController`: `/api/v1/admin/auth/{login,me,refresh,logout}`.

use chalkak_http::Request;
use serde::Serialize;

use super::service::{
    AdminAuthenticationService, AdminLoginResult, AdminRefreshTokenService, CurrentAdminResult,
    TokenRefreshResult,
};
use crate::admin::binding::{has_text, utf16_len, validation};
use crate::admin::support::{AdminDeps, no_content, resolve_current_admin};
use crate::web::binding::{JsonObject, read_json_object};
use crate::web::{ApiResult, Routes, ok_json};

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let authentication = AdminAuthenticationService::new(deps.clone());
    let refresh_tokens = AdminRefreshTokenService::new(deps.clone());
    let me_deps = deps.clone();
    let me_authentication = authentication.clone();
    let refresh_service = refresh_tokens.clone();
    routes
        .post("/api/v1/admin/auth/login", move |req| {
            login(&authentication, req)
        })
        .get("/api/v1/admin/auth/me", move |_| {
            let admin_id = resolve_current_admin(&me_deps)?;
            let result = me_authentication.get_current_admin(admin_id)?;
            Ok(ok_json(&CurrentAdminResponse::from(result)))
        })
        .post("/api/v1/admin/auth/refresh", move |req| {
            let token = refresh_token_of(req)?;
            let result = refresh_service.refresh(&token)?;
            Ok(ok_json(&TokenRefreshResponse::from(result)))
        })
        .post("/api/v1/admin/auth/logout", move |req| {
            let token = refresh_token_of(req)?;
            refresh_tokens.logout(&token)?;
            Ok(no_content())
        })
}

/// `AdminRefreshTokenRequest`: `@NotBlank`.
fn refresh_token_of(req: &Request) -> Result<String, crate::web::ApiError> {
    let body = read_json_object(req)?;
    match body.string("refreshToken")? {
        Some(token) if has_text(&token) => Ok(token),
        _ => Err(validation("리프레시 토큰은 필수입니다.")),
    }
}

/// `AdminLoginRequest`의 검증. 위반이 여럿일 때 원본이 먼저 내는 순서를 따른다:
/// 비밀번호 길이, 아이디 길이, 비밀번호 공백, 아이디 공백.
fn read_login(body: &JsonObject) -> Result<(String, String), crate::web::ApiError> {
    let username = body.string("username")?;
    let password = body.string("password")?;
    if password.as_deref().is_some_and(|p| utf16_len(p) > 200) {
        return Err(validation("관리자 비밀번호 형식이 올바르지 않습니다."));
    }
    if username.as_deref().is_some_and(|u| utf16_len(u) > 100) {
        return Err(validation("관리자 아이디 형식이 올바르지 않습니다."));
    }
    let password = match password {
        Some(p) if has_text(&p) => p,
        _ => return Err(validation("관리자 비밀번호를 입력해 주세요.")),
    };
    let username = match username {
        Some(u) if has_text(&u) => u,
        _ => return Err(validation("관리자 아이디를 입력해 주세요.")),
    };
    Ok((username, password))
}

fn login(service: &AdminAuthenticationService, req: &Request) -> ApiResult {
    let body = read_json_object(req)?;
    let (username, password) = read_login(&body)?;
    let result = service.login(&username, &password)?;
    Ok(ok_json(&LoginResponse::from(result)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginResponse {
    admin_id: String,
    username: String,
    access_token: String,
    expires_in: u64,
    refresh_token: String,
    refresh_token_expires_in: i64,
}

impl From<AdminLoginResult> for LoginResponse {
    fn from(r: AdminLoginResult) -> Self {
        LoginResponse {
            admin_id: r.admin_id.hyphenated().to_string(),
            username: r.username,
            access_token: r.access_token.value,
            expires_in: r.access_token.expires_in_secs,
            refresh_token: r.refresh_token.value,
            refresh_token_expires_in: r.refresh_token.expires_in_secs,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenRefreshResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: String,
    refresh_token_expires_in: i64,
}

impl From<TokenRefreshResult> for TokenRefreshResponse {
    fn from(r: TokenRefreshResult) -> Self {
        TokenRefreshResponse {
            access_token: r.access_token.value,
            expires_in: r.access_token.expires_in_secs,
            refresh_token: r.refresh_token.value,
            refresh_token_expires_in: r.refresh_token.expires_in_secs,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentAdminResponse {
    admin_id: String,
    username: String,
}

impl From<CurrentAdminResult> for CurrentAdminResponse {
    fn from(r: CurrentAdminResult) -> Self {
        CurrentAdminResponse {
            admin_id: r.admin_id.hyphenated().to_string(),
            username: r.username,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(value: serde_json::Value) -> JsonObject {
        let mut r = Request::new(chalkak_http::Method::Post, "/");
        r.headers.append("Content-Type", "application/json");
        r.body = value.to_string().into_bytes();
        read_json_object(&r).unwrap()
    }

    fn message(result: Result<(String, String), crate::web::ApiError>) -> String {
        match result {
            Err(crate::web::ApiError::Validation(m)) => m,
            other => panic!("검증 오류가 아니다: {other:?}"),
        }
    }

    // AdminLoginRequest
    #[test]
    fn 아이디와_비밀번호는_비어_있으면_안_된다() {
        assert_eq!(
            message(read_login(&body(json!({"password": "p"})))),
            "관리자 아이디를 입력해 주세요."
        );
        assert_eq!(
            message(read_login(&body(json!({"username": "u", "password": " "})))),
            "관리자 비밀번호를 입력해 주세요."
        );
        assert!(read_login(&body(json!({"username": "u", "password": "p"}))).is_ok());
    }

    #[test]
    fn 길이_제한을_넘으면_형식_오류다() {
        assert_eq!(
            message(read_login(&body(
                json!({"username": "u".repeat(101), "password": "p"})
            ))),
            "관리자 아이디 형식이 올바르지 않습니다."
        );
        assert_eq!(
            message(read_login(&body(
                json!({"username": "u", "password": "p".repeat(201)})
            ))),
            "관리자 비밀번호 형식이 올바르지 않습니다."
        );
    }
}
