//! 게시물 조회(단건·목록·오늘 상태·캘린더)와 게시물 저장소의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PostQueryServiceTest`, `PostTodayStatusServiceTest`, `PostTodayStatusConsistencyTest`,
//! `PostCalendarServiceTest`, `PostCalendarRepositoryTest`, `PostRepositoryTest`, `PostRepositoryImplTest`를 옮겼다.

mod support;

use chalkak_app::photo::domain::Photo;
use chalkak_app::post::domain::{ModerationStatus, Post};
use chalkak_app::post::query::PostSort;
use chalkak_app::post::repository::{ListOrder, PostRepository};
use chalkak_app::post::support::in_transaction;
use chalkak_app::web::ApiError;
use chalkak_app::web::binding::LocalDate;
use jiff::civil::date;
use support::{Fx, NOW, assert_err, at, ts};
use uuid::Uuid;

const UNAUTHORIZED: &str = "유효하지 않은 인증 정보입니다.";
const POST_NOT_FOUND: &str = "게시물을 찾을 수 없습니다.";

fn today() -> LocalDate {
    LocalDate::Valid(date(2026, 10, 12))
}

fn older_topic(fx: &Fx, day: u32) -> Uuid {
    let d = format!("2026-10-{day:02}");
    let start = format!("2026-10-{:02}T15:00:00Z", day - 1);
    let end = format!("2026-10-{day:02}T15:00:00Z");
    fx.topic(&d, &start, &end)
}

fn ids(posts: &[chalkak_app::post::query::PostSummary]) -> Vec<Uuid> {
    posts.iter().map(|p| p.id).collect()
}

// ---------------------------------------------------------------------------------------------
// PostQueryServiceTest
// ---------------------------------------------------------------------------------------------

// PostQueryServiceTest#getPost_visiblePost_returnsPostDetailWithImageUrls
#[test]
fn get_post_visible_post_returns_post_detail_with_image_urls() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let viewer = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(author, topic);
    fx.like(post, viewer);
    fx.like(post, author);
    fx.exec(&format!(
        "UPDATE posts SET title = '제목' WHERE id = '{post}'"
    ));

    let detail = fx.query.get_post(post, viewer).unwrap();

    assert_eq!(detail.id, post);
    assert_eq!(detail.topic.id, topic);
    assert_eq!(detail.topic.topic_date, date(2026, 10, 12));
    assert_eq!(detail.title.as_deref(), Some("제목"));
    assert_eq!(detail.like_count, 2);
    assert!(detail.is_liked);
    assert!(
        detail
            .original_image_url
            .unwrap()
            .starts_with("https://cdn.test.chalkak/posts/test/original/")
    );
    assert!(
        detail
            .thumbnail_image_url
            .unwrap()
            .starts_with("https://cdn.test.chalkak/posts/test/thumbnail/")
    );
    assert_eq!(
        detail.signature_original_image_url.unwrap(),
        format!("https://cdn.test.chalkak/signatures/{author}/original.png")
    );
}

// PostQueryServiceTest#getPost_invisiblePost_throwsNotFoundException
#[test]
fn get_post_invisible_post_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let topic = fx.open_topic();
    let (pending, _) = fx.post(author, topic, "PENDING", &at(-60));
    assert_err(fx.query.get_post(pending, author), 404, POST_NOT_FOUND);
    assert_err(
        fx.query.get_post(Uuid::new_v4(), author),
        404,
        POST_NOT_FOUND,
    );
}

// PostQueryServiceTest#getPost_unknownUser_throwsUnauthorizedException
#[test]
fn get_post_unknown_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(author, topic);
    assert_err(fx.query.get_post(post, Uuid::new_v4()), 401, UNAUTHORIZED);
}

// PostQueryServiceTest#getPosts_recentSort_returnsPostList
#[test]
fn get_posts_recent_sort_returns_post_list() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let (a, b) = (fx.user(), fx.user());
    let old = fx.post(a, topic, "APPROVED", &at(-7200)).0;
    let new = fx.post(b, topic, "APPROVED", &at(-3600)).0;

    let result = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, None)
        .unwrap();

    assert_eq!(result.current_page, 1);
    assert_eq!(result.page_size, 10);
    assert!(!result.has_next);
    assert_eq!(result.random_seed, None);
    assert_eq!(ids(&result.posts), vec![new, old]);
    assert_eq!(
        result.posts[0].submitted_at,
        ts(NOW) - std::time::Duration::from_secs(3600)
    );
}

