//! OpenAI Responses API의 Structured Outputs로 사진 리뷰를 생성하는 어댑터(원본 `OpenAiResponsesReviewGenerator`,
//! `OpenAiProperties`, `OpenAiConfiguration`).
//!
//! 요청·응답 본문과 API 키, 이미지 주소는 로그나 오류 메시지에 남기지 않는다. HTTP는 `chalkak-http` 클라이언트로 직접 보낸다.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chalkak_core::Clock;
use chalkak_core::config::{ConfigError, Env};
use chalkak_http::client::{ClientError, HttpClient};
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use super::domain::{Feedback, PhotoReviewOutput};
use super::generator::{
    AiReviewGeneration, AiReviewGenerationError, AiReviewGenerationRequest, AiReviewGenerator,
    Attempt, FailureType, Usage,
};
use crate::web::json::to_json;

const MESSAGE_TYPE: &str = "message";
const OUTPUT_TEXT_TYPE: &str = "output_text";
const REFUSAL_TYPE: &str = "refusal";
const STATUS_INCOMPLETE: &str = "incomplete";
const STATUS_COMPLETED: &str = "completed";
const STATUS_FAILED: &str = "failed";
// 429 등 HTTP 오류 응답이 알려 주는 최소 대기 시간. 밀리초 헤더가 더 정확하므로 먼저 읽는다.
const RETRY_AFTER_MS_HEADER: &str = "retry-after-ms";
const RETRY_AFTER_HEADER: &str = "retry-after";
const RATE_LIMIT_ERROR_CODE: &str = "rate_limit_exceeded";
// status=failed의 error.code 중 정책 위반은 같은 입력을 다시 보내도 거절되므로 재시도 대상이 아니다.
const REFUSED_ERROR_CODES: [&str; 3] = [
    "image_content_policy_violation",
    "bio_policy",
    "misalignment_policy_violation",
];
// 프롬프트나 이미지 자체가 잘못된 경우라 재시도해도 같은 결과다. 목록에 없는 코드는 일시 장애로 본다.
const BAD_REQUEST_ERROR_CODES: [&str; 14] = [
    "invalid_prompt",
    "data_residency_mismatch",
    "invalid_image",
    "invalid_image_format",
    "invalid_base64_image",
    "invalid_image_url",
    "image_too_large",
    "image_too_small",
    "image_parse_error",
    "invalid_image_mode",
    "image_file_too_large",
    "unsupported_image_media_type",
    "empty_image_file",
    "image_file_not_found",
];
// 429여도 code 또는 type이 결제·한도 소진이면 기다려도 풀리지 않아 재시도 대상이 아니다.
const QUOTA_ERROR_CODES: [&str; 5] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// OpenAI Structured Outputs 전용 스키마. strict 모드는 모든 필드를 required로 두고 additionalProperties를 막아야 한다.
const PHOTO_REVIEW_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["composition","theme_expression","feedback"],"properties":{"composition":{"type":"number","minimum":0,"maximum":10},"theme_expression":{"type":"number","minimum":0,"maximum":10},"feedback":{"type":"object","additionalProperties":false,"required":["summary","strength","weakness","suggestion"],"properties":{"summary":{"type":"string"},"strength":{"type":"string"},"weakness":{"type":"string"},"suggestion":{"type":"string"}}}}}"#;

// ---------------------------------------------------------------------------------------------
// 설정 (chalkak.ai-review.openai.*)
// ---------------------------------------------------------------------------------------------

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_ALLOWED_MODELS: &str = "gpt-6.1-sol,gpt-6-luna";

#[derive(Clone)]
pub struct OpenAiSettings {
    pub api_key: String,
    pub base_url: String,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub allowed_models: Vec<String>,
}

// API 키가 로그나 오류에 찍혀도 노출되지 않게 한다.
impl fmt::Debug for OpenAiSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiSettings")
            .field("base_url", &self.base_url)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("allowed_models", &self.allowed_models)
            .finish()
    }
}

fn invalid(key: &str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        reason: reason.to_owned(),
    }
}

