//! 게시물 업로드·콜백·수정·삭제 동시성의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PostImageUploadConcurrencyTest`, `PostUpdateConcurrencyTest`를 옮겼다.
//!
//! 원본은 저장소 스파이로 락 직후에 멈추는 방식이었다. 여기서는 같은 효과를 데이터베이스 쪽에서 낸다. 락을 쥔 쪽은
//! 직접 연 트랜잭션으로 흉내 내고(검수는 관리자 도메인의 몫이라 같은 SQL로 대신한다), 수정 요청을 멈춰 둘 때는
//! `topics` 테이블을 잠가 수정이 게시물 락을 쥔 채 주제 조회에서 기다리게 한다.

mod support;

use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use chalkak_app::post::PostWiring;
use chalkak_app::post::storage::{ObjectStore, PresignPut, StorageError};
use chalkak_app::web::ApiError;
use serde_json::{Map, Value};
use support::{Fx, at, settings};
use uuid::Uuid;

const WAIT: Duration = Duration::from_millis(400);
const SUCCESS: &str = "SUCCESS";

fn metadata() -> Map<String, Value> {
    let Value::Object(map) = serde_json::json!({"width": 4032, "height": 3024}) else {
        unreachable!()
    };
    map
}

fn outcome<T>(result: Result<T, ApiError>) -> String {
    match result {
        Ok(_) => SUCCESS.to_owned(),
        Err(ApiError::App(e)) => e.message,
        Err(other) => format!("{other:?}"),
    }
}

/// 두 작업을 장벽 뒤에서 동시에 시작해 결과 메시지를 모은다(`runConcurrently`).
fn run_concurrently(
    first: impl FnOnce() -> String + Send + 'static,
    second: impl FnOnce() -> String + Send + 'static,
) -> Vec<String> {
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
    vec![t1.join().unwrap(), t2.join().unwrap()]
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

struct Seed {
    fx: Fx,
    user: Uuid,
    topic: Uuid,
    upload: Uuid,
}

fn seed() -> Option<Seed> {
    let fx = Fx::create()?;
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "ISSUED");
    fx.stage(upload);
    Some(Seed {
        fx,
        user,
        topic,
        upload,
    })
}

fn post_state(fx: &Fx, post: Uuid) -> (Option<String>, String, bool, bool) {
    let row = fx
        .client()
        .query_one(
            "SELECT title, moderation_status::text AS s, moderated_at IS NOT NULL AS moderated, \
             deleted_at IS NOT NULL AS deleted FROM posts WHERE id = $1",
            &[&post],
        )
        .unwrap();
    (
        row.get("title"),
        row.get("s"),
        row.get("moderated"),
        row.get("deleted"),
    )
}

// ---------------------------------------------------------------------------------------------
// PostImageUploadConcurrencyTest
// ---------------------------------------------------------------------------------------------

// PostImageUploadConcurrencyTest#createPost_concurrentSameUpload_createsSinglePost
#[test]
fn create_post_concurrent_same_upload_creates_single_post() {
    let Some(s) = seed() else { return };
    let (a, b) = (s.fx.command.clone(), s.fx.command.clone());
    let (user, topic, upload) = (s.user, s.topic, s.upload);

    let results = run_concurrently(
        move || outcome(a.create_post(user, topic, upload, None)),
        move || outcome(b.create_post(user, topic, upload, None)),
    );

    assert_eq!(s.fx.count("SELECT count(*) FROM posts"), 1);
    assert_eq!(s.fx.count("SELECT count(*) FROM photos"), 1);
    assert_eq!(
        sorted(results),
        sorted(vec![
            SUCCESS.to_owned(),
            "이미 사용된 사진입니다.".to_owned()
        ])
    );
}

// PostImageUploadConcurrencyTest#createPost_concurrentWithCompleteCallback_keepsStateConsistent
#[test]
fn create_post_concurrent_with_complete_callback_keeps_state_consistent() {
    let Some(s) = seed() else { return };
    let (a, b) = (s.fx.command.clone(), s.fx.command.clone());
    let (user, topic, upload) = (s.user, s.topic, s.upload);

    let results = run_concurrently(
        move || outcome(a.create_post(user, topic, upload, None)),
        move || outcome(b.complete_post_image_processing(upload, metadata())),
    );

    assert!(results.iter().any(|r| r == SUCCESS), "{results:?}");
    assert_eq!(s.fx.count("SELECT count(*) FROM posts"), 1);
    assert_eq!(
        s.fx.count("SELECT count(*) FROM post_image_uploads WHERE claimed_at IS NOT NULL"),
        1
    );
    // 어느 순서로 끝나든 이미지 처리가 끝난 게시물은 관리자 검수 대기 상태로 수렴한다.
    assert_eq!(
        s.fx.text("SELECT moderation_status::text FROM posts")
            .as_deref(),
        Some("PENDING")
    );
}

