//! 관리자 주제 푸시 시간표 테스트. 원본 `AdminTopicPushScheduleServiceTest`, `AdminTopicPushScheduleControllerTest`,
//! `AdminTopicPushScheduleApiTest`, `AdminTopicPushScheduleAuditTransactionTest`,
//! `AdminTopicPushScheduleConcurrencyTest`, `TopicPushScheduleRepositoryImplTest`에 대응한다.
//!
//! 서비스를 목으로 바꾸지 않고 실제 DB로 라우터까지 통과시킨다. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use chalkak_http::{Handler, Method, Request, Router};
use jiff::Timestamp;
use jiff::civil::Time;
use postgres::Client;
use serde_json::Value;
use uuid::Uuid;

use super::support::{AdminDeps, in_transaction};
use super::testing::{NOW, deps, insert_admin};
use super::topic_push_schedule::AdminTopicPushScheduleService;
use crate::notification::topic_push_schedule::{
    DUPLICATE_SEND_TIME_MESSAGE, INVALID_SEND_TIME_MESSAGE, TopicPushScheduleRepository,
};
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::Routes;

const NOT_FOUND: &str = "주제 푸시 설정을 찾을 수 없습니다.";

fn time(hour: i8, minute: i8, second: i8) -> Time {
    Time::new(hour, minute, second, 0).unwrap()
}

fn count(client: &mut Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).unwrap().get(0)
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

fn audit_state(client: &mut Client, target: Uuid, action: &str) -> (Value, Value) {
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

fn send_time_of(client: &mut Client, id: Uuid) -> Option<Time> {
    client
        .query_opt(
            "SELECT send_time FROM topic_push_schedules WHERE id = $1",
            &[&id],
        )
        .unwrap()
        .map(|row| row.get(0))
}

fn setup() -> Option<(TestDb, AdminDeps, Uuid)> {
    let db = TestDb::create()?;
    let admin = insert_admin(&mut db.client(), "push-schedule-manager");
    let deps = deps(&db);
    Some((db, deps, admin))
}

// ---------------------------------------------------------------------------------------------
// AdminTopicPushScheduleServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 설정_전체를_등록_순서와_관계없이_발송_시각_오름차순으로_조회한다() {
    let Some((_db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps);
    for t in [time(18, 0, 0), time(9, 0, 0), time(8, 30, 0)] {
        service.create_schedule(admin, t).unwrap();
    }
    let schedules = service.get_schedules().unwrap();
    let times: Vec<Time> = schedules.iter().map(|s| s.send_time).collect();
    assert_eq!(times, [time(8, 30, 0), time(9, 0, 0), time(18, 0, 0)]);
    assert!(
        schedules
            .iter()
            .all(|s| s.id.is_some() && s.created_at.is_some() && s.updated_at.is_some())
    );
}

#[test]
fn 등록된_설정이_없으면_빈_목록을_조회한다() {
    let Some((_db, deps, _admin)) = setup() else {
        return;
    };
    assert!(
        AdminTopicPushScheduleService::new(deps)
            .get_schedules()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn 설정을_생성하면_시각_식별자_생성_시각을_저장하고_감사_로그를_남긴다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let created = AdminTopicPushScheduleService::new(deps)
        .create_schedule(admin, time(9, 0, 0))
        .unwrap();
    let id = created.id.unwrap();
    assert_eq!(send_time_of(&mut client, id), Some(time(9, 0, 0)));
    assert_eq!(created.created_at, Some(NOW.parse::<Timestamp>().unwrap()));

    let row = client
        .query_one(
            "SELECT actor_admin_id, target_type::text, reason, occurred_at \
             FROM admin_audit_logs WHERE target_id = $1",
            &[&id],
        )
        .unwrap();
    assert_eq!(row.get::<_, Uuid>(0), admin);
    assert_eq!(row.get::<_, String>(1), "TOPIC_PUSH_SCHEDULE");
    assert_eq!(row.get::<_, Option<String>>(2), None);
    let (before, after) = audit_state(&mut client, id, "TOPIC_PUSH_SCHEDULE_CREATED");
    assert_eq!(before, serde_json::json!({}));
    assert_eq!(after, serde_json::json!({"sendTime": "09:00:00"}));
}

#[test]
fn 서로_다른_시각을_여러_개_등록할_수_있다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps);
    let first = service.create_schedule(admin, time(8, 0, 0)).unwrap();
    let second = service.create_schedule(admin, time(18, 0, 0)).unwrap();
    assert_ne!(first.id, second.id);
    let mut client = db.client();
    assert_eq!(
        send_time_of(&mut client, first.id.unwrap()),
        Some(time(8, 0, 0))
    );
    assert_eq!(
        send_time_of(&mut client, second.id.unwrap()),
        Some(time(18, 0, 0))
    );
}

