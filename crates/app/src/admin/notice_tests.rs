//! 관리자 공지 관리 테스트. 원본 `AdminNoticeServiceTest`, `AdminNoticeControllerTest`, `AdminNoticeApiTest`,
//! `AdminNoticeAuditTransactionTest`, `AdminNoticeConcurrencyTest`에 대응한다.
//!
//! 서비스를 목으로 바꾸지 않고 실제 DB로 라우터까지 통과시킨다(`endpoint_tests.rs`와 같은 방식).
//! `CHALKAK_TEST_DB_URL`이 없으면 DB가 필요한 테스트는 건너뛴다.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chalkak_http::{Handler, Method, Request, Router};
use jiff::Timestamp;
use postgres::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use super::notice::AdminNoticeService;
use super::support::AdminDeps;
use super::testing::{NOW, deps, deps_at, insert_admin};
use crate::common::sha256_hex;
use crate::notification::distribution::distribute_notices;
use crate::notification::notice::{NoticeRepository, NoticeStatus};
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::{ApiError, Routes};

const BODY: &str = "점검합니다.";
const SCHEDULED_AT: &str = "2026-10-19T09:00:00Z";

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn count(client: &mut Client, table: &str) -> i64 {
    client
        .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

/// 공지를 SQL로 넣는다(`created_at`으로 등록 순서를 정한다).
fn insert_notice(client: &mut Client, title: &str, created_at: &str) -> Uuid {
    client
        .query_one(
            "INSERT INTO notices (title, body, scheduled_at, created_at, updated_at) \
             VALUES ($1, $2, $3::text::timestamptz, $4::text::timestamptz, $4::text::timestamptz) \
             RETURNING id",
            &[&title, &BODY, &SCHEDULED_AT, &created_at],
        )
        .unwrap()
        .get(0)
}

fn audit_rows(client: &mut Client, target: Uuid, action: &str) -> Vec<postgres::Row> {
    client
        .query(
            "SELECT actor_admin_id, target_type::text, reason, before_state::text, \
             after_state::text FROM admin_audit_logs WHERE target_id = $1 AND action = $2::text::admin_action",
            &[&target, &action],
        )
        .unwrap()
}

fn json_of(text: String) -> Value {
    serde_json::from_str(&text).unwrap()
}

fn message(error: &ApiError) -> String {
    error.message()
}

struct Api {
    router: Router,
    admin: Uuid,
    db: TestDb,
}

fn api() -> Option<Api> {
    let db = TestDb::create()?;
    let admin = insert_admin(&mut db.client(), "operator");
    let routes = super::register_with(Routes::new(deps(&db).logger.clone()), &deps(&db));
    Some(Api {
        router: routes.into_router(),
        admin,
        db,
    })
}

struct Reply {
    status: u16,
    body: Value,
}

impl Api {
    fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&str>,
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
            req.body = body.as_bytes().to_vec();
        }
        let response = self.router.handle(req);
        Reply {
            status: response.status,
            body: serde_json::from_slice(&response.body).unwrap_or(Value::Null),
        }
    }

    fn request(&self, method: Method, path: &str, body: &str) -> Reply {
        self.call(method, path, &[], Some(body))
    }
}

fn notice_json(title: &str, body: &str, scheduled_at: &str) -> String {
    json!({"title": title, "body": body, "scheduledAt": scheduled_at}).to_string()
}

// ---------------------------------------------------------------------------------------------
// AdminNoticeServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 공지를_등록하면_정규화한_내용을_저장하고_감사_로그에는_본문_대신_해시를_남긴다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let service = AdminNoticeService::new(deps(&db));
    let scheduled_at = ts("2026-10-12T02:00:00.123456Z");

    let result = service
        .create_notice(
            admin,
            " 서비스 점검 안내 ",
            "10월 20일 02:00부터 점검합니다.",
            scheduled_at,
        )
        .unwrap();

    let id = result.id.unwrap();
    let stored = NoticeRepository
        .find_by_id(&mut db.pool(1).get().unwrap(), id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.title, "서비스 점검 안내");
    assert_eq!(stored.body, "10월 20일 02:00부터 점검합니다.");
    assert_eq!(stored.scheduled_at, scheduled_at);
    assert_eq!(result.status(), NoticeStatus::Scheduled);
    assert_eq!(result.distributed_at, None);
    assert_eq!(result.created_at, Some(ts(NOW)));
    assert_eq!(result.updated_at, Some(ts(NOW)));

    let rows = audit_rows(&mut client, id, "NOTICE_CREATED");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<_, Uuid>(0), admin);
    assert_eq!(row.get::<_, String>(1), "NOTICE");
    assert_eq!(row.get::<_, Option<String>>(2), None);
    assert_eq!(json_of(row.get(3)), json!({}));
    assert_eq!(
        json_of(row.get(4)),
        json!({
            "title": "서비스 점검 안내",
            "scheduledAt": "2026-10-12T02:00:00.123456Z",
            "bodySha256": sha256_hex("10월 20일 02:00부터 점검합니다.".as_bytes()),
        })
    );
}

