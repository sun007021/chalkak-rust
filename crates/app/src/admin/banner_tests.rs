//! 배너 테스트. 원본 `AdminBannerServiceTest`, `AdminBannerDeletionServiceTest`, `AdminBannerTransactionTest`,
//! `AdminBannerApiTest`, `AdminBannerControllerTest`, `BannerApiTest`, `BannerControllerTest`, `BannerServiceTest`,
//! `BannerRepositoryTest`에 대응한다.
//!
//! 서비스를 목으로 바꾸지 않고 실제 DB로 라우터까지 통과시킨다(`notice_tests.rs`와 같은 방식).
//! `CHALKAK_TEST_DB_URL`이 없으면 DB가 필요한 테스트는 건너뛴다.

use chalkak_http::{Handler, Method, Request, Router};
use postgres::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use super::banner::AdminBannerService;
use super::testing::{deps, insert_admin};
use crate::banner::domain::Banner;
use crate::banner::repository::{BannerRepository, SaveError};
use crate::common::sha256_hex;
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::Routes;

const URL: &str = "https://chalkak.vercel.app/event";
const THUMBNAIL: &str = "https://cdn.example.com/banner.png";

fn count(client: &mut Client, table: &str) -> i64 {
    client
        .query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

fn audit_rows(client: &mut Client, target: Uuid, action: &str) -> Vec<postgres::Row> {
    client
        .query(
            "SELECT actor_admin_id, target_type::text, reason, before_state::text, after_state::text \
             FROM admin_audit_logs WHERE target_id = $1 AND action = $2::text::admin_action",
            &[&target, &action],
        )
        .unwrap()
}

fn json_of(text: String) -> Value {
    serde_json::from_str(&text).unwrap()
}

/// 배너를 SQL로 넣는다(`created_at`으로 등록 순서를 정한다).
fn insert_banner(client: &mut Client, web_view_url: &str, created_at: &str) -> Uuid {
    client
        .query_one(
            "INSERT INTO banners (title, subtitle, web_view_url, thumbnail_url, created_at, updated_at) \
             VALUES ('제목', '부제목', $1, $2, $3::text::timestamptz, $3::text::timestamptz) RETURNING id",
            &[&web_view_url, &THUMBNAIL, &created_at],
        )
        .unwrap()
        .get(0)
}

struct Api {
    router: Router,
    admin: Uuid,
    db: TestDb,
}

fn api() -> Option<Api> {
    let db = TestDb::create()?;
    let admin = insert_admin(&mut db.client(), "banner-manager");
    let deps = deps(&db);
    let routes = Routes::new(deps.logger.clone());
    let routes = super::register_with(routes, &deps);
    let ctx_routes = crate::banner::api::register_with_pool(routes, deps.pool.clone());
    Some(Api {
        router: ctx_routes.into_router(),
        admin,
        db,
    })
}

struct Reply {
    status: u16,
    body: Value,
    raw: Vec<u8>,
}

impl Api {
    fn call(
        &self,
        principal: Option<Principal>,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        let _guard = principal.map(security::enter);
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
            raw: response.body,
        }
    }

    fn admin_principal(&self) -> Principal {
        Principal {
            subject: self.admin.hyphenated().to_string(),
            authorities: vec![AUTHORITY_ADMIN.to_owned()],
        }
    }

    fn admin(&self, method: Method, path: &str, body: Option<&str>) -> Reply {
        self.call(Some(self.admin_principal()), method, path, &[], body)
    }

    fn list(&self, query: &[(&str, &str)]) -> Reply {
        self.call(None, Method::Get, "/api/v1/banners", query, None)
    }

    fn create(&self, web_view_url: &str) -> Reply {
        self.admin(
            Method::Post,
            "/api/v1/admin/banners",
            Some(&banner_json("제목", "부제목", web_view_url, THUMBNAIL)),
        )
    }
}

fn banner_json(title: &str, subtitle: &str, web_view_url: &str, thumbnail_url: &str) -> String {
    json!({
        "title": title,
        "subtitle": subtitle,
        "webViewUrl": web_view_url,
        "thumbnailUrl": thumbnail_url,
    })
    .to_string()
}

