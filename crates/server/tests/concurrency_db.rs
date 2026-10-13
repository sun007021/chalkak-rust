//! 전체 핸들러(라우트 + 보안 체인 + 도메인 + DB)에 동시 요청을 보내는 시험. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 근거: 전환 계획 4.4 동시성(좋아요 중복, 같은 게시물의 검수·수정 경합), 4.5 장시간·부하 중 커넥션 풀 고갈.
//! 실행 중인 서버(Java와 Rust)에 같은 시험을 거는 쪽은 `harness/loadgen`의 `race` 명령이다. 여기서는 CI에서
//! 프로세스 안에서 같은 불변식을 지키는지 본다.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use chalkak_app::testing::TestDb;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock};
use chalkak_db::Pool;
use chalkak_http::{Handler, Method, Request, Response};
use chalkak_server::build_handler;
use chalkak_server::settings::Settings;
use serde_json::json;

const SECRET_HEX: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const NOW: &str = "2026-10-12T03:00:00Z"; // KST 2026-10-12 12:00
const NOW_SECONDS: i64 = 1_791_774_000;
const ADMIN: &str = "01990000-0000-7000-8000-0000000000a1";
const TOPIC: &str = "01990000-0000-7000-8000-0000000000f1";

fn user_id(n: u32) -> String {
    format!("01990000-0000-7000-8000-{n:012x}")
}

fn post_id(n: u32) -> String {
    format!("01990000-0000-7000-8000-{:012x}", 0x300 + n)
}

fn handler_with(pool: Pool) -> Arc<dyn Handler> {
    let mut pairs = vec![
        ("ACCESS_TOKEN_SECRET", SECRET_HEX),
        ("ACCESS_TOKEN_EXPIRATION", "15m"),
        ("REFRESH_TOKEN_INACTIVITY_EXPIRATION", "30d"),
        ("REFRESH_TOKEN_ABSOLUTE_EXPIRATION", "90d"),
        ("CLOUDFRONT_ORIGIN_PATH", "/chalkak"),
        ("ADMIN_DEVELOPMENT_BYPASS_ENABLED", "false"),
    ];
    pairs.extend_from_slice(&chalkak_app::config::TEST_ENV);
    let settings = Settings::from_env(&Env::from_pairs(pairs)).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(NOW.parse().unwrap()));
    let logger = Arc::new(Logger::new(
        Box::new(std::io::sink()),
        Level::Info,
        Arc::clone(&clock),
    ));
    build_handler(&settings, pool, clock, logger)
}

fn token(subject: &str, scope: &str) -> String {
    let key = chalkak_jwt::decode_hex(SECRET_HEX).unwrap();
    chalkak_jwt::hs256_encode(
        &json!({"typ":"JWT","alg":"HS256"}),
        &json!({
            "iss": "chalkak-backend", "aud": ["chalkak-access"], "sub": subject,
            "iat": NOW_SECONDS - 5, "exp": NOW_SECONDS + 900, "jti": "j",
            "purpose": "ACCESS", "scope": scope,
        }),
        &key,
    )
}

fn request(method: Method, path: &str, bearer: &str, body: Option<&str>) -> Request {
    let mut req = Request::new(method, path);
    req.headers
        .append("Authorization", format!("Bearer {bearer}"));
    if let Some(b) = body {
        req.headers.append("Content-Type", "application/json");
        req.body = b.as_bytes().to_vec();
    }
    req
}

/// 요청들을 같은 순간에 시작해 응답을 순서대로 돌려준다.
fn race(h: &Arc<dyn Handler>, requests: Vec<Request>) -> Vec<Response> {
    let barrier = Arc::new(Barrier::new(requests.len()));
    let handles: Vec<_> = requests
        .into_iter()
        .map(|req| {
            let (h, b) = (Arc::clone(h), Arc::clone(&barrier));
            thread::spawn(move || {
                b.wait();
                h.handle(req)
            })
        })
        .collect();
    handles.into_iter().map(|j| j.join().unwrap()).collect()
}

/// 회원 `users`명, 오늘 주제, 승인된 게시물 하나(작성자 1번), 관리자 하나.
fn seed(db: &TestDb, users: u32) {
    let mut sql = String::new();
    for n in 1..=users {
        sql.push_str(&format!(
            "INSERT INTO users (id, email, signature_original_storage_key) VALUES \
             ('{}', 'u{n}@test.chalkak', 'chalkak/signatures/u{n}/original.png');",
            user_id(n)
        ));
    }
    sql.push_str(&format!(
        "INSERT INTO admins (id, username, password) VALUES ('{ADMIN}', 'operator', 'x'); \
         INSERT INTO topics (id, title, topic_date, starts_at, ends_at) VALUES \
           ('{TOPIC}', '주제', '2026-10-12', '2026-10-12T00:00:00+09:00', '2026-10-13T00:00:00+09:00'); \
         INSERT INTO photos (id, original_storage_key, thumbnail_storage_key) VALUES \
           ('01990000-0000-7000-8000-0000000000b1', 'chalkak/posts/p1/original.webp', 'chalkak/posts/p1/thumbnail.webp'), \
           ('01990000-0000-7000-8000-0000000000b2', 'chalkak/posts/p2/original.webp', 'chalkak/posts/p2/thumbnail.webp'); \
         INSERT INTO posts (id, user_id, topic_id, photo_id, title, moderation_status, moderated_at) VALUES \
           ('{}', '{}', '{TOPIC}', '01990000-0000-7000-8000-0000000000b1', '글', 'APPROVED', now());",
        post_id(1),
        user_id(1)
    ));
    db.client().batch_execute(&sql).unwrap();
}

