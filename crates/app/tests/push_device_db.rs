//! 푸시 기기 등록·해제와 알림 수신 설정의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PushDeviceServiceTest`, `PushDeviceLogoutTest`, `PushDeviceLogoutTransactionTest`,
//! `PushDeviceRefreshRevocationTest`, `PushDeviceWithdrawalTest`, `PushDeviceWithdrawalTransactionTest`,
//! `PushDeviceConcurrencyTest`, `PushDeviceJpaRepositoryTest`, `PushDeviceMigrationTest`,
//! `NotificationSettingsServiceTest`, `NotificationSettingsConcurrencyTest`를 옮겼다.
//!
//! 원본의 동시성 테스트는 한 트랜잭션 안에서 두 서비스를 부르고 첫 트랜잭션을 열어 둔 채 두 번째가 기다리는지 본다.
//! 여기서 서비스는 각자 트랜잭션을 열므로, 열어 둔 쪽은 같은 SQL을 직접 연 트랜잭션으로 흉내 낸다
//! (`register_in`은 호출자의 트랜잭션에 참여한다).

mod common;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use chalkak_app::auth::domain::IssuedRefreshToken;
use chalkak_app::notification::push_device::{FcmToken, PushDeviceService};
use chalkak_app::notification::settings::{
    NotificationSettingsResult, NotificationSettingsService,
};
use chalkak_app::testing::TestDb;
use chalkak_app::web::ApiError;
use chalkak_core::error::{ErrorCode, ErrorKind};
use common::{World, plus, ts};
use jiff::Timestamp;
use postgres::Client;
use uuid::Uuid;

const NOW: &str = "2026-09-30T00:00:00Z";

struct Fx {
    w: World,
    devices: PushDeviceService,
    settings: NotificationSettingsService,
}

fn fx() -> Option<Fx> {
    let w = World::create()?;
    w.clock.set(ts(NOW));
    let clock: Arc<dyn chalkak_core::Clock> = w.clock.clone();
    let devices = PushDeviceService::new(w.pool.clone(), clock.clone());
    let settings = NotificationSettingsService::new(w.pool.clone(), clock);
    Some(Fx {
        w,
        devices,
        settings,
    })
}

#[derive(Clone)]
struct Login {
    user_id: Uuid,
    token: IssuedRefreshToken,
}

impl Login {
    fn session_id(&self) -> Uuid {
        self.token.session_id
    }
}

struct Device {
    id: Uuid,
    token: String,
    hash: String,
    registered_at: Timestamp,
    updated_at: Timestamp,
}

#[track_caller]
fn reauth_required<T: std::fmt::Debug>(result: Result<T, ApiError>) {
    let ApiError::App(e) = result.expect_err("오류가 나야 한다") else {
        panic!("도메인 오류여야 한다");
    };
    assert_eq!(e.kind, ErrorKind::Unauthorized);
    assert_eq!(e.code, ErrorCode::REAUTHENTICATION_REQUIRED);
}

impl Fx {
    fn client(&self) -> Client {
        self.w.db.client()
    }

    /// 새 회원과 첫 로그인.
    fn login(&self) -> Login {
        let user = self.w.save_user();
        self.login_of(user.id)
    }

    fn login_of(&self, user_id: Uuid) -> Login {
        let token = self
            .w
            .refresh
            .issue(&mut self.client(), user_id)
            .expect("로그인 발급");
        Login { user_id, token }
    }

    fn register(&self, login: &Login, token: &str) {
        self.devices
            .register(Some(login.user_id), Some(login.session_id()), token)
            .unwrap();
    }

    fn try_register(&self, user: Option<Uuid>, session: Option<Uuid>) -> Result<(), ApiError> {
        self.devices.register(user, session, "shared-token")
    }

    fn device(&self, session_id: Uuid) -> Option<Device> {
        self.client()
            .query_opt(
                "SELECT id, fcm_token, fcm_token_hash, registered_at, updated_at \
                 FROM push_devices WHERE session_id = $1",
                &[&session_id],
            )
            .unwrap()
            .map(|row| Device {
                id: row.get("id"),
                token: row.get("fcm_token"),
                hash: row.get("fcm_token_hash"),
                registered_at: row.get("registered_at"),
                updated_at: row.get("updated_at"),
            })
    }

    fn device_exists(&self, id: Uuid) -> bool {
        self.count_with("SELECT COUNT(*) FROM push_devices WHERE id = $1", &id) > 0
    }

    fn count_with(&self, sql: &str, param: &(dyn postgres::types::ToSql + Sync)) -> i64 {
        self.client().query_one(sql, &[param]).unwrap().get(0)
    }

    fn count_devices(&self) -> i64 {
        self.w.count("SELECT COUNT(*) FROM push_devices")
    }

    fn live_tokens(&self, session_id: Uuid) -> i64 {
        self.count_with(
            "SELECT COUNT(*) FROM user_refresh_tokens WHERE session_id = $1 AND revoked_at IS NULL",
            &session_id,
        )
    }

    fn set_token_column(&self, session_id: Uuid, column: &str, at: Timestamp) {
        self.client()
            .execute(
                &format!("UPDATE user_refresh_tokens SET {column} = $1 WHERE session_id = $2"),
                &[&at, &session_id],
            )
            .unwrap();
    }

    /// 기기 행 삭제를 외래 키로 막는 보호 테이블(원본 `blockDeviceDeletion`).
    fn block_device_deletion(&self, session_id: Uuid) {
        let mut c = self.client();
        c.batch_execute(
            "CREATE TABLE push_device_guard (device_id UUID NOT NULL REFERENCES push_devices(id) ON DELETE RESTRICT)",
        )
        .unwrap();
        c.execute(
            "INSERT INTO push_device_guard (device_id) SELECT id FROM push_devices WHERE session_id = $1",
            &[&session_id],
        )
        .unwrap();
    }

    fn user_active(&self, user_id: Uuid) -> bool {
        self.w.find_active_user(user_id).is_some()
    }

    fn at(&self, seconds: i64) -> Timestamp {
        plus(ts(NOW), seconds)
    }
}

fn hash(token: &str) -> String {
    FcmToken::new(token).unwrap().hash().to_owned()
}

// ---------------------------------------------------------------------------------------------
// PushDeviceServiceTest
// ---------------------------------------------------------------------------------------------

// PushDeviceServiceTest#register_liveSession_storesDevice
#[test]
fn register_live_session_stores_device() {
    let Some(f) = fx() else { return };
    let login = f.login();

    f.register(&login, "fcm-token");

    let d = f.device(login.session_id()).unwrap();
    let owner: Uuid = f
        .client()
        .query_one("SELECT user_id FROM push_devices WHERE id = $1", &[&d.id])
        .unwrap()
        .get(0);
    assert_eq!(owner, login.user_id);
    assert_eq!(d.token, "fcm-token");
    assert_eq!(d.hash, hash("fcm-token"));
    assert_eq!(d.registered_at, ts(NOW));
    assert_eq!(d.updated_at, ts(NOW));
}

