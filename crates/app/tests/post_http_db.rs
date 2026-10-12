//! 게시물·좋아요·이미지 처리 콜백 엔드포인트의 요청·응답 통합 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PostControllerTest`, `PostLikeControllerTest`, `PostImageProcessingCallbackControllerTest`를 옮겼다. 원본은
//! 서비스를 목으로 바꾼 웹 슬라이스 테스트였지만, 여기서는 실제 서비스와 데이터베이스를 쓰고 응답에 드러나는 결과(상태
//! 코드, 본문, 반영된 행)를 확인한다. 인증 필터는 앞단 조립의 몫이라 로그인 회원은 `security::enter`로 정한다.

mod support;

use chalkak_app::post::callback::{SIGNATURE_HEADER, TIMESTAMP_HEADER};
use chalkak_app::register_routes;
use chalkak_app::security::{self, AUTHORITY_USER, Principal};
use chalkak_app::web::Routes;
use chalkak_core::Clock;
use chalkak_http::{Handler, Method, Request, Response, Router};
use serde_json::Value;
use support::{Fx, at};
use uuid::Uuid;

struct Http {
    fx: Fx,
    router: Router,
}

fn http() -> Option<Http> {
    let fx = Fx::create()?;
    let router = register_routes(Routes::new(fx.ctx.logger.clone()), &fx.ctx).into_router();
    Some(Http { fx, router })
}

struct Call<'a> {
    method: Method,
    path: String,
    query: Vec<(&'a str, &'a str)>,
    body: Option<String>,
    user: Option<Uuid>,
}

impl Http {
    fn send(&self, call: Call<'_>) -> (Response, Value) {
        let mut req = Request::new(call.method, call.path);
        req.query = call
            .query
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        if let Some(body) = call.body {
            req.body = body.into_bytes();
            req.headers.append("Content-Type", "application/json");
        }
        let _guard = call.user.map(|id| {
            security::enter(Principal {
                subject: id.to_string(),
                authorities: vec![AUTHORITY_USER.to_owned()],
            })
        });
        let response = self.router.handle(req);
        let json = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        (response, json)
    }

    fn get(&self, path: &str, query: &[(&str, &str)], user: Option<Uuid>) -> (Response, Value) {
        self.send(Call {
            method: Method::Get,
            path: path.to_owned(),
            query: query.to_vec(),
            body: None,
            user,
        })
    }

    fn with_body(
        &self,
        method: Method,
        path: &str,
        body: &str,
        user: Option<Uuid>,
    ) -> (Response, Value) {
        self.send(Call {
            method,
            path: path.to_owned(),
            query: Vec::new(),
            body: Some(body.to_owned()),
            user,
        })
    }

    fn no_body(&self, method: Method, path: &str, user: Option<Uuid>) -> (Response, Value) {
        self.send(Call {
            method,
            path: path.to_owned(),
            query: Vec::new(),
            body: None,
            user,
        })
    }

    /// 서명한 콜백 요청.
    fn callback(&self, upload: &str, result: &str, body: Option<&str>) -> (Response, Value) {
        let path = format!("/internal/v1/post-image-processing/{upload}/{result}");
        let authenticator = self.fx.ctx.post.authenticator(&self.fx.ctx);
        let timestamp = self.fx.clock.now().as_second().to_string();
        let signature = authenticator.sign(&timestamp, &path, body.map(str::as_bytes));
        let mut req = Request::new(Method::Post, path);
        req.headers.append(TIMESTAMP_HEADER, timestamp);
        req.headers.append(SIGNATURE_HEADER, signature);
        if let Some(body) = body {
            req.body = body.as_bytes().to_vec();
            req.headers.append("Content-Type", "application/json");
        }
        let response = self.router.handle(req);
        let json = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        (response, json)
    }
}

fn error_code(json: &Value) -> &str {
    json["errorCode"].as_str().unwrap_or("")
}

// ---------------------------------------------------------------------------------------------
// PostControllerTest: 업로드 발급·생성
// ---------------------------------------------------------------------------------------------

