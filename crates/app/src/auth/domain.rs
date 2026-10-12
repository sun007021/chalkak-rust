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

/// 새로 만들 리프레시 토큰 행의 검증(`RefreshToken#initialize`).
pub fn validate_new_refresh_token(
    token_hash: &str,
    expires_at: Timestamp,
    absolute_expires_at: Timestamp,
) -> Result<(), AppError> {
    const MESSAGE: &str = "리프레시 토큰 정보가 올바르지 않습니다.";
    let is_hex64 = token_hash.len() == 64
        && token_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !is_hex64 || expires_at > absolute_expires_at {
        return Err(AppError::business(ErrorCode::BUSINESS_ERROR, MESSAGE));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn token(expires: &str, absolute: &str) -> RefreshToken {
        RefreshToken {
            id: Uuid::nil(),
            user_id: Uuid::nil(),
            session_id: Uuid::nil(),
            token_hash: "a".repeat(64),
            expires_at: ts(expires),
            absolute_expires_at: ts(absolute),
            rotated_at: None,
            revoked_at: None,
        }
    }

    // RefreshTokenPolicy: 회전해도 절대 만료를 넘지 않는다.
    #[test]
    fn 회전_후_만료는_절대_만료로_잘린다() {
        let policy = RefreshTokenPolicy::new(
            std::time::Duration::from_secs(30 * 86400),
            std::time::Duration::from_secs(90 * 86400),
        )
        .unwrap();
        let now = ts("2026-01-01T00:00:00Z");
        let absolute = policy.absolute_expires_at(now);
        assert_eq!(absolute, ts("2026-04-01T00:00:00Z"));
        assert_eq!(
            policy.next_expires_at(now, absolute),
            ts("2026-01-31T00:00:00Z")
        );
        let late = ts("2026-03-20T00:00:00Z");
        assert_eq!(policy.next_expires_at(late, absolute), absolute);
    }

    #[test]
    fn 정책_값은_양수이고_비활동이_절대를_넘지_않는다() {
        let d = std::time::Duration::from_secs;
        assert!(RefreshTokenPolicy::new(d(0), d(10)).is_err());
        assert!(RefreshTokenPolicy::new(d(11), d(10)).is_err());
        assert!(RefreshTokenPolicy::new(d(10), d(10)).is_ok());
    }

    // RefreshToken#isExpired: 경계 시각도 만료
    #[test]
    fn 만료_판정은_경계_시각을_포함한다() {
        let t = token("2026-01-31T00:00:00Z", "2026-04-01T00:00:00Z");
        assert!(!t.is_expired(ts("2026-01-30T23:59:59Z")));
        assert!(t.is_expired(ts("2026-01-31T00:00:00Z")));
        let t = token("2026-01-31T00:00:00Z", "2026-01-31T00:00:00Z");
        assert!(t.is_expired(ts("2026-01-31T00:00:00Z")));
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