#[test]
fn 본문에_설정값_형식_문구나_이미지_링크가_있어도_공지를_등록한다() {
    let Some(db) = TestDb::create() else { return };
    let admin = insert_admin(&mut db.client(), "notice-manager");
    let body = "비밀번호 정책 변경 안내 — password: 8자 이상\nhttps://cdn.example.com/banner.png";
    let result = AdminNoticeService::new(deps(&db))
        .create_notice(admin, "비밀번호 정책 변경 안내", body, ts(NOW))
        .unwrap();
    let stored = NoticeRepository
        .find_by_id(&mut db.pool(1).get().unwrap(), result.id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.body, body);
}

#[test]
fn 현재_시각_기준_5분_넘게_과거인_예약_시각은_거절하고_저장하지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let error = AdminNoticeService::new(deps(&db))
        .create_notice(admin, "서비스 점검 안내", BODY, ts("2026-10-12T00:00:00Z"))
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(
        message(&error),
        "공지 예약 시각은 현재 시각보다 5분 넘게 과거일 수 없습니다."
    );
    assert_eq!(count(&mut client, "notices"), 0);
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 공지_목록은_등록_최신순으로_요청한_페이지만_조회하고_다음_페이지_여부를_알려준다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let oldest = insert_notice(&mut client, "첫 공지", "2026-10-10T00:00:00Z");
    let middle = insert_notice(&mut client, "둘째 공지", "2026-10-11T00:00:00Z");
    let latest = insert_notice(&mut client, "셋째 공지", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));

    let first = service.get_notices(1, 2).unwrap();
    let second = service.get_notices(2, 2).unwrap();

    let ids = |page: &super::support::AdminPage<_>| -> Vec<Uuid> {
        page.items
            .iter()
            .map(|n: &crate::notification::notice::Notice| n.id.unwrap())
            .collect()
    };
    assert_eq!(ids(&first), vec![latest, middle]);
    assert!(first.has_next);
    assert_eq!((first.current_page, first.page_size), (1, 2));
    assert_eq!(ids(&second), vec![oldest]);
    assert!(!second.has_next);
}

#[test]
fn 등록된_공지가_없으면_빈_목록을_조회한다() {
    let Some(db) = TestDb::create() else { return };
    let result = AdminNoticeService::new(deps(&db))
        .get_notices(1, 20)
        .unwrap();
    assert!(result.items.is_empty());
    assert!(!result.has_next);
}

#[test]
fn 페이지_번호나_페이지_크기가_범위를_벗어나면_거절한다() {
    let Some(db) = TestDb::create() else { return };
    let service = AdminNoticeService::new(deps(&db));
    for (page, size) in [(0, 20), (1, 0), (1, 101)] {
        let error = service.get_notices(page, size).unwrap_err();
        assert_eq!(error.status(), 400, "{page} {size}");
        assert_eq!(message(&error), "조회 조건이 올바르지 않습니다.");
    }
}

#[test]
fn 공지_상세는_저장된_내용을_조회하고_없으면_404다() {
    let Some(db) = TestDb::create() else { return };
    let id = insert_notice(&mut db.client(), "서비스 점검 안내", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));
    let notice = service.get_notice(id).unwrap();
    assert_eq!(notice.title, "서비스 점검 안내");
    assert_eq!(notice.body, BODY);
    assert_eq!(notice.scheduled_at, ts(SCHEDULED_AT));

    let error = service.get_notice(Uuid::new_v4()).unwrap_err();
    assert_eq!(error.status(), 404);
    assert_eq!(message(&error), "공지를 찾을 수 없습니다.");
}

