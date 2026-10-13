//! 원본 `Admin`, `RefreshTokenPolicy`, 리프레시 토큰 생성·해시(`SecureRandomRefreshTokenProvider`).

use std::io::Read;
use std::time::Duration;

use chalkak_core::error::{AppError, ErrorCode};
use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::admin::binding::{java_trim, utf16_len};
use crate::web::binding::is_java_whitespace;

const MAX_USERNAME_LENGTH: usize = 100;
const MAX_PASSWORD_HASH_LENGTH: usize = 255;
const INVALID_ACCOUNT_MESSAGE: &str = "관리자 계정 정보가 올바르지 않습니다.";

/// 존재하지 않는 계정에도 BCrypt 비교를 한 번 수행하기 위한 더미 해시(`AdminAuthenticationService.DUMMY_PASSWORD_HASH`).
pub const DUMMY_PASSWORD_HASH: &str =
    "$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";

/// BCrypt가 입력으로 받는 비밀번호의 최대 바이트 수. 넘는 값은 로그인 실패로 다룬다.
const BCRYPT_MAX_PASSWORD_BYTES: usize = 72;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admin {
    pub id: Option<Uuid>,
    pub username: String,
    pub password_hash: String,
}

impl Admin {
    /// `Admin.create`: 사용자명(공백뿐이거나 100자 초과 불가)과 BCrypt 해시 형식을 검증한다.
    pub fn create(username: &str, password_hash: &str) -> Result<Admin, AppError> {
        let invalid = || AppError::business(ErrorCode::BUSINESS_ERROR, INVALID_ACCOUNT_MESSAGE);
        if username.chars().all(is_java_whitespace) || utf16_len(username) > MAX_USERNAME_LENGTH {
            return Err(invalid());
        }
        if password_hash.chars().all(is_java_whitespace)
            || utf16_len(password_hash) > MAX_PASSWORD_HASH_LENGTH
            || !is_bcrypt_hash(password_hash)
        {
            return Err(invalid());
        }
        Ok(Admin {
            id: None,
            username: username.to_owned(),
            password_hash: password_hash.to_owned(),
        })
    }
}

/// `\$2[aby]\$\d{2}\$[./A-Za-z0-9]{53}` 전체 일치.
pub fn is_bcrypt_hash(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 60
        && b[0] == b'$'
        && b[1] == b'2'
        && matches!(b[2], b'a' | b'b' | b'y')
        && b[3] == b'$'
        && b[4].is_ascii_digit()
        && b[5].is_ascii_digit()
        && b[6] == b'$'
        && b[7..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'.' || *c == b'/')
}

/// `BCryptPasswordEncoder.matches`. 72바이트를 넘는 비밀번호와 형식이 틀린 해시는 일치하지 않는 것으로 본다.
pub fn password_matches(raw_password: &str, password_hash: &str) -> bool {
    if raw_password.len() > BCRYPT_MAX_PASSWORD_BYTES || !is_bcrypt_hash(password_hash) {
        return false;
    }
    bcrypt::verify(raw_password, password_hash).unwrap_or(false)
}

/// 비밀번호의 BCrypt 해시(`BCryptPasswordEncoder.encode`, 강도 10).
pub fn encode_password(raw_password: &str) -> Result<String, AppError> {
    bcrypt::hash_with_result(raw_password, 10)
        .map(|h| h.format_for_version(bcrypt::Version::TwoA))
        .map_err(|_| AppError::business(ErrorCode::BUSINESS_ERROR, INVALID_ACCOUNT_MESSAGE))
}

// ---------------------------------------------------------------------------------------------
// 리프레시 토큰
// ---------------------------------------------------------------------------------------------

/// 리프레시 토큰 수명 정책(`RefreshTokenPolicy`). 비활동 만료는 회전마다 갱신하고 절대 만료는 최초 로그인에 고정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshTokenPolicy {
    inactivity: Duration,
    absolute: Duration,
}

impl RefreshTokenPolicy {
    pub fn new(inactivity: Duration, absolute: Duration) -> Result<RefreshTokenPolicy, String> {
        if inactivity.is_zero() || absolute.is_zero() {
            return Err("Refresh token durations must be positive".to_owned());
        }
        if inactivity > absolute {
            return Err(
                "Refresh token inactivity expiration must not exceed absolute expiration"
                    .to_owned(),
            );
        }
        Ok(RefreshTokenPolicy {
            inactivity,
            absolute,
        })
    }