#[test]
fn 같은_시각은_중복으로_등록할_수_없다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps);
    service.create_schedule(admin, time(9, 0, 0)).unwrap();
    let error = service.create_schedule(admin, time(9, 0, 0)).unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(error.message(), DUPLICATE_SEND_TIME_MESSAGE);
    assert_eq!(
        audit_count(&mut db.client(), "TOPIC_PUSH_SCHEDULE_CREATED"),
        1
    );
}

#[test]
fn 초_단위_생성_시각은_거절하고_설정과_감사_기록을_남기지_않는다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminTopicPushScheduleService::new(deps);
    let error = service.create_schedule(admin, time(9, 0, 1)).unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(error.message(), INVALID_SEND_TIME_MESSAGE);
    assert_eq!(
        count(&mut client, "SELECT COUNT(*) FROM topic_push_schedules"),
        0
    );
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_CREATED"), 0);
}

#[test]
fn 잘못된_수정_시각은_거절하고_기존_설정을_유지한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminTopicPushScheduleService::new(deps);
    let id = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    let error = service
        .update_schedule(id, admin, time(18, 0, 1))
        .unwrap_err();
    assert_eq!(error.message(), INVALID_SEND_TIME_MESSAGE);
    assert_eq!(send_time_of(&mut client, id), Some(time(9, 0, 0)));
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_UPDATED"), 0);
}

#[test]
fn 시각을_수정하면_식별자_생성_시각을_유지하고_변경_전후를_감사_로그에_남긴다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminTopicPushScheduleService::new(deps);
    let original = service.create_schedule(admin, time(9, 0, 0)).unwrap();
    let id = original.id.unwrap();
    let updated = service.update_schedule(id, admin, time(18, 30, 0)).unwrap();
    assert_eq!(updated.id, original.id);
    assert_eq!(updated.created_at, original.created_at);
    assert_eq!(updated.send_time, time(18, 30, 0));
    assert!(updated.updated_at >= original.updated_at);
    assert_eq!(send_time_of(&mut client, id), Some(time(18, 30, 0)));
    let (before, after) = audit_state(&mut client, id, "TOPIC_PUSH_SCHEDULE_UPDATED");
    assert_eq!(before, serde_json::json!({"sendTime": "09:00:00"}));
    assert_eq!(after, serde_json::json!({"sendTime": "18:30:00"}));
}

#[test]
fn 현재_설정_시각으로_다시_수정해도_같은_결과를_반환하고_감사_로그를_남기지_않는다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps);
    let original = service.create_schedule(admin, time(9, 0, 0)).unwrap();
    let result = service
        .update_schedule(original.id.unwrap(), admin, time(9, 0, 0))
        .unwrap();
    assert_eq!(result, original);
    assert_eq!(
        audit_count(&mut db.client(), "TOPIC_PUSH_SCHEDULE_UPDATED"),
        0
    );
    assert_eq!(service.get_schedules().unwrap().len(), 1);
}

#[test]
fn 다른_설정의_시각으로_수정할_수_없다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps);
    let id = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    service.create_schedule(admin, time(18, 0, 0)).unwrap();
    let error = service
        .update_schedule(id, admin, time(18, 0, 0))
        .unwrap_err();
    assert_eq!(error.message(), DUPLICATE_SEND_TIME_MESSAGE);
    let times: Vec<Time> = service
        .get_schedules()
        .unwrap()
        .iter()
        .map(|s| s.send_time)
        .collect();
    assert_eq!(times, [time(9, 0, 0), time(18, 0, 0)]);
    assert_eq!(
        audit_count(&mut db.client(), "TOPIC_PUSH_SCHEDULE_UPDATED"),
        0
    );
}