#[test]
fn 예약_상태의_공지를_수정하면_정규화한_내용을_저장하고_변경_전후를_감사_로그에_남긴다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-11T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));
    let scheduled_at = ts("2026-10-14T01:00:00Z");

    let result = service
        .update_notice(
            id,
            admin,
            " 점검 일정 변경 ",
            "10월 21일로 변경합니다.",
            scheduled_at,
        )
        .unwrap();

    assert_eq!(result.title, "점검 일정 변경");
    assert_eq!(result.status(), NoticeStatus::Scheduled);
    assert_eq!(result.updated_at, Some(ts(NOW)));
    let stored = service.get_notice(id).unwrap();
    assert_eq!(stored.body, "10월 21일로 변경합니다.");
    assert_eq!(stored.scheduled_at, scheduled_at);
    assert_eq!(stored.updated_at, Some(ts(NOW)));

    let rows = audit_rows(&mut client, id, "NOTICE_UPDATED");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, Option<String>>(2), None);
    assert_eq!(
        json_of(rows[0].get(3)),
        json!({
            "title": "서비스 점검 안내",
            "scheduledAt": SCHEDULED_AT,
            "bodySha256": sha256_hex(BODY.as_bytes()),
        })
    );
    assert_eq!(
        json_of(rows[0].get(4)),
        json!({
            "title": "점검 일정 변경",
            "scheduledAt": "2026-10-14T01:00:00Z",
            "bodySha256": sha256_hex("10월 21일로 변경합니다.".as_bytes()),
        })
    );
}

#[test]
fn 같은_내용으로_수정하면_바뀌지_않은_공지를_그대로_응답하고_감사_로그를_남기지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));

    // 나노초만 다른 같은 예약 시각도 변경으로 보지 않는다(저장 정밀도인 마이크로초로 맞춘다).
    for scheduled_at in [ts(SCHEDULED_AT), ts("2026-10-19T09:00:00.000000789Z")] {
        let result = service
            .update_notice(id, admin, " 서비스 점검 안내 ", BODY, scheduled_at)
            .unwrap();
        assert_eq!(result.title, "서비스 점검 안내");
        assert_eq!(result.updated_at, Some(ts("2026-10-12T00:00:00Z")));
    }
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 배포된_공지는_수정을_거절하고_내용과_감사_로그를_바꾸지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    client
        .execute(
            "UPDATE notices SET distributed_at = scheduled_at WHERE id = $1",
            &[&id],
        )
        .unwrap();
    let service = AdminNoticeService::new(deps(&db));

    let error = service
        .update_notice(id, admin, "점검 일정 변경", BODY, ts(SCHEDULED_AT))
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(message(&error), "배포된 공지는 수정할 수 없습니다.");
    assert_eq!(service.get_notice(id).unwrap().title, "서비스 점검 안내");
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 삭제된_공지는_수정을_거절하고_없는_공지는_404다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    client
        .execute(
            "UPDATE notices SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1",
            &[&id],
        )
        .unwrap();
    let service = AdminNoticeService::new(deps(&db));

    let error = service
        .update_notice(id, admin, "점검 일정 변경", BODY, ts(NOW))
        .unwrap_err();
    assert_eq!(message(&error), "삭제된 공지는 수정할 수 없습니다.");

    let error = service
        .update_notice(Uuid::new_v4(), admin, "점검 일정 변경", BODY, ts(NOW))
        .unwrap_err();
    assert_eq!(error.status(), 404);
    assert_eq!(message(&error), "공지를 찾을 수 없습니다.");
}

#[test]
fn 예약_상태의_공지를_삭제하면_발송을_취소하고_사유와_함께_감사_로그를_남긴다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));

    service
        .delete_notice(id, admin, " 점검 일정 취소 ")
        .unwrap();

    let notice = service.get_notice(id).unwrap();
    assert_eq!(notice.deleted_at, Some(ts(NOW)));
    assert_eq!(notice.status(), NoticeStatus::Scheduled);
    let rows = audit_rows(&mut client, id, "NOTICE_DELETED");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, Option<String>>(2).as_deref(),
        Some("점검 일정 취소")
    );
    let before = json_of(rows[0].get(3));
    assert_eq!(before["title"], "서비스 점검 안내");
    assert_eq!(before["bodySha256"], sha256_hex(BODY.as_bytes()));
    assert_eq!(before["deletedAt"], Value::Null);
    assert_eq!(json_of(rows[0].get(4))["deletedAt"], NOW);
}