    /// 최초 로그인 시점에 한 번 정해지는 절대 만료 시각.
    pub fn absolute_expires_at(&self, now: Timestamp) -> Timestamp {
        add(now, self.absolute)
    }

    /// 회전 후의 비활동 만료 시각. 절대 만료를 넘으면 절대 만료 시각으로 자른다.
    pub fn next_expires_at(&self, now: Timestamp, absolute_expires_at: Timestamp) -> Timestamp {
        add(now, self.inactivity).min(absolute_expires_at)
    }
}

fn add(at: Timestamp, duration: Duration) -> Timestamp {
    let duration = SignedDuration::try_from(duration).unwrap_or(SignedDuration::MAX);
    at.checked_add(duration).unwrap_or(Timestamp::MAX)
}

/// 방금 만든 리프레시 토큰: 평문(응답에만 나간다)과 저장용 해시.
#[derive(Clone, Debug)]
pub struct GeneratedRefreshToken {
    pub value: String,
    pub token_hash: String,
}

/// 32바이트 난수를 URL-safe Base64(패딩 없음)로 쓴 불투명 토큰.
pub fn generate_refresh_token() -> GeneratedRefreshToken {
    let value = base64url(&random_bytes::<32>());
    let token_hash = hash_refresh_token(&value);
    GeneratedRefreshToken { value, token_hash }
}

/// 저장용 해시: UTF-8 바이트의 SHA-256 소문자 16진수.
pub fn hash_refresh_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let from_os = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if from_os.is_err() {
        // 운영체제 난수를 읽지 못하면 UUID v4(운영체제 난수 기반)로 채운다.
        for chunk in bytes.chunks_mut(16) {
            let random = *Uuid::new_v4().as_bytes();
            chunk.copy_from_slice(&random[..chunk.len()]);
        }
    }
    bytes
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}

/// 저장된 리프레시 토큰 한 행.
#[derive(Clone, Debug)]
pub struct RefreshTokenRecord {
    pub id: Uuid,
    pub admin_id: Uuid,
    pub session_id: Uuid,
    pub expires_at: Timestamp,
    pub absolute_expires_at: Timestamp,
    pub rotated_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

impl RefreshTokenRecord {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// 이미 회전된 토큰이 다시 들어왔다 = 탈취로 본다.
    pub fn is_reused(&self) -> bool {
        self.rotated_at.is_some()
    }

    /// 비활동 만료와 절대 만료 중 하나라도 지났으면 만료.
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now >= self.expires_at || now >= self.absolute_expires_at
    }
}

/// 이름 표시용: 응답의 `expiresIn`은 `Duration.toSeconds()`(내림)이다.
pub fn whole_seconds(duration: SignedDuration) -> i64 {
    duration.as_secs()
}

