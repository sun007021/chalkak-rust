//! 게시물 생성·이미지 처리 콜백·업로드 발급·삭제·수정 흐름의 DB 통합 테스트.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `PostCreationServiceTest`, `PostImageProcessingServiceTest`, `PostImageUploadServiceTest`,
//! `PostDeletionServiceTest`, `PostUpdateServiceTest`, `PostModerationNotificationTransactionTest`,
//! `PostModerationPendingLogTest`를 옮겼다.

mod support;

use chalkak_app::post::domain::ModerationStatus;
use serde_json::{Map, Value};
use support::{Fx, NOW, assert_err, at, ts};
use uuid::Uuid;

fn metadata() -> Map<String, Value> {
    let Value::Object(map) = serde_json::json!({"width": 4032, "height": 3024}) else {
        unreachable!()
    };
    map
}

fn rejected_count(fx: &Fx) -> i64 {
    fx.count("SELECT count(*) FROM posts WHERE moderation_status = 'REJECTED'")
}

// ---------------------------------------------------------------------------------------------
// PostCreationServiceTest
// ---------------------------------------------------------------------------------------------

// PostCreationServiceTest#createPost_validRequest_savesPhotoAndValidatingPost
#[test]
fn create_post_valid_request_saves_photo_and_validating_post() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "ISSUED");
    fx.stage(upload);

    let result = fx
        .command
        .create_post(user, topic, upload, Some("제목"))
        .unwrap();

    assert_eq!(result.moderation_status, ModerationStatus::Validating);
    let row = fx
        .client()
        .query_one(
            "SELECT p.title, p.moderation_status::text AS s, p.moderated_at, p.post_image_upload_id, \
             ph.original_storage_key, ph.thumbnail_storage_key \
             FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = $1",
            &[&result.post_id],
        )
        .unwrap();
    assert_eq!(
        row.get::<_, Option<String>>("title").as_deref(),
        Some("제목")
    );
    assert_eq!(row.get::<_, String>("s"), "VALIDATING");
    assert!(
        row.get::<_, Option<jiff::Timestamp>>("moderated_at")
            .is_none()
    );
    assert_eq!(
        row.get::<_, Option<Uuid>>("post_image_upload_id"),
        Some(upload)
    );
    assert_eq!(
        row.get::<_, String>("original_storage_key"),
        format!("chalkak/posts/test/original/{upload}.webp")
    );
    assert!(
        row.get::<_, Option<String>>("thumbnail_storage_key")
            .is_none()
    );
    assert!(fx.events.0.lock().unwrap().is_empty());
}

// PostCreationServiceTest#createPost_validRequest_claimsUpload
#[test]
fn create_post_valid_request_claims_upload() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "ISSUED");
    fx.stage(upload);

    fx.command.create_post(user, topic, upload, None).unwrap();

    let claimed: Option<jiff::Timestamp> = fx
        .client()
        .query_one(
            "SELECT claimed_at FROM post_image_uploads WHERE id = $1",
            &[&upload],
        )
        .unwrap()
        .get(0);
    assert_eq!(claimed, Some(ts(NOW)));
}

// PostCreationServiceTest#createPost_readyUpload_savesPendingPost
#[test]
fn create_post_ready_upload_saves_pending_post() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "READY");

    let result = fx.command.create_post(user, topic, upload, None).unwrap();

    assert_eq!(result.moderation_status, ModerationStatus::Pending);
    let row = fx
        .client()
        .query_one(
            "SELECT ph.thumbnail_storage_key, ph.metadata::text AS metadata \
             FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = $1",
            &[&result.post_id],
        )
        .unwrap();
    assert_eq!(
        row.get::<_, String>("thumbnail_storage_key"),
        format!("chalkak/posts/test/thumbnail/{upload}.webp")
    );
    let stored: Value = serde_json::from_str(&row.get::<_, String>("metadata")).unwrap();
    assert_eq!(stored["width"], 4032);
    assert_eq!(fx.events.0.lock().unwrap().len(), 1);
    // READY 업로드는 staging 객체를 확인하지 않는다.
    assert!(fx.store.heads.lock().unwrap().is_empty());
}

// PostCreationServiceTest#createPost_tenCodePointEmojiTitle_savesTitle
#[test]
fn create_post_ten_code_point_emoji_title_saves_title() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "READY");
    let title = "😀".repeat(10);

    let result = fx
        .command
        .create_post(user, topic, upload, Some(&title))
        .unwrap();

    let stored = fx.text(&format!(
        "SELECT title FROM posts WHERE id = '{}'",
        result.post_id
    ));
    assert_eq!(stored.as_deref(), Some(title.as_str()));
}