// PushDeviceServiceTest#register_sameSession_keepsRowAndFirstRegistration
#[test]
fn register_same_session_keeps_row_and_first_registration() {
    for replacement in ["first-token", "replacement-token"] {
        let Some(f) = fx() else { return };
        let login = f.login();
        f.register(&login, "first-token");
        let original = f.device(login.session_id()).unwrap().id;
        f.w.clock.set(f.at(60));

        f.register(&login, replacement);

        let d = f.device(login.session_id()).unwrap();
        assert_eq!(d.id, original);
        assert_eq!(d.registered_at, ts(NOW));
        assert_eq!(d.updated_at, f.at(60));
        assert_eq!(d.token, replacement);
        assert_eq!(f.count_devices(), 1);
    }
}

// PushDeviceServiceTest#register_distinctDevicesOfSameUser_keepsBothActive
#[test]
fn register_distinct_devices_of_same_user_keeps_both_active() {
    let Some(f) = fx() else { return };
    let first = f.login();
    let second = f.login_of(first.user_id);

    f.register(&first, "first-device");
    f.register(&second, "second-device");

    assert_eq!(f.device(first.session_id()).unwrap().token, "first-device");
    assert_eq!(
        f.device(second.session_id()).unwrap().token,
        "second-device"
    );
    assert_eq!(f.count_devices(), 2);
}

// PushDeviceServiceTest#register_sameTokenOnAnotherUser_transfersActiveBinding
#[test]
fn register_same_token_on_another_user_transfers_active_binding() {
    let Some(f) = fx() else { return };
    let (first, second) = (f.login(), f.login());
    f.register(&first, "shared-token");
    let old_id = f.device(first.session_id()).unwrap().id;
    f.w.clock.set(f.at(60));

    f.register(&second, "shared-token");

    assert!(!f.device_exists(old_id));
    assert_eq!(f.device(second.session_id()).unwrap().token, "shared-token");
}

// PushDeviceServiceTest#register_previouslyDetachedSession_createsNewRow
#[test]
fn register_previously_detached_session_creates_new_row() {
    let Some(f) = fx() else { return };
    let (first, second) = (f.login(), f.login());
    f.register(&first, "shared-token");
    let original = f.device(first.session_id()).unwrap().id;
    f.register(&second, "shared-token");
    let second_id = f.device(second.session_id()).unwrap().id;
    f.w.clock.set(f.at(120));

    f.register(&first, "shared-token");

    let d = f.device(first.session_id()).unwrap();
    assert_ne!(d.id, original);
    assert_eq!(d.registered_at, f.at(120));
    assert!(!f.device_exists(original));
    assert!(!f.device_exists(second_id));
    assert_eq!(f.count_devices(), 1);
}

// PushDeviceServiceTest#register_newLoginAfterLogout_createsNewRow
#[test]
fn register_new_login_after_logout_creates_new_row() {
    for token in ["first-token", "replacement-token"] {
        let Some(f) = fx() else { return };
        let first = f.login();
        f.register(&first, "first-token");
        let original = f.device(first.session_id()).unwrap().id;
        f.w.refresh.logout(&first.token.value).unwrap();
        let second = f.login_of(first.user_id);
        f.w.clock.set(f.at(60));

        f.register(&second, token);

        let d = f.device(second.session_id()).unwrap();
        assert_ne!(d.id, original);
        assert_eq!(d.token, token);
        assert_eq!(d.registered_at, f.at(60));
        assert!(!f.device_exists(original));
        assert_eq!(f.count_devices(), 1);
    }
}

// PushDeviceServiceTest#register_rotatedSessionWithLiveSuccessor_acceptsRegistration
#[test]
fn register_rotated_session_with_live_successor_accepts_registration() {
    let Some(f) = fx() else { return };
    let login = f.login();
    f.w.refresh.refresh(&login.token.value).unwrap();

    f.register(&login, "fcm-token");

    assert_eq!(f.device(login.session_id()).unwrap().token, "fcm-token");
}

// PushDeviceServiceTest#register_unusableSession_doesNotChangeExistingBinding
#[test]
fn register_unusable_session_does_not_change_existing_binding() {
    for column in ["revoked_at", "rotated_at"] {
        let Some(f) = fx() else { return };
        let existing = f.login();
        let invalid = f.login();
        f.register(&existing, "shared-token");
        f.set_token_column(invalid.session_id(), column, ts(NOW));

        reauth_required(f.try_register(Some(invalid.user_id), Some(invalid.session_id())));

        assert_eq!(
            f.device(existing.session_id()).unwrap().token,
            "shared-token"
        );
        assert_eq!(f.count_devices(), 1);
    }
}

// PushDeviceServiceTest#register_expiryBoundary_acceptsOnlyBeforeExpiry
#[test]
fn register_expiry_boundary_accepts_only_before_expiry() {
    for column in ["expires_at", "absolute_expires_at"] {
        let Some(f) = fx() else { return };
        let login = f.login();
        let expiry = f.at(60);
        // 비활동 만료가 절대 만료를 넘지 않는 기존 DB 제약을 유지한다.
        if column == "absolute_expires_at" {
            f.set_token_column(login.session_id(), "expires_at", expiry);
        }
        f.set_token_column(login.session_id(), column, expiry);
        f.w.clock.set(
            expiry
                .checked_sub(jiff::SignedDuration::from_micros(1))
                .unwrap(),
        );

        f.register(&login, "before-expiry");
        f.w.clock.set(expiry);

        reauth_required(f.try_register(Some(login.user_id), Some(login.session_id())));
        f.w.clock.set(
            expiry
                .checked_add(jiff::SignedDuration::from_micros(1))
                .unwrap(),
        );
        reauth_required(f.try_register(Some(login.user_id), Some(login.session_id())));
        assert_eq!(f.device(login.session_id()).unwrap().token, "before-expiry");
    }
}

// PushDeviceServiceTest#register_unknownOrOtherUsersSession_requiresReauthentication
#[test]
fn register_unknown_or_other_users_session_requires_reauthentication() {
    let Some(f) = fx() else { return };
    let (owner, other) = (f.login(), f.login());

    reauth_required(f.try_register(Some(other.user_id), Some(owner.session_id())));
    reauth_required(f.try_register(Some(owner.user_id), Some(Uuid::new_v4())));
    reauth_required(f.try_register(Some(Uuid::new_v4()), Some(owner.session_id())));
    reauth_required(f.try_register(Some(owner.user_id), None));
    reauth_required(f.try_register(None, Some(owner.session_id())));
    assert_eq!(f.count_devices(), 0);
}