// PostImageUploadConcurrencyTest#processImage_concurrentCompleteAndFail_keepsStatesConsistent
#[test]
fn process_image_concurrent_complete_and_fail_keeps_states_consistent() {
    let Some(s) = seed() else { return };
    s.fx.command
        .create_post(s.user, s.topic, s.upload, None)
        .unwrap();
    let (a, b) = (s.fx.command.clone(), s.fx.command.clone());
    let upload = s.upload;

    let results = run_concurrently(
        move || outcome(a.complete_post_image_processing(upload, metadata())),
        move || outcome(b.fail_post_image_processing(upload, Some("PROCESSING_ERROR"))),
    );

    assert_eq!(results, vec![SUCCESS.to_owned(), SUCCESS.to_owned()]);
    let state = format!(
        "{}/{}",
        s.fx.upload_status(upload),
        s.fx.text("SELECT moderation_status::text FROM posts")
            .unwrap()
    );
    assert!(
        state == "READY/PENDING" || state == "REJECTED/REJECTED",
        "{state}"
    );
}

/// 업로드 확인 지점에서 멈추는 스토리지. 재시도가 S3 확인 중일 때 다른 요청이 끝날 수 있어야 한다.
struct GateStore {
    reached: mpsc::Sender<()>,
    release: std::sync::Mutex<mpsc::Receiver<()>>,
}

impl ObjectStore for GateStore {
    fn object_exists(&self, _bucket: &str, _key: &str) -> Result<bool, StorageError> {
        self.reached.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| StorageError("완료 콜백이 잠금 뒤에서 대기했습니다.".to_owned()))?;
        Ok(true)
    }

    fn presign_put(&self, _request: &PresignPut) -> Result<String, StorageError> {
        Ok("https://example.test/".to_owned())
    }
}