// ---------------------------------------------------------------------------------------------
// AdminBannerServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 배너_등록은_정규화한_정보를_저장하고_url_없이_감사_로그를_남긴다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let service = AdminBannerService::new(deps(&db));

    let created = service
        .create_banner(
            admin,
            " 제목 ",
            " 부제목 ",
            &format!(" {URL} "),
            " https://cdn.example.com/banner.png ",
        )
        .unwrap();

    let id = created.id.unwrap();
    assert_eq!(id.get_version_num(), 7);
    let row = client
        .query_one(
            "SELECT title, subtitle, web_view_url, thumbnail_url, created_at, updated_at, deleted_at \
             FROM banners WHERE id = $1",
            &[&id],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>("title"), "제목");
    assert_eq!(row.get::<_, String>("subtitle"), "부제목");
    assert_eq!(row.get::<_, String>("web_view_url"), URL);
    assert_eq!(row.get::<_, String>("thumbnail_url"), THUMBNAIL);
    assert_eq!(
        row.get::<_, jiff::Timestamp>("created_at"),
        created.created_at.unwrap()
    );
    assert!(
        row.get::<_, Option<jiff::Timestamp>>("deleted_at")
            .is_none()
    );
    let audits = audit_rows(&mut client, id, "BANNER_CREATED");
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].get::<_, Uuid>("actor_admin_id"), admin);
    assert_eq!(audits[0].get::<_, String>("target_type"), "BANNER");
    assert_eq!(audits[0].get::<_, Option<String>>("reason"), None);
    assert_eq!(json_of(audits[0].get("before_state")), json!({}));
    assert_eq!(
        json_of(audits[0].get("after_state")),
        json!({
            "titleSha256": sha256_hex("제목".as_bytes()),
            "subtitleSha256": sha256_hex("부제목".as_bytes()),
        })
    );
}

#[test]
fn 잘못된_도메인_입력이면_배너와_감사_기록을_저장하지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let service = AdminBannerService::new(deps(&db));

    let error = service
        .create_banner(admin, "제목", "부제목", "http://example.com", THUMBNAIL)
        .unwrap_err();

    assert_eq!(error.status(), 400);
    assert_eq!(count(&mut client, "banners"), 0);
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 쿼리나_프래그먼트가_다르면_별도_배너를_등록한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let service = AdminBannerService::new(deps(&db));
    service
        .create_banner(admin, "제목", "부제목", URL, THUMBNAIL)
        .unwrap();
    for suffix in ["?source=home", "?source=notice", "#camera"] {
        service
            .create_banner(
                admin,
                "제목",
                "부제목",
                &format!("{URL}{suffix}"),
                THUMBNAIL,
            )
            .unwrap();
    }
    assert_eq!(count(&mut client, "banners"), 4);
}

#[test]
fn 중복은_사전_검사에서_거절해_db_제약_오류_없이_기존_데이터를_유지한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let service = AdminBannerService::new(deps(&db));
    service
        .create_banner(admin, "제목", "부제목", URL, THUMBNAIL)
        .unwrap();

    let error = service
        .create_banner(
            admin,
            "다른 제목",
            "다른 부제목",
            &format!("\u{2003}{URL}\u{2003}"),
            "https://cdn.example.com/other.png",
        )
        .unwrap_err();

    assert_eq!(error.status(), 400);
    assert_eq!(error.message(), "동일한 웹뷰 URL의 배너가 이미 존재합니다.");
    assert_eq!(count(&mut client, "banners"), 1);
    assert_eq!(count(&mut client, "admin_audit_logs"), 1);
}

#[test]
fn 이천자의_다국어_url도_인덱스_크기_오류_없이_저장한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let service = AdminBannerService::new(deps(&db));
    let mut url = String::from(URL);
    while url.chars().count() < 2000 {
        let code = 0x4E00 + url.chars().count() as u32;
        url.push(char::from_u32(code).unwrap());
    }
    let created = service
        .create_banner(admin, "제목", "부제목", &url, THUMBNAIL)
        .unwrap();
    let stored: String = client
        .query_one(
            "SELECT web_view_url FROM banners WHERE id = $1",
            &[&created.id],
        )
        .unwrap()
        .get(0);
    assert_eq!(stored, url);
}

