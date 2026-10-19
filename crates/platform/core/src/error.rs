//! 오류 모델과 오류 응답 본문.
//!
//! 원본 `com.chalkak.backend.exception` 패키지(`ErrorCode`, `BaseException` 계열, `DomainErrorHttpMapper`,
//! `ErrorResponse`)를 옮긴 것이다.
//!
//! - 예외 종류 -> 상태 코드: Business 400, NotFound 404, Unauthorized 401, Forbidden 403
//!   (`DomainErrorHttpMapper`). 500은 도메인 오류가 아니라 처리되지 않은 실패라서 [`internal_error_body`]로만 만든다.
//! - 응답 본문은 Jackson이 `record ErrorResponse(String errorCode, String message)`를 직렬화한 결과와 바이트까지
//!   같다: `{"errorCode":"...","message":"..."}`. 필드 순서가 고정이고 공백이 없으며 한글은 그대로 두고,
//!   `" \ \b \f \n \r \t`는 짧은 이스케이프, 그 밖의 0x20 미만 제어문자는 `\u00XX`(16진수 대문자)로 쓴다.
//!   `/`, 0x7F 이상(U+2028 포함)은 이스케이프하지 않는다.

use std::fmt;

/// 원본 `ErrorCode` enum 10개. 직렬화 이름이 곧 변형 이름이다(`"BUSINESS_ERROR"` 등).
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum ErrorCode {
    BUSINESS_ERROR,
    SIGNATURE_REGISTRATION_REQUIRED,
    SIGNATURE_REUPLOAD_REQUIRED,
    SIGNATURE_PROCESSING_PENDING,
    RESOURCE_STATE_CHANGED,
    /// 액세스 토큰만 만료된 상태. 클라이언트가 재발급 후 다시 요청하면 된다.
    UNAUTHORIZED,
    /// 재발급까지 실패해 저장한 토큰을 버리고 로그인 화면으로 보내야 하는 경우.
    REAUTHENTICATION_REQUIRED,
    FORBIDDEN,
    INTERNAL_ERROR,
    /// 이미 도는 집계가 끝난 뒤 다시 요청하면 되므로 입력을 고쳐야 하는 `BUSINESS_ERROR`와 재시도 방식이 다르다.
    USER_METRICS_AGGREGATION_IN_PROGRESS,
}

impl ErrorCode {
    pub const ALL: [Self; 10] = [
        Self::BUSINESS_ERROR,
        Self::SIGNATURE_REGISTRATION_REQUIRED,
        Self::SIGNATURE_REUPLOAD_REQUIRED,
        Self::SIGNATURE_PROCESSING_PENDING,
        Self::RESOURCE_STATE_CHANGED,
        Self::UNAUTHORIZED,
        Self::REAUTHENTICATION_REQUIRED,
        Self::FORBIDDEN,
        Self::INTERNAL_ERROR,
        Self::USER_METRICS_AGGREGATION_IN_PROGRESS,
    ];

    /// 원본 `ErrorCode.name()`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BUSINESS_ERROR => "BUSINESS_ERROR",
            Self::SIGNATURE_REGISTRATION_REQUIRED => "SIGNATURE_REGISTRATION_REQUIRED",
            Self::SIGNATURE_REUPLOAD_REQUIRED => "SIGNATURE_REUPLOAD_REQUIRED",
            Self::SIGNATURE_PROCESSING_PENDING => "SIGNATURE_PROCESSING_PENDING",
            Self::RESOURCE_STATE_CHANGED => "RESOURCE_STATE_CHANGED",
            Self::UNAUTHORIZED => "UNAUTHORIZED",
            Self::REAUTHENTICATION_REQUIRED => "REAUTHENTICATION_REQUIRED",
            Self::FORBIDDEN => "FORBIDDEN",
            Self::INTERNAL_ERROR => "INTERNAL_ERROR",
            Self::USER_METRICS_AGGREGATION_IN_PROGRESS => "USER_METRICS_AGGREGATION_IN_PROGRESS",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 원본 예외 계층(`BusinessException`, `NotFoundException`, `UnauthorizedException`, `ForbiddenException`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400
    Business,
    /// 404
    NotFound,
    /// 401
    Unauthorized,
    /// 403
    Forbidden,
}

impl ErrorKind {
    /// 원본 `DomainErrorHttpMapper.statusOf`.
    pub const fn status(self) -> u16 {
        match self {
            Self::Business => 400,
            Self::NotFound => 404,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
        }
    }
}

/// 도메인 오류. 핸들러가 `Err(AppError)`로 돌려주면 서버가 [`AppError::status`]와 [`AppError::to_json`]으로
/// 응답을 만든다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    pub kind: ErrorKind,
    pub code: ErrorCode,
    pub message: String,
}

