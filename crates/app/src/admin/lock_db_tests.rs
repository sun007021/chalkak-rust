//! 잠금 강도 시험: 원본(Hibernate PostgreSQL 방언)과 같은 행·같은 종류만 잠그는지 확인한다.
//!
//! 원본 `@Lock(PESSIMISTIC_WRITE)`는 `for no key update of <별칭>`, `PESSIMISTIC_READ`는 `for share of <별칭>`으로 나가고,
//! 사진을 `JOIN FETCH`하는 조회도 게시물 행만 잠근다(#36 트랜잭션 모양 비교의 Java 문장 로그). 더 세게 잠그면 데이터는
//! 맞지만 원본에서 기다리지 않던 요청이 기다린다. 예: `FOR UPDATE`는 외래 키 검사가 잡는 `FOR KEY SHARE`와 충돌해서,
//! 회원 행을 잠근 트랜잭션이 끝날 때까지 그 회원을 참조하는 INSERT(좋아요·게시물·알림)가 막힌다.
//!
//! 방법: 연결 A가 트랜잭션 안에서 저장소의 잠금 조회를 부르고, 연결 B가 `lock_timeout`을 짧게 두고 원본에서는 막히지 않는
//! 문장을 보낸다. B가 시간 안에 끝나야 한다.

use jiff::Timestamp;
use postgres::Client;
use uuid::Uuid;

use super::post::repository::AdminPostRepository;
use super::testing::{insert_post, insert_topic, insert_user};
use super::user::repository::AdminUserRepository;
use crate::auth::repository::PendingAppleAuthorizationRepository;
use crate::post::repository::PostRepository;
use crate::testing::TestDb;
use crate::user::UserRepository;

/// A가 `hold`로 잠근 채로 B가 `probe`를 보낸다. B가 잠금 대기로 실패하면 그 오류를 돌려준다.
fn probe_while_locked(
    db: &TestDb,
    hold: impl FnOnce(&mut postgres::Transaction),
    probe: &str,
    params: &[&(dyn postgres::types::ToSql + Sync)],
) -> Result<(), postgres::Error> {
    let mut a = db.client();
    let mut tx = a.transaction().unwrap();
    hold(&mut tx);
    let mut b = db.client();
    b.batch_execute("SET lock_timeout = '500ms'").unwrap();
    let result = b.execute(probe, params).map(|_| ());
    tx.rollback().unwrap();
    result
}

/// 외래 키 검사가 부모 행에 잡는 잠금(`INSERT INTO post_likes ...`가 `users`에 잡는 것과 같다).
const FK_CHECK: &str = "SELECT 1 FROM users WHERE id = $1 FOR KEY SHARE";

struct Fixture {
    user: Uuid,
    post: Uuid,
    photo: Uuid,
}

fn fixture(client: &mut Client, status: &str) -> Fixture {
    let user = insert_user(client, "lock@example.com", "ACTIVE");
    let topic = insert_topic(
        client,
        "t",
        "2026-10-12",
        "2026-10-12T00:00:00Z",
        "2026-10-12T14:59:00Z",
    );
    let post = insert_post(client, user, topic, status);
    let photo = client
        .query_one("SELECT photo_id FROM posts WHERE id = $1", &[&post])
        .unwrap()
        .get(0);
    Fixture { user, post, photo }
}

const PHOTO_UPDATE: &str = "UPDATE photos SET metadata = metadata WHERE id = $1";

#[test]
fn 회원_잠금은_외래_키_검사를_막지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let f = fixture(&mut db.client(), "APPROVED");
    let held = probe_while_locked(
        &db,
        |tx| {
            UserRepository
                .find_active_by_id_for_update(tx, f.user)
                .unwrap()
                .unwrap();
        },
        FK_CHECK,
        &[&f.user],
    );
    assert!(held.is_ok(), "{held:?}");
}

#[test]
fn 관리자_회원_잠금은_외래_키_검사를_막지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let f = fixture(&mut db.client(), "APPROVED");
    let held = probe_while_locked(
        &db,
        |tx| {
            AdminUserRepository
                .find_by_id_for_update(tx, f.user)
                .unwrap()
                .unwrap();
        },
        FK_CHECK,
        &[&f.user],
    );
    assert!(held.is_ok(), "{held:?}");
}

#[test]
fn 대기_중인_애플_인증_잠금은_키_공유_잠금을_막지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let hmac = "a".repeat(64);
    let id: Uuid = db
        .client()
        .query_one(
            "INSERT INTO pending_apple_authorizations (subject_hmac, encrypted_refresh_token, expires_at) \
             VALUES ($1, 'enc', now() + interval '1 hour') RETURNING id",
            &[&hmac],
        )
        .unwrap()
        .get(0);
    let held = probe_while_locked(
        &db,
        |tx| {
            PendingAppleAuthorizationRepository
                .find_latest_unexpired_for_update(tx, &hmac, Timestamp::now())
                .unwrap()
                .unwrap();
        },
        "SELECT 1 FROM pending_apple_authorizations WHERE id = $1 FOR KEY SHARE",
        &[&id],
    );
    assert!(held.is_ok(), "{held:?}");
}

#[test]
fn 게시물_수정_잠금은_사진_행을_잠그지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let f = fixture(&mut db.client(), "APPROVED");
    let held = probe_while_locked(
        &db,
        |tx| {
            PostRepository::default()
                .find_by_id_for_update(tx, f.post)
                .unwrap()
                .unwrap();
        },
        PHOTO_UPDATE,
        &[&f.photo],
    );
    assert!(held.is_ok(), "{held:?}");
}

#[test]
fn 좋아요의_게시물_공유_잠금은_사진_행을_잠그지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let f = fixture(&mut db.client(), "APPROVED");
    let held = probe_while_locked(
        &db,
        |tx| {
            PostRepository::default()
                .find_visible_by_id(tx, f.post, true)
                .unwrap()
                .unwrap();
        },
        PHOTO_UPDATE,
        &[&f.photo],
    );
    assert!(held.is_ok(), "{held:?}");
}

#[test]
fn 관리자_게시물_잠금은_사진_행을_잠그지_않는다() {
    let Some(db) = TestDb::create() else { return };
    let f = fixture(&mut db.client(), "PENDING");
    let held = probe_while_locked(
        &db,
        |tx| {
            AdminPostRepository
                .find_for_update(tx, f.post, false)
                .unwrap()
                .unwrap();
        },
        PHOTO_UPDATE,
        &[&f.photo],
    );
    assert!(held.is_ok(), "{held:?}");
}

#[test]
fn 잠금은_여전히_같은_행의_수정끼리는_막는다() {
    let Some(db) = TestDb::create() else { return };
    let f = fixture(&mut db.client(), "APPROVED");
    let held = probe_while_locked(
        &db,
        |tx| {
            UserRepository
                .find_active_by_id_for_update(tx, f.user)
                .unwrap()
                .unwrap();
        },
        "SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE",
        &[&f.user],
    );
    assert!(
        held.is_err(),
        "같은 회원 행의 쓰기 잠금끼리는 기다려야 한다"
    );
}