// ---------------------------------------------------------------------------------------------
// BannerRepositoryTest, AdminBannerTransactionTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 사전_검사를_우회한_중복도_저장_안에서_중복_오류로_변환한다() {
    let Some(db) = TestDb::create() else { return };
    let mut conn = db.pool(2).get().unwrap();
    let now = deps(&db).now();
    let mut first =
        Banner::create(Some("제목"), Some("부제목"), Some(URL), Some(THUMBNAIL)).unwrap();
    BannerRepository.insert(&mut conn, &mut first, now).unwrap();
    assert!(first.id.is_some());
    assert_eq!(first.created_at, Some(now));
    let mut second =
        Banner::create(Some("제목2"), Some("부제목2"), Some(URL), Some(THUMBNAIL)).unwrap();
    let error = BannerRepository
        .insert(&mut conn, &mut second, now)
        .unwrap_err();
    assert!(matches!(error, SaveError::DuplicateWebViewUrl));
}

#[test]
fn 사전_조회는_전체_문자열을_비교하고_쿼리_프래그먼트_차이를_구분한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    insert_banner(&mut client, &format!("{URL}?a=1"), "2026-10-19T00:00:00Z");
    let mut conn = db.pool(2).get().unwrap();
    assert!(
        BannerRepository
            .exists_by_web_view_url(&mut conn, &format!("{URL}?a=1"))
            .unwrap()
    );
    assert!(
        !BannerRepository
            .exists_by_web_view_url(&mut conn, URL)
            .unwrap()
    );
    assert!(
        !BannerRepository
            .exists_by_web_view_url(&mut conn, &format!("{URL}?a=1#x"))
            .unwrap()
    );
}

#[test]
fn 감사_기록이_실패하면_배너_등록도_롤백된다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let service = AdminBannerService::new(deps(&db));
    // 존재하지 않는 관리자: 감사 로그의 관리자 FK가 실패한다.
    let error = service
        .create_banner(Uuid::new_v4(), "제목", "부제목", URL, THUMBNAIL)
        .unwrap_err();
    assert_eq!(error.status(), 500);
    assert_eq!(count(&mut client, "banners"), 0);
}

// ---------------------------------------------------------------------------------------------
// AdminBannerDeletionServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 사유_없이_삭제_감사를_기록하고_배너는_목록에서_제외한다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let id = insert_banner(&mut client, URL, "2026-10-19T00:00:00Z");

    let service = AdminBannerService::new(deps(&api.db));
    service.delete_banner(id, api.admin).unwrap();

    let deleted_at: jiff::Timestamp = client
        .query_one("SELECT deleted_at FROM banners WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert_eq!(api.list(&[]).body["banners"], json!([]));
    let audits = audit_rows(&mut client, id, "BANNER_DELETED");
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].get::<_, Option<String>>("reason"), None);
    assert_eq!(audits[0].get::<_, Uuid>("actor_admin_id"), api.admin);
    let before = json_of(audits[0].get("before_state"));
    let after = json_of(audits[0].get("after_state"));
    assert_eq!(before["deletedAt"], Value::Null);
    assert_eq!(before.as_object().unwrap().len(), 3);
    assert_eq!(
        after["deletedAt"],
        json!(crate::web::json::format_instant(deleted_at))
    );
    assert_eq!(after.as_object().unwrap().len(), 3);
}

