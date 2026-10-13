//! 좋아요 저장소·서비스와 좋아요/삭제 동시성의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PostLikeRepositoryTest`, `PostLikeServiceTest`, `PostLikeDeletionConcurrencyTest`를 옮겼다.

mod support;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chalkak_app::like::repository::PostLikeRepository;
use chalkak_app::post::repository::PostRepository;
use support::{Fx, assert_err};
use uuid::Uuid;

const WAIT: Duration = Duration::from_millis(400);

fn setup(fx: &Fx) -> (Uuid, Uuid, Uuid) {
    let author = fx.user();
    let fan = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(author, topic);
    (author, fan, post)
}

// ---------------------------------------------------------------------------------------------
// PostLikeRepositoryTest
// ---------------------------------------------------------------------------------------------

// PostLikeRepositoryTest#createIfAbsent_newPostLike_createsPostLike
#[test]
fn create_if_absent_new_post_like_creates_post_like() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    let mut client = fx.client();

    let created = PostLikeRepository
        .create_if_absent(&mut client, post, fan)
        .unwrap();

    assert_eq!(created, 1);
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 1);
}

// PostLikeRepositoryTest#createIfAbsent_existingPostLike_doesNotCreateDuplicate
#[test]
fn create_if_absent_existing_post_like_does_not_create_duplicate() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    fx.like(post, fan);
    let mut client = fx.client();

    let created = PostLikeRepository
        .create_if_absent(&mut client, post, fan)
        .unwrap();

    assert_eq!(created, 0);
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 1);
}

// PostLikeRepositoryTest#deleteByPostIdAndUserId_existingPostLike_deletesPostLike
#[test]
fn delete_by_post_id_and_user_id_existing_post_like_deletes_post_like() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    fx.like(post, fan);
    let mut client = fx.client();

    let deleted = PostLikeRepository
        .delete_by_post_id_and_user_id(&mut client, post, fan)
        .unwrap();

    assert_eq!(deleted, 1);
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 0);
}

// PostLikeRepositoryTest#deleteByPostIdAndUserId_missingPostLike_deletesNothing
#[test]
fn delete_by_post_id_and_user_id_missing_post_like_deletes_nothing() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    let mut client = fx.client();

    let deleted = PostLikeRepository
        .delete_by_post_id_and_user_id(&mut client, post, fan)
        .unwrap();

    assert_eq!(deleted, 0);
}

// PostLikeRepositoryTest#deleteByPostId_existingPostLikes_deletesOnlyTargetPostLikes
#[test]
fn delete_by_post_id_existing_post_likes_deletes_only_target_post_likes() {
    let Some(fx) = Fx::create() else { return };
    let (author, fan, post) = setup(&fx);
    let other_topic = fx.topic("2026-10-11", "2026-10-10T15:00:00Z", "2026-10-11T15:00:00Z");
    let other_post = fx.approved(author, other_topic);
    let second_fan = fx.user();
    fx.like(post, fan);
    fx.like(post, second_fan);
    fx.like(other_post, fan);
    let mut client = fx.client();

    let deleted = PostLikeRepository
        .delete_by_post_id(&mut client, post)
        .unwrap();

    assert_eq!(deleted, 2);
    assert_eq!(
        PostLikeRepository
            .count_by_post_id(&mut client, other_post)
            .unwrap(),
        1
    );
}

// PostLikeRepositoryTest#existsByPostIdAndUserId_existingPostLike_returnsTrue
#[test]
fn exists_by_post_id_and_user_id_existing_post_like_returns_true() {
    let Some(fx) = Fx::create() else { return };
    let (author, fan, post) = setup(&fx);
    fx.like(post, fan);
    let mut client = fx.client();

    assert!(
        PostLikeRepository
            .exists_by_post_id_and_user_id(&mut client, post, fan)
            .unwrap()
    );
    assert!(
        !PostLikeRepository
            .exists_by_post_id_and_user_id(&mut client, post, author)
            .unwrap()
    );
}

