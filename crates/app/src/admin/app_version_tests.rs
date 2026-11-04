//! 관리자 앱 버전 테스트. 원본 `AdminAppVersionServiceTest`, `AdminAppVersionTransactionTest`,
//! `AdminAppVersionControllerTest`에 대응한다.
//!
//! 서비스를 목으로 바꾸지 않고 실제 DB로 라우터까지 통과시킨다. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use chalkak_core::error::ErrorCode;
use chalkak_http::{Handler, Method, Request, Router};
use jiff::Timestamp;
use postgres::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use super::app_version::{AdminAppVersionService, SettingResult};
use super::support::AdminDeps;
use super::testing::{NOW, deps, deps_at, insert_admin};
use crate::appversion::domain::AppPlatform;
use crate::appversion::repository::{AppVersionRepository, SaveError};
use crate::appversion::service::AppVersionService;
use crate::security::{self, AUTHORITY_ADMIN, Principal};
use crate::testing::TestDb;
use crate::web::{ApiError, Routes};

const URL: &str = "https://play.google.com/store/apps/details?id=com.example.chalkak";
const CHANGED_MESSAGE: &str = "설정이 변경되었습니다. 다시 조회한 뒤 저장해 주세요.";

fn setup() -> Option<(TestDb, AdminDeps, Uuid)> {
    let db = TestDb::create()?;
    let admin = insert_admin(&mut db.client(), "app-version-manager");
    let deps = deps(&db);
    Some((db, deps, admin))
}

fn count(client: &mut Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).unwrap().get(0)
}

fn audit_count(client: &mut Client) -> i64 {
    count(
        client,
        "SELECT COUNT(*) FROM admin_audit_logs WHERE target_type = 'APP_VERSION'",
    )
}

fn save(
    service: &AdminAppVersionService,
    admin: Uuid,
    revision: Option<i64>,
    version: &str,
    url: &str,
) -> Result<SettingResult, ApiError> {
    service.save_app_version(
        admin,
        AppPlatform::Android,
        revision,
        version,
        url,
        "  출시 확인  ",
    )
}

fn assert_conflict(result: Result<SettingResult, ApiError>) {
    let error = result.unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(error.code(), ErrorCode::RESOURCE_STATE_CHANGED);
    assert_eq!(error.message(), CHANGED_MESSAGE);
}

// ---------------------------------------------------------------------------------------------
// AdminAppVersionServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 미설정_두_플랫폼은_null_설정으로_조회하고_앱의_이용을_허용한다() {
    let Some((db, deps, _admin)) = setup() else {
        return;
    };
    let settings = AdminAppVersionService::new(deps.clone())
        .get_app_versions()
        .unwrap();
    let platforms: Vec<_> = settings.iter().map(|s| s.platform).collect();
    assert_eq!(platforms, [AppPlatform::Android, AppPlatform::Ios]);
    for setting in &settings {
        assert_eq!(
            *setting,
            SettingResult::unconfigured(setting.platform),
            "platform 외 모든 필드는 null"
        );
    }
    let public = AppVersionService::new(deps.pool.clone())
        .get_app_version(AppPlatform::Android)
        .unwrap();
    assert_eq!(public.minimum_supported_version, None);
    assert_eq!(
        count(&mut db.client(), "SELECT COUNT(*) FROM app_versions"),
        0
    );
}