#[test]
fn 배포된_공지를_삭제하면_배포_시각을_유지한_채_삭제_시각을_기록한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    client
        .execute(
            "UPDATE notices SET distributed_at = scheduled_at WHERE id = $1",
            &[&id],
        )
        .unwrap();
    let service = AdminNoticeService::new(deps(&db));

    service.delete_notice(id, admin, "잘못 보낸 공지").unwrap();

    let notice = service.get_notice(id).unwrap();
    assert!(notice.deleted_at.is_some());
    assert_eq!(notice.status(), NoticeStatus::Distributed);
    assert_eq!(notice.distributed_at, Some(ts(SCHEDULED_AT)));
}

#[test]
fn 이미_삭제한_공지를_다시_삭제하면_최초_삭제_시각을_유지하고_감사_로그를_추가하지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    client
        .execute(
            "UPDATE notices SET deleted_at = '2026-10-13T00:00:00Z' WHERE id = $1",
            &[&id],
        )
        .unwrap();
    let service = AdminNoticeService::new(deps(&db));

    service.delete_notice(id, admin, "다시 삭제").unwrap();

    assert_eq!(
        service.get_notice(id).unwrap().deleted_at,
        Some(ts("2026-10-13T00:00:00Z"))
    );
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 삭제_사유가_없거나_공백뿐이면_거절하고_공지를_삭제하지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));
    for reason in ["", "   "] {
        let error = service.delete_notice(id, admin, reason).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(
            message(&error),
            "공지 삭제 사유는 1자 이상 500자 이하여야 합니다."
        );
    }
    assert_eq!(service.get_notice(id).unwrap().deleted_at, None);
}

#[test]
fn 삭제_사유는_앞뒤_공백을_제거한_뒤_500자까지_허용하고_501자는_거절한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let rejected = insert_notice(&mut client, "거절할 공지", "2026-10-12T00:00:00Z");
    let accepted = insert_notice(&mut client, "삭제할 공지", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));

    let error = service
        .delete_notice(rejected, admin, &"가".repeat(501))
        .unwrap_err();
    assert_eq!(
        message(&error),
        "공지 삭제 사유는 1자 이상 500자 이하여야 합니다."
    );
    service
        .delete_notice(accepted, admin, &format!(" {} ", "가".repeat(500)))
        .unwrap();
    assert!(service.get_notice(accepted).unwrap().deleted_at.is_some());
}

#[test]
fn 없는_공지를_삭제하면_404다() {
    let Some(db) = TestDb::create() else { return };
    let admin = insert_admin(&mut db.client(), "notice-manager");
    let error = AdminNoticeService::new(deps(&db))
        .delete_notice(Uuid::new_v4(), admin, "점검 일정 취소")
        .unwrap_err();
    assert_eq!(error.status(), 404);
    assert_eq!(message(&error), "공지를 찾을 수 없습니다.");
}

#[test]
fn 공지_목록은_삭제된_공지도_삭제_시각과_함께_보여준다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    client
        .execute(
            "UPDATE notices SET deleted_at = '2026-10-13T00:00:00Z' WHERE id = $1",
            &[&id],
        )
        .unwrap();
    let result = AdminNoticeService::new(deps(&db))
        .get_notices(1, 20)
        .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].deleted_at, Some(ts("2026-10-13T00:00:00Z")));
}

// ---------------------------------------------------------------------------------------------
// AdminNoticeAuditTransactionTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 감사_로그_저장이_실패하면_공지_등록도_함께_롤백한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    // 없는 관리자라 감사 로그의 외래 키가 실패한다.
    let error = AdminNoticeService::new(deps(&db))
        .create_notice(Uuid::new_v4(), "서비스 점검 안내", BODY, ts(NOW))
        .unwrap_err();
    assert_eq!(error.status(), 500);
    assert_eq!(count(&mut client, "notices"), 0);
}

