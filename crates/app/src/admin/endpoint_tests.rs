//! 관리자 API를 라우터까지 통과시켜 보는 엔드포인트 테스트.
//!
//! 원본의 `@WebMvcTest` 컨트롤러 테스트에 대응한다. 서비스를 목으로 바꾸지 않고 실제 DB로 끝까지 돌리므로
//! 원본 테스트가 목 서비스로 확인하던 것(요청 변환·검증 응답·상태 코드)과 서비스가 답한 결과를 함께 본다.

use chalkak_http::{Handler, Method, Request, Response, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use super::support::AdminDeps;
use super::testing::{
    deps, insert_admin, insert_admin_with_hash, insert_post, insert_topic, insert_user,
};
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::Routes;

struct Api {
    router: Router,
    admin: Uuid,
    deps: AdminDeps,
    db: TestDb,
}

fn api() -> Option<Api> {
    let db = TestDb::create()?;
    let deps = deps(&db);
    let admin = insert_admin(&mut db.client(), "operator");
    let routes = super::register_with(Routes::new(deps.logger.clone()), &deps);
    Some(Api {
        router: routes.into_router(),
        admin,
        deps,
        db,
    })
}

struct Reply {
    status: u16,
    body: Value,
    raw: Response,
}

impl Api {
    fn principal(&self) -> Principal {
        Principal {
            subject: self.admin.hyphenated().to_string(),
            authorities: vec![AUTHORITY_ADMIN.to_owned()],
        }
    }

    fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
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
        let body = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        Reply {
            status: response.status,
            body,
            raw: response,
        }
    }

    fn as_admin(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let _guard = security::enter(self.principal());
        self.send(method, path, query, body)
    }

    fn get(&self, path: &str, query: &[(&str, &str)]) -> Reply {
        self.as_admin(Method::Get, path, query, None)
    }
}

fn id() -> String {
    Uuid::new_v4().hyphenated().to_string()
}

// AdminArgumentResolverFailClosedWebMvcTest, CurrentAdminArgumentResolverTest
#[test]
fn 인증_주체가_없으면_요청_값으로_관리자를_꾸밀_수_없다() {
    let Some(api) = api() else { return };
    let mut req = Request::new(Method::Get, "/api/v1/admin/auth/me");
    req.headers
        .append("X-Admin-Id", api.admin.hyphenated().to_string());
    let response = api.router.handle(req);
    assert_eq!(response.status, 401);
    let forbidden = {
        let _guard = security::enter(Principal {
            subject: api.admin.hyphenated().to_string(),
            authorities: vec!["SCOPE_USER".to_owned()],
        });
        api.send(Method::Get, "/api/v1/admin/auth/me", &[], None)
    };
    assert_eq!(forbidden.status, 403);
}

// AdminAuthControllerTest
#[test]
fn 로그인은_토큰을_함께_주고_검증_오류는_400이다() {
    let Some(api) = api() else { return };
    let hash = bcrypt::hash_with_result("operator-password", 4)
        .unwrap()
        .format_for_version(bcrypt::Version::TwoY);
    insert_admin_with_hash(&mut api.db.client(), "login-admin", &hash);
    let ok = api.send(
        Method::Post,
        "/api/v1/admin/auth/login",
        &[],
        Some(json!({"username": "login-admin", "password": "operator-password"})),
    );
    assert_eq!(ok.status, 200);
    assert!(ok.body["accessToken"].as_str().is_some());
    assert!(ok.body["refreshToken"].as_str().is_some());
    assert_eq!(ok.body["username"], "login-admin");

    let blank = api.send(
        Method::Post,
        "/api/v1/admin/auth/login",
        &[],
        Some(json!({"username": " ", "password": "x"})),
    );
    assert_eq!(blank.status, 400);
    assert_eq!(blank.body["message"], "관리자 아이디를 입력해 주세요.");
}