#[test]
fn 재삭제는_최초_시각과_감사_한_건을_유지한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let id = insert_banner(&mut client, URL, "2026-10-19T00:00:00Z");
    AdminBannerService::new(deps_at_time(&db, "2026-10-12T01:00:00.123456789Z"))
        .delete_banner(id, admin)
        .unwrap();
    let first: jiff::Timestamp = client
        .query_one("SELECT deleted_at FROM banners WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert_eq!(first, "2026-10-12T01:00:00.123456Z".parse().unwrap());

    AdminBannerService::new(deps_at_time(&db, "2026-10-13T00:00:00Z"))
        .delete_banner(id, admin)
        .unwrap();

    let again: jiff::Timestamp = client
        .query_one("SELECT deleted_at FROM banners WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert_eq!(again, first);
    assert_eq!(audit_rows(&mut client, id, "BANNER_DELETED").len(), 1);
}

fn deps_at_time(db: &TestDb, now: &str) -> super::support::AdminDeps {
    super::testing::deps_at(db, now)
}

#[test]
fn 존재하지_않는_배너_삭제는_404이다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-manager");
    let error = AdminBannerService::new(deps(&db))
        .delete_banner(Uuid::new_v4(), admin)
        .unwrap_err();
    assert_eq!(error.status(), 404);
    assert_eq!(error.message(), "배너를 찾을 수 없습니다.");
    assert_eq!(count(&mut client, "admin_audit_logs"), 0);
}

#[test]
fn 삭제한_url로_새_배너를_등록하면_새_id로_활성_목록에_나타난다() {
    let Some(api) = api() else { return };
    let created = api.create(URL);
    assert_eq!(created.status, 201);
    let old_id = created.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        api.admin(
            Method::Delete,
            &format!("/api/v1/admin/banners/{old_id}"),
            None
        )
        .status,
        204
    );

    let again = api.create(URL);

    assert_eq!(again.status, 201);
    let new_id = again.body["id"].as_str().unwrap();
    assert_ne!(new_id, old_id);
    let list = api.list(&[]);
    assert_eq!(list.body["banners"].as_array().unwrap().len(), 1);
    assert_eq!(list.body["banners"][0]["id"], new_id);
    // 같은 활성 URL은 다시 거절한다.
    assert_eq!(api.create(URL).status, 400);
}

#[test]
fn 삭제_감사_기록이_실패하면_삭제_시각도_롤백된다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let id = insert_banner(&mut client, URL, "2026-10-19T00:00:00Z");
    let error = AdminBannerService::new(deps(&db))
        .delete_banner(id, Uuid::new_v4())
        .unwrap_err();
    assert_eq!(error.status(), 500);
    let deleted: Option<jiff::Timestamp> = client
        .query_one("SELECT deleted_at FROM banners WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert!(deleted.is_none());
}

// ---------------------------------------------------------------------------------------------
// AdminBannerApiTest, AdminBannerControllerTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 등록은_201과_여섯_가지_응답_필드를_반환하고_감사_로그에_관리자를_기록한다() {
    let Some(api) = api() else { return };

    let reply = api.create(URL);

    assert_eq!(reply.status, 201);
    let keys: Vec<&str> = reply
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "title",
            "subtitle",
            "webViewUrl",
            "thumbnailUrl",
            "createdAt"
        ]
    );
    assert_eq!(reply.body["title"], "제목");
    assert_eq!(reply.body["webViewUrl"], URL);
    let id: Uuid = reply.body["id"].as_str().unwrap().parse().unwrap();
    let mut client = api.db.client();
    let audits = audit_rows(&mut client, id, "BANNER_CREATED");
    assert_eq!(audits[0].get::<_, Uuid>("actor_admin_id"), api.admin);
    let list = api.list(&[]);
    assert_eq!(
        list.body["banners"][0]["createdAt"],
        reply.body["createdAt"]
    );
}