#[test]
fn 최초_저장_버전_하향_주소_변경을_공개_조회에_반영하고_전후_값과_관리자를_감사_기록으로_남긴다() {
    let Some((db, _deps, admin)) = setup() else {
        return;
    };
    let at = |t: &str| AdminAppVersionService::new(deps_at(&db, t));
    let created = save(&at("2026-10-29T01:00:00Z"), admin, None, "2.0.0", URL).unwrap();
    let lowered = save(
        &at("2026-10-29T02:00:00Z"),
        admin,
        created.revision,
        "1.10.0",
        URL,
    )
    .unwrap();
    let changed_url = format!("{URL}&hl=ko");
    let changed = save(
        &at("2026-10-29T03:00:00Z"),
        admin,
        lowered.revision,
        "1.10.0",
        &changed_url,
    )
    .unwrap();

    assert_eq!(created.revision, Some(0));
    assert_eq!(changed.revision, Some(2));
    assert_eq!(changed.created_at, created.created_at);
    assert_eq!(
        changed.created_at,
        Some("2026-10-29T01:00:00Z".parse().unwrap())
    );
    assert_eq!(
        changed.updated_at,
        Some("2026-10-29T03:00:00Z".parse().unwrap())
    );
    assert_eq!(changed.last_changed_by, Some(admin));
    assert_eq!(changed.change_reason.as_deref(), Some("출시 확인"));

    let public = AppVersionService::new(deps(&db).pool);
    let android = public.get_app_version(AppPlatform::Android).unwrap();
    assert_eq!(android.minimum_supported_version.as_deref(), Some("1.10.0"));
    assert_eq!(android.store_url.as_deref(), Some(changed_url.as_str()));
    assert_eq!(
        public
            .get_app_version(AppPlatform::Ios)
            .unwrap()
            .minimum_supported_version,
        None
    );

    let mut client = db.client();
    let rows = client
        .query(
            "SELECT action::text, actor_admin_id, reason, before_state::text, after_state::text, \
             request_id FROM admin_audit_logs WHERE target_type = 'APP_VERSION' \
             ORDER BY (after_state->>'revision')::int DESC",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    let state = |row: &postgres::Row, i: usize| -> Value {
        serde_json::from_str(&row.get::<_, String>(i)).unwrap()
    };
    assert_eq!(rows[0].get::<_, String>(0), "APP_VERSION_UPDATED");
    assert_eq!(state(&rows[0], 3)["storeUrl"], json!(URL));
    assert_eq!(state(&rows[0], 3)["revision"], json!(1));
    assert_eq!(state(&rows[0], 4)["storeUrl"], json!(changed_url));
    assert_eq!(state(&rows[0], 4)["revision"], json!(2));
    assert_eq!(rows[2].get::<_, String>(0), "APP_VERSION_CREATED");
    assert_eq!(state(&rows[2], 3), json!({}));
    for row in &rows {
        assert_eq!(row.get::<_, Uuid>(1), admin);
        assert_eq!(
            row.get::<_, Option<String>>(2).as_deref(),
            Some("출시 확인")
        );
    }
}

#[test]
fn 같은_값의_오래된_재시도는_이력_시각_사유_revision을_바꾸지_않는다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminAppVersionService::new(deps);
    let first = save(&service, admin, None, "1.0.0", URL).unwrap();
    let retry = service
        .save_app_version(
            admin,
            AppPlatform::Android,
            None,
            "1.0.0",
            &format!(" {URL} "),
            "다른 사유",
        )
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(service.get_app_versions().unwrap()[0], first);
    assert_eq!(audit_count(&mut db.client()), 1);
}

#[test]
fn 같은_값의_재시도도_입력_조건은_충족해야_한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminAppVersionService::new(deps);
    save(&service, admin, None, "1.0.0", URL).unwrap();
    let error = service
        .save_app_version(admin, AppPlatform::Android, None, "1.0.0", URL, " ")
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(error.code(), ErrorCode::BUSINESS_ERROR);
    assert_eq!(audit_count(&mut db.client()), 1);
}

#[test]
fn 최초_저장의_잘못된_revision과_수정의_오래된_revision은_상태_충돌로_거절한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminAppVersionService::new(deps);
    assert_conflict(save(&service, admin, Some(0), "1.0.0", URL));
    let first = save(&service, admin, None, "1.0.0", URL).unwrap();
    assert_conflict(save(&service, admin, Some(9), "2.0.0", URL));
    assert_conflict(save(&service, admin, None, "2.0.0", URL));
    assert_eq!(service.get_app_versions().unwrap()[0], first);
    assert_eq!(audit_count(&mut db.client()), 1);
}

#[test]
fn 플랫폼과_다른_주소_잘못된_버전은_저장_전에_거절한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminAppVersionService::new(deps);
    for (version, url, message) in [
        (
            "1.0.0",
            "https://apps.apple.com/app/id123",
            "스토어 주소는 해당 플랫폼의 공식 HTTPS 앱 상세 주소여야 합니다.",
        ),
        (
            "01.0.0",
            URL,
            "최소 지원 버전은 앞자리 0 없이 0~2147483647 범위의 major.minor.patch여야 합니다.",
        ),
    ] {
        let error = save(&service, admin, None, version, url).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.message(), message);
    }
    assert_eq!(
        count(&mut db.client(), "SELECT COUNT(*) FROM app_versions"),
        0
    );
}

