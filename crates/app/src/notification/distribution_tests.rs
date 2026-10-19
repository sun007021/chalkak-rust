//! 공지 배포 테스트. 원본 `NoticeDistributionServiceTest`, `NoticeRepositoryImplTest`에 대응한다.
//! 시계는 고정 시계를 써서 "지금"을 정한다. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

use std::sync::{Arc, Barrier};
use std::thread;

use chalkak_core::FixedClock;
use chalkak_core::log::{Level, Logger};
use chalkak_db::Pool;
use jiff::{SignedDuration, Timestamp};
use postgres::Client;
use uuid::Uuid;

use super::distribution::distribute_notices;
use super::notice::{DistributableNotice, NoticeRepository};
use crate::testing::TestDb;

const NOW: &str = "2026-10-12T01:00:00Z";

fn now() -> Timestamp {
    NOW.parse().unwrap()
}

fn minus(seconds: i64) -> Timestamp {
    now()
        .checked_sub(SignedDuration::from_secs(seconds))
        .unwrap()
}

fn plus(seconds: i64) -> Timestamp {
    now()
        .checked_add(SignedDuration::from_secs(seconds))
        .unwrap()
}

struct Fixture {
    db: TestDb,
    pool: Pool,
    clock: Arc<FixedClock>,
    logger: Logger,
    active: Uuid,
    banned: Uuid,
}

fn fixture() -> Option<Fixture> {
    let db = TestDb::create()?;
    let clock = Arc::new(FixedClock::new(now()));
    let logger = Logger::new(Box::new(std::io::sink()), Level::Info, clock.clone());
    let pool = db.pool(4);
    let mut client = db.client();
    let active = insert_user(&mut client, "ACTIVE", false);
    let banned = insert_user(&mut client, "BANNED", false);
    insert_user(&mut client, "ACTIVE", true);
    drop(client);
    Some(Fixture {
        db,
        pool,
        clock,
        logger,
        active,
        banned,
    })
}

impl Fixture {
    fn run(&self) {
        distribute_notices(&self.pool, &*self.clock, &self.logger).unwrap();
    }

    fn client(&self) -> Client {
        self.db.client()
    }
}

fn insert_user(client: &mut Client, status: &str, withdrawn: bool) -> Uuid {
    let id = Uuid::new_v4();
    let key = format!("{id}/signature.png");
    client
        .execute(
            "INSERT INTO users (id, email, status, signature_original_storage_key, deleted_at) \
             VALUES ($1, $2, $3::text::user_status, $4, CASE WHEN $5 THEN now() END)",
            &[&id, &format!("{id}@notice.test"), &status, &key, &withdrawn],
        )
        .unwrap();
    id
}

fn insert_notice_at(client: &mut Client, scheduled_at: Timestamp, distributed: bool) -> Uuid {
    let distributed_at = distributed.then_some(scheduled_at);
    client
        .query_one(
            "INSERT INTO notices (title, body, scheduled_at, distributed_at, created_at, updated_at) \
             VALUES ('서비스 점검 안내', '점검합니다.', $1, $2, now(), now()) RETURNING id",
            &[&scheduled_at, &distributed_at],
        )
        .unwrap()
        .get(0)
}

fn insert_notice(client: &mut Client, scheduled_at: Timestamp) -> Uuid {
    insert_notice_at(client, scheduled_at, false)
}

/// 배포가 실패하도록 같은 공지·회원의 알림을 미리 넣는다(알림의 고유 제약 위반).
fn insert_conflicting_notification(client: &mut Client, user: Uuid, notice: Uuid) {
    client
        .execute(
            "INSERT INTO notifications (user_id, source_type, source_id, event_key, type, title, \
             body, created_at, sqs_publish_status) \
             VALUES ($1, 'NOTICE', $2, $2, 'NOTICE', '공지', '본문', now(), 'NOT_REQUIRED')",
            &[&user, &notice],
        )
        .unwrap();
}

fn count_notifications(client: &mut Client) -> i64 {
    client
        .query_one("SELECT COUNT(*) FROM notifications", &[])
        .unwrap()
        .get(0)
}

fn distributed_at(client: &mut Client, notice: Uuid) -> Option<Timestamp> {
    client
        .query_one(
            "SELECT distributed_at FROM notices WHERE id = $1",
            &[&notice],
        )
        .unwrap()
        .get(0)
}

fn count_undistributed(client: &mut Client) -> i64 {
    client
        .query_one(
            "SELECT COUNT(*) FROM notices WHERE distributed_at IS NULL",
            &[],
        )
        .unwrap()
        .get(0)
}

// ---------------------------------------------------------------------------------------------
// NoticeDistributionServiceTest
// ---------------------------------------------------------------------------------------------

