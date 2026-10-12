//! 관리자 API가 함께 쓰는 보조: 의존성 묶음, 현재 관리자 해석(`AdminActorResolver`), 경로 식별자 해석, 트랜잭션,
//! 페이지 결과.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_core::log::Logger;
use chalkak_db::{DbError, Pool};
use jiff::Timestamp;
use postgres::Transaction;
use uuid::Uuid;

use super::config::{AdminConfig, ImageUrlProvider, InvalidStorageKey};
use crate::common::java_uuid_from_string;
use crate::security::{self, AUTHORITY_ADMIN};
use crate::web::ApiError;

/// 개발 우회 모드에서 `CurrentAdmin`이 가리키는 계정(`DevelopmentAdminBootstrap.DEVELOPMENT_ADMIN_USERNAME`).
pub const DEVELOPMENT_ADMIN_USERNAME: &str = "dev-admin";

/// 관리자 서비스가 공통으로 받는 의존성.
#[derive(Clone)]
pub struct AdminDeps {
    pub pool: Pool,
    pub clock: Arc<dyn Clock>,
    pub logger: Arc<Logger>,
    pub config: Arc<AdminConfig>,
}

impl AdminDeps {
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    pub fn image_urls(&self) -> ImageUrlProvider {
        self.config.image_urls()
    }
}

impl From<InvalidStorageKey> for ApiError {
    fn from(e: InvalidStorageKey) -> Self {
        ApiError::Internal(e.0)
    }
}

/// `204 No Content`(본문 없음).
pub fn no_content() -> chalkak_http::Response {
    chalkak_http::Response::new(204)
}

/// 원본 `AdminActorResolver`: 요청을 보낸 관리자의 식별자.
pub fn resolve_current_admin(deps: &AdminDeps) -> Result<Uuid, ApiError> {
    if deps.config.development_bypass {
        let mut conn = deps.pool.get()?;
        let row = conn
            .query_opt(
                "SELECT id FROM admins WHERE username = $1",
                &[&DEVELOPMENT_ADMIN_USERNAME],
            )
            .map_err(DbError::from)?;
        return match row {
            Some(row) => Ok(row.try_get(0).map_err(DbError::from)?),
            None => Err(ApiError::Internal(
                "개발 관리자 계정이 존재하지 않습니다.".to_owned(),
            )),
        };
    }
    admin_from_principal(security::current())
}

/// 우회가 꺼져 있을 때의 해석(`SecurityContextAdminActorResolver`).
pub fn admin_from_principal(principal: Option<security::Principal>) -> Result<Uuid, ApiError> {
    let Some(principal) = principal else {
        return Err(ApiError::App(AppError::unauthorized(
            ErrorCode::UNAUTHORIZED,
            "유효하지 않은 인증 정보입니다.",
        )));
    };
    if !principal.has_authority(AUTHORITY_ADMIN) {
        return Err(ApiError::App(AppError::forbidden(
            ErrorCode::FORBIDDEN,
            "관리자 권한이 필요합니다.",
        )));
    }
    java_uuid_from_string(&principal.subject)
        .ok_or_else(|| ApiError::Internal(format!("Invalid UUID string: {}", principal.subject)))
}

/// `CanonicalUuidParser.parse`: 소문자 표준 형식(대소문자 무시)만 받는다.
pub fn parse_path_uuid(value: &str) -> Result<Uuid, ApiError> {
    let invalid = || {
        ApiError::App(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            "ID 형식이 올바르지 않습니다.",
        ))
    };
    let uuid = java_uuid_from_string(value).ok_or_else(invalid)?;
    if !uuid.hyphenated().to_string().eq_ignore_ascii_case(value) {
        return Err(invalid());
    }
    Ok(uuid)
}

/// 요청의 경로 변수를 `CanonicalUuidParser`로 읽는다.
pub fn path_uuid(req: &chalkak_http::Request, name: &str) -> Result<Uuid, ApiError> {
    parse_path_uuid(req.param(name).unwrap_or_default())
}

