//! 원본 `AppVersionMigrationValidator`(Flyway `BEFORE_MIGRATE` 콜백): 기존 `app_versions` 행이 새 도메인으로 조회·감사할 수
//! 있는지 DDL을 실행하기 전에 확인한다. 읽기만 하며 값을 고치거나 지우지 않는다.
//!
//! 테이블이 없는 첫 설치와 빈 테이블은 통과한다. 행이 맞지 않거나 조회가 실패하면 서버 시작을 멈춘다. 오류 메시지에는
//! 행 식별자만 싣고 원문 주소·민감정보는 싣지 않는다.

use std::fmt;

use postgres::Client;
use uuid::Uuid;

use super::domain::{AppPlatform, MinimumSupportedVersion, StoreUrl};
use crate::admin::audit::domain::AuditSnapshot;

#[derive(Debug)]
pub enum AppVersionPrecheckError {
    /// 사전 검증을 위한 조회가 실패했다. 유효한 데이터로 간주하지 않는다.
    Query(postgres::Error),
    /// 행 하나가 새 형식과 맞지 않는다.
    InvalidRow(Uuid),
}

impl fmt::Display for AppVersionPrecheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppVersionPrecheckError::Query(_) => {
                write!(f, "앱 버전 설정 사전 검증을 위한 DB 조회에 실패했습니다.")
            }
            AppVersionPrecheckError::InvalidRow(id) => write!(
                f,
                "앱 버전 설정 사전 검증 실패(id={id}): 버전·플랫폼·스토어 주소와 공백·민감정보를 확인한 뒤 다시 배포해 주세요."
            ),
        }
    }
}

impl std::error::Error for AppVersionPrecheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppVersionPrecheckError::Query(e) => Some(e),
            AppVersionPrecheckError::InvalidRow(_) => None,
        }
    }
}

/// 마이그레이션을 적용하기 전에 부른다(`BEFORE_MIGRATE`).
pub fn validate_before_migrate(client: &mut Client) -> Result<(), AppVersionPrecheckError> {
    let has_table = client
        .query_one("SELECT to_regclass('app_versions') IS NOT NULL", &[])
        .map_err(AppVersionPrecheckError::Query)?
        .get::<_, bool>(0);
    if !has_table {
        return Ok(());
    }
    let rows = client
        .query(
            "SELECT id, platform, minimum_supported_version, store_url FROM app_versions",
            &[],
        )
        .map_err(AppVersionPrecheckError::Query)?;
    for row in &rows {
        let id: Uuid = row.try_get("id").map_err(AppVersionPrecheckError::Query)?;
        let platform: Option<String> = row
            .try_get("platform")
            .map_err(AppVersionPrecheckError::Query)?;
        let version: Option<String> = row
            .try_get("minimum_supported_version")
            .map_err(AppVersionPrecheckError::Query)?;
        let url: Option<String> = row
            .try_get("store_url")
            .map_err(AppVersionPrecheckError::Query)?;
        if !is_valid_setting(platform.as_deref(), version.as_deref(), url.as_deref()) {
            return Err(AppVersionPrecheckError::InvalidRow(id));
        }
    }
    Ok(())
}

/// 플랫폼 이름, 버전, 저장 원문 주소, 그리고 감사 스냅샷으로 만들 수 있는지까지 본다.
fn is_valid_setting(platform: Option<&str>, version: Option<&str>, url: Option<&str>) -> bool {
    let (Some(platform), Some(version), Some(url)) = (platform, version, url) else {
        return false;
    };
    let (Some(platform), Ok(version), Ok(url)) = (
        AppPlatform::parse(platform),
        MinimumSupportedVersion::new(version),
        StoreUrl::new(url),
    ) else {
        return false;
    };
    AuditSnapshot::from_app_version(platform, &version, &url, 0).is_ok()
}
