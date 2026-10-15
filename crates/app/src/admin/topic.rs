//! 관리자 주제 관리(원본 `AdminTopicController`, `AdminTopicQueryService`, `AdminTopicCommandService`,
//! `AdminTopicQueryRepositoryImpl`): `/api/v1/admin/topics`.
//!
//! 주제 쓰기(생성·수정·삭제 규칙과 제약 위반 번역)는 `topic` 모듈의 도메인·저장소를 그대로 쓰고, 여기서는 관리자 목록·상세
//! 조회(게시물 수 집계)와 감사 로그 연결만 더한다.

use chalkak_db::Db;
use chalkak_db::DbError;
use chalkak_http::{Method, Request};
use jiff::Timestamp;
use jiff::civil::Date;
use serde::Serialize;
use uuid::Uuid;

use crate::admin::audit::domain::{
    AdminAction, AdminTargetType, AuditSnapshot, date_value, instant_value, text_value,
};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{
    MUST_NOT_BE_BLANK, MUST_NOT_BE_NULL, QueryParams, body_instant, body_local_date, code_points,
    has_text, java_trim, page_params, size_message, utf16_len, validation,
};
use crate::admin::support::{
    AdminDeps, AdminPage, business, in_transaction, limit_offset, no_content, not_found, path_uuid,
    resolve_current_admin,
};
use crate::topic::domain::{ParticipationPeriod, Topic, TopicPhase};
use crate::topic::repository::{SaveError, TopicRepository};
use crate::web::binding::read_json_object;
use crate::web::json::serialize_instant;
use crate::web::{ApiError, ApiResult, Routes, created_json, ok_json};

const INVALID_REQUEST_MESSAGE: &str = "관리자 주제 요청이 올바르지 않습니다.";
const MAX_REASON_LENGTH: usize = 500;
const NOT_FOUND_MESSAGE: &str = "주제를 찾을 수 없습니다.";

fn invalid_request() -> ApiError {
    business(INVALID_REQUEST_MESSAGE)
}