#[test]
fn 보조_문자_500자의_사유를_저장하고_같은_code_point_길이로_조회한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let reason = "😀".repeat(500);
    let service = AdminAppVersionService::new(deps);
    let result = service
        .save_app_version(
            admin,
            AppPlatform::Android,
            None,
            "1.0.0",
            URL,
            &format!("  {reason}  "),
        )
        .unwrap();
    assert_eq!(result.change_reason.as_deref(), Some(reason.as_str()));
    assert_eq!(
        service.get_app_versions().unwrap()[0]
            .change_reason
            .as_deref(),
        Some(reason.as_str())
    );
    assert_eq!(
        count(
            &mut db.client(),
            "SELECT char_length(change_reason)::bigint FROM app_versions"
        ),
        500
    );
}

// ---------------------------------------------------------------------------------------------
// AdminAppVersionTransactionTest
// ---------------------------------------------------------------------------------------------

fn race(deps: &AdminDeps, admin: Uuid, revision: Option<i64>) -> Vec<&'static str> {
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let service = AdminAppVersionService::new(deps.clone());
            let barrier = Arc::clone(&barrier);
            let version = match (revision, i) {
                (None, 0) => "1.0.0",
                (None, _) => "2.0.0",
                (Some(_), 0) => "3.0.0",
                (Some(_), _) => "4.0.0",
            };
            thread::spawn(move || {
                barrier.wait();
                match save(&service, admin, revision, version, URL) {
                    Ok(_) => "saved",
                    Err(e) => {
                        assert_eq!(e.code(), ErrorCode::RESOURCE_STATE_CHANGED);
                        "conflict"
                    }
                }
            })
        })
        .collect();
    let mut outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    outcomes.sort_unstable();
    outcomes
}

#[test]
fn 경쟁하는_최초_저장과_같은_revision_수정은_한_요청만_변경하고_다른_요청은_충돌로_거절한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    assert_eq!(race(&deps, admin, None), ["conflict", "saved"]);
    assert_eq!(count(&mut client, "SELECT COUNT(*) FROM app_versions"), 1);
    assert_eq!(audit_count(&mut client), 1);
    assert_eq!(race(&deps, admin, Some(0)), ["conflict", "saved"]);
    assert_eq!(audit_count(&mut client), 2);
    assert_eq!(count(&mut client, "SELECT revision FROM app_versions"), 1);
}

#[test]
fn 최초_저장_재시도가_동시에_와도_같은_값은_한_행과_한_감사_이력만_남긴다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let service = AdminAppVersionService::new(deps.clone());
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                save(&service, admin, None, "1.0.0", URL).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results[0], results[1]);
    let mut client = db.client();
    assert_eq!(count(&mut client, "SELECT COUNT(*) FROM app_versions"), 1);
    assert_eq!(audit_count(&mut client), 1);
}

#[test]
fn 플랫폼_잠금을_유지하면_같은_플랫폼만_대기하고_다른_플랫폼_저장은_완료된다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut holder = db.client();
    let mut tx = holder.transaction().unwrap();
    tx.query_one("SELECT pg_advisory_xact_lock(3, hashtext('ANDROID'))", &[])
        .unwrap();

    let blocked = {
        let service = AdminAppVersionService::new(deps.clone());
        thread::spawn(move || save(&service, admin, None, "1.0.0", URL).unwrap())
    };
    thread::sleep(Duration::from_millis(300));
    assert!(
        !blocked.is_finished(),
        "같은 플랫폼 저장은 잠금을 기다려야 한다"
    );
    AdminAppVersionService::new(deps.clone())
        .save_app_version(
            admin,
            AppPlatform::Ios,
            None,
            "1.0.0",
            "https://apps.apple.com/app/id123",
            "출시 확인",
        )
        .unwrap();
    tx.commit().unwrap();
    blocked.join().unwrap();
    assert_eq!(
        count(&mut db.client(), "SELECT COUNT(*) FROM app_versions"),
        2
    );
}

#[test]
fn 잠금을_우회한_직접_수정도_낙관적_잠금으로_충돌을_업무_예외로_바꾼다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminAppVersionService::new(deps.clone());
    save(&service, admin, None, "1.0.0", URL).unwrap();
    let mut conn = deps.pool.get().unwrap();
    let repo = AppVersionRepository;
    let mut stale = repo
        .find_by_platform(&mut conn, AppPlatform::Android)
        .unwrap()
        .unwrap();
    let mut fresh = stale.clone();
    let now: Timestamp = NOW.parse().unwrap();
    repo.save_and_flush(&mut conn, &mut fresh, now).unwrap();
    assert_eq!(fresh.revision, 1);
    match repo.save_and_flush(&mut conn, &mut stale, now) {
        Err(SaveError::Business(e)) => {
            assert_eq!(e.code, ErrorCode::RESOURCE_STATE_CHANGED);
            assert_eq!(e.message, CHANGED_MESSAGE);
        }
        other => panic!("충돌이어야 한다: {other:?}"),
    }
    assert_eq!(
        count(&mut db.client(), "SELECT revision FROM app_versions"),
        1
    );
}

