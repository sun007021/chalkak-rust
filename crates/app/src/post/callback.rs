//! 원본 `PostImageProcessingCallbackController`(`/internal/v1/post-image-processing`)와 `ProcessingCallbackAuthenticator`.
//!
//! 이미지 처리 람다가 부르는 내부 API다. 본문은 서명 검증 때문에 원문 바이트로 받고, 인증을 통과한 뒤에만 읽는다.
//! 본문 크기 제한은 서버 조립의 필터가 맡는다(`CallbackBodySizeLimitFilter`).

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_http::{Method, Request, Response};
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::command::PostCommandService;
use super::request::parse_canonical_uuid;
use crate::AppContext;
use crate::web::binding::is_java_whitespace;
use crate::web::{ApiError, ApiResult, Routes, ok_json};

pub const CALLBACK_PATH: &str = "/internal/v1/post-image-processing";
pub const TIMESTAMP_HEADER: &str = "X-Chalkak-Callback-Timestamp";
pub const SIGNATURE_HEADER: &str = "X-Chalkak-Callback-Signature";

const SIGNATURE_PREFIX: &str = "v1=";
const MAX_AGE_SECONDS: i64 = 300;
/// 미래 타임스탬프는 시계 오차로만 설명되므로 과거만큼 넉넉히 받을 이유가 없다.
const MAX_FUTURE_SKEW_SECONDS: i64 = 30;

const MAX_CAPTURED_AT_LENGTH: usize = 64;
const MAX_META_ATTRIBUTES: usize = 200;
const MAX_REASON_LENGTH: usize = 50;

fn unauthorized() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 이미지 처리 콜백입니다.",
    ))
}

// ---------------------------------------------------------------------------------------------
// 인증
// ---------------------------------------------------------------------------------------------

/// 이미지 처리 람다 콜백의 HMAC 인증기. 서명 대상 경로에 콜백 종류가 들어가므로 사인 콜백 서명을 게시물 콜백에
/// 재사용할 수 없고, 환경(`audience`)도 넣어 dev용 서명이 prod에서 통하지 않게 한다.
#[derive(Clone)]
pub struct ProcessingCallbackAuthenticator {
    secret: Vec<u8>,
    audience: String,
    clock: Arc<dyn Clock>,
}

impl ProcessingCallbackAuthenticator {
    /// 비밀키는 32자 이상, 대상은 비어 있지 않아야 한다(`IllegalArgumentException`).
    pub fn new(secret: &str, audience: &str, clock: Arc<dyn Clock>) -> Result<Self, String> {
        if secret.chars().count() < 32 {
            return Err("IMAGE_PROCESSOR_CALLBACK_SECRET은 32자 이상이어야 합니다.".to_owned());
        }
        if audience.chars().all(is_java_whitespace) {
            return Err("chalkak.image.environment는 비어 있을 수 없습니다.".to_owned());
        }
        Ok(ProcessingCallbackAuthenticator {
            secret: secret.as_bytes().to_vec(),
            audience: audience.to_owned(),
            clock,
        })
    }

    /// 실패하면 모두 같은 401이다(어느 단계에서 막혔는지 알려 주지 않는다).
    pub fn authenticate(
        &self,
        path: &str,
        raw_body: Option<&[u8]>,
        timestamp: Option<&str>,
        signature: Option<&str>,
    ) -> Result<(), ApiError> {
        let timestamp = timestamp.ok_or_else(unauthorized)?;
        let requested_at: i64 = timestamp.parse().map_err(|_| unauthorized())?;
        let age = self.clock.now().as_second().saturating_sub(requested_at);
        if !(-MAX_FUTURE_SKEW_SECONDS..=MAX_AGE_SECONDS).contains(&age) {
            return Err(unauthorized());
        }
        let provided = decode_signature(signature).ok_or_else(unauthorized)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).map_err(|_| unauthorized())?;
        mac.update(self.payload(timestamp, path, raw_body).as_bytes());
        // 길이가 다르거나 값이 다르면 오류. 상수 시간 비교다.
        mac.verify_slice(&provided).map_err(|_| unauthorized())
    }

    /// 서명 대상 문자열. 본문 해시는 **수신 바이트**로 계산한다.
    pub fn payload(&self, timestamp: &str, path: &str, raw_body: Option<&[u8]>) -> String {
        let hash = hex(&Sha256::digest(raw_body.unwrap_or_default()));
        format!("{timestamp}\nPOST\n{path}\n{hash}\n{}", self.audience)
    }

    /// 테스트·하네스용: 같은 규칙으로 서명을 만든다.
    pub fn sign(&self, timestamp: &str, path: &str, raw_body: Option<&[u8]>) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC은 모든 길이의 키를 받는다");
        mac.update(self.payload(timestamp, path, raw_body).as_bytes());
        format!("{SIGNATURE_PREFIX}{}", hex(&mac.finalize().into_bytes()))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `v1=` + 16진수(`HexFormat.parseHex`: 짝수 길이, 대소문자 모두).