fn parse_phase(name: &str) -> Option<TopicPhase> {
    match name {
        "BEFORE_OPEN" => Some(TopicPhase::BeforeOpen),
        "OPEN" => Some(TopicPhase::Open),
        "CLOSED" => Some(TopicPhase::Closed),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminTopicSort {
    TopicDateDesc,
    TopicDateAsc,
    CreatedAtDesc,
    CreatedAtAsc,
}

impl AdminTopicSort {
    pub fn from_query(value: &str) -> Option<AdminTopicSort> {
        match value {
            "topicDateDesc" => Some(AdminTopicSort::TopicDateDesc),
            "topicDateAsc" => Some(AdminTopicSort::TopicDateAsc),
            "createdAtDesc" => Some(AdminTopicSort::CreatedAtDesc),
            "createdAtAsc" => Some(AdminTopicSort::CreatedAtAsc),
            _ => None,
        }
    }

    const fn order_by(self) -> &'static str {
        match self {
            AdminTopicSort::TopicDateDesc => "t.topic_date DESC, t.id DESC",
            AdminTopicSort::TopicDateAsc => "t.topic_date ASC, t.id ASC",
            AdminTopicSort::CreatedAtDesc => "t.created_at DESC, t.id DESC",
            AdminTopicSort::CreatedAtAsc => "t.created_at ASC, t.id ASC",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TopicQueryCriteria {
    pub phase: Option<TopicPhase>,
    pub date_from: Option<Date>,
    pub date_to: Option<Date>,
    pub sort: AdminTopicSort,
    pub now: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopicPostCounts {
    pub pending: i64,
    pub approved: i64,
    pub rejected: i64,
}

/// 목록·상세의 한 주제(`AdminTopicDetail`).
#[derive(Clone, Debug)]
pub struct AdminTopicDetail {
    pub topic_id: Uuid,
    pub title: String,
    pub topic_date: Date,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub phase: TopicPhase,
    pub post_counts: TopicPostCounts,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

const TOPIC_SELECT: &str = "SELECT t.id, t.title, t.topic_date, t.starts_at, t.ends_at, \
    t.created_at, t.updated_at, \
    COUNT(p.id) FILTER (WHERE p.moderation_status = 'PENDING') AS pending_count, \
    COUNT(p.id) FILTER (WHERE p.moderation_status = 'APPROVED') AS approved_count, \
    COUNT(p.id) FILTER (WHERE p.moderation_status = 'REJECTED') AS rejected_count \
    FROM topics t LEFT JOIN posts p ON p.topic_id = t.id AND p.deleted_at IS NULL";
const TOPIC_GROUP_BY: &str =
    " GROUP BY t.id, t.title, t.topic_date, t.starts_at, t.ends_at, t.created_at, t.updated_at";

fn detail_from_row(row: &postgres::Row, now: Timestamp) -> Result<AdminTopicDetail, DbError> {
    let starts_at: Timestamp = row.try_get("starts_at")?;
    let ends_at: Timestamp = row.try_get("ends_at")?;
    Ok(AdminTopicDetail {
        topic_id: row.try_get("id")?,
        title: row.try_get("title")?,
        topic_date: row.try_get("topic_date")?,
        starts_at,
        ends_at,
        phase: ParticipationPeriod::new(starts_at, ends_at)
            .map(|p| p.phase_at(now))
            .unwrap_or(TopicPhase::Closed),
        post_counts: TopicPostCounts {
            pending: row.try_get("pending_count")?,
            approved: row.try_get("approved_count")?,
            rejected: row.try_get("rejected_count")?,
        },
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AdminTopicQueryRepository;

impl AdminTopicQueryRepository {
    pub fn find_topics(
        &self,
        db: &mut impl Db,
        criteria: &TopicQueryCriteria,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<AdminTopicDetail>, DbError> {
        let mut filter = crate::admin::support::SqlFilter::new();
        filter.add_fixed("t.deleted_at IS NULL");
        match criteria.phase {
            None => {}
            Some(TopicPhase::BeforeOpen) => filter.add("t.starts_at > ?", criteria.now),
            Some(TopicPhase::Open) => {
                filter.add("t.starts_at <= ?", criteria.now);
                filter.add("t.ends_at > ?", criteria.now);
            }
            Some(TopicPhase::Closed) => filter.add("t.ends_at <= ?", criteria.now),
        }
        filter.add_opt("t.topic_date >= ?", criteria.date_from);
        filter.add_opt("t.topic_date <= ?", criteria.date_to);
        let (limit, offset) = limit_offset(page, page_size);
        let sql = format!(
            "{TOPIC_SELECT}{}{TOPIC_GROUP_BY} ORDER BY {} LIMIT ${} OFFSET ${}",
            filter.where_clause(),
            criteria.sort.order_by(),
            filter.next_placeholder(),
            filter.next_placeholder() + 1,
        );
        let params = filter.params_with(&[&limit, &offset]);
        let rows = db.query(&sql, &params)?;
        let items = rows
            .iter()
            .map(|row| detail_from_row(row, criteria.now))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AdminPage::from_overfetched(items, page, page_size))
    }

    pub fn find_active_topic_by_id(
        &self,
        db: &mut impl Db,
        topic_id: Uuid,
        now: Timestamp,
    ) -> Result<Option<AdminTopicDetail>, DbError> {
        let sql = format!(
            "{TOPIC_SELECT} WHERE t.id = $1 AND t.deleted_at IS NULL{TOPIC_GROUP_BY} LIMIT 1"
        );
        let row = db.query_opt(&sql, &[&topic_id])?;
        row.map(|row| detail_from_row(&row, now)).transpose()
    }
}

#[derive(Clone)]
pub struct AdminTopicQueryService {
    deps: AdminDeps,
}

impl AdminTopicQueryService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminTopicQueryService { deps }
    }

    pub fn get_topics(
        &self,
        phase: Option<TopicPhase>,
        date_from: Option<Date>,
        date_to: Option<Date>,
        sort: AdminTopicSort,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<AdminTopicDetail>, ApiError> {
        validate_query(date_from, date_to, page, page_size)?;
        let now = self.deps.now();
        let criteria = TopicQueryCriteria {
            phase,
            date_from,
            date_to,
            sort,
            now,
        };
        let mut conn = self.deps.pool.get()?;
        Ok(AdminTopicQueryRepository.find_topics(&mut conn, &criteria, page, page_size)?)
    }

    pub fn get_topic(&self, topic_id: Uuid) -> Result<AdminTopicDetail, ApiError> {
        let mut conn = self.deps.pool.get()?;
        read_topic(&mut conn, topic_id, self.deps.now())
    }
}

fn read_topic(
    db: &mut impl Db,
    topic_id: Uuid,
    now: Timestamp,
) -> Result<AdminTopicDetail, ApiError> {
    AdminTopicQueryRepository
        .find_active_topic_by_id(db, topic_id, now)?
        .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))
}

fn validate_query(
    date_from: Option<Date>,
    date_to: Option<Date>,
    page: i32,
    page_size: i32,
) -> Result<(), ApiError> {
    let offset = (i64::from(page) - 1) * i64::from(page_size);
    let inverted = matches!((date_from, date_to), (Some(from), Some(to)) if from > to);
    if page < 1 || !(1..=100).contains(&page_size) || offset > i64::from(i32::MAX) || inverted {
        return Err(invalid_request());
    }
    Ok(())
}

/// 생성·수정 요청의 값(검증을 통과한 뒤).
#[derive(Clone, Debug)]
pub struct TopicMutation {
    pub title: String,
    pub topic_date: Date,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
}

#[derive(Clone)]
pub struct AdminTopicCommandService {
    deps: AdminDeps,
}

fn save_error(e: SaveError) -> ApiError {
    match e {
        SaveError::Business(e) => ApiError::App(e),
        SaveError::Db(e) => ApiError::from(e),
    }
}

impl AdminTopicCommandService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminTopicCommandService { deps }
    }

    pub fn create_topic(
        &self,
        admin_id: Uuid,
        request: &TopicMutation,
    ) -> Result<AdminTopicDetail, ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = TopicRepository;
            let period = ParticipationPeriod::new(request.starts_at, request.ends_at)?;
            let mut topic = Topic::create(&request.title, request.topic_date, period, now)?;
            if repo.exists_active_by_topic_date(tx, request.topic_date)? {
                return Err(business("해당 날짜의 주제가 이미 존재합니다."));
            }
            if repo.exists_active_overlapping_period(tx, request.starts_at, request.ends_at)? {
                return Err(business("다른 주제의 참여 기간과 겹칩니다."));
            }
            repo.save_and_flush(tx, &mut topic, now)
                .map_err(save_error)?;
            let topic_id = topic.id.ok_or_else(invalid_request)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::TopicCreated,
                    target_type: AdminTargetType::Topic,
                    target_id: topic_id,
                    reason: None,
                    before_state: AuditSnapshot::empty(),
                    after_state: topic_state(&topic)?,
                },
                now,
            )?;
            read_topic(tx, topic_id, now)
        })
    }

    pub fn update_topic(
        &self,
        topic_id: Uuid,
        admin_id: Uuid,
        request: &TopicMutation,
    ) -> Result<AdminTopicDetail, ApiError> {
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = TopicRepository;
            let mut topic = repo
                .find_active_by_id_for_update(tx, topic_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            let before = topic_state(&topic)?;
            topic.validate_editable(now)?;
            let period = ParticipationPeriod::new(request.starts_at, request.ends_at)?;
            if repo.exists_active_by_topic_date_excluding_id(tx, request.topic_date, topic_id)? {
                return Err(business("해당 날짜의 주제가 이미 존재합니다."));
            }
            if repo.exists_active_overlapping_period_excluding_id(
                tx,
                request.starts_at,
                request.ends_at,
                topic_id,
            )? {
                return Err(business("다른 주제의 참여 기간과 겹칩니다."));
            }
            topic.update(&request.title, request.topic_date, period, now)?;
            repo.save_and_flush(tx, &mut topic, now)
                .map_err(save_error)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::TopicUpdated,
                    target_type: AdminTargetType::Topic,
                    target_id: topic_id,
                    reason: None,
                    before_state: before,
                    after_state: topic_state(&topic)?,
                },
                now,
            )?;
            read_topic(tx, topic_id, now)
        })
    }

    pub fn delete_topic(
        &self,
        topic_id: Uuid,
        admin_id: Uuid,
        reason: &str,
    ) -> Result<(), ApiError> {
        let reason = normalize_reason(reason)?;
        let now = self.deps.now();
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let repo = TopicRepository;
            let mut topic = repo
                .find_active_by_id_for_update(tx, topic_id)?
                .ok_or_else(|| not_found(NOT_FOUND_MESSAGE))?;
            let before = deletion_state(&topic)?;
            topic.delete(now)?;
            repo.save_and_flush(tx, &mut topic, now)
                .map_err(save_error)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::TopicDeleted,
                    target_type: AdminTargetType::Topic,
                    target_id: topic_id,
                    reason: Some(reason.clone()),
                    before_state: before,
                    after_state: deletion_state(&topic)?,
                },
                now,
            )?;
            Ok(())
        })
    }
}

