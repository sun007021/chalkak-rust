//! 원본 `OpenAiResponsesReviewGeneratorTest`의 기대값을 가짜 HTTP 서버로 확인한다.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chalkak_core::Clock;
use jiff::Timestamp;
use serde_json::{Value, json};

use super::*;
use crate::aireview::domain::{
    AiPromptDraft, AiPromptParameters, AiPromptType, Criterion, Feedback, ImageDetail,
    ReasoningEffort,
};

const API_KEY: &str = "sk-test-secret";
const IMAGE_URL: &str = "https://cdn.test/photo.jpg";
const FEEDBACK: &str = r#"{"summary":"구도가 좋습니다.","strength":"빛이 좋습니다.","weakness":"수평이 아쉽습니다.","suggestion":"낮게 찍어 보세요."}"#;

fn ok_output() -> String {
    format!(r#"{{"composition":8.72,"theme_expression":8,"feedback":{FEEDBACK}}}"#)
}

// ---- 가짜 OpenAI 서버 ----

enum Reply {
    Http {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    /// 응답 없이 연결을 닫는다.
    Close,
    /// 읽기 제한 시간보다 오래 가만히 있는다.
    Hang(Duration),
}

fn http(status: u16, body: &str) -> Reply {
    Reply::Http {
        status,
        headers: vec![("Content-Type", "application/json".to_owned())],
        body: body.to_owned(),
    }
}

#[derive(Clone, Debug)]
struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

struct FakeServer {
    base_url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
}

fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some(Captured {
        request_line,
        headers,
        body: String::from_utf8_lossy(&data[head_end..]).into_owned(),
    })
}

impl FakeServer {
    fn start(replies: Vec<Reply>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let mut replies: VecDeque<Reply> = replies.into();
        thread::spawn(move || {
            while let Some(reply) = replies.pop_front() {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                if let Some(request) = read_request(&mut stream) {
                    sink.lock().unwrap().push(request);
                }
                match reply {
                    Reply::Close => {}
                    Reply::Hang(d) => thread::sleep(d),
                    Reply::Http {
                        status,
                        headers,
                        body,
                    } => {
                        let mut out = format!("HTTP/1.1 {status} X\r\n");
                        for (n, v) in headers {
                            out.push_str(&format!("{n}: {v}\r\n"));
                        }
                        out.push_str(&format!(
                            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ));
                        let _ = stream.write_all(out.as_bytes());
                    }
                }
            }
        });
        FakeServer {
            base_url: format!("http://127.0.0.1:{port}/v1/"),
            captured,
        }
    }

    fn requests(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }
}

/// 호출할 때마다 정해 둔 시각을 차례로 돌려주고, 끝나면 마지막 시각을 반복한다.
struct SequenceClock(Mutex<VecDeque<Timestamp>>);

impl Clock for SequenceClock {
    fn now(&self) -> Timestamp {
        let mut queue = self.0.lock().unwrap();
        if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            *queue.front().unwrap()
        }
    }
}

fn clock(offsets_ms: &[i64]) -> Arc<dyn Clock> {
    let start: Timestamp = "2026-10-19T00:00:00Z".parse().unwrap();
    Arc::new(SequenceClock(Mutex::new(
        offsets_ms
            .iter()
            .map(|ms| Timestamp::from_millisecond(start.as_millisecond() + ms).unwrap())
            .collect(),
    )))
}

fn generator_with(
    server: &FakeServer,
    clock: Arc<dyn Clock>,
    read_timeout: Duration,
) -> OpenAiResponsesReviewGenerator {
    OpenAiResponsesReviewGenerator::new(
        &OpenAiSettings {
            api_key: API_KEY.to_owned(),
            base_url: server.base_url.clone(),
            connect_timeout: Duration::from_secs(2),
            read_timeout,
            allowed_models: vec!["gpt-6.1-sol".into(), "gpt-6-luna".into()],
        },
        clock,
    )
    .unwrap()
}

fn generator(server: &FakeServer) -> OpenAiResponsesReviewGenerator {
    generator_with(server, clock(&[0, 1234]), Duration::from_secs(60))
}