// PostQueryServiceTest#getPosts_popularSort_returnsPopularPostList
#[test]
fn get_posts_popular_sort_returns_popular_post_list() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let (a, b, fan) = (fx.user(), fx.user(), fx.user());
    let plain = fx.post(a, topic, "APPROVED", &at(-3600)).0;
    let loved = fx.post(b, topic, "APPROVED", &at(-7200)).0;
    fx.like(loved, fan);

    let result = fx
        .query
        .get_posts(today(), PostSort::Popular, None, 1, 10, None)
        .unwrap();

    assert_eq!(ids(&result.posts), vec![loved, plain]);
    assert_eq!(result.posts[0].like_count, 1);
}

// PostQueryServiceTest#getPosts_randomFirstPageWithoutSeed_generatesRandomSeed
#[test]
fn get_posts_random_first_page_without_seed_generates_random_seed() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    fx.approved(fx.user(), topic);

    let result = fx
        .query
        .get_posts(today(), PostSort::Random, None, 1, 10, None)
        .unwrap();

    let seed = result.random_seed.unwrap();
    assert_eq!(seed.len(), 8);
    assert!(
        seed.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
}

// PostQueryServiceTest#getPosts_randomSortWithSeed_reusesRandomSeed
#[test]
fn get_posts_random_sort_with_seed_reuses_random_seed() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    fx.approved(fx.user(), topic);

    let result = fx
        .query
        .get_posts(today(), PostSort::Random, Some("abcd1234"), 2, 10, None)
        .unwrap();

    assert_eq!(result.random_seed.as_deref(), Some("abcd1234"));
    assert_eq!(result.current_page, 2);
}

// PostQueryServiceTest#getPosts_invalidRandomSeedCombination_throwsBusinessException
#[test]
fn get_posts_invalid_random_seed_combination_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    fx.approved(fx.user(), topic);
    for (sort, seed, page) in [
        (PostSort::Recent, Some("abcd1234"), 1),
        (PostSort::Popular, Some("abcd1234"), 1),
        (PostSort::Random, None, 2),
    ] {
        assert_err(
            fx.query.get_posts(today(), sort, seed, page, 10, None),
            400,
            "조회 조건이 올바르지 않습니다.",
        );
    }
}

// PostQueryServiceTest#getPosts_futureTopicDate_throwsBusinessException
#[test]
fn get_posts_future_topic_date_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    assert_err(
        fx.query.get_posts(
            LocalDate::Valid(date(2026, 10, 13)),
            PostSort::Recent,
            None,
            1,
            10,
            None,
        ),
        400,
        "미래 날짜의 게시물은 조회할 수 없습니다.",
    );
}

// PostQueryServiceTest#getPosts_unknownTopicDate_throwsNotFoundException
#[test]
fn get_posts_unknown_topic_date_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    assert_err(
        fx.query
            .get_posts(today(), PostSort::Recent, None, 1, 10, None),
        404,
        "해당 날짜의 주제를 찾을 수 없습니다.",
    );
}

// PostQueryServiceTest#getPosts_withoutVisiblePosts_returnsEmptyList
#[test]
fn get_posts_without_visible_posts_returns_empty_list() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    fx.post(fx.user(), topic, "PENDING", &at(-60));

    let result = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, None)
        .unwrap();

    assert!(result.posts.is_empty());
    assert!(!result.has_next);
}

// PostQueryServiceTest#getPosts_authenticatedUser_returnsLikeInformation
#[test]
fn get_posts_authenticated_user_returns_like_information() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let (author, viewer, other) = (fx.user(), fx.user(), fx.user());
    let post = fx.approved(author, topic);
    fx.like(post, viewer);
    fx.like(post, other);

    let result = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, Some(viewer))
        .unwrap();

    assert_eq!(result.posts[0].like_count, 2);
    assert!(result.posts[0].is_liked);
}

// PostQueryServiceTest#getPosts_authenticatedUser_marksOwnPostAsMine
#[test]
fn get_posts_authenticated_user_marks_own_post_as_mine() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let (author, other) = (fx.user(), fx.user());
    let post = fx.approved(author, topic);

    let mine = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, Some(author))
        .unwrap();
    let theirs = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, Some(other))
        .unwrap();

    assert_eq!(ids(&mine.posts), vec![post]);
    assert!(mine.posts[0].is_mine);
    assert!(!theirs.posts[0].is_mine);
}

// PostQueryServiceTest#getPosts_anonymousUser_returnsLikeCountAndUnlikedStatus
#[test]
fn get_posts_anonymous_user_returns_like_count_and_unliked_status() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let (author, fan) = (fx.user(), fx.user());
    let post = fx.approved(author, topic);
    fx.like(post, fan);

    let result = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, None)
        .unwrap();

    assert_eq!(result.posts[0].like_count, 1);
    assert!(!result.posts[0].is_liked);
}

