//! 원본 `AuthController`(`/api/v1/auth/**`)와 요청·응답 DTO.
//!
//! 흐름은 다른 도메인과 같다: 본문 읽기와 `@Valid` 검증 -> 서비스. 이 경로들은 보안 설정에서 공개라 `@RequiresExistingUser`가
//! 없다.

use chalkak_http::Request;
use serde::Serialize;
use serde_json::Value;

use super::domain::SocialProvider;
use super::refresh::UserRefreshTokenService;
use super::service::{SocialLoginResult, SocialLoginService, SocialSignupService};
use crate::AppContext;
use crate::web::binding::is_java_whitespace;
use crate::web::{ApiError, ApiResult, Routes, binding, ok_json};
use crate::wiring::Components;

const PROVIDER_REQUIRED: &str = "소셜 로그인 제공자는 필수입니다.";
const ID_TOKEN_REQUIRED: &str = "ID Token은 필수입니다.";
const RAW_NONCE_REQUIRED: &str = "rawNonce는 필수입니다.";
const AUTHORIZATION_CODE_INVALID: &str = "Authorization Code는 APPLE 로그인에만 필요합니다.";
const REFRESH_TOKEN_REQUIRED: &str = "리프레시 토큰은 필수입니다.";
const SIGNUP_TOKEN_REQUIRED: &str = "회원가입 토큰은 필수입니다.";

// ---------------------------------------------------------------------------------------------
// 요청
// ---------------------------------------------------------------------------------------------

fn validation(message: &str) -> ApiError {
    ApiError::Validation(message.to_owned())
}

/// `@NotBlank`(`String.isBlank`).
fn not_blank(value: Option<String>, message: &str) -> Result<String, ApiError> {
    match value {
        Some(v) if !v.chars().all(is_java_whitespace) => Ok(v),
        _ => Err(validation(message)),
    }
}

/// Jackson이 `SocialProvider` 열거형을 읽는 규칙. 없거나 `null`이면 `None`(이어서 `@NotNull`이 거절한다).
/// - 문자열: 앞뒤 `trim()`(U+0020 이하) 뒤 이름이 정확히 같아야 한다. 비면 `null`로 본다. 숫자로 시작하면 서수 문자열로
///   본다.
/// - 정수: 서수. 범위 밖이거나 실수·불리언·배열·객체이면 본문 해석 오류(400).
pub fn parse_provider(value: Option<&Value>) -> Result<Option<SocialProvider>, ApiError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim_matches(|c: char| c <= ' ');
            if text.is_empty() {
                return Ok(None);
            }
            if let Some(provider) = SocialProvider::from_name(text) {
                return Ok(Some(provider));
            }
            if text.as_bytes()[0].is_ascii_digit() {
                // `Integer.parseInt`: 범위를 넘으면 실패한다.
                if let Some(provider) = text
                    .parse::<i32>()
                    .ok()
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(SocialProvider::from_ordinal)
                {
                    return Ok(Some(provider));
                }
            }
            Err(ApiError::UnreadableBody)
        }
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|i| usize::try_from(i).ok())
            .and_then(SocialProvider::from_ordinal)
            .map(Some)
            .ok_or(ApiError::UnreadableBody),
        Some(_) => Err(ApiError::UnreadableBody),
    }
}

struct ProviderCredentials {
    provider: SocialProvider,
    id_token: String,
    raw_nonce: String,
}

/// `provider`, `idToken`, `rawNonce`의 검증(필드 선언 순서). 로그인과 업로드 URL 발급이 함께 쓴다.
fn read_credentials(
    body: &binding::JsonObject,
) -> Result<(ProviderCredentials, Option<String>), ApiError> {
    let provider = parse_provider(body.value("provider"))?;
    let id_token = body.string("idToken")?;
    let raw_nonce = body.string("rawNonce")?;
    let authorization_code = body.string("authorizationCode")?;
    let provider = provider.ok_or_else(|| validation(PROVIDER_REQUIRED))?;
    let id_token = not_blank(id_token, ID_TOKEN_REQUIRED)?;
    let raw_nonce = not_blank(raw_nonce, RAW_NONCE_REQUIRED)?;
    Ok((
        ProviderCredentials {
            provider,
            id_token,
            raw_nonce,
        },
        authorization_code,
    ))
}

