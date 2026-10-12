//! 인증 저장소·리프레시 토큰 서비스의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `UserRefreshTokenRepositoryTest`, `ConsumedSignupTokenRepositoryTest`,
//! `ConsumedSignupTokenRepositoryConcurrencyTest`, `PendingAppleAuthorizationRepositoryTest`,
//! `AppleAuthorizationRepositoryTest`, `SocialAccountRepositoryTest`, `UserRefreshTokenServiceTest`,
//! `UserRefreshTokenConcurrencyTest`를 옮겼다.

use std::sync::mpsc::channel;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use chalkak_app::auth::domain::{RefreshTokenPolicy, SocialProvider};
use chalkak_app::auth::refresh::UserRefreshTokenService;
use chalkak_app::auth::repository::{
    AppleAuthorizationRepository, ConsumedSignupTokenRepository, NewRefreshToken,
    PendingAppleAuthorizationRepository, RefreshTokenRepository, SocialAccountRepository,
};
use chalkak_app::auth::tokens::{AccessTokenIssuer, hash_refresh_token};
use chalkak_app::testing::TestDb;
use chalkak_app::user::UserRepository;
use chalkak_app::user::domain::{SignatureStorageKeys, User};
use chalkak_app::web::ApiError;
use chalkak_core::FixedClock;
use chalkak_core::error::{ErrorCode, ErrorKind};
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

const NOW: &str = "2026-09-02T00:00:00Z";

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn now() -> Timestamp {
    ts(NOW)
}

fn plus(t: Timestamp, seconds: i64) -> Timestamp {
    t.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

fn days(n: i64) -> i64 {
    n * 86400
}

fn token_hash() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// `UserFixture.create()`.
fn save_user(db: &TestDb) -> User {
    let unique = Uuid::new_v4();
    let keys = SignatureStorageKeys {
        original: format!("signatures/original/{unique}"),
        thumbnail: format!("signatures/thumbnail/{unique}"),
    };
    let user = User::create(None, &keys).unwrap();
    UserRepository
        .insert(&mut db.client(), &user, now())
        .unwrap()
}

fn insert_token(
    db: &TestDb,
    user: &User,
    session_id: Uuid,
    token_hash: &str,
    expires_at: Timestamp,
    absolute_expires_at: Timestamp,
) {
    RefreshTokenRepository
        .insert(
            &mut db.client(),
            &NewRefreshToken {
                user_id: user.id,
                session_id,
                token_hash,
                expires_at,
                absolute_expires_at,
                created_at: now(),
            },
        )
        .unwrap();
}

fn create_refresh_token(db: &TestDb, user: &User, session_id: Uuid, hash: &str) {
    insert_token(
        db,
        user,
        session_id,
        hash,
        plus(now(), 3600),
        plus(now(), 7200),
    );
}

fn revoked_at(db: &TestDb, hash: &str) -> Option<Timestamp> {
    RefreshTokenRepository
        .find_by_token_hash(&mut db.client(), hash)
        .unwrap()
        .expect("토큰이 있다")
        .revoked_at
}

// ---------------------------------------------------------------------------------------------
// UserRefreshTokenRepositoryTest
// ---------------------------------------------------------------------------------------------

#[test]
fn find_by_token_hash_existing_token_returns_token() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    let hash = token_hash();
    create_refresh_token(&db, &user, Uuid::new_v4(), &hash);

    let found = RefreshTokenRepository
        .find_by_token_hash(&mut db.client(), &hash)
        .unwrap()
        .unwrap();

    assert_eq!(found.token_hash, hash);
    assert_eq!(found.user_id, user.id);
}

#[test]
fn find_by_token_hash_unknown_token_returns_empty() {
    let Some(db) = TestDb::create() else { return };
    assert!(
        RefreshTokenRepository
            .find_by_token_hash(&mut db.client(), &token_hash())
            .unwrap()
            .is_none()
    );
}