// PostQueryServiceTest#getPosts_anonymousUser_returnsNoOwnedPost
#[test]
fn get_posts_anonymous_user_returns_no_owned_post() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    fx.approved(fx.user(), topic);
    let result = fx
        .query
        .get_posts(today(), PostSort::Recent, None, 1, 10, None)
        .unwrap();
    assert!(!result.posts[0].is_mine);
}

// ---------------------------------------------------------------------------------------------
// PostTodayStatusServiceTest
// ---------------------------------------------------------------------------------------------

// PostTodayStatusServiceTest#getMyTodayPostStatus_activePostExists_returnsPosted
#[test]
fn get_my_today_post_status_active_post_exists_returns_posted() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    for (status, created) in [
        ("VALIDATING", at(-60)),
        ("PENDING", at(-60)),
        ("APPROVED", at(-60)),
    ] {
        let other_user = fx.user();
        let (post, _) = fx.post(other_user, topic, status, &created);
        let result = fx.query.get_my_today_post_status(other_user).unwrap();
        assert!(result.is_posted, "{status}");
        assert_eq!(result.post_id, Some(post));
        assert_eq!(
            result.moderation_status.map(ModerationStatus::as_str),
            Some(status)
        );
        assert_eq!(result.topic_date, date(2026, 10, 12));
    }
    let _ = user;
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_noTopic_throwsNotFoundException
#[test]
fn get_my_today_post_status_no_topic_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    assert_err(
        fx.query.get_my_today_post_status(user),
        404,
        "참여할 수 있는 주제가 없습니다.",
    );
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_beforeOpenTopicOnly_throwsNotFoundException
#[test]
fn get_my_today_post_status_before_open_topic_only_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    fx.topic("2026-10-13", "2026-10-12T15:00:00Z", "2026-10-13T15:00:00Z");
    assert_err(
        fx.query.get_my_today_post_status(user),
        404,
        "참여할 수 있는 주제가 없습니다.",
    );
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_closedTopicOnly_throwsNotFoundException
#[test]
fn get_my_today_post_status_closed_topic_only_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    older_topic(&fx, 11);
    assert_err(
        fx.query.get_my_today_post_status(user),
        404,
        "참여할 수 있는 주제가 없습니다.",
    );
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_deletedOpenTopic_throwsNotFoundException
#[test]
fn get_my_today_post_status_deleted_open_topic_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.delete_topic(topic);
    assert_err(
        fx.query.get_my_today_post_status(user),
        404,
        "참여할 수 있는 주제가 없습니다.",
    );
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_unknownUser_throwsUnauthorizedException
#[test]
fn get_my_today_post_status_unknown_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    fx.open_topic();
    assert_err(
        fx.query.get_my_today_post_status(Uuid::new_v4()),
        401,
        UNAUTHORIZED,
    );
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_deletedPost_returnsNotPosted
#[test]
fn get_my_today_post_status_deleted_post_returns_not_posted() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.soft_delete("posts", post);
    let result = fx.query.get_my_today_post_status(user).unwrap();
    assert!(!result.is_posted);
    assert_eq!(result.post_id, None);
    assert_eq!(result.moderation_status, None);
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_otherAuthorPost_returnsNotPosted
#[test]
fn get_my_today_post_status_other_author_post_returns_not_posted() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.approved(fx.user(), topic);
    assert!(!fx.query.get_my_today_post_status(user).unwrap().is_posted);
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_rejectedPost_returnsNotPosted
#[test]
fn get_my_today_post_status_rejected_post_returns_not_posted() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.post(user, topic, "REJECTED", &at(-60));
    assert!(!fx.query.get_my_today_post_status(user).unwrap().is_posted);
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_postOnClosedTopic_returnsNotPosted
#[test]
fn get_my_today_post_status_post_on_closed_topic_returns_not_posted() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let closed = older_topic(&fx, 11);
    fx.open_topic();
    fx.approved(user, closed);
    assert!(!fx.query.get_my_today_post_status(user).unwrap().is_posted);
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_validatingPostTimedOut_returnsNotPosted
#[test]
fn get_my_today_post_status_validating_post_timed_out_returns_not_posted() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.post(user, topic, "VALIDATING", &at(-8 * 60));
    assert!(!fx.query.get_my_today_post_status(user).unwrap().is_posted);
}

// PostTodayStatusServiceTest#getMyTodayPostStatus_topicOpenedYesterday_returnsYesterdayTopicDate
#[test]
fn get_my_today_post_status_topic_opened_yesterday_returns_yesterday_topic_date() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    fx.topic("2026-10-11", "2026-10-11T06:00:00Z", "2026-10-12T06:00:00Z");
    let result = fx.query.get_my_today_post_status(user).unwrap();
    assert_eq!(result.topic_date, date(2026, 10, 11));
}

