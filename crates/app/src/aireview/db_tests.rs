//! AI 리뷰 요청·조회 API와 워커를 실제 DB로 끝까지 돌려 보는 시험.
//!
//! 기대값은 원본 `PostAiReviewControllerTest`, `PostAiReviewServiceTest`, `AiReviewWorkerServiceTest`,
//! `AiReviewRepositoryTest`에서 가져왔다. 목 대신 DB와 대역 생성기를 쓴다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_core::FixedClock;
use chalkak_core::log::{Level, Logger};
use chalkak_db::Pool;
use chalkak_http::{Handler, Method, Request, Router};
use jiff::Timestamp;
use serde_json::Value;
use uuid::Uuid;

use super::backoff::AiReviewRetryBackoff;
use super::domain::{Feedback, PhotoReviewOutput};
use super::generator::{
    AiReviewGeneration, AiReviewGenerationError, AiReviewGenerator, Attempt, FailureType, Usage,
};
use super::review::Status;
use super::review_repository::AiReviewRepository;
use super::testing::StubGenerator;
use super::worker_service::{AbandonReason, AiReviewWorkerService};
use crate::admin::testing::{insert_admin, insert_post, insert_topic, insert_user};
use crate::analytics::testing::Lines;
use crate::photo::url::CloudFrontUrlProvider;
use crate::security::{self, AUTHORITY_USER, Principal};
use crate::testing::TestDb;
use crate::web::Routes;

const START: &str = "2026-10-20T05:00:00Z";

struct Fx {
    db: TestDb,
    pool: Pool,
    clock: Arc<FixedClock>,
    lines: Lines,
    stub: Arc<StubGenerator>,
    router: Router,
    admin: Uuid,
    author: Uuid,
    post: Uuid,
    version: Uuid,
}

struct Reply {
    status: u16,
    body: Value,
    text: String,
}

fn fx(outcomes: Vec<Result<AiReviewGeneration, AiReviewGenerationError>>) -> Option<Fx> {
    let db = TestDb::create()?;
    let pool = db.pool(4);
    let clock = Arc::new(FixedClock::new(START.parse().unwrap()));
    let lines = Lines::default();
    let logger = Arc::new(Logger::new(
        Box::new(lines.clone()),
        Level::Info,
        clock.clone(),
    ));
    let stub = Arc::new(StubGenerator::new(&["gpt-6.1-sol", "gpt-6-luna"], outcomes));
    let generator: Arc<dyn AiReviewGenerator> = stub.clone();
    let mut client = db.client();
    let admin = insert_admin(&mut client, "operator");
    let author = insert_user(&mut client, "author@example.com", "ACTIVE");
    let topic = insert_topic(
        &mut client,
        "오늘의 주제",
        "2026-10-20",
        "2026-10-19T15:00:00Z",
        "2026-10-20T14:59:59Z",
    );
    let post = insert_post(&mut client, author, topic, "APPROVED");
    let version = insert_version(&db, admin, 1);
    let routes = super::api::register_parts(
        Routes::new(logger.clone()),
        pool.clone(),
        clock.clone(),
        logger,
        generator,
    );
    Some(Fx {
        router: routes.into_router(),
        db,
        pool,
        clock,
        lines,
        stub,
        admin,
        author,
        post,
        version,
    })
}

fn insert_version(db: &TestDb, admin: Uuid, version: i32) -> Uuid {
    db.client()
        .query_one(
            "INSERT INTO ai_prompt_versions (type, version, system_prompt, user_prompt_template, \
             model, parameters, output_schema_version, created_by_admin_id, created_at) \
             VALUES ('PHOTO_REVIEW', $1, 's', '{{topicTitle}}', 'gpt-6.1-sol', '{}'::jsonb, '2', $2, now()) \
             RETURNING id",
            &[&version, &admin],
        )
        .unwrap()
        .get(0)
}