// PostCreationServiceTest#createPost_unknownUser_throwsUnauthorizedException
#[test]
fn create_post_unknown_user_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    let topic = fx.open_topic();
    assert_err(
        fx.command
            .create_post(Uuid::new_v4(), topic, Uuid::new_v4(), None),
        401,
        "유효하지 않은 인증 정보입니다.",
    );
}

// PostCreationServiceTest#createPost_deletedTopic_throwsNotFoundException
#[test]
fn create_post_deleted_topic_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.delete_topic(topic);
    assert_err(
        fx.command.create_post(user, topic, Uuid::new_v4(), None),
        404,
        "게시물을 작성할 주제를 찾을 수 없습니다.",
    );
}

// PostCreationServiceTest#createPost_beforeOpenTopic_throwsBusinessException
#[test]
fn create_post_before_open_topic_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.topic("2026-10-13", "2026-10-12T15:00:00Z", "2026-10-13T15:00:00Z");
    assert_err(
        fx.command.create_post(user, topic, Uuid::new_v4(), None),
        400,
        "현재 게시물을 작성할 수 없는 주제입니다.",
    );
}

// PostCreationServiceTest#createPost_closedTopic_throwsBusinessException
#[test]
fn create_post_closed_topic_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.topic("2026-10-11", "2026-10-10T15:00:00Z", "2026-10-11T15:00:00Z");
    assert_err(
        fx.command.create_post(user, topic, Uuid::new_v4(), None),
        400,
        "현재 게시물을 작성할 수 없는 주제입니다.",
    );
}

// PostCreationServiceTest#createPost_unknownUploadId_throwsNotFoundException
#[test]
fn create_post_unknown_upload_id_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    assert_err(
        fx.command.create_post(user, topic, Uuid::new_v4(), None),
        404,
        "업로드한 사진을 찾을 수 없습니다.",
    );
}

// PostCreationServiceTest#createPost_otherUsersUploadId_throwsNotFoundException
#[test]
fn create_post_other_users_upload_id_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let other = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(other, "READY");
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        404,
        "업로드한 사진을 찾을 수 없습니다.",
    );
    assert_eq!(fx.count("SELECT count(*) FROM posts"), 0);
}

// PostCreationServiceTest#createPost_missingUploadedImage_throwsNotFoundException
#[test]
fn create_post_missing_uploaded_image_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "ISSUED");
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        404,
        "업로드한 사진을 찾을 수 없습니다.",
    );
    assert_eq!(fx.count("SELECT count(*) FROM posts"), 0);
    assert_eq!(fx.count("SELECT count(*) FROM photos"), 0);
}

// PostCreationServiceTest#createPost_expiredUpload_throwsBusinessException
#[test]
fn create_post_expired_upload_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload(user, "READY", &at(-1), None);
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "사진 업로드 유효 시간이 지났습니다.",
    );
}

// PostCreationServiceTest#createPost_rejectedUpload_throwsBusinessException
#[test]
fn create_post_rejected_upload_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "REJECTED");
    fx.stage(upload);
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "WebP 이미지만 업로드할 수 있습니다.",
    );
}

// PostCreationServiceTest#createPost_reusedPhotoUpload_throwsBusinessException
#[test]
fn create_post_reused_photo_upload_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload(user, "READY", &at(600), Some(&at(-60)));
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "이미 사용된 사진입니다.",
    );
}

// PostCreationServiceTest#createPost_orphanPhotoWithSameStorageKey_throwsBusinessException
#[test]
fn create_post_orphan_photo_with_same_storage_key_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "READY");
    fx.exec(&format!(
        "INSERT INTO photos (original_storage_key, created_at, updated_at) VALUES \
         ('chalkak/posts/test/original/{upload}.webp', TIMESTAMPTZ '{NOW}', TIMESTAMPTZ '{NOW}')"
    ));
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "이미 사용된 사진입니다.",
    );
    assert!(fx.events.0.lock().unwrap().is_empty());
}

// PostCreationServiceTest#createPost_duplicateUserAndTopic_throwsBusinessException
#[test]
fn create_post_duplicate_user_and_topic_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.post(user, topic, "APPROVED", &at(-3600));
    let upload = fx.upload_in(user, "READY");
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "이미 해당 주제에 게시물을 작성했습니다.",
    );
}

