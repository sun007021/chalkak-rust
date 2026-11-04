//! 관리자 리마인더 관리(원본 `AdminReminderController`, `AdminReminderService`, DTO): `/api/v1/admin/reminders`.
//!
//! 리마인더는 알림함에 남기지 않고 푸시로만 보낸다. 정시 발행은 [`crate::notification::reminder_push`]가 한다. 이 모듈은
//! 목록 조회, 등록·수정·삭제(소프트 삭제)와 감사 로그 연결만 맡는다. 감사 로그에는 본문 원문 대신 SHA-256 해시를 남긴다.

use chalkak_http::{Method, Request};
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use crate::admin::audit::domain::{
    AdminAction, AdminTargetType, AuditSnapshot, instant_value, text_value,
};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{
    MUST_NOT_BE_BLANK, MUST_NOT_BE_NULL, QueryParams, body_enum, body_instant, has_text,
    page_params, validation,
};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, in_transaction, no_content, not_found, path_uuid,
    resolve_current_admin, validate_pagination,
};
use crate::common::sha256_hex;
use crate::notification::reminder::{Reminder, ReminderRepeat, ReminderRepository, ReminderTarget};
use crate::web::binding::read_json_object;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, created_json, ok_json};

const NOT_FOUND_MESSAGE: &str = "리마인더를 찾을 수 없습니다.";

/// 등록·수정 요청에서 같이 받는 값.
#[derive(Clone, Debug)]
pub struct ReminderInput {
    pub title: String,
    pub body: String,
    pub repeat: ReminderRepeat,
    pub target: ReminderTarget,
    pub scheduled_at: Timestamp,
}

#[derive(Clone)]
pub struct AdminReminderService {
    deps: AdminDeps,
}

impl AdminReminderService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminReminderService { deps }
    }

    /// `getReminders`: 삭제하지 않은 리마인더를 등록 최신순으로.
    pub fn get_reminders(
        &self,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<Reminder>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let mut conn = self.deps.pool.get()?;
        let slice = ReminderRepository.find_reminders(&mut conn, page, page_size)?;
        Ok(AdminPage {
            items: slice.reminders,
            current_page: page,
            page_size,
            has_next: slice.has_next,
        })
    }

    pub fn create_reminder(
        &self,
        admin_id: Uuid,
        input: &ReminderInput,
    ) -> Result<Reminder, ApiError> {
        let now = self.deps.now();
        let mut reminder = Reminder::create(
            &input.title,
            &input.body,
            input.repeat,
            input.target,
            input.scheduled_at,
            now,
        )?;
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            ReminderRepository.save_and_flush(tx, &mut reminder, now)?;
            let reminder_id = reminder.id.ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::ReminderCreated,
                    target_type: AdminTargetType::Reminder,
                    target_id: reminder_id,
                    reason: None,
                    before_state: AuditSnapshot::empty(),
                    after_state: reminder_state(&reminder)?,
                },
                now,
            )?;
            Ok(())
        })?;
        Ok(reminder)
    }

    pub fn update_reminder(
        &self,
        reminder_id: Uuid,
        admin_id: Uuid,
        input: &ReminderInput,
    ) -> Result<Reminder, ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let mut reminder = ReminderRepository
                .find_by_id_for_update(tx, reminder_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            let before = reminder_state(&reminder)?;
            reminder.update(
                &input.title,
                &input.body,
                input.repeat,
                input.target,
                input.scheduled_at,
                now,
            )?;
            let after = reminder_state(&reminder)?;
            // 같은 내용으로 수정하면 바뀐 것이 없으므로 저장하지 않고 감사 로그도 남기지 않는다.
            if before.values() == after.values() {
                return Ok(reminder);
            }
            // 응답의 수정 시각이 채워지도록 UPDATE를 먼저 실행한다.
            ReminderRepository.save_and_flush(tx, &mut reminder, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::ReminderUpdated,
                    target_type: AdminTargetType::Reminder,
                    target_id: reminder_id,
                    reason: None,
                    before_state: before,
                    after_state: after,
                },
                now,
            )?;
            Ok(reminder)
        })
    }

    pub fn delete_reminder(&self, reminder_id: Uuid, admin_id: Uuid) -> Result<(), ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let mut reminder = ReminderRepository
                .find_by_id_for_update(tx, reminder_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            // 이미 삭제한 리마인더는 최초 삭제 결과를 유지하고 감사 로그를 추가하지 않는다.
            if reminder.is_deleted() {
                return Ok(());
            }
            let before = deletion_state(&reminder)?;
            reminder.delete(now)?;
            ReminderRepository.save_and_flush(tx, &mut reminder, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::ReminderDeleted,
                    target_type: AdminTargetType::Reminder,
                    target_id: reminder_id,
                    reason: None,
                    before_state: before,
                    after_state: deletion_state(&reminder)?,
                },
                now,
            )?;
            Ok(())
        })
    }
}

