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
    let mut pairs = vec![
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("REFRESH_TOKEN_INACTIVITY_EXPIRATION", "30d"),
        ("REFRESH_TOKEN_ABSOLUTE_EXPIRATION", "90d"),
        ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
        ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false"),
    ];
    pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
    let env = Env::from_pairs(pairs);
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

fn send(
    h: &Arc<dyn Handler>,
    method: Method,
    path: &str,
    query: &[(&str, &str)],
    bearer: Option<&str>,
) -> Response {
    let mut req = Request::new(method, path);
    req.query = query
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    call(h, req, bearer)
}

// 하네스 시나리오 notification/* 의 핵심 흐름: 목록·상세·미읽음·읽음·모두 읽기
#[test]
fn notification_inbox_flow() {
    let Some(db) = TestDb::create() else { return };
    let admin = "01990000-0000-7000-8000-0000000000a1";
    let (topic, photo, post, event, notification) = (
        "01990000-0000-7000-8000-0000000000f1",
        "01990000-0000-7000-8000-0000000000b1",
        "01990000-0000-7000-8000-0000000000c1",
        "01990000-0000-7000-8000-0000000000d1",
        "01990000-0000-7000-8000-000000000101",
    );
    db.client()
        .batch_execute(&format!(
            "INSERT INTO users (id, email, signature_original_storage_key) VALUES \
               ('{USER}', 'user1@test.chalkak', 'chalkak/signatures/user1/original.png'); \
             INSERT INTO admins (id, username, password) VALUES ('{admin}', 'operator', 'x'); \
             INSERT INTO topics (id, title, topic_date, starts_at, ends_at) VALUES \
               ('{topic}', '주제', '2026-10-12', '2026-10-12T00:00:00+09:00', '2026-10-13T00:00:00+09:00'); \
             INSERT INTO photos (id, original_storage_key, thumbnail_storage_key) VALUES \
               ('{photo}', 'chalkak/posts/p1/original.webp', 'chalkak/posts/p1/thumbnail.webp'); \
             INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status) VALUES \
               ('{post}', '{USER}', '{topic}', '{photo}', 'REJECTED'); \
             INSERT INTO admin_audit_logs (id, actor_admin_id, action, target_type, target_id, reason, \
               before_state, after_state, occurred_at, request_id) VALUES \
               ('{event}', '{admin}', 'POST_REJECTED', 'POST', '{post}', '사진 품질', \
                '{{\"status\":\"PENDING\"}}', '{{\"status\":\"REJECTED\"}}', '2026-10-12T00:00:00Z', gen_random_uuid()); \
             INSERT INTO notifications (id, user_id, post_id, event_key, type, title, body, rejection_reason, created_at) VALUES \
               ('{notification}', '{USER}', '{post}', '{event}', 'POST_REJECTED', '게시물이 반려되었습니다.', \
                '반려 사유를 확인해 주세요.', '사진 품질', '2026-10-12T01:00:00.5Z')"
        ))
        .unwrap();
    let h = handler(&db);
    let t = user_token();
    let base = "/api/v1/notifications";

    assert_eq!(send(&h, Method::Get, base, &[], None).status, 401);
    // 조회 조건 오류는 회원 확인보다 먼저다(@Valid 인자 해석 -> @RequiresExistingUser).
    let r = send(&h, Method::Get, base, &[("pageSize", "0")], Some(&t));
    assert_eq!(
        (r.status, json_body(&r)["message"].as_str().unwrap()),
        (400, "조회 조건이 올바르지 않습니다.")
    );

    let r = send(&h, Method::Get, base, &[], Some(&t));
    assert_eq!(r.status, 200);
    assert_eq!(
        json_body(&r),
        json!({
            "currentPage": 1, "pageSize": 20, "hasNext": false,
            "notifications": [{
                "id": notification, "type": "POST_REJECTED",
                "title": "게시물이 반려되었습니다.", "body": "반려 사유를 확인해 주세요.",
                "thumbnailImageUrl": "https://cdn.test.chalkak/posts/p1/thumbnail.webp",
                "readAt": null, "createdAt": "2026-10-12T01:00:00.500Z"
            }]
        })
    );

    let detail = send(
        &h,
        Method::Get,
        &format!("{base}/{notification}"),
        &[],
        Some(&t),
    );
    assert_eq!(detail.status, 200);
    let d = json_body(&detail);
    assert_eq!(
        d["originalImageUrl"],
        "https://cdn.test.chalkak/posts/p1/original.webp"
    );
    assert_eq!(d["rejectionReason"], "사진 품질");
    assert_eq!(
        send(&h, Method::Get, &format!("{base}/abc"), &[], Some(&t)).status,
        400
    );

    let unread = send(
        &h,
        Method::Get,
        &format!("{base}/unread-status"),
        &[],
        Some(&t),
    );
    assert_eq!(json_body(&unread), json!({"hasUnread": true}));

    let r = send(
        &h,
        Method::Patch,
        &format!("{base}/{notification}/read"),
        &[],
        Some(&t),
    );
    assert_eq!((r.status, r.body.len()), (204, 0));
    let unread = send(
        &h,
        Method::Get,
        &format!("{base}/unread-status"),
        &[],
        Some(&t),
    );
    assert_eq!(json_body(&unread), json!({"hasUnread": false}));
    let r = send(
        &h,
        Method::Get,
        &format!("{base}/{notification}"),
        &[],
        Some(&t),
    );
    assert_eq!(json_body(&r)["readAt"], NOW);

    // 게시물이 삭제되면 알림은 남지만 보이지 않고 읽음 처리도 404다.
    db.client()
        .batch_execute(&format!(
            "UPDATE posts SET deleted_at = CURRENT_TIMESTAMP WHERE id = '{post}'"
        ))
        .unwrap();
    let r = send(&h, Method::Get, base, &[], Some(&t));
    assert_eq!(json_body(&r)["notifications"], json!([]));
    let r = send(
        &h,
        Method::Patch,
        &format!("{base}/{notification}/read"),
        &[],
        Some(&t),
    );
    assert_eq!(r.status, 404);
    let r = send(
        &h,
        Method::Patch,
        &format!("{base}/read-all"),
        &[],
        Some(&t),
    );
    assert_eq!(r.status, 204);
    assert_eq!(send(&h, Method::Delete, base, &[], Some(&t)).status, 405);
}