// PostCreationServiceTest#createPost_recentValidatingPost_throwsBusinessException
#[test]
fn create_post_recent_validating_post_throws_business_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (post, _) = fx.post(user, topic, "VALIDATING", &at(-6 * 60));
    let upload = fx.upload_in(user, "READY");
    assert_err(
        fx.command.create_post(user, topic, upload, None),
        400,
        "이미 해당 주제에 게시물을 작성했습니다.",
    );
    assert_eq!(fx.moderation(post), "VALIDATING");
}

// PostCreationServiceTest#createPost_stalledValidatingPost_rejectsItAndAllowsNewPost
#[test]
fn create_post_stalled_validating_post_rejects_it_and_allows_new_post() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (stalled, _) = fx.post(user, topic, "VALIDATING", &at(-8 * 60));
    let upload = fx.upload_in(user, "READY");

    let result = fx.command.create_post(user, topic, upload, None).unwrap();

    assert_eq!(fx.moderation(stalled), "REJECTED");
    assert_eq!(result.moderation_status, ModerationStatus::Pending);
    assert_ne!(result.post_id, stalled);
}

// PostCreationServiceTest#createPost_afterRejection_createsNewPost
#[test]
fn create_post_after_rejection_creates_new_post() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    fx.post(user, topic, "REJECTED", &at(-3600));
    let upload = fx.upload_in(user, "READY");

    let result = fx.command.create_post(user, topic, upload, None).unwrap();

    assert_eq!(result.moderation_status, ModerationStatus::Pending);
    assert_eq!(rejected_count(&fx), 1);
}

// PostCreationServiceTest#createPost_validatingPost_isNotVisible
#[test]
fn create_post_validating_post_is_not_visible() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "ISSUED");
    fx.stage(upload);
    let result = fx.command.create_post(user, topic, upload, None).unwrap();

    assert_err(
        fx.query.get_post(result.post_id, user),
        404,
        "게시물을 찾을 수 없습니다.",
    );
}

// 원본에는 없는 Rust 전용 확인: 실패한 생성은 알림을 보내지 않는다.
#[test]
fn create_post_failure_does_not_notify_the_listener() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload(user, "READY", &at(-1), None);
    let _ = fx.command.create_post(user, topic, upload, None);
    assert!(fx.events.0.lock().unwrap().is_empty());
    assert!(fx.log.lines().is_empty());
}

// ---------------------------------------------------------------------------------------------
// PostImageProcessingServiceTest
// ---------------------------------------------------------------------------------------------

// PostImageProcessingServiceTest#completePostImageProcessing_issuedUpload_marksReady
#[test]
fn complete_post_image_processing_issued_upload_marks_ready() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let upload = fx.upload_in(user, "ISSUED");

    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();

    assert_eq!(fx.upload_status(upload), "READY");
}

// PostImageProcessingServiceTest#completePostImageProcessing_unknownUploadId_doesNothing
#[test]
fn complete_post_image_processing_unknown_upload_id_does_nothing() {
    let Some(fx) = Fx::create() else { return };
    fx.command
        .complete_post_image_processing(Uuid::new_v4(), metadata())
        .unwrap();
    assert_eq!(fx.count("SELECT count(*) FROM post_image_uploads"), 0);
}

// PostImageProcessingServiceTest#completePostImageProcessing_withoutPost_marksReadyOnly
#[test]
fn complete_post_image_processing_without_post_marks_ready_only() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let upload = fx.upload_in(user, "ISSUED");
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    assert_eq!(fx.count("SELECT count(*) FROM posts"), 0);
    assert!(fx.events.0.lock().unwrap().is_empty());
}

fn validating_post_with_upload(fx: &Fx) -> (Uuid, Uuid) {
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "ISSUED");
    fx.stage(upload);
    let result = fx.command.create_post(user, topic, upload, None).unwrap();
    (result.post_id, upload)
}

// PostImageProcessingServiceTest#completePostImageProcessing_validatingPost_requestsModeration
#[test]
fn complete_post_image_processing_validating_post_requests_moderation() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);

    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();

    assert_eq!(fx.moderation(post), "PENDING");
    let thumbnail = fx.text(&format!(
        "SELECT ph.thumbnail_storage_key FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = '{post}'"
    ));
    assert_eq!(
        thumbnail,
        Some(format!("chalkak/posts/test/thumbnail/{upload}.webp"))
    );
    assert_eq!(fx.events.0.lock().unwrap().len(), 1);
}

