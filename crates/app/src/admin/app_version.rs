//! 관리자 앱 버전 관리(원본 `AdminAppVersionController`, `AdminAppVersionService`, DTO):
//! `GET /api/v1/admin/app-versions`, `PUT /api/v1/admin/app-versions/{platform}`.
//!
//! 플랫폼마다 설정 하나를 둔다. 저장은 플랫폼 단위 어드바이저리 락으로 직렬화하고, 값이 바뀌는 저장만 revision을 맞춰
//! 확인한다. 설정 변경과 감사 로그는 같은 트랜잭션이다.

use chalkak_core::error::{AppError, ErrorCode, messages};
use chalkak_http::{Method, Request};
use jiff::Timestamp;
use serde::{Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

use crate::admin::audit::domain::{AdminAction, AdminTargetType, AuditSnapshot};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{MUST_NOT_BE_BLANK, has_text, java_trim, validation};
use crate::admin::support::{AdminDeps, in_transaction, resolve_current_admin, state_changed};
use crate::appversion::domain::{
    AppPlatform, AppVersion, ChangeReason, MinimumSupportedVersion, StoreUrl,
};
use crate::appversion::repository::{AppVersionRepository, STATE_CHANGED_MESSAGE, SaveError};
use crate::web::binding::{JsonObject, read_json_object};
use crate::web::json::format_instant;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

const NEGATIVE_REVISION_MESSAGE: &str = "must be greater than or equal to 0";

/// 서비스가 돌려주는 설정 한 줄(`SettingResult`). 미설정 플랫폼은 `platform` 말고 모두 `None`이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingResult {
    pub platform: AppPlatform,
    pub minimum_supported_version: Option<String>,
    pub store_url: Option<String>,
    pub revision: Option<i64>,
    pub last_changed_by: Option<Uuid>,
    pub change_reason: Option<String>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl SettingResult {
    pub fn from_setting(setting: &AppVersion) -> SettingResult {
        SettingResult {
            platform: setting.platform,
            minimum_supported_version: Some(setting.minimum_supported_version.value().to_owned()),
            store_url: Some(setting.store_url.value().to_owned()),
            revision: Some(setting.revision),
            last_changed_by: setting.last_changed_by,
            change_reason: setting.change_reason.clone(),
            created_at: setting.created_at,
            updated_at: setting.updated_at,
        }
    }

    pub fn unconfigured(platform: AppPlatform) -> SettingResult {
        SettingResult {
            platform,
            minimum_supported_version: None,
            store_url: None,
            revision: None,
            last_changed_by: None,
            change_reason: None,
            created_at: None,
            updated_at: None,
        }
    }
}

#[derive(Clone)]
pub struct AdminAppVersionService {
    deps: AdminDeps,
}

impl AdminAppVersionService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminAppVersionService { deps }
    }

    /// `getAppVersions`: ANDROID, IOS 순서. 없는 플랫폼은 미설정 줄이다.
    pub fn get_app_versions(&self) -> Result<Vec<SettingResult>, ApiError> {
        let mut conn = self.deps.pool.get()?;
        let mut results = Vec::with_capacity(AppPlatform::ALL.len());
        for platform in AppPlatform::ALL {
            let setting = AppVersionRepository.find_by_platform(&mut conn, platform)?;
            results.push(match setting {
                Some(setting) => SettingResult::from_setting(&setting),
                None => SettingResult::unconfigured(platform),
            });
        }
        Ok(results)
    }

    /// `saveAppVersion`
    pub fn save_app_version(
        &self,
        admin_id: Uuid,
        platform: AppPlatform,
        revision: Option<i64>,
        minimum_supported_version: &str,
        store_url: &str,
        reason: &str,
    ) -> Result<SettingResult, ApiError> {
        // 입력 검증은 DB를 쓰기 전에 끝난다.
        let version = MinimumSupportedVersion::new(minimum_supported_version)?;
        let url = StoreUrl::from_input(store_url)?;
        url.validate_platform(platform)?;
        let reason = ChangeReason::new(reason)?;
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = AppVersionRepository;
            repo.lock_platform(tx, platform)?;
            let setting = repo.find_by_platform(tx, platform)?;
            // 재시도와 같은 값 재저장은 시각·사유·관리자·revision·감사 이력을 모두 유지한다.
            if let Some(setting) = &setting
                && version == setting.minimum_supported_version
                && url == setting.store_url
            {
                return Ok(SettingResult::from_setting(setting));
            }
            let current_revision = setting.as_ref().map(|s| s.revision);
            if revision != current_revision {
                return Err(state_changed(STATE_CHANGED_MESSAGE));
            }
            match setting {
                None => {
                    let created = AppVersion::create(platform, version, url, admin_id, &reason)?;
                    save_with_audit(
                        tx,
                        admin_id,
                        created,
                        AdminAction::AppVersionCreated,
                        AuditSnapshot::empty(),
                        &reason,
                        now,
                    )
                }
                Some(mut setting) => {
                    let before = snapshot(&setting)?;
                    setting.update(version, url, admin_id, &reason)?;
                    save_with_audit(
                        tx,
                        admin_id,
                        setting,
                        AdminAction::AppVersionUpdated,
                        before,
                        &reason,
                        now,
                    )
                }
            }
        })
    }
}

