//! 관리자 공지 관리(원본 `AdminNoticeController`, `AdminNoticeService`, DTO): `/api/v1/admin/notices`.
//!
//! 공지는 예약 시각에 회원 알림함으로 배포된다([`crate::notification::distribution`]). 이 모듈은 목록·상세 조회, 등록·수정·
//! 삭제(소프트 삭제)와 감사 로그 연결만 맡는다. 배포된 공지는 수정할 수 없고, 삭제는 배포 전이면 발송 취소, 배포 후이면 회원
//! 알림함에서 숨김이다. 감사 로그에는 본문 원문 대신 SHA-256 해시를 남긴다(공개 문구인 본문은 민감 정보 검사 대상이 아니다).

use chalkak_http::{Method, Request};
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use crate::admin::audit::domain::{
    AdminAction, AdminTargetType, AuditSnapshot, instant_value, text_value,
};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{
    MUST_NOT_BE_BLANK, MUST_NOT_BE_NULL, QueryParams, body_instant, code_points, has_text,
    java_trim, page_params, size_message, utf16_len, validation,
};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, business, in_transaction, no_content, not_found,
    path_uuid, resolve_current_admin, validate_pagination,
};
use crate::common::sha256_hex;
use crate::notification::notice::{Notice, NoticeRepository};
use crate::web::binding::read_json_object;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, created_json, ok_json};

const MAX_DELETION_REASON_LENGTH: usize = 500;
const NOT_FOUND_MESSAGE: &str = "공지를 찾을 수 없습니다.";
const DELETION_REASON_MESSAGE: &str = "공지 삭제 사유는 1자 이상 500자 이하여야 합니다.";

#[derive(Clone)]
pub struct AdminNoticeService {
    deps: AdminDeps,
}

impl AdminNoticeService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminNoticeService { deps }
    }

    /// `getNotices`: 등록 최신순. 삭제한 공지도 삭제 시각과 함께 보인다.
    pub fn get_notices(&self, page: i32, page_size: i32) -> Result<AdminPage<Notice>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let mut conn = self.deps.pool.get()?;
        let slice = NoticeRepository.find_notices(&mut conn, page, page_size)?;
        Ok(AdminPage {
            items: slice.notices,
            current_page: page,
            page_size,
            has_next: slice.has_next,
        })
    }

    pub fn get_notice(&self, notice_id: Uuid) -> Result<Notice, ApiError> {
        let mut conn = self.deps.pool.get()?;
        NoticeRepository
            .find_by_id(&mut conn, notice_id)?
            .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))
    }

    pub fn create_notice(
        &self,
        admin_id: Uuid,
        title: &str,
        body: &str,
        scheduled_at: Timestamp,
    ) -> Result<Notice, ApiError> {
        let now = self.deps.now();
        let mut notice = Notice::create(title, body, scheduled_at, now)?;
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            NoticeRepository.save_and_flush(tx, &mut notice, now)?;
            let notice_id = notice
                .id
                .ok_or_else(|| business("공지 식별자가 없습니다."))?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::NoticeCreated,
                    target_type: AdminTargetType::Notice,
                    target_id: notice_id,
                    reason: None,
                    before_state: AuditSnapshot::empty(),
                    after_state: notice_state(&notice)?,
                },
                now,
            )?;
            Ok(())
        })?;
        Ok(notice)
    }

    pub fn update_notice(
        &self,
        notice_id: Uuid,
        admin_id: Uuid,
        title: &str,
        body: &str,
        scheduled_at: Timestamp,
    ) -> Result<Notice, ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let mut notice = NoticeRepository
                .find_by_id_for_update(tx, notice_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            let before = notice_state(&notice)?;
            notice.update(title, body, scheduled_at, now)?;
            let after = notice_state(&notice)?;
            // 같은 내용으로 수정하면 바뀐 것이 없으므로 저장하지 않고 감사 로그도 남기지 않는다.
            if before.values() == after.values() {
                return Ok(notice);
            }
            NoticeRepository.save_and_flush(tx, &mut notice, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::NoticeUpdated,
                    target_type: AdminTargetType::Notice,
                    target_id: notice_id,
                    reason: None,
                    before_state: before,
                    after_state: after,
                },
                now,
            )?;
            Ok(notice)
        })
    }

    pub fn delete_notice(
        &self,
        notice_id: Uuid,
        admin_id: Uuid,
        reason: &str,
    ) -> Result<(), ApiError> {
        let reason = normalize_deletion_reason(reason)?;
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let mut notice = NoticeRepository
                .find_by_id_for_update(tx, notice_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            // 이미 삭제한 공지는 최초 삭제 결과를 유지하고 감사 로그를 추가하지 않는다.
            if notice.is_deleted() {
                return Ok(());
            }
            let before = deletion_state(&notice)?;
            notice.delete(now)?;
            NoticeRepository.save_and_flush(tx, &mut notice, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::NoticeDeleted,
                    target_type: AdminTargetType::Notice,
                    target_id: notice_id,
                    reason: Some(reason.clone()),
                    before_state: before,
                    after_state: deletion_state(&notice)?,
                },
                now,
            )?;
            Ok(())
        })
    }
}

fn normalize_deletion_reason(reason: &str) -> Result<String, ApiError> {
    let normalized = java_trim(reason);
    if normalized.is_empty() || code_points(normalized) > MAX_DELETION_REASON_LENGTH {
        return Err(business(DELETION_REASON_MESSAGE));
    }
    Ok(normalized.to_owned())
}