// PostLikeRepositoryTest#countByPostIds_postIds_returnsLikeCounts
#[test]
fn count_by_post_ids_post_ids_returns_like_counts() {
    let Some(fx) = Fx::create() else { return };
    let (author, fan, post) = setup(&fx);
    let other_topic = fx.topic("2026-10-11", "2026-10-10T15:00:00Z", "2026-10-11T15:00:00Z");
    let liked_once = fx.approved(author, other_topic);
    let no_likes = fx.approved(fan, other_topic);
    fx.like(post, fan);
    fx.like(post, author);
    fx.like(liked_once, fan);
    let mut client = fx.client();

    let counts = PostLikeRepository
        .count_by_post_ids(&mut client, &[post, liked_once, no_likes])
        .unwrap();

    assert_eq!(counts.get(&post), Some(&2));
    assert_eq!(counts.get(&liked_once), Some(&1));
    assert!(!counts.contains_key(&no_likes));
}

// PostLikeRepositoryTest#findLikedPostIds_postIdsAndUserId_returnsLikedPostIds
#[test]
fn find_liked_post_ids_post_ids_and_user_id_returns_liked_post_ids() {
    let Some(fx) = Fx::create() else { return };
    let (author, fan, post) = setup(&fx);
    let other_topic = fx.topic("2026-10-11", "2026-10-10T15:00:00Z", "2026-10-11T15:00:00Z");
    let not_liked = fx.approved(author, other_topic);
    fx.like(post, fan);
    fx.like(not_liked, author);
    let mut client = fx.client();

    let liked = PostLikeRepository
        .find_liked_post_ids(&mut client, &[post, not_liked], fan)
        .unwrap();

    assert_eq!(liked.len(), 1);
    assert!(liked.contains(&post));
}

// ---------------------------------------------------------------------------------------------
// PostLikeServiceTest
// ---------------------------------------------------------------------------------------------

// PostLikeServiceTest#likePost_visiblePost_returnsLikedResult
#[test]
fn like_post_visible_post_returns_liked_result() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);

    let result = fx.likes.like_post(post, fan).unwrap();

    assert_eq!(result.post_id, post);
    assert_eq!(result.like_count, 1);
    assert!(result.is_liked);
}

// PostLikeServiceTest#likePost_existingPostLike_returnsSameResult
#[test]
fn like_post_existing_post_like_returns_same_result() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    let first = fx.likes.like_post(post, fan).unwrap();

    let second = fx.likes.like_post(post, fan).unwrap();

    assert_eq!(first, second);
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 1);
}

// PostLikeServiceTest#likePost_invisiblePost_throwsNotFoundException
#[test]
fn like_post_invisible_post_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let fan = fx.user();
    let topic = fx.open_topic();
    let (pending, _) = fx.post(author, topic, "PENDING", "2026-10-12T01:00:00Z");

    assert_err(
        fx.likes.like_post(pending, fan),
        404,
        "게시물을 찾을 수 없습니다.",
    );
    assert_err(
        fx.likes.like_post(Uuid::new_v4(), fan),
        404,
        "게시물을 찾을 수 없습니다.",
    );
}

// PostLikeServiceTest#likePost_unknownUser_throwsUnauthorizedException
#[test]
fn like_post_unknown_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    let (_, _, post) = setup(&fx);
    assert_err(
        fx.likes.like_post(post, Uuid::new_v4()),
        401,
        "유효하지 않은 인증 정보입니다.",
    );
}

// PostLikeServiceTest#unlikePost_existingPostLike_returnsUnlikedResult
#[test]
fn unlike_post_existing_post_like_returns_unliked_result() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    fx.like(post, fan);

    let result = fx.likes.unlike_post(post, fan).unwrap();

    assert_eq!(result.like_count, 0);
    assert!(!result.is_liked);
}

