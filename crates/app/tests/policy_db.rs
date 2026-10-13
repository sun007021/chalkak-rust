//! 접근 판정(`UsableUserPolicy`)의 DB 테스트. `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.
//!
//! 원본 `UsableUserPolicyTest`는 저장소를 Mockito로 대체했지만 여기서는 실제 저장소와 PostgreSQL을 쓴다. 인증
//! 주체는 `security::enter`로 올린다. 주체가 없거나 액세스 토큰이 아닌 경우(`validateUsable_noAuthentication`,
//! `validateUsable_nonJwtPrincipal`)는 옮기지 않았다: 러스트에서 주체 해석(`@LoginUser`)은 판정 앞에서 401로 끝나
//! 판정에 닿지 않는다.

mod common;

use chalkak_app::auth::UsableUserPolicy;
use chalkak_app::security::{self, AUTHORITY_USER, Principal, PrincipalGuard};
use chalkak_core::error::{ErrorCode, ErrorKind};
use common::{World, assert_app_error};
use uuid::Uuid;

const UNAUTHORIZED_MESSAGE: &str = "유효하지 않은 인증 정보입니다.";

fn authentication_of(user_id: Uuid) -> PrincipalGuard {
    security::enter(Principal {
        subject: user_id.to_string(),
        authorities: vec![AUTHORITY_USER.to_owned()],
    })
}

fn policy(w: &World) -> UsableUserPolicy {
    UsableUserPolicy::new(w.pool.clone())
}

// UsableUserPolicyTest#validateUsable_suspendedUser_throwsForbiddenException
#[test]
fn validate_usable_suspended_user_throws_forbidden_exception() {
    let Some(w) = World::create() else { return };
    let user = w.save_banned_user();
    let _auth = authentication_of(user.id);

    assert_app_error(
        policy(&w).validate_usable(),
        ErrorKind::Forbidden,
        ErrorCode::FORBIDDEN,
        "차단된 회원입니다.",
    );
}

// UsableUserPolicyTest#validateUsable_activeUser_returnsTrue
#[test]
fn validate_usable_active_user_returns_true() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let _auth = authentication_of(user.id);

    assert_eq!(policy(&w).validate_usable().unwrap().user_id, user.id);
}

// UsableUserPolicyTest#validateUsable_unknownUser_throwsUnauthorizedException
#[test]
fn validate_usable_unknown_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let _auth = authentication_of(Uuid::new_v4());

    assert_app_error(
        policy(&w).validate_usable(),
        ErrorKind::Unauthorized,
        ErrorCode::UNAUTHORIZED,
        UNAUTHORIZED_MESSAGE,
    );
}

// UsableUserPolicyTest#validateUsable_withdrawnUser_throwsUnauthorizedException
#[test]
fn validate_usable_withdrawn_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.withdraw_directly(user.id);
    let _auth = authentication_of(user.id);

    assert_app_error(
        policy(&w).validate_usable(),
        ErrorKind::Unauthorized,
        ErrorCode::UNAUTHORIZED,
        UNAUTHORIZED_MESSAGE,
    );
}

// UsableUserPolicyTest#validateExisting_activeUser_returnsTrue
#[test]
fn validate_existing_active_user_returns_true() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    let _auth = authentication_of(user.id);

    assert_eq!(policy(&w).validate_existing().unwrap().user_id, user.id);
}

// UsableUserPolicyTest#validateExisting_suspendedUser_returnsTrue
#[test]
fn validate_existing_suspended_user_returns_true() {
    let Some(w) = World::create() else { return };
    let user = w.save_banned_user();
    let _auth = authentication_of(user.id);

    assert_eq!(policy(&w).validate_existing().unwrap().user_id, user.id);
}

// UsableUserPolicyTest#validateExisting_withdrawnUser_throwsUnauthorizedException
#[test]
fn validate_existing_withdrawn_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let user = w.save_user();
    w.withdraw_directly(user.id);
    let _auth = authentication_of(user.id);

    assert_app_error(
        policy(&w).validate_existing(),
        ErrorKind::Unauthorized,
        ErrorCode::UNAUTHORIZED,
        UNAUTHORIZED_MESSAGE,
    );
}

// UsableUserPolicyTest#validateExisting_unknownUser_throwsUnauthorizedException
#[test]
fn validate_existing_unknown_user_throws_unauthorized_exception() {
    let Some(w) = World::create() else { return };
    let _auth = authentication_of(Uuid::new_v4());

    assert_app_error(
        policy(&w).validate_existing(),
        ErrorKind::Unauthorized,
        ErrorCode::UNAUTHORIZED,
        UNAUTHORIZED_MESSAGE,
    );
}