#[test]
fn 필수_항목이_누락되거나_공백이면_400이다() {
    let Some(api) = api() else { return };
    for field in ["title", "subtitle", "webViewUrl", "thumbnailUrl"] {
        let mut body = json!({
            "title": "제목", "subtitle": "부제목", "webViewUrl": URL, "thumbnailUrl": THUMBNAIL
        });
        body.as_object_mut().unwrap().remove(field);
        let reply = api.admin(
            Method::Post,
            "/api/v1/admin/banners",
            Some(&body.to_string()),
        );
        assert_eq!(reply.status, 400, "{field}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
        body[field] = json!(" ");
        let reply = api.admin(
            Method::Post,
            "/api/v1/admin/banners",
            Some(&body.to_string()),
        );
        assert_eq!(reply.status, 400, "{field}");
    }
}

#[test]
fn 도메인_검증_실패와_중복은_400_business_error이다() {
    let Some(api) = api() else { return };
    let invalid = api.admin(
        Method::Post,
        "/api/v1/admin/banners",
        Some(&banner_json(
            "제목",
            "부제목",
            "http://example.com",
            THUMBNAIL,
        )),
    );
    assert_eq!(invalid.status, 400);
    assert_eq!(invalid.body["errorCode"], "BUSINESS_ERROR");
    assert_eq!(
        invalid.body["message"],
        "배너 웹뷰 URL은 호스트가 있는 HTTPS 절대 주소여야 합니다."
    );
    assert_eq!(api.create(URL).status, 201);
    let duplicate = api.create(URL);
    assert_eq!(duplicate.status, 400);
    assert_eq!(
        duplicate.body["message"],
        "동일한 웹뷰 URL의 배너가 이미 존재합니다."
    );
}

#[test]
fn 본문_없는_삭제는_204와_빈_응답이고_잘못된_id는_400_없는_배너는_404이다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let id = insert_banner(&mut client, URL, "2026-10-19T00:00:00Z");

    let deleted = api.admin(Method::Delete, &format!("/api/v1/admin/banners/{id}"), None);
    assert_eq!(deleted.status, 204);
    assert!(deleted.raw.is_empty());

    let bad = api.admin(Method::Delete, "/api/v1/admin/banners/not-a-uuid", None);
    assert_eq!(bad.status, 400);
    assert_eq!(bad.body["errorCode"], "BUSINESS_ERROR");
    assert_eq!(bad.body["message"], "ID 형식이 올바르지 않습니다.");

    let missing = api.admin(
        Method::Delete,
        &format!("/api/v1/admin/banners/{}", Uuid::new_v4()),
        None,
    );
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body["errorCode"], "BUSINESS_ERROR");
}

// ---------------------------------------------------------------------------------------------
// BannerApiTest, BannerControllerTest, BannerServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 익명_조회는_삭제_배너를_제외하고_최신순_id_내림차순으로_페이지를_구성한다() {
    let Some(api) = api() else { return };
    let mut client = api.db.client();
    let old = insert_banner(
        &mut client,
        "https://example.com/old",
        "2026-10-19T00:00:00Z",
    );
    // 같은 생성 시각이면 id 내림차순이다.
    let tie_a = insert_banner(&mut client, "https://example.com/a", "2026-10-19T01:00:00Z");
    let tie_b = insert_banner(&mut client, "https://example.com/b", "2026-10-19T01:00:00Z");
    let deleted = insert_banner(
        &mut client,
        "https://example.com/deleted",
        "2026-10-19T02:00:00Z",
    );
    client
        .execute(
            "UPDATE banners SET deleted_at = now() WHERE id = $1",
            &[&deleted],
        )
        .unwrap();
    let mut newest = [tie_a, tie_b];
    newest.sort();
    newest.reverse();

    let first = api.list(&[("pageSize", "2")]);
    assert_eq!(first.status, 200);
    assert_eq!(first.body["banners"][0]["id"], newest[0].to_string());
    assert_eq!(first.body["banners"][1]["id"], newest[1].to_string());
    assert_eq!(first.body["hasNext"], true);
    let second = api.list(&[("page", "2"), ("pageSize", "2")]);
    assert_eq!(second.body["banners"].as_array().unwrap().len(), 1);
    assert_eq!(second.body["banners"][0]["id"], old.to_string());
    assert_eq!(second.body["hasNext"], false);
    let third = api.list(&[("page", "3"), ("pageSize", "2")]);
    assert_eq!(third.body["banners"], json!([]));
    assert_eq!(third.body["hasNext"], false);
}