// PostTodayStatusConsistencyTest#getMyTodayPostStatus_existingPost_agreesWithCreatePost
#[test]
fn get_my_today_post_status_existing_post_agrees_with_create_post() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.post(user, topic, "PENDING", &at(-60));
    let upload = fx.upload_in(user, "READY");

    assert!(fx.query.get_my_today_post_status(user).unwrap().is_posted);
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "이미 해당 주제에 게시물을 작성했습니다.",
    );
}

// PostTodayStatusConsistencyTest#getMyTodayPostStatus_noExistingPost_agreesWithCreatePost
#[test]
fn get_my_today_post_status_no_existing_post_agrees_with_create_post() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.post(user, topic, "VALIDATING", &at(-8 * 60));
    let upload = fx.upload_in(user, "READY");

    assert!(!fx.query.get_my_today_post_status(user).unwrap().is_posted);
    fx.command.create_post(user, topic, upload, None).unwrap();
}

// ---------------------------------------------------------------------------------------------
// PostCalendarServiceTest
// ---------------------------------------------------------------------------------------------

// PostCalendarServiceTest#getMyPostCalendar_validYearMonth_returnsCalendarResult
#[test]
fn get_my_post_calendar_valid_year_month_returns_calendar_result() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let t11 = older_topic(&fx, 11);
    let t12 = fx.open_topic();
    let p11 = fx.post(user, t11, "APPROVED", &at(-90000)).0;
    let p12 = fx.post(user, t12, "PENDING", &at(-3600)).0;

    let result = fx.query.get_my_post_calendar(user, 2026, 10).unwrap();

    assert_eq!((result.year, result.month), (2026, 10));
    assert_eq!(result.posts.len(), 2);
    assert_eq!(result.posts[0].post_id, p11);
    assert_eq!(result.posts[0].topic_date, date(2026, 10, 11));
    assert_eq!(result.posts[0].status, ModerationStatus::Approved);
    assert_eq!(result.posts[1].post_id, p12);
    assert_eq!(result.posts[1].status, ModerationStatus::Pending);
    assert!(
        result.posts[1]
            .thumbnail_image_url
            .as_ref()
            .unwrap()
            .contains("/posts/test/thumbnail/")
    );
}

// PostCalendarServiceTest#getMyPostCalendar_noPosts_returnsEmptyResult
#[test]
fn get_my_post_calendar_no_posts_returns_empty_result() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let result = fx.query.get_my_post_calendar(user, 2026, 9).unwrap();
    assert!(result.posts.is_empty());
}

// PostCalendarServiceTest#getMyPostCalendar_unknownUser_throwsUnauthorizedException
#[test]
fn get_my_post_calendar_unknown_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    assert_err(
        fx.query.get_my_post_calendar(Uuid::new_v4(), 2026, 10),
        401,
        UNAUTHORIZED,
    );
}

// PostCalendarServiceTest#getMyPostCalendarMonths_validUser_returnsRecordedMonths
#[test]
fn get_my_post_calendar_months_valid_user_returns_recorded_months() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let t = fx.topic("2026-09-30", "2026-09-29T15:00:00Z", "2026-09-30T15:00:00Z");
    fx.approved(user, t);
    fx.approved(user, fx.open_topic());
    assert_eq!(
        fx.query.get_my_post_calendar_months(user).unwrap(),
        vec![(2026, 10), (2026, 9)]
    );
}

// PostCalendarServiceTest#getMyPostCalendarMonths_noPosts_returnsEmpty
#[test]
fn get_my_post_calendar_months_no_posts_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    assert!(
        fx.query
            .get_my_post_calendar_months(user)
            .unwrap()
            .is_empty()
    );
}

// PostCalendarServiceTest#getMyPostCalendarMonths_unknownUser_throwsUnauthorizedException
#[test]
fn get_my_post_calendar_months_unknown_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    assert_err(
        fx.query.get_my_post_calendar_months(Uuid::new_v4()),
        401,
        UNAUTHORIZED,
    );
}

// PostCalendarServiceTest#getMyPostCalendarMonths_deletedUser_throwsUnauthorizedException
#[test]
fn get_my_post_calendar_months_deleted_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    fx.delete_user(user);
    assert_err(
        fx.query.get_my_post_calendar_months(user),
        401,
        UNAUTHORIZED,
    );
}

// ---------------------------------------------------------------------------------------------
// PostCalendarRepositoryTest
// ---------------------------------------------------------------------------------------------