fn save_with_audit(
    tx: &mut chalkak_db::Tx<'_>,
    admin_id: Uuid,
    mut setting: AppVersion,
    action: AdminAction,
    before: AuditSnapshot,
    reason: &ChangeReason,
    now: Timestamp,
) -> Result<SettingResult, ApiError> {
    AppVersionRepository
        .save_and_flush(tx, &mut setting, now)
        .map_err(|e| match e {
            SaveError::Business(e) => ApiError::App(e),
            SaveError::Db(e) => ApiError::from(e),
        })?;
    let target_id = setting
        .id
        .ok_or_else(|| ApiError::Internal("저장한 앱 버전 설정의 식별자가 없습니다.".to_owned()))?;
    create_audit_log(
        tx,
        AuditLogCommand {
            actor_admin_id: admin_id,
            action,
            target_type: AdminTargetType::AppVersion,
            target_id,
            reason: Some(reason.value().to_owned()),
            before_state: before,
            after_state: snapshot(&setting)?,
        },
        now,
    )?;
    Ok(SettingResult::from_setting(&setting))
}

fn snapshot(setting: &AppVersion) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::from_app_version(
        setting.platform,
        &setting.minimum_supported_version,
        &setting.store_url,
        setting.revision,
    )?)
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let service = AdminAppVersionService::new(deps.clone());
    let (list_deps, save_deps) = (deps.clone(), deps.clone());
    let list_service = service.clone();
    routes
        .get("/api/v1/admin/app-versions", move |_req| {
            resolve_current_admin(&list_deps)?;
            let results = list_service.get_app_versions()?;
            Ok(ok_json(&AppVersionListResponse {
                app_versions: results.into_iter().map(AppVersionResponse::from).collect(),
            }))
        })
        .route(
            Method::Put,
            "/api/v1/admin/app-versions/{platform}",
            move |req| save_app_version(&save_deps, &service, req),
        )
}

/// 인자 해석 순서: 관리자, 경로의 플랫폼, 본문(검증 포함).
fn save_app_version(
    deps: &AdminDeps,
    service: &AdminAppVersionService,
    req: &Request,
) -> ApiResult {
    let admin_id = resolve_current_admin(deps)?;
    let platform = path_platform(req)?;
    let body = read_save_request(req)?;
    let result = service.save_app_version(
        admin_id,
        platform,
        body.revision,
        &body.minimum_supported_version,
        &body.store_url,
        &body.reason,
    )?;
    Ok(ok_json(&AppVersionResponse::from(result)))
}

/// `@PathVariable AppPlatform platform`: 이름이 정확히 같아야 한다(앞뒤 공백은 뗀다).
fn path_platform(req: &Request) -> Result<AppPlatform, ApiError> {
    let text = req.param("platform").unwrap_or_default();
    match AppPlatform::from_request_text(text) {
        Ok(Some(platform)) => Ok(platform),
        // 변환 뒤 값이 비면 `MissingPathVariableException`(400)이고, 처리기가 일반 4xx 문구로 답한다.
        Ok(None) => Err(ApiError::App(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            messages::UNSUPPORTED_REQUEST,
        ))),
        Err(_) => Err(ApiError::ArgumentTypeMismatch("platform".to_owned())),
    }
}

