//! 도메인 모듈. Java 패키지(auth, user, topic, post, like, feedback, notification, admin, photo)와
//! 1:1로 대응시킨다. 도메인 순서대로 채운다.
//!
//! 모듈 구성은 원본 패키지를 따른다: `api`(컨트롤러·DTO) / `service` / `domain` / `repository`.
//! 여러 도메인이 함께 쓰는 웹 계층 보조(JSON 출력, 오류 응답, 요청 바인딩, 라우트 등록)는 [`web`]에 있다.
//!
//! 지금 들어 있는 것:
//! - [`topic`]: 날짜로 주제 조회(`GET /api/v1/topics`)와 주제 도메인 전체(생성·수정·삭제 규칙, 저장소 질의).
//! - [`feedback`], [`user`], [`auth`], [`admin`]: 하네스 platform 시나리오가 건드리는 최소 조각.
//!   (`POST /api/v1/feedbacks`, 회원 존재 확인, 관리자 로그아웃 요청 검증.) 해당 도메인 이슈에서 확장한다.

pub mod admin;
pub mod auth;
pub mod common;
pub mod config;
pub mod feedback;
pub mod security;
#[doc(hidden)]
pub mod testing;
pub mod topic;
pub mod user;
pub mod web;
pub mod wiring;

use std::sync::Arc;

use chalkak_core::{Clock, log::Logger};
use chalkak_db::Pool;

/// 도메인 조립에 필요한 공용 의존성. 서버(`chalkak-server`)가 만들어 넘긴다.
#[derive(Clone)]
pub struct AppContext {
    pub pool: Pool,
    pub clock: Arc<dyn Clock>,
    pub logger: Arc<Logger>,
    /// auth·user 도메인 설정.
    pub settings: Arc<config::AppSettings>,
}

/// 모든 도메인의 라우트를 등록한다.
pub fn register_routes(routes: web::Routes, ctx: &AppContext) -> web::Routes {
    // 설정이 잘못되면 서버를 띄우지 않는다(스프링의 빈 생성 실패와 같다). 설정 값 검증은 `AppSettings::from_env`가 먼저 한다.
    let components = wiring::Components::build(ctx)
        .unwrap_or_else(|e| panic!("auth·user 구성 요소를 만들 수 없습니다: {e}"));
    let routes = topic::api::register(routes, ctx);
    let routes = feedback::api::register(routes, ctx);
    let routes = auth::api::register(routes, ctx, &components);
    let routes = user::api::register(routes, ctx, &components);
    admin::auth_api::register(routes)
}