fn request(
    detail: ImageDetail,
    max_tokens: i32,
    effort: Option<ReasoningEffort>,
    threshold: Option<f64>,
) -> AiReviewGenerationRequest {
    let draft = AiPromptDraft::new(
        AiPromptType::PhotoReview,
        "사진을 평가한다.",
        "주제: {{topicTitle}}, 다시 {{topicTitle}}",
        "gpt-5-mini",
        AiPromptParameters::new(Some(detail), Some(max_tokens), effort, threshold).unwrap(),
    )
    .unwrap();
    AiReviewGenerationRequest::new(draft, IMAGE_URL, "노을").unwrap()
}

fn default_request() -> AiReviewGenerationRequest {
    request(ImageDetail::Auto, 500, None, None)
}

fn response_body(status: &str, output: Value) -> String {
    json!({
        "id": "resp_123",
        "status": status,
        "output": output,
        "usage": {
            "input_tokens": 1200,
            "input_tokens_details": {"cached_tokens": 300},
            "output_tokens": 85,
            "output_tokens_details": {"reasoning_tokens": 40}
        }
    })
    .to_string()
}

fn success_body(output_text: &str) -> String {
    response_body(
        "completed",
        json!([{"type": "message", "content": [{"type": "output_text", "text": output_text}]}]),
    )
}

fn run(replies: Vec<Reply>) -> Result<AiReviewGeneration, AiReviewGenerationError> {
    let server = FakeServer::start(replies);
    generator(&server).generate(&default_request())
}

fn failure(replies: Vec<Reply>) -> AiReviewGenerationError {
    run(replies).unwrap_err()
}

fn assert_failure(error: &AiReviewGenerationError, expected: FailureType, retryable: bool) {
    assert_eq!(error.failure_type, expected);
    assert_eq!(error.failure_type.is_retryable(), retryable);
}

// ---- 요청 ----

