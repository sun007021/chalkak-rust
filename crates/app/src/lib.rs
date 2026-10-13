//! 도메인 모듈. Java 패키지(auth, user, topic, post, like, feedback, notification, admin, photo)와
//! 1:1로 대응시킨다. 도메인 순서대로 채운다.
//!
//! 모듈 구성은 원본 패키지를 따른다: `api`(컨트롤러·DTO) / `service` / `domain` / `repository`.
//! 여러 도메인이 함께 쓰는 웹 계층 보조(JSON 출력, 오류 응답, 요청 바인딩, 라우트 등록)는 [`web`]에 있다.
//!
//! 지금 들어 있는 것:
//! - [`topic`]: 날짜로 주제 조회(`GET /api/v1/topics`)와 주제 도메인 전체(생성·수정·삭제 규칙, 저장소 질의).
//! - [`notification`]: 알림함 조회·읽음 처리, 검수 결과 알림 생성, 관리자 Slack 알림, 보관 기간이 지난 알림 정리 쿼리.
//! - [`feedback`], [`user`], [`auth`], [`admin`]: 하네스 platform 시나리오가 건드리는 최소 조각.
//!   (`POST /api/v1/feedbacks`, 회원 존재 확인, 관리자 로그아웃 요청 검증.) 해당 도메인 이슈에서 확장한다.

pub mod admin;
pub mod auth;
pub mod common;
pub mod feedback;
pub mod like;
pub mod notification;
pub mod photo;
pub mod post;
pub mod security;
#[doc(hidden)]
pub mod testing;
pub mod topic;
pub mod user;
pub mod web;

use std::sync::Arc;

use chalkak_core::{Clock, log::Logger};
use chalkak_db::Pool;

/// 도메인 조립에 필요한 공용 의존성. 서버(`chalkak-server`)가 만들어 넘긴다.
#[derive(Clone)]
pub struct AppContext {
    pub pool: Pool,
    pub clock: Arc<dyn Clock>,
    pub logger: Arc<Logger>,
    /// 게시물·사진·좋아요 흐름이 서버 조립에서 받는 것(설정, S3 호출, 검수 대기 구독자).
    pub post: post::PostWiring,
}

/// 모든 도메인의 라우트를 등록한다.
pub fn register_routes(routes: web::Routes, ctx: &AppContext) -> web::Routes {
    let routes = topic::api::register(routes, ctx);
    let routes = feedback::api::register(routes, ctx);
    let routes = notification::api::register(routes, ctx);
    let routes = post::api::register(routes, ctx);
    let routes = like::api::register(routes, ctx);
    let routes = post::callback::register(routes, ctx);
    admin::auth_api::register(routes)
}
