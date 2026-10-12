//! 원본 `admin.domain`의 감사 로그: `AdminAction`, `AdminTargetType`, `AdminAuditSnapshot`, `AdminAuditLog`,
//! `AdminAuditTransitionRules`.
//!
//! 감사 로그는 "허용한 업무 상태"만 담는다. 작업마다 허용한 필드와 상태 전이가 정해져 있고, 민감정보는 키와 값 양쪽에서 막는다.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use jiff::civil::Date;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::sensitive::contains_sensitive_value;
use crate::web::binding::is_java_whitespace;
use crate::web::json::format_instant;

const MAX_REASON_LENGTH: usize = 500;
const MAX_FIELD_COUNT: usize = 50;
const MAX_KEY_LENGTH: usize = 100;
const MAX_STRING_LENGTH: usize = 2_000;
const MAX_INSPECTION_DEPTH: usize = 5;
const MAX_INSPECTION_VALUE_COUNT: usize = 100;
const MAX_TOPIC_TITLE_LENGTH: usize = 255;

const INVALID_STATE_MESSAGE: &str = "관리자 감사 로그 상태가 올바르지 않습니다.";
const SENSITIVE_MESSAGE: &str = "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다.";
const INVALID_AUDIT_LOG_MESSAGE: &str = "관리자 감사 로그 정보가 올바르지 않습니다.";

const SENSITIVE_KEY_FRAGMENTS: [&str; 15] = [
    "password",
    "passwd",
    "token",
    "secret",
    "credential",
    "authorization",
    "cookie",
    "webhook",
    "fcm",
    "storagekey",
    "image",
    "photo",
    "signature",
    "thumbnail",
    "url",
];

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

fn invalid_state() -> AppError {
    business(INVALID_STATE_MESSAGE)
}

fn invalid_audit_log() -> AppError {
    business(INVALID_AUDIT_LOG_MESSAGE)
}

/// 민감정보 거절 오류. 메시지에 값을 싣지 않는다.
pub fn sensitive_value_error() -> AppError {
    business(SENSITIVE_MESSAGE)
}

/// 값 안에 민감정보가 있으면 거절한다(`AdminAuditSensitiveValueValidator.validate`).
pub fn validate_no_sensitive_value(value: &str) -> Result<(), AppError> {
    if contains_sensitive_value(value) {
        Err(sensitive_value_error())
    } else {
        Ok(())
    }
}

fn code_points(text: &str) -> usize {
    text.chars().count()
}

/// Java `String.trim()`: U+0020 이하의 앞뒤 문자를 뗀다.
fn java_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminTargetType {
    Post,
    User,
    Topic,
}