// PushDeviceServiceTest#register_sessionWithoutToken_requiresReauthentication
#[test]
fn register_session_without_token_requires_reauthentication() {
    let Some(f) = fx() else { return };
    let login = f.login();
    f.client()
        .execute(
            "DELETE FROM user_refresh_tokens WHERE session_id = $1",
            &[&login.session_id()],
        )
        .unwrap();

    reauth_required(f.try_register(Some(login.user_id), Some(login.session_id())));
    assert_eq!(f.count_devices(), 0);
}

// PushDeviceServiceTest#register_withdrawnUser_rejectsRegistration
#[test]
fn register_withdrawn_user_rejects_registration() {
    let Some(f) = fx() else { return };
    let login = f.login();
    f.w.withdraw_directly(login.user_id);

    let result = f
        .devices
        .register(Some(login.user_id), Some(login.session_id()), "fcm-token");

    let ApiError::App(e) = result.unwrap_err() else {
        panic!("도메인 오류여야 한다");
    };
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Unauthorized, ErrorCode::UNAUTHORIZED)
    );
    assert_eq!(f.count_devices(), 0);
}

// FcmToken 검증은 잠금 전에 한다: 빈 토큰은 400이고 기기를 만들지 않는다.
#[test]
fn register_blank_token_is_rejected_before_touching_the_database() {
    let Some(f) = fx() else { return };
    let login = f.login();

    let result = f
        .devices
        .register(Some(login.user_id), Some(login.session_id()), "  ");

    let ApiError::App(e) = result.unwrap_err() else {
        panic!("도메인 오류여야 한다");
    };
    assert_eq!(
        (e.kind, e.message.as_str()),
        (ErrorKind::Business, "FCM 토큰이 필요합니다.")
    );
    assert_eq!(f.count_devices(), 0);
}

// ---------------------------------------------------------------------------------------------
// PushDeviceJpaRepositoryTest (제약)
// ---------------------------------------------------------------------------------------------

fn insert_raw(
    f: &Fx,
    session_id: Uuid,
    token: Option<&str>,
    token_hash: Option<&str>,
) -> Result<u64, postgres::Error> {
    f.client().execute(
        "INSERT INTO push_devices (user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at) \
         VALUES ((SELECT user_id FROM user_refresh_tokens WHERE session_id = $1 LIMIT 1), $1, $2, $3, \
         CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        &[&session_id, &token, &token_hash],
    )
}

fn violated(error: postgres::Error) -> String {
    format!("{error:?}")
}

// PushDeviceJpaRepositoryTest#insert_duplicateSession_rejectsSecondRow
#[test]
fn insert_duplicate_session_rejects_second_row() {
    let Some(f) = fx() else { return };
    let session = f.login().session_id();
    insert_raw(&f, session, Some("first-token"), Some(&hash("first-token"))).unwrap();

    let error = insert_raw(
        &f,
        session,
        Some("second-token"),
        Some(&hash("second-token")),
    )
    .unwrap_err();

    assert!(violated(error).contains("ux_push_devices_session"));
}

// PushDeviceJpaRepositoryTest#insert_duplicateToken_rejectsSecondBinding
#[test]
fn insert_duplicate_token_rejects_second_binding() {
    let Some(f) = fx() else { return };
    let (first, second) = (f.login().session_id(), f.login().session_id());
    let h = hash("shared-token");
    insert_raw(&f, first, Some("shared-token"), Some(&h)).unwrap();

    let error = insert_raw(&f, second, Some("shared-token"), Some(&h)).unwrap_err();

    assert!(violated(error).contains("ux_push_devices_fcm_token"));
}

// PushDeviceJpaRepositoryTest#insert_deletedDuplicateToken_allowsNewBinding
#[test]
fn insert_deleted_duplicate_token_allows_new_binding() {
    let Some(f) = fx() else { return };
    let (first, second) = (f.login().session_id(), f.login().session_id());
    let h = hash("shared-token");
    insert_raw(&f, first, Some("shared-token"), Some(&h)).unwrap();
    f.client()
        .execute("DELETE FROM push_devices WHERE session_id = $1", &[&first])
        .unwrap();

    insert_raw(&f, second, Some("shared-token"), Some(&h)).unwrap();

    assert_eq!(
        f.count_with(
            "SELECT COUNT(*) FROM push_devices WHERE fcm_token_hash = $1",
            &h
        ),
        1
    );
}

// PushDeviceJpaRepositoryTest#insert_incompleteOrInvalidTokenPair_rejectsRow
#[test]
fn insert_incomplete_or_invalid_token_pair_rejects_row() {
    for (token, token_hash) in [
        (Some("token"), None),
        (None, Some("invalid-hash")),
        (Some("token"), Some("invalid-hash")),
    ] {
        let Some(f) = fx() else { return };
        let session = f.login().session_id();
        assert!(insert_raw(&f, session, token, token_hash).is_err());
    }
}

// PushDeviceJpaRepositoryTest#insert_deviceWithoutIdentity_rejectsRow
#[test]
fn insert_device_without_identity_rejects_row() {
    for column in ["user_id", "session_id"] {
        let Some(f) = fx() else { return };
        let login = f.login();
        let user: Option<Uuid> = (column != "user_id").then_some(login.user_id);
        let session: Option<Uuid> = (column != "session_id").then_some(login.session_id());

        let error = f
            .client()
            .execute(
                "INSERT INTO push_devices (user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at) \
                 VALUES ($1, $2, 'device-token', $3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                &[&user, &session, &hash("device-token")],
            )
            .unwrap_err();

        assert!(violated(error).contains(column), "{column}");
    }
}

// ---------------------------------------------------------------------------------------------
// PushDeviceMigrationTest
// ---------------------------------------------------------------------------------------------