// PostControllerTest#createPostImageUpload_authenticatedUser_returnsIssuedUpload
#[test]
fn create_post_image_upload_authenticated_user_returns_issued_upload() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, json) = h.no_body(Method::Post, "/api/v1/posts/uploads", Some(user));
    assert_eq!(response.status, 200);
    assert!(json["uploadId"].as_str().unwrap().parse::<Uuid>().is_ok());
    assert!(
        json["uploadUrl"]
            .as_str()
            .unwrap()
            .contains("X-Amz-Expires=300")
    );
    assert_eq!(json["expiresInSeconds"], 300);
    assert_eq!(json["contentType"], "image/webp");
    assert_eq!(json["maxBytes"], 5_242_880);
}

// PostControllerTest#createPostImageUpload_unauthenticated_returnsUnauthorized
#[test]
fn create_post_image_upload_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, json) = h.no_body(Method::Post, "/api/v1/posts/uploads", None);
    assert_eq!(response.status, 401);
    assert_eq!(error_code(&json), "UNAUTHORIZED");
}

fn create_body(topic: Uuid, upload: Uuid, title: Option<&str>) -> String {
    match title {
        Some(t) => format!(r#"{{"topicId":"{topic}","photoUploadId":"{upload}","title":"{t}"}}"#),
        None => format!(r#"{{"topicId":"{topic}","photoUploadId":"{upload}"}}"#),
    }
}

// PostControllerTest#createPost_validRequest_returnsCreatedPost
#[test]
fn create_post_valid_request_returns_created_post() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let upload = h.fx.upload_in(user, "READY");
    let (response, json) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, upload, Some("제목")),
        Some(user),
    );
    assert_eq!(response.status, 201);
    assert!(json["postId"].as_str().unwrap().parse::<Uuid>().is_ok());
    assert_eq!(json["moderationStatus"], "PENDING");
}

// PostControllerTest#createPost_unauthenticated_returnsUnauthorized
#[test]
fn create_post_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let topic = h.fx.open_topic();
    let (response, _) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, Uuid::new_v4(), None),
        None,
    );
    assert_eq!(response.status, 401);
}

// PostControllerTest#createPost_missingTopicId_returnsBadRequest
#[test]
fn create_post_missing_topic_id_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let body = format!(r#"{{"photoUploadId":"{}"}}"#, Uuid::new_v4());
    let (response, json) = h.with_body(Method::Post, "/api/v1/posts", &body, Some(user));
    assert_eq!(response.status, 400);
    assert_eq!(error_code(&json), "BUSINESS_ERROR");
}

// PostControllerTest#createPost_missingPhotoUploadId_returnsBadRequest
#[test]
fn create_post_missing_photo_upload_id_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let body = format!(r#"{{"topicId":"{}"}}"#, Uuid::new_v4());
    let (response, _) = h.with_body(Method::Post, "/api/v1/posts", &body, Some(user));
    assert_eq!(response.status, 400);
}

// PostControllerTest#createPost_tooLongTitle_returnsBadRequest
#[test]
fn create_post_too_long_title_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let upload = h.fx.upload_in(user, "READY");
    let (response, json) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, upload, Some("열한글자제목입니다만이렇게")),
        Some(user),
    );
    assert_eq!(response.status, 400);
    assert_eq!(json["message"], "제목은 10자 이하여야 합니다.");
}

// PostControllerTest#createPost_tooLongNormalizedTitle_returnsBadRequest
#[test]
fn create_post_too_long_normalized_title_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let upload = h.fx.upload_in(user, "READY");
    let (response, _) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, upload, Some("  열한글자제목입니다만이  ")),
        Some(user),
    );
    assert_eq!(response.status, 400);
}

// PostControllerTest#createPost_tenCharacterNormalizedTitle_returnsCreatedPost
#[test]
fn create_post_ten_character_normalized_title_returns_created_post() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let upload = h.fx.upload_in(user, "READY");
    let (response, json) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, upload, Some("  열글자제목입니다만  ")),
        Some(user),
    );
    assert_eq!(response.status, 201, "{json}");
    let stored = h.fx.text(&format!(
        "SELECT title FROM posts WHERE id = '{}'",
        json["postId"].as_str().unwrap()
    ));
    assert_eq!(stored.as_deref(), Some("열글자제목입니다만"));
}