impl AdminTargetType {
    pub const fn as_str(self) -> &'static str {
        match self {
            AdminTargetType::Post => "POST",
            AdminTargetType::User => "USER",
            AdminTargetType::Topic => "TOPIC",
        }
    }

    pub fn parse(name: &str) -> Option<AdminTargetType> {
        Some(match name {
            "POST" => AdminTargetType::Post,
            "USER" => AdminTargetType::User,
            "TOPIC" => AdminTargetType::Topic,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminAction {
    PostApproved,
    PostRejected,
    PostDeleted,
    UserBanned,
    UserUnbanned,
    TopicCreated,
    TopicUpdated,
    TopicDeleted,
}

impl AdminAction {
    pub const ALL: [AdminAction; 8] = [
        AdminAction::PostApproved,
        AdminAction::PostRejected,
        AdminAction::PostDeleted,
        AdminAction::UserBanned,
        AdminAction::UserUnbanned,
        AdminAction::TopicCreated,
        AdminAction::TopicUpdated,
        AdminAction::TopicDeleted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AdminAction::PostApproved => "POST_APPROVED",
            AdminAction::PostRejected => "POST_REJECTED",
            AdminAction::PostDeleted => "POST_DELETED",
            AdminAction::UserBanned => "USER_BANNED",
            AdminAction::UserUnbanned => "USER_UNBANNED",
            AdminAction::TopicCreated => "TOPIC_CREATED",
            AdminAction::TopicUpdated => "TOPIC_UPDATED",
            AdminAction::TopicDeleted => "TOPIC_DELETED",
        }
    }

    pub fn parse(name: &str) -> Option<AdminAction> {
        AdminAction::ALL.into_iter().find(|a| a.as_str() == name)
    }

    pub const fn target_type(self) -> AdminTargetType {
        match self {
            AdminAction::PostApproved | AdminAction::PostRejected | AdminAction::PostDeleted => {
                AdminTargetType::Post
            }
            AdminAction::UserBanned | AdminAction::UserUnbanned => AdminTargetType::User,
            AdminAction::TopicCreated | AdminAction::TopicUpdated | AdminAction::TopicDeleted => {
                AdminTargetType::Topic
            }
        }
    }

    pub const fn reason_required(self) -> bool {
        matches!(
            self,
            AdminAction::PostRejected
                | AdminAction::PostDeleted
                | AdminAction::UserBanned
                | AdminAction::UserUnbanned
                | AdminAction::TopicDeleted
        )
    }

    fn is_valid_state_change(
        self,
        actor_admin_id: Uuid,
        before: &AuditSnapshot,
        after: &AuditSnapshot,
    ) -> bool {
        match self {
            AdminAction::PostApproved => {
                is_post_moderation(actor_admin_id, before, after, "APPROVED")
            }
            AdminAction::PostRejected => {
                is_post_moderation(actor_admin_id, before, after, "REJECTED")
            }
            AdminAction::PostDeleted => is_post_deleted(before, after),
            AdminAction::UserBanned => is_user_status_change(before, after, "ACTIVE", "BANNED"),
            AdminAction::UserUnbanned => is_user_status_change(before, after, "BANNED", "ACTIVE"),
            AdminAction::TopicCreated => {
                before.is_empty() && is_valid_topic_state(after, &TOPIC_STATE_FIELDS)
            }
            AdminAction::TopicUpdated => {
                is_valid_topic_state(before, &TOPIC_STATE_FIELDS)
                    && is_valid_topic_state(after, &TOPIC_STATE_FIELDS)
                    && before.values != after.values
            }
            AdminAction::TopicDeleted => is_topic_deleted(before, after),
        }
    }
}

/// 변경 전후 상태. 허용한 스칼라 값만 담는다(`AdminAuditSnapshot`).
#[derive(Clone, Debug, PartialEq)]
pub struct AuditSnapshot {
    values: Map<String, Value>,
}

impl AuditSnapshot {
    pub fn empty() -> AuditSnapshot {
        AuditSnapshot { values: Map::new() }
    }

    /// 키-값 쌍으로 만든다. UUID·시각·날짜는 호출 측이 원본 `toString()` 형태의 문자열로 넘긴다.
    pub fn of(pairs: Vec<(&str, Value)>) -> Result<AuditSnapshot, AppError> {
        let map = pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        AuditSnapshot::from_map(&map)
    }

    /// `AdminAuditSnapshot.from`: 개수·깊이·민감 키를 먼저 검사하고, 값을 스칼라로 정규화한다.
    pub fn from_map(values: &Map<String, Value>) -> Result<AuditSnapshot, AppError> {
        if values.len() > MAX_FIELD_COUNT {
            return Err(invalid_state());
        }
        let mut inspected = 0usize;
        inspect_entries(values, 0, &mut inspected)?;
        for key in values.keys() {
            validate_key(key)?;
        }
        let mut normalized = Map::new();
        for (key, value) in values {
            normalized.insert(key.clone(), normalize_scalar(value)?);
        }
        Ok(AuditSnapshot { values: normalized })
    }

    pub fn values(&self) -> &Map<String, Value> {
        &self.values
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn value(&self, field: &str) -> Option<&Value> {
        self.values.get(field)
    }

    fn has_exactly_fields(&self, fields: &[&str]) -> bool {
        self.values.len() == fields.len() && fields.iter().all(|f| self.values.contains_key(*f))
    }

    fn has_str(&self, field: &str, expected: &str) -> bool {
        self.value(field).and_then(Value::as_str) == Some(expected)
    }
}

fn validate_key(key: &str) -> Result<(), AppError> {
    if key.chars().all(is_java_whitespace) || code_points(key) > MAX_KEY_LENGTH {
        return Err(invalid_state());
    }
    Ok(())
}

fn validate_sensitive_key(key: &str) -> Result<(), AppError> {
    let normalized: String = key
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    if SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| normalized.contains(fragment))
    {
        return Err(sensitive_value_error());
    }
    Ok(())
}

fn inspect_entries(
    map: &Map<String, Value>,
    depth: usize,
    inspected: &mut usize,
) -> Result<(), AppError> {
    *inspected += 1;
    if depth > MAX_INSPECTION_DEPTH || *inspected > MAX_INSPECTION_VALUE_COUNT {
        return Err(invalid_state());
    }
    for (key, value) in map {
        validate_key(key)?;
        validate_sensitive_key(key)?;
        inspect_value(value, depth + 1, inspected)?;
    }
    Ok(())
}

fn inspect_value(value: &Value, depth: usize, inspected: &mut usize) -> Result<(), AppError> {
    match value {
        Value::Object(map) => inspect_entries(map, depth, inspected),
        Value::Array(items) => {
            *inspected += 1;
            if depth > MAX_INSPECTION_DEPTH || *inspected > MAX_INSPECTION_VALUE_COUNT {
                return Err(invalid_state());
            }
            for item in items {
                inspect_value(item, depth + 1, inspected)?;
            }
            Ok(())
        }
        _ => {
            *inspected += 1;
            if depth > MAX_INSPECTION_DEPTH || *inspected > MAX_INSPECTION_VALUE_COUNT {
                return Err(invalid_state());
            }
            Ok(())
        }
    }
}

fn normalize_scalar(value: &Value) -> Result<Value, AppError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(value.clone()),
        Value::String(text) => {
            if code_points(text) > MAX_STRING_LENGTH {
                return Err(invalid_state());
            }
            validate_no_sensitive_value(text)?;
            Ok(value.clone())
        }
        Value::Array(_) | Value::Object(_) => Err(invalid_state()),
    }
}

