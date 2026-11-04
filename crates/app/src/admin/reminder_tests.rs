//! 관리자 리마인더 테스트. 원본 `AdminReminderServiceTest`, `AdminReminderControllerTest`, `AdminReminderApiTest`,
//! `AdminReminderAuditTransactionTest`, `AdminReminderConcurrencyTest`에 대응한다.
//!
//! 서비스를 목으로 바꾸지 않고 실제 DB로 라우터까지 통과시킨다. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::{Arc, Barrier};
use std::thread;

use chalkak_http::{Handler, Method, Request, Router};
use postgres::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use super::reminder::{AdminReminderService, ReminderInput};
use super::support::AdminDeps;
use super::testing::{NOW, deps, insert_admin};
use crate::notification::reminder::{ReminderRepeat, ReminderTarget};
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::Routes;

const PATH: &str = "/api/v1/admin/reminders";
const TITLE: &str = "오늘이 얼마 남지 않았어요";
const BODY: &str = "사진을 자랑해 주세요!";
const FUTURE: &str = "2026-10-12T02:00:00Z";

fn input(title: &str, repeat: ReminderRepeat, scheduled_at: &str) -> ReminderInput {
    ReminderInput {
        title: title.to_owned(),
        body: BODY.to_owned(),
        repeat,
        target: ReminderTarget::All,
        scheduled_at: scheduled_at.parse().unwrap(),
    }
}

fn request(title: &str, repeat: &str, scheduled_at: &str) -> Value {
    json!({
        "title": title,
        "body": BODY,
        "repeat": repeat,
        "target": "ALL",
        "scheduledAt": scheduled_at,
    })
}

struct Api {
    router: Router,
    admin: Uuid,
    deps: AdminDeps,
    db: TestDb,
}

struct Reply {
    status: u16,
    body: Value,
}

fn api() -> Option<Api> {
    let db = TestDb::create()?;
    let admin = insert_admin(&mut db.client(), "reminder-manager");
    let deps = deps(&db);
    let routes = super::register_with(Routes::new(deps.logger.clone()), &deps);
    Some(Api {
        router: routes.into_router(),
        admin,
        deps,
        db,
    })
}

impl Api {
    fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Reply {
        self.call_raw(method, path, body.map(Value::to_string).as_deref())
    }

    fn call_raw(&self, method: Method, path: &str, body: Option<&str>) -> Reply {
        let _guard = security::enter(Principal {
            subject: self.admin.hyphenated().to_string(),
            authorities: vec![AUTHORITY_ADMIN.to_owned()],
        });
        let mut req = Request::new(method, path);
        if let Some(body) = body {
            req.headers.append("Content-Type", "application/json");
            req.body = body.as_bytes().to_vec();
        }
        let response = self.router.handle(req);
        Reply {
            status: response.status,
            body: serde_json::from_slice(&response.body).unwrap_or(Value::Null),
        }
    }

    fn query(&self, path: &str, query: &[(&str, &str)]) -> Reply {
        let _guard = security::enter(Principal {
            subject: self.admin.hyphenated().to_string(),
            authorities: vec![AUTHORITY_ADMIN.to_owned()],
        });
        let mut req = Request::new(Method::Get, path);
        req.query = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let response = self.router.handle(req);
        Reply {
            status: response.status,
            body: serde_json::from_slice(&response.body).unwrap_or(Value::Null),
        }
    }

    fn create(&self, title: &str, repeat: &str, scheduled_at: &str) -> Reply {
        self.call(
            Method::Post,
            PATH,
            Some(&request(title, repeat, scheduled_at)),
        )
    }

    fn client(&self) -> Client {
        self.db.client()
    }
}

fn audit_count(client: &mut Client, action: &str) -> i64 {
    client
        .query_one(
            "SELECT COUNT(*) FROM admin_audit_logs WHERE action = $1::text::admin_action",
            &[&action],
        )
        .unwrap()
        .get(0)
}

fn audit_state(client: &mut Client, target: &str, action: &str) -> (Value, Value) {
    let target: Uuid = target.parse().unwrap();
    let row = client
        .query_one(
            "SELECT before_state::text, after_state::text FROM admin_audit_logs \
             WHERE target_id = $1 AND action = $2::text::admin_action",
            &[&target, &action],
        )
        .unwrap();
    let parse = |text: String| serde_json::from_str::<Value>(&text).unwrap();
    (parse(row.get(0)), parse(row.get(1)))
}

fn reminder_count(client: &mut Client) -> i64 {
    client
        .query_one("SELECT COUNT(*) FROM reminders", &[])
        .unwrap()
        .get(0)
}