/// `AdminAppVersionRequest`: `@PositiveOrZero Long revision`, `@NotBlank` 버전·주소·사유.
struct SaveRequest {
    revision: Option<i64>,
    minimum_supported_version: String,
    store_url: String,
    reason: String,
}

fn read_save_request(req: &Request) -> Result<SaveRequest, ApiError> {
    let json = read_json_object(req)?;
    let revision = body_long(&json, "revision")?;
    let version = json.string("minimumSupportedVersion")?;
    let url = json.string("storeUrl")?;
    let reason = json.string("reason")?;
    let nonblank = |value: Option<String>| value.filter(|v| has_text(v));
    let (version, url, reason) = (nonblank(version), nonblank(url), nonblank(reason));
    if revision.is_some_and(|r| r < 0) {
        return Err(validation(NEGATIVE_REVISION_MESSAGE));
    }
    match (version, url, reason) {
        (Some(minimum_supported_version), Some(store_url), Some(reason)) => Ok(SaveRequest {
            revision,
            minimum_supported_version,
            store_url,
            reason,
        }),
        _ => Err(validation(MUST_NOT_BE_BLANK)),
    }
}

/// Jackson의 `Long` 필드: 정수, 소수(0 방향으로 버린다), 숫자 문자열을 받는다. 빈 문자열과 `null`은 값 없음이다.
/// 범위를 벗어난 값과 그 밖의 형태(불리언, 배열, 객체, 숫자가 아닌 문자열)는 읽을 수 없는 본문이다.
fn body_long(body: &JsonObject, field: &str) -> Result<Option<i64>, ApiError> {
    match body.value(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(v) = n.as_i64() {
                return Ok(Some(v));
            }
            if n.is_u64() {
                return Err(ApiError::UnreadableBody);
            }
            truncate_double(n.as_f64().unwrap_or(f64::NAN)).map(Some)
        }
        Some(Value::String(text)) => {
            let trimmed = java_trim(text);
            if trimmed.is_empty() {
                return Ok(None);
            }
            parse_long(trimmed)
                .map(Some)
                .ok_or(ApiError::UnreadableBody)
        }
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

fn truncate_double(value: f64) -> Result<i64, ApiError> {
    const LIMIT: f64 = i64::MAX as f64;
    // -2^63 정확히는 `Long` 범위를 넘친 정수 토큰과 구분되지 않아 거부한다(원본은 넘친 정수를 읽지 못한다).
    if value.is_nan() || value <= -LIMIT || value >= LIMIT {
        return Err(ApiError::UnreadableBody);
    }
    Ok(value as i64)
}

/// `NumberInput.parseLong`: 부호는 선택, 숫자만, `Long` 범위.
fn parse_long(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.strip_prefix('+').unwrap_or(text).parse().ok()
}

// ---------------------------------------------------------------------------------------------
// 응답 DTO
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppVersionListResponse {
    app_versions: Vec<AppVersionResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppVersionResponse {
    platform: &'static str,
    minimum_supported_version: Option<String>,
    store_url: Option<String>,
    revision: Option<i64>,
    last_changed_by: Option<String>,
    change_reason: Option<String>,
    #[serde(serialize_with = "serialize_opt_instant")]
    created_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_opt_instant")]
    updated_at: Option<Timestamp>,
}

impl From<SettingResult> for AppVersionResponse {
    fn from(result: SettingResult) -> Self {
        AppVersionResponse {
            platform: result.platform.as_str(),
            minimum_supported_version: result.minimum_supported_version,
            store_url: result.store_url,
            revision: result.revision,
            last_changed_by: result.last_changed_by.map(|id| id.hyphenated().to_string()),
            change_reason: result.change_reason,
            created_at: result.created_at,
            updated_at: result.updated_at,
        }
    }
}

fn serialize_opt_instant<S: Serializer>(
    value: &Option<Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(ts) => serializer.serialize_str(&format_instant(*ts)),
        None => serializer.serialize_none(),
    }
}