// PostControllerTest#createPost_tenCodePointEmojiTitle_returnsCreatedPost
#[test]
fn create_post_ten_code_point_emoji_title_returns_created_post() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let upload = h.fx.upload_in(user, "READY");
    let title = "😀".repeat(10);
    let (response, _) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, upload, Some(&title)),
        Some(user),
    );
    assert_eq!(response.status, 201);
}

// PostControllerTest#createPost_longBlankTitle_passesRequestValidation
#[test]
fn create_post_long_blank_title_passes_request_validation() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let upload = h.fx.upload_in(user, "READY");
    let blank = "\u{3000}".repeat(30);
    let (response, json) = h.with_body(
        Method::Post,
        "/api/v1/posts",
        &create_body(topic, upload, Some(&blank)),
        Some(user),
    );
    assert_eq!(response.status, 201, "{json}");
    let stored = h.fx.text(&format!(
        "SELECT title FROM posts WHERE id = '{}'",
        json["postId"].as_str().unwrap()
    ));
    assert_eq!(stored, None);
}

// ---------------------------------------------------------------------------------------------
// PostControllerTest: 수정·삭제
// ---------------------------------------------------------------------------------------------

fn approved_post(h: &Http) -> (Uuid, Uuid, Uuid) {
    let user = h.fx.user();
    let topic = h.fx.open_topic();
    let post = h.fx.approved(user, topic);
    (user, topic, post)
}

// PostControllerTest#updatePost_validRequest_returnsUpdatedPost
#[test]
fn update_post_valid_request_returns_updated_post() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, json) = h.with_body(
        Method::Put,
        &format!("/api/v1/posts/{post}"),
        r#"{"title":"새 제목"}"#,
        Some(user),
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["postId"], post.to_string());
    assert_eq!(json["title"], "새 제목");
}

// PostControllerTest#updatePost_nullTitle_returnsDeletedTitle
#[test]
fn update_post_null_title_returns_deleted_title() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, json) = h.with_body(
        Method::Put,
        &format!("/api/v1/posts/{post}"),
        r#"{"title":null}"#,
        Some(user),
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["title"], Value::Null);
}

// PostControllerTest#updatePost_missingTitle_returnsBadRequest
#[test]
fn update_post_missing_title_returns_bad_request() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, _) = h.with_body(
        Method::Put,
        &format!("/api/v1/posts/{post}"),
        "{}",
        Some(user),
    );
    assert_eq!(response.status, 400);
}

// PostControllerTest#updatePost_tooLongNormalizedTitle_returnsBadRequest
#[test]
fn update_post_too_long_normalized_title_returns_bad_request() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, _) = h.with_body(
        Method::Put,
        &format!("/api/v1/posts/{post}"),
        r#"{"title":"  열한글자제목입니다만이  "}"#,
        Some(user),
    );
    assert_eq!(response.status, 400);
}

// PostControllerTest#updatePost_invalidPostId_returnsBadRequest
#[test]
fn update_post_invalid_post_id_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, _) = h.with_body(
        Method::Put,
        "/api/v1/posts/not-a-uuid",
        r#"{"title":"x"}"#,
        Some(user),
    );
    assert_eq!(response.status, 400);
}

// PostControllerTest#updatePost_unauthenticated_returnsUnauthorized
#[test]
fn update_post_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, _) = h.with_body(
        Method::Put,
        &format!("/api/v1/posts/{}", Uuid::new_v4()),
        r#"{"title":"x"}"#,
        None,
    );
    assert_eq!(response.status, 401);
}

// PostControllerTest#deletePost_validRequest_returnsNoContent
#[test]
fn delete_post_valid_request_returns_no_content() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, _) = h.no_body(Method::Delete, &format!("/api/v1/posts/{post}"), Some(user));
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
}

// PostControllerTest#deletePost_invalidPostId_returnsBadRequest
#[test]
fn delete_post_invalid_post_id_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, _) = h.no_body(Method::Delete, "/api/v1/posts/abc", Some(user));
    assert_eq!(response.status, 400);
}

// PostControllerTest#deletePost_unauthenticated_returnsUnauthorized
#[test]
fn delete_post_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, _) = h.no_body(
        Method::Delete,
        &format!("/api/v1/posts/{}", Uuid::new_v4()),
        None,
    );
    assert_eq!(response.status, 401);
}

// ---------------------------------------------------------------------------------------------
// PostControllerTest: 오늘 상태·캘린더
// ---------------------------------------------------------------------------------------------