fn normalize_reason(reason: &str) -> Result<String, ApiError> {
    if !has_text(reason) {
        return Err(invalid_request());
    }
    let normalized = java_trim(reason);
    if code_points(normalized) > MAX_REASON_LENGTH {
        return Err(invalid_request());
    }
    Ok(normalized.to_owned())
}

fn topic_pairs(topic: &Topic) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("title", text_value(&topic.title)),
        ("topicDate", date_value(topic.topic_date)),
        (
            "startsAt",
            instant_value(topic.participation_period.starts_at()),
        ),
        (
            "endsAt",
            instant_value(topic.participation_period.ends_at()),
        ),
    ]
}

fn topic_state(topic: &Topic) -> Result<AuditSnapshot, ApiError> {
    Ok(AuditSnapshot::of(topic_pairs(topic))?)
}

fn deletion_state(topic: &Topic) -> Result<AuditSnapshot, ApiError> {
    let mut pairs = topic_pairs(topic);
    pairs.push((
        "deletedAt",
        topic
            .deleted_at
            .map_or(serde_json::Value::Null, instant_value),
    ));
    Ok(AuditSnapshot::of(pairs)?)
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps) -> Routes {
    let query = AdminTopicQueryService::new(deps.clone());
    let command = AdminTopicCommandService::new(deps.clone());
    let (list_deps, create_deps, detail_deps, update_deps, delete_deps) = (
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
    );
    let (detail_query, create_command, update_command) =
        (query.clone(), command.clone(), command.clone());
    routes
        .get("/api/v1/admin/topics", move |req| {
            resolve_current_admin(&list_deps)?;
            get_topics(&query, req)
        })
        .post("/api/v1/admin/topics", move |req| {
            let admin_id = resolve_current_admin(&create_deps)?;
            let request = read_mutation(req)?;
            let detail = create_command.create_topic(admin_id, &request)?;
            Ok(created_json(&TopicDetailResponse::from(detail)))
        })
        .get("/api/v1/admin/topics/{topicId}", move |req| {
            resolve_current_admin(&detail_deps)?;
            let detail = detail_query.get_topic(path_uuid(req, "topicId")?)?;
            Ok(ok_json(&TopicDetailResponse::from(detail)))
        })
        .route(Method::Put, "/api/v1/admin/topics/{topicId}", move |req| {
            let admin_id = resolve_current_admin(&update_deps)?;
            let request = read_mutation(req)?;
            let topic_id = path_uuid(req, "topicId")?;
            let detail = update_command.update_topic(topic_id, admin_id, &request)?;
            Ok(ok_json(&TopicDetailResponse::from(detail)))
        })
        .route(
            Method::Delete,
            "/api/v1/admin/topics/{topicId}",
            move |req| {
                let admin_id = resolve_current_admin(&delete_deps)?;
                let reason = read_deletion_reason(req)?;
                let topic_id = path_uuid(req, "topicId")?;
                command.delete_topic(topic_id, admin_id, &reason)?;
                Ok(no_content())
            },
        )
}