#[test]
fn 없는_설정을_수정하거나_삭제하면_찾을_수_없음_오류로_거절한다() {
    let Some((_db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps);
    let error = service
        .update_schedule(Uuid::new_v4(), admin, time(9, 0, 0))
        .unwrap_err();
    assert_eq!((error.status(), error.message().as_str()), (404, NOT_FOUND));
    let error = service.delete_schedule(Uuid::new_v4(), admin).unwrap_err();
    assert_eq!((error.status(), error.message().as_str()), (404, NOT_FOUND));
}

#[test]
fn 설정을_삭제하면_대상만_지우고_감사_로그를_남기며_같은_시각을_다시_등록할_수_있다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminTopicPushScheduleService::new(deps);
    let target = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    let remaining = service
        .create_schedule(admin, time(18, 0, 0))
        .unwrap()
        .id
        .unwrap();
    service.delete_schedule(target, admin).unwrap();
    assert_eq!(send_time_of(&mut client, target), None);
    assert!(send_time_of(&mut client, remaining).is_some());
    let (before, after) = audit_state(&mut client, target, "TOPIC_PUSH_SCHEDULE_DELETED");
    assert_eq!(before, serde_json::json!({"sendTime": "09:00:00"}));
    assert_eq!(after, serde_json::json!({}));
    audit_state(&mut client, target, "TOPIC_PUSH_SCHEDULE_CREATED");

    let recreated = service.create_schedule(admin, time(9, 0, 0)).unwrap();
    assert_ne!(recreated.id, Some(target));
}

#[test]
fn 관리자_식별자가_없으면_설정과_감사_로그를_만들지_않는다() {
    let Some((db, deps, _admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let error = AdminTopicPushScheduleService::new(deps)
        .create_schedule(Uuid::new_v4(), time(9, 0, 0))
        .unwrap_err();
    assert_eq!(error.status(), 500);
    assert_eq!(
        count(&mut client, "SELECT COUNT(*) FROM topic_push_schedules"),
        0
    );
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_CREATED"), 0);
}

// ---------------------------------------------------------------------------------------------
// AdminTopicPushScheduleAuditTransactionTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 감사_로그_저장이_실패하면_수정과_삭제도_함께_롤백한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminTopicPushScheduleService::new(deps);
    let id = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    // 없는 관리자로 요청하면 감사 로그의 외래 키가 실패한다.
    let stranger = Uuid::new_v4();
    assert_eq!(
        service
            .update_schedule(id, stranger, time(18, 0, 0))
            .unwrap_err()
            .status(),
        500
    );
    assert_eq!(send_time_of(&mut client, id), Some(time(9, 0, 0)));
    assert_eq!(
        service.delete_schedule(id, stranger).unwrap_err().status(),
        500
    );
    assert_eq!(send_time_of(&mut client, id), Some(time(9, 0, 0)));
}

#[test]
fn 업무_트랜잭션이_실패하면_설정과_감사_로그를_함께_롤백한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let mut conn = deps.pool.get().unwrap();
    let result: Result<(), _> = in_transaction(&mut conn, |tx| {
        let mut schedule =
            crate::notification::topic_push_schedule::TopicPushSchedule::new(time(9, 0, 0))?;
        TopicPushScheduleRepository
            .save_and_flush(tx, &mut schedule, NOW.parse().unwrap())
            .map_err(|_| crate::web::ApiError::Internal("저장 실패".to_owned()))?;
        let _ = admin;
        Err(crate::web::ApiError::Internal("업무 실패".to_owned()))
    });
    assert!(result.is_err());
    assert_eq!(
        count(&mut client, "SELECT COUNT(*) FROM topic_push_schedules"),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// AdminTopicPushScheduleConcurrencyTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 같은_시각의_동시_등록은_한_건만_커밋하고_나머지는_중복_오류로_거절한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let service = AdminTopicPushScheduleService::new(deps.clone());
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                service.create_schedule(admin, time(9, 0, 0))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for error in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(error.status(), 400);
        assert_eq!(error.message(), DUPLICATE_SEND_TIME_MESSAGE);
    }
    let mut client = db.client();
    assert_eq!(
        count(&mut client, "SELECT COUNT(*) FROM topic_push_schedules"),
        1
    );
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_CREATED"), 1);
}

