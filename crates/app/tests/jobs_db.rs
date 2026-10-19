//! 주기 작업(정리 4종)의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `RefreshTokenCleanupSchedulerTest`, `UserRefreshTokenRepositoryTest#deleteUnusableBefore_*`,
//! `ConsumedSignupTokenCleanupSchedulerTest`, `PendingAppleAuthorizationCleanupSchedulerTest`를 옮겼다.
//! 스케줄러 연결(시계가 cron 시각에 닿으면 실행, 종료 때 정리)은 `FixedClock`으로 시각을 옮겨 확인한다.

use std::collections::HashSet;
use std::io::Write;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chalkak_app::auth::apple::{AppleTokenClient, AppleTokenExchangeResult};
use chalkak_app::auth::tokens::AppleCipher;
use chalkak_app::jobs::cleanup::{
    delete_expired_signup_tokens, delete_unusable_refresh_tokens,
    revoke_and_delete_expired_authorizations,
};
use chalkak_app::jobs::{AppleRevocation, JobDeps, JobsSettings};
use chalkak_app::testing::TestDb;
use chalkak_app::web::ApiError;
use chalkak_core::log::{Level, Logger};
use chalkak_core::{Clock, Env, FixedClock, ShutdownSignal};
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn plus(t: Timestamp, seconds: i64) -> Timestamp {
    t.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

const DAY: i64 = 24 * 60 * 60;

/// 로그 sink: 줄을 모아 두고 나중에 읽는다.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Lines {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn logger(lines: &Lines, clock: Arc<dyn Clock>) -> Arc<Logger> {
    Arc::new(Logger::new(Box::new(lines.clone()), Level::Info, clock))
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "시간 초과: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn count(db: &TestDb, sql: &str) -> i64 {
    db.client().query_one(sql, &[]).unwrap().get(0)
}

fn token_hash() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------------------------
// 리프레시 토큰
// ---------------------------------------------------------------------------------------------

const NOW: &str = "2026-09-02T00:00:00Z";

struct Tokens {
    db: TestDb,
    user: Uuid,
    admin: Uuid,
}

fn tokens() -> Option<Tokens> {
    let db = TestDb::create()?;
    let user = Uuid::new_v4();
    let admin = Uuid::new_v4();
    let mut c = db.client();
    c.execute(
        "INSERT INTO users (id, email, signature_original_storage_key, signature_thumbnail_storage_key) \
         VALUES ($1, 'cleanup@test.chalkak', 'sig/original', 'sig/thumbnail')",
        &[&user],
    )
    .unwrap();
    c.execute(
        "INSERT INTO admins (id, username, password) VALUES ($1, 'cleanup-operator', 'hash')",
        &[&admin],
    )
    .unwrap();
    drop(c);
    Some(Tokens { db, user, admin })
}

impl Tokens {
    /// 원본 `saveUserToken`/`saveAdminToken`: 회전된 토큰은 비활동 만료가 회전 시각이다.
    fn save(
        &self,
        table: &str,
        owner_column: &str,
        owner: Uuid,
        absolute: Timestamp,
        rotated: Option<Timestamp>,
        revoked: Option<Timestamp>,
    ) -> String {
        let hash = token_hash();
        let expires_at = rotated.unwrap_or(absolute);
        self.db
            .client()
            .execute(
                &format!(
                    "INSERT INTO {table} ({owner_column}, session_id, token_hash, expires_at, \
                     absolute_expires_at, rotated_at, revoked_at) VALUES ($1, $2, $3, $4, $5, $6, $7)"
                ),
                &[
                    &owner,
                    &Uuid::new_v4(),
                    &hash,
                    &expires_at,
                    &absolute,
                    &rotated,
                    &revoked,
                ],
            )
            .unwrap();
        hash
    }

    fn user_token(
        &self,
        absolute: Timestamp,
        rotated: Option<Timestamp>,
        revoked: Option<Timestamp>,
    ) -> String {
        self.save(
            "user_refresh_tokens",
            "user_id",
            self.user,
            absolute,
            rotated,
            revoked,
        )
    }