fn body_hash() -> String {
    crate::common::sha256_hex(BODY.as_bytes())
}

// ----- 목록 -----

#[test]
fn 목록은_등록_최신순으로_페이지_정보와_함께_응답하고_삭제한_것은_뺀다() {
    let Some(api) = api() else { return };
    let empty = api.query(PATH, &[]);
    assert_eq!(empty.status, 200);
    assert_eq!(
        empty.body,
        json!({"currentPage": 1, "pageSize": 20, "hasNext": false, "reminders": []})
    );

    let first = api.create("첫째", "ONCE", FUTURE);
    // 등록 시각이 같으면 ID 역순이므로 시계를 움직이지 않고 연속 등록해도 순서가 정해진다.
    let second = api.create("둘째", "DAILY", FUTURE);
    let third = api.create("셋째", "ONCE", FUTURE);
    let ids: Vec<&str> = [&first, &second, &third]
        .iter()
        .map(|r| r.body["reminderId"].as_str().unwrap())
        .collect();

    let page = api.query(PATH, &[("page", "1"), ("pageSize", "2")]);
    assert_eq!(page.status, 200);
    assert_eq!(page.body["currentPage"], 1);
    assert_eq!(page.body["pageSize"], 2);
    assert_eq!(page.body["hasNext"], true);
    assert_eq!(page.body["reminders"].as_array().unwrap().len(), 2);
    assert_eq!(page.body["reminders"][0]["reminderId"], ids[2]);
    assert_eq!(page.body["reminders"][1]["reminderId"], ids[1]);

    let last = api.query(PATH, &[("page", "2"), ("pageSize", "2")]);
    assert_eq!(last.body["hasNext"], false);
    assert_eq!(last.body["reminders"][0]["reminderId"], ids[0]);

    let deleted = api.call(Method::Delete, &format!("{PATH}/{}", ids[1]), None);
    assert_eq!(deleted.status, 204);
    let after = api.query(PATH, &[]);
    assert_eq!(after.body["reminders"].as_array().unwrap().len(), 2);
}

#[test]
fn 페이지_번호_0이나_크기_0_101은_400이다() {
    let Some(api) = api() else { return };
    for query in [
        [("page", "0"), ("pageSize", "20")],
        [("page", "1"), ("pageSize", "0")],
        [("page", "1"), ("pageSize", "101")],
    ] {
        let reply = api.query(PATH, &query);
        assert_eq!(reply.status, 400, "{query:?}");
        assert_eq!(reply.body["message"], "조회 조건이 올바르지 않습니다.");
    }
}

// ----- 등록 -----

#[test]
fn 등록_성공_시_201과_정규화한_리마인더를_응답하고_감사_로그에는_본문_해시를_남긴다() {
    let Some(api) = api() else { return };
    let reply = api.call(
        Method::Post,
        PATH,
        Some(&json!({
            "title": format!("  {TITLE}  "),
            "body": format!(" {BODY}\n"),
            "repeat": "DAILY",
            "target": "TOPIC_NON_PARTICIPANTS",
            "scheduledAt": "2026-10-12T02:00:00.123456789Z",
        })),
    );
    assert_eq!(reply.status, 201);
    assert_eq!(reply.body["title"], TITLE);
    assert_eq!(reply.body["body"], BODY);
    assert_eq!(reply.body["repeat"], "DAILY");
    assert_eq!(reply.body["target"], "TOPIC_NON_PARTICIPANTS");
    assert_eq!(reply.body["scheduledAt"], "2026-10-12T02:00:00.123456Z");
    assert_eq!(reply.body["createdAt"], NOW);
    assert_eq!(reply.body["updatedAt"], NOW);
    let id = reply.body["reminderId"].as_str().unwrap();
    let (before, after) = audit_state(&mut api.client(), id, "REMINDER_CREATED");
    assert_eq!(before, json!({}));
    assert_eq!(
        after,
        json!({
            "title": TITLE,
            "bodySha256": body_hash(),
            "repeat": "DAILY",
            "target": "TOPIC_NON_PARTICIPANTS",
            "scheduledAt": "2026-10-12T02:00:00.123456Z",
        })
    );
}