// PostControllerTest#getMyTodayPostStatus_posted_returnsPostedStatus
#[test]
fn get_my_today_post_status_posted_returns_posted_status() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, json) = h.get("/api/v1/posts/today", &[], Some(user));
    assert_eq!(response.status, 200);
    assert_eq!(json["topicDate"], "2026-10-12");
    assert_eq!(json["isPosted"], true);
    assert_eq!(json["postId"], post.to_string());
    assert_eq!(json["moderationStatus"], "APPROVED");
}

// PostControllerTest#getMyTodayPostStatus_notPosted_returnsNullPostFields
#[test]
fn get_my_today_post_status_not_posted_returns_null_post_fields() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    h.fx.open_topic();
    let (response, json) = h.get("/api/v1/posts/today", &[], Some(user));
    assert_eq!(response.status, 200);
    assert_eq!(json["isPosted"], false);
    assert_eq!(json["postId"], Value::Null);
    assert_eq!(json["moderationStatus"], Value::Null);
}

// PostControllerTest#getMyTodayPostStatus_noOpenTopic_returnsNotFound
#[test]
fn get_my_today_post_status_no_open_topic_returns_not_found() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, json) = h.get("/api/v1/posts/today", &[], Some(user));
    assert_eq!(response.status, 404);
    assert_eq!(json["message"], "참여할 수 있는 주제가 없습니다.");
}

// PostControllerTest#getMyTodayPostStatus_unauthenticated_returnsUnauthorized
#[test]
fn get_my_today_post_status_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, _) = h.get("/api/v1/posts/today", &[], None);
    assert_eq!(response.status, 401);
}

// PostControllerTest#getMyPostCalendar_supportedStatus_returnsCalendar
#[test]
fn get_my_post_calendar_supported_status_returns_calendar() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    let (response, json) = h.get(
        "/api/v1/posts/calendar",
        &[("year", "2026"), ("month", "10")],
        Some(user),
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["year"], 2026);
    assert_eq!(json["month"], 10);
    assert_eq!(json["posts"][0]["postId"], post.to_string());
    assert_eq!(json["posts"][0]["status"], "APPROVED");
    assert_eq!(json["posts"][0]["topicDate"], "2026-10-12");
}

// PostControllerTest#getMyPostCalendar_noPosts_returnsEmptyResults
#[test]
fn get_my_post_calendar_no_posts_returns_empty_results() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, json) = h.get(
        "/api/v1/posts/calendar",
        &[("year", "2026"), ("month", "9")],
        Some(user),
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["posts"], serde_json::json!([]));
}

// PostControllerTest#getMyPostCalendar_missingYearMonth_returnsBadRequest
#[test]
fn get_my_post_calendar_missing_year_month_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    assert_eq!(
        h.get("/api/v1/posts/calendar", &[], Some(user)).0.status,
        400
    );
    assert_eq!(
        h.get("/api/v1/posts/calendar", &[("year", "2026")], Some(user))
            .0
            .status,
        400
    );
}

// PostControllerTest#getMyPostCalendar_invalidYear_returnsBadRequest
#[test]
fn get_my_post_calendar_invalid_year_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    for year in ["0", "10000", "-1"] {
        let (response, _) = h.get(
            "/api/v1/posts/calendar",
            &[("year", year), ("month", "1")],
            Some(user),
        );
        assert_eq!(response.status, 400, "{year}");
    }
}

// PostControllerTest#getMyPostCalendar_invalidMonth_returnsBadRequest
#[test]
fn get_my_post_calendar_invalid_month_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    for month in ["0", "13"] {
        let (response, _) = h.get(
            "/api/v1/posts/calendar",
            &[("year", "2026"), ("month", month)],
            Some(user),
        );
        assert_eq!(response.status, 400, "{month}");
    }
}

// PostControllerTest#getMyPostCalendar_unauthenticated_returnsUnauthorized
#[test]
fn get_my_post_calendar_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, _) = h.get(
        "/api/v1/posts/calendar",
        &[("year", "2026"), ("month", "10")],
        None,
    );
    assert_eq!(response.status, 401);
}

