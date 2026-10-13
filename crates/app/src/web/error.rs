//! 컨트롤러 오류 → 응답. 원본 `GlobalExceptionHandler`와 `DomainErrorHttpMapper`에 해당한다.
//!
//! | 원본 예외 | 여기서 | 응답 |
//! | --- | --- | --- |
//! | `BaseException` 계열 | [`ApiError::App`] | 종류별 400/404/401/403 + 코드·메시지 |
//! | `MissingServletRequestParameterException` | [`ApiError::MissingRequestParameter`] | 400 `이름: 필수 요청 값이 없습니다.` |
//! | `MethodArgumentTypeMismatchException` | [`ApiError::ArgumentTypeMismatch`] | 400 `이름: 요청 값의 형식이 올바르지 않습니다.` |
//! | `HttpMessageNotReadableException` | [`ApiError::UnreadableBody`] | 400 `JSON 형식이 올바르지 않거나...` |
//! | `MethodArgumentNotValidException` | [`ApiError::Validation`] | 400 제약 메시지 |
//! | `HttpMediaTypeNotSupportedException` | [`ApiError::UnsupportedMediaType`] | 415 `지원하지 않는 요청 방식이거나 형식입니다.` |
//! | `HttpRequestMethodNotSupportedException` | [`ApiError::MethodNotSupported`] | 405 같은 문구 |
//! | `NoResourceFoundException` | [`ApiError::NotFound`] | 404 `요청한 API를 찾을 수 없습니다.` |
//! | 그 밖의 예외 | [`ApiError::Internal`] | 500 `INTERNAL_ERROR` |
//!
//! 4xx는 requestId와 함께 warn 로그를 남기고(예외 메시지에는 사용자 값이 섞일 수 있어 종류만 기록), 5xx는 error 로그를
//! 남긴다. 없는 경로(404)는 접근 로그가 이미 status를 남기므로 따로 기록하지 않는다.

use std::fmt;

use chalkak_core::error::{
    AppError, ErrorCode, ErrorKind, error_body, internal_error_body, messages,
};
use chalkak_core::log::{Logger, fields};
use chalkak_db::DbError;
use chalkak_http::Response;

use super::{json_response, marker};

const LOGGER_NAME: &str = "com.chalkak.backend.exception.GlobalExceptionHandler";

#[derive(Debug)]
pub enum ApiError {
    /// 도메인 규칙 위반 등 `BaseException` 계열.
    App(AppError),
    /// 필수 요청 파라미터(쿼리) 누락. 값은 파라미터 이름.
    MissingRequestParameter(String),
    /// 요청 파라미터 형식 오류. 값은 파라미터 이름.
    ArgumentTypeMismatch(String),
    /// 본문이 비었거나 JSON 형식·타입이 맞지 않음.
    UnreadableBody,
    /// Bean Validation 위반. 값은 제약에 적힌 메시지.
    Validation(String),
    UnsupportedMediaType,
    MethodNotSupported,
    NotFound,
    /// 처리되지 않은 실패. 값은 로그에만 남는 원인 설명이다(응답에는 나가지 않는다).
    Internal(String),
}

impl ApiError {
    /// 필드 이름이 붙은 메시지(`이름: 문구`)를 만든다.
    fn field_message(field: &str, message: &str) -> String {
        messages::field_message(Some(field), message)
    }

