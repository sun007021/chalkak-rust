//! 작성자 공개 사진 목록(`GET /api/v1/users/{userId}/posts`)의 DB 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `AuthorPostRepositoryTest`, `AuthorPostQueryServiceTest`, `UserControllerTest#getAuthorPosts_*`, `PostLikesTest`를
//! 옮겼다. 컨트롤러 테스트는 서비스를 목으로 바꿨지만 여기서는 실제 서비스와 DB로 라우터까지 통과시킨다.

mod support;

use chalkak_app::register_routes;
use chalkak_app::security::{self, AUTHORITY_USER, Principal};
use chalkak_app::web::Routes;
use chalkak_http::{Handler, Method, Request, Response, Router};
use serde_json::{Value, json};
use support::{Fx, assert_err, at};
use uuid::Uuid;

const UNAUTHORIZED: &str = "유효하지 않은 인증 정보입니다.";
const AUTHOR_NOT_FOUND: &str = "작성자를 찾을 수 없습니다.";

struct Http {
    fx: Fx,
    router: Router,
}

fn http() -> Option<Http> {
    let fx = Fx::create()?;
    let router = register_routes(Routes::new(fx.ctx.logger.clone()), &fx.ctx).into_router();
    Some(Http { fx, router })
}

impl Http {
    fn get(&self, path: &str, query: &[(&str, &str)], user: Option<Uuid>) -> (Response, Value) {
        let mut req = Request::new(Method::Get, path.to_owned());
        req.query = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let _guard = user.map(|id| {
            security::enter(Principal {
                subject: id.to_string(),
                authorities: vec![AUTHORITY_USER.to_owned()],
            })
        });
        let response = self.router.handle(req);
        let json = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        (response, json)
    }

    fn author_posts(
        &self,
        author: &str,
        query: &[(&str, &str)],
        viewer: Option<Uuid>,
    ) -> (Response, Value) {
        self.get(&format!("/api/v1/users/{author}/posts"), query, viewer)
    }
}

/// 주제 날짜별로 겹치지 않는 참여 기간(날짜 00:00Z ~ 다음 날 00:00Z)을 가진 주제.
fn topic_on(fx: &Fx, date: jiff::civil::Date) -> Uuid {
    let next = date + jiff::Span::new().days(1);
    fx.topic(
        &date.to_string(),
        &format!("{date}T00:00:00Z"),
        &format!("{next}T00:00:00Z"),
    )
}

/// 날짜(`2026-04-DD`)별 주제를 만들고 그 주제에 승인된 게시물을 넣는다. 반환값은 (게시물 id, 주제 id).
fn approved_on(fx: &Fx, author: Uuid, day: u32) -> (Uuid, Uuid) {
    let topic = topic_on(fx, jiff::civil::date(2026, 4, day as i8));
    (fx.approved(author, topic), topic)
}

