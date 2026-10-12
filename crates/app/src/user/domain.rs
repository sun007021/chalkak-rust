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
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            deleted_at: None,
        })
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

    fn keys() -> SignatureStorageKeys {
        SignatureStorageKeys {
            original: "chalkak/signatures/test/original/a.png".into(),
            thumbnail: "chalkak/signatures/test/thumbnail/a.png".into(),
        }
    }

    fn user() -> User {
        let mut u = User::create(Some("a@b.c".into()), &keys()).unwrap();
        u.id = Uuid::parse_str("0190f3a2-7b1c-7d2e-8f4a-123456789abc").unwrap();
        u
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    // UserTest#create
    #[test]
    fn 회원을_만들면_활성이고_썸네일이_있다() {
        let u = user();
        assert_eq!(u.status, UserStatus::Active);
        assert_eq!(
            u.signature_thumbnail_storage_key.as_deref(),
            Some("chalkak/signatures/test/thumbnail/a.png")
        );
        assert!(u.signature_processing_status.is_none());
        assert!(!u.is_deleted());
    }

    #[test]
    fn 저장_키가_비어_있으면_만들_수_없다() {
        for (o, t) in [("", "t"), ("o", " "), ("\n", "t")] {
            let err = User::create(
                None,
                &SignatureStorageKeys {
                    original: o.into(),
                    thumbnail: t.into(),
                },
            )
            .unwrap_err();
            assert_eq!(err.message, "사인 이미지 업로드 정보가 올바르지 않습니다.");
        }
    }

    // UserTest#withdraw
    #[test]
    fn 탈퇴하면_개인_정보를_지우고_처리_상태를_비운다() {
        let mut u = user();
        u.start_signature_processing(Uuid::new_v4(), ts("2026-01-01T00:00:00Z"))
            .unwrap();
        u.withdraw(ts("2026-02-01T00:00:00Z")).unwrap();
        assert_eq!(
            u.email.as_deref(),
            Some("withdrawn+0190f3a2-7b1c-7d2e-8f4a-123456789abc@chalkak.invalid")
        );
        assert_eq!(
            u.signature_original_storage_key,
            "withdrawn/0190f3a2-7b1c-7d2e-8f4a-123456789abc"
        );
        assert_eq!(u.signature_thumbnail_storage_key, None);
        assert_eq!(u.pending_signature_upload_id, None);
        assert_eq!(u.signature_processing_status, None);
        assert_eq!(u.signature_processing_started_at, None);
        assert_eq!(u.deleted_at, Some(ts("2026-02-01T00:00:00Z")));
        let err = u.withdraw(ts("2026-03-01T00:00:00Z")).unwrap_err();
        assert_eq!(err.message, "이미 탈퇴한 회원입니다.");
    }

    // UserTest#startSignatureProcessing 등
    #[test]
    fn 사인_처리는_시작_완료_실패_취소를_따른다() {
        let upload = Uuid::new_v4();
        let other = Uuid::new_v4();
        let new_keys = SignatureStorageKeys {
            original: "n/o.png".into(),
            thumbnail: "n/t.png".into(),
        };
        let mut u = user();
        u.start_signature_processing(upload, ts("2026-01-01T00:00:00Z"))
            .unwrap();
        assert!(u.is_signature_processing(upload));
        assert!(!u.is_signature_processing(other));
        assert!(!u.fail_signature_processing(other));
        assert!(!u.complete_signature_processing(other, &new_keys).unwrap());

        assert!(u.complete_signature_processing(upload, &new_keys).unwrap());
        assert_eq!(u.signature_original_storage_key, "n/o.png");
        assert_eq!(
            u.signature_thumbnail_storage_key.as_deref(),
            Some("n/t.png")
        );
        assert_eq!(u.pending_signature_upload_id, None);

        u.start_signature_processing(upload, ts("2026-01-01T00:00:00Z"))
            .unwrap();
        assert!(u.fail_signature_processing(upload));
        assert!(u.is_signature_processing_failed(upload));
        assert!(!u.is_signature_processing(upload));
        assert!(
            !u.fail_signature_processing(upload),
            "실패한 처리는 다시 실패시키지 않는다"
        );

        u.cancel_signature_processing();
        assert!(!u.is_signature_processing_failed(upload));
    }

    #[test]
    fn 탈퇴한_회원은_처리를_시작할_수_없다() {
        let mut u = user();
        u.withdraw(ts("2026-02-01T00:00:00Z")).unwrap();
        let err = u
            .start_signature_processing(Uuid::new_v4(), ts("2026-02-02T00:00:00Z"))
            .unwrap_err();
        assert_eq!(err.message, "이미 탈퇴한 회원입니다.");
    }

    #[test]
    fn 완료할_때_새_키가_비어_있으면_거절한다() {
        let upload = Uuid::new_v4();
        let mut u = user();
        u.start_signature_processing(upload, ts("2026-01-01T00:00:00Z"))
            .unwrap();
        let err = u
            .complete_signature_processing(
                upload,
                &SignatureStorageKeys {
                    original: " ".into(),
                    thumbnail: "t".into(),
                },
            )
            .unwrap_err();
        assert_eq!(err.message, "사인 이미지 업로드 정보가 올바르지 않습니다.");
    }

    // User#validateAccessible / validateNotWithdrawn
    #[test]
    fn 접근_가능_판정은_탈퇴_401_차단_403이다() {
        let mut u = user();
        assert!(u.validate_accessible().is_ok());
        u.status = UserStatus::Banned;
        let err = u.validate_accessible().unwrap_err();
        assert_eq!(
            (err.status(), err.message.as_str()),
            (403, "차단된 회원입니다.")
        );
        assert!(u.validate_not_withdrawn().is_ok(), "정지는 통과");
        u.withdraw(ts("2026-02-01T00:00:00Z")).unwrap();
        let err = u.validate_accessible().unwrap_err();
        assert_eq!(
            (err.status(), err.message.as_str()),
            (401, "유효하지 않은 인증 정보입니다.")
        );
    }

    // SignatureImagePolicyTest
    #[test]
    fn 이미지_정책은_타입과_크기를_본다() {
        let policy = SignatureImagePolicy {
            max_bytes: 1_048_576,
            allowed_content_types: vec!["image/png".into()],
        };
        let image = |t: Option<&str>, n: i64| StoredImageMetadata {
            content_type: t.map(str::to_owned),
            byte_size: n,
        };
        assert!(
            policy
                .validate(&image(Some("image/png"), 1_048_576))
                .is_ok()
        );
        for bad in [
            image(Some("image/png"), 1_048_577),
            image(Some("image/jpeg"), 10),
            image(None, 10),
            image(Some("IMAGE/PNG"), 10),
        ] {
            assert_eq!(
                policy.validate(&bad).unwrap_err().message,
                "사용할 수 없는 사인 이미지입니다."
            );
        }
    }
}
