//! 원본 `AppVersionController`(`GET /api/v1/app-version`)와 응답 DTO.

use serde::Serialize;

use super::domain::AppPlatform;
use super::service::{AppVersionResult, AppVersionService};
use crate::AppContext;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppVersionResponse {
    platform: &'static str,
    minimum_supported_version: Option<String>,
    store_url: Option<String>,
}

impl From<AppVersionResult> for AppVersionResponse {
    fn from(result: AppVersionResult) -> Self {
        AppVersionResponse {
            platform: result.platform.as_str(),
            minimum_supported_version: result.minimum_supported_version,
            store_url: result.store_url,
        }
    }
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    register_with_service(routes, AppVersionService::new(ctx.pool.clone()))
}

/// 서비스를 직접 받아 등록한다(엔드포인트 테스트가 쓴다).
pub fn register_with_service(routes: Routes, service: AppVersionService) -> Routes {
    routes.get("/api/v1/app-version", move |req| {
        get_app_version(&service, req)
    })
}

/// `@RequestParam AppPlatform platform`: 없거나 공백뿐이면 누락, 알 수 없는 이름이면 형식 오류.
fn get_app_version(service: &AppVersionService, req: &chalkak_http::Request) -> ApiResult {
    let platform = match req.query_param("platform") {
        None => None,
        Some(text) => AppPlatform::from_request_text(text)
            .map_err(|_| ApiError::ArgumentTypeMismatch("platform".to_owned()))?,
    }
    .ok_or_else(|| ApiError::MissingRequestParameter("platform".to_owned()))?;
    let result = service.get_app_version(platform)?;
    Ok(ok_json(&AppVersionResponse::from(result)).with_header("Cache-Control", "no-store"))
}
