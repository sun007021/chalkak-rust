//! base64 / base64url / 16진수. 표준 라이브러리에 없어 직접 구현한다(허용 목록 밖의 `base64` crate를 쓰지 않는다).
//!
//! 디코더는 Java/Nimbus처럼 마지막 글자의 남는 비트를 무시하고, 끝의 `=` 패딩은 있어도 없어도 받는다.
//! 알파벳 밖의 글자와 길이가 4n+1인 입력은 거절한다.

#[derive(Clone, Copy)]
pub(crate) enum Alphabet {
    Url,
    #[allow(dead_code)] // ES256 PEM 해석 커밋에서 사용
    Standard,
}

const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(crate) fn encode_url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn value(c: u8, alphabet: Alphabet) -> Option<u32> {
    Some(u32::from(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' if matches!(alphabet, Alphabet::Url) => 62,
        b'_' if matches!(alphabet, Alphabet::Url) => 63,
        b'+' if matches!(alphabet, Alphabet::Standard) => 62,
        b'/' if matches!(alphabet, Alphabet::Standard) => 63,
        _ => return None,
    }))
}

pub(crate) fn decode(text: &str, alphabet: Alphabet) -> Result<Vec<u8>, ()> {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    let mut padding = 0;
    while end > 0 && bytes[end - 1] == b'=' && padding < 2 {
        end -= 1;
        padding += 1;
    }
    let data = &bytes[..end];
    if data.len() % 4 == 1 {
        return Err(());
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in data {
        acc = (acc << 6) | value(c, alphabet).ok_or(())?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

pub(crate) fn decode_url(text: &str) -> Result<Vec<u8>, ()> {
    decode(text, Alphabet::Url)
}

/// `java.util.HexFormat.of().parseHex`: 짝수 길이, 대소문자 구분 없는 16진수.
pub fn decode_hex(text: &str) -> Result<Vec<u8>, crate::JwtError> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(crate::JwtError::InvalidKey(
            "hex string must have an even length".into(),
        ));
    }
    let digit = |c: u8| -> Result<u8, crate::JwtError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(crate::JwtError::InvalidKey(
                "hex string contains a non-hex character".into(),
            )),
        }
    };
    bytes
        .chunks(2)
        .map(|pair| Ok((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_벡터() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_url(plain.as_bytes()), enc);
            assert_eq!(decode_url(enc).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn url_알파벳과_패딩() {
        assert_eq!(encode_url(&[0xfb, 0xff, 0xfe]), "-__-");
        assert_eq!(decode_url("-__-").unwrap(), [0xfb, 0xff, 0xfe]);
        assert_eq!(decode_url("Zg==").unwrap(), b"f");
        assert!(decode_url("+/+/").is_err());
        assert!(decode_url("Z").is_err());
        assert!(decode_url("Zm9v!").is_err());
    }

    #[test]
    fn 표준_알파벳() {
        assert_eq!(decode(" ", Alphabet::Standard), Err(()));
        assert_eq!(
            decode("+/+/", Alphabet::Standard).unwrap(),
            [0xfb, 0xff, 0xbf]
        );
        assert!(decode("-_-_", Alphabet::Standard).is_err());
    }
}