#[test]
fn revoke_session_live_session_revokes_only_that_session() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    let target = Uuid::new_v4();
    let other = Uuid::new_v4();
    let (rotated, live, other_hash) = (token_hash(), token_hash(), token_hash());
    create_refresh_token(&db, &user, target, &rotated);
    create_refresh_token(&db, &user, target, &live);
    create_refresh_token(&db, &user, other, &other_hash);

    let count = RefreshTokenRepository
        .revoke_session(&mut db.client(), target, now())
        .unwrap();

    assert_eq!(count, 2);
    assert_eq!(revoked_at(&db, &rotated), Some(now()));
    assert_eq!(revoked_at(&db, &live), Some(now()));
    assert_eq!(revoked_at(&db, &other_hash), None);
}

#[test]
fn revoke_session_already_revoked_token_is_not_counted() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    let session = Uuid::new_v4();
    let first = token_hash();
    create_refresh_token(&db, &user, session, &first);
    RefreshTokenRepository
        .revoke_session(&mut db.client(), session, plus(now(), -60))
        .unwrap();
    create_refresh_token(&db, &user, session, &token_hash());

    let count = RefreshTokenRepository
        .revoke_session(&mut db.client(), session, now())
        .unwrap();

    assert_eq!(count, 1);
    assert_eq!(revoked_at(&db, &first), Some(plus(now(), -60)));
}

#[test]
fn revoke_all_by_owner_id_multiple_sessions_revokes_only_that_owner() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    let other_user = save_user(&db);
    let (first, second, other) = (token_hash(), token_hash(), token_hash());
    create_refresh_token(&db, &user, Uuid::new_v4(), &first);
    create_refresh_token(&db, &user, Uuid::new_v4(), &second);
    create_refresh_token(&db, &other_user, Uuid::new_v4(), &other);

    let count = RefreshTokenRepository
        .revoke_all_by_user_id(&mut db.client(), user.id, now())
        .unwrap();

    assert_eq!(count, 2);
    assert_eq!(revoked_at(&db, &first), Some(now()));
    assert_eq!(revoked_at(&db, &second), Some(now()));
    assert_eq!(revoked_at(&db, &other), None);
}

// ---------------------------------------------------------------------------------------------
// ConsumedSignupTokenRepositoryTest
// ---------------------------------------------------------------------------------------------

fn future() -> Timestamp {
    plus(Timestamp::now(), 300)
}

#[test]
fn consume_if_absent_new_jti_returns_true() {
    let Some(db) = TestDb::create() else { return };
    let jti = Uuid::new_v4().to_string();
    assert!(
        ConsumedSignupTokenRepository
            .consume_if_absent(&mut db.client(), &jti, future())
            .unwrap()
    );
}

#[test]
fn consume_if_absent_already_consumed_jti_returns_false() {
    let Some(db) = TestDb::create() else { return };
    let jti = Uuid::new_v4().to_string();
    let mut client = db.client();
    ConsumedSignupTokenRepository
        .consume_if_absent(&mut client, &jti, future())
        .unwrap();
    assert!(
        !ConsumedSignupTokenRepository
            .consume_if_absent(&mut client, &jti, future())
            .unwrap()
    );
}

#[test]
fn consume_if_absent_already_consumed_jti_keeps_transaction_usable() {
    let Some(db) = TestDb::create() else { return };
    let jti = Uuid::new_v4().to_string();
    let mut client = db.client();
    let mut tx = client.transaction().unwrap();
    ConsumedSignupTokenRepository
        .consume_if_absent(&mut tx, &jti, future())
        .unwrap();
    assert!(
        !ConsumedSignupTokenRepository
            .consume_if_absent(&mut tx, &jti, future())
            .unwrap()
    );
    // 중복을 감지한 뒤에도 같은 트랜잭션에서 이어지는 쿼리를 실행할 수 있다.
    assert!(
        ConsumedSignupTokenRepository
            .consume_if_absent(&mut tx, &Uuid::new_v4().to_string(), future())
            .unwrap()
    );
    tx.commit().unwrap();
}