/// `SocialLoginRequest#isValidAuthorizationCode`: APPLE이면 공백이 아닌 값이 필요하고 그 밖에는 없어야 한다.
fn validate_authorization_code(
    provider: SocialProvider,
    code: Option<&str>,
) -> Result<(), ApiError> {
    let valid = match provider {
        SocialProvider::Apple => code.is_some_and(|c| !c.chars().all(is_java_whitespace)),
        _ => code.is_none(),
    };
    if valid {
        Ok(())
    } else {
        Err(validation(AUTHORIZATION_CODE_INVALID))
    }
}

// ---------------------------------------------------------------------------------------------
// 응답
// ---------------------------------------------------------------------------------------------

/// `SocialLoginResponse`(`NON_NULL`): 가입이 필요하면 `status`만 내려간다.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialLoginResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token_expires_in: Option<i64>,
}

impl From<SocialLoginResult> for SocialLoginResponse {
    fn from(result: SocialLoginResult) -> Self {
        match result {
            SocialLoginResult::SignUpRequired => SocialLoginResponse {
                status: "SIGN_UP_REQUIRED",
                user_id: None,
                access_token: None,
                expires_in: None,
                refresh_token: None,
                refresh_token_expires_in: None,
            },
            SocialLoginResult::LoginSuccess(s) => SocialLoginResponse {
                status: "LOGIN_SUCCESS",
                user_id: Some(s.user_id.to_string()),
                access_token: Some(s.access_token.value),
                expires_in: Some(s.access_token.expires_in_seconds),
                refresh_token: Some(s.refresh_token.value),
                refresh_token_expires_in: Some(s.refresh_token.expires_in_seconds),
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialSignupResponse {
    user_id: String,
    access_token: String,
    expires_in: i64,
    refresh_token: String,
    refresh_token_expires_in: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialSignupSignatureUploadResponse {
    upload_id: String,
    upload_url: String,
    expires_in_seconds: i64,
    signup_token: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenRefreshResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: String,
    refresh_token_expires_in: i64,
}

// ---------------------------------------------------------------------------------------------
// 라우트
// ---------------------------------------------------------------------------------------------

pub fn register(routes: Routes, _ctx: &AppContext, components: &Components) -> Routes {
    let login = components.login.clone();
    let signup = components.signup.clone();
    let signup_upload = components.signup.clone();
    let refresh = components.refresh.clone();
    let logout = components.refresh.clone();
    routes
        .post("/api/v1/auth/social-login", move |req| {
            social_login(&login, req)
        })
        .post("/api/v1/auth/social-signup/signature/uploads", move |req| {
            create_signup_signature_upload(&signup_upload, req)
        })
        .post("/api/v1/auth/social-signup", move |req| {
            social_signup(&signup, req)
        })
        .post("/api/v1/auth/refresh", move |req| {
            refresh_token(&refresh, req)
        })
        .post("/api/v1/auth/logout", move |req| logout_token(&logout, req))
}

fn social_login(service: &SocialLoginService, req: &Request) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let (credentials, authorization_code) = read_credentials(&body)?;
    validate_authorization_code(credentials.provider, authorization_code.as_deref())?;
    let result = service.login(
        credentials.provider,
        &credentials.id_token,
        &credentials.raw_nonce,
        authorization_code.as_deref(),
    )?;
    Ok(ok_json(&SocialLoginResponse::from(result)))
}

fn create_signup_signature_upload(service: &SocialSignupService, req: &Request) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let (credentials, _) = read_credentials(&body)?;
    let result = service.create_signature_upload(
        credentials.provider,
        &credentials.id_token,
        &credentials.raw_nonce,
    )?;
    Ok(ok_json(&SocialSignupSignatureUploadResponse {
        upload_id: result.upload.upload_id.to_string(),
        upload_url: result.upload.upload_url,
        expires_in_seconds: result.upload.expires_in_seconds,
        signup_token: result.signup_token.value,
    }))
}

fn social_signup(service: &SocialSignupService, req: &Request) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let signup_token = not_blank(body.string("signupToken")?, SIGNUP_TOKEN_REQUIRED)?;
    let result = service.signup(&signup_token)?;
    Ok(ok_json(&SocialSignupResponse {
        user_id: result.user_id.to_string(),
        access_token: result.access_token.value,
        expires_in: result.access_token.expires_in_seconds,
        refresh_token: result.refresh_token.value,
        refresh_token_expires_in: result.refresh_token.expires_in_seconds,
    }))
}

fn refresh_token(service: &UserRefreshTokenService, req: &Request) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let token = not_blank(body.string("refreshToken")?, REFRESH_TOKEN_REQUIRED)?;
    let result = service.refresh(&token)?;
    Ok(ok_json(&TokenRefreshResponse {
        access_token: result.access_token.value,
        expires_in: result.access_token.expires_in_seconds,
        refresh_token: result.refresh_token.value,
        refresh_token_expires_in: result.refresh_token.expires_in_seconds,
    }))
}

fn logout_token(service: &UserRefreshTokenService, req: &Request) -> ApiResult {
    let body = binding::read_json_object(req)?;
    let token = not_blank(body.string("refreshToken")?, REFRESH_TOKEN_REQUIRED)?;
    service.logout(&token)?;
    Ok(chalkak_http::Response::new(204))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::json::to_json;
    use serde_json::json;

    fn provider(v: Value) -> Result<Option<SocialProvider>, ApiError> {
        parse_provider(Some(&v))
    }

    // SocialLoginRequest / Jackson 열거형 해석
    #[test]
    fn 제공자는_이름이나_서수로_읽는다() {
        assert_eq!(
            provider(json!("GOOGLE")).unwrap(),
            Some(SocialProvider::Google)
        );
        assert_eq!(
            provider(json!("  KAKAO\n")).unwrap(),
            Some(SocialProvider::Kakao)
        );
        assert_eq!(provider(json!(2)).unwrap(), Some(SocialProvider::Apple));
        assert_eq!(provider(json!("1")).unwrap(), Some(SocialProvider::Kakao));
        assert_eq!(provider(json!(Value::Null)).unwrap(), None);
        assert_eq!(parse_provider(None).unwrap(), None);
    }

    #[test]
    fn 제공자_형식이_틀리면_본문_오류다() {
        for bad in [
            json!("google"),
            json!("NAVER"),
            json!(3),
            json!(-1),
            json!(1.5),
            json!(true),
            json!([]),
            json!({}),
            json!("3"),
            json!("1x"),
        ] {
            assert!(
                matches!(provider(bad.clone()), Err(ApiError::UnreadableBody)),
                "{bad}"
            );
        }
    }

    #[test]
    fn 인증_코드는_애플에만_필요하다() {
        use SocialProvider::*;
        assert!(validate_authorization_code(Apple, Some("code")).is_ok());
        assert!(validate_authorization_code(Apple, None).is_err());
        assert!(validate_authorization_code(Apple, Some("  ")).is_err());
        assert!(validate_authorization_code(Google, None).is_ok());
        let err = validate_authorization_code(Kakao, Some("code")).unwrap_err();
        assert_eq!(err.message(), AUTHORIZATION_CODE_INVALID);
        assert!(validate_authorization_code(Google, Some("")).is_err());
    }

    #[test]
    fn 필수_문자열은_공백이면_거절한다() {
        assert!(not_blank(Some("a".into()), "m").is_ok());
        for v in [None, Some(String::new()), Some(" \t\n".into())] {
            assert_eq!(not_blank(v, "m").unwrap_err().message(), "m");
        }
    }

    // SocialLoginResponse: @JsonInclude(NON_NULL)
    #[test]
    fn 가입이_필요하면_상태만_내려간다() {
        let body = to_json(&SocialLoginResponse::from(
            SocialLoginResult::SignUpRequired,
        ));
        assert_eq!(body, r#"{"status":"SIGN_UP_REQUIRED"}"#);
    }
}