#[test]
fn 감사_로그가_민감_정보로_판정하는_형식을_제목에_포함하면_공지_등록도_함께_롤백한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let error = AdminNoticeService::new(deps(&db))
        .create_notice(
            admin,
            "password: 8자 이상",
            "비밀번호 정책이 변경됩니다.",
            ts(NOW),
        )
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(
        message(&error),
        "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
    );
    assert_eq!(count(&mut client, "notices"), 0);
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 감사_로그가_민감_정보로_판정하는_형식을_수정_제목에_포함하면_공지_수정도_함께_롤백한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));
    let error = service
        .update_notice(
            id,
            admin,
            "password: 8자 이상",
            "비밀번호 정책이 변경됩니다.",
            ts(NOW),
        )
        .unwrap_err();
    assert_eq!(
        message(&error),
        "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
    );
    assert_eq!(service.get_notice(id).unwrap().title, "서비스 점검 안내");
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 감사_로그_저장이_실패하면_공지_삭제도_함께_롤백한다() {
    let Some(db) = TestDb::create() else { return };
    let id = insert_notice(&mut db.client(), "서비스 점검 안내", "2026-10-12T00:00:00Z");
    let service = AdminNoticeService::new(deps(&db));
    let error = service
        .delete_notice(id, Uuid::new_v4(), "점검 일정 취소")
        .unwrap_err();
    assert_eq!(error.status(), 500);
    assert_eq!(service.get_notice(id).unwrap().deleted_at, None);
}

// ---------------------------------------------------------------------------------------------
// AdminNoticeConcurrencyTest: 배포와 관리자 변경이 같은 공지 행 잠금으로 겹치지 않는다.
// ---------------------------------------------------------------------------------------------

