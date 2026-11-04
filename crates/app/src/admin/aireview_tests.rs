//! 관리자 AI 프롬프트·리뷰 API를 라우터까지 통과시켜 보는 테스트.
//!
//! 원본 `AdminAiPromptVersionControllerTest`, `AdminAiPromptPreviewControllerTest`, `AdminAiReviewControllerTest`,
//! `AdminAiPromptVersionServiceTest`, `AdminAiPromptPreviewServiceTest`, `AdminAiReviewQueryRepositoryTest`가 확인하던 것을
//! 목 대신 실제 DB로 끝까지 돌려 확인한다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_http::{Handler, Method, Request, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use super::aireview::{minify_json, register};
use super::testing::{deps, insert_admin, insert_post, insert_topic, insert_user};
use crate::aireview::domain::{Feedback, PhotoReviewOutput};
use crate::aireview::generator::{
    AiReviewGeneration, AiReviewGenerationError, AiReviewGenerator, Attempt, FailureType, Usage,
};
use crate::aireview::testing::StubGenerator;
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::Routes;

const PV: &str = "/api/v1/admin/ai-prompt-versions";
const PREVIEW: &str = "/api/v1/admin/ai-prompt-previews";
const REVIEWS: &str = "/api/v1/admin/ai-reviews";

struct Api {
    router: Router,
    admin: Uuid,
    db: TestDb,
    stub: Arc<StubGenerator>,
}

fn api_with(outcomes: Vec<Result<AiReviewGeneration, AiReviewGenerationError>>) -> Option<Api> {
    let db = TestDb::create()?;
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let stub = Arc::new(StubGenerator::new(&["gpt-6.1-sol", "gpt-6-luna"], outcomes));
    let generator: Arc<dyn AiReviewGenerator> = stub.clone();
    let routes = register(Routes::new(deps.logger.clone()), &deps, generator);
    Some(Api {
        router: routes.into_router(),
        admin,
        db,
        stub,
    })
}

fn api() -> Option<Api> {
    api_with(Vec::new())
}

struct Reply {
    status: u16,
    body: Value,
    text: String,
}

impl Api {
    fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let _guard = security::enter(Principal {
            subject: self.admin.hyphenated().to_string(),
            authorities: vec![AUTHORITY_ADMIN.to_owned()],
        });
        let mut req = Request::new(method, path);
        req.query = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        if let Some(body) = body {
            req.headers.append("Content-Type", "application/json");
            req.body = body.to_string().into_bytes();
        }
        let response = self.router.handle(req);
        let text = String::from_utf8_lossy(&response.body).into_owned();
        Reply {
            status: response.status,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        }
    }

    fn get(&self, path: &str, query: &[(&str, &str)]) -> Reply {
        self.call(Method::Get, path, query, None)
    }

    fn post(&self, path: &str, body: Value) -> Reply {
        self.call(Method::Post, path, &[], Some(body))
    }

    fn create_version(&self, extra: Value) -> Reply {
        let mut body = valid_version();
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.post(PV, body)
    }
}

fn valid_version() -> Value {
    json!({
        "type": "PHOTO_REVIEW",
        "systemPrompt": "시스템",
        "userPromptTemplate": "{{topicTitle}}",
        "model": "gpt-6.1-sol"
    })
}

fn message(reply: &Reply) -> &str {
    reply.body["message"].as_str().unwrap_or_default()
}

fn id() -> String {
    Uuid::new_v4().hyphenated().to_string()
}

fn feedback() -> Feedback {
    Feedback {
        summary: "구도가 좋습니다.".to_owned(),
        strength: "빛이 좋습니다.".to_owned(),
        weakness: "수평이 아쉽습니다.".to_owned(),
        suggestion: "낮게 찍어 보세요.".to_owned(),
    }
}