#[test]
fn generate_요청을_보내면_responses_계약대로_본문과_인증_헤더를_구성한다() {
    let server = FakeServer::start(vec![http(200, &success_body(&ok_output()))]);
    generator(&server)
        .generate(&request(ImageDetail::Low, 700, None, None))
        .unwrap();
    let captured = &server.requests()[0];
    assert_eq!(captured.request_line, "POST /v1/responses HTTP/1.1");
    assert_eq!(
        captured.header("Authorization"),
        Some("Bearer sk-test-secret")
    );
    assert_eq!(captured.header("Content-Type"), Some("application/json"));
    let expected = json!({
        "model": "gpt-5-mini",
        "instructions": "사진을 평가한다.",
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": "주제: 노을, 다시 노을"},
            {"type": "input_image", "image_url": "https://cdn.test/photo.jpg", "detail": "low"}
        ]}],
        "text": {"format": {
            "type": "json_schema",
            "name": "photo_review_v2",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["composition", "theme_expression", "feedback"],
                "properties": {
                    "composition": {"type": "number", "minimum": 0, "maximum": 10},
                    "theme_expression": {"type": "number", "minimum": 0, "maximum": 10},
                    "feedback": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["summary", "strength", "weakness", "suggestion"],
                        "properties": {
                            "summary": {"type": "string"},
                            "strength": {"type": "string"},
                            "weakness": {"type": "string"},
                            "suggestion": {"type": "string"}
                        }
                    }
                }
            }
        }},
        "store": false,
        "max_output_tokens": 700
    });
    assert_eq!(captured.json(), expected);
    // 필드 순서는 원본 레코드 순서다.
    assert!(
        captured
            .body
            .starts_with(r#"{"model":"gpt-5-mini","instructions":"#)
    );
    assert!(
        captured
            .body
            .ends_with(r#""store":false,"max_output_tokens":700}"#)
    );
}

#[test]
fn generate_reasoning_effort가_있으면_reasoning을_보내고_없으면_생략한다() {
    let server = FakeServer::start(vec![
        http(200, &success_body(&ok_output())),
        http(200, &success_body(&ok_output())),
    ]);
    let g = generator_with(&server, clock(&[0]), Duration::from_secs(60));
    g.generate(&request(ImageDetail::Auto, 500, None, None))
        .unwrap();
    g.generate(&request(
        ImageDetail::Auto,
        500,
        Some(ReasoningEffort::Medium),
        None,
    ))
    .unwrap();
    let requests = server.requests();
    assert!(requests[0].json().get("reasoning").is_none());
    assert_eq!(requests[1].json()["reasoning"]["effort"], "medium");
}

#[test]
fn generate_이미지_상세도는_소문자로_보낸다() {
    let server = FakeServer::start(vec![http(200, &success_body(&ok_output()))]);
    generator(&server)
        .generate(&request(ImageDetail::High, 500, None, None))
        .unwrap();
    assert_eq!(
        server.requests()[0].json()["input"][0]["content"][1]["detail"],
        "high"
    );
}

// ---- 성공 ----

#[test]
fn generate_성공_응답이면_결과와_토큰_사용량_응답_id_지연시간을_반환한다() {
    let generation = run(vec![http(200, &success_body(&ok_output()))]).unwrap();
    assert_eq!(generation.result.overall_score, 8.7);
    assert_eq!(
        generation.result.criteria,
        vec![Criterion {
            key: "COMPOSITION".into(),
            label: "구도".into(),
            score: 8.7
        }]
    );
    assert!(generation.result.distinctive_perspective);
    let feedback = Feedback {
        summary: "구도가 좋습니다.".into(),
        strength: "빛이 좋습니다.".into(),
        weakness: "수평이 아쉽습니다.".into(),
        suggestion: "낮게 찍어 보세요.".into(),
    };
    assert_eq!(generation.result.feedback, feedback);
    assert_eq!(generation.output.composition_score, 8.72);
    assert_eq!(generation.output.theme_expression_score, 8.0);
    assert_eq!(generation.output.feedback, feedback);
    assert_eq!(generation.raw_output, ok_output());
    assert_eq!(
        generation.attempt.usage,
        Usage {
            input_tokens: Some(1200),
            cached_input_tokens: Some(300),
            output_tokens: Some(85),
            reasoning_tokens: Some(40)
        }
    );
    assert_eq!(generation.attempt.response_id.as_deref(), Some("resp_123"));
    assert_eq!(generation.attempt.latency, Duration::from_millis(1234));
}

#[test]
fn generate_기본_기준_7에서_주제_표현_점수가_6이면_독특한_시각이_아니다() {
    let output = format!(r#"{{"composition":8,"theme_expression":6,"feedback":{FEEDBACK}}}"#);
    let generation = run(vec![http(200, &success_body(&output))]).unwrap();
    assert!(!generation.result.distinctive_perspective);
}

#[test]
fn generate_프롬프트에_기준_6을_지정하면_주제_표현_점수_6도_독특한_시각이다() {
    let output = format!(r#"{{"composition":8,"theme_expression":6,"feedback":{FEEDBACK}}}"#);
    let server = FakeServer::start(vec![http(200, &success_body(&output))]);
    let generation = generator(&server)
        .generate(&request(ImageDetail::Auto, 500, None, Some(6.0)))
        .unwrap();
    assert!(generation.result.distinctive_perspective);
}

#[test]
fn generate_소수_기준_7_5와_소수_주제_표현_점수를_그대로_비교한다() {
    for (theme, expected) in [("7.5", true), ("7.4", false), ("8.0", true)] {
        let output =
            format!(r#"{{"composition":8,"theme_expression":{theme},"feedback":{FEEDBACK}}}"#);
        let server = FakeServer::start(vec![http(200, &success_body(&output))]);
        let generation = generator(&server)
            .generate(&request(ImageDetail::Auto, 500, None, Some(7.5)))
            .unwrap();
        assert_eq!(
            generation.result.distinctive_perspective, expected,
            "{theme}"
        );
        assert_eq!(
            generation.output.theme_expression_score,
            theme.parse::<f64>().unwrap()
        );
    }
}

#[test]
fn supports_model_허용_목록의_모델만_정확히_일치해야_한다() {
    let server = FakeServer::start(vec![]);
    let g = generator(&server);
    assert!(g.supports_model("gpt-6.1-sol"));
    assert!(g.supports_model("gpt-6-luna"));
    assert!(!g.supports_model("gpt-6.1-SOL"));
    assert!(!g.supports_model("gpt-unknown"));
    assert!(!g.supports_model(""));
}

#[test]
fn generate_usage가_없어도_리뷰는_성공하고_토큰_수는_알_수_없음이다() {
    let body = json!({
        "id": "resp_123", "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": ok_output()}]}]
    })
    .to_string();
    let generation = run(vec![http(200, &body)]).unwrap();
    assert_eq!(generation.result.overall_score, 8.7);
    assert_eq!(generation.attempt.usage, Usage::default());
}