/// 트랜잭션 안에서 `f`를 실행한다. `Ok`면 커밋, `Err`면 롤백한다(`@Transactional`의 기본 동작).
pub fn in_transaction<T>(
    conn: &mut chalkak_db::Conn,
    f: impl FnOnce(&mut Transaction<'_>) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let client: &mut postgres::Client = conn;
    let mut tx = client.transaction().map_err(DbError::from)?;
    match f(&mut tx) {
        Ok(value) => {
            tx.commit().map_err(DbError::from)?;
            Ok(value)
        }
        Err(e) => {
            // 롤백 실패는 원래 오류를 가리지 않는다.
            let _ = tx.rollback();
            Err(e)
        }
    }
}

/// 원본 `AdminPage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminPage<T> {
    pub items: Vec<T>,
    pub current_page: i32,
    pub page_size: i32,
    pub has_next: bool,
}

impl<T> AdminPage<T> {
    /// `pageSize + 1`건을 읽은 결과를 슬라이스로 자른다(Spring Data `Slice.hasNext`).
    pub fn from_overfetched(mut rows: Vec<T>, page: i32, page_size: i32) -> AdminPage<T> {
        let limit = usize::try_from(page_size).unwrap_or(0);
        let has_next = rows.len() > limit;
        rows.truncate(limit);
        AdminPage {
            items: rows,
            current_page: page,
            page_size,
            has_next,
        }
    }

    pub fn map<U>(self, f: impl FnMut(T) -> U) -> AdminPage<U> {
        AdminPage {
            items: self.items.into_iter().map(f).collect(),
            current_page: self.current_page,
            page_size: self.page_size,
            has_next: self.has_next,
        }
    }
}

/// 서비스 계층의 페이지 검사. 원본은 서비스마다 같은 식을 쓰고 메시지만 다르다.
pub fn validate_pagination(page: i32, page_size: i32, message: &str) -> Result<(), AppError> {
    let offset = (i64::from(page) - 1) * i64::from(page_size);
    if page < 1 || !(1..=100).contains(&page_size) || offset > i64::from(i32::MAX) {
        return Err(AppError::business(ErrorCode::BUSINESS_ERROR, message));
    }
    Ok(())
}

/// 조회 SQL의 `LIMIT`/`OFFSET`. `pageSize + 1`건을 읽어 다음 페이지 여부를 안다.
pub fn limit_offset(page: i32, page_size: i32) -> (i64, i64) {
    (
        i64::from(page_size) + 1,
        (i64::from(page) - 1) * i64::from(page_size),
    )
}

/// 동적 `WHERE` 절 조립. 조건 문자열의 `?`를 `$n` 자리표시자로 바꾼다.
#[derive(Default)]
pub struct SqlFilter {
    conditions: Vec<String>,
    params: Vec<Box<dyn postgres::types::ToSql + Sync>>,
}

impl SqlFilter {
    pub fn new() -> SqlFilter {
        SqlFilter::default()
    }

    /// 값이 있을 때만 조건을 더한다.
    pub fn add_opt<T: postgres::types::ToSql + Sync + 'static>(
        &mut self,
        condition: &str,
        value: Option<T>,
    ) {
        if let Some(value) = value {
            self.add(condition, value);
        }
    }

    pub fn add<T: postgres::types::ToSql + Sync + 'static>(&mut self, condition: &str, value: T) {
        self.params.push(Box::new(value));
        let placeholder = format!("${}", self.params.len());
        self.conditions.push(condition.replace('?', &placeholder));
    }

    /// 값 없는 고정 조건.
    pub fn add_fixed(&mut self, condition: &str) {
        self.conditions.push(condition.to_owned());
    }

    pub fn where_clause(&self) -> String {
        if self.conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", self.conditions.join(" AND "))
        }
    }

    /// 다음에 붙일 자리표시자 번호(`LIMIT`/`OFFSET`용).
    pub fn next_placeholder(&self) -> usize {
        self.params.len() + 1
    }

    pub fn params_with<'a>(
        &'a self,
        extra: &[&'a (dyn postgres::types::ToSql + Sync)],
    ) -> Vec<&'a (dyn postgres::types::ToSql + Sync)> {
        let mut all: Vec<&(dyn postgres::types::ToSql + Sync)> =
            self.params.iter().map(|p| p.as_ref()).collect();
        all.extend_from_slice(extra);
        all
    }
}