#[test]
fn consume_if_absent_already_consumed_jti_keeps_first_expires_at() {
    let Some(db) = TestDb::create() else { return };
    let jti = Uuid::new_v4().to_string();
    let first_expires_at = ts("2026-09-15T00:05:00Z");
    let mut client = db.client();
    ConsumedSignupTokenRepository
        .consume_if_absent(&mut client, &jti, first_expires_at)
        .unwrap();
    ConsumedSignupTokenRepository
        .consume_if_absent(&mut client, &jti, plus(first_expires_at, 86400))
        .unwrap();

    let stored: Timestamp = client
        .query_one(
            "SELECT expires_at FROM consumed_signup_tokens WHERE jti = $1",
            &[&jti],
        )
        .unwrap()
        .get(0);
    assert_eq!(stored, first_expires_at);
}

// ---------------------------------------------------------------------------------------------
// ConsumedSignupTokenRepositoryConcurrencyTest
// ---------------------------------------------------------------------------------------------

const CONCURRENT_JTI: &str = "0198fd30-0000-7000-8000-00000000413a";

/// 첫 트랜잭션이 jti를 기록한 채 열려 있는 동안 두 번째 트랜잭션이 같은 jti를 기록하게 한다. 두 번째가 첫 트랜잭션의
/// 종료를 기다리는지 확인한 뒤 첫 트랜잭션을 끝낸다.
fn consume_while_first_transaction_open(db: &TestDb, rollback_first: bool) -> (bool, bool) {
    let (consumed_tx, consumed_rx) = channel::<()>();
    let (finish_tx, finish_rx) = channel::<()>();
    let (second_tx, second_rx) = channel::<bool>();

    let mut first_client = db.client();
    let first = std::thread::spawn(move || {
        let mut tx = first_client.transaction().unwrap();
        let consumed = ConsumedSignupTokenRepository
            .consume_if_absent(&mut tx, CONCURRENT_JTI, future())
            .unwrap();
        consumed_tx.send(()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if rollback_first {
            tx.rollback().unwrap();
        } else {
            tx.commit().unwrap();
        }
        consumed
    });
    consumed_rx.recv_timeout(Duration::from_secs(10)).unwrap();

    let mut second_client = db.client();
    let second = std::thread::spawn(move || {
        let mut tx = second_client.transaction().unwrap();
        let consumed = ConsumedSignupTokenRepository
            .consume_if_absent(&mut tx, CONCURRENT_JTI, future())
            .unwrap();
        tx.commit().unwrap();
        second_tx.send(consumed).unwrap();
        consumed
    });

    // 첫 트랜잭션이 열려 있는 동안 두 번째는 끝나지 않는다.
    assert!(second_rx.recv_timeout(Duration::from_millis(300)).is_err());
    finish_tx.send(()).unwrap();
    (first.join().unwrap(), second.join().unwrap())
}

fn count_tokens(db: &TestDb) -> i64 {
    db.client()
        .query_one(
            "SELECT COUNT(*) FROM consumed_signup_tokens WHERE jti = $1",
            &[&CONCURRENT_JTI],
        )
        .unwrap()
        .get(0)
}

#[test]
fn consume_if_absent_concurrent_same_jti_after_commit_returns_false_without_exception() {
    let Some(db) = TestDb::create() else { return };
    let (first, second) = consume_while_first_transaction_open(&db, false);
    assert!(first);
    assert!(!second);
    assert_eq!(count_tokens(&db), 1);
}

#[test]
fn consume_if_absent_concurrent_same_jti_after_rollback_returns_true() {
    let Some(db) = TestDb::create() else { return };
    let (first, second) = consume_while_first_transaction_open(&db, true);
    assert!(first);
    assert!(second);
    assert_eq!(count_tokens(&db), 1);
}

// ---------------------------------------------------------------------------------------------
// PendingAppleAuthorizationRepositoryTest
// ---------------------------------------------------------------------------------------------

fn subject_a() -> String {
    "a".repeat(64)
}

fn subject_b() -> String {
    "b".repeat(64)
}

fn pending_expires_at() -> Timestamp {
    ts("2026-09-04T00:05:00Z")
}

fn save_pending(
    db: &TestDb,
    subject: &str,
    token: &str,
    expires_at: Timestamp,
    created_at: Timestamp,
) {
    PendingAppleAuthorizationRepository
        .insert(&mut db.client(), subject, token, expires_at, created_at)
        .unwrap();
}

fn latest(db: &TestDb, subject: &str, at: Timestamp) -> Option<String> {
    let mut client = db.client();
    let mut tx = client.transaction().unwrap();
    let found = PendingAppleAuthorizationRepository
        .find_latest_unexpired_for_update(&mut tx, subject, at)
        .unwrap()
        .map(|p| p.encrypted_refresh_token);
    tx.commit().unwrap();
    found
}

#[test]
fn find_latest_unexpired_by_subject_hmac_for_update_multiple_rows_returns_latest() {
    let Some(db) = TestDb::create() else { return };
    // 재로그인으로 행이 쌓여도 정식 보관으로 옮길 대상은 마지막 교환 결과 하나뿐이다.
    let base = ts("2026-09-04T00:00:00Z");
    save_pending(&db, &subject_a(), "first-token", pending_expires_at(), base);
    save_pending(
        &db,
        &subject_a(),
        "second-token",
        plus(pending_expires_at(), 600),
        plus(base, 1),
    );
    save_pending(
        &db,
        &subject_a(),
        "third-token",
        pending_expires_at(),
        plus(base, 2),
    );

    let before_expiry = plus(pending_expires_at(), -60);
    assert_eq!(
        latest(&db, &subject_a(), before_expiry).as_deref(),
        Some("third-token")
    );
}

#[test]
fn find_latest_unexpired_by_subject_hmac_for_update_at_expiry_boundary_excludes_expired() {
    let Some(db) = TestDb::create() else { return };
    save_pending(
        &db,
        &subject_a(),
        "boundary-token",
        pending_expires_at(),
        now(),
    );

    let one_ms = SignedDuration::from_millis(1);
    assert!(
        latest(
            &db,
            &subject_a(),
            pending_expires_at().checked_sub(one_ms).unwrap()
        )
        .is_some()
    );
    assert!(latest(&db, &subject_a(), pending_expires_at()).is_none());
    assert!(
        latest(
            &db,
            &subject_a(),
            pending_expires_at().checked_add(one_ms).unwrap()
        )
        .is_none()
    );
}

#[test]
fn find_latest_unexpired_by_subject_hmac_for_update_other_subject_hmac_returns_empty() {
    let Some(db) = TestDb::create() else { return };
    save_pending(
        &db,
        &subject_b(),
        "other-token",
        pending_expires_at(),
        now(),
    );
    assert!(latest(&db, &subject_a(), plus(pending_expires_at(), -60)).is_none());
}

// ---------------------------------------------------------------------------------------------
// SocialAccountRepositoryTest, AppleAuthorizationRepositoryTest
// ---------------------------------------------------------------------------------------------

const GOOGLE_HMAC: &str = "921c5d35312df654eaa8ec114fd1de5a156cbcc64b23ddb6a709a9423f90c218";
const KAKAO_HMAC: &str = "73bce40e3e8b39018d68b14b38454b28892c1b50c93b143ce36b5406949542ba";

#[test]
fn find_by_provider_and_subject_hmac_existing_account_loads_user() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    SocialAccountRepository
        .insert(
            &mut db.client(),
            user.id,
            SocialProvider::Google,
            GOOGLE_HMAC,
            now(),
        )
        .unwrap();

    let (account, found_user) = SocialAccountRepository
        .find_by_provider_and_subject_hmac(&mut db.client(), SocialProvider::Google, GOOGLE_HMAC)
        .unwrap()
        .unwrap();

    assert_eq!(account.subject_hmac, GOOGLE_HMAC);
    assert_eq!(found_user.id, user.id);
}