    pub fn status(&self) -> u16 {
        match self {
            ApiError::App(e) => e.status(),
            ApiError::MissingRequestParameter(_)
            | ApiError::ArgumentTypeMismatch(_)
            | ApiError::UnreadableBody
            | ApiError::Validation(_) => 400,
            ApiError::UnsupportedMediaType => 415,
            ApiError::MethodNotSupported => 405,
            ApiError::NotFound => 404,
            ApiError::Internal(_) => 500,
        }
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            ApiError::App(e) => e.code,
            ApiError::Internal(_) => ErrorCode::INTERNAL_ERROR,
            _ => ErrorCode::BUSINESS_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ApiError::App(e) => e.message.clone(),
            ApiError::MissingRequestParameter(name) => {
                Self::field_message(name, messages::MISSING_VALUE)
            }
            ApiError::ArgumentTypeMismatch(name) => {
                Self::field_message(name, messages::INVALID_FORMAT)
            }
            ApiError::UnreadableBody => messages::UNREADABLE_BODY.to_owned(),
            ApiError::Validation(message) => message.clone(),
            ApiError::UnsupportedMediaType | ApiError::MethodNotSupported => {
                messages::UNSUPPORTED_REQUEST.to_owned()
            }
            ApiError::NotFound => messages::API_NOT_FOUND.to_owned(),
            ApiError::Internal(_) => messages::INTERNAL.to_owned(),
        }
    }

    /// 원본 예외 클래스 단순 이름. 로그의 `exception` 필드(대시보드 집계용)에 쓴다.
    pub fn exception_name(&self) -> &'static str {
        match self {
            ApiError::App(e) => match e.kind {
                ErrorKind::Business => "BusinessException",
                ErrorKind::NotFound => "NotFoundException",
                ErrorKind::Unauthorized => "UnauthorizedException",
                ErrorKind::Forbidden => "ForbiddenException",
            },
            ApiError::MissingRequestParameter(_) => "MissingServletRequestParameterException",
            ApiError::ArgumentTypeMismatch(_) => "MethodArgumentTypeMismatchException",
            ApiError::UnreadableBody => "HttpMessageNotReadableException",
            ApiError::Validation(_) => "MethodArgumentNotValidException",
            ApiError::UnsupportedMediaType => "HttpMediaTypeNotSupportedException",
            ApiError::MethodNotSupported => "HttpRequestMethodNotSupportedException",
            ApiError::NotFound => "NoResourceFoundException",
            ApiError::Internal(_) => "Exception",
        }
    }

    /// 오류 응답을 만들고 로그를 남긴다. 콘텐츠 협상 계층이 읽는 출처 표지가 붙는다([`marker`]).
    pub fn into_response(self, logger: &Logger) -> Response {
        self.log(logger);
        let origin = match self {
            ApiError::App(_) | ApiError::Internal(_) => marker::HANDLER,
            _ => marker::SPRING,
        };
        let response = self.to_response().with_header(marker::ORIGIN, origin);
        match self {
            ApiError::UnsupportedMediaType => {
                response.with_header(marker::CARRY_ACCEPT, marker::SUPPORTED_BODY_TYPES)
            }
            _ => response,
        }
    }

    /// 로그 없이 응답만 만든다.
    pub fn to_response(&self) -> Response {
        let body = match self {
            ApiError::Internal(_) => internal_error_body(),
            other => error_body(other.code(), &other.message()),
        };
        json_response(self.status(), body)
    }

    fn log(&self, logger: &Logger) {
        match self {
            // 접근 로그가 status 404로 이미 남기고, 스캐너 요청이 경고 로그를 채우지 않도록 기록하지 않는다.
            ApiError::NotFound => {}
            ApiError::Internal(cause) => {
                let cause = InternalCause(cause.clone());
                logger
                    .error(LOGGER_NAME)
                    .field(fields::TYPE, fields::TYPE_ERROR)
                    .field(fields::ERROR_CODE, ErrorCode::INTERNAL_ERROR.as_str())
                    .field(fields::STATUS, 500)
                    .error(&cause)
                    .log("처리되지 않은 예외");
            }
            other => {
                logger
                    .warn(LOGGER_NAME)
                    .field(fields::TYPE, fields::TYPE_ERROR)
                    .field(fields::ERROR_CODE, other.code().as_str())
                    .field(fields::STATUS, other.status())
                    .field(fields::EXCEPTION, other.exception_name())
                    .log("요청 실패");
            }
        }
    }
}

#[derive(Debug)]
struct InternalCause(String);

impl fmt::Display for InternalCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InternalCause {}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status(), self.message())
    }
}

impl std::error::Error for ApiError {}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        ApiError::App(e)
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        ApiError::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GlobalExceptionHandler와 harness/golden의 메시지 표.
    #[test]
    fn 메시지와_상태는_원본_표를_따른다() {
        let cases: Vec<(ApiError, u16, &str, &str)> = vec![
            (
                ApiError::MissingRequestParameter("date".into()),
                400,
                "BUSINESS_ERROR",
                "date: 필수 요청 값이 없습니다.",
            ),
            (
                ApiError::ArgumentTypeMismatch("date".into()),
                400,
                "BUSINESS_ERROR",
                "date: 요청 값의 형식이 올바르지 않습니다.",
            ),
            (
                ApiError::UnreadableBody,
                400,
                "BUSINESS_ERROR",
                "JSON 형식이 올바르지 않거나 요청 본문이 비어 있습니다.",
            ),
            (
                ApiError::Validation("피드백 내용이 필요합니다.".into()),
                400,
                "BUSINESS_ERROR",
                "피드백 내용이 필요합니다.",
            ),
            (
                ApiError::UnsupportedMediaType,
                415,
                "BUSINESS_ERROR",
                "지원하지 않는 요청 방식이거나 형식입니다.",
            ),
            (
                ApiError::MethodNotSupported,
                405,
                "BUSINESS_ERROR",
                "지원하지 않는 요청 방식이거나 형식입니다.",
            ),
            (
                ApiError::NotFound,
                404,
                "BUSINESS_ERROR",
                "요청한 API를 찾을 수 없습니다.",
            ),
            (
                ApiError::Internal("boom".into()),
                500,
                "INTERNAL_ERROR",
                "서버에서 요청을 처리하지 못했습니다.",
            ),
            (
                ApiError::App(AppError::not_found(
                    ErrorCode::BUSINESS_ERROR,
                    "주제를 찾을 수 없습니다.",
                )),
                404,
                "BUSINESS_ERROR",
                "주제를 찾을 수 없습니다.",
            ),
        ];
        for (error, status, code, message) in cases {
            let response = error.to_response();
            assert_eq!(response.status, status, "{error}");
            assert_eq!(
                response.headers.get("content-type"),
                Some("application/json")
            );
            assert_eq!(
                String::from_utf8(response.body).unwrap(),
                format!("{{\"errorCode\":\"{code}\",\"message\":\"{message}\"}}")
            );
        }
    }

    #[test]
    fn 내부_오류의_원인은_응답에_나가지_않는다() {
        let response =
            ApiError::Internal("connection refused: password=secret".into()).to_response();
        let body = String::from_utf8(response.body).unwrap();
        assert!(!body.contains("secret"));
    }
}
