//! 이미지 처리 콜백의 HMAC 인증. 원본 `ProcessingCallbackAuthenticator`.
//!
//! 서명 대상은 `timestamp \n POST \n path \n sha256hex(body) \n audience`다(`audience`는 이 서버의 이미지 환경).
//! 경로에 콜백 종류가 들어가므로 사인 콜백 서명을 다른 콜백에 재사용할 수 없고, 환경이 들어가므로 dev용 서명이
//! prod에서 통하지 않는다. 사인 콜백은 본문이 없어 빈 본문의 해시를 쓴다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};

use super::oidc::constant_time_eq;
use crate::common::{hex_decode, hex_encode, hmac_sha256, sha256_hex};
use crate::web::ApiError;

const SIGNATURE_PREFIX: &str = "v1=";
const MAX_AGE_SECONDS: i64 = 300;
/// 미래 타임스탬프는 시계 오차로만 설명되므로 과거만큼 넉넉히 받지 않는다.
const MAX_FUTURE_SKEW_SECONDS: i64 = 30;

#[derive(Clone)]
pub struct ProcessingCallbackAuthenticator {
    secret: Vec<u8>,
    audience: String,
    clock: Arc<dyn Clock>,
}

fn invalid_callback() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 이미지 처리 콜백입니다.",
    ))
}

/// `Long.parseLong`(10진수): 선택적 부호 뒤에 숫자가 하나 이상, 범위는 `i64`.
pub fn parse_java_long(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.strip_prefix('+').unwrap_or(text).parse().ok()
}

impl ProcessingCallbackAuthenticator {
    pub fn new(secret: &str, audience: &str, clock: Arc<dyn Clock>) -> Result<Self, String> {
        if secret.chars().count() < 32 {
            return Err("IMAGE_PROCESSOR_CALLBACK_SECRET은 32자 이상이어야 합니다.".to_owned());
        }
        if audience.trim().is_empty() {
            return Err("chalkak.image.environment는 비어 있을 수 없습니다.".to_owned());
        }
        Ok(ProcessingCallbackAuthenticator {
            secret: secret.as_bytes().to_vec(),
            audience: audience.to_owned(),
            clock,
        })
    }

    /// 서명 문자열(`v1=<hex>`)을 만든다. 시험과 하네스 문서용이다.
    pub fn sign(&self, timestamp: &str, path: &str, raw_body: Option<&[u8]>) -> String {
        let payload = format!(
            "{timestamp}\nPOST\n{path}\n{}\n{}",
            sha256_hex(raw_body.unwrap_or(&[])),
            self.audience
        );
        format!(
            "{SIGNATURE_PREFIX}{}",
            hex_encode(&hmac_sha256(&self.secret, payload.as_bytes()))
        )
    }

