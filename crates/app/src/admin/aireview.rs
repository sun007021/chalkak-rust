//! 관리자 AI 프롬프트·리뷰 관리(원본 `AdminAiPromptVersionController`, `AdminAiPromptPreviewController`,
//! `AdminAiReviewController`와 서비스, 조회 저장소, DTO).
//!
//! - `/api/v1/admin/ai-prompt-versions`: 프롬프트 버전 만들기·목록·상세. 만든 버전은 바꾸거나 지울 수 없다.
//! - `/api/v1/admin/ai-prompt-previews`: 저장하지 않은 초안을 공개 게시물에 실행해 본다. OpenAI 실패도 200으로 돌려준다.
//! - `/api/v1/admin/ai-reviews`: 사용자 앱에서 숨겨진 것까지 모든 AI 리뷰를 상태·프롬프트 버전으로 걸러 본다.

use std::sync::Arc;

use chalkak_db::{Db, DbError};
use chalkak_http::Request;
use jiff::Timestamp;
use postgres::Row;
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::admin::audit::domain::{AdminAction, AdminTargetType, AuditSnapshot, text_value};
use crate::admin::audit::service::{AuditLogCommand, create_audit_log};
use crate::admin::binding::{
    MUST_NOT_BE_BLANK, MUST_NOT_BE_NULL, QueryParams, body_enum, has_text, page_params,
    size_message, utf16_len, validation,
};
use crate::admin::support::{
    AdminDeps, AdminPage, INVALID_QUERY_MESSAGE, business, in_transaction, limit_offset, not_found,
    parse_path_uuid, path_uuid, resolve_current_admin, validate_pagination,
};
use crate::aireview::domain::{
    AiPromptDraft, AiPromptParameters, AiPromptType, AiPromptVersion, AiReviewResult, ImageDetail,
    ReasoningEffort,
};
use crate::aireview::generator::{
    AiReviewGeneration, AiReviewGenerationError, AiReviewGenerationRequest, AiReviewGenerator,
    Usage,
};
use crate::aireview::post_query::find_visible_post;
use crate::aireview::prompt_repository::PromptVersionRepository;
use crate::photo::url::ImageUrlProvider;
use crate::web::binding::{JsonObject, read_json_object};
use crate::web::json::{serialize_instant, to_json};
use crate::web::{ApiError, ApiResult, Routes, created_json, ok_json};

const MODEL_MESSAGE: &str = "허용되지 않은 모델입니다.";
const VERSION_NOT_FOUND: &str = "프롬프트 버전을 찾을 수 없습니다.";
const REVIEW_NOT_FOUND: &str = "AI 리뷰를 찾을 수 없습니다.";
const POST_NOT_FOUND: &str = "게시물을 찾을 수 없습니다.";
const MAX_SYSTEM_PROMPT: usize = 50_000;
const MAX_USER_TEMPLATE: usize = 2_000;
const MAX_MODEL: usize = 100;
/// `LogFields.TYPE_AI_REVIEW`
pub const TYPE_AI_REVIEW: &str = "ai_review";
const AI_REVIEW_LOGGER: &str = "chalkak.aireview";

// ---------------------------------------------------------------------------------------------
// 요청 읽기
// ---------------------------------------------------------------------------------------------

/// 프롬프트 만들기·미리보기가 함께 쓰는 필드(`AiPromptVersionCreateRequest`, `AiPromptPreviewRequest`).
pub struct PromptRequest {
    prompt_type: AiPromptType,
    system_prompt: String,
    user_prompt_template: String,
    model: String,
    parameters: ParametersRequest,
}

/// `AiPromptParametersRequest`: 모든 항목이 선택이다.
#[derive(Default)]
pub struct ParametersRequest {
    image_detail: Option<ImageDetail>,
    max_output_tokens: Option<i32>,
    reasoning_effort: Option<ReasoningEffort>,
    distinctive_perspective_threshold: Option<f64>,
}

fn unreadable() -> ApiError {
    ApiError::UnreadableBody
}

/// Jackson 열거형 값(`body_enum`과 같은 규칙, 중첩 객체용).
fn value_enum<T: Copy>(value: Option<&Value>, names: &[(&str, T)]) -> Result<Option<T>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = crate::admin::binding::java_trim(text);
            if let Some((_, found)) = names.iter().find(|(name, _)| *name == trimmed) {
                return Ok(Some(*found));
            }
            if !trimmed.is_empty()
                && trimmed.bytes().all(|d| d.is_ascii_digit())
                && let Some((_, found)) = trimmed.parse::<usize>().ok().and_then(|o| names.get(o))
            {
                return Ok(Some(*found));
            }
            Err(unreadable())
        }
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|o| usize::try_from(o).ok())
            .and_then(|o| names.get(o))
            .map(|(_, found)| Some(*found))
            .ok_or_else(unreadable),
        Some(_) => Err(unreadable()),
    }
}