impl OpenAiSettings {
    /// `OPENAI_API_KEY`는 필수다. 나머지는 `application.yml`의 정책 값이 기본이고, 같은 이름(Spring 느슨한 바인딩)의
    /// 환경변수로 덮어쓸 수 있다(하네스가 가짜 서버로 돌릴 때 쓴다).
    pub fn from_env(env: &Env) -> Result<OpenAiSettings, ConfigError> {
        let api_key = env.required("OPENAI_API_KEY")?;
        if api_key.trim().is_empty() {
            return Err(invalid("OPENAI_API_KEY", "비어 있을 수 없습니다"));
        }
        let positive = |key: &str, default: Duration| -> Result<Duration, ConfigError> {
            let d = env.duration_or(key, default)?;
            if d.is_zero() {
                return Err(invalid(key, "0보다 커야 합니다"));
            }
            Ok(d)
        };
        let models_key = "CHALKAK_AIREVIEW_OPENAI_ALLOWEDMODELS";
        let allowed_models: Vec<String> = env
            .string_or(models_key, DEFAULT_ALLOWED_MODELS)
            .split(',')
            .map(|m| m.trim().to_owned())
            .collect();
        if allowed_models.iter().any(String::is_empty) {
            return Err(invalid(models_key, "비어 있지 않은 이름이어야 합니다"));
        }
        Ok(OpenAiSettings {
            api_key: api_key.to_owned(),
            base_url: env
                .string_or("CHALKAK_AIREVIEW_OPENAI_BASEURL", DEFAULT_BASE_URL)
                .to_owned(),
            connect_timeout: positive(
                "CHALKAK_AIREVIEW_OPENAI_CONNECTTIMEOUT",
                Duration::from_secs(5),
            )?,
            read_timeout: positive(
                "CHALKAK_AIREVIEW_OPENAI_READTIMEOUT",
                Duration::from_secs(60),
            )?,
            allowed_models,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 요청 본문
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: Vec<InputMessage<'a>>,
    text: TextOptions,
    store: bool,
    max_output_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
}

#[derive(Serialize)]
struct InputMessage<'a> {
    role: &'a str,
    content: Vec<InputContent<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum InputContent<'a> {
    Text {
        r#type: &'a str,
        text: &'a str,
    },
    Image {
        r#type: &'a str,
        image_url: &'a str,
        detail: &'a str,
    },
}

#[derive(Serialize)]
struct TextOptions {
    format: JsonSchemaFormat,
}

#[derive(Serialize)]
struct JsonSchemaFormat {
    r#type: &'static str,
    name: String,
    strict: bool,
    schema: Box<RawValue>,
}

#[derive(Serialize)]
struct Reasoning {
    effort: &'static str,
}

// ---------------------------------------------------------------------------------------------
// 어댑터
// ---------------------------------------------------------------------------------------------

pub struct OpenAiResponsesReviewGenerator {
    client: HttpClient,
    clock: Arc<dyn Clock>,
    api_key: String,
    responses_url: String,
    allowed_models: Vec<String>,
    read_timeout: Duration,
}

impl OpenAiResponsesReviewGenerator {
    pub fn new(
        settings: &OpenAiSettings,
        clock: Arc<dyn Clock>,
    ) -> Result<OpenAiResponsesReviewGenerator, ClientError> {
        // 리다이렉트는 따라가지 않는다(클라이언트가 따르지 않는다).
        let client = HttpClient::builder()
            .connect_timeout(settings.connect_timeout)
            .read_timeout(settings.read_timeout)
            .build()?;
        Ok(OpenAiResponsesReviewGenerator {
            client,
            clock,
            api_key: settings.api_key.clone(),
            responses_url: format!("{}/responses", settings.base_url.trim_end_matches('/')),
            allowed_models: settings.allowed_models.clone(),
            read_timeout: settings.read_timeout,
        })
    }

    fn request_body(&self, request: &AiReviewGenerationRequest) -> Result<String, String> {
        let draft = &request.draft;
        let parameters = &draft.parameters;
        let user_text = draft.render_user_prompt(&request.topic_title)?;
        let schema =
            RawValue::from_string(PHOTO_REVIEW_SCHEMA.to_owned()).map_err(|e| e.to_string())?;
        let body = ResponsesRequest {
            model: &draft.model,
            instructions: &draft.system_prompt,
            input: vec![InputMessage {
                role: "user",
                content: vec![
                    InputContent::Text {
                        r#type: "input_text",
                        text: &user_text,
                    },
                    InputContent::Image {
                        r#type: "input_image",
                        image_url: &request.image_url,
                        detail: parameters.image_detail.lower(),
                    },
                ],
            }],
            text: TextOptions {
                format: JsonSchemaFormat {
                    r#type: "json_schema",
                    name: format!("photo_review_v{}", PhotoReviewOutput::SCHEMA_VERSION),
                    strict: true,
                    schema,
                },
            },
            store: false,
            max_output_tokens: parameters.max_output_tokens,
            reasoning: parameters
                .reasoning_effort
                .map(|e| Reasoning { effort: e.lower() }),
        };
        Ok(to_json(&body))
    }

    fn elapsed(&self, started_at: jiff::Timestamp) -> Duration {
        let millis = self.clock.now().as_millisecond() - started_at.as_millisecond();
        Duration::from_millis(u64::try_from(millis).unwrap_or(0))
    }

    /// 읽기 제한 시간 안에 응답이 없었으면 시간 초과, 그 밖의 전송 오류는 상위 오류.
    fn transport_failure(
        &self,
        error: &ClientError,
        started_at: jiff::Timestamp,
    ) -> AiReviewGenerationError {
        let timed_out = matches!(
            error,
            ClientError::ReadTimeout | ClientError::ConnectTimeout
        );
        let deadline_exceeded = self.elapsed(started_at) >= self.read_timeout;
        AiReviewGenerationError::of(if timed_out || deadline_exceeded {
            FailureType::Timeout
        } else {
            FailureType::UpstreamError
        })
    }

    fn send(
        &self,
        body: &str,
        started_at: jiff::Timestamp,
    ) -> Result<String, AiReviewGenerationError> {
        let authorization = format!("Bearer {}", self.api_key);
        let response = self
            .client
            .post_json(
                &self.responses_url,
                &[("Authorization", authorization.as_str())],
                body,
            )
            .map_err(|e| self.transport_failure(&e, started_at))?;
        if !response.is_success() {
            let failure_type = http_failure_type(response.status, &response.body);
            return Err(
                match retry_after_of(failure_type, |name| response.header(name)) {
                    Some(retry_after) => {
                        AiReviewGenerationError::with_retry_after(failure_type, retry_after)
                    }
                    None => AiReviewGenerationError::of(failure_type),
                },
            );
        }
        Ok(response.body_text())
    }
}

impl AiReviewGenerator for OpenAiResponsesReviewGenerator {
    fn generate(
        &self,
        request: &AiReviewGenerationRequest,
    ) -> Result<AiReviewGeneration, AiReviewGenerationError> {
        // 템플릿 검증을 이미 통과한 초안이라 치환은 실패하지 않는다.
        let body = self
            .request_body(request)
            .map_err(|_| AiReviewGenerationError::of(FailureType::BadRequest))?;
        let started_at = self.clock.now();
        let response_body = self.send(&body, started_at)?;
        let latency = self.elapsed(started_at);
        parse(
            &response_body,
            latency,
            request.draft.parameters.distinctive_perspective_threshold,
        )
    }

    fn supports_model(&self, model: &str) -> bool {
        self.allowed_models.iter().any(|m| m == model)
    }
}

// ---------------------------------------------------------------------------------------------
// 오류 응답 분류
// ---------------------------------------------------------------------------------------------

/// 429의 원인을 가르는 error.code·type만 읽는다. 본문이 비었거나 읽을 수 없으면 비어 있는 값으로 본다.
fn read_error_body(body: &[u8]) -> ErrorBody {
    let Ok(Value::Object(envelope)) = serde_json::from_slice::<Value>(body) else {
        return ErrorBody::default();
    };
    match envelope.get("error") {
        Some(Value::Object(error)) => ErrorBody {
            code: string_field(error, "code"),
            kind: string_field(error, "type"),
        },
        _ => ErrorBody::default(),
    }
}

fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

#[derive(Default)]
struct ErrorBody {
    code: Option<String>,
    kind: Option<String>,
}

fn http_failure_type(status: u16, body: &[u8]) -> FailureType {
    match status {
        429 => {
            let error = read_error_body(body);
            let exhausted = [&error.code, &error.kind]
                .into_iter()
                .flatten()
                .any(|v| QUOTA_ERROR_CODES.contains(&v.as_str()));
            if exhausted {
                FailureType::QuotaExceeded
            } else {
                FailureType::RateLimited
            }
        }
        408 => FailureType::Timeout,
        409 => FailureType::UpstreamError,
        400..=499 => FailureType::BadRequest,
        _ => FailureType::UpstreamError,
    }
}

/// 재시도하지 않는 유형은 기다려도 소용없으므로 헤더가 있어도 싣지 않는다. 날짜 형식의 retry-after는 읽지 않는다.
fn retry_after_of<'a>(
    failure_type: FailureType,
    header: impl Fn(&str) -> Option<&'a str>,
) -> Option<Duration> {
    if !failure_type.is_retryable() {
        return None;
    }
    parse_delay(header(RETRY_AFTER_MS_HEADER), Duration::from_millis)
        .or_else(|| parse_delay(header(RETRY_AFTER_HEADER), Duration::from_secs))
}

fn parse_delay(value: Option<&str>, unit: fn(u64) -> Duration) -> Option<Duration> {
    let amount = value?.trim().parse::<i64>().ok()?;
    u64::try_from(amount).ok().map(unit)
}

/// status=failed 응답의 error.code로 실패 유형을 가른다.
fn failure_type_of_error(error: Option<&ErrorBody>) -> FailureType {
    let Some(code) = error.and_then(|e| e.code.as_deref()) else {
        return FailureType::UpstreamError;
    };
    if code == RATE_LIMIT_ERROR_CODE {
        FailureType::RateLimited
    } else if REFUSED_ERROR_CODES.contains(&code) {
        FailureType::Refused
    } else if BAD_REQUEST_ERROR_CODES.contains(&code) {
        FailureType::BadRequest
    } else {
        FailureType::UpstreamError
    }
}

// ---------------------------------------------------------------------------------------------
// 성공 응답 해석
// ---------------------------------------------------------------------------------------------

/// Jackson의 느슨한 변환을 따라 스칼라는 문자열로 받고 배열·객체는 거절한다.
fn lenient_string(value: Option<&Value>) -> Result<Option<String>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(_) => Err(()),
    }
}