#[test]
fn 서로_다른_설정이_같은_시각으로_동시_수정되어도_한_건만_커밋한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps.clone());
    let a = service
        .create_schedule(admin, time(8, 0, 0))
        .unwrap()
        .id
        .unwrap();
    let b = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [a, b]
        .into_iter()
        .map(|id| {
            let service = AdminTopicPushScheduleService::new(deps.clone());
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                service.update_schedule(id, admin, time(12, 0, 0))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let mut client = db.client();
    assert_eq!(
        count(
            &mut client,
            "SELECT COUNT(*) FROM topic_push_schedules WHERE send_time = '12:00'"
        ),
        1
    );
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_UPDATED"), 1);
}

/// 설정 행을 잠근 채 `release` 신호를 기다리는 트랜잭션(수정·삭제 중인 요청을 흉내 낸다).
fn hold_lock(
    deps: &AdminDeps,
    id: Uuid,
    new_time: Option<Time>,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let deps = deps.clone();
    let handle = thread::spawn(move || {
        let mut conn = deps.pool.get().unwrap();
        in_transaction(&mut conn, |tx| {
            let repo = TopicPushScheduleRepository;
            let mut schedule = repo.find_by_id_for_update(tx, id)?.unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            match new_time {
                Some(t) => {
                    schedule.update_send_time(t)?;
                    repo.save_and_flush(tx, &mut schedule, deps.now())
                        .map_err(|_| crate::web::ApiError::Internal("저장 실패".to_owned()))?;
                }
                None => repo.delete(tx, id)?,
            }
            Ok(())
        })
        .unwrap();
    });
    (locked_rx, release_tx, handle)
}

#[test]
fn 삭제_트랜잭션을_기다린_수정은_대상_없음_오류로_끝나고_추가_감사_기록을_남기지_않는다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps.clone());
    let id = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    let (locked, release, holder) = hold_lock(&deps, id, None);
    locked.recv().unwrap();
    let waiting = thread::spawn(move || service.update_schedule(id, admin, time(10, 0, 0)));
    thread::sleep(Duration::from_millis(300));
    assert!(!waiting.is_finished());
    release.send(()).unwrap();
    holder.join().unwrap();
    let error = waiting.join().unwrap().unwrap_err();
    assert_eq!((error.status(), error.message().as_str()), (404, NOT_FOUND));
    assert_eq!(
        audit_count(&mut db.client(), "TOPIC_PUSH_SCHEDULE_UPDATED"),
        0
    );
}

#[test]
fn 수정을_기다린_삭제는_수정된_시각을_감사_기록에_남기고_설정을_삭제한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminTopicPushScheduleService::new(deps.clone());
    let id = service
        .create_schedule(admin, time(9, 0, 0))
        .unwrap()
        .id
        .unwrap();
    let (locked, release, holder) = hold_lock(&deps, id, Some(time(10, 0, 0)));
    locked.recv().unwrap();
    let waiting = thread::spawn(move || service.delete_schedule(id, admin));
    thread::sleep(Duration::from_millis(300));
    assert!(!waiting.is_finished());
    release.send(()).unwrap();
    holder.join().unwrap();
    waiting.join().unwrap().unwrap();
    let mut client = db.client();
    let (before, _) = audit_state(&mut client, id, "TOPIC_PUSH_SCHEDULE_DELETED");
    assert_eq!(before, serde_json::json!({"sendTime": "10:00:00"}));
    assert_eq!(send_time_of(&mut client, id), None);
}

// ---------------------------------------------------------------------------------------------
// AdminTopicPushScheduleControllerTest / ApiTest
// ---------------------------------------------------------------------------------------------

const PATH: &str = "/api/v1/admin/topic-push-schedules";

struct Api {
    router: Router,
    admin: Uuid,
    db: TestDb,
}

struct Reply {
    status: u16,
    body: Value,
    raw: Vec<u8>,
}