#[test]
fn 감사_저장_실패는_최초_생성과_변경된_값_시각_revision을_모두_롤백한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminAppVersionService::new(deps);
    client
        .batch_execute(
            "ALTER TABLE admin_audit_logs ADD CONSTRAINT ck_test_app_version_audit \
             CHECK (target_type <> 'APP_VERSION')",
        )
        .unwrap();
    let error = save(&service, admin, None, "1.0.0", URL).unwrap_err();
    assert_eq!(error.status(), 500);
    assert_eq!(count(&mut client, "SELECT COUNT(*) FROM app_versions"), 0);
    client
        .batch_execute("ALTER TABLE admin_audit_logs DROP CONSTRAINT ck_test_app_version_audit")
        .unwrap();

    let first = save(&service, admin, None, "1.0.0", URL).unwrap();
    client
        .batch_execute(
            "ALTER TABLE admin_audit_logs ADD CONSTRAINT ck_test_app_version_audit \
             CHECK (target_type <> 'APP_VERSION') NOT VALID",
        )
        .unwrap();
    let error = save(&service, admin, Some(0), "2.0.0", URL).unwrap_err();
    assert_eq!(error.status(), 500);
    assert_eq!(service.get_app_versions().unwrap()[0], first);
    assert_eq!(audit_count(&mut client), 1);
}

#[test]
fn 감사에_기록할_수_없는_스토어_주소는_최초_입력과_기존_설정_변경을_모두_롤백한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let mut client = db.client();
    let service = AdminAppVersionService::new(deps);
    let sensitive = "https://apps.apple.com/app/token=example-secret/id123";
    let ios = |revision, version: &str, url: &str, reason: &str| {
        service.save_app_version(admin, AppPlatform::Ios, revision, version, url, reason)
    };
    let error = ios(None, "1.0.0", sensitive, "출시 확인").unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(
        error.message(),
        "민감한 정보는 관리자 감사 로그에 저장할 수 없습니다."
    );
    assert_eq!(count(&mut client, "SELECT COUNT(*) FROM app_versions"), 0);
    assert_eq!(audit_count(&mut client), 0);

    let first = ios(
        None,
        "1.0.0",
        "https://apps.apple.com/app/id123",
        "출시 확인",
    )
    .unwrap();
    assert!(ios(first.revision, "2.0.0", sensitive, "주소 변경").is_err());
    assert_eq!(service.get_app_versions().unwrap()[1], first);
    assert_eq!(audit_count(&mut client), 1);
}

