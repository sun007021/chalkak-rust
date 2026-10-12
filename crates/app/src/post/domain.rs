//! 게시물 도메인 규칙. 원본 `Post`, `PostTitle`, `PostImageUpload`, `ModerationStatus`, `PostImageUploadStatus`.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::photo::domain::Photo;
use crate::topic::domain::TopicPhase;
use crate::web::binding::is_java_whitespace;

// ---------------------------------------------------------------------------------------------
// 열거형
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModerationStatus {
    Validating,
    Pending,
    Approved,
    Rejected,
}

impl ModerationStatus {
    /// DB enum(`moderation_status`)과 JSON 응답에 쓰는 이름.
    pub const fn as_str(self) -> &'static str {
        match self {
            ModerationStatus::Validating => "VALIDATING",
            ModerationStatus::Pending => "PENDING",
            ModerationStatus::Approved => "APPROVED",
            ModerationStatus::Rejected => "REJECTED",
        }
    }

    pub fn parse(name: &str) -> Option<ModerationStatus> {
        Some(match name {
            "VALIDATING" => ModerationStatus::Validating,
            "PENDING" => ModerationStatus::Pending,
            "APPROVED" => ModerationStatus::Approved,
            "REJECTED" => ModerationStatus::Rejected,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostImageUploadStatus {
    Issued,
    Ready,
    Rejected,
}

impl PostImageUploadStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            PostImageUploadStatus::Issued => "ISSUED",
            PostImageUploadStatus::Ready => "READY",
            PostImageUploadStatus::Rejected => "REJECTED",
        }
    }

    pub fn parse(name: &str) -> Option<PostImageUploadStatus> {
        Some(match name {
            "ISSUED" => PostImageUploadStatus::Issued,
            "READY" => PostImageUploadStatus::Ready,
            "REJECTED" => PostImageUploadStatus::Rejected,
            _ => return None,
        })
    }
}

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

// ---------------------------------------------------------------------------------------------
// 제목
// ---------------------------------------------------------------------------------------------

const MAX_TITLE_LENGTH: usize = 10;

/// Java `String.strip()`.
pub fn java_strip(s: &str) -> &str {
    s.trim_matches(is_java_whitespace)
}

/// 게시물 제목의 정규화·길이 규칙을 한곳에 모은 값 객체. 제목 없음은 `Option<PostTitle>`의 `None`으로만 표현한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostTitle(String);

impl PostTitle {
    /// `new PostTitle(value)`: 앞뒤 공백을 떼고 비었거나 길면 거절한다.
    pub fn new(value: &str) -> Result<PostTitle, AppError> {
        let value = java_strip(value);
        if value.is_empty() {
            return Err(business(
                "제목 없음은 PostTitle이 아니라 null로 표현해야 합니다.",
            ));
        }
        if exceeds_max_length(value) {
            return Err(business(&format!(
                "제목은 {MAX_TITLE_LENGTH}자 이하여야 합니다."
            )));
        }
        Ok(PostTitle(value.to_owned()))
    }

    /// `PostTitle.from`: 없거나 공백뿐이면 제목 없음(`None`).
    pub fn from(raw: Option<&str>) -> Result<Option<PostTitle>, AppError> {
        match raw {
            None => Ok(None),
            Some(raw) if raw.chars().all(is_java_whitespace) => Ok(None),
            Some(raw) => PostTitle::new(raw).map(Some),
        }
    }

    /// `PostTitle.isValid`: 요청 검증용. 공백뿐인 제목은 길이를 재지 않는다.
    pub fn is_valid(raw: Option<&str>) -> bool {
        raw.is_none_or(|raw| !exceeds_max_length(java_strip(raw)))
    }

    pub fn value(&self) -> &str {
        &self.0
    }
}

/// 이모지 한 글자는 UTF-16 code unit 두 칸을 쓰므로 code point 기준으로 판정한다(`posts.title`은 `VARCHAR(10)`).
fn exceeds_max_length(value: &str) -> bool {
    value.chars().count() > MAX_TITLE_LENGTH
}

// ---------------------------------------------------------------------------------------------
// 게시물
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Post {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub author_id: Uuid,
    pub topic_id: Uuid,
    pub photo: Photo,
    /// 소비한 이미지 업로드. 다른 애그리게이트라 연관관계가 아니라 식별자로 들고 있다.
    pub post_image_upload_id: Option<Uuid>,
    pub title: Option<PostTitle>,
    pub moderation_status: ModerationStatus,
    pub moderated_at: Option<Timestamp>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}

impl Post {
    /// `Post.createPost`
    pub fn create(
        author_id: Uuid,
        topic_id: Uuid,
        photo: Photo,
        post_image_upload_id: Option<Uuid>,
        title: Option<&str>,
    ) -> Result<Post, AppError> {
        if post_image_upload_id.is_none() {
            return Err(business("게시물 생성 정보가 올바르지 않습니다."));
        }
        Ok(Post {
            id: None,
            author_id,
            topic_id,
            photo,
            post_image_upload_id,
            title: PostTitle::from(title)?,
            moderation_status: ModerationStatus::Validating,
            moderated_at: None,
            created_at: None,
            updated_at: None,
            deleted_at: None,
        })
    }

    pub fn title_value(&self) -> Option<&str> {
        self.title.as_ref().map(PostTitle::value)
    }

    pub fn is_validating(&self) -> bool {
        self.moderation_status == ModerationStatus::Validating
    }

    pub fn request_moderation(&mut self) -> Result<(), AppError> {
        self.validate_moderation_status(ModerationStatus::Validating)?;
        self.moderation_status = ModerationStatus::Pending;
        Ok(())
    }

