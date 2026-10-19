//! 인증 도메인 값. 원본 `auth.domain` 패키지에 대응한다.

use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use chalkak_core::error::{AppError, ErrorCode};

/// `SocialProvider`: 선언 순서가 곧 서수다(`@JsonCreator` 없는 Jackson enum은 정수도 서수로 받는다).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SocialProvider {
    Google,
    Kakao,
    Apple,
}

impl SocialProvider {
    pub const ALL: [SocialProvider; 3] = [
        SocialProvider::Google,
        SocialProvider::Kakao,
        SocialProvider::Apple,
    ];

    /// `Enum.name()`. 로그인 제공자 이름은 응답 메시지와 DB enum 값에 그대로 쓰인다.
    pub fn name(self) -> &'static str {
        match self {
            SocialProvider::Google => "GOOGLE",
            SocialProvider::Kakao => "KAKAO",
            SocialProvider::Apple => "APPLE",
        }
    }

    /// `Enum.valueOf`: 정확히 같은 이름만 받는다.
    pub fn from_name(name: &str) -> Option<SocialProvider> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    pub fn from_ordinal(ordinal: usize) -> Option<SocialProvider> {
        Self::ALL.get(ordinal).copied()
    }
}

/// 제공자 ID Token 검증을 마친 소셜 식별 정보(`VerifiedSocialIdentity`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedSocialIdentity {
    pub provider: SocialProvider,
    pub subject: String,
    pub email: Option<String>,
}

/// 회원가입 토큰 검증 결과(`VerifiedSocialSignupToken`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedSocialSignupToken {
    pub provider: SocialProvider,
    pub subject: String,
    pub upload_id: Uuid,
    pub email: Option<String>,
    pub token_id: String,
    pub expires_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedSocialSignupToken {
    pub value: String,
    pub expires_at: Timestamp,
}

/// 액세스 토큰 발급 결과. `expires_in`은 `Duration.toSeconds()`로 응답에 나간다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedAccessToken {
    pub value: String,
    pub expires_in_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedRefreshToken {
    pub value: String,
    pub expires_in_seconds: i64,
    /// 이 토큰이 속한 로그인(계보) 식별자. 액세스 토큰의 `session_id` 클레임이 된다.
    pub session_id: Uuid,
}

/// 평문 리프레시 토큰과 저장용 해시(`GeneratedRefreshToken`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedRefreshToken {
    pub value: String,
    pub token_hash: String,
}

/// `RefreshTokenPolicy`: 비활동 만료는 회전할 때마다 갱신하고 절대 만료는 최초 로그인 시점에 고정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshTokenPolicy {
    inactivity: SignedDuration,
    absolute: SignedDuration,
}

impl RefreshTokenPolicy {
    pub fn new(
        inactivity: std::time::Duration,
        absolute: std::time::Duration,
    ) -> Result<RefreshTokenPolicy, String> {
        if inactivity.is_zero() || absolute.is_zero() {
            return Err("Refresh token durations must be positive".to_owned());
        }
        if inactivity > absolute {
            return Err(
                "Refresh token inactivity expiration must not exceed absolute expiration"
                    .to_owned(),
            );
        }
        let to_signed = |d: std::time::Duration| {
            SignedDuration::try_from(d).map_err(|e| format!("만료 기간이 너무 깁니다: {e}"))
        };
        Ok(RefreshTokenPolicy {
            inactivity: to_signed(inactivity)?,
            absolute: to_signed(absolute)?,
        })
    }

    /// 회전 후의 비활동 만료. 절대 만료를 넘으면 절대 만료로 자른다.
    pub fn next_expires_at(&self, now: Timestamp, absolute_expires_at: Timestamp) -> Timestamp {
        let expires_at = now
            .saturating_add(self.inactivity)
            .unwrap_or(Timestamp::MAX);
        if expires_at > absolute_expires_at {
            absolute_expires_at
        } else {
            expires_at
        }
    }