// AdminAuthControllerTest
#[test]
fn 재발급은_회전하고_쓸_수_없는_토큰은_재로그인을_알린다() {
    let Some(api) = api() else { return };
    let hash = bcrypt::hash_with_result("pw-for-rotate", 4)
        .unwrap()
        .format_for_version(bcrypt::Version::TwoY);
    insert_admin_with_hash(&mut api.db.client(), "rotate-admin", &hash);
    let login = api.send(
        Method::Post,
        "/api/v1/admin/auth/login",
        &[],
        Some(json!({"username": "rotate-admin", "password": "pw-for-rotate"})),
    );
    let first = login.body["refreshToken"].as_str().unwrap().to_owned();
    let rotated = api.send(
        Method::Post,
        "/api/v1/admin/auth/refresh",
        &[],
        Some(json!({"refreshToken": first})),
    );
    assert_eq!(rotated.status, 200);
    assert_ne!(rotated.body["refreshToken"], json!(first));

    let reused = api.send(
        Method::Post,
        "/api/v1/admin/auth/refresh",
        &[],
        Some(json!({"refreshToken": first})),
    );
    assert_eq!(reused.status, 401);
    assert_eq!(reused.body["errorCode"], "REAUTHENTICATION_REQUIRED");

    let blank = api.send(
        Method::Post,
        "/api/v1/admin/auth/refresh",
        &[],
        Some(json!({"refreshToken": "  "})),
    );
    assert_eq!(blank.status, 400);
}

// AdminAuthControllerTest
#[test]
fn 로그아웃은_액세스_토큰_없이_204이고_빈_토큰은_400이다() {
    let Some(api) = api() else { return };
    let hash = bcrypt::hash_with_result("pw-logout", 4)
        .unwrap()
        .format_for_version(bcrypt::Version::TwoY);
    insert_admin_with_hash(&mut api.db.client(), "logout-admin", &hash);
    let login = api.send(
        Method::Post,
        "/api/v1/admin/auth/login",
        &[],
        Some(json!({"username": "logout-admin", "password": "pw-logout"})),
    );
    let token = login.body["refreshToken"].as_str().unwrap().to_owned();
    let out = api.send(
        Method::Post,
        "/api/v1/admin/auth/logout",
        &[],
        Some(json!({"refreshToken": token})),
    );
    assert_eq!(out.status, 204);
    let after = api.send(
        Method::Post,
        "/api/v1/admin/auth/refresh",
        &[],
        Some(json!({"refreshToken": token})),
    );
    assert_eq!(after.status, 401);
    let blank = api.send(
        Method::Post,
        "/api/v1/admin/auth/logout",
        &[],
        Some(json!({})),
    );
    assert_eq!(blank.status, 400);
}

// AdminAuthControllerTest
#[test]
fn 인증된_관리자는_자신의_정보를_조회한다() {
    let Some(api) = api() else { return };
    let me = api.get("/api/v1/admin/auth/me", &[]);
    assert_eq!(me.status, 200);
    assert_eq!(me.body["username"], "operator");
    assert_eq!(me.body["adminId"], api.admin.hyphenated().to_string());
}

// AdminUserControllerTest
#[test]
fn 사용자_목록은_상태를_생략하면_활성_사용자를_준다() {
    let Some(api) = api() else { return };
    insert_user(&mut api.db.client(), "a@example.com", "ACTIVE");
    insert_user(&mut api.db.client(), "b@example.com", "BANNED");
    let list = api.get("/api/v1/admin/users", &[]);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["users"].as_array().unwrap().len(), 1);
    assert_eq!(list.body["currentPage"], 1);
    let filtered = api.get(
        "/api/v1/admin/users",
        &[("status", "BANNED"), ("email", "B@EXA"), ("pageSize", "5")],
    );
    assert_eq!(filtered.body["users"][0]["email"], "b@example.com");
    assert_eq!(filtered.body["pageSize"], 5);
    let bad = api.get("/api/v1/admin/users", &[("pageSize", "101")]);
    assert_eq!(bad.status, 400);
    let unknown = api.get("/api/v1/admin/users", &[("status", "WITHDRAWN_X")]);
    assert_eq!(unknown.status, 400);
}