    fn admin_token(
        &self,
        absolute: Timestamp,
        rotated: Option<Timestamp>,
        revoked: Option<Timestamp>,
    ) -> String {
        self.save(
            "admin_refresh_tokens",
            "admin_id",
            self.admin,
            absolute,
            rotated,
            revoked,
        )
    }

    fn exists(&self, table: &str, hash: &str) -> bool {
        self.db
            .client()
            .query_one(
                &format!("SELECT COUNT(*) FROM {table} WHERE token_hash = $1"),
                &[&hash],
            )
            .unwrap()
            .get::<_, i64>(0)
            > 0
    }

    fn user_exists(&self, hash: &str) -> bool {
        self.exists("user_refresh_tokens", hash)
    }

    fn admin_exists(&self, hash: &str) -> bool {
        self.exists("admin_refresh_tokens", hash)
    }
}

// RefreshTokenCleanupSchedulerTest#deleteUnusableTokens_unusableUserTokens_deletesOnlyThose
// UserRefreshTokenRepositoryTest#deleteUnusableBefore_expiredAndLongRevokedTokens_deletesOnlyThose
#[test]
fn delete_unusable_tokens_unusable_user_tokens_deletes_only_those() {
    let Some(t) = tokens() else { return };
    let now = ts(NOW);
    let far_future = plus(now, 30 * DAY);
    let threshold = plus(now, -7 * DAY);
    let absolutely_expired = t.user_token(plus(now, -1), None, None);
    let long_revoked = t.user_token(far_future, None, Some(plus(threshold, -1)));
    let live = t.user_token(far_future, None, None);
    let just_revoked = t.user_token(far_future, None, Some(plus(now, -1)));

    let (users, admins) = delete_unusable_refresh_tokens(&t.db.pool(2), now).unwrap();

    assert_eq!((users, admins), (2, 0));
    assert!(!t.user_exists(&absolutely_expired));
    assert!(!t.user_exists(&long_revoked));
    assert!(t.user_exists(&live));
    assert!(t.user_exists(&just_revoked));
}

// RefreshTokenCleanupSchedulerTest#deleteUnusableTokens_rotatedButNotAbsolutelyExpired_keepsReuseEvidence
#[test]
fn delete_unusable_tokens_rotated_but_not_absolutely_expired_keeps_reuse_evidence() {
    let Some(t) = tokens() else { return };
    let now = ts(NOW);
    let rotated = t.user_token(plus(now, 30 * DAY), Some(plus(now, -40 * DAY)), None);

    delete_unusable_refresh_tokens(&t.db.pool(2), now).unwrap();

    assert!(t.user_exists(&rotated));
}

// RefreshTokenCleanupSchedulerTest#deleteUnusableTokens_tokensExactlyOnBoundary_areKept
// UserRefreshTokenRepositoryTest#deleteUnusableBefore: expiringNow / revokedOnThreshold
#[test]
fn delete_unusable_tokens_tokens_exactly_on_boundary_are_kept() {
    let Some(t) = tokens() else { return };
    let now = ts(NOW);
    let expiring_now = t.user_token(now, None, None);
    let revoked_on_threshold = t.user_token(plus(now, 30 * DAY), None, Some(plus(now, -7 * DAY)));

    let (users, admins) = delete_unusable_refresh_tokens(&t.db.pool(2), now).unwrap();

    assert_eq!((users, admins), (0, 0));
    assert!(t.user_exists(&expiring_now));
    assert!(t.user_exists(&revoked_on_threshold));
}