fn success() -> AiReviewGeneration {
    let output = PhotoReviewOutput::new(
        8.72,
        8.0,
        Feedback {
            summary: "구도가 좋습니다.".to_owned(),
            strength: "빛이 좋습니다.".to_owned(),
            weakness: "수평이 아쉽습니다.".to_owned(),
            suggestion: "낮게 찍어 보세요.".to_owned(),
        },
    )
    .unwrap();
    AiReviewGeneration {
        result: output.to_result(7.0),
        output,
        raw_output: "{\"compositionScore\":8.72}".to_owned(),
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

fn failure(kind: FailureType) -> Result<AiReviewGeneration, AiReviewGenerationError> {
    Err(AiReviewGenerationError::of(kind))
}

fn call_router(router: &Router, method: Method, user: Uuid, post: &str) -> Reply {
    let _guard = security::enter(Principal {
        subject: user.hyphenated().to_string(),
        authorities: vec![AUTHORITY_USER.to_owned()],
    });
    let response = router.handle(Request::new(
        method,
        format!("/api/v1/posts/{post}/ai-review"),
    ));
    let text = String::from_utf8_lossy(&response.body).into_owned();
    Reply {
        status: response.status,
        body: serde_json::from_str(&text).unwrap_or(Value::Null),
        text,
    }
}

impl Fx {
    fn service(&self) -> AiReviewWorkerService {
        self.service_with(self.stub.clone())
    }

    fn service_with(&self, generator: Arc<dyn AiReviewGenerator>) -> AiReviewWorkerService {
        AiReviewWorkerService::new(
            self.pool.clone(),
            generator,
            Arc::new(CloudFrontUrlProvider::new(
                "https://cdn.test.chalkak",
                "/chalkak",
            )),
            self.clock.clone(),
            Arc::new(Logger::new(
                Box::new(self.lines.clone()),
                Level::Info,
                self.clock.clone(),
            )),
            Duration::from_secs(120),
            3,
            AiReviewRetryBackoff::new(|| 0.5),
        )
    }

    fn call(&self, method: Method, user: Uuid, post: &str) -> Reply {
        call_router(&self.router, method, user, post)
    }

    fn request(&self) -> Reply {
        self.call(Method::Post, self.author, &self.post.to_string())
    }

    fn get(&self) -> Reply {
        self.call(Method::Get, self.author, &self.post.to_string())
    }

    fn review(&self) -> super::review::AiReview {
        AiReviewRepository
            .find_by_post_id(&mut self.db.client(), self.post)
            .unwrap()
            .expect("리뷰 행")
    }

    fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
    }

    fn notifications(&self) -> Vec<(String, String, String, String)> {
        self.db
            .client()
            .query(
                "SELECT type, title, body, sqs_publish_status FROM notifications ORDER BY created_at, id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
            .collect()
    }

    /// 요청 → 가져가기 → 처리까지 한 번에 돌린다.
    fn run_worker_once(&self) {
        let service = self.service();
        let claim = service.claim_next().unwrap().expect("가져갈 행");
        service.process(&claim, &|| false);
    }

    fn events(&self, name: &str) -> Vec<Value> {
        self.lines
            .events()
            .into_iter()
            .filter(|e| e["event"] == name)
            .collect()
    }
}

// PostAiReviewControllerTest: 요청은 처리 중이면 202, 응답 필드는 status, requestedAt, completedAt, result, canRequestAgain 순서
// PostAiReviewServiceTest#request_firstTime_createsQueuedReviewPinnedToLatestVersion
#[test]
fn 처음_요청하면_최신_버전을_고정한_대기_행을_만들고_202_처리_중으로_응답한다() {
    let Some(fx) = fx(vec![]) else { return };
    let reply = fx.request();
    assert_eq!(reply.status, 202, "{}", reply.text);
    assert_eq!(
        reply.text,
        "{\"status\":\"PROCESSING\",\"requestedAt\":\"2026-10-20T05:00:00Z\",\"completedAt\":null,\"result\":null,\"canRequestAgain\":false}"
    );
    let review = fx.review();
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.prompt_version_id, fx.version);
    assert_eq!(review.user_id, fx.author);
    assert_eq!(review.request_count, 1);
    assert_eq!(review.attempt, 0);
    // 요청 로그는 새 행을 만들 때만 한 번, 프롬프트·이미지·주제는 남기지 않는다.
    assert_eq!(fx.stub.requests().len(), 0);
    fx.request();
    let logs = fx.events("review_requested");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["created"], true);
    assert_eq!(logs[0]["promptVersion"], 1);
    assert_eq!(logs[0]["requestCount"], 1);
    assert_eq!(logs[0]["message"], "AI 리뷰 요청");
}

// PostAiReviewControllerTest#getAiReview_notRequested_returnsNone
#[test]
fn 요청한_적_없으면_조회는_200과_요청_전_상태이고_행을_만들지_않는다() {
    let Some(fx) = fx(vec![]) else { return };
    let reply = fx.get();
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.text,
        "{\"status\":\"NONE\",\"requestedAt\":null,\"completedAt\":null,\"result\":null,\"canRequestAgain\":false}"
    );
    let rows: i64 = fx
        .db
        .client()
        .query_one("SELECT count(*) FROM ai_reviews", &[])
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
}

// PostAiReviewServiceTest: 작성자가 아니거나 공개 조건을 만족하지 않는 게시물은 요청도 조회도 404이고 행을 만들지 않는다
#[test]
fn 작성자가_아니거나_공개되지_않은_게시물은_404다() {
    let Some(fx) = fx(vec![]) else { return };
    let stranger = insert_user(&mut fx.db.client(), "stranger@example.com", "ACTIVE");
    let post = fx.post.to_string();
    for method in [Method::Post, Method::Get] {
        let reply = fx.call(method, stranger, &post);
        assert_eq!(reply.status, 404, "{}", reply.text);
        assert_eq!(reply.body["message"], "게시물을 찾을 수 없습니다.");
    }
    fx.db
        .client()
        .execute(
            "UPDATE posts SET moderation_status = 'PENDING'::moderation_status WHERE id = $1",
            &[&fx.post],
        )
        .unwrap();
    assert_eq!(fx.request().status, 404);
    assert_eq!(fx.get().status, 404);
    let missing = Uuid::new_v4().to_string();
    assert_eq!(fx.call(Method::Get, fx.author, &missing).status, 404);
    let rows: i64 = fx
        .db
        .client()
        .query_one("SELECT count(*) FROM ai_reviews", &[])
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
}

// PostAiReviewControllerTest#requestAiReview_invalidPostId_returnsBadRequest
#[test]
fn 게시물_id_형식이_올바르지_않으면_400이다() {
    let Some(fx) = fx(vec![]) else { return };
    let reply = fx.call(Method::Post, fx.author, "not-a-uuid");
    assert_eq!(reply.status, 400);
    assert_eq!(reply.body["message"], "ID 형식이 올바르지 않습니다.");
}