// AdminUserControllerTest
#[test]
fn 사용자_상세는_없으면_404이고_상태_변경은_차단_결과를_준다() {
    let Some(api) = api() else { return };
    let missing = api.get(&format!("/api/v1/admin/users/{}", id()), &[]);
    assert_eq!(missing.status, 404);
    let user = insert_user(&mut api.db.client(), "c@example.com", "ACTIVE");
    let detail = api.get(&format!("/api/v1/admin/users/{user}"), &[]);
    assert_eq!(detail.status, 200);
    assert_eq!(detail.body["postCounts"]["pending"], 0);

    let path = format!("/api/v1/admin/users/{user}/status");
    let blank = api.as_admin(
        Method::Patch,
        &path,
        &[],
        Some(json!({"status": "BANNED", "reason": " "})),
    );
    assert_eq!(blank.status, 400);
    let banned = api.as_admin(
        Method::Patch,
        &path,
        &[],
        Some(json!({"status": "BANNED", "reason": "스팸"})),
    );
    assert_eq!(banned.status, 200);
    assert_eq!(banned.body["status"], "BANNED");
    let same = api.as_admin(
        Method::Patch,
        &path,
        &[],
        Some(json!({"status": "BANNED", "reason": "again"})),
    );
    assert_eq!(same.status, 400);
    assert_eq!(same.body["errorCode"], "RESOURCE_STATE_CHANGED");
    let withdrawn = api.as_admin(
        Method::Patch,
        &path,
        &[],
        Some(json!({"status": "WITHDRAWN", "reason": "x"})),
    );
    assert_eq!(withdrawn.status, 400);
}

struct Seeded {
    topic: Uuid,
    user: Uuid,
}

fn seeded(api: &Api) -> Seeded {
    let mut client = api.db.client();
    let topic = insert_topic(
        &mut client,
        "오늘의 주제",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let user = insert_user(&mut client, "author@example.com", "ACTIVE");
    Seeded { topic, user }
}

fn post_of(api: &Api, s: &Seeded, status: &str) -> Uuid {
    let mut client = api.db.client();
    let email = format!("{}@example.com", Uuid::new_v4());
    let user = insert_user(&mut client, &email, "ACTIVE");
    let _ = s.user;
    insert_post(&mut client, user, s.topic, status)
}

// AdminPostControllerTest
#[test]
fn 게시물_목록은_기본_조건으로_읽고_잘못된_조건은_400이다() {
    let Some(api) = api() else { return };
    let s = seeded(&api);
    post_of(&api, &s, "PENDING");
    post_of(&api, &s, "VALIDATING");
    let list = api.get("/api/v1/admin/posts", &[]);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["posts"].as_array().unwrap().len(), 1);
    let boundary_low = api.get("/api/v1/admin/posts", &[("pageSize", "1")]);
    let boundary_high = api.get("/api/v1/admin/posts", &[("pageSize", "100")]);
    assert_eq!((boundary_low.status, boundary_high.status), (200, 200));
    for query in [
        [("pageSize", "0")],
        [("pageSize", "101")],
        [("page", "0")],
        [("status", "VALIDATING")],
        [("topicId", "not-a-uuid")],
        [("createdAtFrom", "2026-10-12")],
        [("topicDate", "20261012")],
    ] {
        assert_eq!(
            api.get("/api/v1/admin/posts", &query).status,
            400,
            "{query:?}"
        );
    }
    let filtered = api.get(
        "/api/v1/admin/posts",
        &[
            ("status", "PENDING"),
            ("topicId", &s.topic.to_string()),
            ("topicDate", "2026-10-12"),
            ("sort", "createdAtAsc"),
        ],
    );
    assert_eq!(filtered.status, 200);
    assert_eq!(filtered.body["posts"].as_array().unwrap().len(), 1);
}

