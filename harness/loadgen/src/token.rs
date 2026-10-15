//! 액세스 토큰 발급과 날짜 계산. 하네스(`harness/parity/src/token.rs`)와 같은 헤더·클레임·HS256 서명이다.
//! 서버의 `ACCESS_TOKEN_SECRET`(hex)만 알면 로그인 없이 회원·관리자 토큰을 만든다.

use hmac::{Hmac, Mac};
use sha2::Sha256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    User,
    Admin,
}

pub fn base64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex 길이가 홀수입니다".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// 헤더 `{"typ":"JWT","alg":"HS256"}`에 `payload`를 HS256으로 서명한 JWT. 회원가입 토큰처럼 액세스 토큰이 아닌 클레임에 쓴다.
pub fn hs256_jwt(key: &[u8], payload: &serde_json::Value) -> String {
    let header = r#"{"typ":"JWT","alg":"HS256"}"#;
    let signing_input = format!(
        "{}.{}",
        base64url(header.as_bytes()),
        base64url(payload.to_string().as_bytes())
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC은 모든 키 길이를 받는다");
    mac.update(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        base64url(&mac.finalize().into_bytes())
    )
}

pub struct Signer {
    key: Vec<u8>,
}

impl Signer {
    pub fn from_hex(secret_hex: &str) -> Result<Signer, String> {
        Ok(Signer {
            key: hex_decode(secret_hex)?,
        })
    }

    pub fn token(&self, subject: &str, scope: Scope, iat: i64, ttl_secs: i64, jti: &str) -> String {
        let header = r#"{"typ":"JWT","alg":"HS256"}"#;
        let scope = match scope {
            Scope::User => "USER",
            Scope::Admin => "ADMIN",
        };
        let payload = serde_json::json!({
            "iss": "chalkak-backend",
            "aud": ["chalkak-access"],
            "sub": subject,
            "iat": iat,
            "exp": iat + ttl_secs,
            "jti": jti,
            "purpose": "ACCESS",
            "scope": scope,
        });
        let signing_input = format!(
            "{}.{}",
            base64url(header.as_bytes()),
            base64url(payload.to_string().as_bytes())
        );
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC은 모든 키 길이를 받는다");
        mac.update(signing_input.as_bytes());
        format!(
            "{signing_input}.{}",
            base64url(&mac.finalize().into_bytes())
        )
    }
}

/// 회원 UUID. 시드(`seed.rs`)가 같은 규칙으로 만든다.
pub fn user_uuid(n: u64) -> String {
    format!("01990000-0000-7000-8000-{n:012x}")
}

/// 유닉스 초를 한국 시각의 (연, 월, 일)로.
pub fn kst_date(unix_secs: i64) -> (i32, u32, u32) {
    civil_from_days((unix_secs + 9 * 3600).div_euclid(86_400))
}

/// 1970-01-01부터의 일수 -> (연, 월, 일). Howard Hinnant의 알고리즘.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// 한국 시각 기준 `days_ago`일 전 날짜를 `YYYY-MM-DD`로.
pub fn kst_date_string(unix_secs: i64, days_ago: i64) -> String {
    let (y, m, d) = kst_date(unix_secs - days_ago * 86_400);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url이_rfc4648_벡터와_같다() {
        // RFC 4648 section 10 (패딩 없음, URL-safe)
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn 토큰이_세_부분이고_서명이_검증된다() {
        let signer = Signer::from_hex("00ff").unwrap();
        let t = signer.token("u1", Scope::User, 1000, 900, "j");
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let mut mac = Hmac::<Sha256>::new_from_slice(&[0x00, 0xff]).unwrap();
        mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
        assert_eq!(parts[2], base64url(&mac.finalize().into_bytes()));
        assert_eq!(
            parts[0],
            base64url(br#"{"typ":"JWT","alg":"HS256"}"#),
            "원본과 같은 헤더"
        );
    }

    #[test]
    fn 한국_날짜는_자정이_아홉시간_앞선다() {
        // 2026-10-12T14:59:59Z = KST 23:59:59, 15:00:00Z = 다음 날
        let base = 1_791_817_199; // 2026-10-12T14:59:59Z
        assert_eq!(kst_date_string(base, 0), "2026-10-12");
        assert_eq!(kst_date_string(base + 1, 0), "2026-10-13");
        assert_eq!(kst_date_string(base, 12), "2026-09-30");
        assert_eq!(kst_date(0), (1970, 1, 1));
    }

    #[test]
    fn 회원_uuid_형식() {
        assert_eq!(user_uuid(1), "01990000-0000-7000-8000-000000000001");
        assert_eq!(user_uuid(255), "01990000-0000-7000-8000-0000000000ff");
    }
}
