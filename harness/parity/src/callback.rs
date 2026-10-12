//! 이미지 처리 콜백(`/internal/v1/**`)의 HMAC 서명. 원본 `ProcessingCallbackAuthenticator`와 같은 규칙이다.
//!
//! 서명 대상은 `timestamp \n POST \n 경로 \n sha256hex(본문) \n audience`이고(audience는 앱의 이미지 환경, 하네스에서는
//! `local`), 헤더 값은 `v1=<hmac-sha256 hex>`다. 사인 콜백은 본문을 읽지 않으므로 빈 본문의 해시를 쓴다.

use crate::scenario::CallbackSign;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const TIMESTAMP_HEADER: &str = "X-Chalkak-Callback-Timestamp";
pub const SIGNATURE_HEADER: &str = "X-Chalkak-Callback-Signature";

/// `harness/env/oracle.env`의 `IMAGE_PROCESSOR_CALLBACK_SECRET`.
const DEFAULT_SECRET: &str = "test-callback-secret-with-enough-length";
const DEFAULT_AUDIENCE: &str = "local";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `v1=<hex>` 서명.
pub fn signature(secret: &str, audience: &str, timestamp: &str, path: &str, body: &[u8]) -> String {
    let payload = format!(
        "{timestamp}\nPOST\n{path}\n{}\n{audience}",
        hex(&Sha256::digest(body))
    );
    // HMAC은 키 길이를 가리지 않는다.
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC 키");
    mac.update(payload.as_bytes());
    format!("v1={}", hex(&mac.finalize().into_bytes()))
}

/// 단계에 붙일 콜백 헤더. `now`는 이 단계의 앱 시각(epoch 초), `path`는 쿼리를 뗀 요청 경로다.
pub fn headers(spec: &CallbackSign, now: i64, path: &str) -> Vec<(String, String)> {
    let timestamp = (now + spec.timestamp_offset.unwrap_or(0)).to_string();
    let signed_path = spec.sign_path.as_deref().unwrap_or(path);
    let value = spec.signature.clone().unwrap_or_else(|| {
        signature(
            spec.secret.as_deref().unwrap_or(DEFAULT_SECRET),
            spec.audience.as_deref().unwrap_or(DEFAULT_AUDIENCE),
            &timestamp,
            signed_path,
            &[],
        )
    });
    let mut out = Vec::new();
    if !spec.omit_timestamp {
        out.push((TIMESTAMP_HEADER.to_owned(), timestamp));
    }
    if !spec.omit_signature {
        out.push((SIGNATURE_HEADER.to_owned(), value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 원본 ProcessingCallbackAuthenticatorTest와 같은 서명 대상 문자열
    #[test]
    fn 서명은_타임스탬프_경로_본문해시_audience를_묶는다() {
        let sig = signature("k", "local", "1", "/p", b"");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"k").unwrap();
        mac.update(
            b"1\nPOST\n/p\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\nlocal",
        );
        assert_eq!(sig, format!("v1={}", hex(&mac.finalize().into_bytes())));
    }

    #[test]
    fn 기본값은_하네스_비밀키와_local이다() {
        let spec = CallbackSign::default();
        let h = headers(&spec, 100, "/internal/v1/x/complete");
        assert_eq!(h[0], (TIMESTAMP_HEADER.to_owned(), "100".to_owned()));
        assert_eq!(
            h[1].1,
            signature(
                DEFAULT_SECRET,
                DEFAULT_AUDIENCE,
                "100",
                "/internal/v1/x/complete",
                b""
            )
        );
    }

    #[test]
    fn 오프셋과_누락_덮어쓰기를_지원한다() {
        let spec = CallbackSign {
            timestamp_offset: Some(-301),
            omit_signature: true,
            ..CallbackSign::default()
        };
        let h = headers(&spec, 1000, "/p");
        assert_eq!(h, vec![(TIMESTAMP_HEADER.to_owned(), "699".to_owned())]);
        let spec = CallbackSign {
            signature: Some("v1=00".into()),
            omit_timestamp: true,
            ..CallbackSign::default()
        };
        assert_eq!(
            headers(&spec, 1000, "/p"),
            vec![(SIGNATURE_HEADER.to_owned(), "v1=00".to_owned())]
        );
    }
}
