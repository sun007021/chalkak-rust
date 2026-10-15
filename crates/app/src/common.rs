//! 여러 도메인이 함께 쓰는 작은 도구.

use chalkak_db::{DbError, Pool};
use uuid::Uuid;

use crate::web::ApiError;

/// 트랜잭션 하나 안에서 `f`를 실행한다. `Ok`면 커밋, `Err`면 롤백한다(패닉이면 롤백).
/// `@Transactional`의 기본 규칙이다.
pub fn transact<T>(
    pool: &Pool,
    f: impl FnOnce(&mut chalkak_db::Tx<'_>) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    transact_unless(pool, |_| false, f)
}

/// [`transact`]와 같되, `Err`가 `commit_on_error`에 맞으면 롤백하지 않고 커밋한 뒤 오류를 돌려준다
/// (`@Transactional(noRollbackFor = ...)`).
pub fn transact_unless<T>(
    pool: &Pool,
    commit_on_error: impl Fn(&ApiError) -> bool,
    f: impl FnOnce(&mut chalkak_db::Tx<'_>) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let mut conn = pool.get()?;
    let mut tx = conn.begin().map_err(DbError::from)?;
    match f(&mut tx) {
        Ok(value) => {
            tx.commit().map_err(DbError::from)?;
            Ok(value)
        }
        Err(error) => {
            if commit_on_error(&error) {
                tx.commit().map_err(DbError::from)?;
            } else {
                // 롤백 실패는 원래 오류를 가리지 않는다.
                let _ = tx.rollback();
            }
            Err(error)
        }
    }
}

/// Java `UUID.fromString`이 받는 문자열을 `Uuid`로 바꾼다.
///
/// 표준 형식만 받는 것이 아니다. 길이 36 이하, `-`가 정확히 4개, 다섯 성분 각각이 `Long.parseLong(성분, 16)`을 통과하면
/// 된다(앞의 `+`와 앞자리 0 허용, 값은 i64 범위). 예: `1-2-3-4-5`는 `00000001-0002-0003-0004-000000000005`.
/// 액세스 토큰 검증기(`chalkak-jwt`)가 같은 규칙으로 `sub`를 통과시키므로, 통과한 값을 같은 식별자로 읽으려는 것이다.
///
/// 각 성분은 JDK 구현처럼 최상위 비트를 자른다: 첫 성분 32비트, 둘·셋째 16비트, 넷째 16비트, 다섯째 48비트.
pub fn java_uuid_from_string(text: &str) -> Option<Uuid> {
    if text.len() > 36 {
        return None;
    }
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let mut v = [0u64; 5];
    for (slot, part) in v.iter_mut().zip(&parts) {
        *slot = parse_long_radix16(part)?;
    }
    let most = ((v[0] & 0xffff_ffff) << 32) | ((v[1] & 0xffff) << 16) | (v[2] & 0xffff);
    let least = ((v[3] & 0xffff) << 48) | (v[4] & 0xffff_ffff_ffff);
    Some(Uuid::from_u64_pair(most, least))
}

/// `Long.parseLong(part, 16)`에서 부호 없는 양수만(`+` 허용, `-`는 UUID 성분으로는 거절한다).
fn parse_long_radix16(part: &str) -> Option<u64> {
    let digits = part.strip_prefix('+').unwrap_or(part);
    if digits.is_empty() {
        return None;
    }
    let mut value: u128 = 0;
    for c in digits.chars() {
        value = value * 16 + u128::from(c.to_digit(16)?);
        if value > i64::MAX as u128 {
            return None;
        }
    }
    u64::try_from(value).ok()
}

/// Java `String.length()`(UTF-16 코드 유닛 수).
pub fn java_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 영문 소문자 16진수.
pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// `HexFormat.of().parseHex`: 길이가 짝수이고 대소문자 16진수 숫자만 받는다.
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| {
            let hi = char::from(pair[0]).to_digit(16)?;
            let lo = char::from(pair[1]).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC은 어떤 길이의 키도 받는다");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// 운영체제 난수 `n`바이트(`SecureRandom`).
pub fn random_bytes<const N: usize>() -> [u8; N] {
    use aes_gcm::aead::OsRng;
    use aes_gcm::aead::rand_core::RngCore;
    let mut buf = [0u8; N];
    OsRng.fill_bytes(&mut buf);
    buf
}

const BASE64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64_encode(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = [
            alphabet[(n >> 18) as usize & 63],
            alphabet[(n >> 12) as usize & 63],
            alphabet[(n >> 6) as usize & 63],
            alphabet[n as usize & 63],
        ];
        let keep = chunk.len() + 1;
        for c in &chars[..keep] {
            out.push(char::from(*c));
        }
        if pad {
            for _ in keep..4 {
                out.push('=');
            }
        }
    }
    out
}

/// `Base64.getEncoder()`.
pub fn base64_std_encode(bytes: &[u8]) -> String {
    base64_encode(bytes, BASE64_STD, true)
}

/// `Base64.getUrlEncoder().withoutPadding()`.
pub fn base64_url_encode_no_pad(bytes: &[u8]) -> String {
    base64_encode(bytes, BASE64_URL, false)
}

/// `Base64.getDecoder()`: 표준 알파벳, 끝의 `=`는 있어도 없어도 된다. 알파벳 밖 문자와 흩어진 `=`는 거절한다.
pub fn base64_std_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let data_len = bytes.iter().rposition(|b| *b != b'=').map_or(0, |i| i + 1);
    let padding = bytes.len() - data_len;
    if padding > 2 || (padding > 0 && !bytes.len().is_multiple_of(4)) {
        return None;
    }
    let mut out = Vec::with_capacity(data_len * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in &bytes[..data_len] {
        let v = BASE64_STD.iter().position(|c| c == b)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    // 마지막 그룹이 한 글자(6비트)뿐이면 바이트를 만들 수 없다.
    if data_len % 4 == 1 {
        return None;
    }
    Some(out)
}

/// `CanonicalUuidParser.parse`: `UUID.fromString`이 받고 `toString()`(소문자 표준형)과 대소문자 무시로 같아야 한다.
pub fn parse_canonical_uuid(text: &str) -> Option<Uuid> {
    let uuid = java_uuid_from_string(text)?;
    uuid.hyphenated()
        .to_string()
        .eq_ignore_ascii_case(text)
        .then_some(uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // JwtAccessTokenProvider#validateSubject가 UUID.fromString으로 sub를 확인한다.
    #[test]
    fn 표준_형식은_그대로_읽는다() {
        let s = "01990000-0000-7000-8000-000000000001";
        assert_eq!(java_uuid_from_string(s), Some(Uuid::parse_str(s).unwrap()));
        let upper = s.to_uppercase();
        assert_eq!(
            java_uuid_from_string(&upper),
            Some(Uuid::parse_str(s).unwrap())
        );
    }

    #[test]
    fn 자바가_받는_느슨한_형식도_읽는다() {
        assert_eq!(
            java_uuid_from_string("1-2-3-4-5"),
            Some(Uuid::parse_str("00000001-0002-0003-0004-000000000005").unwrap())
        );
        assert_eq!(
            java_uuid_from_string("+1-2-3-4-5"),
            Some(Uuid::parse_str("00000001-0002-0003-0004-000000000005").unwrap())
        );
    }

    #[test]
    fn 형식이_틀리면_거절한다() {
        for s in [
            "",
            "abc",
            "1-2-3-4",
            "1-2-3-4-5-6",
            "g-2-3-4-5",
            "--3-4-5",
            "-1-2-3-4-5",
        ] {
            assert_eq!(java_uuid_from_string(s), None, "{s}");
        }
        // 36자를 넘으면 거절
        assert_eq!(
            java_uuid_from_string("0000000000000000000000000000001-2-3-4-5"),
            None
        );
    }

    #[test]
    fn 십육진수_변환() {
        assert_eq!(hex_encode(&[0, 0xab, 0xff]), "00abff");
        assert_eq!(hex_decode("00ABff"), Some(vec![0, 0xab, 0xff]));
        assert_eq!(hex_decode("0"), None);
        assert_eq!(hex_decode("zz"), None);
        assert_eq!(hex_decode(""), Some(vec![]));
    }

    #[test]
    fn base64_변환은_자바_기본_인코더와_같다() {
        assert_eq!(base64_std_encode(b""), "");
        assert_eq!(base64_std_encode(b"f"), "Zg==");
        assert_eq!(base64_std_encode(b"fo"), "Zm8=");
        assert_eq!(base64_std_encode(b"foo"), "Zm9v");
        assert_eq!(base64_url_encode_no_pad(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_std_decode("Zm9vYg=="), Some(b"foob".to_vec()));
        assert_eq!(base64_std_decode("Zm9vYg"), Some(b"foob".to_vec()));
        assert_eq!(base64_std_decode("Zm9v!"), None);
        assert_eq!(base64_std_decode("Z"), None);
        assert_eq!(base64_std_decode("Zg=a"), None);
    }

    // CanonicalUuidParser: 표준 형식만 받는다(Java 느슨한 형식은 거절).
    #[test]
    fn 표준_형식_uuid만_받는다() {
        let s = "0190f3a2-7b1c-7d2e-8f4a-123456789abc";
        assert_eq!(parse_canonical_uuid(s), Some(Uuid::parse_str(s).unwrap()));
        assert!(parse_canonical_uuid(&s.to_uppercase()).is_some());
        assert_eq!(parse_canonical_uuid("1-2-3-4-5"), None);
        assert_eq!(parse_canonical_uuid("abc"), None);
    }
}
