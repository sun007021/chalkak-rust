//! 원본 `UserController`(`/api/v1/users/me/**`)와 `SignatureProcessingCallbackController`
//! (`/internal/v1/signature-processing/**`), 요청·응답 DTO.

use chalkak_http::{Request, Response};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::service::{UserService, UserWithdrawalService};
use crate::AppContext;
use crate::auth::UsableUserPolicy;
use crate::auth::callback::ProcessingCallbackAuthenticator;
use crate::common::{base64_std_decode, java_len, parse_canonical_uuid};
use crate::web::{ApiError, ApiResult, Routes, binding, ok_json};
use crate::wiring::Components;
use chalkak_core::error::{AppError, ErrorCode};

const CALLBACK_PATH: &str = "/internal/v1/signature-processing";
const TIMESTAMP_HEADER: &str = "X-Chalkak-Callback-Timestamp";
const SIGNATURE_HEADER: &str = "X-Chalkak-Callback-Signature";
const UPLOAD_ID_INVALID: &str = "사인 이미지 업로드 정보가 올바르지 않습니다.";

// ---------------------------------------------------------------------------------------------
// 응답
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserSignatureUploadResponse {
    upload_id: String,
    upload_url: String,
    expires_in_seconds: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserSignatureDetailResponse {
    signature_original_image_url: String,
    signature_thumbnail_image_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserSignatureResponse {
    signature_original_image_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignatureProcessingUploadUrlsResponse {
    original_upload_url: String,
    thumbnail_upload_url: String,
    content_type: String,
    cache_control: String,
}

// ---------------------------------------------------------------------------------------------
// 요청
// ---------------------------------------------------------------------------------------------

/// Jackson이 `UUID`를 읽는 규칙(`UUIDDeserializer`). 문자열만 받고, 앞뒤 `trim()` 뒤 비면 `null`로 본다. 길이 36은
/// 하이픈 표준형, 24는 Base64(16바이트)이고 그 밖에는 본문 해석 오류(400)다. 없거나 `null`이면 `@NotNull`이 거절한다.
pub fn parse_upload_id_field(value: Option<&Value>) -> Result<Uuid, ApiError> {
    let missing = || ApiError::Validation(UPLOAD_ID_INVALID.to_owned());
    let text = match value {
        None | Some(Value::Null) => return Err(missing()),
        Some(Value::String(s)) => s.trim_matches(|c: char| c <= ' '),
        Some(_) => return Err(ApiError::UnreadableBody),
    };
    if text.is_empty() {
        return Err(missing());
    }
    match java_len(text) {
        36 => Uuid::parse_str(text).map_err(|_| ApiError::UnreadableBody),
        24 => base64_std_decode(text)
            .and_then(|bytes| Uuid::from_slice(&bytes).ok())
            .ok_or(ApiError::UnreadableBody),
        _ => Err(ApiError::UnreadableBody),
    }
}

// ---------------------------------------------------------------------------------------------
// 라우트
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, _ctx: &AppContext, components: &Components) -> Routes {
    let policy = components.policy.clone();
    let withdrawal = components.withdrawal.clone();
    let withdraw_policy = policy.clone();
    let upload_policy = policy.clone();
    let upload_users = components.users.clone();
    let get_policy = policy.clone();
    let get_users = components.users.clone();
    let update_policy = policy;
    let update_users = components.users.clone();

    let callback_auth = components.callback_auth.clone();
    let callbacks = components.users.clone();
    let urls_auth = callback_auth.clone();
    let urls_users = callbacks.clone();
    let complete_auth = callback_auth.clone();
    let complete_users = callbacks.clone();
    let fail_auth = callback_auth;
    let fail_users = callbacks;

    routes
        .delete("/api/v1/users/me", move |_req| {
            withdraw(&withdraw_policy, &withdrawal)
        })
        .post("/api/v1/users/me/signature/uploads", move |_req| {
            create_signature_upload(&upload_policy, &upload_users)
        })
        .get("/api/v1/users/me/signature", move |_req| {
            get_signature(&get_policy, &get_users)
        })
        .put("/api/v1/users/me/signature", move |req| {
            update_signature(&update_policy, &update_users, req)
        })
        .post(
            "/internal/v1/signature-processing/{uploadId}/upload-urls",
            move |req| issue_upload_urls(&urls_auth, &urls_users, req),
        )
        .post(
            "/internal/v1/signature-processing/{uploadId}/complete",
            move |req| complete(&complete_auth, &complete_users, req),
        )
        .post(
            "/internal/v1/signature-processing/{uploadId}/failed",
            move |req| fail(&fail_auth, &fail_users, req),
        )
}

// `@RequiresExistingUser`/`@RequiresUsableUser`는 인자 해석 뒤, 메서드 실행 앞에서 판정한다.

fn withdraw(policy: &UsableUserPolicy, service: &UserWithdrawalService) -> ApiResult {
    let user = policy.validate_existing()?;
    service.withdraw(user.user_id)?;
    Ok(Response::new(204))
}

fn create_signature_upload(policy: &UsableUserPolicy, service: &UserService) -> ApiResult {
    let user = policy.validate_usable()?;
    let upload = service.create_signature_upload(user.user_id)?;
    Ok(ok_json(&UserSignatureUploadResponse {
        upload_id: upload.upload_id.to_string(),
        upload_url: upload.upload_url,
        expires_in_seconds: upload.expires_in_seconds,
    }))
}

fn get_signature(policy: &UsableUserPolicy, service: &UserService) -> ApiResult {
    let user = policy.validate_existing()?;
    let result = service.get_signature(user.user_id)?;
    Ok(ok_json(&UserSignatureDetailResponse {
        signature_original_image_url: result.original_image_url,
        signature_thumbnail_image_url: result.thumbnail_image_url,
    }))
}

fn update_signature(policy: &UsableUserPolicy, service: &UserService, req: &Request) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let upload_id = parse_upload_id_field(body.value("signatureOriginalUploadId"))?;
    let user = policy.validate_usable()?;
    let image_url = service.update_signature(user.user_id, upload_id)?;
    Ok(ok_json(&UserSignatureResponse {
        signature_original_image_url: image_url,
    }))
}

// ---------------------------------------------------------------------------------------------
// 이미지 처리 콜백
// ---------------------------------------------------------------------------------------------

/// 경로 변수를 표준형 UUID로 읽고(아니면 400), 서명 대상 경로를 정규화된 값으로 만들어 인증한다. 헤더가 없어도 400이
/// 아니라 인증 실패(401)다. 본문은 읽지 않으므로 서명 대상 본문은 비어 있다.
fn authenticate(
    authenticator: &ProcessingCallbackAuthenticator,
    req: &Request,
    result: &str,
) -> Result<Uuid, ApiError> {
    let raw = req.param("uploadId").unwrap_or("");
    let upload_id = parse_canonical_uuid(raw).ok_or_else(|| {
        ApiError::App(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            "ID 형식이 올바르지 않습니다.",
        ))
    })?;
    let path = format!("{CALLBACK_PATH}/{upload_id}/{result}");
    authenticator.authenticate(
        &path,
        None,
        req.headers.get(TIMESTAMP_HEADER),
        req.headers.get(SIGNATURE_HEADER),
    )?;
    Ok(upload_id)
}