// RefreshTokenCleanupSchedulerTest#deleteUnusableTokens_unusableAdminTokens_deletesOnlyThose
#[test]
fn delete_unusable_tokens_unusable_admin_tokens_deletes_only_those() {
    let Some(t) = tokens() else { return };
    let now = ts(NOW);
    let far_future = plus(now, 30 * DAY);
    let threshold = plus(now, -7 * DAY);
    let absolutely_expired = t.admin_token(plus(now, -1), None, None);
    let long_revoked = t.admin_token(far_future, None, Some(plus(threshold, -1)));
    let live = t.admin_token(far_future, None, None);
    let just_revoked = t.admin_token(far_future, None, Some(plus(now, -1)));
    let rotated = t.admin_token(far_future, Some(plus(now, -40 * DAY)), None);

    let (users, admins) = delete_unusable_refresh_tokens(&t.db.pool(2), now).unwrap();

    assert_eq!((users, admins), (0, 2));
    assert!(!t.admin_exists(&absolutely_expired));
    assert!(!t.admin_exists(&long_revoked));
    assert!(t.admin_exists(&live));
    assert!(t.admin_exists(&just_revoked));
    assert!(t.admin_exists(&rotated));
}

// ---------------------------------------------------------------------------------------------
// 회원가입 토큰 소진 기록
// ---------------------------------------------------------------------------------------------

fn consume(db: &TestDb, expires_at: Timestamp) -> String {
    let jti = Uuid::new_v4().to_string();
    db.client()
        .execute(
            "INSERT INTO consumed_signup_tokens (jti, expires_at) VALUES ($1, $2)",
            &[&jti, &expires_at],
        )
        .unwrap();
    jti
}

fn consumed(db: &TestDb, jti: &str) -> bool {
    db.client()
        .query_one(
            "SELECT COUNT(*) FROM consumed_signup_tokens WHERE jti = $1",
            &[&jti],
        )
        .unwrap()
        .get::<_, i64>(0)
        > 0
}

// ConsumedSignupTokenCleanupSchedulerTest#deleteExpiredTokens_expiredRecord_deletesOnlyExpired
// ConsumedSignupTokenRepositoryTest#deleteAllExpiredBefore_mixedRecords_deletesOnlyExpiredOnes
#[test]
fn delete_expired_tokens_expired_record_deletes_only_expired() {
    let Some(db) = TestDb::create() else { return };
    let now = ts("2026-09-04T00:00:00Z");
    let expired = consume(&db, plus(now, -1));
    let live = consume(&db, plus(now, 300));

    let deleted = delete_expired_signup_tokens(&db.pool(2), now).unwrap();

    assert_eq!(deleted, 1);
    assert!(!consumed(&db, &expired));
    assert!(consumed(&db, &live));
}

// ConsumedSignupTokenCleanupSchedulerTest#deleteExpiredTokens_expiringExactlyNow_keepsRecord
#[test]
fn delete_expired_tokens_expiring_exactly_now_keeps_record() {
    let Some(db) = TestDb::create() else { return };
    let now = ts("2026-09-04T00:00:00Z");
    let boundary = consume(&db, now);

    delete_expired_signup_tokens(&db.pool(2), now).unwrap();

    assert!(consumed(&db, &boundary));
}

// ---------------------------------------------------------------------------------------------
// 임시 Apple 인증 정보
// ---------------------------------------------------------------------------------------------

const SUBJECT_HMAC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// `AppleTokenClient` 가짜. 폐기 요청을 기록하고, 정해 둔 토큰은 실패시키며, 호출 때 훅을 돌린다.
#[derive(Default)]
struct FakeApple {
    revoked: Mutex<Vec<String>>,
    failing: Mutex<HashSet<String>>,
    on_revoke: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeApple {
    fn revoked(&self) -> Vec<String> {
        self.revoked.lock().unwrap().clone()
    }
}

impl AppleTokenClient for FakeApple {
    fn exchange_authorization_code(&self, _: &str) -> Result<AppleTokenExchangeResult, ApiError> {
        unreachable!("정리 작업은 토큰 교환을 하지 않는다")
    }