// PostControllerTest#getMyPostCalendarMonths_withoutYearMonth_returnsMonths
#[test]
fn get_my_post_calendar_months_without_year_month_returns_months() {
    let Some(h) = http() else { return };
    let (user, _, _) = approved_post(&h);
    let (response, json) = h.get("/api/v1/posts/calendar/months", &[], Some(user));
    assert_eq!(response.status, 200);
    assert_eq!(
        json,
        serde_json::json!({"months": [{"year": 2026, "month": 10}]})
    );
}

// PostControllerTest#getMyPostCalendarMonths_noPosts_returnsEmptyArray
#[test]
fn get_my_post_calendar_months_no_posts_returns_empty_array() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, json) = h.get("/api/v1/posts/calendar/months", &[], Some(user));
    assert_eq!(response.status, 200);
    assert_eq!(json, serde_json::json!({"months": []}));
}

// ---------------------------------------------------------------------------------------------
// PostControllerTest: 상세·목록
// ---------------------------------------------------------------------------------------------

// PostControllerTest#getPost_validPostId_returnsPostDetail
#[test]
fn get_post_valid_post_id_returns_post_detail() {
    let Some(h) = http() else { return };
    let (user, topic, post) = approved_post(&h);
    let (response, json) = h.get(&format!("/api/v1/posts/{post}"), &[], Some(user));
    assert_eq!(response.status, 200);
    assert_eq!(json["id"], post.to_string());
    assert_eq!(json["topic"]["id"], topic.to_string());
    assert_eq!(json["likeCount"], 0);
    assert_eq!(json["isLiked"], false);
    assert!(
        json["originalImageUrl"]
            .as_str()
            .unwrap()
            .starts_with("https://cdn.test.chalkak/")
    );
}

// PostControllerTest#getPost_invisiblePost_returnsNotFound
#[test]
fn get_post_invisible_post_returns_not_found() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, json) = h.get(
        &format!("/api/v1/posts/{}", Uuid::new_v4()),
        &[],
        Some(user),
    );
    assert_eq!(response.status, 404);
    assert_eq!(json["message"], "게시물을 찾을 수 없습니다.");
}

// PostControllerTest#getPost_invalidPostId_returnsBadRequest
#[test]
fn get_post_invalid_post_id_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    assert_eq!(h.get("/api/v1/posts/abc", &[], Some(user)).0.status, 400);
}

// PostControllerTest#getPost_nonCanonicalUuid_returnsBadRequest
#[test]
fn get_post_non_canonical_uuid_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let (response, json) = h.get("/api/v1/posts/1-1-1-1-1", &[], Some(user));
    assert_eq!(response.status, 400);
    assert_eq!(error_code(&json), "BUSINESS_ERROR");
    assert_eq!(json["message"], "ID 형식이 올바르지 않습니다.");
}

// PostControllerTest#getPost_unauthenticated_returnsUnauthorized
#[test]
fn get_post_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, _) = h.get(&format!("/api/v1/posts/{}", Uuid::new_v4()), &[], None);
    assert_eq!(response.status, 401);
}

// PostControllerTest#getPosts_validTopicDate_returnsPostList
#[test]
fn get_posts_valid_topic_date_returns_post_list() {
    let Some(h) = http() else { return };
    let (_, _, post) = approved_post(&h);
    let (response, json) = h.get("/api/v1/posts", &[("topicDate", "2026-10-12")], None);
    assert_eq!(response.status, 200);
    assert_eq!(json["currentPage"], 1);
    assert_eq!(json["pageSize"], 20);
    assert_eq!(json["hasNext"], false);
    assert_eq!(json["randomSeed"], Value::Null);
    assert_eq!(json["posts"][0]["id"], post.to_string());
    assert_eq!(json["posts"][0]["isMine"], false);
}

// PostControllerTest#getPosts_authenticatedUser_returnsPersonalLikeStatus
#[test]
fn get_posts_authenticated_user_returns_personal_like_status() {
    let Some(h) = http() else { return };
    let (user, _, post) = approved_post(&h);
    h.fx.like(post, user);
    let (_, json) = h.get("/api/v1/posts", &[("topicDate", "2026-10-12")], Some(user));
    assert_eq!(json["posts"][0]["isLiked"], true);
    assert_eq!(json["posts"][0]["isMine"], true);
    assert_eq!(json["posts"][0]["likeCount"], 1);
}