/// 앞뒤 공백을 뗀 사용자명(테스트 보조).
pub fn trimmed(text: &str) -> &str {
    java_trim(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::error::ErrorKind;

    const HASH: &str = "$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";

    fn message(result: Result<Admin, AppError>) -> String {
        let e = result.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Business);
        e.message
    }

    // AdminTest
    #[test]
    fn 사용자명과_해시로_관리자를_만든다() {
        let admin = Admin::create("dev-admin", HASH).unwrap();
        assert_eq!(admin.username, "dev-admin");
        assert_eq!(admin.password_hash, HASH);
        assert!(Admin::create(&"a".repeat(100), HASH).is_ok());
    }

    #[test]
    fn 사용자명이_비었거나_길면_거절한다() {
        for username in ["", " "] {
            assert_eq!(
                message(Admin::create(username, HASH)),
                "관리자 계정 정보가 올바르지 않습니다."
            );
        }
        assert_eq!(
            message(Admin::create(&"a".repeat(101), HASH)),
            "관리자 계정 정보가 올바르지 않습니다."
        );
    }

    #[test]
    fn 해시가_비었거나_길거나_bcrypt_형식이_아니면_거절한다() {
        for hash in ["", " ", "plain-password"] {
            assert_eq!(
                message(Admin::create("dev-admin", hash)),
                "관리자 계정 정보가 올바르지 않습니다."
            );
        }
        assert_eq!(
            message(Admin::create("dev-admin", &"a".repeat(256))),
            "관리자 계정 정보가 올바르지 않습니다."
        );
        // 형식: $2[aby]$dd$ + 53자
        assert!(is_bcrypt_hash(
            "$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6"
        ));
        assert!(!is_bcrypt_hash(&HASH.replace("$2a$", "$2x$")));
        assert!(!is_bcrypt_hash(&format!("{HASH}x")));
        assert!(!is_bcrypt_hash(&HASH[..59]));
        assert!(!is_bcrypt_hash(&HASH.replace("$10$", "$1a$")));
    }

    // AdminAuthenticationServiceTest#login_passwordWithinByteLimit_succeeds / ...ExceedsByteLimit...
    #[test]
    fn 비밀번호는_72바이트까지_일치_검사한다() {
        for len in [71usize, 72] {
            let password = "a".repeat(len);
            let hash = bcrypt::hash(&password, 4).unwrap();
            assert!(password_matches(&password, &hash), "{len}");
        }
        let hash = bcrypt::hash("test-password", 4).unwrap();
        assert!(password_matches("test-password", &hash));
        assert!(!password_matches("wrong", &hash));
        assert!(!password_matches(&"a".repeat(73), &hash));
        assert!(!password_matches(&"가".repeat(73), &hash));
        assert!(!password_matches("x", "not-a-hash"));
    }

    #[test]
    fn 더미_해시는_형식이_맞다() {
        assert!(is_bcrypt_hash(DUMMY_PASSWORD_HASH));
        assert!(!password_matches("operator-password", DUMMY_PASSWORD_HASH));
    }

    // 하네스 시나리오 admin__auth: $2y$ 해시도 검증된다.
    #[test]
    fn y_접두_해시도_검증한다() {
        assert!(password_matches(
            "operator-password",
            "$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6"
        ));
    }

    // RefreshTokenPolicyTest 상당
    #[test]
    fn 정책은_회전해도_절대_만료를_넘기지_않는다() {
        let day = Duration::from_secs(86_400);
        let policy = RefreshTokenPolicy::new(30 * day, 90 * day).unwrap();
        let t0: Timestamp = "2026-10-12T00:00:00Z".parse().unwrap();
        let absolute = policy.absolute_expires_at(t0);
        assert_eq!(absolute, "2027-01-10T00:00:00Z".parse().unwrap());
        assert_eq!(
            policy.next_expires_at(t0, absolute),
            "2026-11-11T00:00:00Z".parse().unwrap()
        );
        let late: Timestamp = "2026-12-20T00:00:00Z".parse().unwrap();
        assert_eq!(policy.next_expires_at(late, absolute), absolute);
    }

    #[test]
    fn 정책은_양수이고_비활동이_절대보다_길_수_없다() {
        let day = Duration::from_secs(86_400);
        assert!(RefreshTokenPolicy::new(Duration::ZERO, day).is_err());
        assert!(RefreshTokenPolicy::new(day, Duration::ZERO).is_err());
        assert!(RefreshTokenPolicy::new(2 * day, day).is_err());
        assert!(RefreshTokenPolicy::new(day, day).is_ok());
    }

    #[test]
    fn 토큰은_43자_base64url이고_해시는_64자_hex다() {
        let token = generate_refresh_token();
        assert_eq!(token.value.len(), 43);
        assert!(
            token
                .value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(token.token_hash.len(), 64);
        assert_eq!(token.token_hash, hash_refresh_token(&token.value));
        assert_ne!(token.value, generate_refresh_token().value);
        // SHA-256("abc")
        assert_eq!(
            hash_refresh_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(base64url(&[0xfb, 0xff, 0xfe]), "-__-");
        assert_eq!(base64url(&[0]), "AA");
        assert_eq!(base64url(&[0, 0]), "AAA");
    }

    #[test]
    fn 토큰_행의_상태_판정() {
        let t = |s: &str| -> Timestamp { s.parse().unwrap() };
        let mut record = RefreshTokenRecord {
            id: Uuid::new_v4(),
            admin_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            expires_at: t("2026-11-01T00:00:00Z"),
            absolute_expires_at: t("2027-01-01T00:00:00Z"),
            rotated_at: None,
            revoked_at: None,
        };
        assert!(!record.is_expired(t("2026-10-31T23:59:59Z")));
        assert!(record.is_expired(t("2026-11-01T00:00:00Z")));
        record.expires_at = t("2027-01-01T00:00:00Z");
        assert!(record.is_expired(t("2027-01-01T00:00:00Z")));
        assert!(!record.is_reused() && !record.is_revoked());
        record.rotated_at = Some(t("2026-10-12T00:00:00Z"));
        record.revoked_at = Some(t("2026-10-12T00:00:00Z"));
        assert!(record.is_reused() && record.is_revoked());
    }
}