fn ids(value: &Value) -> Vec<String> {
    value["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// AuthorPostQueryServiceTest, AuthorPostRepositoryTest
// ---------------------------------------------------------------------------------------------

// AuthorPostQueryServiceTest#getAuthorPosts_existingAuthor_returnsPhotoAndTopic (정상·정지 작성자와 조회자)
#[test]
fn get_author_posts_existing_author_returns_photo_and_topic() {
    for status in ["ACTIVE", "BANNED"] {
        let Some(fx) = Fx::create() else { return };
        let author = fx.user();
        let viewer = fx.user();
        fx.exec(&format!(
            "UPDATE users SET status = '{status}' WHERE id IN ('{author}', '{viewer}')"
        ));
        let (post, topic) = approved_on(&fx, author, 1);

        let result = fx.query.get_author_posts(author, 1, 20, viewer).unwrap();

        assert_eq!(result.current_page, 1);
        assert_eq!(result.page_size, 20);
        assert!(!result.has_next);
        assert_eq!(result.posts.len(), 1);
        let item = &result.posts[0];
        assert_eq!(item.id, post);
        assert!(
            item.original_image_url
                .as_deref()
                .unwrap()
                .starts_with("https://cdn.test.chalkak/posts/test/original/")
        );
        assert!(
            item.thumbnail_image_url
                .as_deref()
                .unwrap()
                .starts_with("https://cdn.test.chalkak/posts/test/thumbnail/")
        );
        assert_eq!(item.topic.id, topic);
        assert_eq!(item.topic.title, "주제 2026-04-01");
        assert_eq!(item.topic.topic_date, jiff::civil::date(2026, 4, 1));
        assert_eq!(item.like_count, 0);
        assert!(!item.is_liked);
    }
}

// AuthorPostQueryServiceTest#getAuthorPosts_missingThumbnail_returnsOriginalUrlAndNullThumbnail
#[test]
fn get_author_posts_missing_thumbnail_returns_original_url_and_null_thumbnail() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let viewer = fx.user();
    let (post, topic_id) = approved_on(&fx, author, 1);
    let _ = topic_id;
    fx.exec(&format!(
        "UPDATE photos SET thumbnail_storage_key = NULL WHERE id = (SELECT photo_id FROM posts WHERE id = '{post}')"
    ));

    let result = fx.query.get_author_posts(author, 1, 20, viewer).unwrap();

    assert_eq!(result.posts[0].id, post);
    assert!(result.posts[0].original_image_url.is_some());
    assert_eq!(result.posts[0].thumbnail_image_url, None);
}

// AuthorPostRepositoryTest#findVisibleRecentByAuthorId_boundaryCount_returnsLatestTopics
#[test]
fn find_visible_recent_by_author_id_boundary_count_returns_latest_topics() {
    for count in [19u32, 20, 21] {
        let Some(fx) = Fx::create() else { return };
        let author = fx.user();
        let viewer = fx.user();
        // 제출(created_at) 순서와 주제 날짜 순서가 반대여도 주제 날짜 최신순이다.
        let mut expected = Vec::new();
        for day in (1..=count).rev() {
            let (post, _) = approved_on(&fx, author, day);
            fx.exec(&format!(
                "UPDATE posts SET created_at = TIMESTAMPTZ '2026-04-01T00:00:00Z' - make_interval(secs => {day}) WHERE id = '{post}'"
            ));
            expected.push(post);
        }

        let result = fx.query.get_author_posts(author, 1, 20, viewer).unwrap();

        let take = count.min(20) as usize;
        assert_eq!(
            result.posts.iter().map(|p| p.id).collect::<Vec<_>>(),
            expected[..take].to_vec(),
            "count={count}"
        );
        assert_eq!(result.has_next, count > 20);
    }
}

// AuthorPostRepositoryTest#findVisibleRecentByAuthorId_otherAuthorAndUnapprovedPosts_excludesBoth
#[test]
fn find_visible_recent_by_author_id_other_author_and_unapproved_posts_excludes_both() {
    for status in ["VALIDATING", "PENDING", "REJECTED"] {
        let Some(fx) = Fx::create() else { return };
        let author = fx.user();
        let other = fx.user();
        let (approved, _) = approved_on(&fx, author, 1);
        let topic2 = topic_on(&fx, jiff::civil::date(2026, 4, 2));
        fx.post(author, topic2, status, &at(-60));
        approved_on(&fx, other, 3);

        // 본인 조회에도 승인 전 사진은 제외한다.
        let result = fx.query.get_author_posts(author, 1, 20, author).unwrap();

        assert_eq!(
            result.posts.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![approved],
            "{status}"
        );
    }
}

// AuthorPostRepositoryTest#findVisibleRecentByAuthorId_deletedResource_returnsEmpty
#[test]
fn find_visible_recent_by_author_id_deleted_resource_returns_empty() {
    for deleted in ["post", "topic", "photo"] {
        let Some(fx) = Fx::create() else { return };
        let author = fx.user();
        let viewer = fx.user();
        let (post, topic) = approved_on(&fx, author, 1);
        match deleted {
            "post" => fx.soft_delete("posts", post),
            "topic" => fx.soft_delete("topics", topic),
            _ => fx.exec(&format!(
                "UPDATE photos SET deleted_at = TIMESTAMPTZ '2026-10-12T00:00:00Z' WHERE id = (SELECT photo_id FROM posts WHERE id = '{post}')"
            )),
        }

        let result = fx.query.get_author_posts(author, 1, 20, viewer).unwrap();

        assert!(result.posts.is_empty(), "{deleted}");
        assert_eq!(result.current_page, 1);
    }
}

// AuthorPostQueryServiceTest#getAuthorPosts_missingOrWithdrawnAuthor_throwsNotFound
#[test]
fn get_author_posts_missing_or_withdrawn_author_throws_not_found() {
    let Some(fx) = Fx::create() else { return };
    let viewer = fx.user();
    let withdrawn = fx.user();
    approved_on(&fx, withdrawn, 1);
    fx.delete_user(withdrawn);
    assert_err(
        fx.query.get_author_posts(Uuid::new_v4(), 1, 20, viewer),
        404,
        AUTHOR_NOT_FOUND,
    );
    assert_err(
        fx.query.get_author_posts(withdrawn, 1, 20, viewer),
        404,
        AUTHOR_NOT_FOUND,
    );
}

// AuthorPostQueryServiceTest#getAuthorPosts_missingViewer_throwsUnauthorized
#[test]
fn get_author_posts_missing_viewer_throws_unauthorized() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    assert_err(
        fx.query.get_author_posts(author, 1, 20, Uuid::new_v4()),
        401,
        UNAUTHORIZED,
    );
    let withdrawn = fx.user();
    fx.delete_user(withdrawn);
    assert_err(
        fx.query.get_author_posts(author, 1, 20, withdrawn),
        401,
        UNAUTHORIZED,
    );
}