// PushDeviceMigrationTest#migrate_existingRotatedTokens_preservesDataAndCreatesDevicesWithoutLoginTable
#[test]
fn migrate_existing_rotated_tokens_preserves_data_and_creates_devices_without_login_table() {
    use chalkak_db::migrate::Migrator;
    let Some(db) = TestDb::create() else { return };
    let mut c = db.client();
    let schema = format!("push_device_migration_{}", Uuid::new_v4().simple());
    c.batch_execute(&format!("CREATE SCHEMA {schema}")).unwrap();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    let migrate = |c: &mut Client, target: &str| {
        let all = Migrator::from_dir(&dir).unwrap();
        let selected: Vec<_> = all
            .migrations()
            .iter()
            .filter(|m| m.script.trim_start_matches('V').split("__").next().unwrap() <= target)
            .cloned()
            .collect();
        Migrator::new(selected)
            .unwrap()
            .schema(&schema)
            .migrate(c)
            .unwrap();
    };
    // 기기 테이블 생성 전 버전에 기존 RT와 수신 설정을 준비한다.
    migrate(&mut c, "202609301100");
    c.batch_execute(&format!("SET search_path TO {schema}"))
        .unwrap();
    let registered = ts("2026-09-30T00:00:00Z");
    let user_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    c.execute(
        "INSERT INTO users (id, email, signature_original_storage_key, topic_push_enabled, moderation_push_enabled) \
         VALUES ($1, 'active@test.chalkak', 'signatures/active', false, true)",
        &[&user_id],
    )
    .unwrap();
    for (hash, rotated_at) in [("1".repeat(64), Some(registered)), ("2".repeat(64), None)] {
        c.execute(
            "INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, absolute_expires_at, rotated_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[&user_id, &session_id, &hash, &plus(registered, 3600), &plus(registered, 7200), &rotated_at],
        )
        .unwrap();
    }
    let tokens_before = c
        .query(
            "SELECT id::text, token_hash, rotated_at FROM user_refresh_tokens ORDER BY id",
            &[],
        )
        .unwrap();
    let settings_before = c
        .query(
            "SELECT id::text, topic_push_enabled, moderation_push_enabled FROM users ORDER BY id",
            &[],
        )
        .unwrap();

    // 기기 테이블을 만드는 마이그레이션을 적용한다.
    c.batch_execute("SET search_path TO public").unwrap();
    migrate(&mut c, "202609301400");
    c.batch_execute(&format!("SET search_path TO {schema}"))
        .unwrap();

    let rows = |c: &mut Client, sql: &str| -> Vec<String> {
        c.query(sql, &[])
            .unwrap()
            .iter()
            .map(|r| format!("{r:?}"))
            .collect()
    };
    let _ = rows;
    assert_eq!(c.query("SELECT * FROM push_devices", &[]).unwrap().len(), 0);
    let tokens_after = c
        .query(
            "SELECT id::text, token_hash, rotated_at FROM user_refresh_tokens ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(tokens_before.len(), tokens_after.len());
    for (b, a) in tokens_before.iter().zip(&tokens_after) {
        assert_eq!(b.get::<_, String>(0), a.get::<_, String>(0));
        assert_eq!(b.get::<_, String>(1), a.get::<_, String>(1));
        assert_eq!(
            b.get::<_, Option<Timestamp>>(2),
            a.get::<_, Option<Timestamp>>(2)
        );
    }
    let settings_after = c
        .query(
            "SELECT id::text, topic_push_enabled, moderation_push_enabled FROM users ORDER BY id",
            &[],
        )
        .unwrap();
    assert_eq!(settings_before.len(), settings_after.len());
    for (b, a) in settings_before.iter().zip(&settings_after) {
        assert_eq!(b.get::<_, String>(0), a.get::<_, String>(0));
        assert_eq!(b.get::<_, bool>(1), a.get::<_, bool>(1));
        assert_eq!(b.get::<_, bool>(2), a.get::<_, bool>(2));
    }
    let count = |c: &mut Client, sql: &str| -> i64 { c.query_one(sql, &[&schema]).unwrap().get(0) };
    assert_eq!(
        count(
            &mut c,
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name = 'login_sessions'"
        ),
        0
    );
    assert_eq!(
        c.query_one(
            "SELECT COUNT(*) FROM flyway_schema_history WHERE version IN ('202609301130', '202610061400', '202610061500')",
            &[]
        )
        .unwrap()
        .get::<_, i64>(0),
        0
    );
    // 기기는 회원과 로그인에 직접 연결하고 해제된 행은 보관하지 않는다.
    c.execute(
        "INSERT INTO push_devices (user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at) \
         VALUES ($1, $2, 'active-token', $3, $4, $4)",
        &[&user_id, &session_id, &hash("active-token"), &registered],
    )
    .unwrap();
    assert_eq!(
        c.query_one(
            "SELECT COUNT(*) FROM push_devices WHERE user_id = $1 AND session_id = $2 AND fcm_token = 'active-token'",
            &[&user_id, &session_id]
        )
        .unwrap()
        .get::<_, i64>(0),
        1
    );
    assert_eq!(
        count(
            &mut c,
            "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = $1 AND table_name = 'push_devices' \
             AND column_name IN ('installation_id', 'platform', 'disabled_at')"
        ),
        0
    );
    c.batch_execute("SET search_path TO public").unwrap();
}

// ---------------------------------------------------------------------------------------------
// PushDeviceConcurrencyTest
// ---------------------------------------------------------------------------------------------

fn run_concurrently(first: impl FnOnce() + Send + 'static, second: impl FnOnce() + Send + 'static) {
    let barrier = Arc::new(Barrier::new(2));
    let (b1, b2) = (barrier.clone(), barrier);
    let t1 = thread::spawn(move || {
        b1.wait();
        first()
    });
    let t2 = thread::spawn(move || {
        b2.wait();
        second()
    });
    t1.join().unwrap();
    t2.join().unwrap();
}

fn registering(f: &Fx, login: &Login, token: &'static str) -> impl FnOnce() + Send + 'static {
    let (devices, user, session) = (f.devices.clone(), login.user_id, login.session_id());
    move || devices.register(Some(user), Some(session), token).unwrap()
}

// PushDeviceConcurrencyTest#register_sameTokenConcurrently_keepsSingleActiveBinding
#[test]
fn register_same_token_concurrently_keeps_single_active_binding() {
    let Some(f) = fx() else { return };
    let (first, second) = (f.login(), f.login());

    run_concurrently(
        registering(&f, &first, "shared-token"),
        registering(&f, &second, "shared-token"),
    );

    assert_eq!(f.count_devices(), 1);
    assert_eq!(
        f.w.count("SELECT COUNT(*) FROM push_devices WHERE fcm_token = 'shared-token'"),
        1
    );
}

// PushDeviceConcurrencyTest#register_sameSessionConcurrently_keepsSingleDeviceRow
#[test]
fn register_same_session_concurrently_keeps_single_device_row() {
    let Some(f) = fx() else { return };
    let login = f.login();

    run_concurrently(
        registering(&f, &login, "first-token"),
        registering(&f, &login, "second-token"),
    );

    assert_eq!(f.count_devices(), 1);
    let token = f.device(login.session_id()).unwrap().token;
    assert!(token == "first-token" || token == "second-token");
}

// PushDeviceConcurrencyTest#register_swappingTokensConcurrently_completesWithoutDeadlock
#[test]
fn register_swapping_tokens_concurrently_completes_without_deadlock() {
    let Some(f) = fx() else { return };
    let (first, second) = (f.login(), f.login());
    f.register(&first, "first-token");
    f.register(&second, "second-token");

    run_concurrently(
        registering(&f, &first, "second-token"),
        registering(&f, &second, "first-token"),
    );

    assert_eq!(f.device(first.session_id()).unwrap().token, "second-token");
    assert_eq!(f.device(second.session_id()).unwrap().token, "first-token");
    assert_eq!(f.count_devices(), 2);
}

// ---------------------------------------------------------------------------------------------
// PushDeviceLogoutTest
// ---------------------------------------------------------------------------------------------

struct Registered {
    login: Login,
    device_id: Uuid,
}

impl Fx {
    fn registered(&self, user: Uuid, token: &str) -> Registered {
        let login = self.login_of(user);
        self.register(&login, token);
        let device_id = self.device(login.session_id()).unwrap().id;
        Registered { login, device_id }
    }

    fn usable(&self, login: &Login, at: Timestamp) -> bool {
        use chalkak_app::auth::repository::RefreshTokenRepository;
        RefreshTokenRepository
            .exists_usable_by_session_id_and_user_id(
                &mut self.client(),
                login.session_id(),
                login.user_id,
                at,
            )
            .unwrap()
    }
}

// PushDeviceLogoutTest#logout_registeredSession_deletesOnlyItsDevice
#[test]
fn logout_registered_session_deletes_only_its_device() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let first = f.registered(user, "first-device");
    let second = f.registered(user, "second-device");
    let other = f.registered(f.w.save_user().id, "other-user-device");
    f.w.clock.set(f.at(60));

    f.w.refresh.logout(&first.login.token.value).unwrap();

    assert!(!f.device_exists(first.device_id));
    assert_eq!(
        f.device(second.login.session_id()).unwrap().token,
        "second-device"
    );
    assert_eq!(
        f.device(other.login.session_id()).unwrap().token,
        "other-user-device"
    );
    assert!(!f.usable(&first.login, f.at(60)));
    assert!(f.usable(&second.login, f.at(60)));
    reauth_required(
        f.devices
            .register(Some(user), Some(first.login.session_id()), "new-token"),
    );
}

// PushDeviceLogoutTest#logout_rotatedToken_deletesDeviceAndRevokesSuccessor
#[test]
fn logout_rotated_token_deletes_device_and_revokes_successor() {
    let Some(f) = fx() else { return };
    let r = f.registered(f.w.save_user().id, "rotated-device");
    f.w.refresh.refresh(&r.login.token.value).unwrap();

    f.w.refresh.logout(&r.login.token.value).unwrap();

    assert!(!f.device_exists(r.device_id));
    assert!(!f.usable(&r.login, ts(NOW)));
}

// PushDeviceLogoutTest#logout_revokedToken_deletesRemainingDevice
#[test]
fn logout_revoked_token_deletes_remaining_device() {
    let Some(f) = fx() else { return };
    let r = f.registered(f.w.save_user().id, "revoked-device");
    use chalkak_app::auth::repository::RefreshTokenRepository;
    RefreshTokenRepository
        .revoke_session(&mut f.client(), r.login.session_id(), ts(NOW))
        .unwrap();

    f.w.refresh.logout(&r.login.token.value).unwrap();

    assert!(!f.device_exists(r.device_id));
}

// PushDeviceLogoutTest#logout_repeatedOrUnknownToken_keepsExistingState
#[test]
fn logout_repeated_or_unknown_token_keeps_existing_state() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let first = f.registered(user, "first-device");
    let second = f.registered(user, "second-device");
    f.w.clock.set(f.at(60));
    f.w.refresh.logout(&first.login.token.value).unwrap();
    f.w.clock.set(f.at(120));

    f.w.refresh.logout(&first.login.token.value).unwrap();
    f.w.refresh.logout("unknown-refresh-token").unwrap();

    assert!(!f.device_exists(first.device_id));
    assert_eq!(
        f.device(second.login.session_id()).unwrap().token,
        "second-device"
    );
}