// AdminPostControllerTest
#[test]
fn 게시물_상세는_없으면_404이고_형식이_틀리면_400이다() {
    let Some(api) = api() else { return };
    let s = seeded(&api);
    let post = post_of(&api, &s, "PENDING");
    let detail = api.get(&format!("/api/v1/admin/posts/{post}"), &[]);
    assert_eq!(detail.status, 200);
    assert_eq!(detail.body["moderationStatus"], "PENDING");
    assert!(detail.body["moderatedAt"].is_null());
    assert!(detail.body["deletedAt"].is_null());
    assert_eq!(
        api.get(&format!("/api/v1/admin/posts/{}", id()), &[])
            .status,
        404
    );
    assert_eq!(api.get("/api/v1/admin/posts/xyz", &[]).status, 400);
}

// AdminPostControllerTest
#[test]
fn 검수는_승인과_거절을_처리하고_잘못된_요청은_400이다() {
    let Some(api) = api() else { return };
    let s = seeded(&api);
    let approve_target = post_of(&api, &s, "PENDING");
    let reject_target = post_of(&api, &s, "PENDING");
    let put = |post: Uuid, body: Value| {
        api.as_admin(
            Method::Put,
            &format!("/api/v1/admin/posts/{post}/moderation"),
            &[],
            Some(body),
        )
    };
    assert_eq!(
        put(
            approve_target,
            json!({"status": "APPROVED", "rejectionReason": "x"})
        )
        .status,
        400
    );
    assert_eq!(
        put(approve_target, json!({"status": "PENDING"})).status,
        400
    );
    assert_eq!(
        put(approve_target, json!({"status": "VALIDATING"})).status,
        400
    );
    assert_eq!(put(approve_target, json!({})).status, 400);
    assert_eq!(
        put(reject_target, json!({"status": "REJECTED"})).status,
        400
    );
    assert_eq!(
        put(
            reject_target,
            json!({"status": "REJECTED", "rejectionReason": " "})
        )
        .status,
        400
    );
    assert_eq!(
        put(
            reject_target,
            json!({"status": "REJECTED", "rejectionReason": "가".repeat(501)})
        )
        .status,
        400
    );
    let empty = api.as_admin(
        Method::Put,
        &format!("/api/v1/admin/posts/{approve_target}/moderation"),
        &[],
        None,
    );
    assert_eq!(empty.status, 400);

    let approved = put(approve_target, json!({"status": "APPROVED"}));
    assert_eq!(approved.status, 200);
    assert_eq!(approved.body["moderationStatus"], "APPROVED");
    let changed = put(
        approve_target,
        json!({"status": "REJECTED", "rejectionReason": "늦음"}),
    );
    assert_eq!(changed.status, 400);
    assert_eq!(changed.body["errorCode"], "RESOURCE_STATE_CHANGED");
    let rejected = put(
        reject_target,
        json!({"status": "REJECTED", "rejectionReason": "가".repeat(500)}),
    );
    assert_eq!(rejected.status, 200);
    assert_eq!(
        put(Uuid::new_v4(), json!({"status": "APPROVED"})).status,
        404
    );
    let bad_id = api.as_admin(
        Method::Put,
        "/api/v1/admin/posts/xyz/moderation",
        &[],
        Some(json!({"status": "APPROVED"})),
    );
    assert_eq!(bad_id.status, 400);
}

// AdminPostControllerTest
#[test]
fn 삭제는_멱등하게_204이고_검증_중이면_400이다() {
    let Some(api) = api() else { return };
    let s = seeded(&api);
    let pending = post_of(&api, &s, "PENDING");
    let validating = post_of(&api, &s, "VALIDATING");
    let delete = |post: String, body: Option<Value>| {
        api.as_admin(
            Method::Delete,
            &format!("/api/v1/admin/posts/{post}"),
            &[],
            body,
        )
    };
    assert_eq!(delete(pending.to_string(), None).status, 400);
    assert_eq!(
        delete(pending.to_string(), Some(json!({"reason": " "}))).status,
        400
    );
    assert_eq!(
        delete(
            pending.to_string(),
            Some(json!({"reason": "가".repeat(501)}))
        )
        .status,
        400
    );
    assert_eq!(
        delete("xyz".to_owned(), Some(json!({"reason": "x"}))).status,
        400
    );
    assert_eq!(delete(id(), Some(json!({"reason": "x"}))).status, 404);
    assert_eq!(
        delete(validating.to_string(), Some(json!({"reason": "x"}))).status,
        400
    );
    assert_eq!(
        delete(
            pending.to_string(),
            Some(json!({"reason": "가".repeat(500)}))
        )
        .status,
        204
    );
    assert_eq!(
        delete(pending.to_string(), Some(json!({"reason": "again"}))).status,
        204
    );
    let detail = api.get(&format!("/api/v1/admin/posts/{pending}"), &[]);
    assert!(detail.body["deletedAt"].is_string());
}