// PostControllerTest#getPosts_popularSort_bindsPostSortEnum
#[test]
fn get_posts_popular_sort_binds_post_sort_enum() {
    let Some(h) = http() else { return };
    let (_, topic, _) = approved_post(&h);
    let loved = h.fx.post(h.fx.user(), topic, "APPROVED", &at(-7200)).0;
    h.fx.like(loved, h.fx.user());
    let (response, json) = h.get(
        "/api/v1/posts",
        &[("topicDate", "2026-10-12"), ("sort", "popular")],
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["posts"][0]["id"], loved.to_string());
}

// PostControllerTest#getPosts_randomSort_bindsPostSortEnum
#[test]
fn get_posts_random_sort_binds_post_sort_enum() {
    let Some(h) = http() else { return };
    approved_post(&h);
    let (response, json) = h.get(
        "/api/v1/posts",
        &[("topicDate", "2026-10-12"), ("sort", "random")],
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["randomSeed"].as_str().unwrap().len(), 8);
}

// PostControllerTest#getPosts_randomNextPage_passesQueryParameters
#[test]
fn get_posts_random_next_page_passes_query_parameters() {
    let Some(h) = http() else { return };
    approved_post(&h);
    let (response, json) = h.get(
        "/api/v1/posts",
        &[
            ("topicDate", "2026-10-12"),
            ("sort", "random"),
            ("randomSeed", "abcd1234"),
            ("page", "2"),
            ("pageSize", "5"),
        ],
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(json["randomSeed"], "abcd1234");
    assert_eq!(json["currentPage"], 2);
    assert_eq!(json["pageSize"], 5);
}

// PostControllerTest#getPosts_missingTopicDate_returnsBadRequest
#[test]
fn get_posts_missing_topic_date_returns_bad_request() {
    let Some(h) = http() else { return };
    let (response, json) = h.get("/api/v1/posts", &[], None);
    assert_eq!(response.status, 400);
    assert_eq!(error_code(&json), "BUSINESS_ERROR");
}

// PostControllerTest#getPosts_invalidTopicDate_returnsBadRequest
#[test]
fn get_posts_invalid_topic_date_returns_bad_request() {
    let Some(h) = http() else { return };
    assert_eq!(
        h.get("/api/v1/posts", &[("topicDate", "2026-13-99")], None)
            .0
            .status,
        400
    );
}

// PostControllerTest#getPosts_invalidPagination_returnsBadRequest
#[test]
fn get_posts_invalid_pagination_returns_bad_request() {
    let Some(h) = http() else { return };
    for (page, size) in [("0", "20"), ("1", "0"), ("1", "101")] {
        let (response, _) = h.get(
            "/api/v1/posts",
            &[
                ("topicDate", "2026-10-12"),
                ("page", page),
                ("pageSize", size),
            ],
            None,
        );
        assert_eq!(response.status, 400, "{page}/{size}");
    }
}

// PostControllerTest#getPosts_invalidQueryParameter_returnsBadRequest
#[test]
fn get_posts_invalid_query_parameter_returns_bad_request() {
    let Some(h) = http() else { return };
    for query in [("sort", "newest"), ("page", "abc"), ("pageSize", "abc")] {
        let (response, _) = h.get("/api/v1/posts", &[("topicDate", "2026-10-12"), query], None);
        assert_eq!(response.status, 400, "{query:?}");
    }
}

// PostControllerTest#getPosts_tooLongRandomSeed_returnsBadRequest
#[test]
fn get_posts_too_long_random_seed_returns_bad_request() {
    let Some(h) = http() else { return };
    let seed = "a".repeat(65);
    let (response, _) = h.get(
        "/api/v1/posts",
        &[
            ("topicDate", "2026-10-12"),
            ("sort", "random"),
            ("randomSeed", &seed),
        ],
        None,
    );
    assert_eq!(response.status, 400);
}

// ---------------------------------------------------------------------------------------------
// PostLikeControllerTest
// ---------------------------------------------------------------------------------------------

// PostLikeControllerTest#likePost_validRequest_returnsLikedResponse
#[test]
fn like_post_valid_request_returns_liked_response() {
    let Some(h) = http() else { return };
    let (_, _, post) = approved_post(&h);
    let fan = h.fx.user();
    let (response, json) = h.no_body(
        Method::Put,
        &format!("/api/v1/posts/{post}/likes"),
        Some(fan),
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        json,
        serde_json::json!({"postId": post.to_string(), "likeCount": 1, "isLiked": true})
    );
}

// PostLikeControllerTest#unlikePost_validRequest_returnsUnlikedResponse
#[test]
fn unlike_post_valid_request_returns_unliked_response() {
    let Some(h) = http() else { return };
    let (_, _, post) = approved_post(&h);
    let fan = h.fx.user();
    h.fx.like(post, fan);
    let (response, json) = h.no_body(
        Method::Delete,
        &format!("/api/v1/posts/{post}/likes"),
        Some(fan),
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        json,
        serde_json::json!({"postId": post.to_string(), "likeCount": 0, "isLiked": false})
    );
}

// PostLikeControllerTest#likePost_invalidPostId_returnsBadRequest
#[test]
fn like_post_invalid_post_id_returns_bad_request() {
    let Some(h) = http() else { return };
    let fan = h.fx.user();
    let (response, _) = h.no_body(Method::Put, "/api/v1/posts/abc/likes", Some(fan));
    assert_eq!(response.status, 400);
}

// PostLikeControllerTest#likePost_invisiblePost_returnsNotFound
#[test]
fn like_post_invisible_post_returns_not_found() {
    let Some(h) = http() else { return };
    let fan = h.fx.user();
    let (response, _) = h.no_body(
        Method::Put,
        &format!("/api/v1/posts/{}/likes", Uuid::new_v4()),
        Some(fan),
    );
    assert_eq!(response.status, 404);
}

// PostLikeControllerTest#likePost_unauthenticated_returnsUnauthorized
#[test]
fn like_post_unauthenticated_returns_unauthorized() {
    let Some(h) = http() else { return };
    let (response, _) = h.no_body(
        Method::Put,
        &format!("/api/v1/posts/{}/likes", Uuid::new_v4()),
        None,
    );
    assert_eq!(response.status, 401);
}

// ---------------------------------------------------------------------------------------------
// PostImageProcessingCallbackControllerTest
// ---------------------------------------------------------------------------------------------

// PostImageProcessingCallbackControllerTest#issueUploadUrls_authenticatedRequest_returnsPresignedUrls
#[test]
fn issue_upload_urls_authenticated_request_returns_presigned_urls() {
    let Some(h) = http() else { return };
    let upload = Uuid::new_v4();
    let (response, json) = h.callback(&upload.to_string(), "upload-urls", None);
    assert_eq!(response.status, 200);
    assert!(
        json["originalUploadUrl"]
            .as_str()
            .unwrap()
            .contains(&upload.to_string())
    );
    assert!(
        json["thumbnailUploadUrl"]
            .as_str()
            .unwrap()
            .contains("/thumbnail/")
    );
    assert_eq!(json["contentType"], "image/webp");
    assert_eq!(json["cacheControl"], "public, max-age=86400");
}

// PostImageProcessingCallbackControllerTest#issueUploadUrls_invalidSignature_returnsUnauthorized
#[test]
fn issue_upload_urls_invalid_signature_returns_unauthorized() {
    let Some(h) = http() else { return };
    let upload = Uuid::new_v4();
    let mut req = Request::new(
        Method::Post,
        format!("/internal/v1/post-image-processing/{upload}/upload-urls"),
    );
    req.headers
        .append(TIMESTAMP_HEADER, h.fx.clock.now().as_second().to_string());
    req.headers.append(SIGNATURE_HEADER, "v1=00");
    let response = h.router.handle(req);
    assert_eq!(response.status, 401);
    assert!(h.fx.store.presigns.lock().unwrap().is_empty());
}

// PostImageProcessingCallbackControllerTest#complete_authenticatedCallback_completesProcessing
#[test]
fn complete_authenticated_callback_completes_processing() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let upload = h.fx.upload_in(user, "ISSUED");
    let body = r#"{"width":4032,"height":3024,"byteSize":1024,"location":{"latitude":37.5,"longitude":127.0},"capturedAt":"2026-10-12T00:00:00Z","metaAttributes":{"make":"x"}}"#;
    let (response, _) = h.callback(&upload.to_string(), "complete", Some(body));
    assert_eq!(response.status, 204);
    assert_eq!(h.fx.upload_status(upload), "READY");
    let width = h.fx.text(&format!(
        "SELECT image_metadata->>'width' FROM post_image_uploads WHERE id = '{upload}'"
    ));
    assert_eq!(width.as_deref(), Some("4032"));
}