pub const INVALID_QUERY_MESSAGE: &str = "조회 조건이 올바르지 않습니다.";

pub fn business(message: &str) -> ApiError {
    ApiError::App(AppError::business(ErrorCode::BUSINESS_ERROR, message))
}

pub fn not_found(message: &str) -> ApiError {
    ApiError::App(AppError::not_found(ErrorCode::BUSINESS_ERROR, message))
}

pub fn state_changed(message: &str) -> ApiError {
    ApiError::App(AppError::business(
        ErrorCode::RESOURCE_STATE_CHANGED,
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AUTHORITY_USER, Principal};

    // CanonicalUuidParser
    #[test]
    fn 경로_식별자는_표준_형식만_받는다() {
        let id = "01990000-0000-7000-8000-0000000000a1";
        assert_eq!(parse_path_uuid(id).unwrap(), Uuid::parse_str(id).unwrap());
        assert_eq!(
            parse_path_uuid(&id.to_uppercase()).unwrap(),
            Uuid::parse_str(id).unwrap()
        );
        for bad in [
            "",
            "abc",
            "1-2-3-4-5",
            "+1990000-0000-7000-8000-0000000000a1",
            "01990000-0000-7000-8000-0000000000a1 ",
            "{01990000-0000-7000-8000-0000000000a1}",
            "01990000000070008000000000a1",
        ] {
            let e = parse_path_uuid(bad).unwrap_err();
            assert_eq!(e.status(), 400, "{bad}");
            assert_eq!(e.message(), "ID 형식이 올바르지 않습니다.");
        }
    }

    fn principal(sub: &str, authorities: &[&str]) -> Principal {
        Principal {
            subject: sub.to_owned(),
            authorities: authorities.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    // SecurityContextAdminActorResolverTest
    #[test]
    fn 주체가_없으면_401이다() {
        let e = admin_from_principal(None).unwrap_err();
        assert_eq!(e.status(), 401);
        assert_eq!(e.message(), "유효하지 않은 인증 정보입니다.");
    }

    #[test]
    fn 관리자_권한이_없으면_403이다() {
        let e = admin_from_principal(Some(principal(
            "01990000-0000-7000-8000-0000000000a1",
            &[AUTHORITY_USER],
        )))
        .unwrap_err();
        assert_eq!(e.status(), 403);
        assert_eq!(e.message(), "관리자 권한이 필요합니다.");
        assert_eq!(e.code().as_str(), "FORBIDDEN");
    }

    #[test]
    fn 관리자_권한이_있으면_sub를_식별자로_읽는다() {
        let id = "01990000-0000-7000-8000-0000000000a1";
        assert_eq!(
            admin_from_principal(Some(principal(id, &[AUTHORITY_ADMIN]))).unwrap(),
            Uuid::parse_str(id).unwrap()
        );
    }

    #[test]
    fn 슬라이스는_한_건_더_읽어_다음_페이지를_안다() {
        let page = AdminPage::from_overfetched(vec![1, 2, 3], 1, 2);
        assert_eq!(page.items, vec![1, 2]);
        assert!(page.has_next);
        let last = AdminPage::from_overfetched(vec![1, 2], 2, 2);
        assert!(!last.has_next);
        assert_eq!(limit_offset(3, 20), (21, 40));
    }

    #[test]
    fn 페이지_검사() {
        let m = "x";
        assert!(validate_pagination(1, 20, m).is_ok());
        assert!(validate_pagination(1, 100, m).is_ok());
        for (p, s) in [(0, 20), (1, 0), (1, 101), (-1, 20)] {
            assert!(validate_pagination(p, s, m).is_err(), "{p} {s}");
        }
        // 2147483647 * 100 쪽 오프셋은 int 범위를 넘는다.
        assert!(validate_pagination(i32::MAX, 100, m).is_err());
        assert!(validate_pagination(i32::MAX, 1, m).is_ok());
    }
}
