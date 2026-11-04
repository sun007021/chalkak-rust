//! 관리자 배너 관리(원본 `AdminBannerController`, `AdminBannerService`, `AdminBannerRequest`):
//! `POST /api/v1/admin/banners`, `DELETE /api/v1/admin/banners/{bannerId}`.
//!
//! 배너는 소프트 삭제한다. 삭제한 배너의 웹뷰 URL은 다시 등록할 수 있다(부분 고유 인덱스). 감사 로그에는 제목·부제목 원문과 URL
//! 대신 제목·부제목의 SHA-256 해시만 남긴다(문구가 감사 민감값 검사에 걸려 등록이 실패하지 않도록).

use chalkak_http::{Method, Request};
use uuid::Uuid;

use crate::admin::audit::domain::{
    AdminAction, AdminTargetType, AuditSnapshot, instant_value, text_value,
};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{MUST_NOT_BE_BLANK, has_text, validation};
use crate::admin::support::{
    AdminDeps, business, in_transaction, no_content, not_found, path_uuid, resolve_current_admin,
};
use crate::banner::api::BannerResponse;
use crate::banner::domain::Banner;
use crate::banner::repository::{BannerRepository, SaveError};
use crate::common::sha256_hex;
use crate::web::binding::read_json_object;
use crate::web::{ApiError, Routes, created_json};

const NOT_FOUND_MESSAGE: &str = "배너를 찾을 수 없습니다.";
const DUPLICATE_MESSAGE: &str = "동일한 웹뷰 URL의 배너가 이미 존재합니다.";

#[derive(Clone)]
pub struct AdminBannerService {
    deps: AdminDeps,
}

impl AdminBannerService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminBannerService { deps }
    }

    /// `createBanner`: 정규화·검증 → 활성 배너와의 웹뷰 URL 중복 사전 검사 → 저장 → 등록 감사 로그(같은 트랜잭션).
    pub fn create_banner(
        &self,
        admin_id: Uuid,
        title: &str,
        subtitle: &str,
        web_view_url: &str,
        thumbnail_url: &str,
    ) -> Result<Banner, ApiError> {
        let now = self.deps.now();
        let mut banner = Banner::create(
            Some(title),
            Some(subtitle),
            Some(web_view_url),
            Some(thumbnail_url),
        )?;
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            if BannerRepository.exists_by_web_view_url(tx, &banner.web_view_url)? {
                return Err(business(DUPLICATE_MESSAGE));
            }
            BannerRepository
                .insert(tx, &mut banner, now)
                .map_err(|e| match e {
                    SaveError::DuplicateWebViewUrl => business(DUPLICATE_MESSAGE),
                    SaveError::Db(e) => ApiError::from(e),
                })?;
            let banner_id = banner
                .id
                .ok_or_else(|| business("배너 식별자가 없습니다."))?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::BannerCreated,
                    target_type: AdminTargetType::Banner,
                    target_id: banner_id,
                    reason: None,
                    before_state: AuditSnapshot::empty(),
                    after_state: banner_state(&banner)?,
                },
                now,
            )?;
            Ok(())
        })?;
        Ok(banner)
    }

    /// `deleteBanner`: 행을 잠근 뒤 소프트 삭제한다. 이미 삭제한 배너는 최초 삭제 결과를 유지하고 감사 로그를 더하지 않는다.
    pub fn delete_banner(&self, banner_id: Uuid, admin_id: Uuid) -> Result<(), ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let mut banner = BannerRepository
                .find_by_id_for_update(tx, banner_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            if banner.is_deleted() {
                return Ok(());
            }
            let before = deletion_state(&banner)?;
            banner.delete(now);
            BannerRepository.save_deletion(tx, &mut banner, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::BannerDeleted,
                    target_type: AdminTargetType::Banner,
                    target_id: banner_id,
                    reason: None,
                    before_state: before,
                    after_state: deletion_state(&banner)?,
                },
                now,
            )?;
            Ok(())
        })
    }
}

fn banner_pairs(banner: &Banner) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "titleSha256",
            text_value(&sha256_hex(banner.title.as_bytes())),
        ),
        (
            "subtitleSha256",
            text_value(&sha256_hex(banner.subtitle.as_bytes())),
        ),
    ]
}

pub(super) fn banner_state(banner: &Banner) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(banner_pairs(banner))?)
}

pub(super) fn deletion_state(banner: &Banner) -> Result<AuditSnapshot, ApiError> {
    let mut pairs = banner_pairs(banner);
    pairs.push((
        "deletedAt",
        banner
            .deleted_at
            .map_or(serde_json::Value::Null, instant_value),
    ));
    Ok(AuditSnapshot::of(pairs)?)
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let service = AdminBannerService::new(deps.clone());
    let (create_deps, delete_deps) = (deps.clone(), deps.clone());
    let create_service = service.clone();
    routes
        .post("/api/v1/admin/banners", move |req| {
            let admin_id = resolve_current_admin(&create_deps)?;
            let request = read_banner_request(req)?;
            let banner = create_service.create_banner(
                admin_id,
                &request.title,
                &request.subtitle,
                &request.web_view_url,
                &request.thumbnail_url,
            )?;
            Ok(created_json(&BannerResponse::from(banner)))
        })
        .route(
            Method::Delete,
            "/api/v1/admin/banners/{bannerId}",
            move |req| {
                let admin_id = resolve_current_admin(&delete_deps)?;
                let banner_id = path_uuid(req, "bannerId")?;
                service.delete_banner(banner_id, admin_id)?;
                Ok(no_content())
            },
        )
}

/// `AdminBannerRequest`: 네 항목 모두 `@NotBlank`.
struct BannerRequest {
    title: String,
    subtitle: String,
    web_view_url: String,
    thumbnail_url: String,
}

fn read_banner_request(req: &Request) -> Result<BannerRequest, ApiError> {
    let json = read_json_object(req)?;
    let title = json.string("title")?;
    let subtitle = json.string("subtitle")?;
    let web_view_url = json.string("webViewUrl")?;
    let thumbnail_url = json.string("thumbnailUrl")?;
    match (title, subtitle, web_view_url, thumbnail_url) {
        (Some(title), Some(subtitle), Some(web_view_url), Some(thumbnail_url))
            if has_text(&title)
                && has_text(&subtitle)
                && has_text(&web_view_url)
                && has_text(&thumbnail_url) =>
        {
            Ok(BannerRequest {
                title,
                subtitle,
                web_view_url,
                thumbnail_url,
            })
        }
        _ => Err(validation(MUST_NOT_BE_BLANK)),
    }
}