// PostImageProcessingServiceTest#completePostImageProcessing_duplicateCallback_staysPending
#[test]
fn complete_post_image_processing_duplicate_callback_stays_pending() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    assert_eq!(fx.moderation(post), "PENDING");
}

// PostImageProcessingServiceTest#completePostImageProcessing_rejectedUpload_keepsPostRejected
#[test]
fn complete_post_image_processing_rejected_upload_keeps_post_rejected() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    fx.command
        .fail_post_image_processing(upload, Some("UNSUPPORTED_FORMAT"))
        .unwrap();
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    assert_eq!(fx.moderation(post), "REJECTED");
    assert_eq!(fx.upload_status(upload), "REJECTED");
}

// PostImageProcessingServiceTest#completePostImageProcessing_changedStorageKeyRule_stillRequestsModeration
#[test]
fn complete_post_image_processing_changed_storage_key_rule_still_requests_moderation() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    // 저장 키 규칙이 바뀌어 사진의 원본 키가 업로드 id로 파생되지 않아도 업로드 연결로 게시물을 찾는다.
    fx.exec(&format!(
        "UPDATE photos SET original_storage_key = 'legacy/' || id::text WHERE id = (SELECT photo_id FROM posts WHERE id = '{post}')"
    ));
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    assert_eq!(fx.moderation(post), "PENDING");
}

// PostImageProcessingServiceTest#failPostImageProcessing_issuedUpload_marksRejected
#[test]
fn fail_post_image_processing_issued_upload_marks_rejected() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let upload = fx.upload_in(user, "ISSUED");
    fx.command
        .fail_post_image_processing(upload, Some("INVALID_IMAGE"))
        .unwrap();
    assert_eq!(fx.upload_status(upload), "REJECTED");
    assert_eq!(
        fx.text(&format!(
            "SELECT rejection_reason FROM post_image_uploads WHERE id = '{upload}'"
        ))
        .as_deref(),
        Some("INVALID_IMAGE")
    );
}

// PostImageProcessingServiceTest#failPostImageProcessing_validatingPost_rejectsPost
#[test]
fn fail_post_image_processing_validating_post_rejects_post() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    fx.command
        .fail_post_image_processing(upload, Some("INVALID_IMAGE"))
        .unwrap();
    assert_eq!(fx.moderation(post), "REJECTED");
    assert!(
        fx.text(&format!(
            "SELECT moderated_at::text FROM posts WHERE id = '{post}'"
        ))
        .is_none()
    );
}

// PostImageProcessingServiceTest#failPostImageProcessing_duplicateCallback_staysImageRejected
#[test]
fn fail_post_image_processing_duplicate_callback_stays_image_rejected() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    fx.command
        .fail_post_image_processing(upload, Some("INVALID_IMAGE"))
        .unwrap();
    fx.command
        .fail_post_image_processing(upload, Some("OTHER"))
        .unwrap();
    assert_eq!(fx.moderation(post), "REJECTED");
    assert_eq!(
        fx.text(&format!(
            "SELECT rejection_reason FROM post_image_uploads WHERE id = '{upload}'"
        ))
        .as_deref(),
        Some("INVALID_IMAGE")
    );
}

// PostImageProcessingServiceTest#failPostImageProcessing_readyUpload_keepsPostPending
#[test]
fn fail_post_image_processing_ready_upload_keeps_post_pending() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "READY");
    let result = fx.command.create_post(user, topic, upload, None).unwrap();
    fx.command
        .fail_post_image_processing(upload, Some("INVALID_IMAGE"))
        .unwrap();
    assert_eq!(fx.moderation(result.post_id), "PENDING");
    assert_eq!(fx.upload_status(upload), "READY");
}

// ---------------------------------------------------------------------------------------------
// PostModerationNotificationTransactionTest, PostModerationPendingLogTest
// ---------------------------------------------------------------------------------------------

// PostModerationNotificationTransactionTest#createPost_readyUpload_sendsAfterCommit
#[test]
fn create_post_ready_upload_sends_after_commit() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let upload = fx.upload_in(user, "READY");

    let result = fx.command.create_post(user, topic, upload, None).unwrap();

    let events = fx.events.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, result.post_id);
    assert_eq!(events[0].1, ts(NOW));
    // 구독자가 불린 시점에는 게시물이 이미 커밋돼 있어야 하므로 다른 연결에서 읽힌다.
    assert_eq!(fx.moderation(result.post_id), "PENDING");
}

