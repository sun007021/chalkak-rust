//! 요청 하나 동안 안쪽 계층이 바깥 계층(접근 로그)에 알려 주는 값.
//!
//! 원본 접근 로그는 인증보다 먼저 실행되는 필터가 쓰므로, 매칭된 경로 템플릿(`BEST_MATCHING_PATTERN_ATTRIBUTE`)과
//! 로그인 회원 식별자를 요청 속성으로 받아 간다. 요청 하나를 워커 스레드 하나가 처음부터 끝까지 처리하므로
//! 같은 일을 스레드 로컬로 한다(MDC·`SecurityContextHolder`와 같은 성질).

use std::cell::RefCell;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestInfo {
    /// 매칭된 경로 템플릿(`/api/v1/topics`). 라우트에 닿기 전에 거절된 요청은 `None`.
    pub route: Option<String>,
    /// 로그인 회원 식별자. 회원 토큰일 때만 채운다(관리자 토큰의 `sub`는 회원 식별자가 아니다).
    pub user_id: Option<String>,
}

thread_local! {
    static CURRENT: RefCell<RequestInfo> = RefCell::new(RequestInfo::default());
}

/// 요청 처리를 시작하며 값을 비운다. 가드를 버리면 다시 비운다.
#[must_use = "가드를 버리면 요청 범위가 바로 끝난다"]
pub struct RequestScope {
    _not_send: std::marker::PhantomData<*const ()>,
}

pub fn begin() -> RequestScope {
    CURRENT.with(|c| *c.borrow_mut() = RequestInfo::default());
    RequestScope {
        _not_send: std::marker::PhantomData,
    }
}

impl RequestScope {
    /// 지금까지 모인 값을 꺼낸다.
    pub fn info(&self) -> RequestInfo {
        CURRENT.with(|c| c.borrow().clone())
    }
}

impl Drop for RequestScope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = RequestInfo::default());
    }
}

/// 매칭된 경로 템플릿을 기록한다.
pub fn set_route(route: &str) {
    CURRENT.with(|c| c.borrow_mut().route = Some(route.to_owned()));
}

/// 로그인 회원 식별자를 기록한다.
pub fn set_user_id(user_id: String) {
    CURRENT.with(|c| c.borrow_mut().user_id = Some(user_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 범위_안에서_모은_값을_꺼내고_끝나면_비운다() {
        let scope = begin();
        assert_eq!(scope.info(), RequestInfo::default());
        set_route("/api/v1/topics");
        set_user_id("u".into());
        assert_eq!(
            scope.info(),
            RequestInfo {
                route: Some("/api/v1/topics".into()),
                user_id: Some("u".into())
            }
        );
        drop(scope);
        assert_eq!(begin().info(), RequestInfo::default());
    }
}