// PushDeviceLogoutTest#logout_withoutRegisteredDevice_revokesRefreshToken
#[test]
fn logout_without_registered_device_revokes_refresh_token() {
    let Some(f) = fx() else { return };
    let login = f.login();

    f.w.refresh.logout(&login.token.value).unwrap();

    assert!(f.device(login.session_id()).is_none());
    assert!(!f.usable(&login, ts(NOW)));
}

// PushDeviceLogoutTest#logout_previousSession_keepsTransferredTokenOnNewSession
#[test]
fn logout_previous_session_keeps_transferred_token_on_new_session() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let first = f.registered(user, "shared-device");
    let second = f.registered(user, "shared-device");

    f.w.refresh.logout(&first.login.token.value).unwrap();

    assert_eq!(
        f.device(second.login.session_id()).unwrap().token,
        "shared-device"
    );
    assert!(f.usable(&second.login, ts(NOW)));
}

// PushDeviceLogoutTest#logout_expiredToken_deletesDevice
#[test]
fn logout_expired_token_deletes_device() {
    let Some(f) = fx() else { return };
    let r = f.registered(f.w.save_user().id, "expired-device");
    f.set_token_column(r.login.session_id(), "expires_at", f.at(-1));

    f.w.refresh.logout(&r.login.token.value).unwrap();

    assert!(!f.device_exists(r.device_id));
}

// ---------------------------------------------------------------------------------------------
// PushDeviceLogoutTransactionTest
// ---------------------------------------------------------------------------------------------

// PushDeviceLogoutTransactionTest#logout_deviceDeletionFails_rollsBackRefreshTokenRevocation
#[test]
fn logout_device_deletion_fails_rolls_back_refresh_token_revocation() {
    let Some(f) = fx() else { return };
    let r = f.registered(f.w.save_user().id, "rollback-token");
    f.block_device_deletion(r.login.session_id());

    assert!(f.w.refresh.logout(&r.login.token.value).is_err());

    assert_eq!(f.live_tokens(r.login.session_id()), 1);
    assert_eq!(
        f.device(r.login.session_id()).unwrap().token,
        "rollback-token"
    );
}

/// 직접 연 트랜잭션 안에서 `first`를 실행하고 열어 둔 채, `second`가 잠금을 기다리는 것을 확인한 뒤 커밋한다.
/// 두 번째 결과를 돌려준다.
fn hold_then_run<R: Send + 'static>(
    f: &Fx,
    first: impl FnOnce(&mut chalkak_db::Tx<'_>),
    second: impl FnOnce() -> R + Send + 'static,
) -> R {
    let mut conn = f.w.pool.get().unwrap();
    let mut tx = conn.begin().unwrap();
    first(&mut tx);
    let handle = thread::spawn(second);
    // 두 번째 요청의 연결 pid를 알 수 없으므로, 잠금을 기다리는 세션이 생기길 관찰자가 기다린다(`awaitBlocked`).
    let mut observer = f.client();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let waiting: i64 = observer
            .query_one(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() \
                 AND cardinality(pg_blocking_pids(pid)) > 0",
                &[],
            )
            .unwrap()
            .get(0);
        if waiting > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "두 번째 요청이 잠금을 기다리지 않았습니다."
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!handle.is_finished());
    tx.commit().unwrap();
    handle.join().unwrap()
}