// PostAiReviewServiceTest#request_noPromptVersion_throwsBusinessException
#[test]
fn 쓸_수_있는_버전이_없으면_400이고_행을_만들지_않는다() {
    let Some(fx) = fx(vec![]) else { return };
    fx.db
        .client()
        .execute(
            "UPDATE ai_prompt_versions SET output_schema_version = '1'",
            &[],
        )
        .unwrap();
    let reply = fx.request();
    assert_eq!(reply.status, 400);
    assert_eq!(
        reply.body["message"],
        "지금은 AI 리뷰를 요청할 수 없습니다."
    );
    let rows: i64 = fx
        .db
        .client()
        .query_one("SELECT count(*) FROM ai_reviews", &[])
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
}

// AiReviewWorkerServiceTest#process_success_completesWithResultAndUsage / 완료하면 완료 알림을 하나 만든다
#[test]
fn 성공하면_결과와_사용량을_저장해_완료하고_완료_알림을_만든다() {
    let Some(fx) = fx(vec![Ok(success())]) else {
        return;
    };
    fx.request();
    fx.advance(5);
    fx.run_worker_once();

    let review = fx.review();
    assert_eq!(review.status, Status::Completed);
    assert_eq!(review.attempt, 1);
    assert_eq!(review.round_attempt, 1);
    assert_eq!(
        review.output.as_deref().map(|o| o.contains("8.72")),
        Some(true)
    );
    assert_eq!(review.invocation.input_tokens, Some(1200));
    assert_eq!(review.invocation.latency_ms, Some(1234));
    assert_eq!(
        review.invocation.openai_response_id.as_deref(),
        Some("resp_1")
    );
    assert_eq!(
        review.completed_at,
        Some("2026-10-20T05:00:05Z".parse().unwrap())
    );
    assert!(review.lease_expires_at.is_none());

    let sent = fx.stub.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].topic_title, "오늘의 주제");
    assert!(sent[0].image_url.starts_with("https://cdn.test.chalkak/"));

    assert_eq!(
        fx.notifications(),
        vec![(
            "AI_REVIEW_COMPLETED".to_owned(),
            "AI 피드백이 도착했어요".to_owned(),
            "사진의 구도 점수와 피드백을 확인해 보세요.".to_owned(),
            "PENDING".to_owned()
        )]
    );
    let created: Timestamp = fx
        .db
        .client()
        .query_one("SELECT created_at FROM notifications", &[])
        .unwrap()
        .get(0);
    assert_eq!(created, "2026-10-20T05:00:05Z".parse().unwrap());

    // 완료 로그: 피드백·프롬프트·주소·주제는 남기지 않는다.
    let logs = fx.events("review_completed");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["message"], "AI 리뷰 완료");
    assert_eq!(logs[0]["queueWaitMs"], 5000);
    assert_eq!(logs[0]["totalMs"], 5000);
    assert_eq!(logs[0]["inputTokens"], 1200);
    assert!(logs[0].get("errorCode").is_none());
    assert!(
        !fx.lines
            .events()
            .iter()
            .any(|e| e.to_string().contains("구도가 좋습니다"))
    );

    // 조회 응답은 완료와 결과를 돌려준다.
    let reply = fx.get();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["status"], "COMPLETED");
    assert_eq!(reply.body["completedAt"], "2026-10-20T05:00:05Z");
    assert_eq!(reply.body["result"]["overallScore"], 8.7);
    assert_eq!(reply.body["result"]["distinctivePerspective"], true);
    assert_eq!(reply.body["result"]["criteria"][0]["key"], "COMPOSITION");
    assert_eq!(reply.body["canRequestAgain"], false);
    // 완료된 리뷰는 다시 요청해도 같은 상태를 200으로 돌려준다.
    assert_eq!(fx.request().status, 200);
}

// AiReviewWorkerServiceTest: 재시도할 수 있는 실패는 대기로 되돌려 다음 시도 시각을 정하고 임대·완료 시각 없이 마지막 시도의 사유를 남긴다
#[test]
fn 재시도할_수_있는_실패는_대기로_되돌리고_시각이_되면_다시_가져간다() {
    let Some(fx) = fx(vec![failure(FailureType::Timeout), Ok(success())]) else {
        return;
    };
    fx.request();
    fx.run_worker_once();

    let review = fx.review();
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.error_code.as_deref(), Some("TIMEOUT"));
    assert_eq!(
        review.next_attempt_at,
        Some("2026-10-20T05:00:10Z".parse().unwrap())
    );
    assert!(review.lease_expires_at.is_none());
    assert!(review.completed_at.is_none());
    assert!(fx.notifications().is_empty());
    // 앱에는 처리 중으로 보이고 결과와 완료 시각을 드러내지 않는다.
    let reply = fx.get();
    assert_eq!(reply.body["status"], "PROCESSING");
    assert_eq!(reply.body["completedAt"], Value::Null);
    let retry = fx.events("review_retry_scheduled");
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0]["retryInMs"], 10_000);
    assert_eq!(retry[0]["retryAfterUsed"], false);
    assert_eq!(retry[0]["roundAttempt"], 1);
    assert!(fx.events("review_failed").is_empty());

    // 다음 시도 시각 전에는 가져가지 않는다.
    let service = fx.service();
    fx.advance(9);
    assert!(service.claim_next().unwrap().is_none());
    fx.advance(1);
    fx.run_worker_once();
    let review = fx.review();
    assert_eq!(review.status, Status::Completed);
    assert_eq!(review.attempt, 2);
    assert_eq!(review.round_attempt, 2);
    assert!(review.next_attempt_at.is_none());
}

