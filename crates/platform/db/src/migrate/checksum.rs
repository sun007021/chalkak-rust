//! Flyway SQL 마이그레이션 체크섬.
//!
//! Flyway `AbstractSqlMigrationResolver`/`ChecksumCalculator`와 같은 방식이다.
//! 스크립트를 UTF-8 텍스트로 읽어 `BufferedReader.readLine()`처럼 줄 단위로 나누고
//! (`\n`, `\r`, `\r\n` 모두 줄 끝, 줄 끝 문자는 제외), 첫 줄의 BOM(U+FEFF)을 지운 뒤,
//! 각 줄의 UTF-8 바이트를 CRC32에 이어서 넣는다. 결과 `long`을 `int`로 줄여 저장한다.

/// CRC-32(IEEE 802.3, 반사형, `java.util.zip.CRC32`와 같다) 누적기.
#[derive(Clone, Copy, Debug)]
pub struct Crc32 {
    state: u32,
}

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

impl Crc32 {
    pub fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let idx = ((self.state ^ u32::from(b)) & 0xFF) as usize;
            self.state = (self.state >> 8) ^ TABLE[idx];
        }
    }

    pub fn value(&self) -> u32 {
        !self.state
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Flyway가 `flyway_schema_history.checksum`(INT)에 저장하는 값을 계산한다.
///
/// UTF-8이 아닌 바이트는 Java 디코더처럼 U+FFFD로 바뀐 채 계산된다.
pub fn flyway_checksum(content: &[u8]) -> i32 {
    let text = String::from_utf8_lossy(content);
    let mut crc = Crc32::new();
    let mut rest: &str = &text;
    let mut first = true;
    while !rest.is_empty() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let mut line = &rest[..end];
        if first {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            first = false;
        }
        crc.update(line.as_bytes());
        let after = &rest[end..];
        rest = match after.as_bytes() {
            [b'\r', b'\n', ..] => &after[2..],
            [_, ..] => &after[1..],
            [] => after,
        };
    }
    crc.value() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 근거: `java.util.zip.CRC32`의 표준 검사값. "123456789" -> 0xCBF43926.
    #[test]
    fn crc32_standard_check_value() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.value(), 0xCBF4_3926);
    }

    #[test]
    fn crc32_update_is_incremental() {
        let mut a = Crc32::new();
        a.update(b"1234");
        a.update(b"56789");
        assert_eq!(a.value(), 0xCBF4_3926);
    }

    /// 근거: Flyway는 줄 끝 문자를 CRC에 넣지 않는다. 줄바꿈 종류와 마지막 줄바꿈 유무가
    /// 체크섬에 영향을 주지 않아야 Windows/Unix 체크아웃이 같은 값을 낸다.
    #[test]
    fn line_terminators_are_excluded() {
        let unix = flyway_checksum(b"select 1;\nselect 2;\n");
        assert_eq!(unix, flyway_checksum(b"select 1;\r\nselect 2;\r\n"));
        assert_eq!(unix, flyway_checksum(b"select 1;\rselect 2;\r"));
        assert_eq!(unix, flyway_checksum(b"select 1;\nselect 2;"));
        assert_eq!(unix, flyway_checksum(b"select 1;select 2;"));
    }

    /// 근거: `BufferedReader.readLine()`은 끝의 빈 줄을 줄로 세지만 빈 줄은 0바이트라
    /// 체크섬이 같다. 빈 입력은 CRC32 초기값이므로 0이다.
    #[test]
    fn blank_lines_do_not_change_checksum_and_empty_is_zero() {
        assert_eq!(flyway_checksum(b""), 0);
        assert_eq!(flyway_checksum(b"\n\n"), 0);
        assert_eq!(flyway_checksum(b"a\n\n\nb\n"), flyway_checksum(b"ab"));
    }

    /// 근거: Flyway `filterBomFromFirstLine` - 첫 줄의 BOM만 지운다.
    #[test]
    fn bom_is_stripped_only_from_first_line() {
        let plain = flyway_checksum(b"select 1;\n");
        assert_eq!(plain, flyway_checksum("\u{feff}select 1;\n".as_bytes()));
        assert_ne!(
            flyway_checksum(b"a\n"),
            flyway_checksum("a\n\u{feff}".as_bytes())
        );
    }

    #[test]
    fn multibyte_text_uses_utf8_bytes() {
        let mut crc = Crc32::new();
        crc.update("-- 한글 주석".as_bytes());
        assert_eq!(
            flyway_checksum("-- 한글 주석\n".as_bytes()),
            crc.value() as i32
        );
    }

    /// 근거: Java(Flyway)가 pg-java-a에 적용한 `flyway_schema_history.checksum` 28건.
    /// `select script, checksum from flyway_schema_history order by installed_rank`의 결과를
    /// 그대로 고정했다. `migrations/` 파일은 원본과 바이트 동일해야 하므로 값이 바뀌면 안 된다.
    const JAVA_FLYWAY_CHECKSUMS: [(&str, i32); 28] = [
        ("V202608191752__create_initial_schema.sql", -62292960),
        ("V202608202034__change_id_default_to_uuidv7.sql", 176110968),
        ("V202608211402__apply_soft_delete.sql", 1877203511),
        (
            "V202608241030__rename_topic_participation_columns.sql",
            -1304069922,
        ),
        (
            "V202608241930__add_signature_processing_state.sql",
            545156218,
        ),
        ("V202608252038__create_post_likes.sql", -68785813),
        ("V202608260030__create_post_image_uploads.sql", 477474006),
        (
            "V202608261500__add_post_image_uploads_state_constraint.sql",
            -2003257050,
        ),
        (
            "V202608261530__allow_repost_after_rejection.sql",
            -1346989453,
        ),
        (
            "V202608261600__link_posts_to_post_image_uploads.sql",
            -1805332059,
        ),
        ("V202608261823__add_social_accounts.sql", 1349244712),
        ("V202608272030__create_admin_audit_logs.sql", 1698084528),
        ("V202608280905__add_admin_post_query_index.sql", 860334329),
        (
            "V202608281716__add_admin_user_post_count_index.sql",
            2066078107,
        ),
        (
            "V202608301055__create_banned_social_identities.sql",
            -1550281854,
        ),
        ("V202608311000__store_social_identity_hmac.sql", 49120092),
        (
            "V202608311807__finalize_social_identity_hmac.sql",
            1742411748,
        ),
        ("V202609021130__create_user_refresh_tokens.sql", -1410868445),
        (
            "V202609021131__create_admin_refresh_tokens.sql",
            -1854852571,
        ),
        (
            "V202609041031__add_refresh_token_revoked_at_index.sql",
            1754568517,
        ),
        ("V202609041146__add_apple_authorizations.sql", -740429763),
        (
            "V202609041147__create_consumed_signup_tokens.sql",
            -1364363252,
        ),
        (
            "V202609041330__remove_apple_authorization_client_id.sql",
            329654810,
        ),
        (
            "V202609041400__create_pending_apple_authorizations.sql",
            -1343031133,
        ),
        (
            "V202609161100__recreate_pending_apple_authorizations_by_identity.sql",
            1364276501,
        ),
        ("V202609161640__create_feedbacks.sql", -1697810608),
        (
            "V202609291035__prevent_overlapping_topic_participation_periods.sql",
            748777606,
        ),
        ("V202609291510__create_notifications.sql", -1538072671),
    ];

    #[test]
    fn checksums_match_java_flyway_history_for_all_migrations() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        for (script, expected) in JAVA_FLYWAY_CHECKSUMS {
            let content = std::fs::read(dir.join(script)).expect(script);
            assert_eq!(flyway_checksum(&content), expected, "{script}");
        }
    }
}