#[test]
fn 예약_시각이_지난_공지를_탈퇴하지_않은_회원_전체의_알림함에_발행_대기로_배포한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let notice = insert_notice(&mut client, minus(60));

    f.run();

    let rows = client
        .query(
            "SELECT user_id, source_type, source_id, event_key, type, title, body, \
             sqs_publish_status, created_at, next_attempt_at FROM notifications",
            &[],
        )
        .unwrap();
    let mut users: Vec<Uuid> = rows.iter().map(|r| r.get("user_id")).collect();
    users.sort();
    let mut expected = vec![f.active, f.banned];
    expected.sort();
    assert_eq!(users, expected);
    for row in &rows {
        assert_eq!(row.get::<_, String>("source_type"), "NOTICE");
        assert_eq!(row.get::<_, String>("type"), "NOTICE");
        assert_eq!(row.get::<_, Uuid>("source_id"), notice);
        assert_eq!(row.get::<_, Uuid>("event_key"), notice);
        assert_eq!(row.get::<_, String>("title"), "서비스 점검 안내");
        assert_eq!(row.get::<_, String>("body"), "점검합니다.");
        assert_eq!(row.get::<_, String>("sqs_publish_status"), "PENDING");
        assert_eq!(row.get::<_, Timestamp>("created_at"), now());
        assert_eq!(
            row.get::<_, Option<Timestamp>>("next_attempt_at"),
            Some(now())
        );
    }
    assert_eq!(distributed_at(&mut client, notice), Some(now()));
}

#[test]
fn 예약_시각_전인_공지는_배포하지_않는다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let notice = insert_notice(&mut client, plus(3600));
    f.run();
    assert_eq!(count_notifications(&mut client), 0);
    assert_eq!(distributed_at(&mut client, notice), None);
}

#[test]
fn 예약_시각이_지금과_같은_공지는_배포한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let notice = insert_notice(&mut client, now());
    f.run();
    assert_eq!(count_notifications(&mut client), 2);
    assert!(distributed_at(&mut client, notice).is_some());
}

#[test]
fn 배포를_다시_실행해도_이미_배포한_공지의_알림을_추가로_만들지_않는다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    insert_notice(&mut client, minus(60));
    f.run();
    f.run();
    assert_eq!(count_notifications(&mut client), 2);
}

#[test]
fn 한_공지의_배포가_실패하면_그_공지는_배포되지_않은_채_남고_다른_공지는_배포한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let failing = insert_notice(&mut client, minus(120));
    let other = insert_notice(&mut client, minus(60));
    insert_conflicting_notification(&mut client, f.active, failing);

    f.run();

    assert_eq!(distributed_at(&mut client, failing), None);
    assert!(distributed_at(&mut client, other).is_some());
    let other_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM notifications WHERE source_id = $1",
            &[&other],
        )
        .unwrap()
        .get(0);
    assert_eq!(other_count, 2);
    // 실패한 공지의 알림은 미리 넣은 한 건만 남는다(부분 배포가 남지 않는다).
    let failing_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM notifications WHERE source_id = $1",
            &[&failing],
        )
        .unwrap()
        .get(0);
    assert_eq!(failing_count, 1);
}

#[test]
fn 계속_실패하는_공지_10건_뒤의_정상_공지도_같은_실행에서_배포한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    for i in 0..10 {
        let failing = insert_notice(&mut client, minus(3600 - i));
        insert_conflicting_notification(&mut client, f.active, failing);
    }
    let normal = insert_notice(&mut client, minus(60));

    f.run();

    assert!(distributed_at(&mut client, normal).is_some());
    assert_eq!(count_undistributed(&mut client), 10);
}

#[test]
fn 배포_대상이_한_묶음을_넘어도_한_번_실행으로_모두_배포한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    for i in 0..12 {
        insert_notice(&mut client, minus(3600 - i));
    }
    f.run();
    assert_eq!(count_undistributed(&mut client), 0);
    assert_eq!(count_notifications(&mut client), 24);
}

#[test]
fn 삭제된_공지는_배포하지_않는다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let deleted = insert_notice(&mut client, minus(60));
    client
        .execute(
            "UPDATE notices SET deleted_at = now() WHERE id = $1",
            &[&deleted],
        )
        .unwrap();
    f.run();
    assert_eq!(count_notifications(&mut client), 0);
    assert_eq!(distributed_at(&mut client, deleted), None);
}

#[test]
fn 두_실행이_동시에_같은_공지를_배포해도_회원당_알림은_하나만_생긴다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let notice = insert_notice(&mut client, minus(60));
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let pool = f.pool.clone();
            let clock = f.clock.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let logger = Logger::new(Box::new(std::io::sink()), Level::Info, clock.clone());
                barrier.wait();
                distribute_notices(&pool, &*clock, &logger).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(count_notifications(&mut client), 2);
    assert!(distributed_at(&mut client, notice).is_some());
}

// ---------------------------------------------------------------------------------------------
// NoticeRepositoryImplTest
// ---------------------------------------------------------------------------------------------