    pub fn approve(&mut self, moderated_at: Option<Timestamp>) -> Result<(), AppError> {
        self.decide_moderation(ModerationStatus::Approved, moderated_at)
    }

    pub fn reject(&mut self, moderated_at: Option<Timestamp>) -> Result<(), AppError> {
        self.decide_moderation(ModerationStatus::Rejected, moderated_at)
    }

    pub fn fail_image_processing(&mut self) -> Result<(), AppError> {
        self.validate_moderation_status(ModerationStatus::Validating)?;
        self.moderation_status = ModerationStatus::Rejected;
        self.moderated_at = None;
        Ok(())
    }

    /// `updateTitle`. `topic_phase`는 수정 시각(`now`) 기준 주제 단계다. 검사 순서는 원본과 같다:
    /// 작성자, 검수 상태, 참여 기간, 제목 길이.
    pub fn update_title(
        &mut self,
        author_id: Uuid,
        title: Option<&str>,
        topic_phase: TopicPhase,
    ) -> Result<(), AppError> {
        self.validate_author(author_id)?;
        self.validate_title_update_status()?;
        if topic_phase != TopicPhase::Open {
            return Err(business("참여 기간이 종료된 게시물은 수정할 수 없습니다."));
        }
        self.title = PostTitle::from(title)?;
        Ok(())
    }

    pub fn delete_by_author(
        &mut self,
        author_id: Uuid,
        deleted_at: Option<Timestamp>,
    ) -> Result<(), AppError> {
        self.validate_author(author_id)?;
        self.validate_author_deletion_status()?;
        self.delete_if_not_deleted(deleted_at)
    }

    pub fn delete_by_admin(&mut self, deleted_at: Option<Timestamp>) -> Result<(), AppError> {
        self.validate_admin_deletion_status()?;
        self.delete_if_not_deleted(deleted_at)
    }

    fn validate_admin_deletion_status(&self) -> Result<(), AppError> {
        match self.moderation_status {
            ModerationStatus::Pending | ModerationStatus::Approved | ModerationStatus::Rejected => {
                Ok(())
            }
            ModerationStatus::Validating => {
                Err(business("이미지 처리 중인 게시물은 삭제할 수 없습니다."))
            }
        }
    }

    fn validate_author(&self, author_id: Uuid) -> Result<(), AppError> {
        if self.author_id != author_id {
            return Err(AppError::forbidden(
                ErrorCode::FORBIDDEN,
                "본인의 게시물만 접근할 수 있습니다.",
            ));
        }
        Ok(())
    }

    fn validate_title_update_status(&self) -> Result<(), AppError> {
        match self.moderation_status {
            ModerationStatus::Pending | ModerationStatus::Approved => Ok(()),
            _ => Err(business("현재 상태의 게시물은 수정할 수 없습니다.")),
        }
    }

    fn validate_author_deletion_status(&self) -> Result<(), AppError> {
        match self.moderation_status {
            ModerationStatus::Pending | ModerationStatus::Approved => Ok(()),
            ModerationStatus::Rejected => Err(business("검수 거절된 게시물은 삭제할 수 없습니다.")),
            ModerationStatus::Validating => {
                Err(business("이미지 처리 중인 게시물은 삭제할 수 없습니다."))
            }
        }
    }

    fn delete_if_not_deleted(&mut self, deleted_at: Option<Timestamp>) -> Result<(), AppError> {
        if self.deleted_at.is_some() {
            return Ok(());
        }
        let Some(at) = deleted_at else {
            return Err(business("게시물 삭제 시각이 필요합니다."));
        };
        self.photo.delete(Some(at))?;
        self.deleted_at = Some(at);
        Ok(())
    }

    fn decide_moderation(
        &mut self,
        status: ModerationStatus,
        moderated_at: Option<Timestamp>,
    ) -> Result<(), AppError> {
        self.validate_moderation_status(ModerationStatus::Pending)?;
        let Some(at) = moderated_at else {
            return Err(business("게시물 검수 시각이 필요합니다."));
        };
        self.moderation_status = status;
        self.moderated_at = Some(at);
        Ok(())
    }