// AdminTopicControllerTest, AdminTopicPaginationTest
#[test]
fn 주제는_등록_조회_수정_삭제를_거친다() {
    let Some(api) = api() else { return };
    let body = json!({"title": "미래 주제", "topicDate": "2026-10-20",
        "startsAt": "2026-10-19T15:00:00Z", "endsAt": "2026-10-20T14:59:00Z"});
    let created = api.as_admin(
        Method::Post,
        "/api/v1/admin/topics",
        &[],
        Some(body.clone()),
    );
    assert_eq!(created.status, 201);
    let topic = created.body["topicId"].as_str().unwrap().to_owned();
    let detail = api.get(&format!("/api/v1/admin/topics/{topic}"), &[]);
    assert_eq!(detail.body["phase"], "BEFORE_OPEN");
    let mut changed = body.clone();
    changed["title"] = json!("수정된 주제");
    let updated = api.as_admin(
        Method::Put,
        &format!("/api/v1/admin/topics/{topic}"),
        &[],
        Some(changed),
    );
    assert_eq!(updated.status, 200);
    assert_eq!(updated.body["title"], "수정된 주제");
    let mut blank = body.clone();
    blank["title"] = json!(" ");
    assert_eq!(
        api.as_admin(Method::Post, "/api/v1/admin/topics", &[], Some(blank))
            .status,
        400
    );
    assert_eq!(
        api.as_admin(Method::Post, "/api/v1/admin/topics", &[], Some(body))
            .status,
        400
    );
    let list = api.get(
        "/api/v1/admin/topics",
        &[
            ("phase", "BEFORE_OPEN"),
            ("dateFrom", "2026-10-01"),
            ("sort", "topicDateAsc"),
        ],
    );
    assert_eq!(list.status, 200);
    assert_eq!(list.body["topics"].as_array().unwrap().len(), 1);
    let overflow = api.get(
        "/api/v1/admin/topics",
        &[("page", "2147483647"), ("pageSize", "100")],
    );
    assert_eq!(overflow.status, 400);
    let no_reason = api.as_admin(
        Method::Delete,
        &format!("/api/v1/admin/topics/{topic}"),
        &[],
        Some(json!({"reason": ""})),
    );
    assert_eq!(no_reason.status, 400);
    let deleted = api.as_admin(
        Method::Delete,
        &format!("/api/v1/admin/topics/{topic}"),
        &[],
        Some(json!({"reason": "취소"})),
    );
    assert_eq!(deleted.status, 204);
    assert_eq!(
        api.get(&format!("/api/v1/admin/topics/{topic}"), &[])
            .status,
        404
    );
}

// AdminFeedbackControllerTest
#[test]
fn 피드백_목록은_기본값과_정렬_오류를_다룬다() {
    let Some(api) = api() else { return };
    let user = insert_user(&mut api.db.client(), "f@example.com", "ACTIVE");
    api.db
        .client()
        .execute(
            "INSERT INTO feedbacks (user_id, content) VALUES ($1, '의견')",
            &[&user],
        )
        .unwrap();
    let list = api.get("/api/v1/admin/feedbacks", &[]);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["feedbacks"][0]["content"], "의견");
    assert_eq!(
        api.get("/api/v1/admin/feedbacks", &[("sort", "createdAtAsc")])
            .status,
        200
    );
    assert_eq!(
        api.get("/api/v1/admin/feedbacks", &[("sort", "bogus")])
            .status,
        400
    );
    assert_eq!(
        api.get("/api/v1/admin/feedbacks", &[("pageSize", "101")])
            .status,
        400
    );
}