    /// 헤더가 없거나 형식이 틀리거나 시각이 어긋나거나 서명이 다르면 모두 같은 401이다.
    pub fn authenticate(
        &self,
        path: &str,
        raw_body: Option<&[u8]>,
        timestamp: Option<&str>,
        signature: Option<&str>,
    ) -> Result<(), ApiError> {
        let timestamp = timestamp.ok_or_else(invalid_callback)?;
        let requested_at = parse_java_long(timestamp).ok_or_else(invalid_callback)?;
        // Java long 연산처럼 넘치면 되감긴다.
        let age = self.clock.now().as_second().wrapping_sub(requested_at);
        if !(-MAX_FUTURE_SKEW_SECONDS..=MAX_AGE_SECONDS).contains(&age) {
            return Err(invalid_callback());
        }

        let expected = hmac_sha256(
            &self.secret,
            format!(
                "{timestamp}\nPOST\n{path}\n{}\n{}",
                sha256_hex(raw_body.unwrap_or(&[])),
                self.audience
            )
            .as_bytes(),
        );
        let provided = signature
            .and_then(|s| s.strip_prefix(SIGNATURE_PREFIX))
            .and_then(hex_decode)
            .ok_or_else(invalid_callback)?;
        if !constant_time_eq(&expected, &provided) {
            return Err(invalid_callback());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;

    const SECRET: &str = "test-callback-secret-with-enough-length";
    const PATH: &str =
        "/internal/v1/signature-processing/0190f3a2-7b1c-7d2e-8f4a-123456789abc/complete";

    fn auth() -> (ProcessingCallbackAuthenticator, Arc<FixedClock>) {
        let clock = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        (
            ProcessingCallbackAuthenticator::new(SECRET, "local", clock.clone()).unwrap(),
            clock,
        )
    }

    fn now_secs() -> i64 {
        "2026-10-12T00:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second()
    }

    fn assert_rejected(result: Result<(), ApiError>) {
        let err = result.unwrap_err();
        assert_eq!(err.status(), 401);
        assert_eq!(err.message(), "유효하지 않은 이미지 처리 콜백입니다.");
    }

    // ProcessingCallbackAuthenticatorTest: 정상 서명
    #[test]
    fn 올바른_서명은_통과한다() {
        let (a, _) = auth();
        let ts = now_secs().to_string();
        let sig = a.sign(&ts, PATH, None);
        assert!(a.authenticate(PATH, None, Some(&ts), Some(&sig)).is_ok());
        let body = b"{}";
        let sig = a.sign(&ts, PATH, Some(body));
        assert!(
            a.authenticate(PATH, Some(body), Some(&ts), Some(&sig))
                .is_ok()
        );
    }

    #[test]
    fn 서명_형식이_틀리면_거절한다() {
        let (a, _) = auth();
        let ts = now_secs().to_string();
        let good = a.sign(&ts, PATH, None);
        let hex = good.strip_prefix("v1=").unwrap().to_owned();
        for sig in [
            None,
            Some(String::new()),
            Some(hex.clone()),
            Some(format!("v2={hex}")),
            Some("v1=".to_owned()),
            Some("v1=zz".to_owned()),
            Some(format!("v1={}", &hex[..hex.len() - 2])),
            Some(format!("v1={hex}00")),
            Some(good.to_uppercase()),
        ] {
            assert_rejected(a.authenticate(PATH, None, Some(&ts), sig.as_deref()));
        }
        // 16진수는 대소문자를 가리지 않는다.
        let upper = format!("v1={}", hex.to_uppercase());
        assert!(a.authenticate(PATH, None, Some(&ts), Some(&upper)).is_ok());
    }

    #[test]
    fn 경로_본문_환경이_다르면_거절한다() {
        let (a, clock) = auth();
        let ts = now_secs().to_string();
        let sig = a.sign(&ts, PATH, None);
        assert_rejected(a.authenticate(
            &PATH.replace("complete", "failed"),
            None,
            Some(&ts),
            Some(&sig),
        ));
        assert_rejected(a.authenticate(PATH, Some(b"x"), Some(&ts), Some(&sig)));
        let prod = ProcessingCallbackAuthenticator::new(SECRET, "prod", clock).unwrap();
        assert_rejected(prod.authenticate(PATH, None, Some(&ts), Some(&sig)));
    }

    // ProcessingCallbackAuthenticatorTest: 시각 창은 과거 300초, 미래 30초
    #[test]
    fn 시각_창_밖이면_거절한다() {
        let (a, _) = auth();
        let check = |offset: i64| {
            let ts = (now_secs() + offset).to_string();
            let sig = a.sign(&ts, PATH, None);
            a.authenticate(PATH, None, Some(&ts), Some(&sig))
        };
        assert!(check(-300).is_ok());
        assert_rejected(check(-301));
        assert!(check(30).is_ok());
        assert_rejected(check(31));
    }

    #[test]
    fn 타임스탬프는_java_long_형식이어야_한다() {
        let (a, _) = auth();
        for ts in [
            None,
            Some(""),
            Some("abc"),
            Some("1.5"),
            Some(" 1"),
            Some("0x10"),
            Some("99999999999999999999"),
        ] {
            assert_rejected(a.authenticate(PATH, None, ts, Some("v1=00")));
        }
        // "+<초>"도 Long.parseLong이 받으므로 서명 대상에는 원문 그대로 들어간다.
        let ts = format!("+{}", now_secs());
        let sig = a.sign(&ts, PATH, None);
        assert!(a.authenticate(PATH, None, Some(&ts), Some(&sig)).is_ok());
    }

    #[test]
    fn long_파서() {
        assert_eq!(parse_java_long("0"), Some(0));
        assert_eq!(parse_java_long("-5"), Some(-5));
        assert_eq!(parse_java_long("+5"), Some(5));
        assert_eq!(parse_java_long("9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_java_long("9223372036854775808"), None);
        assert_eq!(parse_java_long("-"), None);
        assert_eq!(parse_java_long("+"), None);
        assert_eq!(parse_java_long("--1"), None);
        assert_eq!(parse_java_long("1_0"), None);
    }

    #[test]
    fn 비밀키와_환경_검증() {
        let clock = Arc::new(FixedClock::new("2026-10-12T00:00:00Z".parse().unwrap()));
        assert!(ProcessingCallbackAuthenticator::new("short", "local", clock.clone()).is_err());
        assert!(ProcessingCallbackAuthenticator::new(SECRET, " ", clock).is_err());
    }
}