// ---------------------------------------------------------------------------------------------
// 상태 전이 규칙 (AdminAuditTransitionRules)
// ---------------------------------------------------------------------------------------------

const POST_MODERATION_BEFORE_FIELDS: [&str; 2] = ["moderationStatus", "moderatedAt"];
const POST_MODERATION_AFTER_FIELDS: [&str; 3] = ["moderationStatus", "moderatedAt", "moderatedBy"];
const POST_DELETION_FIELDS: [&str; 2] = ["moderationStatus", "deletedAt"];
const USER_STATUS_FIELDS: [&str; 1] = ["status"];
const TOPIC_STATE_FIELDS: [&str; 4] = ["title", "topicDate", "startsAt", "endsAt"];
const DELETED_TOPIC_STATE_FIELDS: [&str; 5] =
    ["title", "topicDate", "startsAt", "endsAt", "deletedAt"];
/// 게시물 `ModerationStatus`의 이름들.
const MODERATION_STATUSES: [&str; 4] = ["VALIDATING", "PENDING", "APPROVED", "REJECTED"];

fn is_post_moderation(
    actor_admin_id: Uuid,
    before: &AuditSnapshot,
    after: &AuditSnapshot,
    decided: &str,
) -> bool {
    before.has_exactly_fields(&POST_MODERATION_BEFORE_FIELDS)
        && after.has_exactly_fields(&POST_MODERATION_AFTER_FIELDS)
        && before.has_str("moderationStatus", "PENDING")
        && matches!(before.value("moderatedAt"), Some(Value::Null))
        && after.has_str("moderationStatus", decided)
        && is_instant(after.value("moderatedAt"))
        && after.has_str("moderatedBy", &actor_admin_id.to_string())
}

fn is_post_deleted(before: &AuditSnapshot, after: &AuditSnapshot) -> bool {
    let before_status = before.value("moderationStatus");
    before.has_exactly_fields(&POST_DELETION_FIELDS)
        && after.has_exactly_fields(&POST_DELETION_FIELDS)
        && before_status
            .and_then(Value::as_str)
            .is_some_and(|s| MODERATION_STATUSES.contains(&s))
        && before_status == after.value("moderationStatus")
        && matches!(before.value("deletedAt"), Some(Value::Null))
        && is_instant(after.value("deletedAt"))
}

