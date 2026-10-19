//! 회원 도메인 값과 규칙. 원본 `User`, `SignatureImagePolicy`, `SignatureStorageKeys`, `StoredImageMetadata`.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserStatus {
    Active,
    Banned,
}

impl UserStatus {
    pub fn name(self) -> &'static str {
        match self {
            UserStatus::Active => "ACTIVE",
            UserStatus::Banned => "BANNED",
        }
    }

    pub fn from_name(name: &str) -> Option<UserStatus> {
        match name {
            "ACTIVE" => Some(UserStatus::Active),
            "BANNED" => Some(UserStatus::Banned),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureProcessingStatus {
    Processing,
    Failed,
}

impl SignatureProcessingStatus {
    pub fn name(self) -> &'static str {
        match self {
            SignatureProcessingStatus::Processing => "PROCESSING",
            SignatureProcessingStatus::Failed => "FAILED",
        }
    }

    pub fn from_name(name: &str) -> Option<SignatureProcessingStatus> {
        match name {
            "PROCESSING" => Some(SignatureProcessingStatus::Processing),
            "FAILED" => Some(SignatureProcessingStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureStorageKeys {
    pub original: String,
    pub thumbnail: String,
}

/// 저장소에 올라온 이미지의 메타데이터(`HeadObject` 응답에서 읽은 값).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredImageMetadata {
    pub content_type: Option<String>,
    pub byte_size: i64,
}

/// `SignatureImagePolicy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureImagePolicy {
    pub max_bytes: i64,
    pub allowed_content_types: Vec<String>,
}

impl SignatureImagePolicy {
    pub fn validate(&self, image: &StoredImageMetadata) -> Result<(), AppError> {
        const MESSAGE: &str = "사용할 수 없는 사인 이미지입니다.";
        let allowed = image
            .content_type
            .as_ref()
            .is_some_and(|t| self.allowed_content_types.contains(t));
        if !allowed || image.byte_size > self.max_bytes {
            return Err(AppError::business(ErrorCode::BUSINESS_ERROR, MESSAGE));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub id: Uuid,
    pub email: Option<String>,
    pub status: UserStatus,
    pub signature_original_storage_key: String,
    pub signature_thumbnail_storage_key: Option<String>,
    pub pending_signature_upload_id: Option<Uuid>,
    pub signature_processing_status: Option<SignatureProcessingStatus>,
    pub signature_processing_started_at: Option<Timestamp>,
    pub app_version: Option<String>,
    /// 주제 푸시 수신 여부(기본 수신).
    pub topic_push_enabled: bool,
    /// 게시물 승인·반려 결과 푸시 수신 여부(기본 수신).
    pub moderation_push_enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
}

fn invalid_signature_info() -> AppError {
    AppError::business(
        ErrorCode::BUSINESS_ERROR,
        "사인 이미지 업로드 정보가 올바르지 않습니다.",
    )
}

fn already_withdrawn() -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, "이미 탈퇴한 회원입니다.")
}

fn validate_storage_keys(keys: &SignatureStorageKeys) -> Result<(), AppError> {
    let blank = |s: &str| s.chars().all(is_java_whitespace);
    if blank(&keys.original) || blank(&keys.thumbnail) {
        return Err(invalid_signature_info());
    }
    Ok(())
}

impl User {
    /// `User.create`. 저장 전이라 식별자와 시각은 자리표시(저장소가 채운다).
    pub fn create(email: Option<String>, keys: &SignatureStorageKeys) -> Result<User, AppError> {
        validate_storage_keys(keys)?;
        Ok(User {
            id: Uuid::nil(),
            email,
            status: UserStatus::Active,
            signature_original_storage_key: keys.original.clone(),
            signature_thumbnail_storage_key: Some(keys.thumbnail.clone()),
            pending_signature_upload_id: None,
            signature_processing_status: None,
            signature_processing_started_at: None,
            app_version: None,
            topic_push_enabled: true,
            moderation_push_enabled: true,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            deleted_at: None,
        })
    }

    /// `updatePushPreferences`: 탈퇴 회원은 바꿀 수 없다(401).
    pub fn update_push_preferences(
        &mut self,
        topic_push_enabled: bool,
        moderation_push_enabled: bool,
    ) -> Result<(), AppError> {
        self.validate_not_withdrawn()?;
        self.topic_push_enabled = topic_push_enabled;
        self.moderation_push_enabled = moderation_push_enabled;
        Ok(())
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// `withdraw`: 개인 정보를 지우고 탈퇴 시각을 남긴다.
    pub fn withdraw(&mut self, now: Timestamp) -> Result<(), AppError> {
        if self.is_deleted() {
            return Err(already_withdrawn());
        }
        self.email = Some(format!("withdrawn+{}@chalkak.invalid", self.id));
        self.signature_original_storage_key = format!("withdrawn/{}", self.id);
        self.signature_thumbnail_storage_key = None;
        self.clear_signature_processing();
        self.deleted_at = Some(now);
        Ok(())
    }

    /// 활성 회원을 차단한다(`ban`).
    pub fn ban(&mut self) -> Result<(), AppError> {
        self.validate_status_change_allowed()?;
        if self.status == UserStatus::Banned {
            return Err(AppError::business(
                ErrorCode::RESOURCE_STATE_CHANGED,
                "이미 차단된 회원입니다.",
            ));
        }
        self.status = UserStatus::Banned;
        Ok(())
    }

    /// 차단된 회원을 해제한다(`unban`).
    pub fn unban(&mut self) -> Result<(), AppError> {
        self.validate_status_change_allowed()?;
        if self.status == UserStatus::Active {
            return Err(AppError::business(
                ErrorCode::RESOURCE_STATE_CHANGED,
                "이미 활성 상태인 회원입니다.",
            ));
        }
        self.status = UserStatus::Active;
        Ok(())
    }

    /// 탈퇴하지 않았고 정지되지도 않아 서비스를 정상적으로 이용할 수 있는가(`isActive`).
    pub fn is_active(&self) -> bool {
        !self.is_deleted() && self.status == UserStatus::Active
    }

    fn validate_status_change_allowed(&self) -> Result<(), AppError> {
        if self.is_deleted() {
            return Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "탈퇴한 회원의 상태를 변경할 수 없습니다.",
            ));
        }
        Ok(())
    }

    pub fn start_signature_processing(
        &mut self,
        upload_id: Uuid,
        started_at: Timestamp,
    ) -> Result<(), AppError> {
        if self.is_deleted() {
            return Err(already_withdrawn());
        }
        self.pending_signature_upload_id = Some(upload_id);
        self.signature_processing_status = Some(SignatureProcessingStatus::Processing);
        self.signature_processing_started_at = Some(started_at);
        Ok(())
    }

    /// 처리 중인 업로드면 원본·썸네일 키를 바꾸고 처리 상태를 비운다. 아니면 `false`.
    pub fn complete_signature_processing(
        &mut self,
        upload_id: Uuid,
        keys: &SignatureStorageKeys,
    ) -> Result<bool, AppError> {
        if !self.is_signature_processing(upload_id) {
            return Ok(false);
        }
        validate_storage_keys(keys)?;
        self.signature_original_storage_key = keys.original.clone();
        self.signature_thumbnail_storage_key = Some(keys.thumbnail.clone());
        self.clear_signature_processing();
        Ok(true)
    }

    pub fn fail_signature_processing(&mut self, upload_id: Uuid) -> bool {
        if !self.is_signature_processing(upload_id) {
            return false;
        }
        self.signature_processing_status = Some(SignatureProcessingStatus::Failed);
        true
    }

    /// 진행 중이던 처리를 포기한다. 이후 도착하는 그 작업의 콜백은 pending 불일치로 무시된다.
    pub fn cancel_signature_processing(&mut self) {
        self.clear_signature_processing();
    }

    pub fn has_signature(&self, storage_key: &str) -> bool {
        self.signature_original_storage_key == storage_key
    }

    pub fn is_signature_processing(&self, upload_id: Uuid) -> bool {
        self.pending_signature_upload_id == Some(upload_id)
            && self.signature_processing_status == Some(SignatureProcessingStatus::Processing)
    }

    pub fn is_signature_processing_failed(&self, upload_id: Uuid) -> bool {
        self.pending_signature_upload_id == Some(upload_id)
            && self.signature_processing_status == Some(SignatureProcessingStatus::Failed)
    }

    /// 탈퇴 회원이면 401.
    pub fn validate_not_withdrawn(&self) -> Result<(), AppError> {
        if self.is_deleted() {
            return Err(AppError::unauthorized(
                ErrorCode::UNAUTHORIZED,
                "유효하지 않은 인증 정보입니다.",
            ));
        }
        Ok(())
    }

    /// 탈퇴 회원은 401, 차단 회원은 403.
    pub fn validate_accessible(&self) -> Result<(), AppError> {
        self.validate_not_withdrawn()?;
        if self.status == UserStatus::Banned {
            return Err(AppError::forbidden(
                ErrorCode::FORBIDDEN,
                "차단된 회원입니다.",
            ));
        }
        Ok(())
    }

    fn clear_signature_processing(&mut self) {
        self.pending_signature_upload_id = None;
        self.signature_processing_status = None;
        self.signature_processing_started_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // UserFixture.create(id): 저장 없이 검증하는 도메인 테스트는 id를 직접 넣는다.
    fn fixture(id: Uuid) -> User {
        let unique = id.to_string();
        let mut user = User::create(
            Some(format!("user-{unique}@chalkak.test")),
            &SignatureStorageKeys {
                original: format!("signatures/{unique}"),
                thumbnail: format!("thumbnails/{unique}"),
            },
        )
        .unwrap();
        user.id = id;
        user
    }

    fn fixture_banned(id: Uuid) -> User {
        let mut user = fixture(id);
        user.status = UserStatus::Banned;
        user
    }

    fn now() -> Timestamp {
        "2026-08-24T10:00:00Z".parse().unwrap()
    }

    // UserPushPreferencesTest#create_newUser_enablesBothPushPreferences
    #[test]
    fn create_new_user_enables_both_push_preferences() {
        let user = fixture(Uuid::new_v4());
        assert!(user.topic_push_enabled);
        assert!(user.moderation_push_enabled);
    }

    // UserPushPreferencesTest#updatePushPreferences_activeUser_updatesBothPreferences
    #[test]
    fn update_push_preferences_active_user_updates_both_preferences() {
        let mut user = fixture(Uuid::new_v4());
        user.update_push_preferences(false, true).unwrap();
        assert!(!user.topic_push_enabled);
        assert!(user.moderation_push_enabled);
        user.update_push_preferences(true, false).unwrap();
        assert!(user.topic_push_enabled);
        assert!(!user.moderation_push_enabled);
    }

    // UserPushPreferencesTest#updatePushPreferences_withdrawnUser_throwsUnauthorizedException
    #[test]
    fn update_push_preferences_withdrawn_user_throws_unauthorized_exception() {
        let mut user = fixture(Uuid::new_v4());
        user.withdraw(now()).unwrap();
        let error = user.update_push_preferences(false, false).unwrap_err();
        assert_eq!(error.kind, chalkak_core::error::ErrorKind::Unauthorized);
        assert_eq!(error.message, "유효하지 않은 인증 정보입니다.");
        assert!(user.topic_push_enabled && user.moderation_push_enabled);
    }

    // UserTest
    #[test]
    fn create_without_email_creates_active_user_with_signature() {
        let keys = SignatureStorageKeys {
            original: "chalkak/signatures/dev/original/signature.png".into(),
            thumbnail: "chalkak/signatures/dev/thumbnail/signature.png".into(),
        };

        let user = User::create(None, &keys).unwrap();

        assert_eq!(user.email, None);
        assert_eq!(user.status, UserStatus::Active);
        assert_eq!(user.signature_original_storage_key, keys.original);
        assert_eq!(
            user.signature_thumbnail_storage_key.as_deref(),
            Some(keys.thumbnail.as_str())
        );
        assert_eq!(user.pending_signature_upload_id, None);
        assert_eq!(user.signature_processing_status, None);
        assert_eq!(user.signature_processing_started_at, None);
    }

    #[test]
    fn create_missing_original_storage_key_throws_exception() {
        // 원본은 null 키도 막지만 Rust에서는 빈 문자열과 공백으로 같은 분기를 확인한다.
        for original in ["", "  ", "\n"] {
            let err = User::create(
                Some("user@chalkak.test".into()),
                &SignatureStorageKeys {
                    original: original.into(),
                    thumbnail: "chalkak/signatures/dev/thumbnail/signature.png".into(),
                },
            )
            .unwrap_err();
            assert_eq!(err.message, "사인 이미지 업로드 정보가 올바르지 않습니다.");
        }
    }

    #[test]
    fn withdraw_active_user_anonymizes_personal_data() {
        let id = Uuid::new_v4();
        let mut user = fixture(id);
        user.start_signature_processing(Uuid::new_v4(), now())
            .unwrap();

        user.withdraw(now()).unwrap();

        assert_eq!(
            user.email.as_deref(),
            Some(format!("withdrawn+{id}@chalkak.invalid").as_str())
        );
        assert_eq!(
            user.signature_original_storage_key,
            format!("withdrawn/{id}")
        );
        assert_eq!(user.signature_thumbnail_storage_key, None);
        assert_eq!(user.pending_signature_upload_id, None);
        assert_eq!(user.signature_processing_status, None);
        assert_eq!(user.signature_processing_started_at, None);
    }

    #[test]
    fn withdraw_active_user_records_deleted_at() {
        let mut user = fixture(Uuid::new_v4());

        user.withdraw(now()).unwrap();

        assert!(user.is_deleted());
        assert_eq!(user.deleted_at, Some(now()));
    }

    #[test]
    fn withdraw_already_withdrawn_user_throws_exception() {
        let mut user = fixture(Uuid::new_v4());
        user.withdraw(now()).unwrap();

        let err = user.withdraw(now()).unwrap_err();

        assert_eq!(err.message, "이미 탈퇴한 회원입니다.");
    }

    #[test]
    fn is_deleted_active_user_returns_false() {
        assert!(!fixture(Uuid::new_v4()).is_deleted());
    }

    #[test]
    fn is_active_active_user_returns_true() {
        assert!(fixture(Uuid::new_v4()).is_active());
    }

    #[test]
    fn is_active_banned_user_returns_false() {
        assert!(!fixture_banned(Uuid::new_v4()).is_active());
    }

    #[test]
    fn is_active_withdrawn_user_returns_false() {
        let mut user = fixture(Uuid::new_v4());
        user.withdraw(now()).unwrap();

        assert!(!user.is_active());
    }

    #[test]
    fn ban_active_user_changes_status_to_banned() {
        let mut user = fixture(Uuid::new_v4());

        user.ban().unwrap();

        assert_eq!(user.status, UserStatus::Banned);
        assert!(!user.is_active());
    }

    #[test]
    fn ban_banned_user_throws_exception() {
        let mut user = fixture_banned(Uuid::new_v4());

        let err = user.ban().unwrap_err();

        assert_eq!(err.message, "이미 차단된 회원입니다.");
    }

    #[test]
    fn ban_withdrawn_user_throws_exception() {
        let mut user = fixture(Uuid::new_v4());
        user.withdraw(now()).unwrap();

        let err = user.ban().unwrap_err();

        assert_eq!(err.message, "탈퇴한 회원의 상태를 변경할 수 없습니다.");
    }

    #[test]
    fn unban_banned_user_changes_status_to_active() {
        let mut user = fixture_banned(Uuid::new_v4());

        user.unban().unwrap();

        assert_eq!(user.status, UserStatus::Active);
        assert!(user.is_active());
    }

    #[test]
    fn unban_active_user_throws_exception() {
        let mut user = fixture(Uuid::new_v4());

        let err = user.unban().unwrap_err();

        assert_eq!(err.message, "이미 활성 상태인 회원입니다.");
    }

    #[test]
    fn validate_accessible_active_user_does_not_throw() {
        assert!(fixture(Uuid::new_v4()).validate_accessible().is_ok());
    }

    #[test]
    fn validate_accessible_banned_user_throws_forbidden_exception() {
        let err = fixture_banned(Uuid::new_v4())
            .validate_accessible()
            .unwrap_err();

        assert_eq!(err.kind, chalkak_core::error::ErrorKind::Forbidden);
        assert_eq!(err.code, ErrorCode::FORBIDDEN);
    }

    #[test]
    fn validate_accessible_withdrawn_user_throws_unauthorized_exception() {
        let mut user = fixture(Uuid::new_v4());
        user.withdraw(now()).unwrap();

        let err = user.validate_accessible().unwrap_err();

        assert_eq!(err.kind, chalkak_core::error::ErrorKind::Unauthorized);
        assert_eq!(err.code, ErrorCode::UNAUTHORIZED);
    }

    #[test]
    fn start_signature_processing_active_signature_preserves_active_keys() {
        let mut user = fixture(Uuid::new_v4());
        let upload_id = Uuid::new_v4();
        let original = user.signature_original_storage_key.clone();
        let thumbnail = user.signature_thumbnail_storage_key.clone();

        user.start_signature_processing(upload_id, now()).unwrap();

        assert_eq!(user.signature_original_storage_key, original);
        assert_eq!(user.signature_thumbnail_storage_key, thumbnail);
        assert_eq!(user.pending_signature_upload_id, Some(upload_id));
        assert_eq!(
            user.signature_processing_status,
            Some(SignatureProcessingStatus::Processing)
        );
        assert_eq!(user.signature_processing_started_at, Some(now()));
    }

    #[test]
    fn complete_signature_processing_matching_upload_promotes_and_clears_pending() {
        let mut user = fixture(Uuid::new_v4());
        let upload_id = Uuid::new_v4();
        let keys = SignatureStorageKeys {
            original: format!("chalkak/signatures/dev/original/{upload_id}.png"),
            thumbnail: format!("chalkak/signatures/dev/thumbnail/{upload_id}.png"),
        };
        user.start_signature_processing(upload_id, now()).unwrap();

        let completed = user
            .complete_signature_processing(upload_id, &keys)
            .unwrap();

        assert!(completed);
        assert_eq!(user.signature_original_storage_key, keys.original);
        assert_eq!(
            user.signature_thumbnail_storage_key.as_deref(),
            Some(keys.thumbnail.as_str())
        );
        assert_eq!(user.pending_signature_upload_id, None);
        assert_eq!(user.signature_processing_status, None);
        assert_eq!(user.signature_processing_started_at, None);
    }

    #[test]
    fn complete_signature_processing_stale_upload_ignores_callback() {
        let mut user = fixture(Uuid::new_v4());
        let pending_upload_id = Uuid::new_v4();
        let original = user.signature_original_storage_key.clone();
        user.start_signature_processing(pending_upload_id, now())
            .unwrap();
        let stale = SignatureStorageKeys {
            original: "chalkak/signatures/dev/original/stale.png".into(),
            thumbnail: "chalkak/signatures/dev/thumbnail/stale.png".into(),
        };

        let completed = user
            .complete_signature_processing(Uuid::new_v4(), &stale)
            .unwrap();

        assert!(!completed);
        assert_eq!(user.signature_original_storage_key, original);
        assert_eq!(user.pending_signature_upload_id, Some(pending_upload_id));
    }

    #[test]
    fn has_signature_same_storage_key_returns_true() {
        let id = Uuid::new_v4();
        let user = fixture(id);

        assert!(user.has_signature(&format!("signatures/{id}")));
    }

    #[test]
    fn has_signature_different_storage_key_returns_false() {
        let user = fixture(Uuid::new_v4());

        assert!(!user.has_signature(&format!(
            "chalkak/signatures/dev/original/{}.png",
            Uuid::new_v4()
        )));
    }

    #[test]
    fn is_signature_processing_processing_upload_returns_true() {
        let mut user = fixture(Uuid::new_v4());
        let upload_id = Uuid::new_v4();
        user.start_signature_processing(upload_id, now()).unwrap();

        assert!(user.is_signature_processing(upload_id));
    }

    #[test]
    fn is_signature_processing_failed_upload_returns_false() {
        let mut user = fixture(Uuid::new_v4());
        let upload_id = Uuid::new_v4();
        user.start_signature_processing(upload_id, now()).unwrap();
        user.fail_signature_processing(upload_id);

        assert!(!user.is_signature_processing(upload_id));
        assert!(user.is_signature_processing_failed(upload_id));
    }

    #[test]
    fn is_signature_processing_different_upload_returns_false() {
        let mut user = fixture(Uuid::new_v4());
        user.start_signature_processing(Uuid::new_v4(), now())
            .unwrap();

        assert!(!user.is_signature_processing(Uuid::new_v4()));
    }

    #[test]
    fn fail_signature_processing_matching_upload_marks_failed_and_preserves_active_keys() {
        let mut user = fixture(Uuid::new_v4());
        let upload_id = Uuid::new_v4();
        let original = user.signature_original_storage_key.clone();
        user.start_signature_processing(upload_id, now()).unwrap();

        let failed = user.fail_signature_processing(upload_id);

        assert!(failed);
        assert_eq!(user.signature_original_storage_key, original);
        assert_eq!(user.pending_signature_upload_id, Some(upload_id));
        assert_eq!(
            user.signature_processing_status,
            Some(SignatureProcessingStatus::Failed)
        );
    }

    #[test]
    fn start_signature_processing_withdrawn_user_throws_exception() {
        let mut user = fixture(Uuid::new_v4());
        user.withdraw(now()).unwrap();

        let err = user
            .start_signature_processing(Uuid::new_v4(), now())
            .unwrap_err();

        assert_eq!(err.message, "이미 탈퇴한 회원입니다.");
    }

    // 구현의 세부 규칙(원본 분기): 완료 시 새 키 검증, 취소, 이름 변환.
    #[test]
    fn 완료할_때_새_키가_비어_있으면_거절한다() {
        let upload_id = Uuid::new_v4();
        let mut user = fixture(Uuid::new_v4());
        user.start_signature_processing(upload_id, now()).unwrap();

        let err = user
            .complete_signature_processing(
                upload_id,
                &SignatureStorageKeys {
                    original: " ".into(),
                    thumbnail: "t".into(),
                },
            )
            .unwrap_err();

        assert_eq!(err.message, "사인 이미지 업로드 정보가 올바르지 않습니다.");
    }

    #[test]
    fn 처리를_취소하면_대기_정보를_지운다() {
        let upload_id = Uuid::new_v4();
        let mut user = fixture(Uuid::new_v4());
        user.start_signature_processing(upload_id, now()).unwrap();
        user.cancel_signature_processing();

        assert!(!user.is_signature_processing(upload_id));
        assert_eq!(user.pending_signature_upload_id, None);
        assert_eq!(user.signature_processing_started_at, None);
    }

    #[test]
    fn 정지_회원은_탈퇴_검증을_통과한다() {
        let user = fixture_banned(Uuid::new_v4());

        assert!(user.validate_not_withdrawn().is_ok());
    }

    #[test]
    fn 열거형_이름_변환() {
        for status in [UserStatus::Active, UserStatus::Banned] {
            assert_eq!(UserStatus::from_name(status.name()), Some(status));
        }
        for status in [
            SignatureProcessingStatus::Processing,
            SignatureProcessingStatus::Failed,
        ] {
            assert_eq!(
                SignatureProcessingStatus::from_name(status.name()),
                Some(status)
            );
        }
        assert_eq!(UserStatus::from_name("active"), None);
    }

    // SignatureImagePolicyTest
    const MAX_BYTES: i64 = 1024;

    fn policy() -> SignatureImagePolicy {
        SignatureImagePolicy {
            max_bytes: MAX_BYTES,
            allowed_content_types: vec!["image/png".into()],
        }
    }

    fn image(content_type: Option<&str>, byte_size: i64) -> StoredImageMetadata {
        StoredImageMetadata {
            content_type: content_type.map(str::to_owned),
            byte_size,
        }
    }

    #[test]
    fn validate_allowed_image_does_not_throw() {
        assert!(
            policy()
                .validate(&image(Some("image/png"), MAX_BYTES))
                .is_ok()
        );
    }

    #[test]
    fn validate_disallowed_content_type_throws_exception() {
        let err = policy()
            .validate(&image(Some("image/jpeg"), MAX_BYTES))
            .unwrap_err();

        assert_eq!(err.message, "사용할 수 없는 사인 이미지입니다.");
    }

    #[test]
    fn validate_unknown_content_type_throws_exception() {
        let err = policy().validate(&image(None, MAX_BYTES)).unwrap_err();

        assert_eq!(err.message, "사용할 수 없는 사인 이미지입니다.");
    }

    #[test]
    fn validate_too_large_image_throws_exception() {
        let err = policy()
            .validate(&image(Some("image/png"), MAX_BYTES + 1))
            .unwrap_err();

        assert_eq!(err.message, "사용할 수 없는 사인 이미지입니다.");
    }
}