#[test]
fn save_duplicate_provider_and_subject_hmac_throws_exception() {
    let Some(db) = TestDb::create() else { return };
    let first = save_user(&db);
    let second = save_user(&db);
    SocialAccountRepository
        .insert(
            &mut db.client(),
            first.id,
            SocialProvider::Google,
            GOOGLE_HMAC,
            now(),
        )
        .unwrap();

    let result = SocialAccountRepository.insert(
        &mut db.client(),
        second.id,
        SocialProvider::Google,
        GOOGLE_HMAC,
        now(),
    );

    assert!(result.is_err());
}

#[test]
fn save_duplicate_user_throws_exception() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    SocialAccountRepository
        .insert(
            &mut db.client(),
            user.id,
            SocialProvider::Google,
            GOOGLE_HMAC,
            now(),
        )
        .unwrap();

    let result = SocialAccountRepository.insert(
        &mut db.client(),
        user.id,
        SocialProvider::Kakao,
        KAKAO_HMAC,
        now(),
    );

    assert!(result.is_err());
}

#[test]
fn save_duplicate_social_account_throws_exception() {
    let Some(db) = TestDb::create() else { return };
    let user = save_user(&db);
    let account_id = SocialAccountRepository
        .insert(
            &mut db.client(),
            user.id,
            SocialProvider::Apple,
            GOOGLE_HMAC,
            now(),
        )
        .unwrap();
    AppleAuthorizationRepository
        .insert(
            &mut db.client(),
            account_id,
            "encrypted-refresh-token",
            now(),
        )
        .unwrap();

    let result = AppleAuthorizationRepository.insert(
        &mut db.client(),
        account_id,
        "another-encrypted-refresh-token",
        now(),
    );

    assert!(result.is_err());
}