// AuthorPostQueryServiceTest#getAuthorPosts_twentyOnePhotosAcrossYear_returnsAllWithoutDuplicates
#[test]
fn get_author_posts_twenty_one_photos_across_year_returns_all_without_duplicates() {
    for page_size in [1, 2, 5, 20, 21, 100] {
        let Some(fx) = Fx::create() else { return };
        let author = fx.user();
        let viewer = fx.user();
        let mut expected = Vec::new();
        // 2026-01-01부터 21일치: 월 경계를 넘는다.
        for offset in (0..21).rev() {
            let date = jiff::civil::date(2026, 1, 1) + jiff::Span::new().days(offset);
            let topic = topic_on(&fx, date);
            expected.push(fx.approved(author, topic));
        }
        let mut actual = Vec::new();
        let mut page = 1;
        loop {
            let result = fx
                .query
                .get_author_posts(author, page, page_size, viewer)
                .unwrap();
            assert!(result.posts.len() <= page_size as usize);
            assert_eq!(result.current_page, page);
            assert_eq!(result.page_size, page_size);
            actual.extend(result.posts.iter().map(|p| p.id));
            if !result.has_next {
                break;
            }
            assert_eq!(result.posts.len(), page_size as usize);
            page += 1;
        }
        assert_eq!(actual, expected, "pageSize={page_size}");
        assert_eq!(page, (21 + page_size - 1) / page_size);
    }
}

// AuthorPostQueryServiceTest#getAuthorPosts_requestedPageSize_limitsReturnedPosts (요청한 pageSize 전체)
#[test]
fn get_author_posts_requested_page_size_limits_returned_posts() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let viewer = fx.user();
    for day in 1..=7 {
        approved_on(&fx, author, day);
    }
    for (page_size, expected_len, has_next) in [
        (1, 1, true),
        (2, 2, true),
        (5, 5, true),
        (6, 6, true),
        (7, 7, false),
        (20, 7, false),
        (100, 7, false),
    ] {
        let result = fx
            .query
            .get_author_posts(author, 1, page_size, viewer)
            .unwrap();
        assert_eq!(result.posts.len(), expected_len, "pageSize={page_size}");
        assert_eq!(result.page_size, page_size);
        assert_eq!(result.has_next, has_next, "pageSize={page_size}");
    }
}