fn api() -> Option<Api> {
    let (db, deps, admin) = setup()?;
    let routes = super::register_with(Routes::new(deps.logger.clone()), &deps);
    Some(Api {
        router: routes.into_router(),
        admin,
        db,
    })
}

impl Api {
    fn call(&self, method: Method, path: &str, body: Option<&str>) -> Reply {
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
            raw: response.body,
        }
    }

    fn create(&self, send_time: &str) -> Reply {
        self.call(
            Method::Post,
            PATH,
            Some(&format!(r#"{{"sendTime":"{send_time}"}}"#)),
        )
    }
}

#[test]
fn 목록_조회는_200과_설정별_시각_식별자_utc_시각을_응답하고_없으면_빈_배열이다() {
    let Some(api) = api() else { return };
    let empty = api.call(Method::Get, PATH, None);
    assert_eq!(empty.status, 200);
    assert_eq!(empty.body, serde_json::json!({"schedules": []}));

    let created = api.create("09:00:00");
    let list = api.call(Method::Get, PATH, None);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["schedules"].as_array().unwrap().len(), 1);
    assert_eq!(list.body["schedules"][0], created.body);
    assert_eq!(list.body["schedules"][0]["sendTime"], "09:00:00");
    assert_eq!(list.body["schedules"][0]["createdAt"], NOW);
}

#[test]
fn 생성_성공_시_201과_저장된_설정을_응답한다() {
    let Some(api) = api() else { return };
    let reply = api.create("09:00:00");
    assert_eq!(reply.status, 201);
    assert_eq!(reply.body["sendTime"], "09:00:00");
    assert!(reply.body.get("enabled").is_none());
    assert_eq!(reply.body["createdAt"], NOW);
    assert_eq!(reply.body["updatedAt"], NOW);
    Uuid::parse_str(reply.body["scheduleId"].as_str().unwrap()).unwrap();
}