    fn validate_moderation_status(&self, expected: ModerationStatus) -> Result<(), AppError> {
        if self.moderation_status != expected {
            return Err(business("게시물 검수 상태를 변경할 수 없습니다."));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// 이미지 업로드 권한
// ---------------------------------------------------------------------------------------------

/// 발급한 업로드를 게시물 생성에 쓸 수 있는 시간.
pub const CLAIM_TTL: SignedDuration = SignedDuration::from_hours(1);

const DEFAULT_REJECTION_MESSAGE: &str = "처리할 수 없는 사진입니다. 다시 업로드해 주세요.";

fn rejection_message_for(reason: &str) -> &'static str {
    match reason {
        "UNSUPPORTED_FORMAT" => "WebP 이미지만 업로드할 수 있습니다.",
        "CORRUPTED_IMAGE" => "사진 파일이 손상되었습니다. 다시 업로드해 주세요.",
        "ANIMATED_IMAGE" => "움직이는 이미지는 업로드할 수 없습니다.",
        "TOO_LARGE" => "더 작은 용량의 이미지를 업로드해 주세요.",
        "TOO_MANY_PIXELS" => "해상도가 너무 큰 이미지입니다. 크기를 줄여 다시 업로드해 주세요.",
        "MISSING_OBJECT" => "업로드한 사진을 찾을 수 없습니다. 다시 업로드해 주세요.",
        "PROCESSING_ERROR" => "사진을 처리하지 못했습니다. 다시 업로드해 주세요.",
        _ => DEFAULT_REJECTION_MESSAGE,
    }
}

/// 게시물 이미지 업로드 권한. presigned URL을 발급하는 시점에 만들어지므로 Lambda 완료 콜백과 게시물 생성 요청 중
/// 어느 쪽이 먼저 도착해도 상대가 남긴 상태를 읽을 수 있다.
#[derive(Clone, Debug, PartialEq)]
pub struct PostImageUpload {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub user_id: Uuid,
    pub status: PostImageUploadStatus,
    pub rejection_reason: Option<String>,
    pub image_metadata: Option<Map<String, Value>>,
    pub expires_at: Timestamp,
    pub claimed_at: Option<Timestamp>,
}

impl PostImageUpload {
    /// `PostImageUpload.createPostImageUpload`
    pub fn create(user_id: Uuid, issued_at: Timestamp) -> PostImageUpload {
        PostImageUpload {
            id: None,
            user_id,
            status: PostImageUploadStatus::Issued,
            rejection_reason: None,
            image_metadata: None,
            expires_at: issued_at
                .checked_add(CLAIM_TTL)
                .expect("발급 시각에 한 시간을 더해도 범위 안이다"),
            claimed_at: None,
        }
    }

    /// 중복·역순 콜백은 상태를 바꾸지 않는다. SQS가 같은 메시지를 다시 전달할 수 있고, 처리 실패 뒤에 도착한 완료
    /// 콜백이 거절된 이미지를 되살리면 안 되기 때문이다. 상태가 바뀌었으면 `true`.
    pub fn complete_processing(&mut self, image_metadata: Map<String, Value>) -> bool {
        if self.status != PostImageUploadStatus::Issued {
            return false;
        }
        self.status = PostImageUploadStatus::Ready;
        self.image_metadata = Some(image_metadata);
        true
    }

    pub fn fail_processing(&mut self, rejection_reason: Option<&str>) -> bool {
        if self.status != PostImageUploadStatus::Issued {
            return false;
        }
        self.status = PostImageUploadStatus::Rejected;
        self.rejection_reason = rejection_reason.map(str::to_owned);
        true
    }

    pub fn claim(&mut self, claimed_at: Timestamp) -> Result<(), AppError> {
        if self.claimed_at.is_some() {
            return Err(business("이미 사용된 사진입니다."));
        }
        if self.status == PostImageUploadStatus::Rejected {
            return Err(business(self.rejection_message()));
        }
        if claimed_at >= self.expires_at {
            return Err(business("사진 업로드 유효 시간이 지났습니다."));
        }
        self.claimed_at = Some(claimed_at);
        Ok(())
    }

    /// 콜백 본문에 사유가 없으면 사유 없는 REJECTED 행이 저장될 수 있으므로 기본 문구로 답한다.
    fn rejection_message(&self) -> &'static str {
        match &self.rejection_reason {
            None => DEFAULT_REJECTION_MESSAGE,
            Some(reason) => rejection_message_for(reason),
        }
    }

    pub fn is_processed(&self) -> bool {
        self.status == PostImageUploadStatus::Ready
    }

    pub fn is_rejected(&self) -> bool {
        self.status == PostImageUploadStatus::Rejected
    }

    pub fn is_owned_by(&self, user_id: Uuid) -> bool {
        self.user_id == user_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AUTHOR_ID: Uuid = Uuid::from_u128(0x0198f6c1_62ba_7d30_8b12_0f733b6570a1);
    const OTHER_USER_ID: Uuid = Uuid::from_u128(0x0198f6c1_62ba_7d30_8b12_0f733b6570a2);
    const UPLOAD_ID: Uuid = Uuid::from_u128(0x0198f6c1_62ba_7d30_8b12_0f733b6570f1);
    const TOPIC_ID: Uuid = Uuid::from_u128(0x0198f6c1_62ba_7d30_8b12_0f733b6570b2);

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn deleted_at() -> Timestamp {
        ts("2026-08-20T01:00:00Z")
    }

    fn photo() -> Photo {
        Photo::create(Some("chalkak/posts/dev/original/upload-id.png")).unwrap()
    }

    fn create(title: Option<&str>) -> Post {
        Post::create(AUTHOR_ID, TOPIC_ID, photo(), Some(UPLOAD_ID), title).unwrap()
    }

    fn pending_post(title: &str) -> Post {
        let mut post = create(Some(title));
        post.request_moderation().unwrap();
        post
    }

    fn post_with_status(status: ModerationStatus) -> Post {
        let mut post = create(Some("제목"));
        match status {
            ModerationStatus::Validating => {}
            ModerationStatus::Rejected => post.fail_image_processing().unwrap(),
            ModerationStatus::Pending | ModerationStatus::Approved => {
                post.request_moderation().unwrap();
                if status == ModerationStatus::Approved {
                    post.approve(Some(ts("2026-08-20T00:00:00Z"))).unwrap();
                }
            }
        }
        post
    }

    // ----- PostTitleTest -----

    // PostTitleTest#from_validTitle_createsPostTitle
    #[test]
    fn post_title_from_valid_title_creates_post_title() {
        let title = PostTitle::from(Some("오늘의 기록")).unwrap().unwrap();
        assert_eq!(title.value(), "오늘의 기록");
    }

    // PostTitleTest#from_titleWithSurroundingSpaces_stripsOnlySurroundingSpaces
    #[test]
    fn post_title_from_title_with_surrounding_spaces_strips_only_surrounding_spaces() {
        let title = PostTitle::from(Some("  오늘의  기록  ")).unwrap().unwrap();
        assert_eq!(title.value(), "오늘의  기록");
    }

    // PostTitleTest#from_blankTitle_returnsNull
    #[test]
    fn post_title_from_blank_title_returns_null() {
        for raw in [None, Some(""), Some("   "), Some("　　")] {
            assert_eq!(PostTitle::from(raw).unwrap(), None, "{raw:?}");
        }
    }

    // PostTitleTest#from_tenCharacterNormalizedTitle_createsPostTitle
    #[test]
    fn post_title_from_ten_character_normalized_title_creates_post_title() {
        let title = PostTitle::from(Some("  1234567890  ")).unwrap().unwrap();
        assert_eq!(title.value(), "1234567890");
    }

    // PostTitleTest#from_tenCodePointEmojiTitle_createsPostTitle
    #[test]
    fn post_title_from_ten_code_point_emoji_title_creates_post_title() {
        let raw = "📸".repeat(10);
        let title = PostTitle::from(Some(&raw)).unwrap().unwrap();
        assert_eq!(title.value(), raw);
    }

    // PostTitleTest#from_tooLongNormalizedTitle_throwsBusinessException
    #[test]
    fn post_title_from_too_long_normalized_title_throws_business_exception() {
        let raw = format!("  {}  ", "📸".repeat(11));
        let err = PostTitle::from(Some(&raw)).unwrap_err();
        assert_eq!(err.message, "제목은 10자 이하여야 합니다.");
    }

    // PostTitleTest#constructor_nullValue_throwsNullPointerException
    // 타입 시스템이 막는다: PostTitle::new는 &str만 받는다.

    // PostTitleTest#constructor_titleWithSurroundingSpaces_stripsTitle
    #[test]
    fn post_title_constructor_title_with_surrounding_spaces_strips_title() {
        assert_eq!(
            PostTitle::new("  오늘의 기록  ").unwrap().value(),
            "오늘의 기록"
        );
    }

    // PostTitleTest#constructor_blankTitle_throwsBusinessException
    #[test]
    fn post_title_constructor_blank_title_throws_business_exception() {
        for value in ["", "   ", "　　"] {
            let err = PostTitle::new(value).unwrap_err();
            assert_eq!(
                err.message,
                "제목 없음은 PostTitle이 아니라 null로 표현해야 합니다."
            );
        }
    }

    // PostTitleTest#constructor_tooLongTitle_throwsBusinessException
    #[test]
    fn post_title_constructor_too_long_title_throws_business_exception() {
        let err = PostTitle::new("12345678901").unwrap_err();
        assert_eq!(err.message, "제목은 10자 이하여야 합니다.");
    }

    // PostTitleTest#isValid_titleWithinMaxLength_returnsTrue
    #[test]
    fn post_title_is_valid_title_within_max_length_returns_true() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("오늘의 기록"),
            Some("1234567890"),
            Some("  1234567890  "),
        ] {
            assert!(PostTitle::is_valid(raw), "{raw:?}");
        }
    }