// ---------------------------------------------------------------------------------------------
// UserRefreshTokenServiceTest
// ---------------------------------------------------------------------------------------------

struct Fixture {
    db: TestDb,
    clock: Arc<FixedClock>,
    service: UserRefreshTokenService,
}

fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let clock = Arc::new(FixedClock::new(now()));
    let access = AccessTokenIssuer::new(
        vec![7; 32],
        "chalkak-access",
        Duration::from_secs(900),
        clock.clone(),
    );
    let service = UserRefreshTokenService::new(
        db.pool(8),
        clock.clone(),
        RefreshTokenPolicy::new(
            Duration::from_secs(30 * 86400),
            Duration::from_secs(90 * 86400),
        )
        .unwrap(),
        access,
    );
    Some(Fixture { db, clock, service })
}

#[derive(Debug)]
struct TokenRow {
    session_id: Uuid,
    rotated_at: Option<Timestamp>,
    revoked_at: Option<Timestamp>,
    expires_at: Timestamp,
    absolute_expires_at: Timestamp,
}

impl Fixture {
    fn issue(&self, user: &User) -> String {
        self.service
            .issue(&mut self.db.client(), user.id)
            .unwrap()
            .value
    }

    fn token_row(&self, token: &str) -> TokenRow {
        let row = self
            .db
            .client()
            .query_one(
                "SELECT session_id, rotated_at, revoked_at, expires_at, absolute_expires_at \
                 FROM user_refresh_tokens WHERE token_hash = $1",
                &[&hash_refresh_token(token)],
            )
            .unwrap();
        TokenRow {
            session_id: row.get("session_id"),
            rotated_at: row.get("rotated_at"),
            revoked_at: row.get("revoked_at"),
            expires_at: row.get("expires_at"),
            absolute_expires_at: row.get("absolute_expires_at"),
        }
    }

    fn count_live_tokens(&self, session_id: Uuid) -> i64 {
        self.db
            .client()
            .query_one(
                "SELECT COUNT(*) FROM user_refresh_tokens \
                 WHERE session_id = $1 AND revoked_at IS NULL",
                &[&session_id],
            )
            .unwrap()
            .get(0)
    }

    fn refresh_at(&self, presented: &str, at: Timestamp) -> String {
        self.clock.set(at);
        self.service.refresh(presented).unwrap().refresh_token.value
    }

    /// 절대 만료 직전까지 비활동 만료를 갱신하며 회전시켜, 마지막 토큰을 돌려준다.
    fn rotate_until_absolute_expiry(&self, first: &str) -> String {
        let mut token = self.refresh_at(first, plus(now(), days(29)));
        token = self.refresh_at(&token, plus(now(), days(58)));
        self.refresh_at(&token, plus(now(), days(87)))
    }

    fn assert_reauthentication_required(&self, presented: &str) {
        match self.service.refresh(presented).unwrap_err() {
            ApiError::App(e) => {
                assert_eq!(e.kind, ErrorKind::Unauthorized);
                assert_eq!(e.code, ErrorCode::REAUTHENTICATION_REQUIRED);
                assert_eq!(e.message, "다시 로그인해 주세요.");
            }
            other => panic!("재로그인 요구가 아니다: {other:?}"),
        }
    }
}

