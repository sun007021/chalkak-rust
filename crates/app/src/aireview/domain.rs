//! AI 리뷰 도메인 값(원본 `aireview.domain`): `AiPromptType`, `AiPromptParameters`, `AiPromptDraft`, `AiPromptVersion`,
//! `PhotoReviewOutput`, `AiReviewResult`.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

/// `isBlank`: 비었거나 공백뿐.
pub fn is_blank(text: &str) -> bool {
    text.chars().all(is_java_whitespace)
}

// ---------------------------------------------------------------------------------------------
// AiPromptType
// ---------------------------------------------------------------------------------------------

/// AI 프롬프트의 용도. 용도마다 템플릿에서 쓸 수 있는 변수와 모델이 지켜야 할 출력 스키마 버전이 정해진다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiPromptType {
    PhotoReview,
}

impl AiPromptType {
    pub const ALL: [AiPromptType; 1] = [AiPromptType::PhotoReview];

    pub const fn as_str(self) -> &'static str {
        match self {
            AiPromptType::PhotoReview => "PHOTO_REVIEW",
        }
    }

    pub fn parse(name: &str) -> Option<AiPromptType> {
        AiPromptType::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// 주제 제목을 넣는 변수 이름.
    pub const fn topic_title_variable(self) -> &'static str {
        match self {
            AiPromptType::PhotoReview => "topicTitle",
        }
    }

    /// 사용자 프롬프트 템플릿에서 쓸 수 있는 변수.
    pub fn allowed_variables(self) -> Vec<&'static str> {
        vec![self.topic_title_variable()]
    }

    /// 서버가 해석하는 출력 스키마 버전(`PhotoReviewOutput.schemaVersion()`).
    pub fn output_schema_version(self) -> String {
        match self {
            AiPromptType::PhotoReview => PhotoReviewOutput::SCHEMA_VERSION.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// AiPromptParameters
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageDetail {
    Low,
    High,
    Auto,
}

impl ImageDetail {
    /// 선언 순서(Jackson 열거형의 서수 순서).
    pub const ALL: [(&'static str, ImageDetail); 3] = [
        ("LOW", ImageDetail::Low),
        ("HIGH", ImageDetail::High),
        ("AUTO", ImageDetail::Auto),
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ImageDetail::Low => "LOW",
            ImageDetail::High => "HIGH",
            ImageDetail::Auto => "AUTO",
        }
    }

    pub fn parse(name: &str) -> Option<ImageDetail> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    /// OpenAI에 보내는 소문자 이름.
    pub const fn lower(self) -> &'static str {
        match self {
            ImageDetail::Low => "low",
            ImageDetail::High => "high",
            ImageDetail::Auto => "auto",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub const ALL: [(&'static str, ReasoningEffort); 3] = [
        ("LOW", ReasoningEffort::Low),
        ("MEDIUM", ReasoningEffort::Medium),
        ("HIGH", ReasoningEffort::High),
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "LOW",
            ReasoningEffort::Medium => "MEDIUM",
            ReasoningEffort::High => "HIGH",
        }
    }

    pub fn parse(name: &str) -> Option<ReasoningEffort> {
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    pub const fn lower(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

const MIN_OUTPUT_TOKENS: i32 = 1;
const MAX_OUTPUT_TOKENS: i32 = 16000;
const DEFAULT_OUTPUT_TOKENS: i32 = 8000;
const DEFAULT_THRESHOLD: f64 = 7.0;
const MIN_THRESHOLD: f64 = 0.0;
const MAX_THRESHOLD: f64 = 10.0;

/// 모델 호출 옵션. 값이 `None`이면 imageDetail은 HIGH, maxOutputTokens는 8000, 기준은 7.0으로 정한다.
/// reasoningEffort가 `None`이면 모델 기본값을 쓰도록 전달하지 않는다.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AiPromptParameters {
    pub image_detail: ImageDetail,
    pub max_output_tokens: i32,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// 주제 표현 점수가 이 값 이상이면 독특한 시각으로 본다.
    pub distinctive_perspective_threshold: f64,
}

impl AiPromptParameters {
    /// 레코드의 compact 생성자: 기본값을 채우고 범위를 검사한다.
    pub fn new(
        image_detail: Option<ImageDetail>,
        max_output_tokens: Option<i32>,
        reasoning_effort: Option<ReasoningEffort>,
        distinctive_perspective_threshold: Option<f64>,
    ) -> Result<AiPromptParameters, AppError> {
        let max_output_tokens = max_output_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS);
        if !(MIN_OUTPUT_TOKENS..=MAX_OUTPUT_TOKENS).contains(&max_output_tokens) {
            return Err(business("최대 출력 토큰은 1 이상 16000 이하여야 합니다."));
        }
        let threshold = distinctive_perspective_threshold.unwrap_or(DEFAULT_THRESHOLD);
        // NaN은 어느 비교도 참이 아니므로 함께 걸러진다.
        if !(threshold >= MIN_THRESHOLD && threshold <= MAX_THRESHOLD) {
            return Err(business("독특한 시각 판정 기준은 0 이상 10 이하여야 합니다."));
        }
        Ok(AiPromptParameters {
            image_detail: image_detail.unwrap_or(ImageDetail::High),
            max_output_tokens,
            reasoning_effort,
            distinctive_perspective_threshold: threshold,
        })
    }

    /// DB(`parameters` jsonb)에 저장하는 JSON. Jackson이 레코드를 쓰는 모양(필드 선언 순서, null 포함)이다.
    pub fn to_json(&self) -> String {
        crate::web::json::to_json(&ParametersJson {
            image_detail: self.image_detail.as_str(),
            max_output_tokens: self.max_output_tokens,
            reasoning_effort: self.reasoning_effort.map(ReasoningEffort::as_str),
            distinctive_perspective_threshold: self.distinctive_perspective_threshold,
        })
    }

    /// 저장된 JSON을 읽는다. 값이 빠져 있으면 레코드 생성자처럼 기본값을 채운다.
    pub fn from_json(text: &str) -> Result<AiPromptParameters, String> {
        let stored: StoredParameters = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let image_detail = match stored.image_detail.as_deref() {
            None => None,
            Some(name) => Some(ImageDetail::parse(name).ok_or("알 수 없는 imageDetail")?),
        };
        let reasoning_effort = match stored.reasoning_effort.as_deref() {
            None => None,
            Some(name) => Some(ReasoningEffort::parse(name).ok_or("알 수 없는 reasoningEffort")?),
        };
        AiPromptParameters::new(
            image_detail,
            stored.max_output_tokens,
            reasoning_effort,
            stored.distinctive_perspective_threshold,
        )
        .map_err(|e| e.message)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ParametersJson {
    image_detail: &'static str,
    max_output_tokens: i32,
    reasoning_effort: Option<&'static str>,
    distinctive_perspective_threshold: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredParameters {
    image_detail: Option<String>,
    max_output_tokens: Option<i32>,
    reasoning_effort: Option<String>,
    distinctive_perspective_threshold: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// AiPromptDraft
// ---------------------------------------------------------------------------------------------

/// 저장 전에 미리 실행해 볼 프롬프트 초안. 사용자 프롬프트 템플릿의 `{{변수}}`는 용도가 허용한 것만 쓸 수 있다.
#[derive(Clone, Debug, PartialEq)]
pub struct AiPromptDraft {
    pub prompt_type: AiPromptType,
    pub system_prompt: String,
    pub user_prompt_template: String,
    pub model: String,
    pub parameters: AiPromptParameters,
}

/// 정규식 `\{\{([^{}]*)}}`의 일치를 왼쪽부터 찾는다. 반환값은 `(시작, 끝, 변수 이름)`의 바이트 위치다.
fn placeholders(text: &str) -> Vec<(usize, usize, &str)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let name_start = i + 2;
            let mut j = name_start;
            while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b'}' {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                found.push((i, j + 2, &text[name_start..j]));
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    found
}

/// Jackson `JsonStringEncoder.quoteAsString`: 따옴표·역슬래시·제어 문자만 이스케이프한다.
fn escape_json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl AiPromptDraft {
    pub fn new(
        prompt_type: AiPromptType,
        system_prompt: &str,
        user_prompt_template: &str,
        model: &str,
        parameters: AiPromptParameters,
    ) -> Result<AiPromptDraft, AppError> {
        if is_blank(system_prompt) {
            return Err(business("시스템 프롬프트를 입력해 주세요."));
        }
        if is_blank(user_prompt_template) {
            return Err(business("사용자 프롬프트 템플릿을 입력해 주세요."));
        }
        if is_blank(model) {
            return Err(business("모델을 입력해 주세요."));
        }
        if !placeholders(system_prompt).is_empty() {
            return Err(business("시스템 프롬프트에는 변수를 쓸 수 없습니다."));
        }
        let allowed = prompt_type.allowed_variables();
        if placeholders(user_prompt_template)
            .iter()
            .any(|(_, _, name)| !allowed.contains(name))
        {
            return Err(business(&format!(
                "사용할 수 없는 프롬프트 변수가 있습니다. 사용 가능: [{}]",
                allowed.join(", ")
            )));
        }
        Ok(AiPromptDraft {
            prompt_type,
            system_prompt: system_prompt.to_owned(),
            user_prompt_template: user_prompt_template.to_owned(),
            model: model.to_owned(),
            parameters,
        })
    }

    /// 주제 제목으로 템플릿의 변수를 치환한다. 값에 변수 표기가 들어 있어도 다시 치환하지 않고, JSON 문자열 안에 넣어도 유효하도록
    /// 이스케이프한다.
    pub fn render_user_prompt(&self, topic_title: &str) -> Result<String, String> {
        let template = &self.user_prompt_template;
        let variable = self.prompt_type.topic_title_variable();
        let mut rendered = String::with_capacity(template.len());
        let mut last = 0;
        for (start, end, name) in placeholders(template) {
            if name != variable {
                return Err("프롬프트 변수 값이 없습니다.".to_owned());
            }
            rendered.push_str(&template[last..start]);
            rendered.push_str(&escape_json_string(topic_title));
            last = end;
        }
        rendered.push_str(&template[last..]);
        Ok(rendered)
    }
}

// ---------------------------------------------------------------------------------------------
// AiPromptVersion
// ---------------------------------------------------------------------------------------------

const MAX_MEMO_LENGTH: usize = 500;

/// 관리자가 만든 AI 프롬프트 버전. 만든 뒤에는 바꾸지 않으며, 같은 용도에서 가장 큰 버전이 최신이다.
#[derive(Clone, Debug, PartialEq)]
pub struct AiPromptVersion {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub prompt_type: AiPromptType,
    pub version: i32,
    pub system_prompt: String,
    pub user_prompt_template: String,
    pub model: String,
    pub parameters: AiPromptParameters,
    pub output_schema_version: String,
    pub memo: Option<String>,
    pub created_by_admin_id: Uuid,
    pub created_at: Option<Timestamp>,
}

impl AiPromptVersion {
    pub fn create(
        draft: &AiPromptDraft,
        version: i32,
        memo: Option<&str>,
        created_by_admin_id: Uuid,
    ) -> Result<AiPromptVersion, AppError> {
        if version < 1 {
            return Err(business("프롬프트 버전 정보가 올바르지 않습니다."));
        }
        Ok(AiPromptVersion {
            id: None,
            prompt_type: draft.prompt_type,
            version,
            system_prompt: draft.system_prompt.clone(),
            user_prompt_template: draft.user_prompt_template.clone(),
            model: draft.model.clone(),
            parameters: draft.parameters,
            output_schema_version: draft.prompt_type.output_schema_version(),
            memo: normalize_memo(memo)?,
            created_by_admin_id,
            created_at: None,
        })
    }

    /// 이 버전이 모델에게 요구하는 출력 스키마가 서버가 지금 해석할 수 있는 버전인지.
    pub fn has_current_output_schema(&self) -> bool {
        self.output_schema_version == self.prompt_type.output_schema_version()
    }

    /// 이 버전을 지금 서버가 호출할 수 있는지. 서버가 해석하는 출력 스키마이고 허용된 모델이어야 한다.
    pub fn is_runnable_with(&self, model_supported: impl Fn(&str) -> bool) -> bool {
        self.has_current_output_schema() && model_supported(&self.model)
    }

    /// 저장된 값에서 초안을 되살린다. 저장 전에 이미 검증한 값이다.
    pub fn to_draft(&self) -> Result<AiPromptDraft, AppError> {
        AiPromptDraft::new(
            self.prompt_type,
            &self.system_prompt,
            &self.user_prompt_template,
            &self.model,
            self.parameters,
        )
    }
}

fn normalize_memo(memo: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(memo) = memo.filter(|m| !is_blank(m)) else {
        return Ok(None);
    };
    let normalized = memo.trim_matches(is_java_whitespace);
    if normalized.chars().count() > MAX_MEMO_LENGTH {
        return Err(business("메모는 500자 이하여야 합니다."));
    }
    Ok(Some(normalized.to_owned()))
}

// ---------------------------------------------------------------------------------------------
// AiReviewResult, PhotoReviewOutput
// ---------------------------------------------------------------------------------------------

/// 모델 출력을 서비스 규칙으로 변환한 사진 리뷰 결과. 필드 선언 순서가 JSON 출력 순서다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiReviewResult {
    pub overall_score: f64,
    pub criteria: Vec<Criterion>,
    pub distinctive_perspective: bool,
    pub feedback: Feedback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    pub summary: String,
    pub strength: String,
    pub weakness: String,
    pub suggestion: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub key: String,
    pub label: String,
    pub score: f64,
}

/// 사진 리뷰 프롬프트가 모델에게 요구하는 출력(스키마 버전 2). 종합 점수와 독특한 시각 여부는 모델이 아니라 서버가 계산한다.
///
/// 모델 출력은 사용자 입력이 아니므로 검증 실패는 `Err(String)`이고, 어댑터가 INVALID_OUTPUT으로 바꾼다.
#[derive(Clone, Debug, PartialEq)]
pub struct PhotoReviewOutput {
    pub composition_score: f64,
    pub theme_expression_score: f64,
    pub feedback: Feedback,
}

const MAX_SUMMARY_LENGTH: usize = 60;
const MAX_FEEDBACK_SECTION_LENGTH: usize = 150;
const COMPOSITION_KEY: &str = "COMPOSITION";
const COMPOSITION_LABEL: &str = "구도";

impl PhotoReviewOutput {
    pub const SCHEMA_VERSION: i32 = 2;
    pub const MIN_SCORE: i32 = 0;
    pub const MAX_SCORE: i32 = 10;

    pub fn new(
        composition_score: f64,
        theme_expression_score: f64,
        feedback: Feedback,
    ) -> Result<PhotoReviewOutput, String> {
        let range = f64::from(Self::MIN_SCORE)..=f64::from(Self::MAX_SCORE);
        if !range.contains(&composition_score) {
            return Err("구도 점수가 0 이상 10 이하가 아닙니다.".to_owned());
        }
        if !range.contains(&theme_expression_score) {
            return Err("주제 표현 점수가 0 이상 10 이하가 아닙니다.".to_owned());
        }
        validate_text(&feedback.summary, MAX_SUMMARY_LENGTH, "피드백 요약")?;
        validate_text(
            &feedback.strength,
            MAX_FEEDBACK_SECTION_LENGTH,
            "피드백 좋았던 점",
        )?;
        validate_text(
            &feedback.weakness,
            MAX_FEEDBACK_SECTION_LENGTH,
            "피드백 아쉬운 점",
        )?;
        validate_text(
            &feedback.suggestion,
            MAX_FEEDBACK_SECTION_LENGTH,
            "피드백 다음 제안",
        )?;
        Ok(PhotoReviewOutput {
            composition_score,
            theme_expression_score,
            feedback,
        })
    }

    /// 주제 표현 점수가 기준 이상이면 독특한 시각으로 판정한다. 원점수는 결과에 드러내지 않는다.
    pub fn to_result(&self, distinctive_perspective_threshold: f64) -> AiReviewResult {
        let composition = round_half_up_one_decimal(self.composition_score);
        // 현재 종합 점수는 구도 점수 하나로 정한다. 항목이 늘면 이 계산을 바꾼다.
        AiReviewResult {
            overall_score: composition,
            criteria: vec![Criterion {
                key: COMPOSITION_KEY.to_owned(),
                label: COMPOSITION_LABEL.to_owned(),
                score: composition,
            }],
            distinctive_perspective: self.theme_expression_score
                >= distinctive_perspective_threshold,
            feedback: self.feedback.clone(),
        }
    }
}

fn validate_text(text: &str, max_length: usize, name: &str) -> Result<(), String> {
    if is_blank(text) {
        return Err(format!("{name}이 비어 있습니다."));
    }
    if text.chars().count() > max_length {
        return Err(format!("{name}이 최대 길이를 넘었습니다."));
    }
    Ok(())
}

/// `BigDecimal.valueOf(score).setScale(1, RoundingMode.HALF_UP)`: 가장 짧은 십진 표기를 기준으로 반올림한다.
fn round_half_up_one_decimal(score: f64) -> f64 {
    let text = format!("{score}");
    let (integer, fraction) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let digits: Vec<u8> = fraction.bytes().map(|b| b - b'0').collect();
    let first = digits.first().copied().unwrap_or(0);
    let round_up = digits.get(1).copied().unwrap_or(0) >= 5;
    let scaled = integer.parse::<i64>().unwrap_or(0).abs() * 10 + i64::from(first) + i64::from(round_up);
    let rounded = scaled as f64 / 10.0;
    if score < 0.0 { -rounded } else { rounded }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> AiPromptParameters {
        AiPromptParameters::new(None, None, None, None).unwrap()
    }

    fn draft(system: &str, template: &str, model: &str) -> Result<AiPromptDraft, AppError> {
        AiPromptDraft::new(AiPromptType::PhotoReview, system, template, model, params())
    }

    fn feedback() -> Feedback {
        Feedback {
            summary: "요약".into(),
            strength: "좋음".into(),
            weakness: "아쉬움".into(),
            suggestion: "제안".into(),
        }
    }

    // AiPromptParametersTest
    #[test]
    fn 옵션을_생략하면_기본값을_채운다() {
        let p = params();
        assert_eq!(p.image_detail, ImageDetail::High);
        assert_eq!(p.max_output_tokens, 8000);
        assert_eq!(p.reasoning_effort, None);
        assert_eq!(p.distinctive_perspective_threshold, 7.0);
    }

    #[test]
    fn 최대_출력_토큰은_1_이상_16000_이하다() {
        for ok in [1, 16000] {
            assert!(AiPromptParameters::new(None, Some(ok), None, None).is_ok());
        }
        for bad in [0, -1, 16001] {
            let e = AiPromptParameters::new(None, Some(bad), None, None).unwrap_err();
            assert_eq!(e.message, "최대 출력 토큰은 1 이상 16000 이하여야 합니다.");
        }
    }

    #[test]
    fn 독특한_시각_기준은_0_이상_10_이하다() {
        for ok in [0.0, 6.5, 10.0] {
            assert!(AiPromptParameters::new(None, None, None, Some(ok)).is_ok());
        }
        for bad in [-0.1, 10.1, f64::NAN, f64::INFINITY] {
            let e = AiPromptParameters::new(None, None, None, Some(bad)).unwrap_err();
            assert_eq!(e.message, "독특한 시각 판정 기준은 0 이상 10 이하여야 합니다.");
        }
    }

    #[test]
    fn 옵션은_레코드_모양_json으로_저장하고_다시_읽는다() {
        let p = AiPromptParameters::new(
            Some(ImageDetail::Low),
            Some(700),
            Some(ReasoningEffort::High),
            Some(6.5),
        )
        .unwrap();
        assert_eq!(
            p.to_json(),
            r#"{"imageDetail":"LOW","maxOutputTokens":700,"reasoningEffort":"HIGH","distinctivePerspectiveThreshold":6.5}"#
        );
        assert_eq!(AiPromptParameters::from_json(&p.to_json()).unwrap(), p);
        assert_eq!(
            params().to_json(),
            r#"{"imageDetail":"HIGH","maxOutputTokens":8000,"reasoningEffort":null,"distinctivePerspectiveThreshold":7.0}"#
        );
        assert_eq!(AiPromptParameters::from_json("{}").unwrap(), params());
    }

    // AiPromptDraftTest
    #[test]
    fn 시스템_프롬프트_템플릿_모델이_비면_거절한다() {
        assert_eq!(
            draft(" ", "{{topicTitle}}", "m").unwrap_err().message,
            "시스템 프롬프트를 입력해 주세요."
        );
        assert_eq!(
            draft("s", "", "m").unwrap_err().message,
            "사용자 프롬프트 템플릿을 입력해 주세요."
        );
        assert_eq!(
            draft("s", "t", "\t").unwrap_err().message,
            "모델을 입력해 주세요."
        );
    }

    #[test]
    fn 시스템_프롬프트에는_변수를_쓸_수_없다() {
        assert_eq!(
            draft("{{topicTitle}}", "t", "m").unwrap_err().message,
            "시스템 프롬프트에는 변수를 쓸 수 없습니다."
        );
        // 닫히지 않은 표기는 변수가 아니다.
        assert!(draft("{{topicTitle", "t", "m").is_ok());
        assert!(draft("{{ }", "t", "m").is_ok());
    }

    #[test]
    fn 템플릿은_허용한_변수만_쓸_수_있다() {
        assert!(draft("s", "{{topicTitle}} {{topicTitle}}", "m").is_ok());
        for bad in ["{{title}}", "{{ topicTitle }}", "{{}}", "{{topicTitle}}{{x}}"] {
            assert_eq!(
                draft("s", bad, "m").unwrap_err().message,
                "사용할 수 없는 프롬프트 변수가 있습니다. 사용 가능: [topicTitle]",
                "{bad}"
            );
        }
    }

    #[test]
    fn 변수를_한_번에_치환하고_json_문자열로_이스케이프한다() {
        let d = draft("s", r#"{"주제": "{{topicTitle}}", "다시": "{{topicTitle}}"}"#, "m").unwrap();
        assert_eq!(
            d.render_user_prompt("노을 \"{{topicTitle}}\"\n\\").unwrap(),
            r#"{"주제": "노을 \"{{topicTitle}}\"\n\\", "다시": "노을 \"{{topicTitle}}\"\n\\"}"#
        );
        assert_eq!(
            draft("s", "제목 {{topicTitle}}", "m")
                .unwrap()
                .render_user_prompt("a\u{1}b")
                .unwrap(),
            "제목 a\\u0001b"
        );
    }

    // AiPromptVersionTest
    #[test]
    fn 버전을_만들면_출력_스키마_버전을_유형에서_가져오고_메모를_다듬는다() {
        let d = draft("s", "{{topicTitle}}", "gpt-6.1-sol").unwrap();
        let admin = Uuid::new_v4();
        let v = AiPromptVersion::create(&d, 3, Some("  메모  "), admin).unwrap();
        assert_eq!(v.version, 3);
        assert_eq!(v.output_schema_version, "2");
        assert_eq!(v.memo.as_deref(), Some("메모"));
        assert_eq!(v.created_by_admin_id, admin);
        for blank in [None, Some(""), Some("  \n")] {
            assert_eq!(AiPromptVersion::create(&d, 1, blank, admin).unwrap().memo, None);
        }
        assert!(AiPromptVersion::create(&d, 0, None, admin).is_err());
    }

    #[test]
    fn 메모는_공백을_뗀_뒤_코드포인트_500자까지다() {
        let d = draft("s", "t", "m").unwrap();
        let admin = Uuid::new_v4();
        let ok = format!(" {} ", "가".repeat(500));
        assert!(AiPromptVersion::create(&d, 1, Some(&ok), admin).is_ok());
        assert!(AiPromptVersion::create(&d, 1, Some(&"😀".repeat(500)), admin).is_ok());
        let e = AiPromptVersion::create(&d, 1, Some(&"😀".repeat(501)), admin).unwrap_err();
        assert_eq!(e.message, "메모는 500자 이하여야 합니다.");
    }

    #[test]
    fn 현재_서버가_해석하는_스키마이고_허용된_모델이어야_실행할_수_있다() {
        let d = draft("s", "t", "gpt-6.1-sol").unwrap();
        let mut v = AiPromptVersion::create(&d, 1, None, Uuid::new_v4()).unwrap();
        assert!(v.is_runnable_with(|m| m == "gpt-6.1-sol"));
        assert!(!v.is_runnable_with(|m| m == "other"));
        v.output_schema_version = "1".into();
        assert!(!v.has_current_output_schema());
        assert!(!v.is_runnable_with(|_| true));
    }

    // PhotoReviewOutputTest
    #[test]
    fn 점수와_피드백이_범위를_벗어나면_거절한다() {
        assert!(PhotoReviewOutput::new(0.0, 10.0, feedback()).is_ok());
        for (c, t) in [(-0.1, 5.0), (10.1, 5.0), (5.0, -1.0), (5.0, 11.0), (f64::NAN, 5.0)] {
            assert!(PhotoReviewOutput::new(c, t, feedback()).is_err(), "{c} {t}");
        }
        let mut f = feedback();
        f.summary = "가".repeat(60);
        assert!(PhotoReviewOutput::new(5.0, 5.0, f.clone()).is_ok());
        f.summary = "가".repeat(61);
        assert!(PhotoReviewOutput::new(5.0, 5.0, f).is_err());
        let mut f = feedback();
        f.weakness = "가".repeat(151);
        assert!(PhotoReviewOutput::new(5.0, 5.0, f).is_err());
        let mut f = feedback();
        f.suggestion = " ".into();
        assert!(PhotoReviewOutput::new(5.0, 5.0, f).is_err());
    }

    #[test]
    fn 결과는_구도_점수_하나로_종합_점수를_정하고_소수_첫째_자리에서_반올림한다() {
        let output = PhotoReviewOutput::new(8.72, 8.0, feedback()).unwrap();
        let result = output.to_result(7.0);
        assert_eq!(result.overall_score, 8.7);
        assert_eq!(
            result.criteria,
            vec![Criterion {
                key: "COMPOSITION".into(),
                label: "구도".into(),
                score: 8.7
            }]
        );
        assert!(result.distinctive_perspective);
        for (raw, expected) in [(8.75, 8.8), (0.05, 0.1), (9.95, 10.0), (7.0, 7.0), (0.0, 0.0), (6.04, 6.0)] {
            let r = PhotoReviewOutput::new(raw, 0.0, feedback()).unwrap().to_result(7.0);
            assert_eq!(r.overall_score, expected, "{raw}");
        }
    }

    #[test]
    fn 독특한_시각은_주제_표현_점수가_기준_이상일_때다() {
        for (theme, threshold, expected) in
            [(6.0, 7.0, false), (6.0, 6.0, true), (7.5, 7.5, true), (7.4, 7.5, false)]
        {
            let r = PhotoReviewOutput::new(8.0, theme, feedback()).unwrap().to_result(threshold);
            assert_eq!(r.distinctive_perspective, expected, "{theme} {threshold}");
        }
    }

    #[test]
    fn 결과_json은_레코드_순서다() {
        let r = PhotoReviewOutput::new(8.0, 8.0, feedback()).unwrap().to_result(7.0);
        assert_eq!(
            crate::web::json::to_json(&r),
            r#"{"overallScore":8.0,"criteria":[{"key":"COMPOSITION","label":"구도","score":8.0}],"distinctivePerspective":true,"feedback":{"summary":"요약","strength":"좋음","weakness":"아쉬움","suggestion":"제안"}}"#
        );
    }
}
