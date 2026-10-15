//! 관리자 도메인(원본 `admin` 패키지).
//!
//! 하위 구성: `auth`(로그인·토큰), `audit`(감사 로그), `user`·`post`·`topic`·`feedback`(관리 API),
//! `bootstrap`(시작 시 계정 보장).

pub mod audit;
pub mod auth;
pub mod binding;
pub mod bootstrap;
pub mod config;
#[cfg(test)]
mod db_tests;
#[cfg(test)]
mod endpoint_tests;
pub mod feedback;
#[cfg(test)]
mod lock_db_tests;
pub mod post;
pub mod support;
#[cfg(test)]
mod testing;
pub mod topic;
pub mod user;

pub use config::AdminConfig;

use crate::AppContext;
use crate::web::Routes;
use std::sync::Arc;

/// 관리자 API 전체를 등록한다.
pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let deps = support::AdminDeps {
        pool: ctx.pool.clone(),
        clock: Arc::clone(&ctx.clock),
        logger: Arc::clone(&ctx.logger),
        config: Arc::new(ctx.admin.clone()),
        image_urls: ctx.post.image_urls(),
    };
    register_with(routes, &deps)
}

/// 의존성을 직접 받아 등록한다(엔드포인트 테스트가 쓴다).
pub fn register_with(routes: Routes, deps: &support::AdminDeps) -> Routes {
    let deps = deps.clone();
    let routes = auth::api::register(routes, &deps);
    let routes = audit::api::register(routes, &deps);
    let routes = user::api::register(routes, &deps);
    let routes = post::api::register(routes, &deps);
    let routes = topic::register(routes, &deps);
    feedback::register(routes, &deps)
}