/// Jackson `Integer`: 정수 토큰, 소수(버림), 숫자 문자열(공백 제거). 빈 문자열은 `null`.
fn jackson_integer(value: Option<&Value>) -> Result<Option<i32>, ApiError> {
    let n: i64 = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i
            } else if n.as_u64().is_some() {
                return Err(unreadable());
            } else {
                let truncated = n.as_f64().ok_or_else(unreadable)?.trunc();
                if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&truncated) {
                    return Err(unreadable());
                }
                truncated as i64
            }
        }
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Ok(None);
            }
            let text = s.trim();
            let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(unreadable());
            }
            text.parse::<i64>().map_err(|_| unreadable())?
        }
        Some(_) => return Err(unreadable()),
    };
    i32::try_from(n).map(Some).map_err(|_| unreadable())
}

/// Jackson `Double`: 숫자 토큰이나 숫자 문자열.
fn jackson_double(value: Option<&Value>) -> Result<Option<f64>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.as_f64().ok_or_else(unreadable)?)),
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Ok(None);
            }
            match s.trim() {
                "NaN" => Ok(Some(f64::NAN)),
                "Infinity" | "+Infinity" => Ok(Some(f64::INFINITY)),
                "-Infinity" => Ok(Some(f64::NEG_INFINITY)),
                text => text
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(Some)
                    .ok_or_else(unreadable),
            }
        }
        Some(_) => Err(unreadable()),
    }
}

fn image_detail_names() -> Vec<(&'static str, ImageDetail)> {
    ImageDetail::ALL.to_vec()
}

fn reasoning_effort_names() -> Vec<(&'static str, ReasoningEffort)> {
    ReasoningEffort::ALL.to_vec()
}

fn prompt_type_names() -> Vec<(&'static str, AiPromptType)> {
    AiPromptType::ALL.iter().map(|t| (t.as_str(), *t)).collect()
}

/// `parameters`가 없거나 `null`이면 모든 항목을 생략한 것과 같다(`AiPromptParametersRequest.orDefaults`).
fn read_parameters(body: &JsonObject) -> Result<ParametersRequest, ApiError> {
    let map: &Map<String, Value> = match body.value("parameters") {
        None | Some(Value::Null) => return Ok(ParametersRequest::default()),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(unreadable()),
    };
    Ok(ParametersRequest {
        image_detail: value_enum(map.get("imageDetail"), &image_detail_names())?,
        max_output_tokens: jackson_integer(map.get("maxOutputTokens"))?,
        reasoning_effort: value_enum(map.get("reasoningEffort"), &reasoning_effort_names())?,
        distinctive_perspective_threshold: jackson_double(
            map.get("distinctivePerspectiveThreshold"),
        )?,
    })
}

/// `@NotNull type`, `@NotBlank @Size system/template/model`. 본문을 모두 읽은 뒤 제약을 필드 순서로 검사한다.
fn read_prompt_request(body: &JsonObject) -> Result<PromptRequest, ApiError> {
    let prompt_type = body_enum(body, "type", &prompt_type_names())?;
    let system_prompt = body.string("systemPrompt")?;
    let user_prompt_template = body.string("userPromptTemplate")?;
    let model = body.string("model")?;
    let parameters = read_parameters(body)?;
    let Some(prompt_type) = prompt_type else {
        return Err(validation(MUST_NOT_BE_NULL));
    };
    let system_prompt = required_text(system_prompt, MAX_SYSTEM_PROMPT)?;
    let user_prompt_template = required_text(user_prompt_template, MAX_USER_TEMPLATE)?;
    let model = required_text(model, MAX_MODEL)?;
    Ok(PromptRequest {
        prompt_type,
        system_prompt,
        user_prompt_template,
        model,
        parameters,
    })
}

/// `@NotBlank @Size(max)`
fn required_text(value: Option<String>, max: usize) -> Result<String, ApiError> {
    match value {
        Some(text) if has_text(&text) => {
            if utf16_len(&text) > max {
                return Err(validation(&size_message(max)));
            }
            Ok(text)
        }
        _ => Err(validation(MUST_NOT_BE_BLANK)),
    }
}

fn build_draft(request: &PromptRequest) -> Result<AiPromptDraft, ApiError> {
    let p = &request.parameters;
    let parameters = AiPromptParameters::new(
        p.image_detail,
        p.max_output_tokens,
        p.reasoning_effort,
        p.distinctive_perspective_threshold,
    )?;
    Ok(AiPromptDraft::new(
        request.prompt_type,
        &request.system_prompt,
        &request.user_prompt_template,
        &request.model,
        parameters,
    )?)
}