impl AppError {
    pub fn new(kind: ErrorKind, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            kind,
            code,
            message: message.into(),
        }
    }

    /// `new BusinessException(code, message)`
    pub fn business(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Business, code, message)
    }

    /// `new NotFoundException(code, message)`
    pub fn not_found(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, code, message)
    }

    /// `new UnauthorizedException(code, message)`
    pub fn unauthorized(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, code, message)
    }

    /// `new ForbiddenException(code, message)`
    pub fn forbidden(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Forbidden, code, message)
    }

    pub const fn status(&self) -> u16 {
        self.kind.status()
    }

    /// 응답 본문 `{"errorCode":"...","message":"..."}`.
    pub fn to_json(&self) -> String {
        error_body(self.code, &self.message)
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

/// 오류 응답 본문. Jackson 출력과 바이트 동일.
pub fn error_body(code: ErrorCode, message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 48);
    out.push_str("{\"errorCode\":\"");
    out.push_str(code.as_str());
    out.push_str("\",\"message\":\"");
    escape_json_into(&mut out, message);
    out.push_str("\"}");
    out
}

/// 처리되지 않은 예외(원본 `GlobalExceptionHandler.handleUnexpectedException`)의 500 본문.
pub fn internal_error_body() -> String {
    error_body(ErrorCode::INTERNAL_ERROR, messages::INTERNAL)
}

/// Jackson 문자열 이스케이프.
fn escape_json_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// `GlobalExceptionHandler`가 쓰는 고정 문구. 응답 `message`는 클라이언트가 사용자에게 그대로 보여준다.
pub mod messages {
    /// `MethodArgumentNotValidException` 등에서 제약 문구가 없을 때.
    pub const INVALID_VALUE: &str = "요청 값이 올바르지 않습니다.";
    /// `HttpMessageNotReadableException`
    pub const UNREADABLE_BODY: &str = "JSON 형식이 올바르지 않거나 요청 본문이 비어 있습니다.";
    /// `MethodArgumentTypeMismatchException` 및 바인딩 실패
    pub const INVALID_FORMAT: &str = "요청 값의 형식이 올바르지 않습니다.";
    /// 필수 파라미터·헤더 누락
    pub const MISSING_VALUE: &str = "필수 요청 값이 없습니다.";
    /// `NoHandlerFoundException`·`NoResourceFoundException`
    pub const API_NOT_FOUND: &str = "요청한 API를 찾을 수 없습니다.";
    /// Spring이 만든 그 밖의 4xx(405 등)
    pub const UNSUPPORTED_REQUEST: &str = "지원하지 않는 요청 방식이거나 형식입니다.";
    /// 처리되지 않은 예외(500)
    pub const INTERNAL: &str = "서버에서 요청을 처리하지 못했습니다.";

