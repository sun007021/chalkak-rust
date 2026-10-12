//! 여러 도메인이 함께 쓰는 작은 도구.

use uuid::Uuid;

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
}