// ---------------------------------------------------------------------------------------------
// 프롬프트 버전
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct AdminAiPromptVersionService {
    deps: AdminDeps,
    generator: Arc<dyn AiReviewGenerator>,
}

impl AdminAiPromptVersionService {
    pub fn new(deps: AdminDeps, generator: Arc<dyn AiReviewGenerator>) -> Self {
        AdminAiPromptVersionService { deps, generator }
    }

    /// `createVersion`: 같은 용도의 번호를 잠근 뒤 가장 큰 번호 다음으로 만든다.
    pub fn create_version(
        &self,
        admin_id: Uuid,
        request: &PromptCommand,
    ) -> Result<AiPromptVersion, ApiError> {
        let draft = build_draft(&request.prompt)?;
        self.validate_model(&draft.model)?;
        let now = self.deps.now();
        let repo = PromptVersionRepository;
        let mut conn = self.deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            repo.lock_type(tx, draft.prompt_type)?;
            let version = repo.find_max_version(tx, draft.prompt_type)? + 1;
            let mut saved =
                AiPromptVersion::create(&draft, version, request.memo.as_deref(), admin_id)?;
            repo.save(tx, &mut saved, now)?;
            let id = saved
                .id
                .ok_or_else(|| business("프롬프트 버전 식별자가 없습니다."))?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin_id,
                    action: AdminAction::AiPromptVersionCreated,
                    target_type: AdminTargetType::AiPromptVersion,
                    target_id: id,
                    reason: None,
                    before_state: AuditSnapshot::empty(),
                    after_state: AuditSnapshot::of(vec![
                        ("type", text_value(saved.prompt_type.as_str())),
                        ("version", Value::from(version)),
                    ])?,
                },
                now,
            )?;
            Ok(saved)
        })
    }

    pub fn get_versions(
        &self,
        prompt_type: AiPromptType,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<AiPromptVersion>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let mut conn = self.deps.pool.get()?;
        let found =
            PromptVersionRepository.find_by_type(&mut conn, prompt_type, page, page_size)?;
        Ok(AdminPage {
            items: found.items,
            current_page: page,
            page_size,
            has_next: found.has_next,
        })
    }

    pub fn get_version(&self, id: Uuid) -> Result<AiPromptVersion, ApiError> {
        let mut conn = self.deps.pool.get()?;
        PromptVersionRepository
            .find_by_id(&mut conn, id)?
            .ok_or_else(|| not_found(VERSION_NOT_FOUND))
    }

    fn validate_model(&self, model: &str) -> Result<(), ApiError> {
        if self.generator.supports_model(model) {
            Ok(())
        } else {
            Err(business(MODEL_MESSAGE))
        }
    }
}

/// `CreateCommand`: 프롬프트 값과 메모.
pub struct PromptCommand {
    prompt: PromptRequest,
    memo: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// 미리보기
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct AdminAiPromptPreviewService {
    deps: AdminDeps,
    generator: Arc<dyn AiReviewGenerator>,
}

/// `PreviewResult`: 실패해도 예외가 아니라 이 값으로 돌려준다.
#[derive(Debug)]
pub struct PreviewResult {
    pub succeeded: bool,
    pub failure_type: Option<&'static str>,
    pub retryable: Option<bool>,
    pub result: Option<AiReviewResult>,
    pub output: Option<(f64, f64)>,
    pub usage: Option<Usage>,
    pub latency_ms: Option<i64>,
    pub response_id: Option<String>,
}

impl PreviewResult {
    fn from_generation(generation: AiReviewGeneration) -> PreviewResult {
        let attempt = generation.attempt;
        PreviewResult {
            succeeded: true,
            failure_type: None,
            retryable: None,
            result: Some(generation.result),
            output: Some((
                generation.output.composition_score,
                generation.output.theme_expression_score,
            )),
            usage: Some(attempt.usage),
            latency_ms: Some(millis(attempt.latency)),
            response_id: attempt.response_id,
        }
    }