// PostModerationNotificationTransactionTest#completePostImageProcessing_validatingPost_sendsAfterCommit
#[test]
fn complete_post_image_processing_validating_post_sends_after_commit() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    let events = fx.events.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, post);
}

// PostModerationNotificationTransactionTest#completePostImageProcessing_duplicateCallback_sendsOnce
#[test]
fn complete_post_image_processing_duplicate_callback_sends_once() {
    let Some(fx) = Fx::create() else { return };
    let (_, upload) = validating_post_with_upload(&fx);
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();
    assert_eq!(fx.events.0.lock().unwrap().len(), 1);
}

// PostModerationPendingLogTest#completePostImageProcessing_validatingPost_logsPending
#[test]
fn complete_post_image_processing_validating_post_logs_pending() {
    let Some(fx) = Fx::create() else { return };
    let (post, upload) = validating_post_with_upload(&fx);
    fx.command
        .complete_post_image_processing(upload, metadata())
        .unwrap();

    let lines = fx.log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let entry: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(entry["logger_name"], "chalkak.moderation");
    assert_eq!(entry["level"], "INFO");
    assert_eq!(entry["type"], "moderation");
    assert_eq!(entry["event"], "pending");
    assert_eq!(entry["postId"], post.to_string());
}

// ---------------------------------------------------------------------------------------------
// PostImageUploadServiceTest
// ---------------------------------------------------------------------------------------------

// PostImageUploadServiceTest#createPostImageUpload_activeUser_savesIssuedUpload
#[test]
fn create_post_image_upload_active_user_saves_issued_upload() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let result = fx.command.create_post_image_upload(user).unwrap();
    let row = fx
        .client()
        .query_one(
            "SELECT user_id, status::text AS s, claimed_at FROM post_image_uploads WHERE id = $1",
            &[&result.upload_id],
        )
        .unwrap();
    assert_eq!(row.get::<_, Uuid>("user_id"), user);
    assert_eq!(row.get::<_, String>("s"), "ISSUED");
    assert!(
        row.get::<_, Option<jiff::Timestamp>>("claimed_at")
            .is_none()
    );
}

// PostImageUploadServiceTest#createPostImageUpload_activeUser_issuesUrlForSavedUploadId
#[test]
fn create_post_image_upload_active_user_issues_url_for_saved_upload_id() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let result = fx.command.create_post_image_upload(user).unwrap();
    assert!(result.upload_url.contains(&result.upload_id.to_string()));
    assert_eq!(result.expires_in_seconds, 300);
    assert_eq!(result.content_type, "image/webp");
    assert_eq!(result.max_bytes, 5_242_880);
}

// PostImageUploadServiceTest#createPostImageUpload_activeUser_expiresAfterClaimTtl
#[test]
fn create_post_image_upload_active_user_expires_after_claim_ttl() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let result = fx.command.create_post_image_upload(user).unwrap();
    let expires: jiff::Timestamp = fx
        .client()
        .query_one(
            "SELECT expires_at FROM post_image_uploads WHERE id = $1",
            &[&result.upload_id],
        )
        .unwrap()
        .get(0);
    let ttl = expires.duration_since(ts(NOW));
    assert_eq!(ttl, chalkak_app::post::domain::CLAIM_TTL);
}

// PostImageUploadServiceTest#createPostImageUpload_unknownUser_throwsUnauthorized
#[test]
fn create_post_image_upload_unknown_user_throws_unauthorized() {
    let Some(fx) = Fx::create() else { return };
    assert_err(
        fx.command.create_post_image_upload(Uuid::new_v4()),
        401,
        "유효하지 않은 인증 정보입니다.",
    );
}

// PostImageUploadServiceTest#createPostImageUpload_withdrawnUser_throwsUnauthorized
#[test]
fn create_post_image_upload_withdrawn_user_throws_unauthorized() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    fx.delete_user(user);
    assert_err(
        fx.command.create_post_image_upload(user),
        401,
        "유효하지 않은 인증 정보입니다.",
    );
    assert_eq!(fx.count("SELECT count(*) FROM post_image_uploads"), 0);
}

// ---------------------------------------------------------------------------------------------
// PostDeletionServiceTest
// ---------------------------------------------------------------------------------------------