fn is_user_status_change(
    before: &AuditSnapshot,
    after: &AuditSnapshot,
    from: &str,
    to: &str,
) -> bool {
    before.has_exactly_fields(&USER_STATUS_FIELDS)
        && after.has_exactly_fields(&USER_STATUS_FIELDS)
        && before.has_str("status", from)
        && after.has_str("status", to)
}

fn is_topic_deleted(before: &AuditSnapshot, after: &AuditSnapshot) -> bool {
    is_valid_topic_state(before, &DELETED_TOPIC_STATE_FIELDS)
        && is_valid_topic_state(after, &DELETED_TOPIC_STATE_FIELDS)
        && matches!(before.value("deletedAt"), Some(Value::Null))
        && is_instant(after.value("deletedAt"))
        && TOPIC_STATE_FIELDS
            .iter()
            .all(|field| before.value(field) == after.value(field))
}

fn is_valid_topic_state(state: &AuditSnapshot, expected_fields: &[&str]) -> bool {
    let starts_at = parse_instant(state.value("startsAt"));
    let ends_at = parse_instant(state.value("endsAt"));
    state.has_exactly_fields(expected_fields)
        && is_valid_title(state.value("title"))
        && is_local_date(state.value("topicDate"))
        && matches!((starts_at, ends_at), (Some(s), Some(e)) if e > s)
}

fn is_valid_title(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|title| {
        !title.chars().all(is_java_whitespace) && code_points(title) <= MAX_TOPIC_TITLE_LENGTH
    })
}

fn is_local_date(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| text.parse::<Date>().is_ok())
}

fn is_instant(value: Option<&Value>) -> bool {
    parse_instant(value).is_some()
}

fn parse_instant(value: Option<&Value>) -> Option<Timestamp> {
    value?.as_str()?.parse::<Timestamp>().ok()
}

// ---------------------------------------------------------------------------------------------
// 감사 로그
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct AdminAuditLog {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub actor_admin_id: Uuid,
    pub action: AdminAction,
    pub target_type: AdminTargetType,
    pub target_id: Uuid,
    pub reason: Option<String>,
    pub before_state: Map<String, Value>,
    pub after_state: Map<String, Value>,
    pub occurred_at: Timestamp,
    pub request_id: Uuid,
}

impl AdminAuditLog {
    /// `AdminAuditLog.create`. 대상 종류·상태 전이·사유를 검증한다.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        actor_admin_id: Uuid,
        action: AdminAction,
        target_type: AdminTargetType,
        target_id: Uuid,
        reason: Option<&str>,
        before: &AuditSnapshot,
        after: &AuditSnapshot,
        occurred_at: Timestamp,
        request_id: Uuid,
    ) -> Result<AdminAuditLog, AppError> {
        if before.is_empty() && after.is_empty() {
            return Err(invalid_audit_log());
        }
        if action.target_type() != target_type
            || !action.is_valid_state_change(actor_admin_id, before, after)
        {
            return Err(invalid_audit_log());
        }
        let reason = normalize_reason(action, reason)?;
        Ok(AdminAuditLog {
            id: None,
            actor_admin_id,
            action,
            target_type,
            target_id,
            reason,
            before_state: before.values.clone(),
            after_state: after.values.clone(),
            occurred_at,
            request_id,
        })
    }
}

fn normalize_reason(action: AdminAction, reason: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(reason) = reason.filter(|r| !r.chars().all(is_java_whitespace)) else {
        if action.reason_required() {
            return Err(invalid_audit_log());
        }
        return Ok(None);
    };
    let normalized = java_trim(reason);
    if code_points(normalized) > MAX_REASON_LENGTH {
        return Err(invalid_audit_log());
    }
    validate_no_sensitive_value(normalized)?;
    Ok(Some(normalized.to_owned()))
}

/// 상태 값 도우미: 시각을 원본 `Instant.toString()` 모양의 JSON 문자열로.
pub fn instant_value(at: Timestamp) -> Value {
    Value::String(format_instant(at))
}

