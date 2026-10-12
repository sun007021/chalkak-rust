//! 인증 결과를 컨트롤러에 전하는 곳. Spring `SecurityContextHolder`에 해당한다.
//!
//! 서버의 인증 미들웨어가 Bearer 토큰을 검증한 뒤 [`enter`]로 주체를 올려 두고, 컨트롤러는 [`login_user`]로 꺼낸다.
//! 요청 하나를 워커 스레드 하나가 처음부터 끝까지 처리하므로 스레드 로컬이면 된다(`SecurityContextHolder` 기본
//! 전략 `MODE_THREADLOCAL`과 같다).

use std::cell::RefCell;

use chalkak_core::error::{AppError, ErrorCode};
use uuid::Uuid;

use crate::common::java_uuid_from_string;
use crate::web::ApiError;

/// `AccessTokenScope#toAuthority`.
pub const AUTHORITY_USER: &str = "SCOPE_USER";
pub const AUTHORITY_ADMIN: &str = "SCOPE_ADMIN";

/// 검증이 끝난 액세스 토큰의 주체.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// 토큰의 `sub`. 검증기가 `UUID.fromString`이 받는 값임을 이미 확인했다.
    pub subject: String,
    /// `scope`(또는 `scp`) 클레임에서 만든 권한. 예: `SCOPE_USER`.
    pub authorities: Vec<String>,
}

impl Principal {
    pub fn has_authority(&self, authority: &str) -> bool {
        self.authorities.iter().any(|a| a == authority)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Principal>> = const { RefCell::new(None) };
}

/// 가드를 버리면 이전 값으로 돌아간다.
#[must_use = "가드를 버리면 인증 범위가 바로 끝난다"]
pub struct PrincipalGuard {
    previous: Option<Principal>,
    _not_send: std::marker::PhantomData<*const ()>,
}

/// 현재 스레드에 주체를 올린다.
pub fn enter(principal: Principal) -> PrincipalGuard {
    let previous = CURRENT.with(|c| c.borrow_mut().replace(principal));
    PrincipalGuard {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for PrincipalGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// 현재 요청의 주체. 익명이면 `None`.
pub fn current() -> Option<Principal> {
    CURRENT.with(|c| c.borrow().clone())
}

/// `AuthenticatedUser`: 회원 API 컨트롤러가 받는 로그인 회원.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub user_id: Uuid,
}

fn unauthorized() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        "유효하지 않은 인증 정보입니다.",
    ))
}

/// `@LoginUser`: 주체의 `sub`를 회원 식별자로 읽는다. 주체가 없으면 401.
///
/// 관리자 토큰이 회원 식별자로 쓰이지 않게 막는 판단은 경로 인가 규칙(`SCOPE_USER` 요구)이 한다. 여기까지 온 주체는
/// 이미 회원 토큰이다.
pub fn login_user() -> Result<AuthenticatedUser, ApiError> {
    let principal = current().ok_or_else(unauthorized)?;
    let user_id = java_uuid_from_string(&principal.subject).ok_or_else(unauthorized)?;
    Ok(AuthenticatedUser { user_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(sub: &str) -> Principal {
        Principal {
            subject: sub.into(),
            authorities: vec![AUTHORITY_USER.into()],
        }
    }

    #[test]
    fn 주체가_없으면_401이다() {
        let err = login_user().unwrap_err();
        assert_eq!(err.status(), 401);
        assert_eq!(err.message(), "유효하지 않은 인증 정보입니다.");
    }

    #[test]
    fn 주체의_sub를_회원_식별자로_읽고_가드가_끝나면_돌아간다() {
        let sub = "01990000-0000-7000-8000-000000000001";
        {
            let _guard = enter(principal(sub));
            assert_eq!(login_user().unwrap().user_id, Uuid::parse_str(sub).unwrap());
            assert!(current().unwrap().has_authority(AUTHORITY_USER));
            assert!(!current().unwrap().has_authority(AUTHORITY_ADMIN));
        }
        assert!(current().is_none());
    }
}