fn target_ids(targets: &[DistributableNotice]) -> Vec<Uuid> {
    targets.iter().map(|t| t.notice_id).collect()
}

fn ordered_by_db(client: &mut Client, a: Uuid, b: Uuid) -> Vec<Uuid> {
    client
        .query(
            "SELECT id FROM notices WHERE id IN ($1, $2) ORDER BY id",
            &[&a, &b],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[test]
fn 배포_대상은_예약_시각이_지났거나_정각인_미배포_공지만_예약_시각_순으로_조회한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let at = now();
    let later = insert_notice(&mut client, at);
    let earlier = insert_notice(&mut client, minus(60));
    insert_notice(
        &mut client,
        at.checked_add(SignedDuration::from_micros(1)).unwrap(),
    );
    insert_notice_at(&mut client, minus(120), true);

    let repo = NoticeRepository;
    assert_eq!(
        target_ids(
            &repo
                .find_distributable_notices(&mut client, at, None, 10)
                .unwrap()
        ),
        [earlier, later]
    );
    let before = at.checked_sub(SignedDuration::from_micros(1)).unwrap();
    assert_eq!(
        target_ids(
            &repo
                .find_distributable_notices(&mut client, before, None, 10)
                .unwrap()
        ),
        [earlier]
    );
    assert_eq!(
        target_ids(
            &repo
                .find_distributable_notices(&mut client, at, None, 1)
                .unwrap()
        ),
        [earlier]
    );
}

#[test]
fn 커서를_주면_그_공지_다음부터_이어서_조회하고_예약_시각이_같으면_id_순으로_잇는다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let first = insert_notice(&mut client, minus(60));
    let second_a = insert_notice(&mut client, now());
    let second_b = insert_notice(&mut client, now());
    let third = insert_notice(&mut client, plus(1));
    let seconds = ordered_by_db(&mut client, second_a, second_b);
    let at = plus(60);

    let repo = NoticeRepository;
    let all = repo
        .find_distributable_notices(&mut client, at, None, 10)
        .unwrap();
    let next = repo
        .find_distributable_notices(&mut client, at, Some(all[1]), 10)
        .unwrap();
    assert_eq!(target_ids(&all), [first, seconds[0], seconds[1], third]);
    assert_eq!(target_ids(&next), [seconds[1], third]);
}

#[test]
fn 배포할_공지를_잠글_때_이미_배포했거나_예약_시각_전인_공지는_조회하지_않는다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let at = now();
    let distributed = insert_notice_at(&mut client, at, true);
    let future = insert_notice(&mut client, plus(60));
    let due = insert_notice(&mut client, at);

    let repo = NoticeRepository;
    let mut tx = client.transaction().unwrap();
    assert!(
        repo.find_distributable_for_update(&mut tx, distributed, at)
            .unwrap()
            .is_none()
    );
    assert!(
        repo.find_distributable_for_update(&mut tx, future, at)
            .unwrap()
            .is_none()
    );
    assert!(
        repo.find_distributable_for_update(&mut tx, due, at)
            .unwrap()
            .is_some()
    );
    tx.rollback().unwrap();
}

#[test]
fn 삭제된_공지는_배포_대상_조회와_배포_잠금_조회에서_제외한다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let deleted = insert_notice(&mut client, now());
    let active = insert_notice(&mut client, plus(1));
    client
        .execute(
            "UPDATE notices SET deleted_at = $2 WHERE id = $1",
            &[&deleted, &now()],
        )
        .unwrap();
    let at = plus(60);

    let repo = NoticeRepository;
    assert_eq!(
        target_ids(
            &repo
                .find_distributable_notices(&mut client, at, None, 10)
                .unwrap()
        ),
        [active]
    );
    let cursor = DistributableNotice {
        notice_id: Uuid::new_v4(),
        scheduled_at: minus(1),
    };
    assert_eq!(
        target_ids(
            &repo
                .find_distributable_notices(&mut client, at, Some(cursor), 10)
                .unwrap()
        ),
        [active]
    );
    let mut tx = client.transaction().unwrap();
    assert!(
        repo.find_distributable_for_update(&mut tx, deleted, at)
            .unwrap()
            .is_none()
    );
    tx.rollback().unwrap();
}

#[test]
fn 다른_배포가_잠근_공지는_건너뛴다() {
    let Some(f) = fixture() else { return };
    let mut client = f.client();
    let notice = insert_notice(&mut client, minus(60));
    let repo = NoticeRepository;
    let mut holder = f.client();
    let mut first = holder.transaction().unwrap();
    assert!(
        repo.find_distributable_for_update(&mut first, notice, now())
            .unwrap()
            .is_some()
    );

    let mut other = f.client();
    let mut second = other.transaction().unwrap();
    assert!(
        repo.find_distributable_for_update(&mut second, notice, now())
            .unwrap()
            .is_none()
    );
    second.rollback().unwrap();
    first.rollback().unwrap();
}