// PostImageProcessingCallbackControllerTest#complete_invalidSignature_returnsUnauthorized
#[test]
fn complete_invalid_signature_returns_unauthorized() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let upload = h.fx.upload_in(user, "ISSUED");
    let mut req = Request::new(
        Method::Post,
        format!("/internal/v1/post-image-processing/{upload}/complete"),
    );
    req.body = br#"{"width":1}"#.to_vec();
    req.headers
        .append(TIMESTAMP_HEADER, h.fx.clock.now().as_second().to_string());
    req.headers.append(SIGNATURE_HEADER, "v1=00");
    assert_eq!(h.router.handle(req).status, 401);
    assert_eq!(h.fx.upload_status(upload), "ISSUED");
}

// PostImageProcessingCallbackControllerTest#complete_missingHeaders_returnsUnauthorized
#[test]
fn complete_missing_headers_returns_unauthorized() {
    let Some(h) = http() else { return };
    let upload = Uuid::new_v4();
    let mut req = Request::new(
        Method::Post,
        format!("/internal/v1/post-image-processing/{upload}/complete"),
    );
    req.body = b"{}".to_vec();
    let response = h.router.handle(req);
    assert_eq!(response.status, 401);
}

// PostImageProcessingCallbackControllerTest#complete_nonCanonicalUploadId_returnsBadRequest
#[test]
fn complete_non_canonical_upload_id_returns_bad_request() {
    let Some(h) = http() else { return };
    // 서명이 틀려도 400이다: 경로 변수 해석이 인증보다 먼저다.
    let mut req = Request::new(
        Method::Post,
        "/internal/v1/post-image-processing/1-1-1-1-1/complete",
    );
    req.headers.append(TIMESTAMP_HEADER, "1787562000");
    req.headers.append(SIGNATURE_HEADER, "signature");
    req.body = b"{}".to_vec();
    assert_eq!(h.router.handle(req).status, 400);
}

