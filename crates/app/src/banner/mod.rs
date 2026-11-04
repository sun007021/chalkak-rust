//! 배너 도메인. 원본 `com.chalkak.backend.banner` 패키지.
//!
//! 홈 화면 배너 공개 목록(`GET /api/v1/banners`). 등록·삭제는 관리자 API([`crate::admin::banner`])가 맡는다.

pub mod api;
pub mod domain;
pub mod repository;
pub mod service;
pub mod uri;