fn calendar_rows(
    fx: &Fx,
    user: Uuid,
    from: (i8, i8),
    to: (i8, i8),
) -> Vec<chalkak_app::post::repository::CalendarPostRow> {
    let mut client = fx.client();
    PostRepository::default()
        .find_calendar_posts(
            &mut client,
            user,
            date(2026, from.0, from.1),
            date(2026, to.0, to.1),
        )
        .unwrap()
}

fn months(fx: &Fx, user: Uuid) -> Vec<(i32, i32)> {
    let mut client = fx.client();
    PostRepository::default()
        .find_calendar_months(&mut client, user)
        .unwrap()
}

// PostCalendarRepositoryTest#findCalendarPostsByAuthorIdAndTopicDateBetween_matchingPosts_returnsOrderedPosts
#[test]
fn find_calendar_posts_by_author_id_and_topic_date_between_matching_posts_returns_ordered_posts() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let late = fx.open_topic();
    let early = older_topic(&fx, 5);
    let late_post = fx.approved(user, late);
    let early_post = fx.approved(user, early);
    let rows = calendar_rows(&fx, user, (10, 1), (10, 31));
    assert_eq!(
        rows.iter().map(|r| r.post_id).collect::<Vec<_>>(),
        vec![early_post, late_post]
    );
}

// PostCalendarRepositoryTest#findCalendarPostsByAuthorIdAndTopicDateBetween_deletedResource_returnsEmpty
#[test]
fn find_calendar_posts_by_author_id_and_topic_date_between_deleted_resource_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (post, photo) = fx.post(user, topic, "APPROVED", &at(-60));
    fx.soft_delete("photos", photo);
    assert!(calendar_rows(&fx, user, (10, 1), (10, 31)).is_empty());
    fx.exec(&format!(
        "UPDATE photos SET deleted_at = NULL WHERE id = '{photo}'"
    ));
    fx.soft_delete("posts", post);
    assert!(calendar_rows(&fx, user, (10, 1), (10, 31)).is_empty());
    fx.exec(&format!(
        "UPDATE posts SET deleted_at = NULL WHERE id = '{post}'"
    ));
    fx.delete_topic(topic);
    assert!(calendar_rows(&fx, user, (10, 1), (10, 31)).is_empty());
}

// PostCalendarRepositoryTest#findCalendarMonthsByAuthorId_noPosts_returnsEmpty
#[test]
fn find_calendar_months_by_author_id_no_posts_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    assert!(months(&fx, user).is_empty());
}

// PostCalendarRepositoryTest#findCalendarMonthsByAuthorId_multipleYears_returnsDistinctMonthsDescending
#[test]
fn find_calendar_months_by_author_id_multiple_years_returns_distinct_months_descending() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    for (d, s, e) in [
        ("2025-12-31", "2025-12-30T15:00:00Z", "2025-12-31T15:00:00Z"),
        ("2026-01-01", "2025-12-31T15:00:00Z", "2026-01-01T15:00:00Z"),
        ("2026-01-02", "2026-01-01T15:00:00Z", "2026-01-02T15:00:00Z"),
    ] {
        let t = fx.topic(d, s, e);
        fx.approved(user, t);
    }
    assert_eq!(months(&fx, user), vec![(2026, 1), (2025, 12)]);
}

// PostCalendarRepositoryTest#findCalendarMonthsByAuthorId_moderationStatus_returnsOnlyVisibleMonths
#[test]
fn find_calendar_months_by_author_id_moderation_status_returns_only_visible_months() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    for (i, status) in ["VALIDATING", "PENDING", "REJECTED", "APPROVED"]
        .iter()
        .enumerate()
    {
        let month = i + 1;
        let t = fx.topic(
            &format!("2026-0{month}-15"),
            &format!("2026-0{month}-14T15:00:00Z"),
            &format!("2026-0{month}-15T15:00:00Z"),
        );
        fx.post(user, t, status, &at(-60));
    }
    assert_eq!(months(&fx, user), vec![(2026, 4), (2026, 2)]);
}

// PostCalendarRepositoryTest#findCalendarMonthsByAuthorId_otherUserMonth_excludesMonth
#[test]
fn find_calendar_months_by_author_id_other_user_month_excludes_month() {
    let Some(fx) = Fx::create() else { return };
    let (user, other) = (fx.user(), fx.user());
    let t = fx.open_topic();
    fx.approved(other, t);
    assert!(months(&fx, user).is_empty());
}

// PostCalendarRepositoryTest#findCalendarMonthsByAuthorId_deletedResource_returnsEmpty
#[test]
fn find_calendar_months_by_author_id_deleted_resource_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let t = fx.open_topic();
    fx.approved(user, t);
    fx.delete_user(user);
    assert!(months(&fx, user).is_empty());
}