fn decode_signature(signature: Option<&str>) -> Option<Vec<u8>> {
    let digits = signature?.strip_prefix(SIGNATURE_PREFIX)?;
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = char::from(pair[0]).to_digit(16)?;
            let lo = char::from(pair[1]).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// 요청 본문
// ---------------------------------------------------------------------------------------------

fn unreadable_body() -> ApiError {
    ApiError::App(AppError::business(
        ErrorCode::BUSINESS_ERROR,
        "이미지 처리 콜백 본문을 읽을 수 없습니다.",
    ))
}

fn invalid_body() -> ApiError {
    ApiError::App(AppError::business(
        ErrorCode::BUSINESS_ERROR,
        "이미지 처리 콜백 본문이 올바르지 않습니다.",
    ))
}

/// 본문 속 JSON 값 하나. `ObjectMapper.readValue(byte[])`는 첫 값만 읽고 뒤에 남은 것은 보지 않는다.
/// 파싱할 수 없으면 `None`(읽을 수 없는 본문). `null` 리터럴은 `Some(Value::Null)`이다.
fn parse_first_value(raw: &[u8]) -> Option<Value> {
    let body = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    serde_json::Deserializer::from_slice(body)
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// 객체 본문을 꺼낸다. `null`이면 검증기가 `IllegalArgumentException`을 던져 500이다. 객체가 아니면 읽을 수 없다.
fn parse_object(raw: &[u8]) -> Result<Map<String, Value>, ApiError> {
    match parse_first_value(raw) {
        Some(Value::Object(map)) => Ok(map),
        Some(Value::Null) => Err(ApiError::Internal(
            "validator: The object to be validated must not be null".to_owned(),
        )),
        _ => Err(unreadable_body()),
    }
}

/// Jackson `Integer`/`Long`: 정수 토큰, 소수(버림), 숫자 문자열(공백 제거). 빈 문자열은 `null`.
fn jackson_integer(value: Option<&Value>, min: i64, max: i64) -> Result<Option<i64>, ApiError> {
    let n = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i
            } else if n.as_u64().is_some() {
                return Err(unreadable_body());
            } else {
                let f = n.as_f64().ok_or_else(unreadable_body)?;
                let t = f.trunc();
                if !(min as f64..=max as f64).contains(&t) {
                    return Err(unreadable_body());
                }
                t as i64
            }
        }
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Ok(None);
            }
            let text = s.trim();
            let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(unreadable_body());
            }
            text.parse::<i64>().map_err(|_| unreadable_body())?
        }
        Some(_) => return Err(unreadable_body()),
    };
    if n < min || n > max {
        return Err(unreadable_body());
    }
    Ok(Some(n))
}

/// Jackson `Double`: 숫자 토큰이나 숫자 문자열.
fn jackson_double(value: Option<&Value>) -> Result<Option<f64>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.as_f64().ok_or_else(unreadable_body)?)),
        Some(Value::String(s)) => {
            if s.is_empty() {
                return Ok(None);
            }
            match s.trim() {
                "NaN" => Ok(Some(f64::NAN)),
                "Infinity" | "+Infinity" => Ok(Some(f64::INFINITY)),
                "-Infinity" => Ok(Some(f64::NEG_INFINITY)),
                text => text
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(Some)
                    .ok_or_else(unreadable_body),
            }
        }
        Some(_) => Err(unreadable_body()),
    }
}

/// Jackson `String`: 숫자·불리언도 문자열로 바꾼다. 배열·객체는 읽을 수 없다.
fn jackson_string(value: Option<&Value>) -> Result<Option<String>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(_) => Err(unreadable_body()),
    }
}