fn get_topics(service: &AdminTopicQueryService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let phase = q.enumeration("phase", parse_phase)?;
    let date_from = q.date("dateFrom")?;
    let date_to = q.date("dateTo")?;
    let sort = q.sort("sort", AdminTopicSort::from_query)?;
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_topics(
        phase,
        date_from,
        date_to,
        sort.unwrap_or(AdminTopicSort::TopicDateDesc),
        page,
        page_size,
    )?;
    Ok(ok_json(&TopicListResponse {
        current_page: result.current_page,
        page_size: result.page_size,
        has_next: result.has_next,
        topics: result
            .items
            .into_iter()
            .map(TopicDetailResponse::from)
            .collect(),
    }))
}

/// `AdminTopicMutationRequest`: `@NotBlank @Size(max = 255) title`, `@NotNull` 날짜·시각 셋.
fn read_mutation(req: &Request) -> Result<TopicMutation, ApiError> {
    let body = read_json_object(req)?;
    let title = body.string("title")?;
    let topic_date = body_local_date(&body, "topicDate")?;
    let starts_at = body_instant(&body, "startsAt")?;
    let ends_at = body_instant(&body, "endsAt")?;
    let (Some(topic_date), Some(starts_at), Some(ends_at)) = (topic_date, starts_at, ends_at)
    else {
        return Err(validation(MUST_NOT_BE_NULL));
    };
    let title = match title {
        Some(t) if has_text(&t) => t,
        _ => return Err(validation(MUST_NOT_BE_BLANK)),
    };
    if utf16_len(&title) > 255 {
        return Err(validation(&size_message(255)));
    }
    Ok(TopicMutation {
        title,
        topic_date: topic_date.date(),
        starts_at,
        ends_at,
    })
}