/// 검수 대기 게시물 하나를 더 만든다(작성자 2번).
fn add_pending_post(db: &TestDb) -> String {
    let id = post_id(2);
    db.client()
        .batch_execute(&format!(
            "INSERT INTO posts (id, user_id, topic_id, photo_id, title, moderation_status) VALUES \
             ('{id}', '{}', '{TOPIC}', '01990000-0000-7000-8000-0000000000b2', '대기', 'PENDING')",
            user_id(2)
        ))
        .unwrap();
    id
}

fn count(db: &TestDb, query: &str) -> i64 {
    db.client().query_one(query, &[]).unwrap().get(0)
}

// 하네스 시나리오 like/like-unlike의 멱등 동작을 동시에 호출해도 유지: (게시물, 회원) 유일 제약.
#[test]
fn concurrent_likes_by_one_user_all_succeed_and_leave_one_row() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let h = handler_with(db.pool(8));
    let path = format!("/api/v1/posts/{}/likes", post_id(1));
    let t = token(&user_id(3), "USER");
    let responses = race(
        &h,
        (0..16)
            .map(|_| request(Method::Put, &path, &t, None))
            .collect(),
    );
    for r in &responses {
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    }
    assert_eq!(count(&db, "SELECT count(*) FROM post_likes"), 1);
}

#[test]
fn concurrent_likes_by_many_users_are_all_counted() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 24);
    let h = handler_with(db.pool(8));
    let path = format!("/api/v1/posts/{}/likes", post_id(1));
    let responses = race(
        &h,
        (1..=24)
            .map(|n| request(Method::Put, &path, &token(&user_id(n), "USER"), None))
            .collect(),
    );
    assert!(responses.iter().all(|r| r.status == 200));
    assert_eq!(count(&db, "SELECT count(*) FROM post_likes"), 24);
    // 응답의 좋아요 수는 단조롭게 이어지고 마지막 조회가 24다.
    let read = h.handle(request(
        Method::Get,
        &format!("/api/v1/posts/{}", post_id(1)),
        &token(&user_id(1), "USER"),
        None,
    ));
    assert_eq!(read.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&read.body).unwrap();
    assert_eq!(body["likeCount"], 24);
}

#[test]
fn concurrent_like_and_unlike_by_one_user_end_in_a_valid_state() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let h = handler_with(db.pool(8));
    let path = format!("/api/v1/posts/{}/likes", post_id(1));
    let t = token(&user_id(3), "USER");
    let responses = race(
        &h,
        (0..20)
            .map(|i| {
                let method = if i % 2 == 0 {
                    Method::Put
                } else {
                    Method::Delete
                };
                request(method, &path, &t, None)
            })
            .collect(),
    );
    assert!(responses.iter().all(|r| r.status == 200 || r.status == 204));
    let rows = count(&db, "SELECT count(*) FROM post_likes");
    assert!((0..=1).contains(&rows), "{rows}");
}