// AiReviewWorkerServiceTest: 세 번째 시도까지 재시도 가능한 실패면 대기 시간을 10초, 30초로 늘리다 마지막 시도의 사유로 실패를 확정한다
#[test]
fn 세_번째_시도까지_재시도_가능한_실패면_10초_30초로_늘리다_실패를_확정한다() {
    let Some(fx) = fx(vec![
        failure(FailureType::RateLimited),
        failure(FailureType::RateLimited),
        failure(FailureType::RateLimited),
    ]) else {
        return;
    };
    fx.request();
    fx.run_worker_once();
    assert_eq!(
        fx.review().next_attempt_at,
        Some("2026-10-20T05:00:10Z".parse().unwrap())
    );
    fx.advance(10);
    fx.run_worker_once();
    assert_eq!(
        fx.review().next_attempt_at,
        Some("2026-10-20T05:00:40Z".parse().unwrap())
    );
    fx.advance(30);
    fx.run_worker_once();

    let review = fx.review();
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.error_code.as_deref(), Some("RATE_LIMITED"));
    assert_eq!(review.round_attempt, 3);
    assert_eq!(fx.events("review_retry_scheduled").len(), 2);
    assert_eq!(fx.events("review_failed").len(), 1);
    assert_eq!(fx.notifications().len(), 1);
}

// AiReviewWorkerServiceTest: Retry-After가 백오프보다 길면 그 값만큼 기다리고 로그에 적용 여부를 남긴다
#[test]
fn retry_after가_백오프보다_길면_그_값만큼_기다린다() {
    let Some(fx) = fx(vec![Err(AiReviewGenerationError::with_retry_after(
        FailureType::RateLimited,
        Duration::from_secs(45),
    ))]) else {
        return;
    };
    fx.request();
    fx.run_worker_once();
    assert_eq!(
        fx.review().next_attempt_at,
        Some("2026-10-20T05:00:45Z".parse().unwrap())
    );
    let retry = fx.events("review_retry_scheduled");
    assert_eq!(retry[0]["retryInMs"], 45_000);
    assert_eq!(retry[0]["retryAfterUsed"], true);
}

// AiReviewWorkerServiceTest: 실패로 끝나면 다시 요청할 수 있다는 문구의 실패 알림을 하나 만든다 /
// 요청 세 번을 모두 실패하면 마지막 알림만 더는 요청할 수 없다는 문구다 / 회차마다 알림이 하나씩 생기고 키가 서로 다르다
#[test]
fn 재요청마다_실패_알림이_하나씩_생기고_세_번째는_더는_요청할_수_없다는_문구다() {
    let Some(fx) = fx(vec![
        failure(FailureType::InvalidOutput),
        failure(FailureType::InvalidOutput),
        failure(FailureType::InvalidOutput),
    ]) else {
        return;
    };
    assert_eq!(fx.request().status, 202);
    fx.run_worker_once();
    let reply = fx.get();
    assert_eq!(reply.body["status"], "FAILED");
    assert_eq!(reply.body["canRequestAgain"], true);
    assert_eq!(reply.body["completedAt"], "2026-10-20T05:00:00Z");

    // 새 버전이 생기면 다시 요청할 때 그 버전으로 다시 고정한다.
    let version2 = insert_version(&fx.db, fx.admin, 2);
    fx.advance(60);
    let again = fx.request();
    assert_eq!(again.status, 202, "{}", again.text);
    let review = fx.review();
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.request_count, 2);
    assert_eq!(review.prompt_version_id, version2);
    assert_eq!(review.round_attempt, 0);
    assert_eq!(review.attempt, 1);
    assert!(review.error_code.is_none());
    assert!(review.completed_at.is_none());
    assert_eq!(review.invocation, Default::default());
    assert_eq!(review.requested_at, START.parse().unwrap());
    let logs = fx.events("review_requested");
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[1]["created"], false);
    assert_eq!(logs[1]["requestCount"], 2);
    assert_eq!(logs[1]["promptVersion"], 2);
    // 같은 요청을 처리 중에 다시 보내도 요청 횟수는 늘지 않는다.
    assert_eq!(fx.request().status, 202);
    assert_eq!(fx.review().request_count, 2);

    fx.run_worker_once();
    assert_eq!(fx.review().attempt, 2);
    fx.advance(60);
    assert_eq!(fx.request().status, 202);
    fx.run_worker_once();

    let notes = fx.notifications();
    assert_eq!(notes.len(), 3);
    assert_eq!(notes[0].1, "AI 피드백을 만들지 못했어요");
    assert_eq!(notes[0].2, "잠시 후 다시 요청해 주세요.");
    assert_eq!(notes[1].2, "잠시 후 다시 요청해 주세요.");
    assert_eq!(
        notes[2].2,
        "이 사진은 더 이상 AI 피드백을 요청할 수 없어요."
    );
    let keys: i64 = fx
        .db
        .client()
        .query_one("SELECT count(DISTINCT event_key) FROM notifications", &[])
        .unwrap()
        .get(0);
    assert_eq!(keys, 3);

    // 상한에 닿은 뒤의 요청은 400이고 행도 로그도 바뀌지 않는다.
    let before = fx.events("review_requested").len();
    let reply = fx.request();
    assert_eq!(reply.status, 400);
    assert_eq!(
        reply.body["message"],
        "더 이상 AI 리뷰를 다시 요청할 수 없습니다."
    );
    assert_eq!(fx.review().request_count, 3);
    assert_eq!(fx.events("review_requested").len(), before);
    assert_eq!(fx.get().body["canRequestAgain"], false);
}