// PostDeletionServiceTest#deletePost_pendingPost_softDeletesPostAndPhotoAndPreservesStorageKeys
#[test]
fn delete_post_pending_post_soft_deletes_post_and_photo_and_preserves_storage_keys() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (post, photo) = fx.post(user, topic, "PENDING", &at(-3600));

    fx.command.delete_post(user, post).unwrap();

    let row = fx
        .client()
        .query_one(
            "SELECT p.deleted_at AS pd, ph.deleted_at AS phd, ph.original_storage_key, \
             ph.thumbnail_storage_key FROM posts p JOIN photos ph ON ph.id = p.photo_id WHERE p.id = $1",
            &[&post],
        )
        .unwrap();
    assert_eq!(row.get::<_, Option<jiff::Timestamp>>("pd"), Some(ts(NOW)));
    assert_eq!(row.get::<_, Option<jiff::Timestamp>>("phd"), Some(ts(NOW)));
    assert_eq!(
        row.get::<_, String>("original_storage_key"),
        format!("chalkak/posts/test/original/{photo}.webp")
    );
    assert!(
        row.get::<_, Option<String>>("thumbnail_storage_key")
            .is_some()
    );
}

// PostDeletionServiceTest#deletePost_approvedPost_softDeletesAndDeletesLikesAndExcludesReadModels
#[test]
fn delete_post_approved_post_soft_deletes_and_deletes_likes_and_excludes_read_models() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let fan = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.like(post, fan);

    fx.command.delete_post(user, post).unwrap();

    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 0);
    assert_err(
        fx.query.get_post(post, fan),
        404,
        "게시물을 찾을 수 없습니다.",
    );
    let calendar = fx.query.get_my_post_calendar(user, 2026, 10).unwrap();
    assert!(calendar.posts.is_empty());
}

// PostDeletionServiceTest#deletePost_otherUser_throwsForbiddenExceptionAndKeepsEveryState
#[test]
fn delete_post_other_user_throws_forbidden_exception_and_keeps_every_state() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let other = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.like(post, other);

    let result = fx.command.delete_post(other, post);

    match result {
        Err(chalkak_app::web::ApiError::App(e)) => assert_eq!(e.status(), 403),
        other => panic!("403이어야 합니다: {other:?}"),
    }
    assert_eq!(
        fx.count("SELECT count(*) FROM posts WHERE deleted_at IS NOT NULL"),
        0
    );
    assert_eq!(
        fx.count("SELECT count(*) FROM photos WHERE deleted_at IS NOT NULL"),
        0
    );
    assert_eq!(fx.count("SELECT count(*) FROM post_likes"), 1);
}

// PostDeletionServiceTest#deletePost_validatingPost_throwsBusinessExceptionAndKeepsPostAndPhoto
#[test]
fn delete_post_validating_post_throws_business_exception_and_keeps_post_and_photo() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (post, _) = fx.post(user, topic, "VALIDATING", &at(-60));
    assert_err(
        fx.command.delete_post(user, post),
        400,
        "이미지 처리 중인 게시물은 삭제할 수 없습니다.",
    );
    assert_eq!(
        fx.count("SELECT count(*) FROM posts WHERE deleted_at IS NOT NULL"),
        0
    );
    assert_eq!(
        fx.count("SELECT count(*) FROM photos WHERE deleted_at IS NOT NULL"),
        0
    );
}

// PostDeletionServiceTest#deletePost_rejectedPost_throwsBusinessExceptionAndKeepsPostAndPhoto
#[test]
fn delete_post_rejected_post_throws_business_exception_and_keeps_post_and_photo() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (post, _) = fx.post(user, topic, "REJECTED", &at(-60));
    assert_err(
        fx.command.delete_post(user, post),
        400,
        "검수 거절된 게시물은 삭제할 수 없습니다.",
    );
    assert_eq!(
        fx.count("SELECT count(*) FROM posts WHERE deleted_at IS NOT NULL"),
        0
    );
}

// PostDeletionServiceTest#deletePost_unknownPost_throwsNotFoundException
#[test]
fn delete_post_unknown_post_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    assert_err(
        fx.command.delete_post(user, Uuid::new_v4()),
        404,
        "게시물을 찾을 수 없습니다.",
    );
}