/// `AdminTopicDeletionRequest`: `@NotBlank @Size(max = 500) reason`.
fn read_deletion_reason(req: &Request) -> Result<String, ApiError> {
    let body = read_json_object(req)?;
    let reason = match body.string("reason")? {
        Some(r) if has_text(&r) => r,
        _ => return Err(validation(MUST_NOT_BE_BLANK)),
    };
    if utf16_len(&reason) > 500 {
        return Err(validation(&size_message(500)));
    }
    Ok(reason)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TopicListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    topics: Vec<TopicDetailResponse>,
}

#[derive(Serialize)]
struct PostCountsResponse {
    pending: i64,
    approved: i64,
    rejected: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TopicDetailResponse {
    topic_id: String,
    title: String,
    topic_date: String,
    #[serde(serialize_with = "serialize_instant")]
    starts_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    ends_at: Timestamp,
    phase: &'static str,
    post_counts: PostCountsResponse,
    #[serde(serialize_with = "serialize_instant")]
    created_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    updated_at: Timestamp,
}

impl From<AdminTopicDetail> for TopicDetailResponse {
    fn from(d: AdminTopicDetail) -> Self {
        TopicDetailResponse {
            topic_id: d.topic_id.hyphenated().to_string(),
            title: d.title,
            topic_date: d.topic_date.to_string(),
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            phase: d.phase.as_str(),
            post_counts: PostCountsResponse {
                pending: d.post_counts.pending,
                approved: d.post_counts.approved,
                rejected: d.post_counts.rejected,
            },
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testing::{deps, insert_admin, insert_post, insert_topic, insert_user};
    use crate::testing::TestDb;
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn mutation(title: &str, day: &str, starts: &str, ends: &str) -> TopicMutation {
        TopicMutation {
            title: title.to_owned(),
            topic_date: date(day),
            starts_at: ts(starts),
            ends_at: ts(ends),
        }
    }

    // 기준 시각은 2026-10-12T01:00:00Z. 10-14(KST)부터 시작하는 주제는 공개 전이다.
    fn future() -> TopicMutation {
        mutation(
            "미래 주제",
            "2026-10-14",
            "2026-10-13T15:00:00Z",
            "2026-10-14T15:00:00Z",
        )
    }

    fn request(method: Method, body: serde_json::Value) -> Request {
        let mut r = Request::new(method, "/");
        r.headers.append("Content-Type", "application/json");
        r.body = body.to_string().into_bytes();
        r
    }

    // AdminTopicMutationRequest
    #[test]
    fn 요청_검증() {
        let msg = |body: serde_json::Value| match read_mutation(&request(Method::Post, body)) {
            Err(ApiError::Validation(m)) => m,
            other => panic!("검증 오류가 아니다: {other:?}"),
        };
        let ok = json!({"title": "t", "topicDate": "2026-10-14",
            "startsAt": "2026-10-13T15:00:00Z", "endsAt": "2026-10-14T15:00:00Z"});
        assert!(read_mutation(&request(Method::Post, ok.clone())).is_ok());
        let mut missing = ok.clone();
        missing.as_object_mut().unwrap().remove("endsAt");
        assert_eq!(msg(missing), "must not be null");
        let mut blank = ok.clone();
        blank["title"] = json!("  ");
        assert_eq!(msg(blank), "must not be blank");
        let mut long = ok.clone();
        long["title"] = json!("가".repeat(256));
        assert_eq!(msg(long), "size must be between 0 and 255");
        // 이모지는 UTF-16으로 둘을 센다.
        let mut emoji = ok;
        emoji["title"] = json!("😀".repeat(128));
        assert_eq!(msg(emoji), "size must be between 0 and 255");
    }

    #[test]
    fn 삭제_요청_검증() {
        let msg =
            |body: serde_json::Value| match read_deletion_reason(&request(Method::Delete, body)) {
                Err(ApiError::Validation(m)) => m,
                other => panic!("검증 오류가 아니다: {other:?}"),
            };
        assert_eq!(msg(json!({})), "must not be blank");
        assert_eq!(
            msg(json!({"reason": "x".repeat(501)})),
            "size must be between 0 and 500"
        );
        assert_eq!(
            read_deletion_reason(&request(Method::Delete, json!({"reason": "사유"}))).unwrap(),
            "사유"
        );
    }

    // AdminTopicQueryServiceTest
    #[test]
    fn 조회_조건_검사() {
        assert!(validate_query(None, None, 1, 20).is_ok());
        for (from, to, page, size) in [
            (None, None, 0, 20),
            (None, None, 1, 101),
            (Some(date("2026-10-02")), Some(date("2026-10-01")), 1, 20),
            (None, None, i32::MAX, 100),
        ] {
            let e = validate_query(from, to, page, size).unwrap_err();
            assert_eq!(e.message(), INVALID_REQUEST_MESSAGE);
        }
        assert!(validate_query(Some(date("2026-10-01")), Some(date("2026-10-01")), 1, 20).is_ok());
    }

    // AdminTopicCommandServiceTest: 생성
    #[test]
    fn 주제를_만들면_감사_로그를_남긴다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = insert_admin(&mut client, "operator");
        let service = AdminTopicCommandService::new(deps(&db));
        let detail = service.create_topic(admin, &future()).unwrap();
        assert_eq!(detail.phase, TopicPhase::BeforeOpen);
        assert_eq!(
            detail.post_counts,
            TopicPostCounts {
                pending: 0,
                approved: 0,
                rejected: 0
            }
        );
        let row = client
            .query_one(
                "SELECT action::text, reason, before_state::text, after_state::text FROM admin_audit_logs WHERE target_id = $1",
                &[&detail.topic_id],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "TOPIC_CREATED");
        assert_eq!(row.get::<_, Option<String>>(1), None);
        assert_eq!(row.get::<_, String>(2), "{}");
        let after: serde_json::Value = serde_json::from_str(&row.get::<_, String>(3)).unwrap();
        assert_eq!(after["title"], "미래 주제");
        assert_eq!(after["topicDate"], "2026-10-14");
        assert_eq!(after["startsAt"], "2026-10-13T15:00:00Z");
    }

    #[test]
    fn 날짜가_겹치거나_기간이_겹치면_거절하고_감사_로그를_남기지_않는다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = insert_admin(&mut client, "operator");
        let service = AdminTopicCommandService::new(deps(&db));
        service.create_topic(admin, &future()).unwrap();
        let e = service.create_topic(admin, &future()).unwrap_err();
        assert_eq!(e.message(), "해당 날짜의 주제가 이미 존재합니다.");
        // 날짜는 다르지만(10-13 KST) 끝이 앞 주제의 시작을 넘는다.
        let overlap = mutation(
            "겹침",
            "2026-10-13",
            "2026-10-13T10:00:00Z",
            "2026-10-13T16:00:00Z",
        );
        let e = service.create_topic(admin, &overlap).unwrap_err();
        assert_eq!(e.message(), "다른 주제의 참여 기간과 겹칩니다.");
        let count: i64 = client
            .query_one("SELECT COUNT(*) FROM admin_audit_logs", &[])
            .unwrap()
            .get(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn 이미_시작한_기간이나_날짜_불일치는_도메인_오류다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = insert_admin(&mut client, "operator");
        let service = AdminTopicCommandService::new(deps(&db));
        let started = mutation(
            "지금",
            "2026-10-12",
            "2026-10-11T15:00:00Z",
            "2026-10-12T15:00:00Z",
        );
        assert_eq!(
            service.create_topic(admin, &started).unwrap_err().message(),
            "주제 정보가 올바르지 않습니다."
        );
        let inverted = mutation(
            "역전",
            "2026-10-14",
            "2026-10-14T15:00:00Z",
            "2026-10-13T15:00:00Z",
        );
        assert_eq!(
            service
                .create_topic(admin, &inverted)
                .unwrap_err()
                .message(),
            "주제 참여 기간이 올바르지 않습니다."
        );
    }

    // 수정
    #[test]
    fn 수정은_공개_전_주제만_하고_바뀐_것이_있어야_한다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = insert_admin(&mut client, "operator");
        let service = AdminTopicCommandService::new(deps(&db));
        let created = service.create_topic(admin, &future()).unwrap();
        let id = created.topic_id;

        let e = service.update_topic(id, admin, &future()).unwrap_err();
        assert_eq!(e.message(), "변경된 주제 정보가 없습니다.");
        assert_eq!(e.code().as_str(), "RESOURCE_STATE_CHANGED");

        let mut changed = future();
        changed.title = "바뀐 제목".to_owned();
        let detail = service.update_topic(id, admin, &changed).unwrap();
        assert_eq!(detail.title, "바뀐 제목");
        let action: String = client
            .query_one(
                "SELECT action::text FROM admin_audit_logs WHERE target_id = $1 ORDER BY occurred_at DESC, id DESC LIMIT 1",
                &[&id],
            )
            .unwrap()
            .get(0);
        assert_eq!(action, "TOPIC_UPDATED");

        let e = service
            .update_topic(Uuid::new_v4(), admin, &changed)
            .unwrap_err();
        assert_eq!(e.message(), NOT_FOUND_MESSAGE);
        assert_eq!(e.status(), 404);

        let open = insert_topic(
            &mut client,
            "열림",
            "2026-10-12",
            "2026-10-11T15:00:00Z",
            "2026-10-12T15:00:00Z",
        );
        let e = service.update_topic(open, admin, &changed).unwrap_err();
        assert_eq!(e.message(), "공개 전 주제만 변경할 수 있습니다.");
    }

