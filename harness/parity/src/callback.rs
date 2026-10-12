//! 이미지 처리 콜백 서명. 원본 `ProcessingCallbackAuthenticator`의 검증 규칙을 거꾸로 적용해 헤더를 만든다.
//!
//! 서명 대상: `timestamp \n POST \n 경로 \n sha256hex(본문) \n audience`, 서명은 `v1=` + HMAC-SHA256 hex.
//! 헤더: `X-Chalkak-Callback-Timestamp`, `X-Chalkak-Callback-Signature`.

use crate::scenario::CallbackSpec;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// `harness/env/oracle.env`의 `IMAGE_PROCESSOR_CALLBACK_SECRET`.
pub const DEFAULT_SECRET: &str = "test-callback-secret-with-enough-length";
/// 프로필 이름(환경 이름). 하네스는 local 프로필이다.
pub const DEFAULT_AUDIENCE: &str = "local";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 서명 헤더 두 개를 만든다. `path`는 쿼리를 뺀 요청 경로, `now`는 앱 시각(epoch 초).
pub fn headers(spec: &CallbackSpec, path: &str, body: &[u8], now: i64) -> Vec<(String, String)> {
    let timestamp = (now + spec.timestamp_offset.unwrap_or(0)).to_string();
    let signed_body = spec.sign_body.as_deref().map_or(body, str::as_bytes);
    let payload = format!(
        "{timestamp}\nPOST\n{path}\n{}\n{}",
        hex(&Sha256::digest(signed_body)),
        spec.audience.as_deref().unwrap_or(DEFAULT_AUDIENCE)
    );
    let secret = spec.secret.as_deref().unwrap_or(DEFAULT_SECRET);
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC은 모든 키 길이를 받는다");
    mac.update(payload.as_bytes());
    let signature = format!("v1={}", hex(&mac.finalize().into_bytes()));

    let mut out = Vec::new();
    if !spec.omit.iter().any(|o| o == "timestamp") {
        out.push(("X-Chalkak-Callback-Timestamp".to_owned(), timestamp));
    }
    if !spec.omit.iter().any(|o| o == "signature") {
        out.push((
            "X-Chalkak-Callback-Signature".to_owned(),
            spec.signature.clone().unwrap_or(signature),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CallbackSpec {
        CallbackSpec::default()
    }

    #[test]
    fn 서명은_v1_접두사와_64자리_hex다() {
        let h = headers(&spec(), "/internal/v1/x", b"{}", 1_000);
        assert_eq!(h[0], ("X-Chalkak-Callback-Timestamp".into(), "1000".into()));
        let sig = &h[1].1;
        assert!(sig.starts_with("v1="));
        assert_eq!(sig.len(), 3 + 64);
    }

    #[test]
    fn 본문_경로_시각이_바뀌면_서명도_바뀐다() {
        let base = headers(&spec(), "/p", b"a", 1)[1].1.clone();
        assert_ne!(base, headers(&spec(), "/p", b"b", 1)[1].1);
        assert_ne!(base, headers(&spec(), "/q", b"a", 1)[1].1);
        assert_ne!(base, headers(&spec(), "/p", b"a", 2)[1].1);
    }

    #[test]
    fn 시각_이동과_헤더_생략을_지정할_수_있다() {
        let s = CallbackSpec {
            timestamp_offset: Some(-400),
            omit: vec!["signature".into()],
            ..CallbackSpec::default()
        };
        let h = headers(&s, "/p", b"", 1_000);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, "600");
    }
}