#[test]
fn issue_new_user_stores_hash_only_and_returns_plain_token() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);

    let issued = f.service.issue(&mut f.db.client(), user.id).unwrap();

    let stored: String =
        f.db.client()
            .query_one(
                "SELECT token_hash FROM user_refresh_tokens WHERE user_id = $1",
                &[&user.id],
            )
            .unwrap()
            .get(0);
    assert!(!issued.value.trim().is_empty());
    assert_ne!(stored, issued.value);
    assert_eq!(stored.len(), 64);
    assert!(
        stored
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_eq!(issued.expires_in_seconds, days(30));
}

#[test]
fn refresh_live_token_rotates_consumed_token_and_reissues_access_token() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);

    let result = f.service.refresh(&issued).unwrap();

    let consumed = f.token_row(&issued);
    let successor = f.token_row(&result.refresh_token.value);
    assert_ne!(result.refresh_token.value, issued);
    assert!(!result.access_token.value.trim().is_empty());
    assert_eq!(consumed.rotated_at, Some(now()));
    assert_eq!(consumed.revoked_at, None);
    assert_eq!(successor.rotated_at, None);
    assert_eq!(successor.session_id, consumed.session_id);
    assert_eq!(successor.absolute_expires_at, consumed.absolute_expires_at);
}

#[test]
fn refresh_rotated_token_revokes_whole_lineage() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);
    f.refresh_at(&issued, now());
    let session_id = f.token_row(&issued).session_id;
    let reused_at = plus(now(), 1);
    f.clock.set(reused_at);

    f.assert_reauthentication_required(&issued);

    assert_eq!(f.count_live_tokens(session_id), 0);
    assert_eq!(f.token_row(&issued).revoked_at, Some(reused_at));
}

#[test]
fn refresh_reused_lineage_revoked_keeps_other_device_lineage_usable() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let first_device = f.issue(&user);
    let second_device = f.issue(&user);
    f.refresh_at(&first_device, now());
    f.clock.set(plus(now(), 1));
    f.assert_reauthentication_required(&first_device);

    let result = f.service.refresh(&second_device).unwrap();

    assert!(!result.refresh_token.value.trim().is_empty());
    assert_eq!(f.token_row(&second_device).revoked_at, None);
}

#[test]
fn refresh_inactivity_expired_token_revokes_lineage_and_requires_reauthentication() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);
    let stored = f.token_row(&issued);
    let inactivity_expired_at = plus(now(), days(30));
    f.clock.set(inactivity_expired_at);

    assert!(stored.absolute_expires_at > inactivity_expired_at);
    f.assert_reauthentication_required(&issued);
    assert_eq!(f.count_live_tokens(stored.session_id), 0);
}

#[test]
fn refresh_absolute_expired_token_revokes_lineage_and_requires_reauthentication() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);
    let last = f.rotate_until_absolute_expiry(&issued);
    let stored = f.token_row(&last);
    f.clock.set(stored.absolute_expires_at);

    f.assert_reauthentication_required(&last);

    assert_eq!(f.count_live_tokens(stored.session_id), 0);
}

#[test]
fn refresh_rotation_near_absolute_expiry_clips_expires_at_to_absolute_expiry() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);

    let last = f.rotate_until_absolute_expiry(&issued);

    let stored = f.token_row(&last);
    assert_eq!(stored.expires_at, stored.absolute_expires_at);
    assert_eq!(stored.absolute_expires_at, plus(now(), days(90)));
}

#[test]
fn refresh_unknown_token_requires_reauthentication() {
    let Some(f) = fixture() else { return };
    f.assert_reauthentication_required("unknown-refresh-token");
}

#[test]
fn refresh_revoked_token_keeps_revoked_at_and_requires_reauthentication() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);
    f.refresh_at(&issued, now());
    f.clock.set(plus(now(), 1));
    f.assert_reauthentication_required(&issued);
    f.clock.set(plus(now(), 30));

    f.assert_reauthentication_required(&issued);

    assert_eq!(f.token_row(&issued).revoked_at, Some(plus(now(), 1)));
}