// AdminAuditLogControllerTest, AdminAuditRequestIdTest
#[test]
fn 감사_로그_목록은_필터를_받고_잘못된_조건은_400이다() {
    let Some(api) = api() else { return };
    let user = insert_user(&mut api.db.client(), "g@example.com", "ACTIVE");
    api.as_admin(
        Method::Patch,
        &format!("/api/v1/admin/users/{user}/status"),
        &[],
        Some(json!({"status": "BANNED", "reason": "스팸"})),
    );
    let list = api.get("/api/v1/admin/audit-logs", &[]);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["auditLogs"][0]["action"], "USER_BANNED");
    let filtered = api.get(
        "/api/v1/admin/audit-logs",
        &[
            ("action", "USER_BANNED"),
            ("targetType", "USER"),
            ("targetId", &user.to_string()),
            ("adminId", &api.admin.to_string()),
        ],
    );
    assert_eq!(filtered.body["auditLogs"].as_array().unwrap().len(), 1);
    assert_eq!(
        api.get("/api/v1/admin/audit-logs", &[("action", "NOPE")])
            .status,
        400
    );
    assert_eq!(
        api.get("/api/v1/admin/audit-logs", &[("page", "0")]).status,
        400
    );
    let _ = &api.deps;
}

// AdminWebMvcConfigTest: 공통 보안 체인 밖이라 라우터는 요청 헤더로 관리자를 정하지 않는다.
#[test]
fn 응답은_json이다() {
    let Some(api) = api() else { return };
    let me = api.get("/api/v1/admin/auth/me", &[]);
    assert!(
        me.raw
            .headers
            .get("Content-Type")
            .is_some_and(|v| v.starts_with("application/json"))
    );
}

// CurrentAdminArgumentResolverTest, DevAdminActorResolverTest, AdminWebMvcConfigTest:
// 개발 우회에서는 인증 없이도 저장된 dev-admin이 작업자이고, 위조한 관리자 헤더는 무시한다.
#[test]
fn 개발_우회에서는_저장된_dev_admin을_작업자로_쓰고_헤더를_무시한다() {
    let Some(api) = api() else { return };
    let dev = insert_admin(&mut api.db.client(), "dev-admin");
    let mut bypass = (*api.deps.config).clone();
    bypass.development_bypass = true;
    let mut deps = api.deps.clone();
    deps.config = std::sync::Arc::new(bypass);
    let router = super::register_with(Routes::new(deps.logger.clone()), &deps).into_router();
    let mut req = Request::new(Method::Get, "/api/v1/admin/auth/me");
    req.headers
        .append("X-Admin-Id", api.admin.hyphenated().to_string());
    let response = router.handle(req);
    assert_eq!(response.status, 200);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["adminId"], dev.hyphenated().to_string());
    assert_eq!(body["username"], "dev-admin");
}

// AdminAuditRequestIdTest: 감사 로그의 requestId는 그 요청의 요청 식별자와 같다.
#[test]
fn 감사_로그에는_현재_요청의_요청_id가_남는다() {
    let Some(api) = api() else { return };
    let user = insert_user(&mut api.db.client(), "r@example.com", "ACTIVE");
    let request_id = Uuid::new_v4();
    let _id = chalkak_core::request_id::enter(request_id);
    let reply = api.as_admin(
        Method::Patch,
        &format!("/api/v1/admin/users/{user}/status"),
        &[],
        Some(json!({"status": "BANNED", "reason": "스팸"})),
    );
    assert_eq!(reply.status, 200);
    let stored: Uuid = api
        .db
        .client()
        .query_one("SELECT request_id FROM admin_audit_logs LIMIT 1", &[])
        .unwrap()
        .get(0);
    assert_eq!(stored, request_id);
}
