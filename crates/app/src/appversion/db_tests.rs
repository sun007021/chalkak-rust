//! 앱 버전 DB 테스트. 원본 `AppVersionServiceTest`, `AppVersionRepositoryTest`, `AppVersionTimestampTest`,
//! `AppVersionMigrationValidationTest`, `AppVersionControllerTest`에 대응한다. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::{Arc, Barrier};
use std::thread;

use chalkak_http::{Handler, Method, Request, Router};
use postgres::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use super::domain::AppPlatform;
use super::migration_validator::validate_before_migrate;
use super::service::AppVersionService;
use crate::admin::testing::{deps, insert_admin};
use crate::testing::TestDb;
use crate::web::Routes;

const ANDROID_URL: &str = "https://play.google.com/store/apps/details?id=com.example.chalkak";

fn insert(client: &mut Client, platform: &str, version: &str, url: &str) {
    client
        .execute(
            "INSERT INTO app_versions (platform, minimum_supported_version, store_url) \
             VALUES ($1, $2, $3)",
            &[&platform, &version, &url],
        )
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// AppVersionServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 플랫폼별로_저장된_최소_지원_버전과_주소를_독립적으로_조회한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    insert(&mut client, "ANDROID", "1.2.0", ANDROID_URL);
    insert(
        &mut client,
        "IOS",
        "2.0.0",
        "https://apps.apple.com/app/id1234567890",
    );
    let service = AppVersionService::new(db.pool(2));
    let android = service.get_app_version(AppPlatform::Android).unwrap();
    assert_eq!(android.platform, AppPlatform::Android);
    assert_eq!(android.minimum_supported_version.as_deref(), Some("1.2.0"));
    assert_eq!(android.store_url.as_deref(), Some(ANDROID_URL));
    let ios = service.get_app_version(AppPlatform::Ios).unwrap();
    assert_eq!(ios.minimum_supported_version.as_deref(), Some("2.0.0"));
    assert_eq!(
        ios.store_url.as_deref(),
        Some("https://apps.apple.com/app/id1234567890")
    );
}

#[test]
fn 설정이_없으면_두_플랫폼_모두_null이고_기본_데이터를_만들지_않는다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let service = AppVersionService::new(db.pool(2));
    for platform in AppPlatform::ALL {
        let result = service.get_app_version(platform).unwrap();
        assert_eq!(result.minimum_supported_version, None);
        assert_eq!(result.store_url, None);
    }
    let count: i64 = db
        .client()
        .query_one("SELECT COUNT(*) FROM app_versions", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[test]
fn 다른_플랫폼만_설정돼_있으면_요청한_플랫폼은_버전과_주소가_모두_null이다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    insert(&mut db.client(), "ANDROID", "1.2.0", ANDROID_URL);
    let result = AppVersionService::new(db.pool(2))
        .get_app_version(AppPlatform::Ios)
        .unwrap();
    assert_eq!(result.platform, AppPlatform::Ios);
    assert_eq!(
        (result.minimum_supported_version, result.store_url),
        (None, None)
    );
}

#[test]
fn db_값이_바뀌면_다음_조회에_새_설정을_반환한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    insert(&mut client, "ANDROID", "1.2.0", ANDROID_URL);
    let service = AppVersionService::new(db.pool(2));
    service.get_app_version(AppPlatform::Android).unwrap();
    client
        .execute(
            "UPDATE app_versions SET minimum_supported_version = '1.3.0', store_url = $1 \
             WHERE platform = 'ANDROID'",
            &[&format!("{ANDROID_URL}&hl=ko")],
        )
        .unwrap();
    let result = service.get_app_version(AppPlatform::Android).unwrap();
    assert_eq!(result.minimum_supported_version.as_deref(), Some("1.3.0"));
    assert!(result.store_url.unwrap().ends_with("&hl=ko"));
}

// ---------------------------------------------------------------------------------------------
// AppVersionRepositoryTest, AppVersionTimestampTest, 제약
// ---------------------------------------------------------------------------------------------

#[test]
fn 중복_플랫폼_저장이_경쟁해도_하나만_저장할_수_있는_고유_제약을_유지한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let mut client = db.client();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                match client.execute(
                    "INSERT INTO app_versions (platform, minimum_supported_version, store_url) \
                     VALUES ('ANDROID', '1.2.0', $1)",
                    &[&ANDROID_URL],
                ) {
                    Ok(_) => true,
                    Err(e) => {
                        let constraint = e
                            .as_db_error()
                            .and_then(|d| d.constraint().map(str::to_owned));
                        assert_eq!(constraint.as_deref(), Some("uq_app_versions_platform"));
                        false
                    }
                }
            })
        })
        .collect();
    let mut results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    results.sort_unstable();
    assert_eq!(results, [false, true]);
}