// PostCalendarRepositoryTest#findCalendarMonthsByAuthorId_lastPostDeleted_removesOnlyThatMonth
#[test]
fn find_calendar_months_by_author_id_last_post_deleted_removes_only_that_month() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let sep = fx.topic("2026-09-30", "2026-09-29T15:00:00Z", "2026-09-30T15:00:00Z");
    fx.approved(user, sep);
    let oct = fx.open_topic();
    let oct_post = fx.approved(user, oct);
    fx.soft_delete("posts", oct_post);
    assert_eq!(months(&fx, user), vec![(2026, 9)]);
}

// ---------------------------------------------------------------------------------------------
// PostRepositoryTest
// ---------------------------------------------------------------------------------------------

fn visible(fx: &Fx, post: Uuid) -> Option<chalkak_app::post::repository::VisiblePost> {
    let mut client = fx.client();
    PostRepository::default()
        .find_visible_by_id(&mut client, post, false)
        .unwrap()
}

// PostRepositoryTest#findVisibleById_approvedPost_returnsPost
#[test]
fn find_visible_by_id_approved_post_returns_post() {
    let Some(fx) = Fx::create() else { return };
    let (user, topic) = (fx.user(), fx.open_topic());
    let post = fx.approved(user, topic);
    let found = visible(&fx, post).unwrap();
    assert_eq!(found.id, post);
    assert_eq!(found.topic_id, topic);
    assert_eq!(found.topic_date, date(2026, 10, 12));
    assert!(
        found
            .signature_original_storage_key
            .contains(&user.to_string())
    );
}

// PostRepositoryTest#findVisibleById_postWithTitle_mapsTitleToString
#[test]
fn find_visible_by_id_post_with_title_maps_title_to_string() {
    let Some(fx) = Fx::create() else { return };
    let post = fx.approved(fx.user(), fx.open_topic());
    fx.exec(&format!(
        "UPDATE posts SET title = '제목' WHERE id = '{post}'"
    ));
    assert_eq!(visible(&fx, post).unwrap().title.as_deref(), Some("제목"));
}

// PostRepositoryTest#findVisibleById_postWithoutTitle_mapsTitleToNull
#[test]
fn find_visible_by_id_post_without_title_maps_title_to_null() {
    let Some(fx) = Fx::create() else { return };
    let post = fx.approved(fx.user(), fx.open_topic());
    assert_eq!(visible(&fx, post).unwrap().title, None);
}

// PostRepositoryTest#findVisibleById_unknownPost_returnsEmpty
#[test]
fn find_visible_by_id_unknown_post_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    assert!(visible(&fx, Uuid::new_v4()).is_none());
}

// PostRepositoryTest#findVisibleById_unapprovedPost_returnsEmpty
#[test]
fn find_visible_by_id_unapproved_post_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    for status in ["VALIDATING", "PENDING", "REJECTED"] {
        let (post, _) = fx.post(fx.user(), topic, status, &at(-60));
        assert!(visible(&fx, post).is_none(), "{status}");
    }
}

// PostRepositoryTest#findVisibleById_deletedPost_returnsEmpty
#[test]
fn find_visible_by_id_deleted_post_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let post = fx.approved(fx.user(), fx.open_topic());
    fx.soft_delete("posts", post);
    assert!(visible(&fx, post).is_none());
}

// PostRepositoryTest#findVisibleById_deletedTopic_returnsEmpty
#[test]
fn find_visible_by_id_deleted_topic_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let post = fx.approved(fx.user(), topic);
    fx.delete_topic(topic);
    assert!(visible(&fx, post).is_none());
}

// PostRepositoryTest#findVisibleById_deletedPhoto_returnsEmpty
#[test]
fn find_visible_by_id_deleted_photo_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let (post, photo) = fx.post(fx.user(), fx.open_topic(), "APPROVED", &at(-60));
    fx.soft_delete("photos", photo);
    assert!(visible(&fx, post).is_none());
}

// PostRepositoryTest#findVisibleById_deletedAuthor_returnsEmpty
#[test]
fn find_visible_by_id_deleted_author_returns_empty() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let post = fx.approved(user, fx.open_topic());
    fx.delete_user(user);
    assert!(visible(&fx, post).is_none());
}

fn slice(
    fx: &Fx,
    topic: Uuid,
    order: ListOrder<'_>,
    page: i64,
    size: i64,
) -> chalkak_app::post::repository::PostSlice {
    let mut client = fx.client();
    PostRepository::default()
        .find_visible_slice(&mut client, topic, order, page, size)
        .unwrap()
}