#[test]
fn refresh_banned_user_still_rotates_token() {
    let Some(f) = fixture() else { return };
    let mut user = save_user(&f.db);
    let issued = f.issue(&user);
    let before = user.clone();
    user.ban().unwrap();
    UserRepository
        .update_if_changed(&mut f.db.client(), &before, &user, now())
        .unwrap();

    let result = f.service.refresh(&issued).unwrap();

    assert_ne!(result.refresh_token.value, issued);
    assert!(!result.access_token.value.trim().is_empty());
}

#[test]
fn logout_live_token_revokes_only_that_lineage() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let logged_out = f.issue(&user);
    let other = f.issue(&user);

    f.service.logout(&logged_out).unwrap();

    f.assert_reauthentication_required(&logged_out);
    assert_eq!(f.count_live_tokens(f.token_row(&logged_out).session_id), 0);
    assert_eq!(f.token_row(&other).revoked_at, None);
}

#[test]
fn logout_unknown_or_revoked_token_does_nothing() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let issued = f.issue(&user);
    f.service.logout(&issued).unwrap();

    f.service.logout(&issued).unwrap();
    f.service.logout("unknown-refresh-token").unwrap();
}

#[test]
fn revoke_all_multiple_devices_revokes_every_lineage() {
    let Some(f) = fixture() else { return };
    let user = save_user(&f.db);
    let first = f.issue(&user);
    let second = f.issue(&user);

    f.service.revoke_all(&mut f.db.client(), user.id).unwrap();

    assert_eq!(f.token_row(&first).revoked_at, Some(now()));
    assert_eq!(f.token_row(&second).revoked_at, Some(now()));
}

// ---------------------------------------------------------------------------------------------
// UserRefreshTokenConcurrencyTest
// ---------------------------------------------------------------------------------------------

const SUCCESS: &str = "SUCCESS";

/// 장벽에서 출발해 서비스를 한 번 부르는 동작.
type RefreshAction = Box<dyn FnOnce(&UserRefreshTokenService) -> Result<(), ApiError> + Send>;
const PRESENTED_TOKEN: &str = "concurrent-refresh-token";
const OTHER_TOKEN: &str = "concurrent-refresh-token-sibling";

struct Concurrent {
    f: Fixture,
    user: User,
    session_id: Uuid,
}

fn concurrent() -> Option<Concurrent> {
    let f = fixture()?;
    let user = save_user(&f.db);
    Some(Concurrent {
        f,
        user,
        session_id: Uuid::new_v4(),
    })
}

impl Concurrent {
    fn insert_token(&self, token: &str, rotated_at: Option<Timestamp>) {
        let hash = hash_refresh_token(token);
        insert_token(
            &self.f.db,
            &self.user,
            self.session_id,
            &hash,
            plus(now(), days(30)),
            plus(now(), days(90)),
        );
        if let Some(rotated_at) = rotated_at {
            RefreshTokenRepository
                .mark_rotated(
                    &mut self.f.db.client(),
                    RefreshTokenRepository
                        .find_by_token_hash(&mut self.f.db.client(), &hash)
                        .unwrap()
                        .unwrap()
                        .id,
                    rotated_at,
                )
                .unwrap();
        }
    }

    fn count(&self, live: bool) -> i64 {
        let sql = if live {
            "SELECT COUNT(*) FROM user_refresh_tokens WHERE session_id = $1 AND revoked_at IS NULL"
        } else {
            "SELECT COUNT(*) FROM user_refresh_tokens WHERE session_id = $1 AND revoked_at IS NOT NULL"
        };
        self.f
            .db
            .client()
            .query_one(sql, &[&self.session_id])
            .unwrap()
            .get(0)
    }