#[test]
fn 시각을_지정하지_않은_입력은_두_시각의_db_기본값과_revision_0_메타데이터_없음으로_저장한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    insert(&mut client, "ANDROID", "1.2.0", ANDROID_URL);
    let row = client
        .query_one(
            "SELECT revision, last_changed_by, change_reason, created_at = updated_at, \
             created_at > now() - interval '1 minute' FROM app_versions",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 0);
    assert_eq!(row.get::<_, Option<Uuid>>(1), None);
    assert_eq!(row.get::<_, Option<String>>(2), None);
    assert!(row.get::<_, bool>(3));
    assert!(row.get::<_, bool>(4));
}

#[test]
fn 새_db_제약은_주소_원문의_후행_공백_개행_탭을_거절한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    for whitespace in [" ", "\n", "\t"] {
        let error = client
            .execute(
                "INSERT INTO app_versions (platform, minimum_supported_version, store_url) \
                 VALUES ('IOS', '1.0.0', $1)",
                &[&format!("https://apps.apple.com/app/id123{whitespace}")],
            )
            .unwrap_err();
        assert_eq!(
            error.as_db_error().and_then(|d| d.constraint()),
            Some("ck_app_versions_store_url")
        );
    }
}

#[test]
fn 앱_버전_감사_사유_누락은_공통_필수_사유_제약으로_거절한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    let admin = insert_admin(&mut client, "app-version-reason");
    let error = client
        .execute(
            "INSERT INTO admin_audit_logs (actor_admin_id, action, target_type, target_id, reason, \
             before_state, after_state, occurred_at, request_id) \
             VALUES ($1, 'APP_VERSION_CREATED', 'APP_VERSION', $2, NULL, '{}', \
             '{\"platform\":\"IOS\"}', now(), $3)",
            &[&admin, &Uuid::new_v4(), &Uuid::new_v4()],
        )
        .unwrap_err();
    assert_eq!(
        error.as_db_error().and_then(|d| d.constraint()),
        Some("ck_admin_audit_logs_required_reason")
    );
}

// ---------------------------------------------------------------------------------------------
// AppVersionMigrationValidationTest
// ---------------------------------------------------------------------------------------------

/// 임시 테이블로 마이그레이션 전 상태를 흉내 낸다(같은 연결의 `pg_temp`가 실제 테이블을 가린다).
fn legacy_table(client: &mut Client) {
    client
        .batch_execute(
            "CREATE TEMPORARY TABLE app_versions (id UUID PRIMARY KEY, platform TEXT, \
             minimum_supported_version TEXT, store_url TEXT)",
        )
        .unwrap();
}

fn insert_legacy(client: &mut Client, platform: &str, version: &str, url: &str) -> Uuid {
    let id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO app_versions (id, platform, minimum_supported_version, store_url) \
             VALUES ($1, $2, $3, $4)",
            &[&id, &platform, &version, &url],
        )
        .unwrap();
    id
}

#[test]
fn 기존_버전_주소_플랫폼_감사_값이_유효하지_않으면_사전_검증으로_중단하고_값을_노출하지_않는다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    for (platform, version, url) in [
        ("IOS", "01.2.0", "https://apps.apple.com/app/id123"),
        ("IOS", "2147483648.0.0", "https://apps.apple.com/app/id123"),
        ("IOS", "1.0.0", "https://itunes.apple.com/app/id123"),
        ("IOS", "1.0.0", "https://apps.apple.com/app/id123?uo=4"),
        ("ANDROID", "1.0.0", "https://apps.apple.com/app/id123"),
        (
            "ANDROID",
            "1.0.0",
            "https://play.google.com/store/apps/details?id=com.a.b&referrer=sample",
        ),
        (
            "IOS",
            "1.0.0",
            "https://apps.apple.com/app/token=example-secret/id123",
        ),
        ("WEB", "1.0.0", "https://apps.apple.com/app/id123"),
    ] {
        let mut client = db.client();
        legacy_table(&mut client);
        let id = insert_legacy(&mut client, platform, version, url);
        let message = validate_before_migrate(&mut client)
            .unwrap_err()
            .to_string();
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(message.contains("사전 검증"), "{message}");
        assert!(!message.contains(url), "{message}");
        let stored: String = client
            .query_one("SELECT store_url FROM app_versions", &[])
            .unwrap()
            .get(0);
        assert_eq!(stored, url);
    }
}

