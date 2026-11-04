//! 원본 `BannerController`(`GET /api/v1/banners`)와 `BannerListRequest`·`BannerListResponse`·`BannerResponse`.

use chalkak_http::Request;
use jiff::Timestamp;
use serde::Serialize;

use super::domain::Banner;
use super::repository::BannerPage;
use super::service::BannerService;
use crate::AppContext;
use crate::admin::binding::QueryParams;
use crate::web::json::serialize_instant;
use crate::web::{ApiResult, Routes, ok_json};

const DEFAULT_PAGE: i32 = 1;
const DEFAULT_PAGE_SIZE: i32 = 20;

/// 배너 한 건의 응답(`BannerResponse`). 관리자 등록 응답도 같은 모양이다.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BannerResponse {
    id: String,
    title: String,
    subtitle: String,
    web_view_url: String,
    thumbnail_url: String,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
}

impl From<Banner> for BannerResponse {
    fn from(banner: Banner) -> Self {
        BannerResponse {
            id: banner.id.unwrap_or_default().hyphenated().to_string(),
            title: banner.title,
            subtitle: banner.subtitle,
            web_view_url: banner.web_view_url,
            thumbnail_url: banner.thumbnail_url,
            created_at: banner.created_at.unwrap_or(Timestamp::UNIX_EPOCH),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BannerListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    banners: Vec<BannerResponse>,
}

impl From<BannerPage> for BannerListResponse {
    fn from(page: BannerPage) -> Self {
        BannerListResponse {
            current_page: page.current_page,
            page_size: page.page_size,
            has_next: page.has_next,
            banners: page.items.into_iter().map(BannerResponse::from).collect(),
        }
    }
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    register_with_pool(routes, ctx.pool.clone())
}

/// 연결 풀만 받아 등록한다(테스트가 쓴다).
pub fn register_with_pool(routes: Routes, pool: chalkak_db::Pool) -> Routes {
    let service = BannerService::new(pool);
    routes.get("/api/v1/banners", move |req| get_banners(&service, req))
}

fn get_banners(service: &BannerService, req: &Request) -> ApiResult {
    let (page, page_size) = read_list_request(req)?;
    let page = service.get_banners(page, page_size)?;
    Ok(ok_json(&BannerListResponse::from(page)))
}

/// `BannerListRequest`: `page`(기본 1)와 `pageSize`(기본 20). 범위 검사는 서비스가 한다.
fn read_list_request(req: &Request) -> Result<(i32, i32), crate::web::ApiError> {
    let params = QueryParams::new(req);
    let page = params.integer("page")?.unwrap_or(DEFAULT_PAGE);
    let page_size = params.integer("pageSize")?.unwrap_or(DEFAULT_PAGE_SIZE);
    Ok((page, page_size))
}
