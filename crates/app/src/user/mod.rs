//! 회원 도메인: 회원 저장, 사인 이미지 등록과 처리 콜백, 탈퇴.

pub mod api;
pub mod domain;
pub mod repository;
pub mod service;
pub mod storage;

use chalkak_db::Db;
use chalkak_db::DbError;
use uuid::Uuid;

/// 회원 행의 존재 여부와 탈퇴 여부(`UserRepository.findById`에서 필요한 부분).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserPresence {
    Missing,
    Withdrawn,
    Present,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UserRepository;

impl UserRepository {
    /// 탈퇴 회원도 읽어야 없는 회원과 구분할 수 있으므로 `deleted_at`을 거르지 않는다.
    pub fn presence(&self, db: &mut impl Db, user_id: Uuid) -> Result<UserPresence, DbError> {
        let row = db.query_opt(
            "SELECT deleted_at IS NOT NULL AS withdrawn FROM users WHERE id = $1",
            &[&user_id],
        )?;
        Ok(match row {
            None => UserPresence::Missing,
            Some(row) if row.try_get::<_, bool>("withdrawn")? => UserPresence::Withdrawn,
            Some(_) => UserPresence::Present,
        })
    }
}