pub fn uuid_value(id: Uuid) -> Value {
    Value::String(id.to_string())
}

pub fn date_value(date: Date) -> Value {
    Value::String(date.to_string())
}

pub fn text_value(text: &str) -> Value {
    Value::String(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::error::ErrorKind;
    use serde_json::json;

    fn id() -> Uuid {
        Uuid::new_v4()
    }

    fn occurred_at() -> Timestamp {
        "2026-08-27T11:20:00Z".parse().unwrap()
    }

    fn snap(value: Value) -> AuditSnapshot {
        AuditSnapshot::from_map(value.as_object().unwrap()).unwrap()
    }

    fn err_message(result: Result<impl std::fmt::Debug, AppError>) -> String {
        let e = result.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Business);
        e.message
    }

    // ----- AdminAuditSnapshotTest -----

    #[test]
    fn snapshot_민감_키는_대소문자와_구분자가_달라도_막는다() {
        let m = json!({"Refresh_Token": "secret-value"});
        let message = err_message(AuditSnapshot::from_map(m.as_object().unwrap()));
        assert_eq!(
            message,
            "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
        );
        assert!(!message.contains("secret-value"));
    }

    #[test]
    fn snapshot_허용_필드의_문자열에_숨긴_민감정보도_막는다() {
        for value in [
            "accessToken=secret-value",
            "https://hooks.slack.com/services/T000/B000/SECRET",
            "fcmToken=fake-device-registration-token",
            "https://storage.example.com/private/photo.webp?version=1",
            "data:image/png;base64,AAAA",
        ] {
            let m = json!({ "status": value });
            let message = err_message(AuditSnapshot::from_map(m.as_object().unwrap()));
            assert_eq!(
                message,
                "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
            );
            assert!(!message.contains(value));
        }
    }

    #[test]
    fn snapshot_숫자는_저장한다() {
        let s = snap(json!({"count": 1, "score": 1.5}));
        assert_eq!(s.values()["count"], json!(1));
        assert_eq!(s.values()["score"], json!(1.5));
    }

    #[test]
    fn snapshot_일반_문장과_문서_url은_저장한다() {
        let value = "토큰 만료 관련 문의: https://docs.example.com/admin/moderation";
        let s = snap(json!({ "title": value }));
        assert_eq!(s.values()["title"], json!(value));
    }

    #[test]
    fn snapshot_중첩_객체나_목록은_저장할_수_없다() {
        for v in [
            json!({"status": {"value": "APPROVED"}}),
            json!({"status": ["A"]}),
        ] {
            let message = err_message(AuditSnapshot::from_map(v.as_object().unwrap()));
            assert_eq!(message, "관리자 감사 로그 상태가 올바르지 않습니다.");
        }
    }

    #[test]
    fn snapshot_과도하게_중첩된_상태는_저장할_수_없다() {
        let v =
            json!({"level1": {"level2": {"level3": {"level4": {"level5": {"level6": "value"}}}}}});
        let message = err_message(AuditSnapshot::from_map(v.as_object().unwrap()));
        assert_eq!(message, "관리자 감사 로그 상태가 올바르지 않습니다.");
    }

    #[test]
    fn snapshot_문자열은_코드_포인트_2000자까지() {
        let ok = "📸".repeat(2_000);
        assert!(AuditSnapshot::from_map(json!({ "value": ok }).as_object().unwrap()).is_ok());
        let too_long = "가".repeat(2_001);
        let message = err_message(AuditSnapshot::from_map(
            json!({ "value": too_long }).as_object().unwrap(),
        ));
        assert_eq!(message, "관리자 감사 로그 상태가 올바르지 않습니다.");
    }

    #[test]
    fn snapshot_필드_수와_키_길이_제한() {
        let mut many = Map::new();
        for i in 0..51 {
            many.insert(format!("k{i}"), json!("v"));
        }
        assert_eq!(
            err_message(AuditSnapshot::from_map(&many)),
            "관리자 감사 로그 상태가 올바르지 않습니다."
        );
        many.remove("k50");
        assert!(AuditSnapshot::from_map(&many).is_ok());

        let long_key = "k".repeat(101);
        let m = json!({ long_key: "v" });
        assert_eq!(
            err_message(AuditSnapshot::from_map(m.as_object().unwrap())),
            "관리자 감사 로그 상태가 올바르지 않습니다."
        );
        let blank_key = json!({ " ": "v" });
        assert_eq!(
            err_message(AuditSnapshot::from_map(blank_key.as_object().unwrap())),
            "관리자 감사 로그 상태가 올바르지 않습니다."
        );
    }

    #[test]
    fn snapshot_검사_값_개수는_100개까지() {
        let items: Vec<Value> = (0..100).map(|_| json!("v")).collect();
        // 최상위 맵 1 + 배열 1 + 원소 100 = 102 > 100
        let m = json!({ "list": items });
        assert_eq!(
            err_message(AuditSnapshot::from_map(m.as_object().unwrap())),
            "관리자 감사 로그 상태가 올바르지 않습니다."
        );
    }

    // ----- AdminAuditLogTest -----

    fn pending() -> AuditSnapshot {
        snap(json!({"moderationStatus": "PENDING", "moderatedAt": null}))
    }

    fn decided(status: &str, by: Uuid) -> AuditSnapshot {
        snap(json!({
            "moderationStatus": status,
            "moderatedAt": "2026-08-27T11:20:00Z",
            "moderatedBy": by.to_string(),
        }))
    }

    fn create(
        action: AdminAction,
        target: AdminTargetType,
        reason: Option<&str>,
        before: &AuditSnapshot,
        after: &AuditSnapshot,
        actor: Uuid,
    ) -> Result<AdminAuditLog, AppError> {
        AdminAuditLog::create(
            actor,
            action,
            target,
            id(),
            reason,
            before,
            after,
            occurred_at(),
            id(),
        )
    }

    fn approve(
        reason: Option<&str>,
        before: &AuditSnapshot,
        after: &AuditSnapshot,
        actor: Uuid,
    ) -> Result<AdminAuditLog, AppError> {
        create(
            AdminAction::PostApproved,
            AdminTargetType::Post,
            reason,
            before,
            after,
            actor,
        )
    }

    #[test]
    fn create_올바른_정보로_감사_로그를_만들고_사유를_다듬는다() {
        let actor = id();
        let log = approve(
            Some("  검수 기준 충족  "),
            &pending(),
            &decided("APPROVED", actor),
            actor,
        )
        .unwrap();
        assert_eq!(log.reason.as_deref(), Some("검수 기준 충족"));
        assert_eq!(log.before_state["moderationStatus"], json!("PENDING"));
        assert_eq!(log.before_state["moderatedAt"], Value::Null);
        assert_eq!(
            log.after_state["moderatedAt"],
            json!("2026-08-27T11:20:00Z")
        );
        assert_eq!(log.id, None);
    }

    #[test]
    fn create_선택_사유가_비어_있으면_null로_정규화한다() {
        let actor = id();
        let log = approve(Some("   "), &pending(), &decided("APPROVED", actor), actor).unwrap();
        assert_eq!(log.reason, None);
        let log = approve(None, &pending(), &decided("APPROVED", actor), actor).unwrap();
        assert_eq!(log.reason, None);
    }

    #[test]
    fn create_대상_종류가_작업과_다르면_거절한다() {
        let actor = id();
        let message = err_message(create(
            AdminAction::PostApproved,
            AdminTargetType::User,
            None,
            &pending(),
            &decided("APPROVED", actor),
            actor,
        ));
        assert_eq!(message, "관리자 감사 로그 정보가 올바르지 않습니다.");
    }

    #[test]
    fn create_승인_작업에_거절_상태_전이를_기록할_수_없다() {
        let actor = id();
        let message = err_message(approve(
            None,
            &pending(),
            &decided("REJECTED", actor),
            actor,
        ));
        assert_eq!(message, "관리자 감사 로그 정보가 올바르지 않습니다.");
    }

    #[test]
    fn create_승인_작업에는_검수_시각과_작업자가_모두_필요하다() {
        let actor = id();
        let after = snap(json!({"moderationStatus": "APPROVED"}));
        assert_eq!(
            err_message(approve(None, &pending(), &after, actor)),
            "관리자 감사 로그 정보가 올바르지 않습니다."
        );
    }

    #[test]
    fn create_검수_작업자는_감사_로그_작업자와_같아야_한다() {
        let actor = id();
        assert_eq!(
            err_message(approve(None, &pending(), &decided("APPROVED", id()), actor)),
            "관리자 감사 로그 정보가 올바르지 않습니다."
        );
    }

    #[test]
    fn create_회원_정지_작업에_같은_상태를_기록할_수_없다() {
        let same = snap(json!({"status": "ACTIVE"}));
        assert_eq!(
            err_message(create(
                AdminAction::UserBanned,
                AdminTargetType::User,
                Some("운영 정책 위반"),
                &same,
                &same,
                id()
            )),
            "관리자 감사 로그 정보가 올바르지 않습니다."
        );
    }

    fn topic_state(title: &str) -> Value {
        json!({
            "title": title,
            "topicDate": "2026-08-27",
            "startsAt": "2026-08-27T00:00:00Z",
            "endsAt": "2026-08-27T12:00:00Z",
        })
    }

    #[test]
    fn create_주제_수정에는_수정_전후의_전체_업무_상태가_필요하다() {
        let partial = snap(json!({"title": "새 주제"}));
        assert_eq!(
            err_message(create(
                AdminAction::TopicUpdated,
                AdminTargetType::Topic,
                None,
                &snap(topic_state("이전 주제")),
                &partial,
                id()
            )),
            "관리자 감사 로그 정보가 올바르지 않습니다."
        );
    }

    #[test]
    fn create_작업별_올바른_상태_전이를_감사_로그로_만든다() {
        let actor = id();
        let deleted_post =
            |at: Value| snap(json!({"moderationStatus": "APPROVED", "deletedAt": at}));
        let topic_deleted = |at: Value| {
            let mut v = topic_state("새 주제");
            v["deletedAt"] = at;
            snap(v)
        };
        let cases: Vec<(AdminAction, Option<&str>, AuditSnapshot, AuditSnapshot)> = vec![
            (
                AdminAction::PostApproved,
                None,
                pending(),
                decided("APPROVED", actor),
            ),
            (
                AdminAction::PostRejected,
                Some("검수 기준 미충족"),
                pending(),
                decided("REJECTED", actor),
            ),
            (
                AdminAction::PostDeleted,
                Some("운영 정책 위반"),
                deleted_post(Value::Null),
                deleted_post(json!("2026-08-27T11:20:00Z")),
            ),
            (
                AdminAction::UserBanned,
                Some("운영 정책 위반"),
                snap(json!({"status": "ACTIVE"})),
                snap(json!({"status": "BANNED"})),
            ),
            (
                AdminAction::UserUnbanned,
                Some("정지 사유 해소"),
                snap(json!({"status": "BANNED"})),
                snap(json!({"status": "ACTIVE"})),
            ),
            (
                AdminAction::TopicCreated,
                None,
                AuditSnapshot::empty(),
                snap(topic_state("새 주제")),
            ),
            (
                AdminAction::TopicUpdated,
                None,
                snap(topic_state("이전 주제")),
                snap(topic_state("새 주제")),
            ),
            (
                AdminAction::TopicDeleted,
                Some("잘못 생성된 주제"),
                topic_deleted(Value::Null),
                topic_deleted(json!("2026-08-27T11:20:00Z")),
            ),
        ];
        for (action, reason, before, after) in cases {
            let log = create(action, action.target_type(), reason, &before, &after, actor).unwrap();
            assert_eq!(log.action, action);
            assert_eq!(log.target_type, action.target_type());
        }
    }

    #[test]
    fn create_주제_삭제는_업무_상태가_그대로여야_한다() {
        let mut before = topic_state("a");
        before["deletedAt"] = Value::Null;
        let mut after = topic_state("b");
        after["deletedAt"] = json!("2026-08-27T11:20:00Z");
        assert_eq!(
            err_message(create(
                AdminAction::TopicDeleted,
                AdminTargetType::Topic,
                Some("사유"),
                &snap(before),
                &snap(after),
                id()
            )),
            "관리자 감사 로그 정보가 올바르지 않습니다."
        );
    }

    #[test]
    fn create_사유는_코드_포인트_500자까지() {
        let actor = id();
        let ok = "📸".repeat(500);
        let log = approve(Some(&ok), &pending(), &decided("APPROVED", actor), actor).unwrap();
        assert_eq!(log.reason.as_deref().unwrap().chars().count(), 500);
        let message = err_message(approve(
            Some(&"가".repeat(501)),
            &pending(),
            &decided("APPROVED", actor),
            actor,
        ));
        assert_eq!(message, "관리자 감사 로그 정보가 올바르지 않습니다.");
    }

    #[test]
    fn create_사유에_민감정보_값이_있으면_거절한다() {
        let actor = id();
        for reason in [
            "처리 사유 accessToken=secret-value",
            "https://hooks.slack.com/services/T000/B000/SECRET",
            "fcmToken=fake-device-registration-token",
            "https://storage.example.com/private/photo.webp?version=1",
            "data:image/png;base64,AAAA",
        ] {
            let message = err_message(approve(
                Some(reason),
                &pending(),
                &decided("APPROVED", actor),
                actor,
            ));
            assert_eq!(
                message,
                "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
            );
        }
        let benign = "토큰 만료 관련 문의: https://docs.example.com/admin/moderation";
        assert!(approve(Some(benign), &pending(), &decided("APPROVED", actor), actor).is_ok());
    }

    #[test]
    fn create_거절_작업에_사유가_없으면_거절한다() {
        let actor = id();
        for reason in [None, Some("   ")] {
            let message = err_message(create(
                AdminAction::PostRejected,
                AdminTargetType::Post,
                reason,
                &pending(),
                &decided("REJECTED", actor),
                actor,
            ));
            assert_eq!(message, "관리자 감사 로그 정보가 올바르지 않습니다.");
        }
    }

    #[test]
    fn create_변경된_상태가_없으면_거절한다() {
        let message = err_message(approve(
            None,
            &AuditSnapshot::empty(),
            &AuditSnapshot::empty(),
            id(),
        ));
        assert_eq!(message, "관리자 감사 로그 정보가 올바르지 않습니다.");
    }

    #[test]
    fn create_허용_목록_밖의_필드는_거절한다() {
        let actor = id();
        let mut extra = decided("APPROVED", actor).values().clone();
        extra.insert("extra".into(), json!("x"));
        let after = AuditSnapshot::from_map(&extra).unwrap();
        assert_eq!(
            err_message(approve(None, &pending(), &after, actor)),
            "관리자 감사 로그 정보가 올바르지 않습니다."
        );
    }

    #[test]
    fn create_상태에_비밀번호나_토큰_키가_있으면_거절한다() {
        let nested = json!({"account": {"passwordHash": "hash", "accessToken": "token"}});
        assert_eq!(
            err_message(AuditSnapshot::from_map(nested.as_object().unwrap())),
            "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
        );
    }

    #[test]
    fn create_이미지_url이나_스토리지_키가_있으면_거절한다() {
        let nested = json!({"changes": [{"originalStorageKey": "chalkak/private/image.webp"}]});
        assert_eq!(
            err_message(AuditSnapshot::from_map(nested.as_object().unwrap())),
            "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
        );
    }

    #[test]
    fn 작업_표는_원본과_같다() {
        let required: Vec<&str> = AdminAction::ALL
            .into_iter()
            .filter(|a| a.reason_required())
            .map(AdminAction::as_str)
            .collect();
        assert_eq!(
            required,
            [
                "POST_REJECTED",
                "POST_DELETED",
                "USER_BANNED",
                "USER_UNBANNED",
                "TOPIC_DELETED"
            ]
        );
        for action in AdminAction::ALL {
            assert_eq!(AdminAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(AdminAction::parse("post_approved"), None);
        assert_eq!(
            AdminTargetType::parse("TOPIC"),
            Some(AdminTargetType::Topic)
        );
        assert_eq!(AdminTargetType::parse("x"), None);
    }
}