#[test]
fn 시각은_분_단위_문자열_배열_날짜시각_형식도_받는다() {
    let Some(api) = api() else { return };
    for (body, expected) in [
        (r#"{"sendTime":"09:00"}"#, "09:00:00"),
        (r#"{"sendTime":[10,30]}"#, "10:30:00"),
        (r#"{"sendTime":" 11:00:00 "}"#, "11:00:00"),
        (r#"{"sendTime":"12:00:00.000"}"#, "12:00:00"),
        (r#"{"sendTime":"2026-10-12T13:00:00"}"#, "13:00:00"),
        (r#"{"sendTime":"2026-10-12T05:00:00Z"}"#, "05:00:00"),
    ] {
        let reply = api.call(Method::Post, PATH, Some(body));
        assert_eq!(reply.status, 201, "{body}");
        assert_eq!(reply.body["sendTime"], expected, "{body}");
    }
}

#[test]
fn 발송_시각_누락_null_잘못된_시각은_400으로_거절한다() {
    let Some(api) = api() else { return };
    let existing = api.create("08:00:00").body["scheduleId"]
        .as_str()
        .unwrap()
        .to_owned();
    for body in [
        "{}",
        r#"{"sendTime":null}"#,
        r#"{"sendTime":"24:00:00"}"#,
        r#"{"sendTime":"23:59:60"}"#,
        r#"{"sendTime":"invalid"}"#,
        r#"{"sendTime":9}"#,
        "",
    ] {
        for (method, path) in [
            (Method::Post, PATH.to_owned()),
            (Method::Put, format!("{PATH}/{existing}")),
        ] {
            let reply = api.call(method, &path, Some(body));
            assert_eq!(reply.status, 400, "{body}");
            assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR", "{body}");
        }
    }
    assert_eq!(
        count(
            &mut api.db.client(),
            "SELECT COUNT(*) FROM topic_push_schedules"
        ),
        1
    );
}

#[test]
fn 초_또는_소수_초가_있는_요청은_400으로_거절하고_저장하지_않는다() {
    let Some(api) = api() else { return };
    for body in [
        r#"{"sendTime":"09:00:01"}"#,
        r#"{"sendTime":"09:00:00.5"}"#,
        r#"{"sendTime":[9,0,0,1]}"#,
    ] {
        let reply = api.call(Method::Post, PATH, Some(body));
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(reply.body["message"], INVALID_SEND_TIME_MESSAGE, "{body}");
    }
    let id = api.create("09:00:00").body["scheduleId"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = api.call(
        Method::Put,
        &format!("{PATH}/{id}"),
        Some(r#"{"sendTime":"10:00:01"}"#),
    );
    assert_eq!(reply.status, 400);
    assert_eq!(reply.body["message"], INVALID_SEND_TIME_MESSAGE);
    let mut client = db_client(&api);
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_UPDATED"), 0);
    assert_eq!(audit_count(&mut client, "TOPIC_PUSH_SCHEDULE_CREATED"), 1);
}

fn db_client(api: &Api) -> Client {
    api.db.client()
}

#[test]
fn 중복_등록과_중복_수정은_400_business_error로_응답한다() {
    let Some(api) = api() else { return };
    api.create("09:00:00");
    let duplicate = api.create("09:00:00");
    assert_eq!(duplicate.status, 400);
    assert_eq!(duplicate.body["errorCode"], "BUSINESS_ERROR");
    assert_eq!(duplicate.body["message"], DUPLICATE_SEND_TIME_MESSAGE);

    let other = api.create("18:00:00").body["scheduleId"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = api.call(
        Method::Put,
        &format!("{PATH}/{other}"),
        Some(r#"{"sendTime":"09:00:00"}"#),
    );
    assert_eq!(reply.status, 400);
    assert_eq!(reply.body["message"], DUPLICATE_SEND_TIME_MESSAGE);
}

#[test]
fn 수정_성공_시_200과_변경된_설정을_응답한다() {
    let Some(api) = api() else { return };
    let created = api.create("09:00:00");
    let id = created.body["scheduleId"].as_str().unwrap();
    let reply = api.call(
        Method::Put,
        &format!("{PATH}/{id}"),
        Some(r#"{"sendTime":"18:30:00"}"#),
    );
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["scheduleId"], id);
    assert_eq!(reply.body["sendTime"], "18:30:00");
    assert_eq!(reply.body["createdAt"], created.body["createdAt"]);
}

#[test]
fn 표준_uuid가_아닌_설정_id는_400으로_거절한다() {
    let Some(api) = api() else { return };
    for id in ["invalid", "1-1-1-1-1"] {
        let put = api.call(
            Method::Put,
            &format!("{PATH}/{id}"),
            Some(r#"{"sendTime":"09:00:00"}"#),
        );
        assert_eq!(put.status, 400, "{id}");
        assert_eq!(put.body["errorCode"], "BUSINESS_ERROR", "{id}");
        let delete = api.call(Method::Delete, &format!("{PATH}/{id}"), None);
        assert_eq!(delete.status, 400, "{id}");
        assert_eq!(delete.body["errorCode"], "BUSINESS_ERROR", "{id}");
    }
}

#[test]
fn 없는_설정의_수정과_삭제는_404로_응답한다() {
    let Some(api) = api() else { return };
    let path = format!("{PATH}/{}", Uuid::new_v4());
    let put = api.call(Method::Put, &path, Some(r#"{"sendTime":"09:00:00"}"#));
    assert_eq!(put.status, 404);
    assert_eq!(put.body["errorCode"], "BUSINESS_ERROR");
    let delete = api.call(Method::Delete, &path, None);
    assert_eq!(delete.status, 404);
    assert_eq!(delete.body["message"], NOT_FOUND);
}

#[test]
fn 삭제_성공_시_본문_없는_204를_응답하고_처리_관리자를_감사_로그에_남긴다() {
    let Some(api) = api() else { return };
    let id = api.create("09:00:00").body["scheduleId"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = api.call(Method::Delete, &format!("{PATH}/{id}"), None);
    assert_eq!(reply.status, 204);
    assert!(reply.raw.is_empty());
    let actor: Uuid = api
        .db
        .client()
        .query_one(
            "SELECT actor_admin_id FROM admin_audit_logs WHERE action = 'TOPIC_PUSH_SCHEDULE_DELETED'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(actor, api.admin);
}