// AiReviewWorkerServiceTest: 호출 전에 게시물이 공개 조건을 잃었으면 호출하지 않고 POST_UNAVAILABLE로 실패시킨다
#[test]
fn 게시물이_공개_조건을_잃으면_호출하지_않고_post_unavailable로_실패하고_알림은_없다() {
    let Some(fx) = fx(vec![Ok(success())]) else {
        return;
    };
    fx.request();
    fx.db
        .client()
        .execute(
            "UPDATE posts SET deleted_at = now() WHERE id = $1",
            &[&fx.post],
        )
        .unwrap();
    fx.run_worker_once();
    let review = fx.review();
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.error_code.as_deref(), Some("POST_UNAVAILABLE"));
    assert!(fx.stub.requests().is_empty());
    assert!(fx.notifications().is_empty());
}

// AiReviewWorkerServiceTest: 고정한 버전의 출력 스키마가 다르거나 모델이 더는 허용되지 않으면 호출하지 않고 PROMPT_VERSION_UNAVAILABLE
#[test]
fn 버전을_더는_쓸_수_없으면_호출하지_않고_prompt_version_unavailable로_실패한다() {
    let Some(fx) = fx(vec![Ok(success())]) else {
        return;
    };
    fx.request();
    fx.db
        .client()
        .execute("UPDATE ai_prompt_versions SET model = 'gpt-old'", &[])
        .unwrap();
    fx.run_worker_once();
    let review = fx.review();
    assert_eq!(review.status, Status::Failed);
    assert_eq!(
        review.error_code.as_deref(),
        Some("PROMPT_VERSION_UNAVAILABLE")
    );
    assert!(fx.stub.requests().is_empty());
    // 게시물 사용 불가와 달리 알림은 만든다.
    assert_eq!(fx.notifications().len(), 1);
}

// AiReviewWorkerServiceTest: 가져간 횟수 상한을 넘은 회수 행은 호출하지 않고 LEASE_EXHAUSTED로 끝낸 뒤 다음 행을 가져간다
#[test]
fn 임대_회수로_상한을_넘으면_호출하지_않고_lease_exhausted로_끝내고_알림을_만든다() {
    let Some(fx) = fx(vec![]) else { return };
    fx.request();
    let service = fx.service();
    for attempt in 1..=3 {
        let claim = service.claim_next().unwrap().expect("가져갈 행");
        assert_eq!(claim.attempt, attempt);
        fx.advance(121);
    }
    assert!(service.claim_next().unwrap().is_none());
    let review = fx.review();
    assert_eq!(review.status, Status::Failed);
    assert_eq!(review.error_code.as_deref(), Some("LEASE_EXHAUSTED"));
    assert_eq!(review.attempt, 3);
    assert!(fx.stub.requests().is_empty());
    assert_eq!(fx.notifications().len(), 1);
    assert_eq!(fx.events("review_failed").len(), 1);
}

// AiReviewWorkerServiceTest: 임대가 끝나 다른 워커가 다시 가져간 뒤 늦게 끝난 워커의 결과는 반영하지 않고 로그만 남긴다
#[test]
fn 늦게_끝난_워커의_결과는_반영하지_않고_stale_로그만_남긴다() {
    let Some(fx) = fx(vec![Ok(success())]) else {
        return;
    };
    fx.request();
    let service = fx.service();
    let first = service.claim_next().unwrap().unwrap();
    fx.advance(121);
    let second = service.claim_next().unwrap().unwrap();
    assert_eq!((first.attempt, second.attempt), (1, 2));

    service.process(&first, &|| false);

    let review = fx.review();
    assert_eq!(review.status, Status::Processing);
    assert_eq!(review.attempt, 2);
    assert!(fx.notifications().is_empty());
    assert_eq!(fx.events("review_stale_update").len(), 1);
}

// AiReviewWorkerServiceTest: 종료 중이면 생성이 실패해도 실패를 쓰지 않고 처리 중으로 남긴다
#[test]
fn 종료_중이면_생성이_실패해도_처리_중으로_남긴다() {
    let Some(fx) = fx(vec![failure(FailureType::Timeout)]) else {
        return;
    };
    fx.request();
    let service = fx.service();
    let claim = service.claim_next().unwrap().unwrap();
    service.process(&claim, &|| true);
    assert_eq!(fx.review().status, Status::Processing);
    let abandoned = fx.events("review_abandoned");
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0]["reason"], AbandonReason::Shutdown.name());
}

