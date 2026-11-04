//! 원본 `BannerService`: 공개 배너 목록.

use chalkak_db::Pool;

use super::repository::{BannerPage, BannerRepository};
use crate::admin::support::{INVALID_QUERY_MESSAGE, validate_pagination};
use crate::web::ApiError;

#[derive(Clone)]
pub struct BannerService {
    pool: Pool,
}

impl BannerService {
    pub fn new(pool: Pool) -> Self {
        BannerService { pool }
    }

    /// `getBanners`: 쪽 번호·크기·offset 범위를 서비스에서 검사한다(잘못되면 `BUSINESS_ERROR` 400).
    pub fn get_banners(&self, page: i32, page_size: i32) -> Result<BannerPage, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let mut conn = self.pool.get()?;
        Ok(BannerRepository.find_banners(&mut conn, page, page_size)?)
    }
}