fn lenient_int(value: Option<&Value>) -> Result<Option<i32>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i32::try_from(i).map(Some).map_err(|_| ())
            } else {
                let f = n.as_f64().ok_or(())?;
                if f.is_finite() && f.abs() <= f64::from(i32::MAX) {
                    #[allow(clippy::cast_possible_truncation)]
                    Ok(Some(f as i32))
                } else {
                    Err(())
                }
            }
        }
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s.trim().parse::<i32>().map(Some).map_err(|_| ()),
        Some(_) => Err(()),
    }
}

fn lenient_object(value: Option<&Value>) -> Result<Option<&Map<String, Value>>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(()),
    }
}

fn lenient_array(value: Option<&Value>) -> Result<Option<&Vec<Value>>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => Ok(Some(items)),
        Some(_) => Err(()),
    }
}

struct ContentPart {
    kind: Option<String>,
    text: Option<String>,
}

struct OutputItem {
    kind: Option<String>,
    content: Option<Vec<Option<ContentPart>>>,
}

struct ParsedResponse {
    id: Option<String>,
    status: Option<String>,
    output: Option<Vec<Option<OutputItem>>>,
    usage: Usage,
    error: Option<ErrorBody>,
}

impl ParsedResponse {
    /// `output`이나 `content`에 null 요소가 있는지.
    fn has_null_element(&self) -> bool {
        self.output.as_ref().is_some_and(|items| {
            items.iter().any(|item| match item {
                None => true,
                Some(item) => item
                    .content
                    .as_ref()
                    .is_some_and(|parts| parts.iter().any(Option::is_none)),
            })
        })
    }