    // PostTitleTest#isValid_titleExceedingMaxLength_returnsFalse
    #[test]
    fn post_title_is_valid_title_exceeding_max_length_returns_false() {
        for raw in ["12345678901", "  12345678901  "] {
            assert!(!PostTitle::is_valid(Some(raw)), "{raw}");
        }
    }

    // ----- PostTest -----

    // PostTest#createPost_validValues_createsValidatingPost
    #[test]
    fn create_post_valid_values_creates_validating_post() {
        let post = create(Some("오늘의 기록"));
        assert_eq!(post.author_id, AUTHOR_ID);
        assert_eq!(post.topic_id, TOPIC_ID);
        assert_eq!(
            post.photo.original_storage_key,
            "chalkak/posts/dev/original/upload-id.png"
        );
        assert_eq!(post.title_value(), Some("오늘의 기록"));
        assert_eq!(post.moderation_status, ModerationStatus::Validating);
    }

    // PostTest#createPost_blankTitle_normalizesTitleToNull
    #[test]
    fn create_post_blank_title_normalizes_title_to_null() {
        for title in [None, Some(""), Some("   "), Some("　　")] {
            assert_eq!(create(title).title_value(), None, "{title:?}");
        }
    }

    // PostTest#createPost_tenCharacterTitle_createsPost
    #[test]
    fn create_post_ten_character_title_creates_post() {
        assert_eq!(create(Some("1234567890")).title_value(), Some("1234567890"));
    }

    // PostTest#createPost_longBlankTitle_normalizesTitleToNull
    #[test]
    fn create_post_long_blank_title_normalizes_title_to_null() {
        let title = "　".repeat(11);
        assert_eq!(create(Some(&title)).title_value(), None);
    }

    // PostTest#createPost_tenCodePointEmojiTitle_createsPost
    #[test]
    fn create_post_ten_code_point_emoji_title_creates_post() {
        let title = "📸".repeat(10);
        assert_eq!(create(Some(&title)).title_value(), Some(title.as_str()));
    }

    // PostTest#createPost_tooLongTitle_throwsBusinessException
    #[test]
    fn create_post_too_long_title_throws_business_exception() {
        let err = Post::create(
            AUTHOR_ID,
            TOPIC_ID,
            photo(),
            Some(UPLOAD_ID),
            Some("12345678901"),
        )
        .unwrap_err();
        assert_eq!(err.message, "제목은 10자 이하여야 합니다.");
    }

    // PostTest#createPost_titleWithSurroundingSpaces_normalizesTitle
    #[test]
    fn create_post_title_with_surrounding_spaces_normalizes_title() {
        assert_eq!(
            create(Some("  오늘의  기록  ")).title_value(),
            Some("오늘의  기록")
        );
    }

    // PostTest#createPost_tenCharacterNormalizedTitle_createsPost
    #[test]
    fn create_post_ten_character_normalized_title_creates_post() {
        assert_eq!(
            create(Some("  1234567890  ")).title_value(),
            Some("1234567890")
        );
    }

