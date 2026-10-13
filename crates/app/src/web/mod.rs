//! 웹 계층 공통 보조. Spring MVC가 해 주던 일(응답 직렬화, 예외 → 오류 응답, 요청 바인딩, 라우트 등록)을 맡는다.

pub mod binding;
pub mod context;
pub mod error;
pub mod json;
pub mod marker;
mod routes;

pub use error::{ApiError, unreadable_query_response};
pub use routes::Routes;

use chalkak_http::Response;

/// 컨트롤러가 돌려주는 결과. `Err`는 [`ApiError`]로 응답을 만든다.
pub type ApiResult = Result<Response, ApiError>;

/// 성공 응답에 쓰는 `Content-Type`. 원본(Spring MVC + Jackson)은 charset을 붙이지 않는다.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// `200 OK` JSON 응답.
pub fn ok_json<T: serde::Serialize>(body: &T) -> Response {
    json_response(200, json::to_json(body))
}

/// `201 Created` JSON 응답.
pub fn created_json<T: serde::Serialize>(body: &T) -> Response {
    json_response(201, json::to_json(body))
}

pub fn json_response(status: u16, body: String) -> Response {
    Response::new(status)
        .with_header("Content-Type", JSON_CONTENT_TYPE)
        .with_body(body.into_bytes())
}
