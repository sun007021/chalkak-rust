//! 원본 `AdminAuditLogService`(생성)와 `AdminAuditLogQueryService`(조회).

use jiff::Timestamp;
use postgres::Transaction;
use uuid::Uuid;

use super::domain::{AdminAction, AdminAuditLog, AdminTargetType, AuditSnapshot};
use super::repository::{AuditLogCriteria, AuditLogRepository, AuditLogSort, AuditLogSummary};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, business, validate_pagination,
};
use crate::web::ApiError;

/// 감사 로그를 만들기 위한 요청(`AdminAuditLogCommand`).
#[derive(Clone, Debug)]
pub struct AuditLogCommand {
    pub actor_admin_id: Uuid,
    pub action: AdminAction,
    pub target_type: AdminTargetType,
    pub target_id: Uuid,
    pub reason: Option<String>,
    pub before_state: AuditSnapshot,
    pub after_state: AuditSnapshot,
}

/// `createAuditLog`: 관리자 업무 변경과 **같은 트랜잭션**에서만 만든다(`Propagation.MANDATORY`). 독립 트랜잭션을 열지
/// 않으므로 이후 업무가 실패하면 감사 INSERT도 함께 롤백된다. 현재 요청의 requestId를 붙인다.
pub fn create_audit_log(
    tx: &mut Transaction<'_>,
    command: AuditLogCommand,
    now: Timestamp,
) -> Result<AdminAuditLog, ApiError> {
    let mut log = AdminAuditLog::create(
        command.actor_admin_id,
        command.action,
        command.target_type,
        command.target_id,
        command.reason.as_deref(),
        &command.before_state,
        &command.after_state,
        now,
        chalkak_core::request_id::resolve_current(),
    )?;
    AuditLogRepository.save(tx, &mut log)?;
    Ok(log)
}

/// 목록 조회 조건(검증 전).
#[derive(Clone, Debug)]
pub struct AuditLogQuery {
    pub admin_id: Option<Uuid>,
    pub action: Option<AdminAction>,
    pub target_type: Option<AdminTargetType>,
    pub target_id: Option<Uuid>,
    pub occurred_from: Option<Timestamp>,
    pub occurred_to: Option<Timestamp>,
    pub sort: AuditLogSort,
}

#[derive(Clone)]
pub struct AuditLogQueryService {
    deps: AdminDeps,
}

impl AuditLogQueryService {
    pub fn new(deps: AdminDeps) -> Self {
        AuditLogQueryService { deps }
    }

    /// `getAuditLogs`: 페이지 검사 -> 기간 검사 -> 조회.
    pub fn get_audit_logs(
        &self,
        query: &AuditLogQuery,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<AuditLogSummary>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        if let (Some(from), Some(to)) = (query.occurred_from, query.occurred_to)
            && from > to
        {
            return Err(business("감사 로그 발생 시각 범위가 올바르지 않습니다."));
        }
        let criteria = AuditLogCriteria {
            admin_id: query.admin_id,
            action: query.action,
            target_type: query.target_type,
            target_id: query.target_id,
            occurred_from: query.occurred_from,
            occurred_to: query.occurred_to,
            sort: query.sort,
        };
        let mut conn = self.deps.pool.get()?;
        Ok(AuditLogRepository.find_audit_logs(&mut *conn, &criteria, page, page_size)?)
    }
}