/// 본문은 원문 대신 SHA-256(소문자 hex)을 남긴다.
fn notice_pairs(notice: &Notice) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("title", text_value(&notice.title)),
        ("scheduledAt", instant_value(notice.scheduled_at)),
        (
            "bodySha256",
            text_value(&sha256_hex(notice.body.as_bytes())),
        ),
    ]
}

fn notice_state(notice: &Notice) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(notice_pairs(notice))?)
}

fn deletion_state(notice: &Notice) -> Result<AuditSnapshot, ApiError> {
    let mut pairs = notice_pairs(notice);
    pairs.push((
        "deletedAt",
        notice
            .deleted_at
            .map_or(serde_json::Value::Null, instant_value),
    ));
    Ok(AuditSnapshot::of(pairs)?)
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let service = AdminNoticeService::new(deps.clone());
    let (list_deps, detail_deps, create_deps, update_deps, delete_deps) = (
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
    );
    let (list_service, detail_service, create_service, update_service) = (
        service.clone(),
        service.clone(),
        service.clone(),
        service.clone(),
    );
    routes
        .get("/api/v1/admin/notices", move |req| {
            resolve_current_admin(&list_deps)?;
            get_notices(&list_service, req)
        })
        .get("/api/v1/admin/notices/{noticeId}", move |req| {
            resolve_current_admin(&detail_deps)?;
            let notice = detail_service.get_notice(path_uuid(req, "noticeId")?)?;
            Ok(ok_json(&NoticeResponse::from(notice)))
        })
        .post("/api/v1/admin/notices", move |req| {
            let admin_id = resolve_current_admin(&create_deps)?;
            let request = read_notice_request(req)?;
            let notice = create_service.create_notice(
                admin_id,
                &request.title,
                &request.body,
                request.scheduled_at,
            )?;
            Ok(created_json(&NoticeResponse::from(notice)))
        })
        .route(
            Method::Put,
            "/api/v1/admin/notices/{noticeId}",
            move |req| {
                let admin_id = resolve_current_admin(&update_deps)?;
                let request = read_notice_request(req)?;
                let notice_id = path_uuid(req, "noticeId")?;
                let notice = update_service.update_notice(
                    notice_id,
                    admin_id,
                    &request.title,
                    &request.body,
                    request.scheduled_at,
                )?;
                Ok(ok_json(&NoticeResponse::from(notice)))
            },
        )
        .route(
            Method::Delete,
            "/api/v1/admin/notices/{noticeId}",
            move |req| {
                let admin_id = resolve_current_admin(&delete_deps)?;
                let reason = read_deletion_reason(req)?;
                let notice_id = path_uuid(req, "noticeId")?;
                service.delete_notice(notice_id, admin_id, &reason)?;
                Ok(no_content())
            },
        )
}

fn get_notices(service: &AdminNoticeService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_notices(page, page_size)?;
    Ok(ok_json(&NoticeListResponse {
        current_page: result.current_page,
        page_size: result.page_size,
        has_next: result.has_next,
        notices: result.items.into_iter().map(NoticeResponse::from).collect(),
    }))
}

/// `AdminNoticeRequest`: `@NotBlank title`, `@NotBlank body`, `@NotNull scheduledAt`.
struct NoticeRequest {
    title: String,
    body: String,
    scheduled_at: Timestamp,
}

fn read_notice_request(req: &Request) -> Result<NoticeRequest, ApiError> {
    let json = read_json_object(req)?;
    let title = json.string("title")?;
    let body = json.string("body")?;
    let scheduled_at = body_instant(&json, "scheduledAt")?;
    let Some(scheduled_at) = scheduled_at else {
        return Err(validation(MUST_NOT_BE_NULL));
    };
    match (title, body) {
        (Some(title), Some(body)) if has_text(&title) && has_text(&body) => Ok(NoticeRequest {
            title,
            body,
            scheduled_at,
        }),
        _ => Err(validation(MUST_NOT_BE_BLANK)),
    }
}

/// `AdminNoticeDeletionRequest`: `@NotBlank @Size(max = 500) reason`.
fn read_deletion_reason(req: &Request) -> Result<String, ApiError> {
    let json = read_json_object(req)?;
    let reason = match json.string("reason")? {
        Some(r) if has_text(&r) => r,
        _ => return Err(validation(MUST_NOT_BE_BLANK)),
    };
    if utf16_len(&reason) > MAX_DELETION_REASON_LENGTH {
        return Err(validation(&size_message(MAX_DELETION_REASON_LENGTH)));
    }
    Ok(reason)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NoticeListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    notices: Vec<NoticeResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoticeResponse {
    notice_id: String,
    title: String,
    body: String,
    #[serde(serialize_with = "serialize_instant")]
    scheduled_at: Timestamp,
    status: &'static str,
    #[serde(serialize_with = "crate::admin::user::api::serialize_optional_instant")]
    distributed_at: Option<Timestamp>,
    #[serde(serialize_with = "crate::admin::user::api::serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: Timestamp,
}

impl From<Notice> for NoticeResponse {
    fn from(notice: Notice) -> Self {
        NoticeResponse {
            notice_id: notice.id.unwrap_or_default().hyphenated().to_string(),
            status: notice.status().as_str(),
            title: notice.title,
            body: notice.body,
            scheduled_at: notice.scheduled_at,
            distributed_at: notice.distributed_at,
            deleted_at: notice.deleted_at,
            created_at: notice.created_at.unwrap_or(Timestamp::UNIX_EPOCH),
            updated_at: notice.updated_at.unwrap_or(Timestamp::UNIX_EPOCH),
        }
    }
}
