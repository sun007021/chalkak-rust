//! 원본 `AdminAuditLogRepositoryImpl`·`AdminAuditLogJpaRepository`: 감사 로그 저장과 목록 조회.

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::GenericClient;
use serde_json::Value;
use uuid::Uuid;

use super::domain::{AdminAction, AdminAuditLog, AdminTargetType};
use crate::admin::support::{AdminPage, SqlFilter, limit_offset};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditLogSort {
    OccurredAtDesc,
    OccurredAtAsc,
}

impl AuditLogSort {
    /// `AdminAuditLogSort.from`의 질의 값.
    pub fn from_query(value: &str) -> Option<AuditLogSort> {
        match value {
            "occurredAtDesc" => Some(AuditLogSort::OccurredAtDesc),
            "occurredAtAsc" => Some(AuditLogSort::OccurredAtAsc),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuditLogCriteria {
    pub admin_id: Option<Uuid>,
    pub action: Option<AdminAction>,
    pub target_type: Option<AdminTargetType>,
    pub target_id: Option<Uuid>,
    pub occurred_from: Option<Timestamp>,
    pub occurred_to: Option<Timestamp>,
    pub sort: AuditLogSort,
}

/// 목록의 한 줄(`AdminAuditLogSummaryProjection`).
#[derive(Clone, Debug, PartialEq)]
pub struct AuditLogSummary {
    pub audit_log_id: Uuid,
    pub actor_admin_id: Uuid,
    pub actor_username: String,
    pub action: AdminAction,
    pub target_type: AdminTargetType,
    pub target_id: Uuid,
    pub reason: Option<String>,
    pub before_state: Value,
    pub after_state: Value,
    pub occurred_at: Timestamp,
    pub request_id: Uuid,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AuditLogRepository;

impl AuditLogRepository {
    /// `save`: 새 감사 로그를 넣고 DB가 정한 식별자를 채워 돌려준다.
    pub fn save(
        &self,
        db: &mut impl GenericClient,
        log: &mut AdminAuditLog,
    ) -> Result<(), DbError> {
        let before = Value::Object(log.before_state.clone()).to_string();
        let after = Value::Object(log.after_state.clone()).to_string();
        let row = db.query_one(
            "INSERT INTO admin_audit_logs \
             (actor_admin_id, action, target_type, target_id, reason, before_state, after_state, occurred_at, request_id) \
             VALUES ($1, $2::text::admin_action, $3::text::admin_target_type, $4, $5, $6::text::jsonb, $7::text::jsonb, $8, $9) \
             RETURNING id",
            &[
                &log.actor_admin_id,
                &log.action.as_str(),
                &log.target_type.as_str(),
                &log.target_id,
                &log.reason,
                &before,
                &after,
                &log.occurred_at,
                &log.request_id,
            ],
        )?;
        log.id = Some(row.try_get("id")?);
        Ok(())
    }

    /// `findAuditLogs`
    pub fn find_audit_logs(
        &self,
        db: &mut impl GenericClient,
        criteria: &AuditLogCriteria,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<AuditLogSummary>, DbError> {
        let mut filter = SqlFilter::new();
        filter.add_opt("l.actor_admin_id = ?", criteria.admin_id);
        filter.add_opt(
            "l.action = ?::text::admin_action",
            criteria.action.map(|a| a.as_str().to_owned()),
        );
        filter.add_opt(
            "l.target_type = ?::text::admin_target_type",
            criteria.target_type.map(|t| t.as_str().to_owned()),
        );
        filter.add_opt("l.target_id = ?", criteria.target_id);
        filter.add_opt("l.occurred_at >= ?", criteria.occurred_from);
        filter.add_opt("l.occurred_at <= ?", criteria.occurred_to);
        let direction = match criteria.sort {
            AuditLogSort::OccurredAtDesc => "DESC",
            AuditLogSort::OccurredAtAsc => "ASC",
        };
        let (limit, offset) = limit_offset(page, page_size);
        let sql = format!(
            "SELECT l.id, l.actor_admin_id, a.username, l.action::text AS action, \
             l.target_type::text AS target_type, l.target_id, l.reason, \
             l.before_state::text AS before_state, l.after_state::text AS after_state, \
             l.occurred_at, l.request_id \
             FROM admin_audit_logs l JOIN admins a ON a.id = l.actor_admin_id{} \
             ORDER BY l.occurred_at {direction}, l.id {direction} LIMIT ${} OFFSET ${}",
            filter.where_clause(),
            filter.next_placeholder(),
            filter.next_placeholder() + 1,
        );
        let params = filter.params_with(&[&limit, &offset]);
        let rows = db.query(&sql, &params)?;
        let items = rows
            .iter()
            .map(|row| {
                let action: String = row.try_get("action")?;
                let target_type: String = row.try_get("target_type")?;
                let before: String = row.try_get("before_state")?;
                let after: String = row.try_get("after_state")?;
                Ok(AuditLogSummary {
                    audit_log_id: row.try_get("id")?,
                    actor_admin_id: row.try_get("actor_admin_id")?,
                    actor_username: row.try_get("username")?,
                    action: AdminAction::parse(&action).ok_or_else(|| {
                        DbError::Config(format!("알 수 없는 관리자 작업: {action}"))
                    })?,
                    target_type: AdminTargetType::parse(&target_type).ok_or_else(|| {
                        DbError::Config(format!("알 수 없는 대상 종류: {target_type}"))
                    })?,
                    target_id: row.try_get("target_id")?,
                    reason: row.try_get("reason")?,
                    before_state: serde_json::from_str(&before)
                        .map_err(|e| DbError::Config(e.to_string()))?,
                    after_state: serde_json::from_str(&after)
                        .map_err(|e| DbError::Config(e.to_string()))?,
                    occurred_at: row.try_get("occurred_at")?,
                    request_id: row.try_get("request_id")?,
                })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        Ok(AdminPage::from_overfetched(items, page, page_size))
    }
}