// AuthorPostQueryServiceTest#getAuthorPosts_noExtraPhoto_returnsEndMetadata, _deletedPhotos_returnsCurrentOffsetWithoutSnapshot
#[test]
fn get_author_posts_past_end_keeps_requested_page_metadata() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let viewer = fx.user();
    for day in 1..=3 {
        approved_on(&fx, author, day);
    }
    let empty = fx.query.get_author_posts(author, 3, 2, viewer).unwrap();
    assert!(empty.posts.is_empty());
    assert_eq!(empty.current_page, 3);
    assert_eq!(empty.page_size, 2);
    assert!(!empty.has_next);
    let exact = fx.query.get_author_posts(author, 1, 3, viewer).unwrap();
    assert_eq!(exact.posts.len(), 3);
    assert!(!exact.has_next);
    let first = fx.query.get_author_posts(author, 1, 2, viewer).unwrap();
    assert!(first.has_next);
}

// AuthorPostQueryServiceTest#getAuthorPosts_lateApproval_returnsCurrentPageOrder, _deletedPhotos_returnsCurrentOffsetWithoutSnapshot
#[test]
fn get_author_posts_reflects_current_state_on_every_request() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let viewer = fx.user();
    let posts: Vec<_> = (1..=4).map(|day| approved_on(&fx, author, day).0).collect();
    // 최신 순서: day4, day3, day2, day1
    let first = fx.query.get_author_posts(author, 1, 2, viewer).unwrap();
    assert_eq!(
        first.posts.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![posts[3], posts[2]]
    );
    // 첫 쪽을 본 뒤 가장 최신 사진이 삭제되면 다음 쪽에서 한 장이 건너뛰어진다(스냅샷 없음).
    fx.soft_delete("posts", posts[3]);
    let second = fx.query.get_author_posts(author, 2, 2, viewer).unwrap();
    assert_eq!(
        second.posts.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![posts[0]]
    );
    assert!(!second.has_next);
    let refreshed = fx.query.get_author_posts(author, 1, 2, viewer).unwrap();
    assert_eq!(
        refreshed.posts.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![posts[2], posts[1]]
    );
}

// AuthorPostQueryServiceTest#getAuthorPosts_likesAcrossPages_returnsViewerState, _likeCancelled_returnsCurrentLikes
#[test]
fn get_author_posts_likes_across_pages_returns_viewer_state() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let viewer = fx.user();
    let other = fx.user();
    let posts: Vec<_> = (1..=3).map(|day| approved_on(&fx, author, day).0).collect();
    // 최신순: posts[2], posts[1], posts[0]
    fx.like(posts[2], viewer);
    fx.like(posts[2], other);
    fx.like(posts[1], other);

    let first = fx.query.get_author_posts(author, 1, 2, viewer).unwrap();
    let second = fx.query.get_author_posts(author, 2, 2, viewer).unwrap();
    let own = fx.query.get_author_posts(author, 1, 3, author).unwrap();

    assert_eq!(
        first.posts.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![posts[2], posts[1]]
    );
    assert_eq!(
        first.posts.iter().map(|p| p.like_count).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(
        first.posts.iter().map(|p| p.is_liked).collect::<Vec<_>>(),
        vec![true, false]
    );
    assert!(first.has_next);
    assert_eq!(second.posts.len(), 1);
    assert_eq!(second.posts[0].id, posts[0]);
    assert_eq!(second.posts[0].like_count, 0);
    assert!(!second.posts[0].is_liked);
    // 작성자 본인이 조회해도 isLiked는 조회자 본인의 좋아요 여부다.
    assert!(own.posts.iter().all(|p| !p.is_liked));

    // 좋아요 취소 후 다시 조회하면 현재 값을 반영한다.
    fx.exec(&format!(
        "DELETE FROM post_likes WHERE post_id = '{}' AND user_id = '{viewer}'",
        posts[2]
    ));
    let after = fx.query.get_author_posts(author, 1, 2, viewer).unwrap();
    assert_eq!(after.posts[0].like_count, 1);
    assert!(!after.posts[0].is_liked);
}