// PushDeviceLogoutTransactionTest#registerAndLogout_concurrentRequests_leavesNoActiveDevice (logoutFirst=false)
#[test]
fn register_then_logout_concurrent_leaves_no_active_device() {
    let Some(f) = fx() else { return };
    let r = f.registered(f.w.save_user().id, "initial-token");
    let (devices, refresh, token) = (
        f.devices.clone(),
        f.w.refresh.clone(),
        r.login.token.value.clone(),
    );
    let (user, session) = (r.login.user_id, r.login.session_id());

    // 첫 요청: 등록(토큰 변경)을 열어 둔다. 두 번째: 로그아웃이 같은 로그인 잠금을 기다린다.
    hold_then_run(
        &f,
        |tx| {
            devices
                .register_in(tx, Some(user), Some(session), "updated-token")
                .unwrap()
        },
        move || refresh.logout(&token).unwrap(),
    );

    assert_eq!(f.live_tokens(session), 0);
    assert!(!f.device_exists(r.device_id));
}

// PushDeviceLogoutTransactionTest#registerAndLogout_concurrentRequests_leavesNoActiveDevice (logoutFirst=true)
// 로그아웃이 먼저 커밋되면 뒤이은 등록은 재로그인 오류이고 기기와 유효한 RT는 남지 않는다.
#[test]
fn logout_then_register_leaves_no_active_device() {
    let Some(f) = fx() else { return };
    let r = f.registered(f.w.save_user().id, "initial-token");

    f.w.refresh.logout(&r.login.token.value).unwrap();
    reauth_required(f.devices.register(
        Some(r.login.user_id),
        Some(r.login.session_id()),
        "updated-token",
    ));

    assert_eq!(f.live_tokens(r.login.session_id()), 0);
    assert!(!f.device_exists(r.device_id));
}

// ---------------------------------------------------------------------------------------------
// PushDeviceRefreshRevocationTest
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Cause {
    Reused,
    InactivityExpired,
    AbsoluteExpired,
}

const CAUSES: [Cause; 3] = [
    Cause::Reused,
    Cause::InactivityExpired,
    Cause::AbsoluteExpired,
];

impl Fx {
    fn revoked_at(&self) -> Timestamp {
        plus(ts("2026-10-01T00:00:00Z"), 60)
    }

    fn prepare_revocation(&self, login: &Login, cause: Cause) {
        match cause {
            Cause::Reused => {
                self.w.refresh.refresh(&login.token.value).unwrap();
            }
            Cause::InactivityExpired => {
                self.set_token_column(login.session_id(), "expires_at", self.revoked_at())
            }
            Cause::AbsoluteExpired => {
                self.set_token_column(login.session_id(), "expires_at", self.revoked_at());
                self.set_token_column(login.session_id(), "absolute_expires_at", self.revoked_at());
            }
        }
        self.w.clock.set(self.revoked_at());
    }

    fn assert_revoked(&self, session_id: Uuid) {
        assert_eq!(self.live_tokens(session_id), 0);
        let n: i64 = self
            .client()
            .query_one(
                "SELECT COUNT(*) FROM user_refresh_tokens WHERE session_id = $1 AND revoked_at = $2",
                &[&session_id, &self.revoked_at()],
            )
            .unwrap()
            .get(0);
        assert!(n > 0);
    }

    fn assert_active(&self, r: &Registered, token: &str, registered_at: Timestamp) {
        let d = self.device(r.login.session_id()).unwrap();
        assert_eq!(d.registered_at, registered_at);
        assert_eq!(d.token, token);
    }
}

fn refresh_world() -> Option<Fx> {
    let f = fx()?;
    f.w.clock.set(ts("2026-10-01T00:00:00Z"));
    Some(f)
}

// PushDeviceRefreshRevocationTest#refreshApi_reusedOrExpiredToken_commitsRevocationAndDeviceCleanup
#[test]
fn refresh_reused_or_expired_token_commits_revocation_and_device_cleanup() {
    for cause in CAUSES {
        let Some(f) = refresh_world() else { return };
        let user = f.w.save_user().id;
        let first = f.registered(user, "first-device");
        let second = f.registered(user, "second-device");
        let other = f.registered(f.w.save_user().id, "other-device");
        f.prepare_revocation(&first.login, cause);

        reauth_required(f.w.refresh.refresh(&first.login.token.value));

        f.assert_revoked(first.login.session_id());
        assert!(!f.device_exists(first.device_id), "{cause:?}");
        f.assert_active(&second, "second-device", ts("2026-10-01T00:00:00Z"));
        f.assert_active(&other, "other-device", ts("2026-10-01T00:00:00Z"));
        assert_eq!(f.live_tokens(second.login.session_id()), 1);
        assert_eq!(f.live_tokens(other.login.session_id()), 1);
    }
}

// PushDeviceRefreshRevocationTest#refresh_deviceCleanupFails_rollsBackTokenRevocation
#[test]
fn refresh_device_cleanup_fails_rolls_back_token_revocation() {
    for cause in CAUSES {
        let Some(f) = refresh_world() else { return };
        let r = f.registered(f.w.save_user().id, "rollback-device");
        f.prepare_revocation(&r.login, cause);
        let live = f.live_tokens(r.login.session_id());
        f.block_device_deletion(r.login.session_id());

        assert!(
            f.w.refresh.refresh(&r.login.token.value).is_err(),
            "{cause:?}"
        );

        assert_eq!(f.live_tokens(r.login.session_id()), live);
        f.assert_active(&r, "rollback-device", ts("2026-10-01T00:00:00Z"));
    }
}

// PushDeviceRefreshRevocationTest#refresh_liveToken_keepsRegisteredDevice
#[test]
fn refresh_live_token_keeps_registered_device() {
    let Some(f) = refresh_world() else { return };
    let user = f.w.save_user().id;
    let r = f.registered(user, "normal-device");
    f.w.clock.set(f.revoked_at());

    let result = f.w.refresh.refresh(&r.login.token.value).unwrap();

    assert_eq!(result.refresh_token.session_id, r.login.session_id());
    f.assert_active(&r, "normal-device", ts("2026-10-01T00:00:00Z"));
    assert_eq!(f.live_tokens(r.login.session_id()), 2);
    f.devices
        .register(
            Some(user),
            Some(result.refresh_token.session_id),
            "updated-device",
        )
        .unwrap();
    f.assert_active(&r, "updated-device", ts("2026-10-01T00:00:00Z"));
}

// PushDeviceRefreshRevocationTest#refresh_sessionWithoutDevice_revokesTokens
#[test]
fn refresh_session_without_device_revokes_tokens() {
    let Some(f) = refresh_world() else { return };
    let login = f.login();
    f.prepare_revocation(&login, Cause::Reused);

    reauth_required(f.w.refresh.refresh(&login.token.value));

    f.assert_revoked(login.session_id());
    assert!(f.device(login.session_id()).is_none());
}