// PostImageUploadConcurrencyTest#createPost_stalledRetryWithCompleteCallback_avoidsDeadlock
#[test]
fn create_post_stalled_retry_with_complete_callback_avoids_deadlock() {
    let Some(s) = seed() else { return };
    s.fx.command
        .create_post(s.user, s.topic, s.upload, None)
        .unwrap();
    s.fx.exec(&format!(
        "UPDATE posts SET created_at = TIMESTAMPTZ '{}'",
        at(-8 * 60)
    ));
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let gate = Arc::new(GateStore {
        reached: reached_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let retry_command =
        PostWiring::new(settings(), gate, s.fx.events.clone()).command_service(&s.fx.ctx);
    let callback = s.fx.command.clone();
    let (user, topic, upload) = (s.user, s.topic, s.upload);

    let retry =
        thread::spawn(move || outcome(retry_command.create_post(user, topic, upload, None)));
    reached_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("재시도가 저장소 확인 지점에 도달해야 한다");
    // 재시도가 S3 확인에서 기다리는 동안 완료 콜백은 잠금 대기 없이 끝나야 한다.
    let completed = outcome(callback.complete_post_image_processing(upload, metadata()));
    release_tx.send(()).unwrap();
    let retried = retry.join().unwrap();

    assert_eq!(completed, SUCCESS);
    assert_eq!(retried, "이미 사용된 사진입니다.");
    let state = format!(
        "{}/{}",
        s.fx.upload_status(upload),
        s.fx.text("SELECT moderation_status::text FROM posts")
            .unwrap()
    );
    assert_eq!(state, "READY/PENDING");
}

// ---------------------------------------------------------------------------------------------
// PostUpdateConcurrencyTest
// ---------------------------------------------------------------------------------------------

struct Pending {
    fx: Fx,
    user: Uuid,
    post: Uuid,
}

fn pending_post() -> Option<Pending> {
    let fx = Fx::create()?;
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.post(user, topic, "PENDING", &at(-3600)).0;
    fx.exec(&format!(
        "UPDATE posts SET title = '기존 제목' WHERE id = '{post}'"
    ));
    Some(Pending { fx, user, post })
}

/// 게시물 락을 쥐는 트랜잭션. `statements`는 락을 잡은 직후 실행할 SQL이다.
fn hold_post_lock(fx: &Fx, post: Uuid, statements: &str) -> (postgres::Client, ()) {
    let mut client = fx.client();
    client.batch_execute("BEGIN").unwrap();
    client
        .query_one(
            "SELECT id FROM posts WHERE id = $1 FOR NO KEY UPDATE",
            &[&post],
        )
        .unwrap();
    client.batch_execute(statements).unwrap();
    (client, ())
}

// PostUpdateConcurrencyTest#updateAndModerate_updateLocksFirst_serializesAndApprovesUpdatedPost
#[test]
fn update_and_moderate_update_locks_first_serializes_and_approves_updated_post() {
    let Some(p) = pending_post() else { return };
    // 수정이 게시물 락을 쥔 채 주제 조회에서 멈추도록 topics를 잠가 둔다.
    let mut blocker = p.fx.client();
    blocker
        .batch_execute("BEGIN; LOCK TABLE topics IN ACCESS EXCLUSIVE MODE")
        .unwrap();
    let command = p.fx.command.clone();
    let (user, post) = (p.user, p.post);
    let update = thread::spawn(move || outcome(command.update_post(user, post, Some("수정 제목"))));
    thread::sleep(WAIT);

    let (done_tx, done_rx) = mpsc::channel();
    let mut moderator = p.fx.client();
    let moderation = thread::spawn(move || {
        moderator.batch_execute("BEGIN").unwrap();
        moderator
            .query_one(
                "SELECT id FROM posts WHERE id = $1 FOR NO KEY UPDATE",
                &[&post],
            )
            .unwrap();
        moderator
            .execute(
                "UPDATE posts SET moderation_status = 'APPROVED', moderated_at = CURRENT_TIMESTAMP WHERE id = $1",
                &[&post],
            )
            .unwrap();
        moderator.batch_execute("COMMIT").unwrap();
        done_tx.send(()).unwrap();
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_err(),
        "검수는 수정 커밋을 기다려야 한다"
    );
    blocker.batch_execute("COMMIT").unwrap();

    assert_eq!(update.join().unwrap(), SUCCESS);
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    moderation.join().unwrap();
    let (title, status, moderated, _) = post_state(&p.fx, p.post);
    assert_eq!(title.as_deref(), Some("수정 제목"));
    assert_eq!(status, "APPROVED");
    assert!(moderated);
}

// PostUpdateConcurrencyTest#updateAndModerate_rejectionLocksFirst_rejectsUpdateWithoutChangingTitle
#[test]
fn update_and_moderate_rejection_locks_first_rejects_update_without_changing_title() {
    let Some(p) = pending_post() else { return };
    let (mut moderator, ()) = hold_post_lock(
        &p.fx,
        p.post,
        &format!(
            "UPDATE posts SET moderation_status = 'REJECTED', moderated_at = CURRENT_TIMESTAMP WHERE id = '{}'",
            p.post
        ),
    );
    let (done_tx, done_rx) = mpsc::channel();
    let command = p.fx.command.clone();
    let (user, post) = (p.user, p.post);
    let update = thread::spawn(move || {
        let result = outcome(command.update_post(user, post, Some("수정 제목")));
        done_tx.send(()).unwrap();
        result
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_err(),
        "수정은 검수 커밋을 기다려야 한다"
    );
    moderator.batch_execute("COMMIT").unwrap();

    let update_outcome = update.join().unwrap();
    assert_ne!(update_outcome, SUCCESS);
    let (title, status, moderated, _) = post_state(&p.fx, p.post);
    assert_eq!(title.as_deref(), Some("기존 제목"));
    assert_eq!(status, "REJECTED");
    assert!(moderated);
}

// PostUpdateConcurrencyTest#updateAndDelete_deletionLocksFirst_rejectsUpdateWithoutChangingTitle
#[test]
fn update_and_delete_deletion_locks_first_rejects_update_without_changing_title() {
    let Some(p) = pending_post() else { return };
    let (mut deleter, ()) = hold_post_lock(
        &p.fx,
        p.post,
        &format!(
            "UPDATE posts SET deleted_at = CURRENT_TIMESTAMP WHERE id = '{}'",
            p.post
        ),
    );
    let (done_tx, done_rx) = mpsc::channel();
    let command = p.fx.command.clone();
    let (user, post) = (p.user, p.post);
    let update = thread::spawn(move || {
        let result = outcome(command.update_post(user, post, Some("수정 제목")));
        done_tx.send(()).unwrap();
        result
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_err(),
        "수정은 삭제 커밋을 기다려야 한다"
    );
    deleter.batch_execute("COMMIT").unwrap();

    assert_eq!(update.join().unwrap(), "게시물을 찾을 수 없습니다.");
    let (title, _, _, deleted) = post_state(&p.fx, p.post);
    assert_eq!(title.as_deref(), Some("기존 제목"));
    assert!(deleted);
}

// PostUpdateConcurrencyTest#updateAndUpdate_firstUpdateLocksFirst_appliesSecondTitleLast
#[test]
fn update_and_update_first_update_locks_first_applies_second_title_last() {
    let Some(p) = pending_post() else { return };
    let (mut first, ()) = hold_post_lock(
        &p.fx,
        p.post,
        &format!(
            "UPDATE posts SET title = '첫 번째 제목' WHERE id = '{}'",
            p.post
        ),
    );
    let (done_tx, done_rx) = mpsc::channel();
    let command = p.fx.command.clone();
    let (user, post) = (p.user, p.post);
    let second = thread::spawn(move || {
        let result = outcome(command.update_post(user, post, Some("두 번째 제목")));
        done_tx.send(()).unwrap();
        result
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_err(),
        "두 번째 수정은 첫 번째 수정 커밋을 기다려야 한다"
    );
    first.batch_execute("COMMIT").unwrap();

    assert_eq!(second.join().unwrap(), SUCCESS);
    let (title, status, _, _) = post_state(&p.fx, p.post);
    assert_eq!(title.as_deref(), Some("두 번째 제목"));
    assert_eq!(status, "PENDING");
}