// ---------------------------------------------------------------------------------------------
// UserControllerTest#getAuthorPosts_*
// ---------------------------------------------------------------------------------------------

// UserControllerTest#getAuthorPosts_validAuthor_returnsPhotoAndLikes: 응답 필드와 순서
#[test]
fn get_author_posts_valid_author_returns_photo_and_likes_in_fixed_field_order() {
    let Some(http) = http() else { return };
    let fx = &http.fx;
    let author = fx.user();
    let viewer = fx.user();
    let (post, topic) = approved_on(fx, author, 1);
    fx.like(post, viewer);
    fx.like(post, author);
    let photo: Uuid = fx
        .client()
        .query_one(
            &format!("SELECT photo_id FROM posts WHERE id = '{post}'"),
            &[],
        )
        .unwrap()
        .get(0);

    let (response, body) = http.author_posts(&author.to_string(), &[], Some(viewer));

    assert_eq!(response.status, 200);
    let expected = format!(
        concat!(
            r#"{{"currentPage":1,"pageSize":20,"hasNext":false,"posts":[{{"id":"{post}","#,
            r#""originalImageUrl":"https://cdn.test.chalkak/posts/test/original/{photo}.webp","#,
            r#""thumbnailImageUrl":"https://cdn.test.chalkak/posts/test/thumbnail/{photo}.webp","#,
            r#""topic":{{"id":"{topic}","title":"주제 2026-04-01","topicDate":"2026-04-01"}},"#,
            r#""likeCount":2,"isLiked":true}}]}}"#
        ),
        post = post,
        photo = photo,
        topic = topic
    );
    assert_eq!(String::from_utf8(response.body).unwrap(), expected);
    assert_eq!(body["posts"][0]["likeCount"], 2);
}

// UserControllerTest#getAuthorPosts_secondPage_returnsPaginationMetadata, _validPageSize_passesRequestedSize
#[test]
fn get_author_posts_second_page_returns_pagination_metadata() {
    let Some(http) = http() else { return };
    let fx = &http.fx;
    let author = fx.user();
    let viewer = fx.user();
    for day in 1..=3 {
        approved_on(fx, author, day);
    }
    let (_, body) = http.author_posts(
        &author.to_string(),
        &[("page", "2"), ("pageSize", "2")],
        Some(viewer),
    );
    assert_eq!(body["currentPage"], 2);
    assert_eq!(body["pageSize"], 2);
    assert_eq!(body["hasNext"], false);
    assert_eq!(ids(&body).len(), 1);
    for size in ["1", "100"] {
        let (response, body) =
            http.author_posts(&author.to_string(), &[("pageSize", size)], Some(viewer));
        assert_eq!(response.status, 200, "{size}");
        assert_eq!(body["pageSize"], size.parse::<i64>().unwrap());
    }
    // 빈 값은 기본값이다.
    let (response, body) = http.author_posts(
        &author.to_string(),
        &[("page", ""), ("pageSize", "")],
        Some(viewer),
    );
    assert_eq!(response.status, 200);
    assert_eq!(body["currentPage"], 1);
    assert_eq!(body["pageSize"], 20);
}