#[test]
fn 등록된_배너가_없으면_기본_페이지와_빈_목록을_반환하고_응답_필드_순서를_지킨다() {
    let Some(api) = api() else { return };
    let empty = api.list(&[]);
    assert_eq!(empty.status, 200);
    assert_eq!(
        String::from_utf8(empty.raw).unwrap(),
        r#"{"currentPage":1,"pageSize":20,"hasNext":false,"banners":[]}"#
    );
    let mut client = api.db.client();
    insert_banner(
        &mut client,
        "https://example.com/event",
        "2026-10-19T00:00:00Z",
    );
    let one = api.list(&[("page", " "), ("pageSize", "")]);
    assert_eq!(
        String::from_utf8(one.raw).unwrap(),
        concat!(
            r#"{"currentPage":1,"pageSize":20,"hasNext":false,"banners":[{"id":""#,
            "{id}",
            r#"","title":"제목","subtitle":"부제목","webViewUrl":"https://example.com/event","#,
            r#""thumbnailUrl":"https://cdn.example.com/banner.png","createdAt":"2026-10-19T00:00:00Z"}]}"#
        )
        .replace("{id}", &first_id(&mut client))
    );
}

fn first_id(client: &mut Client) -> String {
    let id: Uuid = client
        .query_one("SELECT id FROM banners", &[])
        .unwrap()
        .get(0);
    id.hyphenated().to_string()
}