#[test]
fn 제목_본문_누락이나_공백_반복_대상_발송_시각_누락이나_형식_오류는_400이다() {
    let Some(api) = api() else { return };
    let bodies = [
        r#"{"body":"b","repeat":"ONCE","target":"ALL","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":" ","body":"b","repeat":"ONCE","target":"ALL","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":"t","body":" ","repeat":"ONCE","target":"ALL","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":"t","body":"b","target":"ALL","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":"t","body":"b","repeat":"WEEKLY","target":"ALL","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":"t","body":"b","repeat":"ONCE","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":"t","body":"b","repeat":"ONCE","target":"NOBODY","scheduledAt":"2026-10-12T02:00:00Z"}"#,
        r#"{"title":"t","body":"b","repeat":"ONCE","target":"ALL"}"#,
        r#"{"title":"t","body":"b","repeat":"ONCE","target":"ALL","scheduledAt":"tomorrow"}"#,
        "",
    ];
    for body in bodies {
        let reply = api.call_raw(Method::Post, PATH, Some(body));
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR", "{body}");
    }
    assert_eq!(reminder_count(&mut api.client()), 0);
}

#[test]
fn 현재보다_과거인_발송_시각과_길이_초과는_거절하고_저장하지_않는다() {
    let Some(api) = api() else { return };
    let past = api.create(TITLE, "ONCE", "2026-10-12T00:59:59Z");
    assert_eq!(past.status, 400);
    assert_eq!(
        past.body["message"],
        "리마인더 발송 시각은 현재 시각보다 과거일 수 없습니다."
    );
    let now = api.create(TITLE, "ONCE", NOW);
    assert_eq!(now.status, 201);
    let long = api.create(&"가".repeat(51), "ONCE", FUTURE);
    assert_eq!(long.status, 400);
    assert_eq!(
        long.body["message"],
        "리마인더 제목은 1자 이상 50자 이하여야 합니다."
    );
    assert_eq!(reminder_count(&mut api.client()), 1);
}

#[test]
fn 감사_로그가_민감_정보로_판정하는_제목이면_등록도_함께_롤백한다() {
    let Some(api) = api() else { return };
    let reply = api.create("accessToken=secret-value", "ONCE", FUTURE);
    assert_eq!(reply.status, 400);
    assert_eq!(reminder_count(&mut api.client()), 0);
    assert_eq!(audit_count(&mut api.client(), "REMINDER_CREATED"), 0);
}

// ----- 수정 -----

#[test]
fn 수정_성공_시_200과_수정된_리마인더를_응답하고_전후_상태를_남긴다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "ONCE", FUTURE);
    let id = created.body["reminderId"].as_str().unwrap().to_owned();
    let reply = api.call(
        Method::Put,
        &format!("{PATH}/{id}"),
        Some(&json!({
            "title": "새 제목",
            "body": BODY,
            "repeat": "DAILY",
            "target": "TOPIC_PARTICIPANTS",
            "scheduledAt": "2026-10-12T03:00:00Z",
        })),
    );
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["reminderId"], id.as_str());
    assert_eq!(reply.body["title"], "새 제목");
    assert_eq!(reply.body["repeat"], "DAILY");
    assert_eq!(reply.body["target"], "TOPIC_PARTICIPANTS");
    assert_eq!(reply.body["scheduledAt"], "2026-10-12T03:00:00Z");
    assert_eq!(reply.body["createdAt"], created.body["createdAt"]);
    let (before, after) = audit_state(&mut api.client(), &id, "REMINDER_UPDATED");
    assert_eq!(before["title"], TITLE);
    assert_eq!(before["repeat"], "ONCE");
    assert_eq!(after["title"], "새 제목");
    assert_eq!(after["target"], "TOPIC_PARTICIPANTS");
}

#[test]
fn 같은_내용으로_수정하면_감사_로그_없이_현재_리마인더를_응답한다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "ONCE", FUTURE);
    let id = created.body["reminderId"].as_str().unwrap();
    let reply = api.call(
        Method::Put,
        &format!("{PATH}/{id}"),
        Some(&request(&format!(" {TITLE} "), "ONCE", FUTURE)),
    );
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, created.body);
    assert_eq!(audit_count(&mut api.client(), "REMINDER_UPDATED"), 0);
}

#[test]
fn 발송_시각이_지난_일회성_수정은_400이고_없는_리마인더는_404다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "ONCE", FUTURE);
    let id = created.body["reminderId"].as_str().unwrap().to_owned();
    // 발송 시각 이후로 시계를 옮긴 서비스로 수정한다.
    let later = super::testing::deps_at(&api.db, "2026-10-12T03:00:00Z");
    let error = AdminReminderService::new(later)
        .update_reminder(
            id.parse().unwrap(),
            api.admin,
            &input("새 제목", ReminderRepeat::Once, "2026-10-12T04:00:00Z"),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with("발송 시각이 지난 일회성 리마인더는 수정할 수 없습니다.")
    );

    let missing = api.call(
        Method::Put,
        &format!("{PATH}/{}", Uuid::new_v4()),
        Some(&request(TITLE, "ONCE", FUTURE)),
    );
    assert_eq!(missing.status, 404);
    let malformed = api.call(
        Method::Put,
        &format!("{PATH}/not-a-uuid"),
        Some(&request(TITLE, "ONCE", FUTURE)),
    );
    assert_eq!(malformed.status, 400);
}