    fn from_failure(error: AiReviewGenerationError) -> PreviewResult {
        let attempt = error.attempt;
        PreviewResult {
            succeeded: false,
            failure_type: Some(error.failure_type.as_str()),
            retryable: Some(error.failure_type.is_retryable()),
            result: None,
            output: None,
            usage: attempt.as_ref().map(|a| a.usage),
            latency_ms: attempt.as_ref().map(|a| millis(a.latency)),
            response_id: attempt.and_then(|a| a.response_id),
        }
    }
}

fn millis(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl AdminAiPromptPreviewService {
    pub fn new(deps: AdminDeps, generator: Arc<dyn AiReviewGenerator>) -> Self {
        AdminAiPromptPreviewService { deps, generator }
    }

    /// `preview`: 트랜잭션을 열지 않는다. 게시물을 읽고 연결을 돌려준 뒤 OpenAI를 호출한다.
    pub fn preview(
        &self,
        admin_id: Uuid,
        request: &PromptRequest,
        post_id: Uuid,
    ) -> Result<PreviewResult, ApiError> {
        let draft = build_draft(request)?;
        if !self.generator.supports_model(&draft.model) {
            return Err(business(MODEL_MESSAGE));
        }
        let post = {
            let mut conn = self.deps.pool.get()?;
            find_visible_post(&mut conn, post_id)?
        }
        .ok_or_else(|| not_found(POST_NOT_FOUND))?;
        let generation_request = AiReviewGenerationRequest::for_post(
            &self.deps.image_urls as &dyn ImageUrlProvider,
            draft.clone(),
            &post.original_storage_key,
            &post.topic_title,
        )
        .map_err(ApiError::Internal)?;
        let result = match self.generator.generate(&generation_request) {
            Ok(generation) => PreviewResult::from_generation(generation),
            Err(error) => PreviewResult::from_failure(error),
        };
        self.log_preview(admin_id, &draft.model, &result);
        Ok(result)
    }

    /// 프롬프트·이미지 URL·주제·피드백은 남기지 않고 값이 있는 항목만 한 줄로 기록한다.
    fn log_preview(&self, admin_id: Uuid, model: &str, result: &PreviewResult) {
        use chalkak_core::log::fields;
        let mut event = self
            .deps
            .logger
            .info(AI_REVIEW_LOGGER)
            .field(fields::TYPE, TYPE_AI_REVIEW)
            .field(fields::EVENT, "preview")
            .field(fields::ADMIN_ID, admin_id.hyphenated().to_string())
            .field("model", model)
            .field("succeeded", result.succeeded);
        if let Some(failure) = result.failure_type {
            event = event.field("failureType", failure);
        }
        if let Some(latency) = result.latency_ms {
            event = event.field("latencyMs", latency);
        }
        if let Some(usage) = result.usage {
            for (key, value) in [
                ("inputTokens", usage.input_tokens),
                ("cachedInputTokens", usage.cached_input_tokens),
                ("outputTokens", usage.output_tokens),
                ("reasoningTokens", usage.reasoning_tokens),
            ] {
                if let Some(value) = value {
                    event = event.field(key, i64::from(value));
                }
            }
        }
        event.log("AI 프롬프트 미리보기");
    }
}

// ---------------------------------------------------------------------------------------------
// 리뷰 조회
// ---------------------------------------------------------------------------------------------

const SUMMARY_COLUMNS: &str = "r.id AS review_id, r.status AS review_status, r.error_code, \
     r.request_count, r.result::text AS result, \
     pv.id AS prompt_version_id, pv.version AS prompt_version, pv.model AS prompt_model, \
     p.id AS post_id, ph.thumbnail_storage_key, p.moderation_status::text AS moderation_status, \
     p.deleted_at AS post_deleted_at, \
     u.id AS author_id, u.email AS author_email, u.status::text AS author_status, \
     u.deleted_at AS author_deleted_at, \
     r.input_tokens, r.cached_input_tokens, r.output_tokens, r.reasoning_tokens, r.latency_ms, \
     r.requested_at, r.last_requested_at, r.completed_at, \
     (p.moderation_status = 'APPROVED' AND p.deleted_at IS NULL AND t.deleted_at IS NULL \
      AND ph.deleted_at IS NULL AND u.deleted_at IS NULL) AS visible_to_author";

/// 게시물·작성자·프롬프트 버전·사진은 FK가 RESTRICT라 항상 있으므로 내부 조인한다.
const FROM_JOINS: &str = " FROM ai_reviews r \
     JOIN posts p ON p.id = r.post_id \
     JOIN topics t ON t.id = p.topic_id \
     JOIN photos ph ON ph.id = p.photo_id \
     JOIN users u ON u.id = p.user_id \
     JOIN ai_prompt_versions pv ON pv.id = r.prompt_version_id";

const DETAIL_COLUMNS: &str = ", r.output::text AS output, r.openai_response_id, r.attempt, \
     r.started_at, ph.original_storage_key";

/// `SummaryProjection`
pub struct ReviewSummary {
    review_id: Uuid,
    status: String,
    error_code: Option<String>,
    request_count: i32,
    result: Option<AiReviewResult>,
    prompt_version_id: Uuid,
    prompt_version: i32,
    prompt_model: String,
    post_id: Uuid,
    thumbnail_storage_key: Option<String>,
    moderation_status: String,
    post_deleted_at: Option<Timestamp>,
    author_id: Uuid,
    author_email: Option<String>,
    author_status: String,
    author_deleted_at: Option<Timestamp>,
    usage: Usage,
    latency_ms: Option<i64>,
    requested_at: Timestamp,
    last_requested_at: Timestamp,
    completed_at: Option<Timestamp>,
    visible_to_author: bool,
}

/// `DetailProjection`
pub struct ReviewDetail {
    summary: ReviewSummary,
    output: Option<String>,
    openai_response_id: Option<String>,
    attempt: i32,
    started_at: Option<Timestamp>,
    original_storage_key: String,
}

fn summary_from_row(row: &Row) -> Result<ReviewSummary, DbError> {
    let result_text: Option<String> = row.try_get("result")?;
    let result = match result_text {
        None => None,
        Some(text) => Some(serde_json::from_str::<AiReviewResult>(&text).map_err(|e| {
            DbError::Config(format!("저장된 AI 리뷰 결과를 읽을 수 없습니다: {e}"))
        })?),
    };
    Ok(ReviewSummary {
        review_id: row.try_get("review_id")?,
        status: row.try_get("review_status")?,
        error_code: row.try_get("error_code")?,
        request_count: row.try_get("request_count")?,
        result,
        prompt_version_id: row.try_get("prompt_version_id")?,
        prompt_version: row.try_get("prompt_version")?,
        prompt_model: row.try_get("prompt_model")?,
        post_id: row.try_get("post_id")?,
        thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
        moderation_status: row.try_get("moderation_status")?,
        post_deleted_at: row.try_get("post_deleted_at")?,
        author_id: row.try_get("author_id")?,
        author_email: row.try_get("author_email")?,
        author_status: row.try_get("author_status")?,
        author_deleted_at: row.try_get("author_deleted_at")?,
        usage: Usage {
            input_tokens: row.try_get("input_tokens")?,
            cached_input_tokens: row.try_get("cached_input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            reasoning_tokens: row.try_get("reasoning_tokens")?,
        },
        latency_ms: row.try_get("latency_ms")?,
        requested_at: row.try_get("requested_at")?,
        last_requested_at: row.try_get("last_requested_at")?,
        completed_at: row.try_get("completed_at")?,
        visible_to_author: row.try_get("visible_to_author")?,
    })
}

/// 목록은 마지막 요청 시각 내림차순, 같으면 ID 내림차순이다.
pub fn find_ai_reviews(
    db: &mut impl Db,
    status: Option<&str>,
    prompt_version_id: Option<Uuid>,
    page: i32,
    page_size: i32,
) -> Result<AdminPage<ReviewSummary>, DbError> {
    let (limit, offset) = limit_offset(page, page_size);
    let sql = format!(
        "SELECT {SUMMARY_COLUMNS}{FROM_JOINS} \
         WHERE ($1::text IS NULL OR r.status = $1::text) \
           AND ($2::uuid IS NULL OR pv.id = $2::uuid) \
         ORDER BY r.last_requested_at DESC, r.id DESC LIMIT $3 OFFSET $4"
    );
    let rows = db.query(&sql, &[&status, &prompt_version_id, &limit, &offset])?;
    let items = rows
        .iter()
        .map(summary_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AdminPage::from_overfetched(items, page, page_size))
}

pub fn find_ai_review(db: &mut impl Db, id: Uuid) -> Result<Option<ReviewDetail>, DbError> {
    let sql = format!("SELECT {SUMMARY_COLUMNS}{DETAIL_COLUMNS}{FROM_JOINS} WHERE r.id = $1");
    let Some(row) = db.query_opt(&sql, &[&id])? else {
        return Ok(None);
    };
    Ok(Some(ReviewDetail {
        summary: summary_from_row(&row)?,
        output: row.try_get("output")?,
        openai_response_id: row.try_get("openai_response_id")?,
        attempt: row.try_get("attempt")?,
        started_at: row.try_get("started_at")?,
        original_storage_key: row.try_get("original_storage_key")?,
    }))
}

#[derive(Clone)]
pub struct AdminAiReviewQueryService {
    deps: AdminDeps,
}

impl AdminAiReviewQueryService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminAiReviewQueryService { deps }
    }

    pub fn get_ai_reviews(
        &self,
        status: Option<&str>,
        prompt_version_id: Option<Uuid>,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<ReviewSummary>, ApiError> {
        validate_pagination(page, page_size, INVALID_QUERY_MESSAGE)?;
        let mut conn = self.deps.pool.get()?;
        Ok(find_ai_reviews(
            &mut conn,
            status,
            prompt_version_id,
            page,
            page_size,
        )?)
    }

    pub fn get_ai_review(&self, id: Uuid) -> Result<ReviewDetail, ApiError> {
        let mut conn = self.deps.pool.get()?;
        find_ai_review(&mut conn, id)?.ok_or_else(|| not_found(REVIEW_NOT_FOUND))
    }

    fn thumbnail_url(&self, key: Option<&str>) -> Result<Option<String>, ApiError> {
        Ok(self.deps.image_urls.get_url(key)?)
    }
}

// ---------------------------------------------------------------------------------------------
// 응답
// ---------------------------------------------------------------------------------------------

fn id_text(id: Uuid) -> String {
    id.hyphenated().to_string()
}

fn serialize_optional_instant<S: serde::Serializer>(
    value: &Option<Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    crate::admin::user::api::serialize_optional_instant(value, serializer)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ParametersResponse {
    image_detail: &'static str,
    max_output_tokens: i32,
    reasoning_effort: Option<&'static str>,
    distinctive_perspective_threshold: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionResponse {
    prompt_version_id: String,
    r#type: &'static str,
    version: i32,
    system_prompt: String,
    user_prompt_template: String,
    model: String,
    parameters: ParametersResponse,
    output_schema_version: String,
    memo: Option<String>,
    created_by_admin_id: String,
    #[serde(serialize_with = "serialize_optional_instant")]
    created_at: Option<Timestamp>,
}

impl From<AiPromptVersion> for VersionResponse {
    fn from(v: AiPromptVersion) -> Self {
        VersionResponse {
            prompt_version_id: id_text(v.id.unwrap_or_default()),
            r#type: v.prompt_type.as_str(),
            version: v.version,
            system_prompt: v.system_prompt,
            user_prompt_template: v.user_prompt_template,
            model: v.model,
            parameters: ParametersResponse {
                image_detail: v.parameters.image_detail.as_str(),
                max_output_tokens: v.parameters.max_output_tokens,
                reasoning_effort: v.parameters.reasoning_effort.map(ReasoningEffort::as_str),
                distinctive_perspective_threshold: v.parameters.distinctive_perspective_threshold,
            },
            output_schema_version: v.output_schema_version,
            memo: v.memo,
            created_by_admin_id: id_text(v.created_by_admin_id),
            created_at: v.created_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionItem {
    prompt_version_id: String,
    r#type: &'static str,
    version: i32,
    model: String,
    output_schema_version: String,
    memo: Option<String>,
    created_by_admin_id: String,
    #[serde(serialize_with = "serialize_optional_instant")]
    created_at: Option<Timestamp>,
}

impl From<AiPromptVersion> for VersionItem {
    fn from(v: AiPromptVersion) -> Self {
        VersionItem {
            prompt_version_id: id_text(v.id.unwrap_or_default()),
            r#type: v.prompt_type.as_str(),
            version: v.version,
            model: v.model,
            output_schema_version: v.output_schema_version,
            memo: v.memo,
            created_by_admin_id: id_text(v.created_by_admin_id),
            created_at: v.created_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    prompt_versions: Vec<VersionItem>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewOutput {
    composition_score: f64,
    theme_expression_score: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageResponse {
    input_tokens: Option<i32>,
    cached_input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    reasoning_tokens: Option<i32>,
}

impl From<Usage> for UsageResponse {
    fn from(u: Usage) -> Self {
        UsageResponse {
            input_tokens: u.input_tokens,
            cached_input_tokens: u.cached_input_tokens,
            output_tokens: u.output_tokens,
            reasoning_tokens: u.reasoning_tokens,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewResponse {
    succeeded: bool,
    failure_type: Option<&'static str>,
    retryable: Option<bool>,
    result: Option<AiReviewResult>,
    output: Option<PreviewOutput>,
    usage: Option<UsageResponse>,
    latency_ms: Option<i64>,
    response_id: Option<String>,
}

impl From<PreviewResult> for PreviewResponse {
    fn from(p: PreviewResult) -> Self {
        PreviewResponse {
            succeeded: p.succeeded,
            failure_type: p.failure_type,
            retryable: p.retryable,
            result: p.result,
            output: p.output.map(|(composition, theme)| PreviewOutput {
                composition_score: composition,
                theme_expression_score: theme,
            }),
            usage: p.usage.map(UsageResponse::from),
            latency_ms: p.latency_ms,
            response_id: p.response_id,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptVersionSummaryResponse {
    prompt_version_id: String,
    version: i32,
    model: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorResponse {
    user_id: String,
    email: Option<String>,
    status: String,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListPostResponse {
    post_id: String,
    thumbnail_url: Option<String>,
    moderation_status: String,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListItem {
    ai_review_id: String,
    status: String,
    error_code: Option<String>,
    request_count: i32,
    overall_score: Option<f64>,
    prompt_version: PromptVersionSummaryResponse,
    post: ListPostResponse,
    author: AuthorResponse,
    usage: UsageResponse,
    latency_ms: Option<i64>,
    #[serde(serialize_with = "serialize_instant")]
    requested_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    last_requested_at: Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    completed_at: Option<Timestamp>,
    visible_to_author: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    current_page: i32,
    page_size: i32,
    has_next: bool,
    ai_reviews: Vec<ListItem>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetailPostResponse {
    post_id: String,
    thumbnail_url: Option<String>,
    image_url: Option<String>,
    moderation_status: String,
    #[serde(serialize_with = "serialize_optional_instant")]
    deleted_at: Option<Timestamp>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetailResponse {
    ai_review_id: String,
    status: String,
    error_code: Option<String>,
    request_count: i32,
    overall_score: Option<f64>,
    prompt_version: PromptVersionSummaryResponse,
    post: DetailPostResponse,
    author: AuthorResponse,
    usage: UsageResponse,
    latency_ms: Option<i64>,
    #[serde(serialize_with = "serialize_instant")]
    requested_at: Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    last_requested_at: Timestamp,
    #[serde(serialize_with = "serialize_optional_instant")]
    started_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_optional_instant")]
    completed_at: Option<Timestamp>,
    visible_to_author: bool,
    attempt: i32,
    openai_response_id: Option<String>,
    result: Option<AiReviewResult>,
    output: Option<Box<serde_json::value::RawValue>>,
}

fn prompt_version_part(s: &ReviewSummary) -> PromptVersionSummaryResponse {
    PromptVersionSummaryResponse {
        prompt_version_id: id_text(s.prompt_version_id),
        version: s.prompt_version,
        model: s.prompt_model.clone(),
    }
}

fn author_part(s: &ReviewSummary) -> AuthorResponse {
    AuthorResponse {
        user_id: id_text(s.author_id),
        email: s.author_email.clone(),
        status: s.author_status.clone(),
        deleted_at: s.author_deleted_at,
    }
}

impl AdminAiReviewQueryService {
    fn list_item(&self, s: ReviewSummary) -> Result<ListItem, ApiError> {
        let thumbnail_url = self.thumbnail_url(s.thumbnail_storage_key.as_deref())?;
        Ok(ListItem {
            ai_review_id: id_text(s.review_id),
            status: s.status.clone(),
            error_code: s.error_code.clone(),
            request_count: s.request_count,
            overall_score: s.result.as_ref().map(|r| r.overall_score),
            prompt_version: prompt_version_part(&s),
            post: ListPostResponse {
                post_id: id_text(s.post_id),
                thumbnail_url,
                moderation_status: s.moderation_status.clone(),
                deleted_at: s.post_deleted_at,
            },
            author: author_part(&s),
            usage: s.usage.into(),
            latency_ms: s.latency_ms,
            requested_at: s.requested_at,
            last_requested_at: s.last_requested_at,
            completed_at: s.completed_at,
            visible_to_author: s.visible_to_author,
        })
    }

    fn detail_response(&self, d: ReviewDetail) -> Result<DetailResponse, ApiError> {
        let s = d.summary;
        let thumbnail_url = self.thumbnail_url(s.thumbnail_storage_key.as_deref())?;
        let image_url = self.thumbnail_url(Some(&d.original_storage_key))?;
        let output = match d.output {
            None => None,
            Some(text) => Some(
                serde_json::value::RawValue::from_string(minify_json(&text)).map_err(|e| {
                    ApiError::Internal(format!("저장된 AI 리뷰 출력을 읽을 수 없습니다: {e}"))
                })?,
            ),
        };
        Ok(DetailResponse {
            ai_review_id: id_text(s.review_id),
            status: s.status.clone(),
            error_code: s.error_code.clone(),
            request_count: s.request_count,
            overall_score: s.result.as_ref().map(|r| r.overall_score),
            prompt_version: prompt_version_part(&s),
            post: DetailPostResponse {
                post_id: id_text(s.post_id),
                thumbnail_url,
                image_url,
                moderation_status: s.moderation_status.clone(),
                deleted_at: s.post_deleted_at,
            },
            author: author_part(&s),
            usage: s.usage.into(),
            latency_ms: s.latency_ms,
            requested_at: s.requested_at,
            last_requested_at: s.last_requested_at,
            started_at: d.started_at,
            completed_at: s.completed_at,
            visible_to_author: s.visible_to_author,
            attempt: d.attempt,
            openai_response_id: d.openai_response_id,
            result: s.result,
            output,
        })
    }
}

/// PostgreSQL `jsonb::text`(키는 jsonb 정렬 순서, 구분자에 공백)를 Jackson이 `JsonNode`를 쓰는 모양(공백 없음, 문자열은
/// Jackson 이스케이프)으로 바꾼다. 키 순서와 숫자 표기는 그대로 둔다.
pub fn minify_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                let end = (i + 1).min(bytes.len());
                match serde_json::from_str::<String>(&text[start..end]) {
                    Ok(decoded) => out.push_str(&to_json(&decoded)),
                    Err(_) => out.push_str(&text[start..end]),
                }
                i = end;
            }
            b' ' | b'\n' | b'\t' | b'\r' => i += 1,
            _ => {
                // 숫자·리터럴·구분자는 한 글자씩 옮긴다(멀티바이트는 문자열 밖에 없다).
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// 컨트롤러
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, deps: &AdminDeps, generator: Arc<dyn AiReviewGenerator>) -> Routes {
    let versions = AdminAiPromptVersionService::new(deps.clone(), Arc::clone(&generator));
    let previews = AdminAiPromptPreviewService::new(deps.clone(), generator);
    let reviews = AdminAiReviewQueryService::new(deps.clone());
    let (d1, d2, d3, d4, d5, d6) = (
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
        deps.clone(),
    );
    let (versions_create, versions_list, versions_get) =
        (versions.clone(), versions.clone(), versions);
    let (reviews_list, reviews_get) = (reviews.clone(), reviews);
    routes
        .post("/api/v1/admin/ai-prompt-versions", move |req| {
            let admin_id = resolve_current_admin(&d1)?;
            let body = read_json_object(req)?;
            let prompt = read_prompt_request(&body)?;
            let memo = body.string("memo")?;
            let command = PromptCommand { prompt, memo };
            let saved = versions_create.create_version(admin_id, &command)?;
            Ok(created_json(&VersionResponse::from(saved)))
        })
        .get("/api/v1/admin/ai-prompt-versions", move |req| {
            resolve_current_admin(&d2)?;
            get_versions(&versions_list, req)
        })
        .get(
            "/api/v1/admin/ai-prompt-versions/{promptVersionId}",
            move |req| {
                resolve_current_admin(&d3)?;
                let id = path_uuid(req, "promptVersionId")?;
                Ok(ok_json(&VersionResponse::from(
                    versions_get.get_version(id)?,
                )))
            },
        )
        .post("/api/v1/admin/ai-prompt-previews", move |req| {
            let admin_id = resolve_current_admin(&d4)?;
            let body = read_json_object(req)?;
            let prompt = read_prompt_request(&body)?;
            let post_id = match body.string("postId")? {
                Some(text) if has_text(&text) => parse_path_uuid(&text)?,
                _ => return Err(validation(MUST_NOT_BE_BLANK)),
            };
            let result = previews.preview(admin_id, &prompt, post_id)?;
            Ok(ok_json(&PreviewResponse::from(result)))
        })
        .get("/api/v1/admin/ai-reviews", move |req| {
            resolve_current_admin(&d5)?;
            get_ai_reviews(&reviews_list, req)
        })
        .get("/api/v1/admin/ai-reviews/{aiReviewId}", move |req| {
            resolve_current_admin(&d6)?;
            let id = path_uuid(req, "aiReviewId")?;
            let detail = reviews_get.get_ai_review(id)?;
            Ok(ok_json(&reviews_get.detail_response(detail)?))
        })
}

fn get_versions(service: &AdminAiPromptVersionService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let prompt_type = q.enumeration("type", AiPromptType::parse)?;
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let Some(prompt_type) = prompt_type else {
        return Err(validation(MUST_NOT_BE_NULL));
    };
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_versions(prompt_type, page, page_size)?;
    Ok(ok_json(&VersionListResponse {
        current_page: result.current_page,
        page_size: result.page_size,
        has_next: result.has_next,
        prompt_versions: result.items.into_iter().map(VersionItem::from).collect(),
    }))
}

fn parse_review_status(name: &str) -> Option<&'static str> {
    ["QUEUED", "PROCESSING", "COMPLETED", "FAILED"]
        .into_iter()
        .find(|status| *status == name)
}

fn get_ai_reviews(service: &AdminAiReviewQueryService, req: &Request) -> ApiResult {
    let q = QueryParams::new(req);
    let status = q.enumeration("status", parse_review_status)?;
    let prompt_version_id = q.uuid("promptVersionId")?;
    let page = q.integer("page")?;
    let page_size = q.integer("pageSize")?;
    let (page, page_size) = page_params(page, page_size)?;
    let result = service.get_ai_reviews(status, prompt_version_id, page, page_size)?;
    let items = result
        .items
        .into_iter()
        .map(|s| service.list_item(s))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ok_json(&ListResponse {
        current_page: result.current_page,
        page_size: result.page_size,
        has_next: result.has_next,
        ai_reviews: items,
    }))
}