#[test]
fn 기존_주소의_개행을_조용히_지우지_않고_저장_원문_검증에서_중단한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    legacy_table(&mut client);
    let raw = "https://apps.apple.com/app/id123\n";
    insert_legacy(&mut client, "IOS", "1.0.0", raw);
    let message = validate_before_migrate(&mut client)
        .unwrap_err()
        .to_string();
    assert!(message.contains("사전 검증"));
    let stored: String = client
        .query_one("SELECT store_url FROM app_versions", &[])
        .unwrap()
        .get(0);
    assert_eq!(stored, raw);
}

#[test]
fn 유효한_기존_설정과_빈_테이블은_값을_바꾸지_않고_마이그레이션을_허용한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    legacy_table(&mut client);
    validate_before_migrate(&mut client).unwrap();
    insert_legacy(
        &mut client,
        "IOS",
        "1.0.0",
        "https://apps.apple.com/app/id123",
    );
    validate_before_migrate(&mut client).unwrap();
    let version: String = client
        .query_one("SELECT minimum_supported_version FROM app_versions", &[])
        .unwrap()
        .get(0);
    assert_eq!(version, "1.0.0");
}

#[test]
fn 첫_설치로_앱_버전_테이블이_아직_없으면_사전_검증을_건너뛴다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    client.batch_execute("DROP TABLE app_versions").unwrap();
    validate_before_migrate(&mut client).unwrap();
}

#[test]
fn 사전_검증_db_조회가_실패하면_유효한_데이터로_간주하지_않고_중단한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    legacy_table(&mut client);
    client
        .batch_execute("ALTER TABLE pg_temp.app_versions DROP COLUMN store_url")
        .unwrap();
    let message = validate_before_migrate(&mut client)
        .unwrap_err()
        .to_string();
    assert!(message.contains("DB 조회"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// AppVersionControllerTest (라우터까지)
// ---------------------------------------------------------------------------------------------

fn router(db: &TestDb) -> Router {
    let deps = deps(db);
    let ctx_routes = Routes::new(deps.logger.clone());
    let service = AppVersionService::new(deps.pool.clone());
    super::api::register_with_service(ctx_routes, service).into_router()
}

fn get(router: &Router, query: &[(&str, &str)]) -> (u16, Option<String>, Value) {
    let mut req = Request::new(Method::Get, "/api/v1/app-version");
    req.query = query
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let response = router.handle(req);
    let cache = response.headers.get("cache-control").map(str::to_owned);
    (
        response.status,
        cache,
        serde_json::from_slice(&response.body).unwrap_or(Value::Null),
    )
}

#[test]
fn 플랫폼_설정의_세_필드와_캐시_방지_헤더를_반환한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    insert(&mut db.client(), "ANDROID", "1.2.0", ANDROID_URL);
    let router = router(&db);
    let (status, cache, body) = get(&router, &[("platform", "ANDROID")]);
    assert_eq!((status, cache.as_deref()), (200, Some("no-store")));
    assert_eq!(
        body,
        json!({"platform": "ANDROID", "minimumSupportedVersion": "1.2.0", "storeUrl": ANDROID_URL})
    );
    // 미설정 플랫폼도 세 필드를 유지하고 null이다.
    let (status, cache, body) = get(&router, &[("platform", "IOS")]);
    assert_eq!((status, cache.as_deref()), (200, Some("no-store")));
    assert_eq!(
        body,
        json!({"platform": "IOS", "minimumSupportedVersion": null, "storeUrl": null})
    );
}

#[test]
fn 플랫폼을_생략하거나_빈_값_허용되지_않은_값이면_400과_공통_오류를_반환한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let router = router(&db);
    let (status, _, body) = get(&router, &[]);
    assert_eq!(status, 400);
    assert_eq!(body["errorCode"], "BUSINESS_ERROR");
    assert_eq!(body["message"], "platform: 필수 요청 값이 없습니다.");
    for blank in ["", " "] {
        let (status, _, body) = get(&router, &[("platform", blank)]);
        assert_eq!(status, 400, "{blank:?}");
        assert_eq!(body["message"], "platform: 필수 요청 값이 없습니다.");
    }
    for invalid in ["android", "ios", "WINDOWS"] {
        let (status, _, body) = get(&router, &[("platform", invalid)]);
        assert_eq!(status, 400, "{invalid}");
        assert_eq!(body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(
            body["message"],
            "platform: 요청 값의 형식이 올바르지 않습니다."
        );
    }
    // 앞뒤 공백은 뗀다.
    assert_eq!(get(&router, &[("platform", " IOS ")]).0, 200);
}