/// UTF-16 코드 유닛 수(`String.length()`). `@Size`가 센다.
fn java_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `PostImageProcessingCompleteRequest.Location`
#[derive(Clone, Copy, Debug, PartialEq)]
struct Location {
    latitude: Option<f64>,
    longitude: Option<f64>,
}

/// `PostImageProcessingCompleteRequest`
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompleteRequest {
    width: Option<i64>,
    height: Option<i64>,
    byte_size: Option<i64>,
    location: Option<Location>,
    captured_at: Option<String>,
    meta_attributes: Option<Map<String, Value>>,
}

impl CompleteRequest {
    /// 파싱 -> 검증(`@Valid`와 같은 제약).
    pub(crate) fn read(raw: &[u8]) -> Result<CompleteRequest, ApiError> {
        let map = parse_object(raw)?;
        let location = match map.get("location") {
            None | Some(Value::Null) => None,
            Some(Value::Object(l)) => Some(Location {
                latitude: jackson_double(l.get("latitude"))?,
                longitude: jackson_double(l.get("longitude"))?,
            }),
            Some(_) => return Err(unreadable_body()),
        };
        let meta_attributes = match map.get("metaAttributes") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => Some(m.clone()),
            Some(_) => return Err(unreadable_body()),
        };
        let request = CompleteRequest {
            width: jackson_integer(map.get("width"), i64::from(i32::MIN), i64::from(i32::MAX))?,
            height: jackson_integer(map.get("height"), i64::from(i32::MIN), i64::from(i32::MAX))?,
            byte_size: jackson_integer(map.get("byteSize"), i64::MIN, i64::MAX)?,
            location,
            captured_at: jackson_string(map.get("capturedAt"))?,
            meta_attributes,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), ApiError> {
        let positive = |v: Option<i64>| v.is_none_or(|v| v > 0);
        let in_range = |v: Option<f64>, bound: f64| {
            v.is_some_and(|v| !v.is_nan() && (-bound..=bound).contains(&v))
        };
        let ok = positive(self.width)
            && positive(self.height)
            && self.byte_size.is_none_or(|v| v >= 0)
            && self
                .location
                .is_none_or(|l| in_range(l.latitude, 90.0) && in_range(l.longitude, 180.0))
            && self
                .captured_at
                .as_deref()
                .is_none_or(|s| java_length(s) <= MAX_CAPTURED_AT_LENGTH)
            && self
                .meta_attributes
                .as_ref()
                .is_none_or(|m| m.len() <= MAX_META_ATTRIBUTES);
        if ok { Ok(()) } else { Err(invalid_body()) }
    }

    /// `toMetadata`: 값이 없어도 키는 남긴다. 위치는 `{latitude, longitude}`, 속성이 없으면 `{}`.
    pub(crate) fn to_metadata(&self) -> Map<String, Value> {
        let number = |v: Option<i64>| v.map_or(Value::Null, Value::from);
        let mut metadata = Map::new();
        metadata.insert("width".to_owned(), number(self.width));
        metadata.insert("height".to_owned(), number(self.height));
        metadata.insert("byteSize".to_owned(), number(self.byte_size));
        metadata.insert(
            "location".to_owned(),
            self.location.map_or(Value::Null, |l| {
                let coordinate = |v: Option<f64>| v.map_or(Value::Null, Value::from);
                let mut location = Map::new();
                location.insert("latitude".to_owned(), coordinate(l.latitude));
                location.insert("longitude".to_owned(), coordinate(l.longitude));
                Value::Object(location)
            }),
        );
        metadata.insert(
            "capturedAt".to_owned(),
            self.captured_at.clone().map_or(Value::Null, Value::from),
        );
        metadata.insert(
            "metaAttributes".to_owned(),
            Value::Object(self.meta_attributes.clone().unwrap_or_default()),
        );
        metadata
    }
}

/// `PostImageProcessingFailRequest`
pub(crate) fn read_fail_reason(raw: &[u8]) -> Result<String, ApiError> {
    let map = parse_object(raw)?;
    let reason = jackson_string(map.get("reason"))?;
    match reason {
        Some(reason)
            if !reason.chars().all(is_java_whitespace)
                && java_length(&reason) <= MAX_REASON_LENGTH =>
        {
            Ok(reason)
        }
        _ => Err(invalid_body()),
    }
}

// ---------------------------------------------------------------------------------------------
// 라우트
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostProcessingUploadUrlsResponse {
    original_upload_url: String,
    thumbnail_upload_url: String,
    content_type: String,
    cache_control: String,
}