/// 공지 행을 잠근 채 `release` 신호를 기다리는 트랜잭션을 다른 스레드에서 연다. 잠그면 `locked`로 알린다.
fn hold_notice_lock(
    deps: &AdminDeps,
    notice_id: Uuid,
    distribute: bool,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let deps = deps.clone();
    let handle = thread::spawn(move || {
        let mut conn = deps.pool.get().unwrap();
        super::support::in_transaction(&mut conn, |tx| {
            let now = deps.now();
            if distribute {
                // 배포 트랜잭션이 하는 일: 잠그고, 알림을 만들고, 배포 표시를 한다.
                let mut notice = NoticeRepository
                    .find_distributable_for_update(tx, notice_id, now)?
                    .unwrap();
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                NoticeRepository.create_notice_notifications(tx, &notice, now)?;
                notice.distribute(now)?;
                NoticeRepository.save_and_flush(tx, &mut notice, now)?;
            } else {
                // 수정·삭제가 하는 일: 행만 잠근다.
                NoticeRepository
                    .find_by_id_for_update(tx, notice_id)?
                    .unwrap();
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
    });
    (locked_rx, release_tx, handle)
}

fn due_notice(client: &mut Client) -> Uuid {
    client
        .query_one(
            "INSERT INTO notices (title, body, scheduled_at, created_at, updated_at) \
             VALUES ('서비스 점검 안내', '점검합니다.', '2026-10-12T00:59:00Z', '2026-10-12T00:00:00Z', \
             '2026-10-12T00:00:00Z') RETURNING id",
            &[],
        )
        .unwrap()
        .get(0)
}

#[test]
fn 배포가_먼저_공지를_잠그면_수정은_배포_커밋을_기다린_뒤_거절된다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = due_notice(&mut client);
    let deps = deps(&db);
    let (locked, release, distribution) = hold_notice_lock(&deps, id, true);
    locked.recv().unwrap();

    let service = AdminNoticeService::new(deps.clone());
    let update =
        thread::spawn(move || service.update_notice(id, admin, "점검 일정 변경", BODY, ts(NOW)));
    thread::sleep(Duration::from_millis(300));
    assert!(!update.is_finished());
    release.send(()).unwrap();
    distribution.join().unwrap();

    let error = update.join().unwrap().unwrap_err();
    assert_eq!(message(&error), "배포된 공지는 수정할 수 없습니다.");
    assert_eq!(
        client
            .query_one("SELECT title FROM notices", &[])
            .unwrap()
            .get::<_, String>(0),
        "서비스 점검 안내"
    );
}

#[test]
fn 수정이_공지를_잠그고_있으면_배포는_그_공지를_건너뛰고_다음_확인_때_배포한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    insert_admin(&mut client, "notice-manager");
    let id = due_notice(&mut client);
    let deps = deps(&db);
    let (locked, release, update) = hold_notice_lock(&deps, id, false);
    locked.recv().unwrap();

    distribute_notices(&deps.pool, &*deps.clock, &deps.logger).unwrap();
    let skipped: Option<Timestamp> = client
        .query_one("SELECT distributed_at FROM notices WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert_eq!(skipped, None);

    release.send(()).unwrap();
    update.join().unwrap();
    distribute_notices(&deps.pool, &*deps.clock, &deps.logger).unwrap();
    let distributed: Option<Timestamp> = client
        .query_one("SELECT distributed_at FROM notices WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert!(distributed.is_some());
}

#[test]
fn 배포가_먼저_공지를_잠그면_삭제는_배포_커밋을_기다린_뒤_배포된_공지를_삭제한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = due_notice(&mut client);
    let deps = deps(&db);
    let (locked, release, distribution) = hold_notice_lock(&deps, id, true);
    locked.recv().unwrap();

    let service = AdminNoticeService::new(deps.clone());
    let delete = thread::spawn(move || service.delete_notice(id, admin, "잘못 보낸 공지"));
    thread::sleep(Duration::from_millis(300));
    assert!(!delete.is_finished());
    release.send(()).unwrap();
    distribution.join().unwrap();
    delete.join().unwrap().unwrap();

    let row = client
        .query_one(
            "SELECT distributed_at IS NOT NULL, deleted_at IS NOT NULL FROM notices WHERE id = $1",
            &[&id],
        )
        .unwrap();
    assert!(row.get::<_, bool>(0));
    assert!(row.get::<_, bool>(1));
}

#[test]
fn 삭제가_공지를_잠그고_있으면_배포는_그_공지를_건너뛰고_삭제_후에도_배포하지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let id = due_notice(&mut client);
    let deps_ = deps(&db);
    // 삭제 트랜잭션이 행을 잡고 있는 동안 배포가 지나가게 하려고 잠금만 쥐는 스레드를 쓰고, 풀린 뒤 실제 삭제를 한다.
    let (locked, release, holder) = hold_notice_lock(&deps_, id, false);
    locked.recv().unwrap();
    distribute_notices(&deps_.pool, &*deps_.clock, &deps_.logger).unwrap();
    release.send(()).unwrap();
    holder.join().unwrap();

    AdminNoticeService::new(deps_.clone())
        .delete_notice(id, admin, "점검 일정 취소")
        .unwrap();
    distribute_notices(&deps_.pool, &*deps_.clock, &deps_.logger).unwrap();

    let row = client
        .query_one(
            "SELECT distributed_at IS NULL, deleted_at IS NOT NULL FROM notices WHERE id = $1",
            &[&id],
        )
        .unwrap();
    assert!(row.get::<_, bool>(0));
    assert!(row.get::<_, bool>(1));
    assert_eq!(count(&mut client, "notifications"), 0);
}

// ---------------------------------------------------------------------------------------------
// AdminNoticeControllerTest, AdminNoticeApiTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 목록_조회는_페이지_정보와_공지_목록을_응답하고_생략하면_1페이지_20개다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let latest = insert_notice(&mut client, "둘째", "2026-10-12T09:00:00Z");
    insert_notice(&mut client, "첫째", "2026-10-11T09:00:00Z");

    let default = api.call(Method::Get, "/api/v1/admin/notices", &[], None);
    assert_eq!(default.status, 200);
    assert_eq!(default.body["currentPage"], 1);
    assert_eq!(default.body["pageSize"], 20);
    assert_eq!(default.body["hasNext"], false);
    assert_eq!(default.body["notices"].as_array().unwrap().len(), 2);

    let paged = api.call(
        Method::Get,
        "/api/v1/admin/notices",
        &[("page", "1"), ("pageSize", "1")],
        None,
    );
    assert_eq!(paged.status, 200);
    assert_eq!(paged.body["hasNext"], true);
    let first = &paged.body["notices"][0];
    assert_eq!(first["noticeId"], latest.hyphenated().to_string());
    assert_eq!(first["title"], "둘째");
    assert_eq!(first["body"], BODY);
    assert_eq!(first["scheduledAt"], SCHEDULED_AT);
    assert_eq!(first["status"], "SCHEDULED");
    assert_eq!(first["distributedAt"], Value::Null);
    assert_eq!(first["deletedAt"], Value::Null);
    assert_eq!(first["createdAt"], "2026-10-12T09:00:00Z");
    assert_eq!(first["updatedAt"], "2026-10-12T09:00:00Z");
}