fn success() -> AiReviewGeneration {
    let output = PhotoReviewOutput::new(8.72, 8.0, feedback()).unwrap();
    AiReviewGeneration {
        result: output.to_result(7.0),
        output,
        raw_output: "{}".to_owned(),
        attempt: Attempt {
            usage: Usage {
                input_tokens: Some(1200),
                cached_input_tokens: Some(300),
                output_tokens: Some(85),
                reasoning_tokens: Some(40),
            },
            latency: Duration::from_millis(1234),
            response_id: Some("resp_1".to_owned()),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// 프롬프트 버전
// ---------------------------------------------------------------------------------------------

// AdminAiPromptVersionControllerTest.createVersion_valid_returnsCreated, withoutOptionalFields_isAccepted
#[test]
fn 버전을_만들면_201과_기본_옵션을_돌려주고_번호는_1부터_늘어난다() {
    let Some(api) = api() else { return };
    let first = api.create_version(json!({"memo": "  메모  "}));
    assert_eq!(first.status, 201, "{}", first.text);
    assert_eq!(first.body["type"], "PHOTO_REVIEW");
    assert_eq!(first.body["version"], 1);
    assert_eq!(first.body["systemPrompt"], "시스템");
    assert_eq!(first.body["userPromptTemplate"], "{{topicTitle}}");
    assert_eq!(first.body["model"], "gpt-6.1-sol");
    assert_eq!(first.body["outputSchemaVersion"], "2");
    assert_eq!(first.body["memo"], "메모");
    assert_eq!(
        first.body["createdByAdminId"],
        api.admin.hyphenated().to_string()
    );
    assert_eq!(first.body["createdAt"], "2026-10-12T01:00:00Z");
    assert!(first.text.contains(
        "\"parameters\":{\"imageDetail\":\"HIGH\",\"maxOutputTokens\":8000,\"reasoningEffort\":null,\"distinctivePerspectiveThreshold\":7.0}"
    ));
    let second = api.create_version(json!({}));
    assert_eq!(second.body["version"], 2);
    assert_eq!(second.body["memo"], Value::Null);
}

// AdminAiPromptVersionControllerTest.createVersion_validRequest_passesCommandToService
#[test]
fn 옵션을_모두_주면_그대로_저장한다() {
    let Some(api) = api() else { return };
    let reply = api.create_version(json!({"parameters": {
        "imageDetail": "LOW", "maxOutputTokens": 700, "reasoningEffort": "HIGH",
        "distinctivePerspectiveThreshold": 6.5
    }}));
    assert_eq!(reply.status, 201, "{}", reply.text);
    assert_eq!(reply.body["parameters"]["imageDetail"], "LOW");
    assert_eq!(reply.body["parameters"]["maxOutputTokens"], 700);
    assert_eq!(reply.body["parameters"]["reasoningEffort"], "HIGH");
    assert_eq!(
        reply.body["parameters"]["distinctivePerspectiveThreshold"],
        6.5
    );
    let stored: String = api
        .db
        .client()
        .query_one("SELECT parameters::text FROM ai_prompt_versions", &[])
        .unwrap()
        .get(0);
    assert!(stored.contains("\"maxOutputTokens\": 700"), "{stored}");
}

// AdminAiPromptVersionServiceTest: 감사 로그를 같은 트랜잭션에 남긴다
#[test]
fn 버전을_만들면_감사_로그를_남긴다() {
    let Some(api) = api() else { return };
    let reply = api.create_version(json!({}));
    let row = api
        .db
        .client()
        .query_one(
            "SELECT action::text, target_type::text, target_id, before_state::text, after_state::text \
             FROM admin_audit_logs",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "AI_PROMPT_VERSION_CREATED");
    assert_eq!(row.get::<_, String>(1), "AI_PROMPT_VERSION");
    assert_eq!(
        row.get::<_, Uuid>(2).hyphenated().to_string(),
        reply.body["promptVersionId"].as_str().unwrap()
    );
    assert_eq!(row.get::<_, String>(3), "{}");
    let after: Value = serde_json::from_str(&row.get::<_, String>(4)).unwrap();
    assert_eq!(after, json!({"type": "PHOTO_REVIEW", "version": 1}));
}

// AdminAiPromptVersionControllerTest.createVersion_serviceBusinessException_returnsBadRequest
#[test]
fn 허용되지_않은_모델은_400이고_저장하지_않는다() {
    let Some(api) = api() else { return };
    let reply = api.create_version(json!({"model": "gpt-unknown"}));
    assert_eq!(reply.status, 400);
    assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    assert_eq!(message(&reply), "허용되지 않은 모델입니다.");
    let count: i64 = api
        .db
        .client()
        .query_one("SELECT count(*) FROM ai_prompt_versions", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

// AdminAiPromptVersionControllerTest.createVersion_missingRequiredField / tooLong / unknownEnumValue
#[test]
fn 필수_값과_길이_열거형을_검사한다() {
    let Some(api) = api() else { return };
    let without = |field: &str| {
        let mut body = valid_version();
        body.as_object_mut().unwrap().remove(field);
        body
    };
    let missing_type = api.post(PV, without("type"));
    assert_eq!(
        (missing_type.status, message(&missing_type)),
        (400, "must not be null")
    );
    for field in ["systemPrompt", "userPromptTemplate", "model"] {
        let reply = api.post(PV, without(field));
        assert_eq!(
            (reply.status, message(&reply)),
            (400, "must not be blank"),
            "{field}"
        );
        let blank = api.create_version(json!({ field: "  " }));
        assert_eq!(
            (blank.status, message(&blank)),
            (400, "must not be blank"),
            "{field}"
        );
    }
    let long_system = api.create_version(json!({"systemPrompt": "가".repeat(50_001)}));
    assert_eq!(long_system.status, 400);
    assert_eq!(message(&long_system), "size must be between 0 and 50000");
    let long_template = api.create_version(json!({"userPromptTemplate": "a".repeat(2001)}));
    assert_eq!(message(&long_template), "size must be between 0 and 2000");
    let long_model = api.create_version(json!({"model": "m".repeat(101)}));
    assert_eq!(message(&long_model), "size must be between 0 and 100");
    for bad in [
        json!({"type": "UNKNOWN"}),
        json!({"parameters": {"imageDetail": "MEDIUM"}}),
        json!({"parameters": {"reasoningEffort": "NONE"}}),
        json!({"parameters": "x"}),
        json!({"parameters": {"maxOutputTokens": "abc"}}),
    ] {
        let reply = api.create_version(bad.clone());
        assert_eq!(reply.status, 400, "{bad}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    }
    // 길이 상한 값은 받는다.
    let max = api.create_version(json!({
        "systemPrompt": "가".repeat(50_000), "userPromptTemplate": "{{topicTitle}}".repeat(100)[..1300].to_owned() + "x",
        "model": "gpt-6.1-sol"
    }));
    assert_eq!(max.status, 201, "{}", max.text);
}

// AdminAiPromptVersionControllerTest.createVersion_memoJudgedByDomain_reachesService, AiPromptParametersTest
#[test]
fn 도메인_규칙을_어기면_400이다() {
    let Some(api) = api() else { return };
    let tokens = api.create_version(json!({"parameters": {"maxOutputTokens": 16001}}));
    assert_eq!(
        message(&tokens),
        "최대 출력 토큰은 1 이상 16000 이하여야 합니다."
    );
    let threshold =
        api.create_version(json!({"parameters": {"distinctivePerspectiveThreshold": 10.1}}));
    assert_eq!(
        message(&threshold),
        "독특한 시각 판정 기준은 0 이상 10 이하여야 합니다."
    );
    let variable = api.create_version(json!({"userPromptTemplate": "{{unknown}}"}));
    assert_eq!(variable.status, 400);
    let memo = api.create_version(json!({"memo": "가".repeat(501)}));
    assert_eq!(message(&memo), "메모는 500자 이하여야 합니다.");
    // 공백뿐인 메모는 저장하지 않는다.
    let blank = api.create_version(json!({"memo": "   "}));
    assert_eq!(blank.status, 201);
    assert_eq!(blank.body["memo"], Value::Null);
    // 소수 maxOutputTokens(700.9)는 Jackson처럼 버린다.
    let fraction = api.create_version(json!({"parameters": {"maxOutputTokens": 700.9}}));
    assert_eq!(fraction.body["parameters"]["maxOutputTokens"], 700);
}

// AdminAiPromptVersionControllerTest.getVersions_*
#[test]
fn 목록은_큰_번호부터_주고_본문은_빼며_페이지를_나눈다() {
    let Some(api) = api() else { return };
    for _ in 0..3 {
        api.create_version(json!({"memo": "메모"}));
    }
    let list = api.get(PV, &[("type", "PHOTO_REVIEW"), ("pageSize", "2")]);
    assert_eq!(list.status, 200, "{}", list.text);
    assert_eq!(list.body["currentPage"], 1);
    assert_eq!(list.body["pageSize"], 2);
    assert_eq!(list.body["hasNext"], true);
    let items = list.body["promptVersions"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["version"], 3);
    assert_eq!(items[1]["version"], 2);
    assert_eq!(items[0]["model"], "gpt-6.1-sol");
    assert_eq!(items[0]["memo"], "메모");
    assert!(items[0].get("systemPrompt").is_none());
    assert!(items[0].get("userPromptTemplate").is_none());
    assert!(items[0].get("parameters").is_none());
    let second = api.get(
        PV,
        &[("type", "PHOTO_REVIEW"), ("pageSize", "2"), ("page", "2")],
    );
    assert_eq!(second.body["hasNext"], false);
    assert_eq!(second.body["promptVersions"].as_array().unwrap().len(), 1);
    let empty = api.get(
        PV,
        &[("type", "PHOTO_REVIEW"), ("page", "3"), ("pageSize", "100")],
    );
    assert_eq!(empty.body["promptVersions"], json!([]));
}

// AdminAiPromptVersionControllerTest.getVersions_invalidParameterType / blankOrUnknownType / noTypeParameter / invalidPagination
#[test]
fn 목록_조회_조건이_틀리면_400이다() {
    let Some(api) = api() else { return };
    for query in [
        vec![("type", "PHOTO_REVIEW"), ("page", "x")],
        vec![("type", "PHOTO_REVIEW"), ("pageSize", "x")],
        vec![("type", "UNKNOWN")],
        vec![("type", "")],
        vec![("type", " ")],
        vec![],
    ] {
        let reply = api.get(PV, &query);
        assert_eq!(reply.status, 400, "{query:?}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    }
    assert_eq!(
        message(&api.get(PV, &[("type", "UNKNOWN")])),
        "type: 요청 값의 형식이 올바르지 않습니다."
    );
    for (name, value) in [
        ("page", "0"),
        ("page", "-1"),
        ("pageSize", "0"),
        ("pageSize", "101"),
    ] {
        let reply = api.get(PV, &[("type", "PHOTO_REVIEW"), (name, value)]);
        assert_eq!(reply.status, 400, "{name}={value}");
        assert_eq!(message(&reply), "조회 조건이 올바르지 않습니다.");
    }
}

// AdminAiPromptVersionControllerTest.getVersion_*
#[test]
fn 상세는_전체_필드를_주고_없으면_404_잘못된_id는_400이다() {
    let Some(api) = api() else { return };
    let created = api.create_version(json!({}));
    let version_id = created.body["promptVersionId"].as_str().unwrap().to_owned();
    let detail = api.get(&format!("{PV}/{version_id}"), &[]);
    assert_eq!(detail.status, 200);
    assert_eq!(detail.text, created.text);
    let unknown = api.get(&format!("{PV}/{}", id()), &[]);
    assert_eq!(unknown.status, 404);
    assert_eq!(message(&unknown), "프롬프트 버전을 찾을 수 없습니다.");
    for bad in ["not-a-uuid", "12345678-1234-1234-1234-12345678901"] {
        let reply = api.get(&format!("{PV}/{bad}"), &[]);
        assert_eq!(reply.status, 400, "{bad}");
        assert_eq!(message(&reply), "ID 형식이 올바르지 않습니다.");
    }
}

// ---------------------------------------------------------------------------------------------
// 미리보기
// ---------------------------------------------------------------------------------------------

struct Seed {
    user: Uuid,
    topic: Uuid,
    post: Uuid,
}

fn seed_post(api: &Api, status: &str) -> Seed {
    let mut client = api.db.client();
    let user = insert_user(&mut client, &format!("{}@example.com", id()), "ACTIVE");
    static NEXT_DAY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT_DAY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let date = format!("2027-{:02}-{:02}", 1 + n / 28, 1 + n % 28);
    let topic = insert_topic(
        &mut client,
        "노을",
        &date,
        &format!("{date}T00:00:00Z"),
        &format!("{date}T12:00:00Z"),
    );
    let post = insert_post(&mut client, user, topic, status);
    Seed { user, topic, post }
}

fn preview_body(post: &str) -> Value {
    let mut body = valid_version();
    body["postId"] = json!(post);
    body
}

// AdminAiPromptPreviewControllerTest.preview_succeeded_serializesOutcome, AdminAiPromptPreviewServiceTest
#[test]
fn 미리보기는_성공_결과와_사용량을_돌려주고_아무것도_저장하지_않는다() {
    let Some(api) = api_with(vec![Ok(success())]) else {
        return;
    };
    let seed = seed_post(&api, "APPROVED");
    let reply = api.post(PREVIEW, preview_body(&seed.post.hyphenated().to_string()));
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(
        reply.text,
        "{\"succeeded\":true,\"failureType\":null,\"retryable\":null,\
         \"result\":{\"overallScore\":8.7,\"criteria\":[{\"key\":\"COMPOSITION\",\"label\":\"구도\",\"score\":8.7}],\
         \"distinctivePerspective\":true,\"feedback\":{\"summary\":\"구도가 좋습니다.\",\"strength\":\"빛이 좋습니다.\",\
         \"weakness\":\"수평이 아쉽습니다.\",\"suggestion\":\"낮게 찍어 보세요.\"}},\
         \"output\":{\"compositionScore\":8.72,\"themeExpressionScore\":8.0},\
         \"usage\":{\"inputTokens\":1200,\"cachedInputTokens\":300,\"outputTokens\":85,\"reasoningTokens\":40},\
         \"latencyMs\":1234,\"responseId\":\"resp_1\"}"
    );
    let requests = api.stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].topic_title, "노을");
    assert!(
        requests[0]
            .image_url
            .starts_with("https://cdn.test.chalkak/"),
        "{}",
        requests[0].image_url
    );
    assert!(
        requests[0].image_url.ends_with("/original.webp"),
        "{}",
        requests[0].image_url
    );
    for table in ["ai_prompt_versions", "ai_reviews", "admin_audit_logs"] {
        let count: i64 = api
            .db
            .client()
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .unwrap()
            .get(0);
        assert_eq!(count, 0, "{table}");
    }
}

// AdminAiPromptPreviewControllerTest.preview_failed_serializesFailureOutcome
#[test]
fn 미리보기의_생성_실패는_200과_실패_유형이다() {
    let Some(api) = api_with(vec![
        Err(AiReviewGenerationError::of(FailureType::Timeout)),
        Err(AiReviewGenerationError::with_attempt(
            FailureType::InvalidOutput,
            Attempt {
                usage: Usage {
                    input_tokens: Some(10),
                    ..Usage::default()
                },
                latency: Duration::from_millis(77),
                response_id: Some("resp_bad".to_owned()),
            },
        )),
    ]) else {
        return;
    };
    let seed = seed_post(&api, "APPROVED");
    let post = seed.post.hyphenated().to_string();
    let timeout = api.post(PREVIEW, preview_body(&post));
    assert_eq!(timeout.status, 200);
    assert_eq!(
        timeout.text,
        "{\"succeeded\":false,\"failureType\":\"TIMEOUT\",\"retryable\":true,\"result\":null,\
         \"output\":null,\"usage\":null,\"latencyMs\":null,\"responseId\":null}"
    );
    let invalid = api.post(PREVIEW, preview_body(&post));
    assert_eq!(
        invalid.text,
        "{\"succeeded\":false,\"failureType\":\"INVALID_OUTPUT\",\"retryable\":false,\"result\":null,\
         \"output\":null,\"usage\":{\"inputTokens\":10,\"cachedInputTokens\":null,\"outputTokens\":null,\
         \"reasoningTokens\":null},\"latencyMs\":77,\"responseId\":\"resp_bad\"}"
    );
}

// AdminAiPromptPreviewServiceTest: 거절한 요청은 OpenAI를 부르지 않는다
#[test]
fn 미리보기는_거절하면_생성기를_부르지_않는다() {
    let Some(api) = api_with(vec![Ok(success())]) else {
        return;
    };
    let seed = seed_post(&api, "APPROVED");
    let pending = seed_post(&api, "PENDING");
    let post = seed.post.hyphenated().to_string();

    let mut model = preview_body(&post);
    model["model"] = json!("gpt-unknown");
    let reply = api.post(PREVIEW, model);
    assert_eq!(
        (reply.status, message(&reply)),
        (400, "허용되지 않은 모델입니다.")
    );

    let unknown = api.post(PREVIEW, preview_body(&id()));
    assert_eq!(
        (unknown.status, message(&unknown)),
        (404, "게시물을 찾을 수 없습니다.")
    );
    let hidden = api.post(
        PREVIEW,
        preview_body(&pending.post.hyphenated().to_string()),
    );
    assert_eq!(hidden.status, 404);

    api.db
        .client()
        .execute(
            "UPDATE posts SET deleted_at = now() WHERE id = $1",
            &[&seed.post],
        )
        .unwrap();
    assert_eq!(api.post(PREVIEW, preview_body(&post)).status, 404);

    let bad_id = api.post(PREVIEW, preview_body("not-a-uuid"));
    assert_eq!(
        (bad_id.status, message(&bad_id)),
        (400, "ID 형식이 올바르지 않습니다.")
    );
    let blank_id = api.post(PREVIEW, preview_body(" "));
    assert_eq!(
        (blank_id.status, message(&blank_id)),
        (400, "must not be blank")
    );
    let mut no_post = valid_version();
    no_post["userPromptTemplate"] = json!("{{x}}");
    let reply = api.post(PREVIEW, no_post);
    assert_eq!(reply.status, 400);
    assert!(api.stub.requests().is_empty());
}

// ---------------------------------------------------------------------------------------------
// 리뷰 조회
// ---------------------------------------------------------------------------------------------

fn insert_version(api: &Api, version: i32) -> Uuid {
    api.db
        .client()
        .query_one(
            "INSERT INTO ai_prompt_versions (type, version, system_prompt, user_prompt_template, \
             model, parameters, output_schema_version, created_by_admin_id, created_at) \
             VALUES ('PHOTO_REVIEW', $1, 's', '{{topicTitle}}', 'gpt-6.1-sol', '{}'::jsonb, '2', $2, now()) \
             RETURNING id",
            &[&version, &api.admin],
        )
        .unwrap()
        .get(0)
}

const COMPLETED_RESULT: &str = r#"{"overallScore":7.5,"criteria":[{"key":"COMPOSITION","label":"구도","score":7.5}],"distinctivePerspective":true,"feedback":{"summary":"요약","strength":"좋음","weakness":"아쉬움","suggestion":"제안"}}"#;
const COMPLETED_OUTPUT: &str = r#"{"compositionScore":7.46,"themeExpressionScore":8,"feedback":{"summary":"요약","strength":"좋음","weakness":"아쉬움","suggestion":"제안"}}"#;

fn insert_completed(api: &Api, seed: &Seed, version: Uuid, last_requested: &str) -> Uuid {
    api.db
        .client()
        .query_one(
            "INSERT INTO ai_reviews (post_id, user_id, prompt_version_id, status, attempt, round_attempt, \
             result, output, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens, latency_ms, \
             openai_response_id, request_id, requested_at, started_at, completed_at, request_count, last_requested_at) \
             VALUES ($1, $2, $3, 'COMPLETED', 1, 1, $4::text::jsonb, $5::text::jsonb, 1200, 200, 300, 100, 21000, \
             'resp_abc', gen_random_uuid(), '2026-10-20T05:00:00Z', '2026-10-21T05:00:01Z', '2026-10-21T05:00:21Z', \
             2, $6::text::timestamptz) RETURNING id",
            &[&seed.post, &seed.user, &version, &COMPLETED_RESULT, &COMPLETED_OUTPUT, &last_requested],
        )
        .unwrap()
        .get(0)
}

fn insert_queued(api: &Api, seed: &Seed, version: Uuid, last_requested: &str) -> Uuid {
    api.db
        .client()
        .query_one(
            "INSERT INTO ai_reviews (post_id, user_id, prompt_version_id, status, request_id, requested_at, \
             last_requested_at) VALUES ($1, $2, $3, 'QUEUED', gen_random_uuid(), $4::text::timestamptz, \
             $4::text::timestamptz) RETURNING id",
            &[&seed.post, &seed.user, &version, &last_requested],
        )
        .unwrap()
        .get(0)
}

// AdminAiReviewControllerTest.getAiReview_existing_returnsFullDetail
#[test]
fn 상세는_결과와_원출력을_jsonb_순서로_그대로_준다() {
    let Some(api) = api() else { return };
    let seed = seed_post(&api, "APPROVED");
    let version = insert_version(&api, 3);
    let review = insert_completed(&api, &seed, version, "2026-10-21T05:00:00Z");
    let reply = api.get(&format!("{REVIEWS}/{}", review.hyphenated()), &[]);
    assert_eq!(reply.status, 200, "{}", reply.text);
    let b = &reply.body;
    assert_eq!(b["status"], "COMPLETED");
    assert_eq!(b["errorCode"], Value::Null);
    assert_eq!(b["requestCount"], 2);
    assert_eq!(b["overallScore"], 7.5);
    assert_eq!(b["promptVersion"]["version"], 3);
    assert_eq!(b["promptVersion"]["model"], "gpt-6.1-sol");
    assert_eq!(b["post"]["postId"], seed.post.hyphenated().to_string());
    assert_eq!(b["post"]["moderationStatus"], "APPROVED");
    assert_eq!(b["post"]["deletedAt"], Value::Null);
    assert!(
        b["post"]["thumbnailUrl"]
            .as_str()
            .unwrap()
            .ends_with("/thumbnail.webp")
    );
    assert!(
        b["post"]["imageUrl"]
            .as_str()
            .unwrap()
            .ends_with("/original.webp")
    );
    assert_eq!(b["author"]["status"], "ACTIVE");
    assert_eq!(b["usage"]["inputTokens"], 1200);
    assert_eq!(b["latencyMs"], 21000);
    assert_eq!(b["requestedAt"], "2026-10-20T05:00:00Z");
    assert_eq!(b["lastRequestedAt"], "2026-10-21T05:00:00Z");
    assert_eq!(b["startedAt"], "2026-10-21T05:00:01Z");
    assert_eq!(b["completedAt"], "2026-10-21T05:00:21Z");
    assert_eq!(b["visibleToAuthor"], true);
    assert_eq!(b["attempt"], 1);
    assert_eq!(b["openaiResponseId"], "resp_abc");
    assert_eq!(b["result"]["criteria"][0]["key"], "COMPOSITION");
    // jsonb는 키를 짧은 것부터 정렬해 돌려준다. 원본은 그 문자열을 그대로 JSON으로 풀어 내보낸다.
    assert!(
        reply.text.contains(
            "\"output\":{\"feedback\":{\"summary\":\"요약\",\"strength\":\"좋음\",\"weakness\":\"아쉬움\",\"suggestion\":\"제안\"},\
             \"compositionScore\":7.46,\"themeExpressionScore\":8}"
        ),
        "{}",
        reply.text
    );
    assert!(reply.text.contains(
        "\"result\":{\"overallScore\":7.5,\"criteria\":[{\"key\":\"COMPOSITION\",\"label\":\"구도\",\"score\":7.5}],\"distinctivePerspective\":true,"
    ));
}

// AdminAiReviewControllerTest.getAiReview_withoutResultAndOutput_returnsNulls, unknownId, invalidId
#[test]
fn 처리_전_리뷰의_상세는_null이고_없는_id는_404다() {
    let Some(api) = api() else { return };
    let seed = seed_post(&api, "APPROVED");
    let version = insert_version(&api, 1);
    let review = insert_queued(&api, &seed, version, "2026-10-21T05:00:00Z");
    let reply = api.get(&format!("{REVIEWS}/{}", review.hyphenated()), &[]);
    assert_eq!(reply.status, 200);
    for field in [
        "result",
        "output",
        "openaiResponseId",
        "startedAt",
        "completedAt",
        "overallScore",
        "latencyMs",
        "errorCode",
    ] {
        assert_eq!(reply.body[field], Value::Null, "{field}");
    }
    assert_eq!(reply.body["attempt"], 0);
    assert_eq!(reply.body["requestCount"], 1);
    assert_eq!(
        reply.body["usage"],
        json!({"inputTokens": null, "cachedInputTokens": null, "outputTokens": null, "reasoningTokens": null})
    );
    let unknown = api.get(&format!("{REVIEWS}/{}", id()), &[]);
    assert_eq!(
        (unknown.status, message(&unknown)),
        (404, "AI 리뷰를 찾을 수 없습니다.")
    );
    let bad = api.get(&format!("{REVIEWS}/xyz"), &[]);
    assert_eq!(
        (bad.status, message(&bad)),
        (400, "ID 형식이 올바르지 않습니다.")
    );
}

// AdminAiReviewQueryRepositoryTest: 정렬, 필터, 사용자 앱에서 숨겨진 리뷰도 조회
#[test]
fn 목록은_마지막_요청_시각_내림차순이고_상태와_버전으로_거른다() {
    let Some(api) = api() else { return };
    let v1 = insert_version(&api, 1);
    let v2 = insert_version(&api, 2);
    let old = seed_post(&api, "APPROVED");
    let mid = seed_post(&api, "APPROVED");
    let hidden = seed_post(&api, "REJECTED");
    let r_old = insert_completed(&api, &old, v1, "2026-10-21T05:00:00Z");
    let r_mid = insert_queued(&api, &mid, v2, "2026-10-22T05:00:00Z");
    let r_hidden = insert_queued(&api, &hidden, v2, "2026-10-23T05:00:00Z");

    let all = api.get(REVIEWS, &[]);
    assert_eq!(all.status, 200, "{}", all.text);
    assert_eq!(all.body["currentPage"], 1);
    assert_eq!(all.body["pageSize"], 20);
    assert_eq!(all.body["hasNext"], false);
    let ids: Vec<&str> = all.body["aiReviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["aiReviewId"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = [r_hidden, r_mid, r_old]
        .iter()
        .map(|u| u.hyphenated().to_string())
        .collect();
    assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>());
    let items = all.body["aiReviews"].as_array().unwrap();
    assert_eq!(items[0]["visibleToAuthor"], false);
    assert_eq!(items[0]["post"]["moderationStatus"], "REJECTED");
    assert_eq!(items[1]["visibleToAuthor"], true);
    assert_eq!(items[2]["overallScore"], 7.5);
    assert_eq!(items[2]["promptVersion"]["version"], 1);
    assert!(items[2].get("startedAt").is_none());
    assert!(items[2]["post"].get("imageUrl").is_none());

    let queued = api.get(REVIEWS, &[("status", "QUEUED")]);
    assert_eq!(queued.body["aiReviews"].as_array().unwrap().len(), 2);
    let by_version = api.get(
        REVIEWS,
        &[("promptVersionId", &v1.hyphenated().to_string())],
    );
    assert_eq!(by_version.body["aiReviews"].as_array().unwrap().len(), 1);
    let both = api.get(
        REVIEWS,
        &[
            ("status", "COMPLETED"),
            ("promptVersionId", &v2.hyphenated().to_string()),
        ],
    );
    assert_eq!(both.body["aiReviews"], json!([]));
    let unknown_version = api.get(REVIEWS, &[("promptVersionId", &id())]);
    assert_eq!(unknown_version.status, 200);
    assert_eq!(unknown_version.body["aiReviews"], json!([]));

    let page = api.get(REVIEWS, &[("pageSize", "2")]);
    assert_eq!(page.body["hasNext"], true);
    assert_eq!(page.body["aiReviews"].as_array().unwrap().len(), 2);
    let page2 = api.get(REVIEWS, &[("pageSize", "2"), ("page", "2")]);
    assert_eq!(page2.body["hasNext"], false);
    assert_eq!(
        page2.body["aiReviews"][0]["aiReviewId"],
        r_old.hyphenated().to_string()
    );
}

// AdminAiReviewControllerTest.getAiReviews_invalidPagination / invalidFilter
#[test]
fn 리뷰_목록의_잘못된_조건은_400이다() {
    let Some(api) = api() else { return };
    for (name, value) in [("page", "0"), ("pageSize", "0"), ("pageSize", "101")] {
        let reply = api.get(REVIEWS, &[(name, value)]);
        assert_eq!(
            (reply.status, message(&reply)),
            (400, "조회 조건이 올바르지 않습니다."),
            "{name}={value}"
        );
    }
    for (name, value) in [
        ("status", "DONE"),
        ("promptVersionId", "x"),
        ("page", "x"),
        ("pageSize", "x"),
    ] {
        let reply = api.get(REVIEWS, &[(name, value)]);
        assert_eq!(reply.status, 400, "{name}={value}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    }
    assert_eq!(api.get(REVIEWS, &[("pageSize", "100")]).status, 200);
    assert_eq!(
        api.get(REVIEWS, &[("status", ""), ("promptVersionId", " ")])
            .status,
        200
    );
}

// 사용자 앱 노출 조건(PostJpaRepository.findVisibleById)과 같은 조건
#[test]
fn 삭제된_작성자_주제_사진_게시물이면_작성자_앱에서_보이지_않는다() {
    let Some(api) = api() else { return };
    let version = insert_version(&api, 1);
    let seed = seed_post(&api, "APPROVED");
    let review = insert_queued(&api, &seed, version, "2026-10-21T05:00:00Z");
    let visible = |api: &Api| {
        api.get(&format!("{REVIEWS}/{}", review.hyphenated()), &[])
            .body["visibleToAuthor"]
            .clone()
    };
    assert_eq!(visible(&api), true);
    let user = seed.user;
    let topic = seed.topic;
    let post = seed.post;
    for (set, clear) in [
        (
            format!("UPDATE users SET deleted_at = now() WHERE id = '{user}'"),
            format!("UPDATE users SET deleted_at = NULL WHERE id = '{user}'"),
        ),
        (
            format!("UPDATE topics SET deleted_at = now() WHERE id = '{topic}'"),
            format!("UPDATE topics SET deleted_at = NULL WHERE id = '{topic}'"),
        ),
        (
            format!(
                "UPDATE photos SET deleted_at = now() WHERE id = (SELECT photo_id FROM posts WHERE id = '{post}')"
            ),
            format!(
                "UPDATE photos SET deleted_at = NULL WHERE id = (SELECT photo_id FROM posts WHERE id = '{post}')"
            ),
        ),
        (
            format!("UPDATE posts SET deleted_at = now() WHERE id = '{post}'"),
            format!("UPDATE posts SET deleted_at = NULL WHERE id = '{post}'"),
        ),
    ] {
        api.db.client().batch_execute(&set).unwrap();
        assert_eq!(visible(&api), false, "{set}");
        api.db.client().batch_execute(&clear).unwrap();
        assert_eq!(visible(&api), true, "{clear}");
    }
    // 정지된 작성자는 영향을 주지 않는다.
    api.db
        .client()
        .batch_execute(&format!(
            "UPDATE users SET status = 'BANNED' WHERE id = '{user}'"
        ))
        .unwrap();
    assert_eq!(visible(&api), true);
}

#[test]
fn 출력_문자열은_공백을_없애고_제이슨_이스케이프로_쓴다() {
    assert_eq!(
        minify_json(
            "{\"a\": [1, 2.50, \"x y\"], \"b\": {\"c\": null}, \"d\": \"\\u0001\\n\\\"q\\\"\"}"
        ),
        "{\"a\":[1,2.50,\"x y\"],\"b\":{\"c\":null},\"d\":\"\\u0001\\n\\\"q\\\"\"}"
    );
}