    // PostTest#createPost_tooLongNormalizedTitle_throwsBusinessException
    #[test]
    fn create_post_too_long_normalized_title_throws_business_exception() {
        let title = format!("  {}  ", "📸".repeat(11));
        let err =
            Post::create(AUTHOR_ID, TOPIC_ID, photo(), Some(UPLOAD_ID), Some(&title)).unwrap_err();
        assert_eq!(err.message, "제목은 10자 이하여야 합니다.");
    }

    // 원본에는 없는 경로: 업로드 식별자가 없으면 만들 수 없다(`postImageUploadId == null`).
    #[test]
    fn create_post_without_upload_id_throws_business_exception() {
        let err = Post::create(AUTHOR_ID, TOPIC_ID, photo(), None, Some("제목")).unwrap_err();
        assert_eq!(err.message, "게시물 생성 정보가 올바르지 않습니다.");
    }

    // PostTest#updateTitle_pendingPostDuringOpenPeriod_updatesTitle
    #[test]
    fn update_title_pending_post_during_open_period_updates_title() {
        let mut post = pending_post("기존 제목");
        post.update_title(AUTHOR_ID, Some("수정 제목"), TopicPhase::Open)
            .unwrap();
        assert_eq!(post.title_value(), Some("수정 제목"));
        assert_eq!(post.moderation_status, ModerationStatus::Pending);
    }

    // PostTest#updateTitle_approvedPostDuringOpenPeriod_updatesTitle
    #[test]
    fn update_title_approved_post_during_open_period_updates_title() {
        let mut post = pending_post("기존 제목");
        post.approve(Some(ts("2026-08-20T00:10:00Z"))).unwrap();
        post.update_title(AUTHOR_ID, Some("수정 제목"), TopicPhase::Open)
            .unwrap();
        assert_eq!(post.title_value(), Some("수정 제목"));
        assert_eq!(post.moderation_status, ModerationStatus::Approved);
        assert_eq!(post.moderated_at, Some(ts("2026-08-20T00:10:00Z")));
    }

    // PostTest#updateTitle_titleWithSurroundingSpaces_normalizesTitle
    #[test]
    fn update_title_title_with_surrounding_spaces_normalizes_title() {
        let mut post = pending_post("기존 제목");
        post.update_title(AUTHOR_ID, Some("  수정 제목  "), TopicPhase::Open)
            .unwrap();
        assert_eq!(post.title_value(), Some("수정 제목"));
    }

    // PostTest#updateTitle_emptyTitle_normalizesTitleToNull
    #[test]
    fn update_title_empty_title_normalizes_title_to_null() {
        for title in [None, Some(""), Some("   "), Some("　　")] {
            let mut post = pending_post("기존 제목");
            post.update_title(AUTHOR_ID, title, TopicPhase::Open)
                .unwrap();
            assert_eq!(post.title_value(), None, "{title:?}");
        }
    }

    // PostTest#updateTitle_tenCharacterNormalizedTitle_updatesTitle
    #[test]
    fn update_title_ten_character_normalized_title_updates_title() {
        let mut post = pending_post("기존 제목");
        post.update_title(AUTHOR_ID, Some("  1234567890  "), TopicPhase::Open)
            .unwrap();
        assert_eq!(post.title_value(), Some("1234567890"));
    }

    // PostTest#updateTitle_tooLongNormalizedTitle_throwsBusinessException
    #[test]
    fn update_title_too_long_normalized_title_throws_business_exception() {
        let mut post = pending_post("기존 제목");
        let title = format!("  {}  ", "📸".repeat(11));
        let err = post
            .update_title(AUTHOR_ID, Some(&title), TopicPhase::Open)
            .unwrap_err();
        assert_eq!(err.message, "제목은 10자 이하여야 합니다.");
        assert_eq!(post.title_value(), Some("기존 제목"));
    }