#[test]
fn 페이지_번호_0이나_페이지_크기_0_101은_400으로_응답한다() {
    let Some(api) = api() else { return };
    for (name, value) in [("page", "0"), ("pageSize", "0"), ("pageSize", "101")] {
        let reply = api.call(Method::Get, "/api/v1/admin/notices", &[(name, value)], None);
        assert_eq!(reply.status, 400, "{name}={value}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(reply.body["message"], "조회 조건이 올바르지 않습니다.");
    }
}

#[test]
fn 상세_조회는_배포_상태와_삭제_시각을_응답하고_잘못된_id와_없는_공지는_400_404다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-12T09:00:00Z");
    client
        .execute(
            "UPDATE notices SET distributed_at = scheduled_at, deleted_at = '2026-10-19T10:00:00Z' WHERE id = $1",
            &[&id],
        )
        .unwrap();

    let reply = api.call(
        Method::Get,
        &format!("/api/v1/admin/notices/{id}"),
        &[],
        None,
    );
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body["status"], "DISTRIBUTED");
    assert_eq!(reply.body["distributedAt"], SCHEDULED_AT);
    assert_eq!(reply.body["deletedAt"], "2026-10-19T10:00:00Z");

    let bad = api.call(Method::Get, "/api/v1/admin/notices/not-a-uuid", &[], None);
    assert_eq!(bad.status, 400);
    assert_eq!(bad.body["errorCode"], "BUSINESS_ERROR");
    let missing = api.call(
        Method::Get,
        &format!("/api/v1/admin/notices/{}", Uuid::new_v4()),
        &[],
        None,
    );
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body["message"], "공지를 찾을 수 없습니다.");
}

#[test]
fn 등록_성공_시_201과_저장된_공지를_응답하고_처리_관리자를_감사_로그에_남긴다() {
    let Some(api) = api() else { return };
    let reply = api.request(
        Method::Post,
        "/api/v1/admin/notices",
        &notice_json("서비스 점검 안내", BODY, "2026-10-19T09:00:00Z"),
    );
    assert_eq!(reply.status, 201);
    assert_eq!(reply.body["title"], "서비스 점검 안내");
    assert_eq!(reply.body["scheduledAt"], SCHEDULED_AT);
    assert_eq!(reply.body["status"], "SCHEDULED");
    assert_eq!(reply.body["createdAt"], NOW);
    assert_eq!(reply.body["updatedAt"], NOW);

    let mut client = api.db.client();
    let actor: Uuid = client
        .query_one(
            "SELECT actor_admin_id FROM admin_audit_logs WHERE action = 'NOTICE_CREATED'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(actor, api.admin);
}

#[test]
fn 제목_본문_누락이나_공백_예약_시각_누락이나_형식_오류는_400으로_응답한다() {
    let Some(api) = api() else { return };
    for body in [
        r#"{"body":"점검합니다.","scheduledAt":"2026-10-19T09:00:00Z"}"#,
        r#"{"title":" ","body":"점검합니다.","scheduledAt":"2026-10-19T09:00:00Z"}"#,
        r#"{"title":"안내","scheduledAt":"2026-10-19T09:00:00Z"}"#,
        r#"{"title":"안내","body":" ","scheduledAt":"2026-10-19T09:00:00Z"}"#,
        r#"{"title":"안내","body":"점검합니다."}"#,
        r#"{"title":"안내","body":"점검합니다.","scheduledAt":"2026-10-19 09:00"}"#,
    ] {
        for (method, path) in [
            (Method::Post, "/api/v1/admin/notices".to_owned()),
            (
                Method::Put,
                format!("/api/v1/admin/notices/{}", Uuid::new_v4()),
            ),
        ] {
            let reply = api.request(method, &path, body);
            assert_eq!(reply.status, 400, "{body}");
            assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR", "{body}");
        }
    }
    assert_eq!(count(&mut api.db.client(), "notices"), 0);
}

#[test]
fn 서비스의_업무_예외는_400과_business_error로_응답한다() {
    let Some(api) = api() else { return };
    let reply = api.request(
        Method::Post,
        "/api/v1/admin/notices",
        &notice_json("서비스 점검 안내", BODY, "2026-10-01T09:00:00Z"),
    );
    assert_eq!(reply.status, 400);
    assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    assert_eq!(
        reply.body["message"],
        "공지 예약 시각은 현재 시각보다 5분 넘게 과거일 수 없습니다."
    );
}

#[test]
fn 수정은_200과_수정된_공지를_응답하고_배포된_공지와_없는_공지와_잘못된_id는_거절한다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-11T09:00:00Z");
    let path = format!("/api/v1/admin/notices/{id}");

    let updated = api.request(
        Method::Put,
        &path,
        &notice_json("점검 일정 변경", BODY, "2026-10-19T09:00:00Z"),
    );
    assert_eq!(updated.status, 200);
    assert_eq!(updated.body["noticeId"], id.hyphenated().to_string());
    assert_eq!(updated.body["title"], "점검 일정 변경");
    assert_eq!(updated.body["updatedAt"], NOW);

    let bad_id = api.request(
        Method::Put,
        "/api/v1/admin/notices/not-a-uuid",
        &notice_json("점검 일정 변경", BODY, "2026-10-19T09:00:00Z"),
    );
    assert_eq!(bad_id.status, 400);
    assert_eq!(bad_id.body["errorCode"], "BUSINESS_ERROR");

    let missing = api.request(
        Method::Put,
        &format!("/api/v1/admin/notices/{}", Uuid::new_v4()),
        &notice_json("점검 일정 변경", BODY, "2026-10-19T09:00:00Z"),
    );
    assert_eq!(missing.status, 404);

    client
        .execute(
            "UPDATE notices SET distributed_at = scheduled_at WHERE id = $1",
            &[&id],
        )
        .unwrap();
    let distributed = api.request(
        Method::Put,
        &path,
        &notice_json("또 변경", BODY, "2026-10-19T09:00:00Z"),
    );
    assert_eq!(distributed.status, 400);
    assert_eq!(
        distributed.body["message"],
        "배포된 공지는 수정할 수 없습니다."
    );
}