#[derive(Clone)]
struct Callback {
    command: PostCommandService,
    authenticator: ProcessingCallbackAuthenticator,
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let callback = Callback {
        command: ctx.post.command_service(ctx),
        authenticator: ctx.post.authenticator(ctx),
    };
    let (for_urls, for_complete, for_failed) = (callback.clone(), callback.clone(), callback);
    routes
        .route(
            Method::Post,
            "/internal/v1/post-image-processing/{uploadId}/upload-urls",
            move |req| issue_upload_urls(&for_urls, req),
        )
        .route(
            Method::Post,
            "/internal/v1/post-image-processing/{uploadId}/complete",
            move |req| complete(&for_complete, req),
        )
        .route(
            Method::Post,
            "/internal/v1/post-image-processing/{uploadId}/failed",
            move |req| fail(&for_failed, req),
        )
}

fn callback_path(upload_id: Uuid, result: &str) -> String {
    format!("{CALLBACK_PATH}/{upload_id}/{result}")
}

fn raw_body(req: &Request) -> Option<&[u8]> {
    (!req.body.is_empty()).then_some(req.body.as_slice())
}

fn issue_upload_urls(callback: &Callback, req: &Request) -> ApiResult {
    let upload_id = parse_canonical_uuid(req.param("uploadId").unwrap_or_default())?;
    callback.authenticator.authenticate(
        &callback_path(upload_id, "upload-urls"),
        None,
        req.header(TIMESTAMP_HEADER),
        req.header(SIGNATURE_HEADER),
    )?;
    let issued = callback
        .command
        .issue_post_image_processing_upload(upload_id)?;
    Ok(ok_json(&PostProcessingUploadUrlsResponse {
        original_upload_url: issued.original_upload_url,
        thumbnail_upload_url: issued.thumbnail_upload_url,
        content_type: issued.content_type,
        cache_control: issued.cache_control,
    }))
}

fn complete(callback: &Callback, req: &Request) -> ApiResult {
    let upload_id = parse_canonical_uuid(req.param("uploadId").unwrap_or_default())?;
    callback.authenticator.authenticate(
        &callback_path(upload_id, "complete"),
        raw_body(req),
        req.header(TIMESTAMP_HEADER),
        req.header(SIGNATURE_HEADER),
    )?;
    let request = CompleteRequest::read(&req.body)?;
    callback
        .command
        .complete_post_image_processing(upload_id, request.to_metadata())?;
    Ok(Response::new(204))
}

fn fail(callback: &Callback, req: &Request) -> ApiResult {
    let upload_id = parse_canonical_uuid(req.param("uploadId").unwrap_or_default())?;
    callback.authenticator.authenticate(
        &callback_path(upload_id, "failed"),
        raw_body(req),
        req.header(TIMESTAMP_HEADER),
        req.header(SIGNATURE_HEADER),
    )?;
    let reason = read_fail_reason(&req.body)?;
    callback
        .command
        .fail_post_image_processing(upload_id, Some(&reason))?;
    Ok(Response::new(204))
}

#[cfg(test)]
mod tests {
    use chalkak_core::FixedClock;
    use jiff::Timestamp;

    use super::*;

    const AUDIENCE: &str = "dev";
    const SECRET: &str = "test-callback-secret-with-enough-length";
    const NOW: i64 = 1_791_774_000;
    const UPLOAD: &str = "0198f6c1-62ba-7d30-8b12-0f733b6570d4";

    fn authenticator_for(audience: &str) -> ProcessingCallbackAuthenticator {
        let clock = Arc::new(FixedClock::new(Timestamp::from_second(NOW).unwrap()));
        ProcessingCallbackAuthenticator::new(SECRET, audience, clock).unwrap()
    }

    fn authenticator() -> ProcessingCallbackAuthenticator {
        authenticator_for(AUDIENCE)
    }

    fn signature_path(result: &str) -> String {
        format!("/internal/v1/signature-processing/{UPLOAD}/{result}")
    }

    fn post_path(result: &str) -> String {
        format!("{CALLBACK_PATH}/{UPLOAD}/{result}")
    }

    fn check(
        path: &str,
        body: Option<&[u8]>,
        timestamp: Option<&str>,
        signature: Option<&str>,
    ) -> Result<(), ApiError> {
        authenticator().authenticate(path, body, timestamp, signature)
    }

