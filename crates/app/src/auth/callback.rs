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
    use uuid::Uuid;

    // ProcessingCallbackAuthenticatorTest의 상수와 같다.
    const AUDIENCE: &str = "dev";
    const SECRET: &str = "test-callback-secret-with-enough-length";
    const NOW: &str = "2026-10-12T00:00:00Z";

    fn now_secs() -> i64 {
        NOW.parse::<jiff::Timestamp>().unwrap().as_second()
    }

    fn authenticator(audience: &str) -> ProcessingCallbackAuthenticator {
        let clock = Arc::new(FixedClock::new(NOW.parse().unwrap()));
        ProcessingCallbackAuthenticator::new(SECRET, audience, clock).unwrap()
    }

    fn signature_path(upload_id: Uuid, result: &str) -> String {
        format!("/internal/v1/signature-processing/{upload_id}/{result}")
    }

    /// 서명 헬퍼는 구현의 `sign`을 쓰지 않고 원본 테스트처럼 HMAC을 직접 계산한다.
    fn sign_bytes(path: &str, raw_body: Option<&[u8]>, timestamp: &str) -> String {
        let body_hash = sha256_hex(raw_body.unwrap_or(&[]));
        let payload = format!("{timestamp}\nPOST\n{path}\n{body_hash}\n{AUDIENCE}");
        format!(
            "v1={}",
            hex_encode(&hmac_sha256(SECRET.as_bytes(), payload.as_bytes()))
        )
    }

    fn assert_unauthorized(result: Result<(), ApiError>) {
        let err = result.unwrap_err();
        assert_eq!(err.status(), 401);
        assert_eq!(err.message(), "유효하지 않은 이미지 처리 콜백입니다.");
    }

    // "현재 시각의 올바른 HMAC 서명은 허용한다"
    #[test]
    fn authenticate_valid_signature_succeeds() {
        let upload_id = Uuid::new_v4();
        let timestamp = now_secs().to_string();
        let path = signature_path(upload_id, "complete");
        let signature = sign_bytes(&path, None, &timestamp);

        assert!(
            authenticator(AUDIENCE)
                .authenticate(&path, None, Some(&timestamp), Some(&signature))
                .is_ok()
        );
    }

    // "잘못된 HMAC 서명은 거부한다"
    #[test]
    fn authenticate_invalid_signature_throws_unauthorized_exception() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = now_secs().to_string();

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &path,
            None,
            Some(&timestamp),
            Some("invalid"),
        ));
    }

    // "5분이 지난 콜백 서명은 재사용 공격으로 보고 거부한다"
    #[test]
    fn authenticate_expired_timestamp_throws_unauthorized_exception() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = (now_secs() - 301).to_string();
        let signature = sign_bytes(&path, None, &timestamp);

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &path,
            None,
            Some(&timestamp),
            Some(&signature),
        ));
    }

    // "timestamp가 없으면 인증 실패로 거부한다"
    #[test]
    fn authenticate_null_timestamp_throws_unauthorized_exception() {
        let path = signature_path(Uuid::new_v4(), "complete");

        assert_unauthorized(authenticator(AUDIENCE).authenticate(&path, None, None, Some("v1=00")));
    }

    // "서명이 없으면 인증 실패로 거부한다"
    #[test]
    fn authenticate_null_signature_throws_unauthorized_exception() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = now_secs().to_string();

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &path,
            None,
            Some(&timestamp),
            None,
        ));
    }

    // "허용 시간 차 직전의 콜백 서명은 통과한다"
    #[test]
    fn authenticate_just_inside_clock_skew_succeeds() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = (now_secs() - 299).to_string();
        let signature = sign_bytes(&path, None, &timestamp);

        assert!(
            authenticator(AUDIENCE)
                .authenticate(&path, None, Some(&timestamp), Some(&signature))
                .is_ok()
        );
    }

    // "본문이 있는 콜백은 수신 원문의 해시로 검증한다"
    #[test]
    fn authenticate_json_body_succeeds() {
        let path = format!(
            "/internal/v1/post-image-processing/{}/complete",
            Uuid::new_v4()
        );
        let raw_body = br#"{"width":4032,"height":3024}"#;
        let timestamp = now_secs().to_string();
        let signature = sign_bytes(&path, Some(raw_body), &timestamp);

        assert!(
            authenticator(AUDIENCE)
                .authenticate(&path, Some(raw_body), Some(&timestamp), Some(&signature))
                .is_ok()
        );
    }

    // "같은 값이어도 직렬화가 다른 본문의 서명은 거부한다"
    #[test]
    fn authenticate_reserialized_body_throws_unauthorized_exception() {
        let path = format!(
            "/internal/v1/post-image-processing/{}/complete",
            Uuid::new_v4()
        );
        let sent_body = br#"{"width":4032,"height":3024}"#;
        let reserialized_body = br#"{"width": 4032, "height": 3024}"#;
        let timestamp = now_secs().to_string();
        let signature = sign_bytes(&path, Some(sent_body), &timestamp);

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &path,
            Some(reserialized_body),
            Some(&timestamp),
            Some(&signature),
        ));
    }

    // "종류가 다른 경로로 만든 서명은 거부한다"
    #[test]
    fn authenticate_other_kind_path_throws_unauthorized_exception() {
        let upload_id = Uuid::new_v4();
        let timestamp = now_secs().to_string();
        let signature = sign_bytes(&signature_path(upload_id, "complete"), None, &timestamp);
        let post_path = format!("/internal/v1/post-image-processing/{upload_id}/complete");

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &post_path,
            None,
            Some(&timestamp),
            Some(&signature),
        ));
    }

    // "유효한 UTF-8이 아닌 본문도 수신 바이트 그대로 검증한다"
    #[test]
    fn authenticate_invalid_utf8_body_succeeds() {
        let path = format!(
            "/internal/v1/post-image-processing/{}/complete",
            Uuid::new_v4()
        );
        let raw_body = [0xC3u8, 0x28];
        let timestamp = now_secs().to_string();
        let signature = sign_bytes(&path, Some(&raw_body), &timestamp);

        assert!(
            authenticator(AUDIENCE)
                .authenticate(&path, Some(&raw_body), Some(&timestamp), Some(&signature))
                .is_ok()
        );
    }

    // "깨진 바이트가 서로 다르면 서명도 달라진다"
    #[test]
    fn authenticate_different_invalid_utf8_body_throws_unauthorized_exception() {
        let path = format!(
            "/internal/v1/post-image-processing/{}/complete",
            Uuid::new_v4()
        );
        let sent_body = [0xC3u8, 0x28];
        let tampered_body = [0xE0u8, 0x28];
        let timestamp = now_secs().to_string();
        let signature = sign_bytes(&path, Some(&sent_body), &timestamp);

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &path,
            Some(&tampered_body),
            Some(&timestamp),
            Some(&signature),
        ));
    }

    // "허용 폭을 넘어 미래로 앞선 타임스탬프는 거부한다"
    #[test]
    fn authenticate_far_future_timestamp_throws_unauthorized_exception() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = (now_secs() + 60).to_string();
        let signature = sign_bytes(&path, None, &timestamp);

        assert_unauthorized(authenticator(AUDIENCE).authenticate(
            &path,
            None,
            Some(&timestamp),
            Some(&signature),
        ));
    }

    // "작은 시계 오차만큼 앞선 타임스탬프는 허용한다"
    #[test]
    fn authenticate_slightly_future_timestamp_succeeds() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = (now_secs() + 10).to_string();
        let signature = sign_bytes(&path, None, &timestamp);

        assert!(
            authenticator(AUDIENCE)
                .authenticate(&path, None, Some(&timestamp), Some(&signature))
                .is_ok()
        );
    }

    // "다른 환경을 대상으로 만든 서명은 거부한다"
    #[test]
    fn authenticate_signature_for_another_environment_throws_unauthorized_exception() {
        let path = signature_path(Uuid::new_v4(), "complete");
        let timestamp = now_secs().to_string();
        let dev_signature = sign_bytes(&path, None, &timestamp);

        assert_unauthorized(authenticator("prod").authenticate(
            &path,
            None,
            Some(&timestamp),
            Some(&dev_signature),
        ));
    }

    // 경계와 형식(원본 구현의 분기). 위의 14개와 별개로 구현의 세부 규칙을 고정한다.
    #[test]
    fn 시각_창은_과거_300초_미래_30초이고_경계를_포함한다() {
        let a = authenticator(AUDIENCE);
        let path = signature_path(Uuid::new_v4(), "complete");
        let check = |offset: i64| {
            let ts = (now_secs() + offset).to_string();
            a.authenticate(&path, None, Some(&ts), Some(&sign_bytes(&path, None, &ts)))
        };
        assert!(check(-300).is_ok());
        assert_unauthorized(check(-301));
        assert!(check(30).is_ok());
        assert_unauthorized(check(31));
    }

    #[test]
    fn 서명_형식이_틀리면_거절한다() {
        let a = authenticator(AUDIENCE);
        let path = signature_path(Uuid::new_v4(), "complete");
        let ts = now_secs().to_string();
        let good = sign_bytes(&path, None, &ts);
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
            assert_unauthorized(a.authenticate(&path, None, Some(&ts), sig.as_deref()));
        }
        // 16진수는 대소문자를 가리지 않는다.
        let upper = format!("v1={}", hex.to_uppercase());
        assert!(a.authenticate(&path, None, Some(&ts), Some(&upper)).is_ok());
    }

    #[test]
    fn 타임스탬프는_java_long_형식이어야_한다() {
        let a = authenticator(AUDIENCE);
        let path = signature_path(Uuid::new_v4(), "complete");
        for ts in [
            None,
            Some(""),
            Some("abc"),
            Some("1.5"),
            Some(" 1"),
            Some("0x10"),
            Some("99999999999999999999"),
        ] {
            assert_unauthorized(a.authenticate(&path, None, ts, Some("v1=00")));
        }
        // "+<초>"도 Long.parseLong이 받으므로 서명 대상에는 원문 그대로 들어간다.
        let ts = format!("+{}", now_secs());
        let sig = sign_bytes(&path, None, &ts);
        assert!(a.authenticate(&path, None, Some(&ts), Some(&sig)).is_ok());
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
        let clock = Arc::new(FixedClock::new(NOW.parse().unwrap()));
        assert!(ProcessingCallbackAuthenticator::new("short", "local", clock.clone()).is_err());
        assert!(ProcessingCallbackAuthenticator::new(SECRET, " ", clock.clone()).is_err());
        // 구현의 sign()도 같은 서명을 만든다.
        let a = ProcessingCallbackAuthenticator::new(SECRET, AUDIENCE, clock).unwrap();
        assert_eq!(
            a.sign("1", "/p", Some(b"x")),
            sign_bytes("/p", Some(b"x"), "1")
        );
    }
}