#[test]
fn 삭제는_204로_응답하고_사유_누락_공백_초과_잘못된_id_없는_공지는_거절한다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let id = insert_notice(&mut client, "서비스 점검 안내", "2026-10-11T09:00:00Z");
    let path = format!("/api/v1/admin/notices/{id}");

    for body in [
        "{}",
        r#"{"reason":" "}"#,
        &json!({"reason": "가".repeat(501)}).to_string(),
    ] {
        let reply = api.request(Method::Delete, &path, body);
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    }
    let bad_id = api.request(
        Method::Delete,
        "/api/v1/admin/notices/not-a-uuid",
        r#"{"reason":"점검 일정 취소"}"#,
    );
    assert_eq!(bad_id.status, 400);
    let missing = api.request(
        Method::Delete,
        &format!("/api/v1/admin/notices/{}", Uuid::new_v4()),
        r#"{"reason":"점검 일정 취소"}"#,
    );
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body["message"], "공지를 찾을 수 없습니다.");

    let deleted = api.request(Method::Delete, &path, r#"{"reason":"점검 일정 취소"}"#);
    assert_eq!(deleted.status, 204);
    let again = api.request(Method::Delete, &path, r#"{"reason":"다시 삭제"}"#);
    assert_eq!(again.status, 204);
    assert_eq!(audit_rows(&mut client, id, "NOTICE_DELETED").len(), 1);
}

// 감사 로그 시각이 요청 시각(고정 시계)으로 남는지 확인하려고 시계를 옮긴 설정을 쓴다.
#[test]
fn 감사_로그와_공지_시각은_서비스의_시계를_따른다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "notice-manager");
    let service = AdminNoticeService::new(deps_at(&db, "2026-10-13T00:00:00Z"));
    let result = service
        .create_notice(admin, "서비스 점검 안내", BODY, ts("2026-10-13T00:00:00Z"))
        .unwrap();
    assert_eq!(result.created_at, Some(ts("2026-10-13T00:00:00Z")));
    let occurred_at: Timestamp = client
        .query_one(
            "SELECT occurred_at FROM admin_audit_logs WHERE target_id = $1",
            &[&result.id.unwrap()],
        )
        .unwrap()
        .get(0);
    assert_eq!(occurred_at, ts("2026-10-13T00:00:00Z"));
}