    fn signed(path: &str, body: Option<&[u8]>, offset: i64) -> (String, String) {
        let timestamp = (NOW + offset).to_string();
        let signature = authenticator().sign(&timestamp, path, body);
        (timestamp, signature)
    }

    fn assert_unauthorized(result: Result<(), ApiError>) {
        match result {
            Err(ApiError::App(e)) => {
                assert_eq!(e.status(), 401);
                assert_eq!(e.message, "유효하지 않은 이미지 처리 콜백입니다.");
            }
            other => panic!("401이어야 합니다: {other:?}"),
        }
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_validSignature_succeeds
    #[test]
    fn authenticate_valid_signature_succeeds() {
        let path = signature_path("complete");
        let (ts, sig) = signed(&path, None, 0);
        check(&path, None, Some(&ts), Some(&sig)).unwrap();
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_invalidSignature_throwsUnauthorizedException
    #[test]
    fn authenticate_invalid_signature_throws_unauthorized_exception() {
        let path = signature_path("complete");
        assert_unauthorized(check(&path, None, Some(&NOW.to_string()), Some("invalid")));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_expiredTimestamp_throwsUnauthorizedException
    #[test]
    fn authenticate_expired_timestamp_throws_unauthorized_exception() {
        let path = signature_path("complete");
        let (ts, sig) = signed(&path, None, -301);
        assert_unauthorized(check(&path, None, Some(&ts), Some(&sig)));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_nullTimestamp_throwsUnauthorizedException
    #[test]
    fn authenticate_null_timestamp_throws_unauthorized_exception() {
        assert_unauthorized(check(
            &signature_path("complete"),
            None,
            None,
            Some("v1=00"),
        ));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_nullSignature_throwsUnauthorizedException
    #[test]
    fn authenticate_null_signature_throws_unauthorized_exception() {
        assert_unauthorized(check(
            &signature_path("complete"),
            None,
            Some(&NOW.to_string()),
            None,
        ));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_justInsideClockSkew_succeeds
    #[test]
    fn authenticate_just_inside_clock_skew_succeeds() {
        let path = signature_path("complete");
        let (ts, sig) = signed(&path, None, -299);
        check(&path, None, Some(&ts), Some(&sig)).unwrap();
        let (ts, sig) = signed(&path, None, -300);
        check(&path, None, Some(&ts), Some(&sig)).unwrap();
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_jsonBody_succeeds
    #[test]
    fn authenticate_json_body_succeeds() {
        let path = post_path("complete");
        let body = br#"{"width":4032,"height":3024}"#;
        let (ts, sig) = signed(&path, Some(body), 0);
        check(&path, Some(body), Some(&ts), Some(&sig)).unwrap();
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_reserializedBody_throwsUnauthorizedException
    #[test]
    fn authenticate_reserialized_body_throws_unauthorized_exception() {
        let path = post_path("complete");
        let (ts, sig) = signed(&path, Some(br#"{"width":4032,"height":3024}"#), 0);
        let reserialized = br#"{"width": 4032, "height": 3024}"#;
        assert_unauthorized(check(&path, Some(reserialized), Some(&ts), Some(&sig)));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_otherKindPath_throwsUnauthorizedException
    #[test]
    fn authenticate_other_kind_path_throws_unauthorized_exception() {
        let (ts, sig) = signed(&signature_path("complete"), None, 0);
        assert_unauthorized(check(&post_path("complete"), None, Some(&ts), Some(&sig)));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_invalidUtf8Body_succeeds
    #[test]
    fn authenticate_invalid_utf8_body_succeeds() {
        let path = post_path("complete");
        let body: &[u8] = &[0xC3, 0x28];
        let (ts, sig) = signed(&path, Some(body), 0);
        check(&path, Some(body), Some(&ts), Some(&sig)).unwrap();
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_differentInvalidUtf8Body_throwsUnauthorizedException
    #[test]
    fn authenticate_different_invalid_utf8_body_throws_unauthorized_exception() {
        let path = post_path("complete");
        let (ts, sig) = signed(&path, Some(&[0xC3, 0x28]), 0);
        assert_unauthorized(check(&path, Some(&[0xE0, 0x28]), Some(&ts), Some(&sig)));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_farFutureTimestamp_throwsUnauthorizedException
    #[test]
    fn authenticate_far_future_timestamp_throws_unauthorized_exception() {
        let path = signature_path("complete");
        let (ts, sig) = signed(&path, None, 60);
        assert_unauthorized(check(&path, None, Some(&ts), Some(&sig)));
        let (ts, sig) = signed(&path, None, 31);
        assert_unauthorized(check(&path, None, Some(&ts), Some(&sig)));
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_slightlyFutureTimestamp_succeeds
    #[test]
    fn authenticate_slightly_future_timestamp_succeeds() {
        let path = signature_path("complete");
        let (ts, sig) = signed(&path, None, 10);
        check(&path, None, Some(&ts), Some(&sig)).unwrap();
        let (ts, sig) = signed(&path, None, 30);
        check(&path, None, Some(&ts), Some(&sig)).unwrap();
    }

    // ProcessingCallbackAuthenticatorTest#authenticate_signatureForAnotherEnvironment_throwsUnauthorizedException
    #[test]
    fn authenticate_signature_for_another_environment_throws_unauthorized_exception() {
        let path = post_path("complete");
        let timestamp = NOW.to_string();
        let prod_signature = authenticator_for("prod").sign(&timestamp, &path, None);
        assert_unauthorized(check(&path, None, Some(&timestamp), Some(&prod_signature)));
    }

    #[test]
    fn 서명은_접두사와_짝수_길이_16진수여야_한다() {
        let path = post_path("complete");
        let (ts, sig) = signed(&path, None, 0);
        let digits = sig.strip_prefix("v1=").unwrap();
        assert_unauthorized(check(&path, None, Some(&ts), Some(digits)));
        assert_unauthorized(check(
            &path,
            None,
            Some(&ts),
            Some(&format!("v1={}", &digits[1..])),
        ));
        assert_unauthorized(check(&path, None, Some(&ts), Some("v1=zz")));
        check(
            &path,
            None,
            Some(&ts),
            Some(&format!("v1={}", digits.to_uppercase())),
        )
        .unwrap();
    }

    #[test]
    fn 타임스탬프가_숫자가_아니면_거부한다() {
        let path = post_path("complete");
        let (_, sig) = signed(&path, None, 0);
        assert_unauthorized(check(&path, None, Some("abc"), Some(&sig)));
        assert_unauthorized(check(&path, None, Some(""), Some(&sig)));
    }

    #[test]
    fn 비밀키는_32자_미만이면_만들_수_없다() {
        let clock = Arc::new(FixedClock::new(Timestamp::from_second(NOW).unwrap()));
        assert!(ProcessingCallbackAuthenticator::new("short", "dev", clock.clone()).is_err());
        assert!(ProcessingCallbackAuthenticator::new(SECRET, "  ", clock).is_err());
    }

    // PostImageProcessingCallbackController: 본문 읽기와 검증
    #[test]
    fn 완료_본문은_값이_없어도_키를_남겨_메타데이터로_만든다() {
        let request = CompleteRequest::read(br#"{"width":10,"height":20}"#).unwrap();
        let metadata = Value::Object(request.to_metadata());
        let expected: Value = serde_json::from_str(
            r#"{"width":10,"height":20,"byteSize":null,"location":null,"capturedAt":null,"metaAttributes":{}}"#,
        )
        .unwrap();
        assert_eq!(metadata, expected);
    }

    #[test]
    fn 완료_본문의_범위_밖_값은_거절한다() {
        for body in [
            r#"{"width":0}"#,
            r#"{"height":-1}"#,
            r#"{"byteSize":-1}"#,
            r#"{"location":{"latitude":91,"longitude":0}}"#,
            r#"{"location":{"latitude":0,"longitude":-181}}"#,
            r#"{"location":{"latitude":0}}"#,
            r#"{"location":{}}"#,
        ] {
            assert!(CompleteRequest::read(body.as_bytes()).is_err(), "{body}");
        }
        assert!(
            CompleteRequest::read(
                br#"{"location":{"latitude":90,"longitude":-180},"capturedAt":"x"}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn 실패_사유는_공백이_아니고_50자_이하여야_한다() {
        assert_eq!(read_fail_reason(br#"{"reason":"BAD"}"#).unwrap(), "BAD");
        assert!(read_fail_reason(b"{\"reason\":\"   \"}").is_err());
        assert!(read_fail_reason(b"{}").is_err());
        let long = format!(r#"{{"reason":"{}"}}"#, "r".repeat(51));
        assert!(read_fail_reason(long.as_bytes()).is_err());
    }
}
