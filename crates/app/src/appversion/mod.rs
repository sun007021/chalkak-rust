//! 앱 최소 지원 버전(원본 `appversion` 패키지): 플랫폼별 설정 도메인과 공개 조회 API.
//!
//! 관리자 조회·저장은 [`crate::admin::app_version`]이 이 모듈의 도메인과 저장소를 쓴다.

pub mod api;
#[cfg(test)]
mod db_tests;
pub mod domain;
pub mod migration_validator;
pub mod repository;
pub mod service;