fn slice_ids(s: &chalkak_app::post::repository::PostSlice) -> Vec<Uuid> {
    s.posts.iter().map(|p| p.id).collect()
}

// PostRepositoryTest#findVisibleByTopicId_invisiblePost_returnsEmptySlice
#[test]
fn find_visible_by_topic_id_invisible_post_returns_empty_slice() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    for status in ["VALIDATING", "PENDING", "REJECTED"] {
        fx.post(fx.user(), topic, status, &at(-60));
    }
    let s = slice(&fx, topic, ListOrder::Recent, 0, 10);
    assert!(s.posts.is_empty());
    assert!(!s.has_next);
}

// PostRepositoryTest#findVisibleRecentByTopicId_visiblePosts_returnsRecentSlice
#[test]
fn find_visible_recent_by_topic_id_visible_posts_returns_recent_slice() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let oldest = fx.post(fx.user(), topic, "APPROVED", &at(-300)).0;
    let middle = fx.post(fx.user(), topic, "APPROVED", &at(-200)).0;
    let newest = fx.post(fx.user(), topic, "APPROVED", &at(-100)).0;
    let first = slice(&fx, topic, ListOrder::Recent, 0, 2);
    assert_eq!(slice_ids(&first), vec![newest, middle]);
    assert!(first.has_next);
    let second = slice(&fx, topic, ListOrder::Recent, 1, 2);
    assert_eq!(slice_ids(&second), vec![oldest]);
    assert!(!second.has_next);
}

// PostRepositoryTest#findVisibleRecentByTopicId_sameCreatedAt_returnsIdDescendingSlice
#[test]
fn find_visible_recent_by_topic_id_same_created_at_returns_id_descending_slice() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let created = at(-100);
    let mut all: Vec<Uuid> = (0..3)
        .map(|_| fx.post(fx.user(), topic, "APPROVED", &created).0)
        .collect();
    all.sort();
    all.reverse();
    assert_eq!(slice_ids(&slice(&fx, topic, ListOrder::Recent, 0, 10)), all);
}

// PostRepositoryTest#findVisiblePopularByTopicId_visiblePosts_returnsLikeCountDescendingSlice
#[test]
fn find_visible_popular_by_topic_id_visible_posts_returns_like_count_descending_slice() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let (f1, f2) = (fx.user(), fx.user());
    let none = fx.post(fx.user(), topic, "APPROVED", &at(-100)).0;
    let one = fx.post(fx.user(), topic, "APPROVED", &at(-200)).0;
    let two = fx.post(fx.user(), topic, "APPROVED", &at(-300)).0;
    fx.like(one, f1);
    fx.like(two, f1);
    fx.like(two, f2);
    assert_eq!(
        slice_ids(&slice(&fx, topic, ListOrder::Popular, 0, 10)),
        vec![two, one, none]
    );
}

// PostRepositoryTest#findVisiblePopularByTopicId_sameLikeCount_returnsCreatedAtDescendingSlice
#[test]
fn find_visible_popular_by_topic_id_same_like_count_returns_created_at_descending_slice() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let old = fx.post(fx.user(), topic, "APPROVED", &at(-300)).0;
    let new = fx.post(fx.user(), topic, "APPROVED", &at(-100)).0;
    assert_eq!(
        slice_ids(&slice(&fx, topic, ListOrder::Popular, 0, 10)),
        vec![new, old]
    );
}

// PostRepositoryTest#findVisiblePopularByTopicId_sameLikeCountAndCreatedAt_returnsIdAscendingSlice
#[test]
fn find_visible_popular_by_topic_id_same_like_count_and_created_at_returns_id_ascending_slice() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let created = at(-100);
    let mut all: Vec<Uuid> = (0..3)
        .map(|_| fx.post(fx.user(), topic, "APPROVED", &created).0)
        .collect();
    all.sort();
    assert_eq!(
        slice_ids(&slice(&fx, topic, ListOrder::Popular, 0, 10)),
        all
    );
}

// PostRepositoryTest#findVisiblePopularByTopicId_multiplePages_returnsStablePages
#[test]
fn find_visible_popular_by_topic_id_multiple_pages_returns_stable_pages() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let fan = fx.user();
    let posts: Vec<Uuid> = (0..5)
        .map(|i| fx.post(fx.user(), topic, "APPROVED", &at(-100 - i)).0)
        .collect();
    fx.like(posts[3], fan);
    let all = slice_ids(&slice(&fx, topic, ListOrder::Popular, 0, 10));
    let mut paged = Vec::new();
    for page in 0..3 {
        paged.extend(slice_ids(&slice(&fx, topic, ListOrder::Popular, page, 2)));
    }
    assert_eq!(paged, all);
    assert_eq!(all[0], posts[3]);
}