    // PostTest#updateTitle_otherUser_throwsForbiddenException
    #[test]
    fn update_title_other_user_throws_forbidden_exception() {
        let mut post = pending_post("기존 제목");
        let err = post
            .update_title(OTHER_USER_ID, Some("수정 제목"), TopicPhase::Open)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::FORBIDDEN);
        assert_eq!(err.status(), 403);
        assert_eq!(err.message, "본인의 게시물만 접근할 수 있습니다.");
        assert_eq!(post.title_value(), Some("기존 제목"));
    }

    // PostTest#updateTitle_uneditableStatus_throwsBusinessException
    #[test]
    fn update_title_uneditable_status_throws_business_exception() {
        for status in [ModerationStatus::Validating, ModerationStatus::Rejected] {
            let mut post = post_with_status(status);
            let err = post
                .update_title(AUTHOR_ID, Some("수정 제목"), TopicPhase::Open)
                .unwrap_err();
            assert_eq!(err.message, "현재 상태의 게시물은 수정할 수 없습니다.");
            assert_eq!(post.title_value(), Some("제목"));
        }
    }

    // PostTest#updateTitle_closedTopic_throwsBusinessException
    #[test]
    fn update_title_closed_topic_throws_business_exception() {
        let mut post = pending_post("기존 제목");
        let err = post
            .update_title(AUTHOR_ID, Some("수정 제목"), TopicPhase::Closed)
            .unwrap_err();
        assert_eq!(
            err.message,
            "참여 기간이 종료된 게시물은 수정할 수 없습니다."
        );
        assert_eq!(post.title_value(), Some("기존 제목"));
    }

    // 주제가 아직 열리지 않았어도 수정할 수 없다(`phaseAt != OPEN`).
    #[test]
    fn update_title_before_open_topic_throws_business_exception() {
        let mut post = pending_post("기존 제목");
        let err = post
            .update_title(AUTHOR_ID, Some("수정 제목"), TopicPhase::BeforeOpen)
            .unwrap_err();
        assert_eq!(
            err.message,
            "참여 기간이 종료된 게시물은 수정할 수 없습니다."
        );
    }

    // PostTest#requestModeration_validatingPost_becomesPending
    #[test]
    fn request_moderation_validating_post_becomes_pending() {
        let mut post = create(Some("제목"));
        post.request_moderation().unwrap();
        assert_eq!(post.moderation_status, ModerationStatus::Pending);
        assert_eq!(post.moderated_at, None);
    }

    // PostTest#approve_pendingPost_becomesApproved
    #[test]
    fn approve_pending_post_becomes_approved() {
        let mut post = pending_post("제목");
        let at = ts("2026-08-20T00:00:00Z");
        post.approve(Some(at)).unwrap();
        assert_eq!(post.moderation_status, ModerationStatus::Approved);
        assert_eq!(post.moderated_at, Some(at));
    }

    // PostTest#reject_pendingPost_becomesRejected
    #[test]
    fn reject_pending_post_becomes_rejected() {
        let mut post = pending_post("제목");
        let at = ts("2026-08-20T00:00:00Z");
        post.reject(Some(at)).unwrap();
        assert_eq!(post.moderation_status, ModerationStatus::Rejected);
        assert_eq!(post.moderated_at, Some(at));
    }

    // PostTest#failImageProcessing_validatingPost_becomesRejectedWithoutModerationTime
    #[test]
    fn fail_image_processing_validating_post_becomes_rejected_without_moderation_time() {
        let mut post = create(Some("제목"));
        post.fail_image_processing().unwrap();
        assert_eq!(post.moderation_status, ModerationStatus::Rejected);
        assert_eq!(post.moderated_at, None);
    }

    // PostTest#approve_validatingPost_throwsBusinessException
    #[test]
    fn approve_validating_post_throws_business_exception() {
        let mut post = create(Some("제목"));
        let err = post.approve(Some(ts("2026-08-20T00:00:00Z"))).unwrap_err();
        assert_eq!(err.message, "게시물 검수 상태를 변경할 수 없습니다.");
        assert_eq!(post.moderation_status, ModerationStatus::Validating);
    }

    // PostTest#reject_validatingPost_throwsBusinessException
    #[test]
    fn reject_validating_post_throws_business_exception() {
        let mut post = create(Some("제목"));
        let err = post.reject(Some(ts("2026-08-20T00:00:00Z"))).unwrap_err();
        assert_eq!(err.message, "게시물 검수 상태를 변경할 수 없습니다.");
        assert_eq!(post.moderation_status, ModerationStatus::Validating);
    }

    // PostTest#failImageProcessing_pendingPost_throwsBusinessException
    #[test]
    fn fail_image_processing_pending_post_throws_business_exception() {
        let mut post = pending_post("제목");
        let err = post.fail_image_processing().unwrap_err();
        assert_eq!(err.message, "게시물 검수 상태를 변경할 수 없습니다.");
        assert_eq!(post.moderation_status, ModerationStatus::Pending);
    }

    // PostTest#requestModeration_pendingPost_throwsBusinessException
    #[test]
    fn request_moderation_pending_post_throws_business_exception() {
        let mut post = pending_post("제목");
        let err = post.request_moderation().unwrap_err();
        assert_eq!(err.message, "게시물 검수 상태를 변경할 수 없습니다.");
    }

    // PostTest#approve_rejectedPost_throwsBusinessException
    #[test]
    fn approve_rejected_post_throws_business_exception() {
        let mut post = pending_post("제목");
        post.reject(Some(ts("2026-08-20T00:00:00Z"))).unwrap();
        let err = post.approve(Some(ts("2026-08-20T00:01:00Z"))).unwrap_err();
        assert_eq!(err.message, "게시물 검수 상태를 변경할 수 없습니다.");
    }

    // PostTest#approve_withoutModeratedAt_throwsBusinessException
    #[test]
    fn approve_without_moderated_at_throws_business_exception() {
        let mut post = pending_post("제목");
        let err = post.approve(None).unwrap_err();
        assert_eq!(err.message, "게시물 검수 시각이 필요합니다.");
        assert_eq!(post.moderation_status, ModerationStatus::Pending);
    }

    // PostTest#reject_withoutModeratedAt_throwsBusinessException
    #[test]
    fn reject_without_moderated_at_throws_business_exception() {
        let mut post = pending_post("제목");
        let err = post.reject(None).unwrap_err();
        assert_eq!(err.message, "게시물 검수 시각이 필요합니다.");
        assert_eq!(post.moderation_status, ModerationStatus::Pending);
    }

    // PostTest#deleteByAuthor_pendingPost_softDeletesPost
    #[test]
    fn delete_by_author_pending_post_soft_deletes_post() {
        let mut post = pending_post("제목");
        post.delete_by_author(AUTHOR_ID, Some(deleted_at()))
            .unwrap();
        assert_eq!(post.deleted_at, Some(deleted_at()));
    }

    // PostTest#deleteByAuthor_approvedPost_softDeletesPostAndPhoto
    #[test]
    fn delete_by_author_approved_post_soft_deletes_post_and_photo() {
        let mut post = post_with_status(ModerationStatus::Approved);
        post.delete_by_author(AUTHOR_ID, Some(deleted_at()))
            .unwrap();
        assert_eq!(post.deleted_at, Some(deleted_at()));
        assert_eq!(post.photo.deleted_at, Some(deleted_at()));
    }

    // PostTest#deleteByAuthor_alreadyDeletedPost_keepsFirstDeletionTime
    #[test]
    fn delete_by_author_already_deleted_post_keeps_first_deletion_time() {
        let mut post = pending_post("제목");
        post.delete_by_author(AUTHOR_ID, Some(deleted_at()))
            .unwrap();
        post.delete_by_author(AUTHOR_ID, Some(ts("2026-08-20T01:01:00Z")))
            .unwrap();
        assert_eq!(post.deleted_at, Some(deleted_at()));
        assert_eq!(post.photo.deleted_at, Some(deleted_at()));
    }

    // PostTest#deleteByAuthor_otherUser_throwsForbiddenException
    #[test]
    fn delete_by_author_other_user_throws_forbidden_exception() {
        let mut post = pending_post("제목");
        let err = post
            .delete_by_author(OTHER_USER_ID, Some(deleted_at()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::FORBIDDEN);
        assert_eq!(err.message, "본인의 게시물만 접근할 수 있습니다.");
        assert_eq!(post.deleted_at, None);
        assert_eq!(post.photo.deleted_at, None);
    }

    // PostTest#deleteByAuthor_rejectedPost_throwsBusinessException
    #[test]
    fn delete_by_author_rejected_post_throws_business_exception() {
        let mut post = post_with_status(ModerationStatus::Rejected);
        let err = post
            .delete_by_author(AUTHOR_ID, Some(deleted_at()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BUSINESS_ERROR);
        assert_eq!(err.message, "검수 거절된 게시물은 삭제할 수 없습니다.");
        assert_eq!(post.deleted_at, None);
        assert_eq!(post.photo.deleted_at, None);
    }

    // PostTest#deleteByAuthor_validatingPost_throwsBusinessException
    #[test]
    fn delete_by_author_validating_post_throws_business_exception() {
        let mut post = create(Some("제목"));
        let err = post
            .delete_by_author(AUTHOR_ID, Some(deleted_at()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BUSINESS_ERROR);
        assert_eq!(err.message, "이미지 처리 중인 게시물은 삭제할 수 없습니다.");
        assert_eq!(post.deleted_at, None);
        assert_eq!(post.photo.deleted_at, None);
    }

    // PostTest#deleteByAdmin_deletableStatus_softDeletesPostAndPhoto
    #[test]
    fn delete_by_admin_deletable_status_soft_deletes_post_and_photo() {
        for status in [
            ModerationStatus::Pending,
            ModerationStatus::Approved,
            ModerationStatus::Rejected,
        ] {
            let mut post = post_with_status(status);
            post.delete_by_admin(Some(deleted_at())).unwrap();
            assert_eq!(post.deleted_at, Some(deleted_at()), "{status:?}");
            assert_eq!(post.photo.deleted_at, Some(deleted_at()), "{status:?}");
        }
    }

    // PostTest#deleteByAdmin_validatingPost_throwsBusinessException
    #[test]
    fn delete_by_admin_validating_post_throws_business_exception() {
        let mut post = create(Some("제목"));
        let err = post.delete_by_admin(Some(deleted_at())).unwrap_err();
        assert_eq!(err.code, ErrorCode::BUSINESS_ERROR);
        assert_eq!(post.deleted_at, None);
        assert_eq!(post.photo.deleted_at, None);
    }

    // PostTest#deleteByAdmin_alreadyDeletedPost_keepsFirstDeletionTime
    #[test]
    fn delete_by_admin_already_deleted_post_keeps_first_deletion_time() {
        let mut post = post_with_status(ModerationStatus::Pending);
        post.delete_by_admin(Some(deleted_at())).unwrap();
        post.delete_by_admin(Some(ts("2026-08-20T01:01:00Z")))
            .unwrap();
        assert_eq!(post.deleted_at, Some(deleted_at()));
        assert_eq!(post.photo.deleted_at, Some(deleted_at()));
    }

    // ----- PostImageUploadTest -----

    const ISSUED_AT: &str = "2026-08-20T00:00:00Z";

    fn upload() -> PostImageUpload {
        PostImageUpload::create(AUTHOR_ID, ts(ISSUED_AT))
    }

    fn metadata(width: i64) -> Map<String, Value> {
        json!({"width": width}).as_object().unwrap().clone()
    }

    // PostImageUploadTest#createPostImageUpload_validUser_startsIssued
    #[test]
    fn create_post_image_upload_valid_user_starts_issued() {
        let upload = upload();
        assert_eq!(upload.status, PostImageUploadStatus::Issued);
        assert_eq!(upload.expires_at, ts("2026-08-20T01:00:00Z"));
        assert_eq!(upload.claimed_at, None);
    }

    // PostImageUploadTest#completeProcessing_issuedUpload_becomesReady
    #[test]
    fn complete_processing_issued_upload_becomes_ready() {
        let mut upload = upload();
        upload.complete_processing(metadata(4032));
        assert_eq!(upload.status, PostImageUploadStatus::Ready);
        assert_eq!(upload.image_metadata, Some(metadata(4032)));
    }

    // PostImageUploadTest#failProcessing_issuedUpload_becomesRejected
    #[test]
    fn fail_processing_issued_upload_becomes_rejected() {
        let mut upload = upload();
        upload.fail_processing(Some("UNSUPPORTED_FORMAT"));
        assert_eq!(upload.status, PostImageUploadStatus::Rejected);
        assert_eq!(
            upload.rejection_reason.as_deref(),
            Some("UNSUPPORTED_FORMAT")
        );
    }

    // PostImageUploadTest#completeProcessing_rejectedUpload_keepsRejected
    #[test]
    fn complete_processing_rejected_upload_keeps_rejected() {
        let mut upload = upload();
        upload.fail_processing(Some("TOO_LARGE"));
        upload.complete_processing(metadata(10));
        assert_eq!(upload.status, PostImageUploadStatus::Rejected);
        assert_eq!(upload.image_metadata, None);
    }

    // PostImageUploadTest#failProcessing_readyUpload_keepsReady
    #[test]
    fn fail_processing_ready_upload_keeps_ready() {
        let mut upload = upload();
        upload.complete_processing(metadata(10));
        upload.fail_processing(Some("TOO_LARGE"));
        assert_eq!(upload.status, PostImageUploadStatus::Ready);
        assert_eq!(upload.rejection_reason, None);
    }

    // PostImageUploadTest#completeProcessing_readyUpload_keepsFirstMetadata
    #[test]
    fn complete_processing_ready_upload_keeps_first_metadata() {
        let mut upload = upload();
        upload.complete_processing(metadata(10));
        upload.complete_processing(metadata(20));
        assert_eq!(upload.image_metadata, Some(metadata(10)));
    }

    // PostImageUploadTest#claim_unclaimedUpload_recordsClaimedAt
    #[test]
    fn claim_unclaimed_upload_records_claimed_at() {
        let mut upload = upload();
        let at = ts("2026-08-20T00:01:00Z");
        upload.claim(at).unwrap();
        assert_eq!(upload.claimed_at, Some(at));
    }

    // PostImageUploadTest#claim_claimedUpload_throwsBusinessException
    #[test]
    fn claim_claimed_upload_throws_business_exception() {
        let mut upload = upload();
        upload.claim(ts("2026-08-20T00:01:00Z")).unwrap();
        let err = upload.claim(ts("2026-08-20T00:02:00Z")).unwrap_err();
        assert_eq!(err.message, "이미 사용된 사진입니다.");
    }

    // PostImageUploadTest#claim_justBeforeExpiry_succeeds
    #[test]
    fn claim_just_before_expiry_succeeds() {
        let mut upload = upload();
        let at = upload
            .expires_at
            .checked_sub(SignedDuration::from_millis(1))
            .unwrap();
        upload.claim(at).unwrap();
        assert_eq!(upload.claimed_at, Some(at));
    }

    // PostImageUploadTest#claim_atExpiry_throwsBusinessException
    #[test]
    fn claim_at_expiry_throws_business_exception() {
        let mut upload = upload();
        let err = upload.claim(upload.expires_at).unwrap_err();
        assert_eq!(err.message, "사진 업로드 유효 시간이 지났습니다.");
    }

    // PostImageUploadTest#claim_rejectedUpload_throwsBusinessExceptionWithReasonMessage
    #[test]
    fn claim_rejected_upload_throws_business_exception_with_reason_message() {
        let mut upload = upload();
        upload.fail_processing(Some("UNSUPPORTED_FORMAT"));
        let err = upload.claim(ts("2026-08-20T00:01:00Z")).unwrap_err();
        assert_eq!(err.message, "WebP 이미지만 업로드할 수 있습니다.");
    }

    // PostImageUploadTest#claim_unknownRejectionReason_throwsBusinessExceptionWithDefaultMessage
    #[test]
    fn claim_unknown_rejection_reason_throws_business_exception_with_default_message() {
        let mut upload = upload();
        upload.fail_processing(Some("SOMETHING_NEW"));
        let err = upload.claim(ts("2026-08-20T00:01:00Z")).unwrap_err();
        assert_eq!(
            err.message,
            "처리할 수 없는 사진입니다. 다시 업로드해 주세요."
        );
    }

    // PostImageUploadTest#createPostImageUpload_nullUser_throwsBusinessException
    // 타입 시스템이 막는다: 회원 식별자와 발급 시각은 null일 수 없다.

    // PostImageUploadTest#claim_rejectedWithoutReason_throwsWithDefaultMessage
    #[test]
    fn claim_rejected_without_reason_throws_with_default_message() {
        let mut upload = upload();
        upload.fail_processing(None);
        let err = upload.claim(ts(ISSUED_AT)).unwrap_err();
        assert!(err.message.contains("처리할 수 없는 사진입니다."));
    }

    // PostImageUploadTest#claim_unknownReason_throwsWithDefaultMessage
    #[test]
    fn claim_unknown_reason_throws_with_default_message() {
        let mut upload = upload();
        upload.fail_processing(Some("SOMETHING_NEW"));
        let err = upload.claim(ts(ISSUED_AT)).unwrap_err();
        assert!(err.message.contains("처리할 수 없는 사진입니다."));
    }

    #[test]
    fn 거절_사유_일곱_가지는_각자의_안내_문구를_가진다() {
        for (reason, message) in [
            ("UNSUPPORTED_FORMAT", "WebP 이미지만 업로드할 수 있습니다."),
            (
                "CORRUPTED_IMAGE",
                "사진 파일이 손상되었습니다. 다시 업로드해 주세요.",
            ),
            ("ANIMATED_IMAGE", "움직이는 이미지는 업로드할 수 없습니다."),
            ("TOO_LARGE", "더 작은 용량의 이미지를 업로드해 주세요."),
            (
                "TOO_MANY_PIXELS",
                "해상도가 너무 큰 이미지입니다. 크기를 줄여 다시 업로드해 주세요.",
            ),
            (
                "MISSING_OBJECT",
                "업로드한 사진을 찾을 수 없습니다. 다시 업로드해 주세요.",
            ),
            (
                "PROCESSING_ERROR",
                "사진을 처리하지 못했습니다. 다시 업로드해 주세요.",
            ),
        ] {
            assert_eq!(rejection_message_for(reason), message);
        }
    }
}
