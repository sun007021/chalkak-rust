//! 원본 `AdminAuditLogController`: `GET /api/v1/admin/audit-logs`.

use chalkak_http::Request;
use serde::Serialize;
use serde::ser::{SerializeMap, SerializeSeq};
use serde_json::Value;

use super::domain::{AdminAction, AdminTargetType};
use super::repository::{AuditLogSort, AuditLogSummary};
use super::service::{AuditLogQuery, AuditLogQueryService};
use crate::admin::binding::{QueryParams, page_params};
use crate::admin::support::{AdminDeps, AdminPage, resolve_current_admin};
use crate::web::json::serialize_instant;
use crate::web::{ApiResult, Routes, ok_json};

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let deps = deps.clone();
    let service = AuditLogQueryService::new(deps.clone());
    routes.get("/api/v1/admin/audit-logs", move |req| {
        get_audit_logs(&deps, &service, req)
    })
}

fn get_audit_logs(deps: &AdminDeps, service: &AuditLogQueryService, req: &Request) -> ApiResult {
    resolve_current_admin(deps)?;
    let q = QueryParams::new(req);
    let admin_id = q.uuid("adminId")?;
    let action = q.enumeration("action", AdminAction::parse)?;
    let target_type = q.enumeration("targetType", AdminTargetType::parse)?;
    let target_id = q.uuid("targetId")?;
    let occurred_from = q.instant("occurredFrom")?;
    let occurred_to = q.instant("occurredTo")?;
    let sort = q.sort("sort", AuditLogSort::from_query)?;
    let (page, page_size) = page_params(q.integer("page")?, q.integer("pageSize")?)?;
    let query = AuditLogQuery {
        admin_id,
        action,
        target_type,
        target_id,
        occurred_from,
        occurred_to,
        sort: sort.unwrap_or(AuditLogSort::OccurredAtDesc),
    };
    let result = service.get_audit_logs(&query, page, page_size)?;
    Ok(ok_json(&AuditLogListResponse::from(result)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditLogListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    audit_logs: Vec<AuditLogResponse>,
}

impl From<AdminPage<AuditLogSummary>> for AuditLogListResponse {
    fn from(page: AdminPage<AuditLogSummary>) -> Self {
        AuditLogListResponse {
            current_page: page.current_page,
            page_size: page.page_size,
            has_next: page.has_next,
            audit_logs: page.items.into_iter().map(AuditLogResponse::from).collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditLogResponse {
    audit_log_id: String,
    actor_admin_id: String,
    actor_username: String,
    action: &'static str,
    target_type: &'static str,
    target_id: String,
    reason: Option<String>,
    before_state: Jsonb,
    after_state: Jsonb,
    #[serde(serialize_with = "serialize_instant")]
    occurred_at: jiff::Timestamp,
    request_id: String,
}

impl From<AuditLogSummary> for AuditLogResponse {
    fn from(log: AuditLogSummary) -> Self {
        AuditLogResponse {
            audit_log_id: log.audit_log_id.to_string(),
            actor_admin_id: log.actor_admin_id.to_string(),
            actor_username: log.actor_username,
            action: log.action.as_str(),
            target_type: log.target_type.as_str(),
            target_id: log.target_id.to_string(),
            reason: log.reason,
            before_state: Jsonb(log.before_state),
            after_state: Jsonb(log.after_state),
            occurred_at: log.occurred_at,
            request_id: log.request_id.to_string(),
        }
    }
}

/// `jsonb` 컬럼 값. PostgreSQL이 객체 키를 (바이트 길이, 바이트 순)으로 정렬해 내보내므로, 원본(Hibernate가 읽은
/// 맵을 그대로 직렬화)과 같은 순서로 출력한다.
pub struct Jsonb(pub Value);

impl Serialize for Jsonb {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_ordered(&self.0, serializer)
    }
}

struct Ordered<'a>(&'a Value);

impl Serialize for Ordered<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_ordered(self.0, serializer)
    }
}

fn serialize_ordered<S: serde::Serializer>(
    value: &Value,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| (a.len(), a.as_bytes()).cmp(&(b.len(), b.as_bytes())));
            let mut out = serializer.serialize_map(Some(keys.len()))?;
            for key in keys {
                out.serialize_entry(key, &Ordered(&map[key]))?;
            }
            out.end()
        }
        Value::Array(items) => {
            let mut out = serializer.serialize_seq(Some(items.len()))?;
            for item in items {
                out.serialize_element(&Ordered(item))?;
            }
            out.end()
        }
        other => other.serialize(serializer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::json::to_json;
    use serde_json::json;

    #[test]
    fn jsonb는_키_길이_순으로_출력한다() {
        let value =
            json!({"topicDate": "d", "title": "t", "startsAt": "s", "endsAt": "e", "ab": 1});
        assert_eq!(
            to_json(&Jsonb(value)),
            r#"{"ab":1,"title":"t","endsAt":"e","startsAt":"s","topicDate":"d"}"#
        );
        assert_eq!(to_json(&Jsonb(json!({}))), "{}");
        assert_eq!(
            to_json(&Jsonb(json!({"b": null, "a": [{"yy": 1, "x": 2}]}))),
            r#"{"a":[{"x":2,"yy":1}],"b":null}"#
        );
    }
}