    fn revoke_refresh_token(&self, refresh_token: &str) -> Result<(), ApiError> {
        if let Some(hook) = &*self.on_revoke.lock().unwrap() {
            hook();
        }
        self.revoked.lock().unwrap().push(refresh_token.to_owned());
        if self.failing.lock().unwrap().contains(refresh_token) {
            return Err(ApiError::Internal("Apple 통신 실패".to_owned()));
        }
        Ok(())
    }
}

struct Pending {
    db: TestDb,
    pool: Pool,
    cipher: AppleCipher,
    apple: Arc<FakeApple>,
    lines: Lines,
    clock: Arc<FixedClock>,
}

fn pending(now: &str) -> Option<Pending> {
    let db = TestDb::create()?;
    let pool = db.pool(4);
    let clock = Arc::new(FixedClock::new(ts(now)));
    Some(Pending {
        db,
        pool,
        cipher: AppleCipher::new(&[7u8; 32]),
        apple: Arc::new(FakeApple::default()),
        lines: Lines::default(),
        clock,
    })
}

impl Pending {
    fn logger(&self) -> Arc<Logger> {
        logger(&self.lines, self.clock.clone())
    }

    /// 평문 리프레시 토큰을 암호화해 임시 보관분으로 저장한다. 암호문을 돌려준다.
    fn save(&self, refresh_token: &str, expires_at: Timestamp) -> String {
        let encrypted = self.cipher.encrypt(refresh_token).unwrap();
        self.save_encrypted(&encrypted, expires_at);
        encrypted
    }

    fn save_encrypted(&self, encrypted: &str, expires_at: Timestamp) {
        self.db
            .client()
            .execute(
                "INSERT INTO pending_apple_authorizations (subject_hmac, encrypted_refresh_token, expires_at) \
                 VALUES ($1, $2, $3)",
                &[&SUBJECT_HMAC, &encrypted, &expires_at],
            )
            .unwrap();
    }

    fn remaining(&self) -> Vec<String> {
        self.db
            .client()
            .query(
                "SELECT encrypted_refresh_token FROM pending_apple_authorizations ORDER BY expires_at",
                &[],
            )
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    fn run(&self) -> Result<u64, ApiError> {
        revoke_and_delete_expired_authorizations(
            &self.pool,
            self.clock.now(),
            &self.cipher,
            self.apple.as_ref(),
            &self.logger(),
            &ShutdownSignal::new(),
        )
    }
}

// PendingAppleAuthorizationCleanupSchedulerTest#revokeAndDeleteExpiredAuthorizations_expiredRecords_revokesAndDeletes
#[test]
fn revoke_and_delete_expired_records_revokes_outside_transaction_and_deletes() {
    let Some(p) = pending("2026-09-04T00:00:00Z") else {
        return;
    };
    let now = p.clock.now();
    p.save("expired-refresh-token", plus(now, -1));
    p.save("boundary-refresh-token", now);
    let live = p.save("live-refresh-token", plus(now, 300));
    // 폐기(HTTP) 호출 중에는 DB 연결을 쥐고 있지 않다(트랜잭션 밖).
    let pool = p.pool.clone();
    let borrowed_during_revoke = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&borrowed_during_revoke);
    *p.apple.on_revoke.lock().unwrap() = Some(Box::new(move || {
        seen.lock().unwrap().push(pool.stats().active);
    }));

    let deleted = p.run().unwrap();