// PostImageProcessingCallbackControllerTest#complete_coordinateOutOfRange_returnsBadRequest
#[test]
fn complete_coordinate_out_of_range_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let upload = h.fx.upload_in(user, "ISSUED");
    let body = r#"{"location":{"latitude":91,"longitude":0}}"#;
    let (response, _) = h.callback(&upload.to_string(), "complete", Some(body));
    assert_eq!(response.status, 400);
    assert_eq!(h.fx.upload_status(upload), "ISSUED");
}

// PostImageProcessingCallbackControllerTest#complete_oversizedCapturedAt_returnsBadRequest
#[test]
fn complete_oversized_captured_at_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let upload = h.fx.upload_in(user, "ISSUED");
    let body = format!(r#"{{"capturedAt":"{}"}}"#, "x".repeat(65));
    let (response, _) = h.callback(&upload.to_string(), "complete", Some(&body));
    assert_eq!(response.status, 400);
    assert_eq!(h.fx.upload_status(upload), "ISSUED");
}

// PostImageProcessingCallbackControllerTest#fail_authenticatedCallback_failsProcessing
#[test]
fn fail_authenticated_callback_fails_processing() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let upload = h.fx.upload_in(user, "ISSUED");
    let (response, _) = h.callback(
        &upload.to_string(),
        "failed",
        Some(r#"{"reason":"UNSUPPORTED_FORMAT"}"#),
    );
    assert_eq!(response.status, 204);
    assert_eq!(h.fx.upload_status(upload), "REJECTED");
}

// PostImageProcessingCallbackControllerTest#fail_blankReason_returnsBadRequest
#[test]
fn fail_blank_reason_returns_bad_request() {
    let Some(h) = http() else { return };
    let user = h.fx.user();
    let upload = h.fx.upload_in(user, "ISSUED");
    let (response, _) = h.callback(&upload.to_string(), "failed", Some(r#"{"reason":"   "}"#));
    assert_eq!(response.status, 400);
    assert_eq!(h.fx.upload_status(upload), "ISSUED");
}
