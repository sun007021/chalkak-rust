//! 원본 `AppVersionService`·`AppVersionResult`: 공개 조회.

use chalkak_db::{DbError, Pool};

use super::domain::{AppPlatform, AppVersion};
use super::repository::AppVersionRepository;

/// 공개 조회 결과. 설정이 없으면 두 값이 모두 `None`이다(앱은 업데이트 제한 없이 이용한다).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppVersionResult {
    pub platform: AppPlatform,
    pub minimum_supported_version: Option<String>,
    pub store_url: Option<String>,
}

impl AppVersionResult {
    pub fn from_setting(setting: &AppVersion) -> AppVersionResult {
        AppVersionResult {
            platform: setting.platform,
            minimum_supported_version: Some(setting.minimum_supported_version.value().to_owned()),
            store_url: Some(setting.store_url.value().to_owned()),
        }
    }

    pub fn unconfigured(platform: AppPlatform) -> AppVersionResult {
        AppVersionResult {
            platform,
            minimum_supported_version: None,
            store_url: None,
        }
    }
}

#[derive(Clone)]
pub struct AppVersionService {
    pool: Pool,
}

impl AppVersionService {
    pub fn new(pool: Pool) -> Self {
        AppVersionService { pool }
    }

    /// `getAppVersion`: 읽기 전용. 설정이 없어도 기본 행을 만들지 않는다.
    pub fn get_app_version(&self, platform: AppPlatform) -> Result<AppVersionResult, DbError> {
        let mut conn = self.pool.get()?;
        Ok(AppVersionRepository
            .find_by_platform(&mut conn, platform)?
            .map_or_else(
                || AppVersionResult::unconfigured(platform),
                |setting| AppVersionResult::from_setting(&setting),
            ))
    }
}