#[test]
fn generate_usage에_0이_오면_0_그대로_보고한다() {
    let body = json!({
        "id": "resp_123", "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": ok_output()}]}],
        "usage": {"input_tokens": 0, "output_tokens": 0}
    })
    .to_string();
    let generation = run(vec![http(200, &body)]).unwrap();
    assert_eq!(
        generation.attempt.usage,
        Usage {
            input_tokens: Some(0),
            cached_input_tokens: None,
            output_tokens: Some(0),
            reasoning_tokens: None
        }
    );
}

// ---- 200 응답의 실패 ----

#[test]
fn generate_거절과_출력_텍스트가_함께_오면_refused로_실패한다() {
    let body = response_body(
        "completed",
        json!([{"type": "message", "content": [
            {"type": "output_text", "text": ok_output()},
            {"type": "refusal", "refusal": "거절"}
        ]}]),
    );
    assert_failure(
        &failure(vec![http(200, &body)]),
        FailureType::Refused,
        false,
    );
}

#[test]
fn generate_출력_텍스트가_없으면_invalid_output으로_실패한다() {
    for output in [
        "[]",
        r#"[{"type":"message","content":[]}]"#,
        r#"[{"type":"reasoning"}]"#,
        r#"[{"type":"message","content":null}]"#,
    ] {
        let body = format!(r#"{{"id":"resp_1","status":"completed","output":{output}}}"#);
        assert_failure(
            &failure(vec![http(200, &body)]),
            FailureType::InvalidOutput,
            false,
        );
    }
}

#[test]
fn generate_모델이_거절하면_refused로_실패한다() {
    let body = response_body(
        "completed",
        json!([{"type": "message", "content": [{"type": "refusal", "refusal": "도와드릴 수 없습니다."}]}]),
    );
    assert_failure(
        &failure(vec![http(200, &body)]),
        FailureType::Refused,
        false,
    );
}

#[test]
fn generate_status가_incomplete이면_incomplete로_실패한다() {
    let body = r#"{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}"#;
    assert_failure(
        &failure(vec![http(200, body)]),
        FailureType::Incomplete,
        false,
    );
}

#[test]
fn generate_status가_completed가_아니면_upstream_error로_실패한다() {
    for status in ["in_progress", "queued", "cancelled"] {
        let body = format!(r#"{{"id":"resp_1","status":"{status}","output":[]}}"#);
        assert_failure(
            &failure(vec![http(200, &body)]),
            FailureType::UpstreamError,
            true,
        );
    }
    // status가 없어도 같다.
    assert_failure(
        &failure(vec![http(200, r#"{"id":"resp_1","output":[]}"#)]),
        FailureType::UpstreamError,
        true,
    );
}

#[test]
fn generate_status가_failed이면_error_code로_실패_유형을_가르고_시도_정보를_담는다() {
    for (code, expected, retryable) in [
        ("invalid_prompt", FailureType::BadRequest, false),
        ("data_residency_mismatch", FailureType::BadRequest, false),
        ("invalid_image", FailureType::BadRequest, false),
        ("invalid_image_format", FailureType::BadRequest, false),
        ("invalid_base64_image", FailureType::BadRequest, false),
        ("invalid_image_url", FailureType::BadRequest, false),
        ("image_too_large", FailureType::BadRequest, false),
        ("image_too_small", FailureType::BadRequest, false),
        ("image_parse_error", FailureType::BadRequest, false),
        ("invalid_image_mode", FailureType::BadRequest, false),
        ("image_file_too_large", FailureType::BadRequest, false),
        (
            "unsupported_image_media_type",
            FailureType::BadRequest,
            false,
        ),
        ("empty_image_file", FailureType::BadRequest, false),
        ("image_file_not_found", FailureType::BadRequest, false),
        (
            "image_content_policy_violation",
            FailureType::Refused,
            false,
        ),
        ("bio_policy", FailureType::Refused, false),
        ("misalignment_policy_violation", FailureType::Refused, false),
        ("rate_limit_exceeded", FailureType::RateLimited, true),
        ("server_error", FailureType::UpstreamError, true),
        ("vector_store_timeout", FailureType::UpstreamError, true),
        ("failed_to_download_image", FailureType::UpstreamError, true),
        ("brand_new_code", FailureType::UpstreamError, true),
    ] {
        let body = format!(
            r#"{{"id":"resp_f","status":"failed","output":[],"error":{{"code":"{code}","message":"secret detail"}}}}"#
        );
        let error = failure(vec![http(200, &body)]);
        assert_failure(&error, expected, retryable);
        assert!(!error.to_string().contains("secret detail"));
        assert_eq!(
            error.attempt.unwrap().response_id.as_deref(),
            Some("resp_f"),
            "{code}"
        );
    }
}

#[test]
fn generate_status가_failed인데_error_code가_없으면_재시도_가능한_upstream_error로_실패한다() {
    for error in [
        r#""error":null"#,
        r#""error":{}"#,
        r#""error":{"code":null,"message":"x"}"#,
    ] {
        let body = format!(r#"{{"id":"resp_f","status":"failed","output":[],{error}}}"#);
        let e = failure(vec![http(200, &body)]);
        assert_failure(&e, FailureType::UpstreamError, true);
        assert_eq!(e.attempt.unwrap().response_id.as_deref(), Some("resp_f"));
    }
}

#[test]
fn generate_본문이_json_null이거나_비면_invalid_output으로_실패하고_시도_정보는_없다() {
    for body in ["null", "", "   ", "not json", "[]"] {
        let e = failure(vec![http(200, body)]);
        assert_failure(&e, FailureType::InvalidOutput, false);
        assert_eq!(e.attempt, None, "{body:?}");
    }
}

#[test]
fn generate_output이나_content에_null_요소가_있으면_시도_정보를_담아_invalid_output으로_실패한다() {
    for output in ["[null]", r#"[{"type":"message","content":[null]}]"#] {
        let body = format!(r#"{{"id":"resp_x","status":"completed","output":{output}}}"#);
        let e = failure(vec![http(200, &body)]);
        assert_failure(&e, FailureType::InvalidOutput, false);
        assert_eq!(e.attempt.unwrap().response_id.as_deref(), Some("resp_x"));
    }
}

#[test]
fn generate_본문을_받은_뒤_실패하면_예외가_사용량_지연시간_응답_id를_담는다() {
    let body = r#"{"id":"resp_9","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":10},"output_tokens":16000,"output_tokens_details":{"reasoning_tokens":15900}}}"#;
    let e = failure(vec![http(200, body)]);
    assert_eq!(e.failure_type, FailureType::Incomplete);
    assert_eq!(
        e.attempt,
        Some(Attempt {
            usage: Usage {
                input_tokens: Some(1000),
                cached_input_tokens: Some(10),
                output_tokens: Some(16000),
                reasoning_tokens: Some(15900)
            },
            latency: Duration::from_millis(1234),
            response_id: Some("resp_9".into())
        })
    );
}

#[test]
fn generate_출력_검증에_실패해도_예외가_사용량을_담는다() {
    let e = failure(vec![http(200, &success_body("사진이 좋네요."))]);
    assert_eq!(e.failure_type, FailureType::InvalidOutput);
    let attempt = e.attempt.unwrap();
    assert_eq!(attempt.usage.input_tokens, Some(1200));
    assert_eq!(attempt.response_id.as_deref(), Some("resp_123"));
}

#[test]
fn generate_요약이_61자면_invalid_output으로_실패한다() {
    let output = format!(
        r#"{{"composition":8,"theme_expression":8,"feedback":{{"summary":"{}","strength":"좋음","weakness":"아쉬움","suggestion":"제안"}}}}"#,
        "가".repeat(61)
    );
    assert_failure(
        &failure(vec![http(200, &success_body(&output))]),
        FailureType::InvalidOutput,
        false,
    );
}

#[test]
fn generate_필드가_없거나_범위를_벗어나면_invalid_output으로_실패한다() {
    let feedback_missing_last = r#"{"summary":"요약","strength":"좋음","weakness":"아쉬움"}"#;
    let cases = [
        r#"{"composition":8,"theme_expression":8}"#.to_owned(),
        format!(r#"{{"theme_expression":8,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":8,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":11,"theme_expression":8,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":8,"theme_expression":11,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":8,"theme_expression":99999999999,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":8,"theme_expression":null,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":null,"theme_expression":8,"feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":8,"theme_expression":"7","feedback":{FEEDBACK}}}"#),
        format!(r#"{{"composition":"8","theme_expression":7,"feedback":{FEEDBACK}}}"#),
        r#"{"composition":8,"theme_expression":7,"feedback":"좋음"}"#.to_owned(),
        r#"{"composition":8,"theme_expression":7,"feedback":null}"#.to_owned(),
        r#"{"composition":8,"theme_expression":7,"feedback":[]}"#.to_owned(),
        format!(r#"{{"composition":8,"theme_expression":7,"feedback":{feedback_missing_last}}}"#),
        r#"{"composition":8,"theme_expression":7,"feedback":{"summary":"요약","strength":"좋음","weakness":"아쉬움","suggestion":3}}"#.to_owned(),
        r#"{"composition":8,"theme_expression":7,"feedback":{"summary":"요약","strength":"좋음","weakness":null,"suggestion":"제안"}}"#.to_owned(),
        r#"{"composition":8,"theme_expression":7,"feedback":{"summary":" ","strength":"좋음","weakness":"아쉬움","suggestion":"제안"}}"#.to_owned(),
    ];
    for output in cases {
        assert_failure(
            &failure(vec![http(200, &success_body(&output))]),
            FailureType::InvalidOutput,
            false,
        );
    }
}

// ---- HTTP 오류 ----

#[test]
fn generate_일시적_한도이거나_원인을_알_수_없는_429_응답이면_rate_limited로_실패하고_사용량은_없다()
{
    for body in [
        r#"{"error":{"message":"Rate limit reached for requests","type":"requests","code":"rate_limit_exceeded"}}"#,
        r#"{"error":{"message":"Slow down","type":"rate_limit_error","code":"slow_down"}}"#,
        r#"{"error":{"message":"unknown","type":"other","code":"new_code"}}"#,
        r#"{"error":{"message":"no code"}}"#,
        r#"{"error":null}"#,
        r#"{"error":"quota"}"#,
        "[]",
        "{}",
        "",
        "not json",
    ] {
        let e = failure(vec![http(429, body)]);
        assert_failure(&e, FailureType::RateLimited, true);
        assert_eq!(e.attempt, None, "{body}");
    }
}

#[test]
fn generate_결제_한도_소진_코드의_429_응답이면_quota_exceeded로_실패한다() {
    for code in [
        "insufficient_quota",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded",
    ] {
        let body = format!(
            r#"{{"error":{{"message":"secret billing detail","type":"billing","code":"{code}"}}}}"#
        );
        let e = failure(vec![http(429, &body)]);
        assert_failure(&e, FailureType::QuotaExceeded, false);
        assert!(!e.to_string().contains("secret billing detail"));
        assert_eq!(e.attempt, None);
    }
    // type만 결제 코드여도 같다.
    let e = failure(vec![http(
        429,
        r#"{"error":{"message":"quota","type":"insufficient_quota","code":null}}"#,
    )]);
    assert_failure(&e, FailureType::QuotaExceeded, false);
}

#[test]
fn generate_결제_코드가_429가_아닌_응답에_있으면_상태_코드_기준으로_분류한다() {
    let e = failure(vec![http(
        400,
        r#"{"error":{"type":"insufficient_quota","code":"insufficient_quota"}}"#,
    )]);
    assert_failure(&e, FailureType::BadRequest, false);
}

#[test]
fn generate_상태_코드별_분류() {
    for (status, expected, retryable) in [
        (400, FailureType::BadRequest, false),
        (401, FailureType::BadRequest, false),
        (403, FailureType::BadRequest, false),
        (404, FailureType::BadRequest, false),
        (408, FailureType::Timeout, true),
        (409, FailureType::UpstreamError, true),
        (429, FailureType::RateLimited, true),
        (500, FailureType::UpstreamError, true),
        (503, FailureType::UpstreamError, true),
        (302, FailureType::UpstreamError, true),
    ] {
        let e = failure(vec![Reply::Http {
            status,
            headers: vec![],
            body: String::new(),
        }]);
        assert_failure(&e, expected, retryable);
    }
}

#[test]
fn generate_400_응답이면_메시지에_본문을_담지_않는다() {
    let e = failure(vec![http(
        400,
        r#"{"error":{"message":"secret prompt text"}}"#,
    )]);
    assert_failure(&e, FailureType::BadRequest, false);
    assert!(!e.to_string().contains("secret prompt text"));
}

#[test]
fn generate_실패해도_예외_메시지에_api_키_프롬프트_이미지_주소를_담지_않는다() {
    let e = failure(vec![http(429, "")]);
    let message = e.to_string();
    for secret in [API_KEY, "사진을 평가한다.", IMAGE_URL] {
        assert!(!message.contains(secret));
    }
}

// ---- 전송 오류 ----

#[test]
fn generate_읽기_시간이_초과되면_재시도_가능한_timeout으로_실패한다() {
    let server = FakeServer::start(vec![Reply::Hang(Duration::from_millis(600))]);
    let g = generator_with(&server, clock(&[0]), Duration::from_millis(200));
    let e = g.generate(&default_request()).unwrap_err();
    assert_failure(&e, FailureType::Timeout, true);
    assert_eq!(e.attempt, None);
}

#[test]
fn generate_연결이_끊기면_재시도_가능한_upstream_error로_실패한다() {
    let server = FakeServer::start(vec![Reply::Close]);
    let e = generator(&server).generate(&default_request()).unwrap_err();
    assert_failure(&e, FailureType::UpstreamError, true);
}

#[test]
fn generate_연결_거절이면_upstream_error이고_제한_시간_이상_지난_뒤면_timeout이다() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let settings = |read_timeout| OpenAiSettings {
        api_key: API_KEY.to_owned(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        connect_timeout: Duration::from_secs(1),
        read_timeout,
        allowed_models: vec!["m".into()],
    };
    let before =
        OpenAiResponsesReviewGenerator::new(&settings(Duration::from_secs(60)), clock(&[0, 1000]))
            .unwrap();
    let e = before.generate(&default_request()).unwrap_err();
    assert_failure(&e, FailureType::UpstreamError, true);
    // 경과 시간이 읽기 제한 이상이면 시간 초과로 본다(`generate_연결 오류가 제한 시간 이상 지난 뒤 나면`).
    let after = OpenAiResponsesReviewGenerator::new(
        &settings(Duration::from_secs(60)),
        clock(&[0, 61_000]),
    )
    .unwrap();
    let e = after.generate(&default_request()).unwrap_err();
    assert_failure(&e, FailureType::Timeout, true);
}

// ---- 재시도 대기 힌트 ----

fn rate_limited(headers: &[(&'static str, &str)]) -> AiReviewGenerationError {
    failure(vec![Reply::Http {
        status: 429,
        headers: headers.iter().map(|(n, v)| (*n, (*v).to_owned())).collect(),
        body: r#"{"error":{"code":"rate_limit_exceeded"}}"#.to_owned(),
    }])
}

#[test]
fn generate_429_응답에_retry_after_ms와_retry_after가_함께_있으면_밀리초_값을_싣는다() {
    let e = rate_limited(&[("retry-after", "30"), ("retry-after-ms", "60000")]);
    assert_eq!(e.failure_type, FailureType::RateLimited);
    assert_eq!(e.retry_after, Some(Duration::from_secs(60)));
}

#[test]
fn generate_429_응답에_retry_after만_있으면_초_단위로_읽는다() {
    assert_eq!(
        rate_limited(&[("retry-after", "60")]).retry_after,
        Some(Duration::from_secs(60))
    );
}

#[test]
fn generate_retry_after_ms를_읽을_수_없으면_retry_after로_대신한다() {
    let e = rate_limited(&[("retry-after-ms", "soon"), ("retry-after", "45")]);
    assert_eq!(e.retry_after, Some(Duration::from_secs(45)));
}

#[test]
fn generate_대기_시간_헤더가_없거나_읽을_수_없으면_힌트는_비어_있다() {
    assert_eq!(rate_limited(&[]).retry_after, None);
    for value in ["abc", "", "-1", "1.5", "Wed, 21 Oct 2026 07:28:00 GMT"] {
        assert_eq!(
            rate_limited(&[("retry-after", value)]).retry_after,
            None,
            "{value}"
        );
    }
    assert_eq!(rate_limited(&[("retry-after-ms", "-5")]).retry_after, None);
}

#[test]
fn generate_5xx_응답에_retry_after가_있으면_힌트로_싣는다() {
    let e = failure(vec![Reply::Http {
        status: 503,
        headers: vec![("retry-after", "20".to_owned())],
        body: String::new(),
    }]);
    assert_eq!(e.failure_type, FailureType::UpstreamError);
    assert_eq!(e.retry_after, Some(Duration::from_secs(20)));
}

#[test]
fn generate_결제_한도_소진_429_응답이면_헤더가_있어도_힌트는_비어_있다() {
    let e = failure(vec![Reply::Http {
        status: 429,
        headers: vec![("retry-after", "60".to_owned())],
        body: r#"{"error":{"code":"insufficient_quota"}}"#.to_owned(),
    }]);
    assert_eq!(e.failure_type, FailureType::QuotaExceeded);
    assert_eq!(e.retry_after, None);
}

#[test]
fn generate_시간_초과나_status가_failed인_200_응답이면_힌트는_비어_있다() {
    let server = FakeServer::start(vec![Reply::Hang(Duration::from_millis(600))]);
    let e = generator_with(&server, clock(&[0]), Duration::from_millis(200))
        .generate(&default_request())
        .unwrap_err();
    assert_eq!(
        (e.failure_type, e.retry_after),
        (FailureType::Timeout, None)
    );
    let body =
        r#"{"id":"resp_1","status":"failed","error":{"code":"rate_limit_exceeded"},"output":[]}"#;
    let e = failure(vec![http(200, body)]);
    assert_eq!(
        (e.failure_type, e.retry_after),
        (FailureType::RateLimited, None)
    );
}

// ---- 설정 ----

#[test]
fn 설정은_api_키가_필수이고_기본값은_원본_application_yml과_같다() {
    use chalkak_core::config::Env;
    assert!(OpenAiSettings::from_env(&Env::from_pairs([("X", "y")])).is_err());
    assert!(OpenAiSettings::from_env(&Env::from_pairs([("OPENAI_API_KEY", "  ")])).is_err());
    let s = OpenAiSettings::from_env(&Env::from_pairs([("OPENAI_API_KEY", "sk")])).unwrap();
    assert_eq!(s.base_url, "https://api.openai.com/v1");
    assert_eq!(s.connect_timeout, Duration::from_secs(5));
    assert_eq!(s.read_timeout, Duration::from_secs(60));
    assert_eq!(s.allowed_models, ["gpt-6.1-sol", "gpt-6-luna"]);
    // 로그에 설정이 찍혀도 키는 나오지 않는다(OpenAiPropertiesTest).
    assert!(!format!("{s:?}").contains("sk\""));
    assert!(!format!("{s:?}").contains("api_key"));
}

#[test]
fn 설정은_환경변수로_덮어쓰고_잘못된_값은_거절한다() {
    use chalkak_core::config::Env;
    let s = OpenAiSettings::from_env(&Env::from_pairs([
        ("OPENAI_API_KEY", "sk"),
        (
            "CHALKAK_AIREVIEW_OPENAI_BASEURL",
            "http://fakes:9000/openai/v1",
        ),
        ("CHALKAK_AIREVIEW_OPENAI_READTIMEOUT", "30s"),
        ("CHALKAK_AIREVIEW_OPENAI_ALLOWEDMODELS", "a, b"),
    ]))
    .unwrap();
    assert_eq!(s.base_url, "http://fakes:9000/openai/v1");
    assert_eq!(s.read_timeout, Duration::from_secs(30));
    assert_eq!(s.allowed_models, ["a", "b"]);
    for (key, value) in [
        ("CHALKAK_AIREVIEW_OPENAI_CONNECTTIMEOUT", "0s"),
        ("CHALKAK_AIREVIEW_OPENAI_READTIMEOUT", "0s"),
        ("CHALKAK_AIREVIEW_OPENAI_ALLOWEDMODELS", "a,,b"),
        ("CHALKAK_AIREVIEW_OPENAI_ALLOWEDMODELS", " "),
    ] {
        let env = Env::from_pairs([("OPENAI_API_KEY", "sk"), (key, value)]);
        assert!(OpenAiSettings::from_env(&env).is_err(), "{key}={value}");
    }
}
