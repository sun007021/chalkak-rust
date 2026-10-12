//! 원본 `AdminAuthController`의 `POST /api/v1/admin/auth/logout` 요청 검증.
//!
//! 요청 검증(`AdminRefreshTokenRequest`의 `@NotBlank`)까지만 옮겼다. 리프레시 토큰을 실제로 폐기하는 일은 관리자
//! 인증 이슈의 몫이다. 그때까지는 검증을 통과한 요청을 성공(204)으로 속이지 않고 서버 오류로 끝낸다.

use chalkak_http::Request;

use crate::web::binding::{is_java_whitespace, read_json_object};
use crate::web::{ApiError, ApiResult, Routes};

pub fn register(routes: Routes) -> Routes {
    routes.post("/api/v1/admin/auth/logout", logout)
}

fn logout(req: &Request) -> ApiResult {
    let body = read_json_object(req)?;
    let refresh_token = body.string("refreshToken")?;
    match refresh_token {
        Some(t) if !t.chars().all(is_java_whitespace) => {}
        _ => {
            return Err(ApiError::Validation(
                "리프레시 토큰은 필수입니다.".to_owned(),
            ));
        }
    }
    Err(ApiError::Internal(
        "관리자 로그아웃은 아직 옮겨지지 않았다".to_owned(),
    ))
}