// PostDeletionServiceTest#deletePost_alreadyDeletedPost_keepsFirstDeletionTime
#[test]
fn delete_post_already_deleted_post_keeps_first_deletion_time() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.command.delete_post(user, post).unwrap();
    fx.clock.advance(std::time::Duration::from_secs(3600));

    fx.command.delete_post(user, post).unwrap();

    let deleted: Option<jiff::Timestamp> = fx
        .client()
        .query_one("SELECT deleted_at FROM posts WHERE id = $1", &[&post])
        .unwrap()
        .get(0);
    assert_eq!(deleted, Some(ts(NOW)));
}

// PostDeletionServiceTest#deletePost_openTopic_allowsNewPostSubmission
#[test]
fn delete_post_open_topic_allows_new_post_submission() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.command.delete_post(user, post).unwrap();
    let upload = fx.upload_in(user, "READY");

    let result = fx.command.create_post(user, topic, upload, None).unwrap();

    assert_eq!(result.moderation_status, ModerationStatus::Pending);
}

// ---------------------------------------------------------------------------------------------
// PostUpdateServiceTest
// ---------------------------------------------------------------------------------------------

fn title_of(fx: &Fx, post: Uuid) -> Option<String> {
    fx.text(&format!("SELECT title FROM posts WHERE id = '{post}'"))
}

// PostUpdateServiceTest#updatePost_pendingPost_updatesNormalizedTitleAndKeepsStatus
#[test]
fn update_post_pending_post_updates_normalized_title_and_keeps_status() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let (post, _) = fx.post(user, topic, "PENDING", &at(-3600));

    let result = fx
        .command
        .update_post(user, post, Some("  수정 제목  "))
        .unwrap();

    assert_eq!(result.title.as_deref(), Some("수정 제목"));
    assert_eq!(title_of(&fx, post).as_deref(), Some("수정 제목"));
    assert_eq!(fx.moderation(post), "PENDING");
}

// PostUpdateServiceTest#updatePost_approvedPost_updatesOnlyTitleAndUpdatedAt
#[test]
fn update_post_approved_post_updates_only_title_and_updated_at() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);

    fx.command.update_post(user, post, Some("새 제목")).unwrap();

    let row = fx
        .client()
        .query_one(
            "SELECT moderation_status::text AS s, created_at, updated_at, moderated_at FROM posts WHERE id = $1",
            &[&post],
        )
        .unwrap();
    assert_eq!(row.get::<_, String>("s"), "APPROVED");
    assert_eq!(row.get::<_, jiff::Timestamp>("updated_at"), ts(NOW));
    assert_ne!(
        row.get::<_, jiff::Timestamp>("created_at"),
        row.get::<_, jiff::Timestamp>("updated_at")
    );
}

// PostUpdateServiceTest#updatePost_emptyTitle_storesNullTitle
#[test]
fn update_post_empty_title_stores_null_title() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.command.update_post(user, post, Some("처음")).unwrap();

    let result = fx.command.update_post(user, post, Some("   ")).unwrap();

    assert_eq!(result.title, None);
    assert_eq!(title_of(&fx, post), None);
}

// PostUpdateServiceTest#updatePost_sameNormalizedTitle_keepsUpdatedAt
#[test]
fn update_post_same_normalized_title_keeps_updated_at() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.command
        .update_post(user, post, Some("같은 제목"))
        .unwrap();
    fx.clock.advance(std::time::Duration::from_secs(3600));

    fx.command
        .update_post(user, post, Some(" 같은 제목 "))
        .unwrap();

    let updated: jiff::Timestamp = fx
        .client()
        .query_one("SELECT updated_at FROM posts WHERE id = $1", &[&post])
        .unwrap()
        .get(0);
    assert_eq!(updated, ts(NOW));
}

// PostUpdateServiceTest#updatePost_deletedPost_throwsNotFoundException
#[test]
fn update_post_deleted_post_throws_not_found_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.soft_delete("posts", post);
    assert_err(
        fx.command.update_post(user, post, Some("제목")),
        404,
        "게시물을 찾을 수 없습니다.",
    );
}

// PostUpdateServiceTest#updatePost_withdrawnAuthor_throwsUnauthorizedException
#[test]
fn update_post_withdrawn_author_throws_unauthorized_exception() {
    let Some(fx) = Fx::create() else { return };
    let user = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(user, topic);
    fx.delete_user(user);
    assert_err(
        fx.command.update_post(user, post, Some("제목")),
        401,
        "유효하지 않은 인증 정보입니다.",
    );
}