// PushDeviceRefreshRevocationTest#refresh_previousSession_keepsTransferredDevice
#[test]
fn refresh_previous_session_keeps_transferred_device() {
    let Some(f) = refresh_world() else { return };
    let user = f.w.save_user().id;
    let first = f.registered(user, "shared-device");
    let second = f.registered(user, "shared-device");
    f.prepare_revocation(&first.login, Cause::Reused);

    reauth_required(f.w.refresh.refresh(&first.login.token.value));

    f.assert_revoked(first.login.session_id());
    assert!(!f.device_exists(first.device_id));
    f.assert_active(&second, "shared-device", ts("2026-10-01T00:00:00Z"));
}

// PushDeviceRefreshRevocationTest#refresh_alreadyRevokedToken_keepsDeviceDeleted
#[test]
fn refresh_already_revoked_token_keeps_device_deleted() {
    let Some(f) = refresh_world() else { return };
    let r = f.registered(f.w.save_user().id, "repeated-device");
    f.prepare_revocation(&r.login, Cause::Reused);
    reauth_required(f.w.refresh.refresh(&r.login.token.value));
    f.w.clock.set(plus(f.revoked_at(), 60));

    reauth_required(f.w.refresh.refresh(&r.login.token.value));

    f.assert_revoked(r.login.session_id());
    assert!(!f.device_exists(r.device_id));
    assert!(f.device(r.login.session_id()).is_none());
}

// ---------------------------------------------------------------------------------------------
// PushDeviceWithdrawalTest
// ---------------------------------------------------------------------------------------------

// PushDeviceWithdrawalTest#withdraw_multipleDevices_deletesAllOwnedDevicesOnly
#[test]
fn withdraw_multiple_devices_deletes_all_owned_devices_only() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let first = f.registered(user, "first-token");
    let second = f.registered(user, "second-token");
    let other_user = f.w.save_user().id;
    let other = f.registered(other_user, "other-token");

    f.w.withdrawal.withdraw(user).unwrap();

    assert!(!f.device_exists(first.device_id));
    assert!(!f.device_exists(second.device_id));
    assert_eq!(f.w.live_refresh_token_count(user), 0);
    assert!(!f.user_active(user));
    assert_eq!(
        f.device(other.login.session_id()).unwrap().token,
        "other-token"
    );
    assert_eq!(f.w.live_refresh_token_count(other_user), 1);
}

// PushDeviceWithdrawalTest#withdraw_noDevice_revokesRefreshToken
#[test]
fn withdraw_no_device_revokes_refresh_token() {
    let Some(f) = fx() else { return };
    let login = f.login();

    f.w.withdrawal.withdraw(login.user_id).unwrap();

    assert!(!f.user_active(login.user_id));
    assert!(f.device(login.session_id()).is_none());
    assert_eq!(
        f.count_with(
            "SELECT COUNT(*) FROM user_refresh_tokens WHERE session_id = $1 AND revoked_at IS NOT NULL",
            &login.session_id()
        ),
        1
    );
}

// PushDeviceWithdrawalTest#withdraw_revokedRefreshToken_deletesRemainingDevice
#[test]
fn withdraw_revoked_refresh_token_deletes_remaining_device() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let r = f.registered(user, "remaining-token");
    f.set_token_column(r.login.session_id(), "revoked_at", f.at(-60));

    f.w.withdrawal.withdraw(user).unwrap();

    assert!(!f.device_exists(r.device_id));
    assert!(!f.user_active(user));
}

// PushDeviceWithdrawalTest#withdraw_transferredToken_preservesNewOwnerAndDeletedPreviousDevice
#[test]
fn withdraw_transferred_token_preserves_new_owner_and_deleted_previous_device() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let original = f.registered(user, "shared-token");
    let other_user = f.w.save_user().id;
    f.w.clock.set(f.at(60));
    let transferred = f.registered(other_user, "shared-token");
    f.w.clock.set(f.at(120));

    f.w.withdrawal.withdraw(user).unwrap();

    assert!(!f.device_exists(original.device_id));
    assert_eq!(
        f.device(transferred.login.session_id()).unwrap().token,
        "shared-token"
    );
}

// PushDeviceWithdrawalTest#withdraw_bannedUser_deletesDevice
#[test]
fn withdraw_banned_user_deletes_device() {
    let Some(f) = fx() else { return };
    let user = f.w.save_banned_user().id;
    let r = f.registered(user, "banned-token");

    f.w.withdrawal.withdraw(user).unwrap();

    assert!(!f.device_exists(r.device_id));
    assert!(!f.user_active(user));
}

// ---------------------------------------------------------------------------------------------
// PushDeviceWithdrawalTransactionTest
// ---------------------------------------------------------------------------------------------

// PushDeviceWithdrawalTransactionTest#withdraw_deviceDeletionFails_rollsBackUserSocialAccountAndRefreshToken
#[test]
fn withdraw_device_deletion_fails_rolls_back_user_social_account_and_refresh_token() {
    use chalkak_app::auth::domain::SocialProvider;
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let account =
        f.w.save_social_account(user, SocialProvider::Google, "rollback-subject");
    let r = f.registered(user, "rollback-token");
    f.block_device_deletion(r.login.session_id());

    assert!(f.w.withdrawal.withdraw(user).is_err());

    assert!(f.user_active(user));
    assert_eq!(
        f.w.find_social_account(SocialProvider::Google, "rollback-subject")
            .unwrap()
            .0
            .id,
        account
    );
    assert_eq!(f.live_tokens(r.login.session_id()), 1);
    assert_eq!(
        f.device(r.login.session_id()).unwrap().token,
        "rollback-token"
    );
}

// PushDeviceWithdrawalTransactionTest#registerAndWithdraw_concurrentRequests_leavesNoActiveDevice (withdrawalFirst=false)
// 기존 연결을 옮기는 등록이 먼저 잠금을 쥐면 탈퇴가 기다렸다가 두 로그인의 기기와 RT를 모두 해제한다.
#[test]
fn register_then_withdraw_concurrent_leaves_no_active_device() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let issued = f.login_of(user);
    f.register(&issued, "initial-token");
    let next = f.login_of(user);
    let (devices, withdrawal) = (f.devices.clone(), f.w.withdrawal.clone());
    let next_session = next.session_id();

    hold_then_run(
        &f,
        |tx| {
            devices
                .register_in(tx, Some(user), Some(next_session), "initial-token")
                .unwrap()
        },
        move || withdrawal.withdraw(user).unwrap(),
    );

    assert_eq!(f.w.live_refresh_token_count(user), 0);
    assert!(!f.user_active(user));
    assert_eq!(
        f.count_with(
            "SELECT COUNT(*) FROM push_devices WHERE user_id = $1",
            &user
        ),
        0
    );
}