    // 삭제
    #[test]
    fn 삭제는_사유를_감사_로그에_남기고_주제를_숨긴다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let admin = insert_admin(&mut client, "operator");
        let service = AdminTopicCommandService::new(deps(&db));
        let created = service.create_topic(admin, &future()).unwrap();
        let id = created.topic_id;
        service.delete_topic(id, admin, " 잘못 등록 ").unwrap();
        let row = client
            .query_one(
                "SELECT reason, after_state::text, (SELECT deleted_at IS NOT NULL FROM topics WHERE id = $1) \
                 FROM admin_audit_logs WHERE target_id = $1 AND action = 'TOPIC_DELETED'",
                &[&id],
            )
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "잘못 등록");
        assert!(row.get::<_, String>(1).contains("deletedAt"));
        assert!(row.get::<_, bool>(2));
        let e = service.delete_topic(id, admin, "again").unwrap_err();
        assert_eq!(e.status(), 404);
        let e = service.delete_topic(id, admin, "  ").unwrap_err();
        assert_eq!(e.message(), INVALID_REQUEST_MESSAGE);
        let query = AdminTopicQueryService::new(deps(&db));
        assert_eq!(query.get_topic(id).unwrap_err().status(), 404);
    }

    // AdminTopicQueryRepositoryTest
    #[test]
    fn 목록은_단계와_날짜로_거르고_삭제되지_않은_게시물만_센다() {
        let Some(db) = TestDb::create() else { return };
        let mut client = db.client();
        let user = insert_user(&mut client, "a@test.chalkak", "ACTIVE");
        let closed = insert_topic(
            &mut client,
            "종료",
            "2026-10-11",
            "2026-10-10T15:00:00Z",
            "2026-10-11T15:00:00Z",
        );
        let open = insert_topic(
            &mut client,
            "열림",
            "2026-10-12",
            "2026-10-11T15:00:00Z",
            "2026-10-12T15:00:00Z",
        );
        let before = insert_topic(
            &mut client,
            "공개전",
            "2026-10-13",
            "2026-10-12T15:00:00Z",
            "2026-10-13T15:00:00Z",
        );
        let deleted = insert_topic(
            &mut client,
            "삭제",
            "2026-10-09",
            "2026-10-08T15:00:00Z",
            "2026-10-09T15:00:00Z",
        );
        client
            .execute(
                "UPDATE topics SET deleted_at = now() WHERE id = $1",
                &[&deleted],
            )
            .unwrap();
        let p = insert_post(&mut client, user, open, "PENDING");
        let other = insert_user(&mut client, "b@test.chalkak", "ACTIVE");
        insert_post(&mut client, other, open, "APPROVED");
        let third = insert_user(&mut client, "c@test.chalkak", "ACTIVE");
        let gone = insert_post(&mut client, third, open, "REJECTED");
        client
            .execute(
                "UPDATE posts SET deleted_at = now() WHERE id = $1",
                &[&gone],
            )
            .unwrap();
        let _ = p;

        let service = AdminTopicQueryService::new(deps(&db));
        let list = |phase, from: Option<&str>, to: Option<&str>, sort| {
            service
                .get_topics(phase, from.map(date), to.map(date), sort, 1, 20)
                .unwrap()
                .items
        };
        let all = list(None, None, None, AdminTopicSort::TopicDateDesc);
        assert_eq!(
            all.iter().map(|t| t.topic_id).collect::<Vec<_>>(),
            vec![before, open, closed]
        );
        let counts = all[1].post_counts;
        assert_eq!(
            counts,
            TopicPostCounts {
                pending: 1,
                approved: 1,
                rejected: 0
            }
        );
        assert_eq!(
            list(
                Some(TopicPhase::Open),
                None,
                None,
                AdminTopicSort::TopicDateAsc
            )[0]
            .topic_id,
            open
        );
        assert_eq!(
            list(
                Some(TopicPhase::Closed),
                None,
                None,
                AdminTopicSort::TopicDateAsc
            )[0]
            .topic_id,
            closed
        );
        assert_eq!(
            list(
                Some(TopicPhase::BeforeOpen),
                None,
                None,
                AdminTopicSort::TopicDateAsc
            )[0]
            .topic_id,
            before
        );
        let ranged = list(
            None,
            Some("2026-10-12"),
            Some("2026-10-12"),
            AdminTopicSort::CreatedAtAsc,
        );
        assert_eq!(ranged.len(), 1);
        assert_eq!(service.get_topic(open).unwrap().phase, TopicPhase::Open);
        assert_eq!(service.get_topic(deleted).unwrap_err().status(), 404);
    }
}