/// 본문 원문을 감사 로그에 노출하지 않도록 공통 UTF-8 SHA-256 해시(소문자 hex)로 기록한다.
fn reminder_pairs(reminder: &Reminder) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("title", text_value(&reminder.title)),
        (
            "bodySha256",
            text_value(&sha256_hex(reminder.body.as_bytes())),
        ),
        ("repeat", text_value(reminder.repeat.as_str())),
        ("target", text_value(reminder.target.as_str())),
        ("scheduledAt", instant_value(reminder.scheduled_at)),
    ]
}

fn reminder_state(reminder: &Reminder) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(reminder_pairs(reminder))?)
}

fn deletion_state(reminder: &Reminder) -> Result<AuditSnapshot, ApiError> {
    let mut pairs = reminder_pairs(reminder);
    pairs.push((
        "deletedAt",
        reminder
            .deleted_at
            .map_or(serde_json::Value::Null, instant_value),
    ));
    Ok(AuditSnapshot::of(pairs)?)
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let service = AdminReminderService::new(deps.clone());
    let (list_deps, create_deps, update_deps, delete_deps) =
        (deps.clone(), deps.clone(), deps.clone(), deps.clone());
    let (list_service, create_service, update_service) =
        (service.clone(), service.clone(), service.clone());
    routes
        .get("/api/v1/admin/reminders", move |req| {
            resolve_current_admin(&list_deps)?;
            get_reminders(&list_service, req)
        })
        .post("/api/v1/admin/reminders", move |req| {
            let admin_id = resolve_current_admin(&create_deps)?;
            let input = read_reminder_request(req)?;
            let reminder = create_service.create_reminder(admin_id, &input)?;
            Ok(created_json(&ReminderResponse::from(reminder)))
        })
        .route(
            Method::Put,
            "/api/v1/admin/reminders/{reminderId}",
            move |req| {
                let admin_id = resolve_current_admin(&update_deps)?;
                let input = read_reminder_request(req)?;
                let reminder_id = path_uuid(req, "reminderId")?;
                let reminder = update_service.update_reminder(reminder_id, admin_id, &input)?;
                Ok(ok_json(&ReminderResponse::from(reminder)))
            },
        )
        .route(
            Method::Delete,
            "/api/v1/admin/reminders/{reminderId}",
            move |req| {
                let admin_id = resolve_current_admin(&delete_deps)?;
                let reminder_id = path_uuid(req, "reminderId")?;
                service.delete_reminder(reminder_id, admin_id)?;
                Ok(no_content())
            },
        )
}

fn get_reminders(service: &AdminReminderService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_reminders(page, page_size)?;
    Ok(ok_json(&ReminderListResponse {
        current_page: result.current_page,
        page_size: result.page_size,
        has_next: result.has_next,
        reminders: result
            .items
            .into_iter()
            .map(ReminderResponse::from)
            .collect(),
    }))
}

/// `AdminReminderRequest`: `@NotBlank title`, `@NotBlank body`, `@NotNull repeat`, `@NotNull target`, `@NotNull scheduledAt`.
fn read_reminder_request(req: &Request) -> Result<ReminderInput, ApiError> {
    let json = read_json_object(req)?;
    let title = json.string("title")?;
    let body = json.string("body")?;
    let repeat = body_enum(
        &json,
        "repeat",
        &[
            ("ONCE", ReminderRepeat::Once),
            ("DAILY", ReminderRepeat::Daily),
        ],
    )?;
    let target = body_enum(
        &json,
        "target",
        &[
            ("ALL", ReminderTarget::All),
            ("TOPIC_PARTICIPANTS", ReminderTarget::TopicParticipants),
            (
                "TOPIC_NON_PARTICIPANTS",
                ReminderTarget::TopicNonParticipants,
            ),
        ],
    )?;
    let scheduled_at = body_instant(&json, "scheduledAt")?;
    let (Some(repeat), Some(target), Some(scheduled_at)) = (repeat, target, scheduled_at) else {
        return Err(validation(MUST_NOT_BE_NULL));
    };
    match (title, body) {
        (Some(title), Some(body)) if has_text(&title) && has_text(&body) => Ok(ReminderInput {
            title,
            body,
            repeat,
            target,
            scheduled_at,
        }),
        _ => Err(validation(MUST_NOT_BE_BLANK)),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReminderListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    reminders: Vec<ReminderResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReminderResponse {
    reminder_id: String,
    title: String,
    body: String,
    repeat: &'static str,
    target: &'static str,
    #[serde(serialize_with = "serialize_instant")]
    scheduled_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: Timestamp,
}

impl From<Reminder> for ReminderResponse {
    fn from(reminder: Reminder) -> Self {
        ReminderResponse {
            reminder_id: reminder.id.unwrap_or_default().hyphenated().to_string(),
            title: reminder.title,
            body: reminder.body,
            repeat: reminder.repeat.as_str(),
            target: reminder.target.as_str(),
            scheduled_at: reminder.scheduled_at,
            created_at: reminder.created_at.unwrap_or(Timestamp::UNIX_EPOCH),
            updated_at: reminder.updated_at.unwrap_or(Timestamp::UNIX_EPOCH),
        }
    }
}