// PostRepositoryTest#findVisibleRandomByTopicId_sameSeed_returnsSameOrder
#[test]
fn find_visible_random_by_topic_id_same_seed_returns_same_order() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    for i in 0..6 {
        fx.post(fx.user(), topic, "APPROVED", &at(-100 - i));
    }
    let a = slice_ids(&slice(&fx, topic, ListOrder::Random("abcd1234"), 0, 10));
    let b = slice_ids(&slice(&fx, topic, ListOrder::Random("abcd1234"), 0, 10));
    assert_eq!(a, b);
    assert_eq!(a.len(), 6);
}

// PostRepositoryTest#findVisibleRandomByTopicId_sameSeed_returnsStablePages
#[test]
fn find_visible_random_by_topic_id_same_seed_returns_stable_pages() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    for i in 0..5 {
        fx.post(fx.user(), topic, "APPROVED", &at(-100 - i));
    }
    let all = slice_ids(&slice(&fx, topic, ListOrder::Random("00ff00ff"), 0, 10));
    let mut paged = Vec::new();
    for page in 0..3 {
        paged.extend(slice_ids(&slice(
            &fx,
            topic,
            ListOrder::Random("00ff00ff"),
            page,
            2,
        )));
    }
    assert_eq!(paged, all);
}

fn new_post(fx: &Fx, user: Uuid, topic: Uuid, upload: Uuid, key: &str) -> Post {
    let _ = fx;
    Post::create(
        user,
        topic,
        Photo::create(Some(key)).unwrap(),
        Some(upload),
        None,
    )
    .unwrap()
}

fn save(fx: &Fx, post: &mut Post) -> Result<(), ApiError> {
    in_transaction(&fx.ctx.pool, |tx| {
        PostRepository::default().save(tx, post, ts(NOW))
    })
}

// PostRepositoryTest#save_duplicateUserAndTopic_throwsBusinessException
#[test]
fn save_duplicate_user_and_topic_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let (user, topic) = (fx.user(), fx.open_topic());
    fx.post(user, topic, "PENDING", &at(-60));
    let upload = fx.upload_in(user, "READY");
    let mut post = new_post(
        &fx,
        user,
        topic,
        upload,
        "chalkak/posts/test/original/new.webp",
    );
    assert_err(
        save(&fx, &mut post),
        400,
        "이미 사용된 게시물 생성 정보입니다.",
    );
}

// PostRepositoryTest#save_duplicatePhotoStorageKey_throwsBusinessException
#[test]
fn save_duplicate_photo_storage_key_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let (user, topic) = (fx.user(), fx.open_topic());
    let (_, photo) = fx.post(fx.user(), topic, "PENDING", &at(-60));
    let upload = fx.upload_in(user, "READY");
    let key = format!("chalkak/posts/test/original/{photo}.webp");
    let mut post = new_post(&fx, user, topic, upload, &key);
    assert_err(
        save(&fx, &mut post),
        400,
        "이미 사용된 게시물 생성 정보입니다.",
    );
}

// PostRepositoryTest#save_postConstraintViolation_rollsBackCascadedPhoto
#[test]
fn save_post_constraint_violation_rolls_back_cascaded_photo() {
    let Some(fx) = Fx::create() else { return };
    let (user, topic) = (fx.user(), fx.open_topic());
    fx.post(user, topic, "PENDING", &at(-60));
    let photos_before = fx.count("SELECT count(*) FROM photos");
    let upload = fx.upload_in(user, "READY");
    let mut post = new_post(
        &fx,
        user,
        topic,
        upload,
        "chalkak/posts/test/original/rolled.webp",
    );
    assert!(save(&fx, &mut post).is_err());
    assert_eq!(fx.count("SELECT count(*) FROM photos"), photos_before);
}

// PostRepositoryImplTest#save_unknownIntegrityViolation_propagatesOriginalException
#[test]
fn save_unknown_integrity_violation_propagates_original_exception() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    let unknown_user = Uuid::new_v4();
    let upload = fx.upload_in(fx.user(), "READY");
    let mut post = new_post(
        &fx,
        unknown_user,
        topic,
        upload,
        "chalkak/posts/test/original/fk.webp",
    );
    match save(&fx, &mut post) {
        Err(ApiError::App(e)) => panic!("업무 오류로 바뀌면 안 됩니다: {e:?}"),
        Err(_) => {}
        Ok(()) => panic!("외래 키 위반이어야 합니다"),
    }
}