    /// 원본 `fieldMessage`: 필드명이 있으면 `필드: 문구`, 비었거나 공백이면 문구만.
    pub fn field_message(field: Option<&str>, message: &str) -> String {
        match field {
            Some(f) if !f.trim().is_empty() => format!("{f}: {message}"),
            _ => message.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 원본_enum_10개가_그대로_있다() {
        let names: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            [
                "BUSINESS_ERROR",
                "SIGNATURE_REGISTRATION_REQUIRED",
                "SIGNATURE_REUPLOAD_REQUIRED",
                "SIGNATURE_PROCESSING_PENDING",
                "RESOURCE_STATE_CHANGED",
                "UNAUTHORIZED",
                "REAUTHENTICATION_REQUIRED",
                "FORBIDDEN",
                "INTERNAL_ERROR",
                "USER_METRICS_AGGREGATION_IN_PROGRESS",
            ]
        );
    }

    #[test]
    fn serde_직렬화_이름은_as_str과_같다() {
        for code in ErrorCode::ALL {
            assert_eq!(
                serde_json::to_string(&code).unwrap(),
                format!("\"{}\"", code.as_str())
            );
        }
    }

    // DomainErrorHttpMapper
    #[test]
    fn 오류_종류별_상태_코드() {
        assert_eq!(ErrorKind::Business.status(), 400);
        assert_eq!(ErrorKind::NotFound.status(), 404);
        assert_eq!(ErrorKind::Unauthorized.status(), 401);
        assert_eq!(ErrorKind::Forbidden.status(), 403);
    }

    #[test]
    fn 생성자는_원본_예외_종류와_짝이다() {
        let e = AppError::not_found(ErrorCode::BUSINESS_ERROR, "없음");
        assert_eq!((e.kind, e.status()), (ErrorKind::NotFound, 404));
        let e = AppError::unauthorized(ErrorCode::REAUTHENTICATION_REQUIRED, "다시 로그인");
        assert_eq!((e.kind, e.status()), (ErrorKind::Unauthorized, 401));
        let e = AppError::forbidden(ErrorCode::FORBIDDEN, "권한 없음");
        assert_eq!((e.kind, e.status()), (ErrorKind::Forbidden, 403));
        let e = AppError::business(ErrorCode::SIGNATURE_PROCESSING_PENDING, "처리 중");
        assert_eq!((e.kind, e.status()), (ErrorKind::Business, 400));
    }

    // GlobalExceptionHandler.handleApiNotFoundException: ErrorResponse("BUSINESS_ERROR", "요청한 API를 찾을 수 없습니다.")
    // 기대 바이트는 Jackson 3.1.4(Spring Boot 4.1.0)로 직접 직렬화한 결과다.
    #[test]
    fn 한글_본문은_이스케이프_없이_그대로() {
        let body = error_body(ErrorCode::BUSINESS_ERROR, messages::API_NOT_FOUND);
        assert_eq!(
            body,
            "{\"errorCode\":\"BUSINESS_ERROR\",\"message\":\"요청한 API를 찾을 수 없습니다.\"}"
        );
    }

    #[test]
    fn app_error_to_json_은_필드_순서가_고정이다() {
        let body =
            AppError::business(ErrorCode::RESOURCE_STATE_CHANGED, "상태가 바뀌었습니다.").to_json();
        assert_eq!(
            body,
            "{\"errorCode\":\"RESOURCE_STATE_CHANGED\",\"message\":\"상태가 바뀌었습니다.\"}"
        );
    }

    // Jackson 3.1.4가 같은 입력에 내놓은 출력(Java로 생성해 옮김).
    #[test]
    fn 이스케이프는_jackson_출력과_바이트까지_같다() {
        let input = "q\"uote \\ back / slash \u{8}\u{c}\n\r\t \u{0} \u{1f} \u{1b} \u{7f} \u{80} \u{9f} \u{a0} é \u{2028} \u{2029} <>&' 😀 \u{fffd}";
        let expected = concat!(
            "{\"errorCode\":\"BUSINESS_ERROR\",\"message\":\"",
            "q\\\"uote \\\\ back / slash \\b\\f\\n\\r\\t \\u0000 \\u001F \\u001B ",
            "\u{7f} \u{80} \u{9f} \u{a0} é \u{2028} \u{2029} <>&' 😀 \u{fffd}",
            "\"}"
        );
        assert_eq!(error_body(ErrorCode::BUSINESS_ERROR, input), expected);
    }

    // GlobalExceptionHandler.handleUnexpectedException
    #[test]
    fn 처리되지_않은_예외_본문() {
        assert_eq!(
            internal_error_body(),
            "{\"errorCode\":\"INTERNAL_ERROR\",\"message\":\"서버에서 요청을 처리하지 못했습니다.\"}"
        );
    }

    // GlobalExceptionHandler.fieldMessage
    #[test]
    fn 필드_메시지() {
        use messages::field_message;
        assert_eq!(
            field_message(Some("size"), messages::INVALID_FORMAT),
            "size: 요청 값의 형식이 올바르지 않습니다."
        );
        assert_eq!(
            field_message(None, messages::MISSING_VALUE),
            messages::MISSING_VALUE
        );
        assert_eq!(
            field_message(Some("  "), messages::MISSING_VALUE),
            messages::MISSING_VALUE
        );
    }

    #[test]
    fn display_는_코드와_메시지() {
        let e = AppError::forbidden(ErrorCode::FORBIDDEN, "접근 불가");
        assert_eq!(e.to_string(), "FORBIDDEN: 접근 불가");
    }
}