// AdminPostService#moderate: PENDING에서만 처리한다. 같은 게시물에 승인을 동시에 여러 번 보내면 한 번만 반영되고
// 나머지는 RESOURCE_STATE_CHANGED(400)다. 감사 로그와 알림도 한 건씩.
#[test]
fn concurrent_approvals_apply_once_with_one_audit_log_and_one_notification() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let pending = add_pending_post(&db);
    let h = handler_with(db.pool(8));
    let path = format!("/api/v1/admin/posts/{pending}/moderation");
    let admin = token(ADMIN, "ADMIN");
    let responses = race(
        &h,
        (0..8)
            .map(|_| request(Method::Put, &path, &admin, Some(r#"{"status":"APPROVED"}"#)))
            .collect(),
    );
    let ok = responses.iter().filter(|r| r.status == 200).count();
    let rejected = responses.iter().filter(|r| r.status == 400).count();
    assert_eq!(
        (ok, rejected),
        (1, 7),
        "{:?}",
        responses.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    assert_eq!(count(&db, "SELECT count(*) FROM admin_audit_logs"), 1);
    assert_eq!(count(&db, "SELECT count(*) FROM notifications"), 1);
    let status: String = db
        .client()
        .query_one(
            &format!("SELECT moderation_status::text FROM posts WHERE id = '{pending}'"),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(status, "APPROVED");
}

#[test]
fn concurrent_approve_and_reject_end_in_one_consistent_outcome() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let pending = add_pending_post(&db);
    let h = handler_with(db.pool(8));
    let path = format!("/api/v1/admin/posts/{pending}/moderation");
    let admin = token(ADMIN, "ADMIN");
    let mut requests = Vec::new();
    for _ in 0..4 {
        requests.push(request(
            Method::Put,
            &path,
            &admin,
            Some(r#"{"status":"APPROVED"}"#),
        ));
        requests.push(request(
            Method::Put,
            &path,
            &admin,
            Some(r#"{"status":"REJECTED","rejectionReason":"부적절"}"#),
        ));
    }
    let responses = race(&h, requests);
    assert_eq!(responses.iter().filter(|r| r.status == 200).count(), 1);
    let status: String = db
        .client()
        .query_one(
            &format!("SELECT moderation_status::text FROM posts WHERE id = '{pending}'"),
            &[],
        )
        .unwrap()
        .get(0);
    let action: String = db
        .client()
        .query_one("SELECT action::text FROM admin_audit_logs", &[])
        .unwrap()
        .get(0);
    let notification: String = db
        .client()
        .query_one("SELECT type FROM notifications", &[])
        .unwrap()
        .get(0);
    let expected = if status == "APPROVED" {
        "POST_APPROVED"
    } else {
        "POST_REJECTED"
    };
    assert_eq!(
        (action.as_str(), notification.as_str()),
        (expected, expected)
    );
}

#[test]
fn concurrent_owner_updates_and_moderation_leave_the_post_approved_with_one_audit_log() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let pending = add_pending_post(&db);
    let h = handler_with(db.pool(8));
    let admin = token(ADMIN, "ADMIN");
    let owner = token(&user_id(2), "USER");
    let mut requests = vec![request(
        Method::Put,
        &format!("/api/v1/admin/posts/{pending}/moderation"),
        &admin,
        Some(r#"{"status":"APPROVED"}"#),
    )];
    for i in 0..6 {
        requests.push(request(
            Method::Put,
            &format!("/api/v1/posts/{pending}"),
            &owner,
            Some(&format!(r#"{{"title":"변경{i}"}}"#)),
        ));
    }
    let responses = race(&h, requests);
    assert_eq!(responses[0].status, 200, "검수는 성공한다");
    assert!(responses.iter().all(|r| r.status < 500));
    let status: String = db
        .client()
        .query_one(
            &format!("SELECT moderation_status::text FROM posts WHERE id = '{pending}'"),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(status, "APPROVED");
    assert_eq!(count(&db, "SELECT count(*) FROM admin_audit_logs"), 1);
}

fn topic_request(bearer: &str) -> Request {
    let mut req = request(Method::Get, "/api/v1/topics", bearer, None);
    req.query = vec![("date".into(), "2026-10-12".into())];
    req
}

// HikariCP 기본 동작: 풀이 비면 connectionTimeout까지 기다리고, 넘으면 오류 응답(500)으로 끝낸다.
// 연결을 돌려주면 바로 회복하고, 실패한 요청이 연결을 새게 하지 않는다.
#[test]
fn pool_exhaustion_times_out_waiters_with_500_and_recovers_without_leaking() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let mut config = db.config.clone();
    config.connection_timeout = Duration::from_millis(300);
    let pool = Pool::connect(&config, 2).unwrap();
    let h = handler_with(pool.clone());
    let t = token(&user_id(1), "USER");

    // 연결 두 개를 모두 붙잡는다.
    let held = (pool.get().unwrap(), pool.get().unwrap());
    assert_eq!(pool.stats().idle, 0);

    let started = Instant::now();
    let responses = race(&h, (0..4).map(|_| topic_request(&t)).collect());
    for r in &responses {
        assert_eq!(r.status, 500, "{}", String::from_utf8_lossy(&r.body));
    }
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(5),
        "대기 한도(300ms) 뒤에 실패해야 한다: {waited:?}"
    );
    // 헬스체크도 DB를 못 만나면 DOWN이다(배포 스크립트가 보는 값).
    let health = h.handle(Request::new(Method::Get, "/actuator/health"));
    assert_eq!(health.status, 503);

    drop(held);
    assert_eq!(pool.stats().waiting, 0, "대기 중인 요청이 남지 않는다");
    let r = h.handle(topic_request(&t));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        h.handle(Request::new(Method::Get, "/actuator/health"))
            .status,
        200
    );
    let stats = pool.stats();
    assert_eq!(
        (stats.total, stats.active),
        (2, 0),
        "연결이 새지 않는다: {stats:?}"
    );
}

#[test]
fn pool_waiters_are_served_once_connections_come_back() {
    let Some(db) = TestDb::create() else { return };
    seed(&db, 3);
    let pool = db.pool(2); // 대기 한도 30초
    let h = handler_with(pool.clone());
    let t = token(&user_id(1), "USER");
    let held = (pool.get().unwrap(), pool.get().unwrap());

    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let started = Instant::now();
    let responses = race(&h, (0..8).map(|_| topic_request(&t)).collect());
    releaser.join().unwrap();
    assert!(responses.iter().all(|r| r.status == 200));
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "풀이 풀릴 때까지 기다렸다가 처리"
    );
    let stats = pool.stats();
    assert_eq!(
        (stats.total, stats.active, stats.waiting),
        (2, 0, 0),
        "{stats:?}"
    );
}