// AiReviewWorkerServiceTest: 가져갈 수 있는 행이 둘이면 오래 기다린 요청부터 가져가고 없으면 비어 있다
#[test]
fn 오래_기다린_요청부터_가져간다() {
    let Some(fx) = fx(vec![]) else { return };
    let other = insert_user(&mut fx.db.client(), "other@example.com", "ACTIVE");
    let topic2 = insert_topic(
        &mut fx.db.client(),
        "다른 주제",
        "2026-10-21",
        "2026-10-20T15:00:00Z",
        "2026-10-21T14:59:59Z",
    );
    let other_post = insert_post(&mut fx.db.client(), other, topic2, "APPROVED");
    let service = fx.service();
    assert!(service.claim_next().unwrap().is_none());
    fx.request();
    fx.advance(1);
    assert_eq!(
        fx.call(Method::Post, other, &other_post.to_string()).status,
        202
    );
    let first = service.claim_next().unwrap().unwrap();
    let second = service.claim_next().unwrap().unwrap();
    assert_eq!(first.post_id, fx.post);
    assert_eq!(second.post_id, other_post);
    assert!(service.claim_next().unwrap().is_none());
}

// AiReviewRepositoryTest#insertIfAbsent: 이미 있으면 아무것도 바꾸지 않는다 / 알림 수신 설정을 끈 회원의 알림은 발행하지 않는다
#[test]
fn 이미_있는_게시물에는_다시_넣지_않고_푸시를_끈_회원의_알림은_발행하지_않는다() {
    let Some(fx) = fx(vec![Ok(success())]) else {
        return;
    };
    fx.request();
    let mut client = fx.db.client();
    let again = super::review::AiReview::request(
        fx.post,
        fx.author,
        fx.version,
        Uuid::new_v4(),
        START.parse().unwrap(),
    );
    assert_eq!(
        AiReviewRepository
            .insert_if_absent(&mut client, &again)
            .unwrap(),
        0
    );

    client
        .execute(
            "UPDATE users SET ai_review_push_enabled = false WHERE id = $1",
            &[&fx.author],
        )
        .unwrap();
    fx.run_worker_once();
    let notes = fx.notifications();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].3, "NOT_REQUIRED");
}

// UserMetricsRepositoryTest#countAiReviews: 최초 요청 시각이 구간에 든 리뷰만 세고 제외 회원은 뺀다. 완료·실패는 집계 시점 상태다.
#[test]
fn 사용자_지표는_최초_요청_시각_기준으로_ai_리뷰를_세고_제외_회원은_뺀다() {
    use crate::analytics::repository::PgUserMetricsRepository;
    use crate::analytics::service::{AiReviewCounts, UserMetricsRepository};

    let Some(fx) = fx(vec![Ok(success())]) else {
        return;
    };
    let from: Timestamp = "2026-10-19T15:00:00Z".parse().unwrap();
    let to: Timestamp = "2026-10-20T15:00:00Z".parse().unwrap();
    let repo = PgUserMetricsRepository::new(fx.db.pool(2), vec![]);
    let counts = |repo: &PgUserMetricsRepository| repo.count_ai_reviews(from, to).unwrap();
    assert_eq!(counts(&repo), AiReviewCounts::default());

    fx.request();
    assert_eq!(
        counts(&repo),
        AiReviewCounts {
            users: 1,
            requests: 1,
            completed: 0,
            failed: 0
        }
    );
    fx.run_worker_once();
    assert_eq!(counts(&repo).completed, 1);
    // 다음 날 구간에는 잡히지 않는다.
    let next = repo
        .count_ai_reviews(to, "2026-10-21T15:00:00Z".parse().unwrap())
        .unwrap();
    assert_eq!(next, AiReviewCounts::default());
    // 제외 회원
    let excluded = PgUserMetricsRepository::new(fx.db.pool(2), vec![fx.author]);
    assert_eq!(counts(&excluded), AiReviewCounts::default());
}

const CONCURRENT_REQUESTS: usize = 5;

impl Fx {
    /// 같은 사용자가 같은 게시물에 동시에 요청한다. 모두 준비되면 한꺼번에 시작한다.
    fn request_concurrently(&self) -> Vec<Reply> {
        let barrier = std::sync::Barrier::new(CONCURRENT_REQUESTS);
        let (router, author, post) = (&self.router, self.author, self.post.to_string());
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..CONCURRENT_REQUESTS)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        call_router(router, Method::Post, author, &post)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }

    fn review_count(&self) -> i64 {
        self.db
            .client()
            .query_one("SELECT COUNT(*) FROM ai_reviews", &[])
            .unwrap()
            .get(0)
    }
}

// PostAiReviewConcurrencyTest#request_concurrent_createsSingleRow
#[test]
fn 같은_게시물에_동시에_요청해도_행은_하나이고_모두_처리_중으로_응답받는다() {
    let Some(fx) = fx(vec![]) else { return };
    let replies = fx.request_concurrently();
    for reply in &replies {
        assert_eq!(reply.status, 202, "{}", reply.text);
        assert_eq!(reply.body["status"], "PROCESSING");
    }
    assert_eq!(fx.review_count(), 1);
    assert_eq!(fx.events("review_requested").len(), 1);
}

// PostAiReviewConcurrencyTest#request_concurrentOnFailedReview_requeuesOnce
#[test]
fn 실패한_리뷰에_동시에_다시_요청해도_요청_횟수는_한_번만_늘고_재요청_로그도_한_건이다() {
    let Some(fx) = fx(vec![failure(FailureType::InvalidOutput)]) else {
        return;
    };
    assert_eq!(fx.request().status, 202);
    fx.run_worker_once();
    assert_eq!(fx.get().body["status"], "FAILED");
    insert_version(&fx.db, fx.admin, 2);
    fx.advance(60);

    let replies = fx.request_concurrently();
    for reply in &replies {
        assert_eq!(reply.status, 202, "{}", reply.text);
        assert_eq!(reply.body["status"], "PROCESSING");
    }
    assert_eq!(fx.review_count(), 1);
    let review = fx.review();
    assert_eq!(review.status, Status::Queued);
    assert_eq!(review.request_count, 2);
    let logs = fx.events("review_requested");
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[1]["created"], false);
}