// PostLikeServiceTest#unlikePost_missingPostLike_returnsSameResult
#[test]
fn unlike_post_missing_post_like_returns_same_result() {
    let Some(fx) = Fx::create() else { return };
    let (author, fan, post) = setup(&fx);
    fx.like(post, author);

    let first = fx.likes.unlike_post(post, fan).unwrap();
    let second = fx.likes.unlike_post(post, fan).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.like_count, 1);
    assert!(!first.is_liked);
}

// ---------------------------------------------------------------------------------------------
// PostLikeDeletionConcurrencyTest
// ---------------------------------------------------------------------------------------------

// PostLikeDeletionConcurrencyTest#deletePost_startedAfterLikeLock_waitsAndDeletesLike
#[test]
fn delete_post_started_after_like_lock_waits_and_deletes_like() {
    let Some(fx) = Fx::create() else { return };
    let (author, fan, post) = setup(&fx);
    let mut client = fx.client();
    let mut tx = client.transaction().unwrap();
    // 좋아요 쪽이 먼저 공유 락을 잡고 좋아요를 넣는다.
    PostRepository::default()
        .find_visible_by_id(&mut tx, post, true)
        .unwrap()
        .expect("공개된 게시물");
    PostLikeRepository
        .create_if_absent(&mut tx, post, fan)
        .unwrap();

    let (done_tx, done_rx) = mpsc::channel();
    let command = fx.command.clone();
    let deleter = thread::spawn(move || {
        let result = command.delete_post(author, post);
        done_tx.send(()).unwrap();
        result
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_err(),
        "삭제는 좋아요가 커밋될 때까지 기다려야 한다"
    );
    tx.commit().unwrap();

    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    deleter.join().unwrap().unwrap();
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 0);
}

// PostLikeDeletionConcurrencyTest#findVisibleByIdForShare_concurrentRequests_acquireLocksTogether
#[test]
fn find_visible_by_id_for_share_concurrent_requests_acquire_locks_together() {
    let Some(fx) = Fx::create() else { return };
    let (_, _, post) = setup(&fx);
    let mut first = fx.client();
    let mut first_tx = first.transaction().unwrap();
    PostRepository::default()
        .find_visible_by_id(&mut first_tx, post, true)
        .unwrap()
        .unwrap();

    let (done_tx, done_rx) = mpsc::channel();
    let config = fx.db.config.clone();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut second = config.pg.connect(postgres::NoTls).unwrap();
            let mut second_tx = second.transaction().unwrap();
            let found = PostRepository::default()
                .find_visible_by_id(&mut second_tx, post, true)
                .unwrap();
            done_tx.send(found.is_some()).unwrap();
            second_tx.commit().unwrap();
        });
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(10)), Ok(true));
    });
    first_tx.commit().unwrap();
}

// PostLikeDeletionConcurrencyTest#likePost_startedAfterDeletion_rejectsLikeAndLeavesNoLike
#[test]
fn like_post_started_after_deletion_rejects_like_and_leaves_no_like() {
    let Some(fx) = Fx::create() else { return };
    let (_, fan, post) = setup(&fx);
    let mut client = fx.client();
    let mut tx = client.transaction().unwrap();
    // 삭제 쪽이 먼저 게시물을 잠그고 삭제 표시를 한다.
    PostRepository::default()
        .find_by_id_for_update(&mut tx, post)
        .unwrap()
        .expect("게시물");
    tx.execute(
        "UPDATE posts SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1",
        &[&post],
    )
    .unwrap();

    let (done_tx, done_rx) = mpsc::channel();
    let likes = fx.likes.clone();
    let liker = thread::spawn(move || {
        let result = likes.like_post(post, fan);
        done_tx.send(()).unwrap();
        result
    });
    assert!(
        done_rx.recv_timeout(WAIT).is_err(),
        "좋아요는 삭제가 커밋될 때까지 기다려야 한다"
    );
    tx.commit().unwrap();

    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_err(liker.join().unwrap(), 404, "게시물을 찾을 수 없습니다.");
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 0);
}