#[test]
fn 수정_감사_로그_저장이_실패하면_수정도_함께_롤백한다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "ONCE", FUTURE);
    let id = created.body["reminderId"].as_str().unwrap();
    let reply = api.call(
        Method::Put,
        &format!("{PATH}/{id}"),
        Some(&request("accessToken=secret-value", "ONCE", FUTURE)),
    );
    assert_eq!(reply.status, 400);
    let read = api.query(PATH, &[]);
    assert_eq!(read.body["reminders"][0]["title"], TITLE);
    assert_eq!(audit_count(&mut api.client(), "REMINDER_UPDATED"), 0);
}

// ----- 삭제 -----

#[test]
fn 삭제하면_204이고_삭제_시각을_저장하며_감사_로그에_전후_상태를_남긴다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "ONCE", FUTURE);
    let id = created.body["reminderId"].as_str().unwrap().to_owned();
    let reply = api.call(Method::Delete, &format!("{PATH}/{id}"), None);
    assert_eq!(reply.status, 204);
    let (before, after) = audit_state(&mut api.client(), &id, "REMINDER_DELETED");
    assert_eq!(before["deletedAt"], Value::Null);
    assert_eq!(after["deletedAt"], NOW);
    assert_eq!(after["title"], TITLE);

    // 이미 삭제한 리마인더는 최초 결과를 유지하고 감사 로그를 추가하지 않는다.
    let again = api.call(Method::Delete, &format!("{PATH}/{id}"), None);
    assert_eq!(again.status, 204);
    assert_eq!(audit_count(&mut api.client(), "REMINDER_DELETED"), 1);

    // 삭제한 리마인더는 수정할 수 없다.
    let update = api.call(
        Method::Put,
        &format!("{PATH}/{id}"),
        Some(&request(TITLE, "ONCE", FUTURE)),
    );
    assert_eq!(update.status, 400);
    assert_eq!(
        update.body["message"],
        "삭제된 리마인더는 수정할 수 없습니다."
    );
}

#[test]
fn 없는_리마인더나_잘못된_id를_삭제하면_404_400이다() {
    let Some(api) = api() else { return };
    let missing = api.call(Method::Delete, &format!("{PATH}/{}", Uuid::new_v4()), None);
    assert_eq!(missing.status, 404);
    let malformed = api.call(Method::Delete, &format!("{PATH}/abc"), None);
    assert_eq!(malformed.status, 400);
}

// ----- 동시성 (AdminReminderConcurrencyTest) -----

#[test]
fn 같은_리마인더를_동시에_수정하면_직전_변경_값을_감사_로그의_변경_전_값으로_기록한다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "DAILY", FUTURE);
    let id: Uuid = created.body["reminderId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["제목 A", "제목 B"]
        .into_iter()
        .map(|title| {
            let service = AdminReminderService::new(api.deps.clone());
            let barrier = barrier.clone();
            let admin = api.admin;
            thread::spawn(move || {
                barrier.wait();
                service.update_reminder(id, admin, &input(title, ReminderRepeat::Daily, FUTURE))
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let rows = api
        .client()
        .query(
            "SELECT before_state->>'title', after_state->>'title' FROM admin_audit_logs \
             WHERE target_id = $1 AND action = 'REMINDER_UPDATED'::admin_action ORDER BY occurred_at, id",
            &[&id],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    let first_after: String = rows[0].get(1);
    let second_before: String = rows[1].get(0);
    assert_eq!(rows[0].get::<_, String>(0), TITLE);
    assert_eq!(second_before, first_after);
}

#[test]
fn 같은_리마인더를_동시에_두_번_삭제해도_삭제_감사_로그는_한_건이다() {
    let Some(api) = api() else { return };
    let created = api.create(TITLE, "ONCE", FUTURE);
    let id: Uuid = created.body["reminderId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let service = AdminReminderService::new(api.deps.clone());
            let barrier = barrier.clone();
            let admin = api.admin;
            thread::spawn(move || {
                barrier.wait();
                service.delete_reminder(id, admin)
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(audit_count(&mut api.client(), "REMINDER_DELETED"), 1);
}