#[test]
fn 잘못된_페이지_크기와_offset_초과는_400_business_error이다() {
    let Some(api) = api() else { return };
    for (page, size) in [
        ("0", "20"),
        ("-1", "20"),
        ("1", "0"),
        ("1", "101"),
        ("2147483647", "100"),
    ] {
        let reply = api.list(&[("page", page), ("pageSize", size)]);
        assert_eq!(reply.status, 400, "{page} {size}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(reply.body["message"], "조회 조건이 올바르지 않습니다.");
    }
    // 허용 경계.
    assert_eq!(api.list(&[("pageSize", "100")]).status, 200);
    assert_eq!(
        api.list(&[("page", "2147483647"), ("pageSize", "1")])
            .status,
        200
    );
}

#[test]
fn 숫자로_바인딩할_수_없는_조회_조건은_400이다() {
    let Some(api) = api() else { return };
    for (name, value) in [("page", "abc"), ("pageSize", "x"), ("page", "1.5")] {
        let reply = api.list(&[(name, value)]);
        assert_eq!(reply.status, 400, "{name}={value}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
    }
}

// ---------------------------------------------------------------------------------------------
// 보강: AdminBannerTransactionTest 동시성, BannerRepositoryImplTest, AdminBannerApiTest
// ---------------------------------------------------------------------------------------------

use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use super::audit::domain::{AdminAction, AdminTargetType, AuditSnapshot};
use super::audit::service::{AuditLogCommand, create_audit_log};
use super::banner::{banner_state, deletion_state};
use super::support::in_transaction;
use crate::web::ApiError;

// AdminBannerTransactionTest#createBanner_concurrentDuplicateAfterPrecheck_persistsOnlyOne
#[test]
fn 두_요청이_사전_검사를_통과한_뒤_경쟁하면_db_중복을_업무_오류로_변환하고_한_건만_저장한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-race");
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let deps = deps(&db);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || -> Result<(), ApiError> {
                let now = deps.now();
                let mut banner =
                    Banner::create(Some("제목"), Some("부제목"), Some(URL), Some(THUMBNAIL))?;
                let mut conn = deps.pool.get()?;
                in_transaction(&mut conn, |tx| {
                    // 두 요청 모두 사전 검사를 통과한 뒤에 저장한다.
                    assert!(!BannerRepository.exists_by_web_view_url(tx, URL)?);
                    barrier.wait();
                    BannerRepository
                        .insert(tx, &mut banner, now)
                        .map_err(|e| match e {
                            SaveError::DuplicateWebViewUrl => super::support::business(
                                "동일한 웹뷰 URL의 배너가 이미 존재합니다.",
                            ),
                            SaveError::Db(e) => ApiError::from(e),
                        })?;
                    create_audit_log(
                        tx,
                        AuditLogCommand {
                            actor_admin_id: admin,
                            action: AdminAction::BannerCreated,
                            target_type: AdminTargetType::Banner,
                            target_id: banner.id.unwrap(),
                            reason: None,
                            before_state: AuditSnapshot::empty(),
                            after_state: banner_state(&banner)?,
                        },
                        now,
                    )?;
                    Ok(())
                })
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let failure = results.into_iter().find_map(Result::err).unwrap();
    assert_eq!(failure.status(), 400);
    assert_eq!(
        failure.message(),
        "동일한 웹뷰 URL의 배너가 이미 존재합니다."
    );
    assert_eq!(count(&mut client, "banners"), 1);
    assert_eq!(count(&mut client, "admin_audit_logs"), 1);
}

// AdminBannerTransactionTest#deleteBanner_concurrent_preservesFirstAudit
#[test]
fn 동시_삭제는_같은_행_잠금으로_직렬화하고_최초_시각_감사_한_건을_유지한다() {
    let Some(db) = TestDb::create() else { return };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "banner-race");
    let id = insert_banner(&mut client, URL, "2026-10-19T00:00:00Z");
    let first_deps = super::testing::deps_at(&db, "2026-10-19T01:00:00Z");
    let second_deps = super::testing::deps_at(&db, "2026-10-19T02:00:00Z");
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();

    let first = thread::spawn(move || -> Result<(), ApiError> {
        let now = first_deps.now();
        let mut conn = first_deps.pool.get()?;
        in_transaction(&mut conn, |tx| {
            let mut banner = BannerRepository
                .find_by_id_for_update(tx, id)?
                .expect("배너가 있다");
            locked_tx.send(()).unwrap();
            // 두 번째 삭제가 잠금을 기다리는 동안 잠금을 유지한다.
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let before = deletion_state(&banner)?;
            banner.delete(now);
            BannerRepository.save_deletion(tx, &mut banner, now)?;
            create_audit_log(
                tx,
                AuditLogCommand {
                    actor_admin_id: admin,
                    action: AdminAction::BannerDeleted,
                    target_type: AdminTargetType::Banner,
                    target_id: id,
                    reason: None,
                    before_state: before,
                    after_state: deletion_state(&banner)?,
                },
                now,
            )?;
            Ok(())
        })
    });
    locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let second =
        thread::spawn(move || AdminBannerService::new(second_deps).delete_banner(id, admin));
    thread::sleep(Duration::from_millis(300));
    assert!(
        !second.is_finished(),
        "첫 트랜잭션이 잠금을 유지하는 동안 두 번째 삭제는 끝날 수 없다"
    );
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();

    assert_eq!(audit_rows(&mut client, id, "BANNER_DELETED").len(), 1);
    let deleted_at: jiff::Timestamp = client
        .query_one("SELECT deleted_at FROM banners WHERE id = $1", &[&id])
        .unwrap()
        .get(0);
    assert_eq!(deleted_at, "2026-10-19T01:00:00Z".parse().unwrap());
}

// BannerRepositoryImplTest#save_unrelatedViolation_preservesException
#[test]
fn 다른_제약_위반은_중복_오류로_바꾸지_않고_db_오류로_유지한다() {
    let Some(db) = TestDb::create() else { return };
    let mut conn = db.pool(2).get().unwrap();
    // 도메인 검증을 거치지 않은 빈 제목: ck_banners_title_not_blank 위반.
    let mut banner =
        Banner::create(Some("제목"), Some("부제목"), Some(URL), Some(THUMBNAIL)).unwrap();
    banner.title = " ".to_owned();
    let error = BannerRepository
        .insert(&mut conn, &mut banner, deps(&db).now())
        .unwrap_err();
    match error {
        SaveError::Db(e) => assert!(format!("{e:?}").contains("ck_banners_title_not_blank")),
        SaveError::DuplicateWebViewUrl => panic!("중복 오류로 바뀌면 안 된다"),
    }
}

// AdminBannerApiTest#getBanners_invalidPagination_returnsBadRequest (관리자 토큰, "21474838, 100" 포함)
#[test]
fn 관리자_토큰으로_조회해도_offset_초과는_400이다() {
    let Some(api) = api() else { return };
    for (page, size) in [("0", "20"), ("1", "0"), ("1", "101"), ("21474838", "100")] {
        let reply = api.call(
            Some(api.admin_principal()),
            Method::Get,
            "/api/v1/banners",
            &[("page", page), ("pageSize", size)],
            None,
        );
        assert_eq!(reply.status, 400, "{page} {size}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(reply.body["message"], "조회 조건이 올바르지 않습니다.");
    }
}