#[test]
fn 사유에_민감한_값이_있으면_감사_검증이_거절하고_설정을_롤백한다() {
    let Some((db, deps, admin)) = setup() else {
        return;
    };
    let service = AdminAppVersionService::new(deps);
    let error = service
        .save_app_version(
            admin,
            AppPlatform::Android,
            None,
            "1.0.0",
            URL,
            "accessToken=secret-value",
        )
        .unwrap_err();
    assert_eq!(error.status(), 400);
    assert_eq!(
        count(&mut db.client(), "SELECT COUNT(*) FROM app_versions"),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// AdminAppVersionControllerTest (라우터까지)
// ---------------------------------------------------------------------------------------------

struct Api {
    router: Router,
    admin: Uuid,
    _db: TestDb,
}

struct Reply {
    status: u16,
    body: Value,
}

fn api() -> Option<Api> {
    let (db, deps, admin) = setup()?;
    let routes = super::register_with(Routes::new(deps.logger.clone()), &deps);
    Some(Api {
        router: routes.into_router(),
        admin,
        _db: db,
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
        }
    }

    fn put(&self, platform: &str, body: Value) -> Reply {
        self.call(
            Method::Put,
            &format!("/api/v1/admin/app-versions/{platform}"),
            Some(&body.to_string()),
        )
    }
}

fn request() -> Value {
    json!({
        "revision": null,
        "minimumSupportedVersion": "1.0.0",
        "storeUrl": "https://apps.apple.com/app/id123",
        "reason": "출시 확인",
    })
}

#[test]
fn 목록_조회는_두_플랫폼과_미설정_null을_직렬화한다() {
    let Some(api) = api() else {
        return;
    };
    let reply = api.call(Method::Get, "/api/v1/admin/app-versions", None);
    assert_eq!(reply.status, 200);
    let list = reply.body["appVersions"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["platform"], "ANDROID");
    assert_eq!(list[1]["platform"], "IOS");
    assert_eq!(
        list[0],
        json!({"platform": "ANDROID", "minimumSupportedVersion": null, "storeUrl": null,
               "revision": null, "lastChangedBy": null, "changeReason": null,
               "createdAt": null, "updatedAt": null})
    );
}

#[test]
fn 최초_저장은_null_revision을_받아_설정_변경자_시각을_200으로_반환한다() {
    let Some(api) = api() else {
        return;
    };
    let reply = api.put("IOS", request());
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        json!({
            "platform": "IOS",
            "minimumSupportedVersion": "1.0.0",
            "storeUrl": "https://apps.apple.com/app/id123",
            "revision": 0,
            "lastChangedBy": api.admin.hyphenated().to_string(),
            "changeReason": "출시 확인",
            "createdAt": NOW,
            "updatedAt": NOW,
        })
    );
    let list = api
        .call(Method::Get, "/api/v1/admin/app-versions", None)
        .body;
    assert_eq!(list["appVersions"][1], reply.body);
    assert_eq!(list["appVersions"][0]["revision"], Value::Null);
}

#[test]
fn 필수_입력의_누락과_공백은_400_업무_오류로_반환한다() {
    let Some(api) = api() else {
        return;
    };
    for field in ["minimumSupportedVersion", "storeUrl", "reason"] {
        let mut body = request();
        body.as_object_mut().unwrap().remove(field);
        let reply = api.put("IOS", body.clone());
        assert_eq!(reply.status, 400, "{field}");
        assert_eq!(reply.body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(reply.body["message"], "must not be blank");
        body[field] = json!(" ");
        assert_eq!(api.put("IOS", body).status, 400, "{field}");
    }
}

#[test]
fn 음수_revision과_잘못된_플랫폼은_400을_반환한다() {
    let Some(api) = api() else {
        return;
    };
    let mut body = request();
    body["revision"] = json!(-1);
    let reply = api.put("IOS", body);
    assert_eq!(reply.status, 400);
    assert_eq!(reply.body["message"], "must be greater than or equal to 0");
    let reply = api.put("WEB", request());
    assert_eq!(reply.status, 400);
    assert_eq!(
        reply.body["message"],
        "platform: 요청 값의 형식이 올바르지 않습니다."
    );
    let reply = api.put("ios", request());
    assert_eq!(reply.status, 400);
}

#[test]
fn 서비스_입력_오류와_수정_충돌은_기존_400_오류_계약으로_반환한다() {
    let Some(api) = api() else {
        return;
    };
    let mut bad = request();
    bad["minimumSupportedVersion"] = json!("01.0.0");
    let reply = api.put("IOS", bad);
    assert_eq!(
        (reply.status, reply.body["errorCode"].as_str()),
        (400, Some("BUSINESS_ERROR"))
    );
    let mut stale = request();
    stale["revision"] = json!(5);
    let reply = api.put("IOS", stale);
    assert_eq!(
        (
            reply.status,
            reply.body["errorCode"].as_str(),
            reply.body["message"].as_str()
        ),
        (400, Some("RESOURCE_STATE_CHANGED"), Some(CHANGED_MESSAGE))
    );
}

#[test]
fn revision은_정수_소수_숫자_문자열을_받고_그_밖의_형태는_본문_오류다() {
    let Some(api) = api() else {
        return;
    };
    // 비어 있는 것으로 읽히는 값은 최초 저장으로 성공하고, 같은 값 재전송도 성공한다.
    for value in ["null", r#""""#, r#""  ""#] {
        let body = request().to_string().replace("null", value);
        let reply = api.call(Method::Put, "/api/v1/admin/app-versions/IOS", Some(&body));
        assert_eq!(reply.status, 200, "{value}");
    }
    for value in [
        "true",
        "[]",
        "{}",
        r#""abc""#,
        "1e400",
        "9223372036854775808",
    ] {
        let body = request().to_string().replace("null", value);
        let reply = api.call(Method::Put, "/api/v1/admin/app-versions/IOS", Some(&body));
        assert_eq!(reply.status, 400, "{value}");
        assert_eq!(
            reply.body["message"], "JSON 형식이 올바르지 않거나 요청 본문이 비어 있습니다.",
            "{value}"
        );
    }
}