    assert_eq!(deleted, 2);
    // 만료가 이른 순서로 폐기한다.
    assert_eq!(
        p.apple.revoked(),
        vec!["expired-refresh-token", "boundary-refresh-token"]
    );
    assert_eq!(*borrowed_during_revoke.lock().unwrap(), vec![0, 0]);
    assert_eq!(p.remaining(), vec![live]);
}

// PendingAppleAuthorizationCleanupSchedulerTest#revokeAndDeleteExpiredAuthorizations_revocationFailure_retainsAndContinues
#[test]
fn revoke_and_delete_revocation_failure_retains_row_and_continues() {
    let Some(p) = pending("2026-09-04T00:00:00Z") else {
        return;
    };
    let now = p.clock.now();
    let failed = p.save("failed-refresh-token", plus(now, -120));
    p.save("expired-refresh-token", plus(now, -60));
    p.apple
        .failing
        .lock()
        .unwrap()
        .insert("failed-refresh-token".to_owned());

    let deleted = p.run().unwrap();

    assert_eq!(deleted, 1);
    assert_eq!(
        p.apple.revoked(),
        vec!["failed-refresh-token", "expired-refresh-token"]
    );
    assert_eq!(p.remaining(), vec![failed]);
    let log = p.lines.text();
    assert!(log.contains("\"level\":\"WARN\""), "{log}");
    assert!(
        log.contains("만료된 임시 Apple 인증 정보 폐기에 실패했습니다. id="),
        "{log}"
    );
    assert!(log.contains("Apple 통신 실패"), "{log}");
}

#[test]
fn revoke_and_delete_undecryptable_row_is_retained_and_logged() {
    let Some(p) = pending("2026-09-04T00:00:00Z") else {
        return;
    };
    let now = p.clock.now();
    p.save_encrypted("not-a-valid-ciphertext", plus(now, -60));
    p.save("expired-refresh-token", plus(now, -30));

    let deleted = p.run().unwrap();

    assert_eq!(deleted, 1);
    assert_eq!(p.apple.revoked(), vec!["expired-refresh-token"]);
    assert_eq!(p.remaining(), vec!["not-a-valid-ciphertext"]);
    assert!(p.lines.text().contains("\"level\":\"WARN\""));
}

// PendingAppleAuthorizationCleanupSchedulerTest#revokeAndDeleteExpiredAuthorizations_liveRecord_doesNothing
#[test]
fn revoke_and_delete_live_record_does_nothing() {
    let Some(p) = pending("2026-09-04T00:00:00Z") else {
        return;
    };
    let live = p.save("live-refresh-token", plus(p.clock.now(), 300));

    let deleted = p.run().unwrap();

    assert_eq!(deleted, 0);
    assert!(p.apple.revoked().is_empty());
    assert_eq!(p.remaining(), vec![live]);
}

// ---------------------------------------------------------------------------------------------
// 스케줄러 연결
// ---------------------------------------------------------------------------------------------

fn deps(p: &Pending) -> JobDeps {
    JobDeps {
        pool: p.pool.clone(),
        clock: p.clock.clone(),
        logger: p.logger(),
        publisher: None,
        apple: Some(AppleRevocation {
            cipher: p.cipher.clone(),
            client: p.apple.clone(),
        }),
    }
}

fn all_enabled() -> JobsSettings {
    JobsSettings::from_env(&Env::from_pairs(Vec::<(String, String)>::new()), false).unwrap()
}

/// 시계가 KST 04:30/04:40/04:50/매시 정각에 닿으면 각 정리가 돈다.
#[test]
fn scheduler_runs_each_cleanup_when_clock_reaches_its_kst_time() {
    // 2026-09-02 04:29:55 KST (UTC 전날 19:29:55).
    let Some(p) = pending("2026-09-01T19:29:55Z") else {
        return;
    };
    let now = p.clock.now();
    // 정리 대상: 절대 만료가 지난 회원 토큰은 FK 때문에 회원이 필요하므로 소진 기록·알림·임시 Apple로 확인한다.
    let signup_jti = consume(&p.db, plus(now, -1));
    p.save("expired-refresh-token", plus(now, -1));
    let shutdown = ShutdownSignal::new();

    let jobs = chalkak_app::jobs::start(&deps(&p), &all_enabled(), &shutdown).unwrap();
    assert_eq!(jobs.started().len(), 4);

    // 04:29:55: 정각(04:00)은 지났고 04:30 전이라 아직 아무것도 돌지 않았다.
    std::thread::sleep(Duration::from_millis(300));
    assert!(consumed(&p.db, &signup_jti));
    assert_eq!(p.remaining().len(), 1);

    // 04:40:00: 소진 기록 정리가 돈다(04:30 리프레시 정리는 지나갔고, 정각 Apple 정리는 05:00까지 안 돈다).
    p.clock.set(ts("2026-09-01T19:40:00Z"));
    wait_until("소진 기록 정리", || !consumed(&p.db, &signup_jti));
    assert_eq!(p.remaining().len(), 1, "Apple 정리는 정각에만 돈다");
    assert!(p.apple.revoked().is_empty());

    // 05:00:00 KST: 임시 Apple 정리가 돈다.
    p.clock.set(ts("2026-09-01T20:00:00Z"));
    wait_until("임시 Apple 정리", || p.remaining().is_empty());
    assert_eq!(p.apple.revoked(), vec!["expired-refresh-token"]);

    shutdown.trigger();
    jobs.join();
}

/// 서버 종료: 새 실행을 시작하지 않고, 실행 중인 정리는 남은 행을 건드리지 않고 멈추며, 모든 스레드가 끝난다.
#[test]
fn scheduler_shutdown_stops_running_cleanup_and_joins() {
    let Some(p) = pending("2026-09-01T19:59:59Z") else {
        return;
    };
    let now = p.clock.now();
    p.save("first-refresh-token", plus(now, -120));
    p.save("second-refresh-token", plus(now, -60));
    let shutdown = ShutdownSignal::new();
    // 첫 폐기 호출 중에 멈춰 서서 종료 신호를 받는다.
    let (entered_tx, entered_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    *p.apple.on_revoke.lock().unwrap() = Some(Box::new(move || {
        entered_tx.lock().unwrap().send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
    }));

    let jobs = chalkak_app::jobs::start(&deps(&p), &all_enabled(), &shutdown).unwrap();
    // cron 스레드가 시작 시각을 읽은 뒤에 시계를 옮긴다.
    std::thread::sleep(Duration::from_millis(300));
    p.clock.set(ts("2026-09-01T20:00:00Z")); // 05:00 KST: 정각 정리가 돈다.
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("정리가 폐기 호출까지 갔다");

    shutdown.trigger();
    release_tx.send(()).unwrap();
    jobs.join();

    // 실행 중이던 첫 행은 마저 처리했고, 남은 행은 건드리지 않았다.
    assert_eq!(p.apple.revoked(), vec!["first-refresh-token"]);
    assert_eq!(p.remaining().len(), 1);
}

#[test]
fn scheduler_with_scheduling_disabled_starts_nothing() {
    let Some(p) = pending("2026-09-01T19:29:55Z") else {
        return;
    };
    let mut settings = all_enabled();
    settings.scheduling_enabled = false;

    let jobs = chalkak_app::jobs::start(&deps(&p), &settings, &ShutdownSignal::new()).unwrap();

    assert!(jobs.started().is_empty());
    jobs.join();
}

#[test]
fn start_without_apple_client_fails_when_pending_cleanup_is_enabled() {
    let Some(p) = pending("2026-09-01T19:29:55Z") else {
        return;
    };
    let mut deps = deps(&p);
    deps.apple = None;

    let result = chalkak_app::jobs::start(&deps, &all_enabled(), &ShutdownSignal::new());

    assert!(result.is_err());
}

/// 런타임 상태 로그는 켜져 있으면 시작 즉시 한 줄 남긴다.
#[test]
fn scheduler_runtime_log_writes_a_line_immediately_when_enabled() {
    let Some(p) = pending("2026-09-01T19:29:55Z") else {
        return;
    };
    let mut settings = all_enabled();
    settings.runtime_log = true;
    settings.refresh_token_cleanup = false;
    settings.consumed_signup_token_cleanup = false;
    settings.notification_cleanup = false;
    settings.pending_apple_authorization_cleanup = false;
    let shutdown = ShutdownSignal::new();

    let jobs = chalkak_app::jobs::start(&deps(&p), &settings, &shutdown).unwrap();
    wait_until("런타임 로그", || {
        p.lines.text().contains("\"type\":\"runtime\"")
    });
    shutdown.trigger();
    jobs.join();

    let line = p.lines.text();
    assert!(
        line.contains("\"logger_name\":\"chalkak.runtime\""),
        "{line}"
    );
    assert!(line.contains("\"threads\":"), "{line}");
    assert!(line.contains("\"hikariActive\":"), "{line}");
    assert_eq!(
        count(&p.db, "SELECT COUNT(*) FROM pending_apple_authorizations"),
        0
    );
}