// UserControllerTest#getAuthorPosts_invalidPage_returnsBadRequest, _invalidPageSize_returnsBadRequest
#[test]
fn get_author_posts_invalid_page_or_page_size_returns_bad_request() {
    let Some(http) = http() else { return };
    let fx = &http.fx;
    let author = fx.user();
    let viewer = fx.user();
    let id = author.to_string();
    for (name, value) in [
        ("page", "0"),
        ("page", "-1"),
        ("pageSize", "0"),
        ("pageSize", "101"),
        ("pageSize", "-5"),
    ] {
        let (response, body) = http.author_posts(&id, &[(name, value)], Some(viewer));
        assert_eq!(response.status, 400, "{name}={value}");
        assert_eq!(body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(body["message"], "조회 조건이 올바르지 않습니다.");
    }
    for (name, value) in [("page", "abc"), ("pageSize", "x"), ("page", "1.5")] {
        let (response, body) = http.author_posts(&id, &[(name, value)], Some(viewer));
        assert_eq!(response.status, 400, "{name}={value}");
        assert_eq!(body["errorCode"], "BUSINESS_ERROR");
    }
}

// UserControllerTest#getAuthorPosts_invalidAuthorId_returnsBadRequest
#[test]
fn get_author_posts_invalid_author_id_returns_bad_request() {
    let Some(http) = http() else { return };
    let viewer = http.fx.user();
    for author in [
        "not-a-uuid",
        "me",
        "1-2-3-4-5",
        "01990000000070008000000000a1",
    ] {
        let (response, body) = http.author_posts(author, &[], Some(viewer));
        assert_eq!(response.status, 400, "{author}");
        assert_eq!(body["errorCode"], "BUSINESS_ERROR");
        assert_eq!(body["message"], "ID 형식이 올바르지 않습니다.");
    }
    // 쿼리 검증이 경로 변수 해석보다 먼저다.
    let (_, body) = http.author_posts("not-a-uuid", &[("page", "0")], Some(viewer));
    assert_eq!(body["message"], "조회 조건이 올바르지 않습니다.");
}

// UserControllerTest#getAuthorPosts_missingAuthor_returnsNotFound, _invalidViewer_returnsUnauthorized
#[test]
fn get_author_posts_missing_author_and_invalid_viewer() {
    let Some(http) = http() else { return };
    let fx = &http.fx;
    let viewer = fx.user();
    let author = fx.user();
    let (response, body) = http.author_posts(&Uuid::new_v4().to_string(), &[], Some(viewer));
    assert_eq!(response.status, 404);
    assert_eq!(
        body,
        json!({"errorCode": "BUSINESS_ERROR", "message": AUTHOR_NOT_FOUND})
    );

    let (response, body) = http.author_posts(&author.to_string(), &[], Some(Uuid::new_v4()));
    assert_eq!(response.status, 401);
    assert_eq!(body["errorCode"], "UNAUTHORIZED");
    assert_eq!(body["message"], UNAUTHORIZED);

    // 정지 조회자는 허용한다(@RequiresExistingUser).
    fx.exec(&format!(
        "UPDATE users SET status = 'BANNED' WHERE id = '{viewer}'"
    ));
    let (response, _) = http.author_posts(&author.to_string(), &[], Some(viewer));
    assert_eq!(response.status, 200);

    // 탈퇴한 조회자는 401이다.
    fx.delete_user(viewer);
    let (response, _) = http.author_posts(&author.to_string(), &[], Some(viewer));
    assert_eq!(response.status, 401);
}

// POST-012: 목록에서 중복 id가 없고 오프셋이 int 범위를 넘으면 서버 오류(Spring Data의 오프셋 변환 실패).
#[test]
fn get_author_posts_offset_beyond_int_range_fails_like_spring_data() {
    let Some(http) = http() else { return };
    let fx = &http.fx;
    let viewer = fx.user();
    let author = fx.user();
    let (response, _) = http.author_posts(
        &author.to_string(),
        &[("page", "2147483647"), ("pageSize", "100")],
        Some(viewer),
    );
    assert_eq!(response.status, 500);
    let (response, _) = http.author_posts(
        &author.to_string(),
        &[("page", "2147483647"), ("pageSize", "1")],
        Some(viewer),
    );
    assert_eq!(response.status, 200);
}