    pub fn absolute_expires_at(&self, now: Timestamp) -> Timestamp {
        now.saturating_add(self.absolute).unwrap_or(Timestamp::MAX)
    }
}

/// `UserRefreshToken`(`RefreshToken` 상태와 판정).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshToken {
    pub id: Uuid,
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub token_hash: String,
    pub expires_at: Timestamp,
    pub absolute_expires_at: Timestamp,
    pub rotated_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

impl RefreshToken {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// 회전된 토큰이 다시 들어오면 예외 없이 탈취로 본다.
    pub fn is_reused(&self) -> bool {
        self.rotated_at.is_some()
    }

    /// 비활동 만료와 절대 만료 중 하나라도 지났으면 만료다(경계 시각 포함).
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now >= self.expires_at || now >= self.absolute_expires_at
    }
}

const ENCRYPTED_REFRESH_TOKEN_MAX_LENGTH: usize = 4096;
const SIGNUP_TOKEN_ID_MAX_LENGTH: usize = 36;

/// `^[0-9a-f]{64}$`.
fn is_lower_hex64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Java `String.isBlank`.
fn is_blank(text: &str) -> bool {
    text.chars().all(crate::web::binding::is_java_whitespace)
}

fn invalid(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

/// `SocialAccount.create`의 식별 정보 검증: subject HMAC은 64자리 소문자 16진수여야 한다.
pub fn validate_social_account(subject_hmac: &str) -> Result<(), AppError> {
    if !is_lower_hex64(subject_hmac) {
        return Err(invalid("소셜 계정 식별 정보가 올바르지 않습니다."));
    }
    Ok(())
}

/// `AppleAuthorization.create`: Apple 소셜 계정에만 붙고, 암호문은 비어 있지 않아야 하며 저장 길이(4096)를 넘지 않는다.
pub fn validate_apple_authorization(
    provider: SocialProvider,
    encrypted_refresh_token: &str,
) -> Result<(), AppError> {
    if provider != SocialProvider::Apple
        || is_blank(encrypted_refresh_token)
        || crate::common::java_len(encrypted_refresh_token) > ENCRYPTED_REFRESH_TOKEN_MAX_LENGTH
    {
        return Err(invalid("Apple 인증 정보가 올바르지 않습니다."));
    }
    Ok(())
}

/// `ConsumedSignupToken.create`: jti는 비어 있지 않고 36자 이하여야 한다.
pub fn validate_consumed_signup_token(jti: &str) -> Result<(), AppError> {
    if is_blank(jti) || crate::common::java_len(jti) > SIGNUP_TOKEN_ID_MAX_LENGTH {
        return Err(invalid("회원가입 토큰 식별자가 올바르지 않습니다."));
    }
    Ok(())
}

/// `PendingAppleAuthorization.create`.
pub fn validate_pending_apple_authorization(
    subject_hmac: &str,
    encrypted_refresh_token: &str,
) -> Result<(), AppError> {
    if !is_lower_hex64(subject_hmac)
        || is_blank(encrypted_refresh_token)
        || crate::common::java_len(encrypted_refresh_token) > ENCRYPTED_REFRESH_TOKEN_MAX_LENGTH
    {
        return Err(invalid("임시 Apple 인증 정보가 올바르지 않습니다."));
    }
    Ok(())
}

/// 새로 만들 리프레시 토큰 행의 검증(`RefreshToken#initialize`).
pub fn validate_new_refresh_token(
    token_hash: &str,
    expires_at: Timestamp,
    absolute_expires_at: Timestamp,
) -> Result<(), AppError> {
    const MESSAGE: &str = "리프레시 토큰 정보가 올바르지 않습니다.";
    if !is_lower_hex64(token_hash) || expires_at > absolute_expires_at {
        return Err(AppError::business(ErrorCode::BUSINESS_ERROR, MESSAGE));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn now() -> Timestamp {
        ts("2026-09-02T00:00:00Z")
    }

    fn plus(t: Timestamp, seconds: i64) -> Timestamp {
        t.checked_add(SignedDuration::from_secs(seconds)).unwrap()
    }

    fn token(expires: Timestamp, absolute: Timestamp) -> RefreshToken {
        RefreshToken {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            session_id: Uuid::nil(),
            token_hash: "a".repeat(64),
            expires_at: expires,
            absolute_expires_at: absolute,
            rotated_at: None,
            revoked_at: None,
        }
    }

    fn days(n: u64) -> Duration {
        Duration::from_secs(n * 86400)
    }

    fn policy() -> RefreshTokenPolicy {
        RefreshTokenPolicy::new(days(30), days(90)).unwrap()
    }

    // RefreshTokenPolicyTest
    #[test]
    fn next_expires_at_before_absolute_expiry_returns_inactivity_expiry() {
        let absolute = plus(now(), 30 * 86400 + 1);
        assert_eq!(
            policy().next_expires_at(now(), absolute),
            plus(now(), 30 * 86400)
        );
    }

    #[test]
    fn next_expires_at_exactly_at_absolute_expiry_returns_absolute_expiry() {
        let absolute = plus(now(), 30 * 86400);
        assert_eq!(policy().next_expires_at(now(), absolute), absolute);
    }

    #[test]
    fn next_expires_at_after_absolute_expiry_returns_absolute_expiry() {
        let absolute = plus(now(), 30 * 86400 - 1);
        assert_eq!(policy().next_expires_at(now(), absolute), absolute);
    }

    #[test]
    fn absolute_expires_at_now_returns_now_plus_absolute_expiration() {
        assert_eq!(policy().absolute_expires_at(now()), plus(now(), 90 * 86400));
    }

    #[test]
    fn 정책_값은_양수이고_비활동이_절대를_넘지_않는다() {
        let d = Duration::from_secs;
        assert!(RefreshTokenPolicy::new(d(0), d(10)).is_err());
        assert!(RefreshTokenPolicy::new(d(11), d(10)).is_err());
        assert!(RefreshTokenPolicy::new(d(10), d(10)).is_ok());
    }

    // RefreshTokenTest
    #[test]
    fn is_reused_never_rotated_returns_false() {
        assert!(!token(plus(now(), 10), plus(now(), 100)).is_reused());
    }

    #[test]
    fn is_reused_rotated_returns_true() {
        let mut t = token(plus(now(), 10), plus(now(), 100));
        t.rotated_at = Some(now());
        assert!(t.is_reused());
    }

    #[test]
    fn is_expired_before_both_expiries_returns_false() {
        assert!(!token(plus(now(), 1), plus(now(), 100)).is_expired(now()));
    }

    #[test]
    fn is_expired_at_inactivity_expiry_returns_true() {
        assert!(token(now(), plus(now(), 100)).is_expired(now()));
    }

    #[test]
    fn is_expired_after_absolute_expiry_returns_true() {
        assert!(token(plus(now(), -10), plus(now(), -5)).is_expired(now()));
    }

    #[test]
    fn 절대_만료가_먼저_지나도_만료로_본다() {
        assert!(token(plus(now(), 50), now()).is_expired(now()));
    }

    #[test]
    fn 새_토큰_행은_해시_형식과_만료_순서를_검증한다() {
        let a = ts("2026-01-01T00:00:00Z");
        let b = ts("2026-01-02T00:00:00Z");
        assert!(validate_new_refresh_token(&"a".repeat(64), a, b).is_ok());
        assert!(validate_new_refresh_token(&"A".repeat(64), a, b).is_err());
        assert!(validate_new_refresh_token("abc", a, b).is_err());
        assert!(validate_new_refresh_token(&"a".repeat(64), b, a).is_err());
    }

    // SocialAccountTest
    #[test]
    fn create_valid_social_identity_connects_user() {
        assert!(validate_social_account(&"a".repeat(64)).is_ok());
        assert!(validate_social_account(&"0123456789abcdef".repeat(4)).is_ok());
    }

    #[test]
    fn create_invalid_subject_hmac_throws_exception() {
        for bad in [
            "".to_owned(),
            " ".to_owned(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
        ] {
            let error = validate_social_account(&bad).unwrap_err();
            assert_eq!(error.message, "소셜 계정 식별 정보가 올바르지 않습니다.");
        }
    }

    // AppleAuthorizationTest
    #[test]
    fn create_valid_authorization_connects_apple_account() {
        assert!(validate_apple_authorization(SocialProvider::Apple, "encrypted").is_ok());
    }

    #[test]
    fn create_maximum_length_encrypted_refresh_token_creates_authorization() {
        assert!(validate_apple_authorization(SocialProvider::Apple, &"a".repeat(4096)).is_ok());
    }

    #[test]
    fn create_too_long_encrypted_refresh_token_throws_exception() {
        let error =
            validate_apple_authorization(SocialProvider::Apple, &"a".repeat(4097)).unwrap_err();
        assert_eq!(error.message, "Apple 인증 정보가 올바르지 않습니다.");
    }

    #[test]
    fn create_missing_encrypted_refresh_token_throws_exception() {
        for blank in ["", " ", "\t\n"] {
            assert!(validate_apple_authorization(SocialProvider::Apple, blank).is_err());
        }
    }

    #[test]
    fn create_non_apple_account_throws_exception() {
        assert!(validate_apple_authorization(SocialProvider::Google, "encrypted").is_err());
        assert!(validate_apple_authorization(SocialProvider::Kakao, "encrypted").is_err());
    }

    // ConsumedSignupTokenTest
    #[test]
    fn create_valid_arguments_creates_token() {
        assert!(validate_consumed_signup_token("0f6a3e7e-1b0e-4b6a-8c4e-0a9a1f0c2d11").is_ok());
    }

    #[test]
    fn create_jti_at_max_length_creates_token() {
        assert!(validate_consumed_signup_token(&"a".repeat(36)).is_ok());
    }

    #[test]
    fn create_jti_too_long_throws_business_exception() {
        let error = validate_consumed_signup_token(&"a".repeat(37)).unwrap_err();
        assert_eq!(error.message, "회원가입 토큰 식별자가 올바르지 않습니다.");
    }

    #[test]
    fn create_blank_jti_throws_business_exception() {
        for blank in ["", " "] {
            assert!(validate_consumed_signup_token(blank).is_err());
        }
    }

    // PendingAppleAuthorizationTest
    #[test]
    fn create_valid_arguments_creates_authorization() {
        assert!(
            validate_pending_apple_authorization(&"a".repeat(64), "encrypted-refresh-token")
                .is_ok()
        );
    }

    #[test]
    fn create_blank_encrypted_refresh_token_throws_business_exception() {
        for blank in ["", " "] {
            let error = validate_pending_apple_authorization(&"a".repeat(64), blank).unwrap_err();
            assert_eq!(error.message, "임시 Apple 인증 정보가 올바르지 않습니다.");
        }
    }

    #[test]
    fn create_too_long_encrypted_refresh_token_throws_business_exception() {
        assert!(validate_pending_apple_authorization(&"a".repeat(64), &"a".repeat(4097)).is_err());
        assert!(validate_pending_apple_authorization(&"a".repeat(64), &"a".repeat(4096)).is_ok());
    }

    #[test]
    fn create_invalid_subject_hmac_throws_business_exception() {
        for bad in [
            "".to_owned(),
            " ".to_owned(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
        ] {
            let error = validate_pending_apple_authorization(&bad, "encrypted").unwrap_err();
            assert_eq!(error.message, "임시 Apple 인증 정보가 올바르지 않습니다.");
        }
    }

    #[test]
    fn 제공자_이름과_서수() {
        assert_eq!(
            SocialProvider::from_name("APPLE"),
            Some(SocialProvider::Apple)
        );
        assert_eq!(SocialProvider::from_name("apple"), None);
        assert_eq!(SocialProvider::from_ordinal(1), Some(SocialProvider::Kakao));
        assert_eq!(SocialProvider::from_ordinal(3), None);
    }
}