    fn content_parts(&self) -> Vec<&ContentPart> {
        self.output
            .iter()
            .flatten()
            .flatten()
            .filter(|item| item.kind.as_deref() == Some(MESSAGE_TYPE))
            .filter_map(|item| item.content.as_ref())
            .flatten()
            .flatten()
            .collect()
    }
}

fn read_response(body: &str) -> Result<ParsedResponse, ()> {
    if body.trim().is_empty() {
        return Err(());
    }
    let value: Value = serde_json::from_str(body).map_err(|_| ())?;
    let root = lenient_object(Some(&value))?.ok_or(())?;
    let output = match lenient_array(root.get("output"))? {
        None => None,
        Some(items) => Some(
            items
                .iter()
                .map(read_output_item)
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    Ok(ParsedResponse {
        id: lenient_string(root.get("id"))?,
        status: lenient_string(root.get("status"))?,
        output,
        usage: read_usage(root.get("usage"))?,
        error: match lenient_object(root.get("error"))? {
            None => None,
            Some(error) => Some(ErrorBody {
                code: lenient_string(error.get("code"))?,
                kind: lenient_string(error.get("type"))?,
            }),
        },
    })
}

fn read_output_item(value: &Value) -> Result<Option<OutputItem>, ()> {
    let Some(item) = lenient_object(Some(value))? else {
        return Ok(None);
    };
    let content = match lenient_array(item.get("content"))? {
        None => None,
        Some(parts) => Some(
            parts
                .iter()
                .map(|part| {
                    Ok(lenient_object(Some(part))?.map(|part| ContentPart {
                        kind: lenient_string(part.get("type")).ok().flatten(),
                        text: lenient_string(part.get("text")).ok().flatten(),
                    }))
                })
                .collect::<Result<Vec<_>, ()>>()?,
        ),
    };
    Ok(Some(OutputItem {
        kind: lenient_string(item.get("type"))?,
        content,
    }))
}

fn read_usage(value: Option<&Value>) -> Result<Usage, ()> {
    let Some(usage) = lenient_object(value)? else {
        return Ok(Usage::default());
    };
    let cached = match lenient_object(usage.get("input_tokens_details"))? {
        None => None,
        Some(details) => lenient_int(details.get("cached_tokens"))?,
    };
    let reasoning = match lenient_object(usage.get("output_tokens_details"))? {
        None => None,
        Some(details) => lenient_int(details.get("reasoning_tokens"))?,
    };
    Ok(Usage {
        input_tokens: lenient_int(usage.get("input_tokens"))?,
        cached_input_tokens: cached,
        output_tokens: lenient_int(usage.get("output_tokens"))?,
        reasoning_tokens: reasoning,
    })
}

fn parse(
    response_body: &str,
    latency: Duration,
    threshold: f64,
) -> Result<AiReviewGeneration, AiReviewGenerationError> {
    let response = read_response(response_body)
        .map_err(|()| AiReviewGenerationError::of(FailureType::InvalidOutput))?;
    let attempt = Attempt {
        usage: response.usage,
        latency,
        response_id: response.id.clone(),
    };
    let fail = |failure_type| AiReviewGenerationError::with_attempt(failure_type, attempt.clone());
    match response.status.as_deref() {
        Some(STATUS_INCOMPLETE) => return Err(fail(FailureType::Incomplete)),
        Some(STATUS_FAILED) => {
            return Err(fail(failure_type_of_error(response.error.as_ref())));
        }
        Some(STATUS_COMPLETED) => {}
        _ => return Err(fail(FailureType::UpstreamError)),
    }
    if response.has_null_element() {
        return Err(fail(FailureType::InvalidOutput));
    }
    let parts = response.content_parts();
    if parts
        .iter()
        .any(|part| part.kind.as_deref() == Some(REFUSAL_TYPE))
    {
        return Err(fail(FailureType::Refused));
    }
    let output_text = parts
        .iter()
        .filter(|part| part.kind.as_deref() == Some(OUTPUT_TEXT_TYPE))
        .find_map(|part| part.text.clone())
        .ok_or_else(|| fail(FailureType::InvalidOutput))?;
    let output = read_output(&output_text).ok_or_else(|| fail(FailureType::InvalidOutput))?;
    Ok(AiReviewGeneration {
        result: output.to_result(threshold),
        output,
        raw_output: output_text,
        attempt: attempt.clone(),
    })
}

/// 모델 출력의 JSON 타입을 직접 확인한다. 문자열 "7" 같은 값은 숫자로 받지 않는다.
fn read_output(output_text: &str) -> Option<PhotoReviewOutput> {
    let value: Value = serde_json::from_str(output_text).ok()?;
    let payload = value.as_object()?;
    let composition = payload.get("composition")?.as_f64()?;
    let theme_expression = payload.get("theme_expression")?.as_f64()?;
    let feedback = payload.get("feedback")?.as_object()?;
    let text = |key: &str| -> Option<String> {
        match feedback.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    };
    let feedback = Feedback {
        summary: text("summary")?,
        strength: text("strength")?,
        weakness: text("weakness")?,
        suggestion: text("suggestion")?,
    };
    PhotoReviewOutput::new(composition, theme_expression, feedback).ok()
}

#[cfg(test)]
mod tests;