fn issue_upload_urls(
    authenticator: &ProcessingCallbackAuthenticator,
    service: &UserService,
    req: &Request,
) -> ApiResult {
    let upload_id = authenticate(authenticator, req, "upload-urls")?;
    let upload = service.issue_signature_processing_upload(upload_id)?;
    Ok(ok_json(&SignatureProcessingUploadUrlsResponse {
        original_upload_url: upload.original_upload_url,
        thumbnail_upload_url: upload.thumbnail_upload_url,
        content_type: upload.content_type,
        cache_control: upload.cache_control,
    }))
}

fn complete(
    authenticator: &ProcessingCallbackAuthenticator,
    service: &UserService,
    req: &Request,
) -> ApiResult {
    let upload_id = authenticate(authenticator, req, "complete")?;
    service.complete_signature_processing(upload_id)?;
    Ok(Response::new(204))
}

fn fail(
    authenticator: &ProcessingCallbackAuthenticator,
    service: &UserService,
    req: &Request,
) -> ApiResult {
    let upload_id = authenticate(authenticator, req, "failed")?;
    service.fail_signature_processing(upload_id)?;
    Ok(Response::new(204))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "0198f6c1-62ba-7d30-8b12-0f733b6570d4";

    fn parse(v: Value) -> Result<Uuid, ApiError> {
        parse_upload_id_field(Some(&v))
    }

    // UserSignatureUpdateRequest: @NotNull + Jackson UUID 해석
    #[test]
    fn 업로드_id는_표준형과_base64를_읽는다() {
        let expected = Uuid::parse_str(ID).unwrap();
        assert_eq!(parse(json!(ID)).unwrap(), expected);
        assert_eq!(
            parse(json!(format!("  {}\n", ID.to_uppercase()))).unwrap(),
            expected
        );
        let b64 = crate::common::base64_std_encode(expected.as_bytes());
        assert_eq!(b64.len(), 24);
        assert_eq!(parse(json!(b64)).unwrap(), expected);
    }

    #[test]
    fn 업로드_id가_없거나_비면_검증_오류다() {
        for v in [None, Some(Value::Null), Some(json!("")), Some(json!("  "))] {
            let err = parse_upload_id_field(v.as_ref()).unwrap_err();
            assert_eq!(err.status(), 400);
            assert_eq!(err.message(), UPLOAD_ID_INVALID);
        }
    }

    #[test]
    fn 업로드_id_형식이_틀리면_본문_오류다() {
        for v in [
            json!("abc"),
            json!(ID.replace('-', "")),
            json!(format!("{ID}0")),
            json!("0198f6c1-62ba-7d30-8b12-0f733b6570dz"),
            json!(1),
            json!(true),
            json!([ID]),
            json!({}),
            json!("!!!!!!!!!!!!!!!!!!!!!!!!"),
        ] {
            assert!(
                matches!(parse(v.clone()), Err(ApiError::UnreadableBody)),
                "{v}"
            );
        }
    }
}