    /// 두 동작을 장벽에서 동시에 출발시키고 각 결과(`SUCCESS`, 오류 코드 이름, `UNEXPECTED:...`)를 모은다.
    fn run(
        &self,
        first: impl FnOnce(&UserRefreshTokenService) -> Result<(), ApiError> + Send + 'static,
        second: impl FnOnce(&UserRefreshTokenService) -> Result<(), ApiError> + Send + 'static,
    ) -> Vec<String> {
        let barrier = Arc::new(Barrier::new(2));
        let spawn = |action: RefreshAction| {
            let barrier = barrier.clone();
            let service = self.f.service.clone();
            std::thread::spawn(move || {
                barrier.wait();
                match action(&service) {
                    Ok(()) => SUCCESS.to_owned(),
                    Err(ApiError::App(e)) => e.code.as_str().to_owned(),
                    Err(other) => format!("UNEXPECTED:{other:?}"),
                }
            })
        };
        let a = spawn(Box::new(first));
        let b = spawn(Box::new(second));
        vec![a.join().unwrap(), b.join().unwrap()]
    }
}

fn refreshing(
    token: &'static str,
) -> impl FnOnce(&UserRefreshTokenService) -> Result<(), ApiError> + Send + 'static {
    move |service| service.refresh(token).map(|_| ())
}

fn logging_out(
    token: &'static str,
) -> impl FnOnce(&UserRefreshTokenService) -> Result<(), ApiError> + Send + 'static {
    move |service| service.logout(token)
}

#[test]
fn refresh_concurrent_rotation_of_same_token_succeeds_for_one_caller_and_revokes_lineage() {
    let Some(c) = concurrent() else { return };
    c.insert_token(PRESENTED_TOKEN, None);

    let mut outcomes = c.run(refreshing(PRESENTED_TOKEN), refreshing(PRESENTED_TOKEN));

    // 뒤늦게 잠금을 얻은 쪽은 이미 회전된 토큰을 제시한 셈이라 탈취로 판정된다.
    outcomes.sort();
    let mut expected = vec![
        SUCCESS.to_owned(),
        ErrorCode::REAUTHENTICATION_REQUIRED.as_str().to_owned(),
    ];
    expected.sort();
    assert_eq!(outcomes, expected);
    assert_eq!(c.count(true), 0);
    assert_eq!(c.count(false), 2);
}

#[test]
fn refresh_concurrent_reuse_of_rotated_token_revokes_lineage_for_both_callers() {
    let Some(c) = concurrent() else { return };
    c.insert_token(PRESENTED_TOKEN, Some(plus(now(), -60)));
    c.insert_token("successor-refresh-token", None);

    let outcomes = c.run(refreshing(PRESENTED_TOKEN), refreshing(PRESENTED_TOKEN));

    let reauth = ErrorCode::REAUTHENTICATION_REQUIRED.as_str().to_owned();
    assert_eq!(outcomes, vec![reauth.clone(), reauth]);
    assert_eq!(c.count(true), 0);
    assert_eq!(c.count(false), 2);
}

#[test]
fn refresh_and_logout_different_tokens_of_same_lineage_leaves_no_live_token() {
    let Some(c) = concurrent() else { return };
    c.insert_token(PRESENTED_TOKEN, None);
    c.insert_token(OTHER_TOKEN, None);

    let outcomes = c.run(refreshing(PRESENTED_TOKEN), logging_out(OTHER_TOKEN));

    // 로그아웃이 먼저면 재발급은 폐기된 토큰을 만나 거절되고, 재발급이 먼저면 후속 토큰까지 로그아웃이 함께 끊는다.
    assert!(
        outcomes.iter().all(|o| !o.starts_with("UNEXPECTED")),
        "{outcomes:?}"
    );
    assert_eq!(c.count(true), 0);
}

#[test]
fn refresh_reuse_detection_races_rotation_revokes_whole_lineage() {
    let Some(c) = concurrent() else { return };
    c.insert_token(PRESENTED_TOKEN, Some(plus(now(), -60)));
    c.insert_token(OTHER_TOKEN, None);

    let outcomes = c.run(refreshing(PRESENTED_TOKEN), refreshing(OTHER_TOKEN));

    // 회전이 먼저 끝나 후속 토큰이 생기더라도, 뒤이은 재사용 탐지가 그 후속 토큰까지 끊는다.
    assert!(
        outcomes.iter().all(|o| !o.starts_with("UNEXPECTED")),
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes[0],
        ErrorCode::REAUTHENTICATION_REQUIRED.as_str().to_owned()
    );
    assert_eq!(c.count(true), 0);
}