// PushDeviceWithdrawalTransactionTest#registerAndWithdraw_concurrentRequests_leavesNoActiveDevice (withdrawalFirst=true)
// 탈퇴가 먼저 커밋되면 뒤이은 등록은 탈퇴 회원 오류이고 기기와 유효한 RT는 남지 않는다.
#[test]
fn withdraw_then_register_leaves_no_active_device() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let issued = f.login_of(user);
    f.register(&issued, "initial-token");
    let next = f.login_of(user);

    f.w.withdrawal.withdraw(user).unwrap();
    let result = f
        .devices
        .register(Some(user), Some(next.session_id()), "initial-token");

    let ApiError::App(e) = result.unwrap_err() else {
        panic!("도메인 오류여야 한다");
    };
    assert_eq!(e.code, ErrorCode::UNAUTHORIZED);
    assert_eq!(f.w.live_refresh_token_count(user), 0);
    assert_eq!(
        f.count_with(
            "SELECT COUNT(*) FROM push_devices WHERE user_id = $1",
            &user
        ),
        0
    );
}

// ---------------------------------------------------------------------------------------------
// NotificationSettingsServiceTest
// ---------------------------------------------------------------------------------------------

fn settings_of(topic: bool, moderation: bool) -> NotificationSettingsResult {
    NotificationSettingsResult {
        topic_push_enabled: topic,
        moderation_push_enabled: moderation,
    }
}

impl Fx {
    fn user_with_preferences(&self, topic: bool, moderation: bool) -> Uuid {
        let user = self.w.save_user();
        self.w.modify_user(user.id, |u| {
            u.update_push_preferences(topic, moderation).unwrap()
        });
        user.id
    }
}

#[track_caller]
fn unauthorized<T: std::fmt::Debug>(result: Result<T, ApiError>) {
    let ApiError::App(e) = result.expect_err("오류가 나야 한다") else {
        panic!("도메인 오류여야 한다");
    };
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Unauthorized, ErrorCode::UNAUTHORIZED)
    );
}

// NotificationSettingsServiceTest#getSettings_savedPreferences_returnsStoredValues
#[test]
fn get_settings_saved_preferences_returns_stored_values() {
    for (topic, moderation) in [(true, true), (true, false), (false, true), (false, false)] {
        let Some(f) = fx() else { return };
        let user = f.user_with_preferences(topic, moderation);
        assert_eq!(
            f.settings.get_settings(user).unwrap(),
            settings_of(topic, moderation)
        );
    }
}

// NotificationSettingsServiceTest#getSettings_missingUser_throwsUnauthorizedException
#[test]
fn get_settings_missing_user_throws_unauthorized_exception() {
    let Some(f) = fx() else { return };
    unauthorized(f.settings.get_settings(Uuid::new_v4()));
}

// NotificationSettingsServiceTest#getSettings_withdrawnUser_throwsUnauthorizedException
#[test]
fn get_settings_withdrawn_user_throws_unauthorized_exception() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    f.w.withdraw_directly(user);
    unauthorized(f.settings.get_settings(user));
}

// NotificationSettingsServiceTest#updateSettings_providedPreferences_persistsOnlyRequestedValues
#[test]
fn update_settings_provided_preferences_persists_only_requested_values() {
    #[allow(clippy::type_complexity)]
    let cases: [(bool, bool, Option<bool>, Option<bool>, bool, bool); 6] = [
        (true, true, Some(false), None, false, true),
        (true, true, None, Some(false), true, false),
        (false, false, Some(true), None, true, false),
        (false, false, None, Some(true), false, true),
        (true, true, Some(false), Some(false), false, false),
        (false, false, Some(true), Some(true), true, true),
    ];
    for (it, im, topic, moderation, et, em) in cases {
        let Some(f) = fx() else { return };
        let user = f.user_with_preferences(it, im);

        f.settings.update_settings(user, topic, moderation).unwrap();
        assert_eq!(f.settings.get_settings(user).unwrap(), settings_of(et, em));

        // 같은 요청을 반복해도 설정을 유지한다.
        f.settings.update_settings(user, topic, moderation).unwrap();
        assert_eq!(f.settings.get_settings(user).unwrap(), settings_of(et, em));
    }
}

// NotificationSettingsServiceTest#updateSettings_missingUser_throwsUnauthorizedException
#[test]
fn update_settings_missing_user_throws_unauthorized_exception() {
    let Some(f) = fx() else { return };
    unauthorized(
        f.settings
            .update_settings(Uuid::new_v4(), Some(false), None),
    );
}

// NotificationSettingsServiceTest#updateSettings_withdrawnUser_throwsUnauthorizedException
#[test]
fn update_settings_withdrawn_user_throws_unauthorized_exception() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    f.w.withdraw_directly(user);

    unauthorized(f.settings.update_settings(user, Some(false), None));

    let saved = f.w.find_user(user);
    assert!(saved.topic_push_enabled);
    assert!(saved.moderation_push_enabled);
}

// 값이 바뀌지 않으면 UPDATE하지 않으므로 updated_at도 그대로다(Hibernate 더티 체킹).
#[test]
fn update_settings_unchanged_values_keep_updated_at() {
    let Some(f) = fx() else { return };
    let user = f.w.save_user().id;
    let before = f.w.find_user(user).updated_at;
    f.w.clock.set(f.at(600));

    f.settings
        .update_settings(user, Some(true), Some(true))
        .unwrap();
    assert_eq!(f.w.find_user(user).updated_at, before);

    f.settings.update_settings(user, Some(false), None).unwrap();
    assert_eq!(f.w.find_user(user).updated_at, f.at(600));
}

// ---------------------------------------------------------------------------------------------
// NotificationSettingsConcurrencyTest
// ---------------------------------------------------------------------------------------------

// NotificationSettingsConcurrencyTest#updateSettings_concurrentPartialUpdates_preservesBothChanges
#[test]
fn update_settings_concurrent_partial_updates_preserves_both_changes() {
    for topic_first in [true, false] {
        let Some(f) = fx() else { return };
        let user = f.w.save_user().id;
        let service = f.settings.clone();
        let column = if topic_first {
            "topic_push_enabled"
        } else {
            "moderation_push_enabled"
        };

        // 첫 요청: 회원 행을 잠그고 한 설정을 바꾼 채 커밋하지 않는다.
        hold_then_run(
            &f,
            |tx| {
                tx.query_one(
                    "SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE",
                    &[&user],
                )
                .unwrap();
                tx.execute(
                    &format!("UPDATE users SET {column} = false WHERE id = $1"),
                    &[&user],
                )
                .unwrap();
            },
            move || {
                if topic_first {
                    service.update_settings(user, None, Some(false)).unwrap()
                } else {
                    service.update_settings(user, Some(false), None).unwrap()
                }
            },
        );

        assert_eq!(
            f.settings.get_settings(user).unwrap(),
            settings_of(false, false)
        );
    }
}
