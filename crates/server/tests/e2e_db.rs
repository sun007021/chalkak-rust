//! 실제 도메인 라우트와 DB를 붙인 전체 핸들러 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//! 같은 동작을 실행 중인 서버에 대해 `harness/golden`이 다시 확인한다.

use std::sync::Arc;

use chalkak_app::testing::TestDb;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock};
use chalkak_http::{Handler, Method, Request, Response};
use chalkak_server::build_handler;
use chalkak_server::settings::Settings;
use serde_json::{Value, json};

const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const NOW: &str = "2026-10-12T03:00:00Z"; // KST 2026-10-12 12:00
const NOW_SECONDS: i64 = 1_791_774_000;
const USER: &str = "01990000-0000-7000-8000-000000000001";

fn handler(db: &TestDb) -> Arc<dyn Handler> {
    let env = Env::from_pairs([
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("REFRESH_TOKEN_INACTIVITY_EXPIRATION", "30d"),
        ("REFRESH_TOKEN_ABSOLUTE_EXPIRATION", "90d"),
        ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
        ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false"),
        ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
        (
            "IMAGE_PROCESSOR_CALLBACK_SECRET",
            "test-callback-secret-with-enough-length",
        ),
        ("CALLBACK_MAX_BODY_BYTES", "65536"),
        ("POST_IMAGE_PROCESSING_TIMEOUT", "7m"),
    ]);
    let settings = Settings::from_env(&env).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let logger = Arc::new(Logger::new(
        Box::new(std::io::sink()),
        Level::Info,
        Arc::clone(&clock),
    ));
    build_handler(&settings, db.pool(4), clock, logger)
}

fn user_token() -> String {
    let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
    chalkak_jwt::hs256_encode(
        &json!({"typ":"JWT","alg":"HS256"}),
        &json!({
            "iss": "chalkak-backend", "aud": ["chalkak-access"], "sub": USER,
            "iat": NOW_SECONDS - 5, "exp": NOW_SECONDS + 900, "jti": "j",
            "purpose": "ACCESS", "scope": "USER",
        }),
        &key,
    )
}

fn call(h: &Arc<dyn Handler>, mut req: Request, bearer: Option<&str>) -> Response {
    if let Some(t) = bearer {
        req.headers.append("Authorization", format!("Bearer {t}"));
    }
    h.handle(req)
}

fn get_topic(h: &Arc<dyn Handler>, date: &str, bearer: Option<&str>) -> Response {
    let mut req = Request::new(Method::Get, "/api/v1/topics");
    req.query = vec![("date".into(), date.into())];
    call(h, req, bearer)
}

fn json_body(r: &Response) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}

// SecurityFilterChainTest#topics_withoutToken_reachesController + TopicServiceTest 합성
#[test]
fn topic_is_readable_without_token_and_phase_follows_the_clock() {
    let Some(db) = TestDb::create() else { return };
    db.client()
        .batch_execute(
            "INSERT INTO topics (id, title, topic_date, starts_at, ends_at, created_at, updated_at) VALUES \
             ('0198f6c1-62ba-7d30-8b12-0f733b6570b2', '가을 하늘', '2026-10-12', \
              '2026-10-11T15:00:00Z', '2026-10-12T15:00:00Z', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        )
        .unwrap();
    let h = handler(&db);

    for bearer in [None, Some(user_token())] {
        let r = get_topic(&h, "2026-10-12", bearer.as_deref());
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("content-type"), Some("application/json"));
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"id":"0198f6c1-62ba-7d30-8b12-0f733b6570b2","title":"가을 하늘","topicDate":"2026-10-12","startsAt":"2026-10-11T15:00:00Z","endsAt":"2026-10-12T15:00:00Z","phase":"OPEN"}"#
        );
    }
    assert_eq!(
        get_topic(&h, "2026-10-13", None).status,
        400,
        "KST 기준 미래"
    );
    assert_eq!(get_topic(&h, "2026-10-11", None).status, 404);
    assert_eq!(get_topic(&h, "2026-13-99", None).status, 400);
}

#[test]
fn health_reports_up_with_a_live_database() {
    let Some(db) = TestDb::create() else { return };
    let h = handler(&db);
    let r = call(&h, Request::new(Method::Get, "/actuator/health"), None);
    assert_eq!(r.status, 200);
    assert_eq!(json_body(&r)["status"], "UP");
}

// 하네스 시나리오 platform/request-body-errors
#[test]
fn feedback_flow_validates_body_then_user_then_stores() {
    let Some(db) = TestDb::create() else { return };
    db.client()
        .batch_execute(&format!(
            "INSERT INTO users (id, email, signature_original_storage_key) VALUES \
             ('{USER}', 'user1@test.chalkak', 'chalkak/signatures/user1/original.png')"
        ))
        .unwrap();
    let h = handler(&db);
    let post = |body: &str, content_type: Option<&str>, bearer: Option<&str>| {
        let mut req = Request::new(Method::Post, "/api/v1/feedbacks");
        if let Some(ct) = content_type {
            req.headers.append("Content-Type", ct);
        }
        req.body = body.as_bytes().to_vec();
        call(&h, req, bearer)
    };
    let t = user_token();
    let json_type = Some("application/json");

    assert_eq!(
        post("{}", json_type, None).status,
        401,
        "토큰 없이는 필터에서 401"
    );
    assert_eq!(
        json_body(&post("{}", json_type, Some(&t)))["message"],
        "피드백 내용이 필요합니다."
    );
    assert_eq!(post("{\"content\": ", json_type, Some(&t)).status, 400);
    assert_eq!(
        post("", None, Some(&t)).status,
        400,
        "Content-Type 없는 빈 본문"
    );
    assert_eq!(
        post("{\"content\":\"x\"}", Some("text/plain"), Some(&t)).status,
        415
    );

    let r = post(r#"{"content": 123, "unknown": true}"#, json_type, Some(&t));
    assert_eq!(r.status, 201);
    let b = json_body(&r);
    assert_eq!(b["createdAt"], "2026-10-12T03:00:00Z");
    let stored: String = db
        .client()
        .query_one("SELECT content FROM feedbacks", &[])
        .unwrap()
        .get(0);
    assert_eq!(stored, "123");

    // 존재하지 않는 회원의 토큰은 본문 검증을 통과한 뒤 401로 끝난다.
    db.client()
        .batch_execute("DELETE FROM feedbacks; DELETE FROM users")
        .unwrap();
    let r = post(r#"{"content":"좋아요"}"#, json_type, Some(&t));
    assert_eq!(r.status, 401);
    assert!(!r.headers.contains("www-authenticate"), "컨트롤러가 낸 401");
}