// ---------------------------------------------------------------------------------------------
// AiReviewWorkerTest: 폴러의 동시 처리 한도, 자리 반납, 종료
// ---------------------------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use super::worker::AiReviewWorker;

/// 열릴 때까지 생성을 붙잡아 두고 동시에 도는 호출 수를 센다.
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    running: AtomicUsize,
    max_running: AtomicUsize,
    started: AtomicUsize,
    panics: bool,
    fails: AtomicBool,
}

impl Gate {
    fn new(panics: bool) -> Arc<Gate> {
        Arc::new(Gate {
            open: Mutex::new(false),
            changed: Condvar::new(),
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            panics,
            fails: AtomicBool::new(false),
        })
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

impl AiReviewGenerator for Gate {
    fn generate(
        &self,
        _request: &super::generator::AiReviewGenerationRequest,
    ) -> Result<AiReviewGeneration, AiReviewGenerationError> {
        if self.panics {
            self.started.fetch_add(1, Ordering::SeqCst);
            panic!("boom");
        }
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(now, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
        drop(open);
        self.running.fetch_sub(1, Ordering::SeqCst);
        if self.fails.load(Ordering::SeqCst) {
            return Err(AiReviewGenerationError::of(FailureType::UpstreamError));
        }
        Ok(success())
    }

    fn supports_model(&self, _model: &str) -> bool {
        true
    }
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("시간 안에 {what} 조건이 맞지 않았다");
}

impl Fx {
    /// 요청 없이 대기 행을 직접 넣는다. 게시물마다 한 행이므로 작성자와 게시물을 새로 만든다.
    fn queue_more(&self, count: usize) {
        let mut client = self.db.client();
        let topic: Uuid = client
            .query_one("SELECT topic_id FROM posts WHERE id = $1", &[&self.post])
            .unwrap()
            .get(0);
        for index in 0..count {
            let user = insert_user(&mut client, &format!("extra{index}@example.com"), "ACTIVE");
            let post = insert_post(&mut client, user, topic, "APPROVED");
            client
                .execute(
                    "INSERT INTO ai_reviews (post_id, user_id, prompt_version_id, status, request_id, \
                     requested_at, last_requested_at) VALUES ($1, $2, $3, 'QUEUED', gen_random_uuid(), \
                     now(), now())",
                    &[&post, &user, &self.version],
                )
                .unwrap();
        }
    }

    fn queued_count(&self) -> i64 {
        self.db
            .client()
            .query_one(
                "SELECT COUNT(*) FROM ai_reviews WHERE status = 'QUEUED'",
                &[],
            )
            .unwrap()
            .get(0)
    }
}

// AiReviewWorkerTest#poll_manyClaimable_neverExceedsConcurrency
#[test]
fn 폴링은_동시_처리_수를_넘지_않고_끝나면_다음_건을_가져간다() {
    let Some(fx) = fx(vec![]) else { return };
    fx.queue_more(3);
    assert_eq!(fx.queued_count(), 3);
    assert_eq!(fx.request().status, 202);
    let gate = Gate::new(false);
    let worker = AiReviewWorker::new(Arc::new(fx.service_with(gate.clone())), 2);

    worker.poll();
    wait_until("두 건이 시작", || {
        gate.started.load(Ordering::SeqCst) == 2
    });
    std::thread::sleep(Duration::from_millis(100));
    // 자리가 없으면 더 가져가지 않는다: 대기 행이 두 건 남아 있다.
    assert_eq!(gate.started.load(Ordering::SeqCst), 2);
    assert_eq!(gate.max_running.load(Ordering::SeqCst), 2);
    assert_eq!(fx.queued_count(), 2);

    gate.release();
    wait_until("자리 반납", || {
        worker.poll();
        gate.started.load(Ordering::SeqCst) == 4
    });
    assert!(gate.max_running.load(Ordering::SeqCst) <= 2);
    assert!(worker.stop(Duration::from_secs(5)));
    assert_eq!(fx.queued_count(), 0);
}

// AiReviewWorkerTest#poll_noClaimable_releasesPermitAndStops
#[test]
fn 가져갈_행이_없으면_자리를_돌려주고_다음_주기에_다시_가져간다() {
    let Some(fx) = fx(vec![]) else { return };
    let gate = Gate::new(false);
    gate.release();
    let worker = AiReviewWorker::new(Arc::new(fx.service_with(gate.clone())), 1);
    worker.poll();
    worker.poll();
    assert_eq!(gate.started.load(Ordering::SeqCst), 0);
    // 자리가 남아 있어야 나중에 생긴 행을 가져간다.
    assert_eq!(fx.request().status, 202);
    worker.poll();
    wait_until("처리", || gate.started.load(Ordering::SeqCst) == 1);
    assert!(worker.stop(Duration::from_secs(5)));
}

// AiReviewWorkerTest#poll_processThrows_releasesPermit
#[test]
fn 처리가_패닉으로_끝나도_자리를_돌려주고_다음_건을_가져간다() {
    let Some(fx) = fx(vec![]) else { return };
    assert_eq!(fx.request().status, 202);
    fx.queue_more(1);
    let gate = Gate::new(true);
    let worker = AiReviewWorker::new(Arc::new(fx.service_with(gate.clone())), 1);
    worker.poll();
    wait_until("첫 건", || gate.started.load(Ordering::SeqCst) == 1);
    wait_until("자리 반납", || {
        worker.poll();
        gate.started.load(Ordering::SeqCst) == 2
    });
    assert!(worker.stop(Duration::from_secs(5)));
    // 생성기의 패닉은 예상 못 한 실패로 확정되어 두 건 모두 끝난다.
    let failed: i64 = fx
        .db
        .client()
        .query_one(
            "SELECT COUNT(*) FROM ai_reviews WHERE status = 'FAILED'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(failed, 2);
}

// AiReviewWorkerTest#poll_claimFails_logsAndReleasesPermit
#[test]
fn 가져오기가_실패해도_예외를_밖으로_던지지_않고_worker_poll_failed를_남긴_뒤_자리를_돌려준다() {
    let Some(fx) = fx(vec![]) else { return };
    assert_eq!(fx.request().status, 202);
    let gate = Gate::new(false);
    gate.release();
    let worker = AiReviewWorker::new(Arc::new(fx.service_with(gate.clone())), 1);
    fx.db
        .client()
        .batch_execute("ALTER TABLE ai_reviews RENAME TO ai_reviews_down")
        .unwrap();
    worker.poll();
    fx.db
        .client()
        .batch_execute("ALTER TABLE ai_reviews_down RENAME TO ai_reviews")
        .unwrap();
    let logs = fx.events("worker_poll_failed");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["level"], "ERROR");
    assert_eq!(logs[0]["type"], "ai_review");
    assert!(logs[0]["errorCode"].is_string());
    assert_eq!(gate.started.load(Ordering::SeqCst), 0);
    // 자리를 돌려줬으므로 다음 주기에 가져간다.
    worker.poll();
    wait_until("처리", || gate.started.load(Ordering::SeqCst) == 1);
    assert!(worker.stop(Duration::from_secs(5)));
}

// AiReviewWorkerTest#poll_afterStop_doesNotClaim
#[test]
fn 종료가_시작되면_새_건을_가져가지_않는다() {
    let Some(fx) = fx(vec![]) else { return };
    assert_eq!(fx.request().status, 202);
    let gate = Gate::new(false);
    gate.release();
    let worker = AiReviewWorker::new(Arc::new(fx.service_with(gate.clone())), 2);
    assert!(worker.stop(Duration::from_millis(200)));
    worker.poll();
    assert_eq!(gate.started.load(Ordering::SeqCst), 0);
    assert_eq!(fx.queued_count(), 1);
}

// AiReviewWorkerTest#stop_processFinishesWithinGrace_doesNotInterrupt / stop_inFlightProcess_signalsStoppingThenInterrupts
// 원본은 유예가 지나면 스레드를 interrupt하지만 블로킹 호출은 끊을 수 없어 기다림만 멈추고, 처리는 종료 신호를 보고 행을 그대로 둔다.
#[test]
fn 종료하면_유예_안에_끝난_처리는_기다리고_끝나지_못하면_기다림만_멈춘다() {
    let Some(fx) = fx(vec![]) else { return };
    assert_eq!(fx.request().status, 202);
    let gate = Gate::new(false);
    gate.fails.store(true, Ordering::SeqCst);
    let worker = AiReviewWorker::new(Arc::new(fx.service_with(gate.clone())), 1);
    worker.poll();
    wait_until("시작", || gate.started.load(Ordering::SeqCst) == 1);

    // 유예 안에 끝나지 못하면 false.
    assert!(!worker.stop(Duration::from_millis(200)));
    // 끝나면 종료로 끊긴 호출은 실패를 쓰지 않고 처리 중으로 남기고 중단 로그를 남긴다.
    gate.release();
    assert!(worker.stop(Duration::from_secs(5)));
    assert_eq!(fx.review().status, Status::Processing);
    assert_eq!(fx.events("review_abandoned").len(), 1);
}

// NotificationServiceTest#createForAiReview_userState_decidesPublishStatus: 탈퇴 회원의 알림은 발행 불필요로 저장한다
// (탈퇴하면 게시물이 보이지 않아 워커 경로로는 닿지 않으므로 알림 생성 함수를 직접 부른다.)
#[test]
fn 탈퇴한_회원의_ai_리뷰_알림은_발행_불필요로_저장한다() {
    use crate::notification::ai_review::{AiReviewOutcome, create_for_ai_review};
    let Some(fx) = fx(vec![]) else { return };
    fx.db
        .client()
        .execute(
            "UPDATE users SET deleted_at = now() WHERE id = $1",
            &[&fx.author],
        )
        .unwrap();
    let (author, post) = (fx.author, fx.post);
    crate::post::support::in_transaction(&fx.pool, |tx| {
        create_for_ai_review(
            tx,
            author,
            post,
            Uuid::new_v4(),
            1,
            AiReviewOutcome::Completed,
            START.parse().unwrap(),
        )
        .map_err(Into::into)
    })
    .unwrap();
    let notes = fx.notifications();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].3, "NOT_REQUIRED");
}
