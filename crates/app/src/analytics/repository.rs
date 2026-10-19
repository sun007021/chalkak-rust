//! 사용자 지표 집계 쿼리(원본 `UserMetricsRepositoryImpl`).
//!
//! 여러 테이블을 가로지르는 집계라 엔티티에 묶지 않고 SQL로 센다. 제외할 회원 ID는 `uuid[]` 파라미터 하나로 넘긴다.
//! `NOT IN ()`은 목록이 비면 문법 오류라, 빈 배열에도 참이 되는 `<> ALL(배열)`을 쓴다.

use chalkak_db::{DbError, Pool};
use jiff::Timestamp;
use uuid::Uuid;

use super::service::UserMetricsRepository;
use crate::web::ApiError;

const COUNT_POST_AUTHORS: &str = "\
SELECT COUNT(DISTINCT p.user_id)
FROM posts p
WHERE p.created_at >= $1
  AND p.created_at < $2
  AND p.user_id <> ALL (CAST($3 AS uuid[]))
";

const COUNT_POSTS: &str = "\
SELECT COUNT(*)
FROM posts p
WHERE p.created_at >= $1
  AND p.created_at < $2
  AND p.user_id <> ALL (CAST($3 AS uuid[]))
";

const COUNT_RETURNING_POST_AUTHORS: &str = "\
SELECT COUNT(DISTINCT c.user_id)
FROM posts c
WHERE c.created_at >= $1
  AND c.created_at < $2
  AND c.user_id <> ALL (CAST($5 AS uuid[]))
  AND EXISTS (
    SELECT 1
    FROM posts r
    WHERE r.user_id = c.user_id
      AND r.created_at >= $3
      AND r.created_at < $4
  )
";

/// 가입 때 사인 등록이 끝나야 하므로(`signature_original_storage_key`는 NOT NULL) 탈퇴하지 않은 ACTIVE 회원은 모두
/// 사인을 등록한 회원이다.
const COUNT_ELIGIBLE_USERS: &str = "\
SELECT COUNT(*)
FROM users u
WHERE u.deleted_at IS NULL
  AND u.status = 'ACTIVE'
  AND u.id <> ALL (CAST($1 AS uuid[]))
";

const COUNT_LIKES_ON_LIVE_POSTS: &str = "\
SELECT COUNT(*)
FROM post_likes pl
JOIN posts p ON p.id = pl.post_id
WHERE p.deleted_at IS NULL
  AND pl.user_id <> ALL (CAST($1 AS uuid[]))
";

/// 풀에서 연결을 빌려 쿼리마다 따로 센다(원본 `NamedParameterJdbcTemplate`, 트랜잭션 없음).
pub struct PgUserMetricsRepository {
    pool: Pool,
    excluded_user_ids: Vec<Uuid>,
}

impl PgUserMetricsRepository {
    pub fn new(pool: Pool, excluded_user_ids: Vec<Uuid>) -> Self {
        PgUserMetricsRepository {
            pool,
            excluded_user_ids,
        }
    }

    fn count(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<i64, ApiError> {
        let mut conn = self.pool.get()?;
        let row = conn.query_one(sql, params).map_err(DbError::from)?;
        Ok(row.try_get::<_, i64>(0).map_err(DbError::from)?)
    }
}

impl UserMetricsRepository for PgUserMetricsRepository {
    fn count_post_authors(&self, from: Timestamp, to: Timestamp) -> Result<i64, ApiError> {
        self.count(COUNT_POST_AUTHORS, &[&from, &to, &self.excluded_user_ids])
    }

    fn count_posts(&self, from: Timestamp, to: Timestamp) -> Result<i64, ApiError> {
        self.count(COUNT_POSTS, &[&from, &to, &self.excluded_user_ids])
    }

    fn count_returning_post_authors(
        &self,
        cohort_from: Timestamp,
        cohort_to: Timestamp,
        return_from: Timestamp,
        return_to: Timestamp,
    ) -> Result<i64, ApiError> {
        self.count(
            COUNT_RETURNING_POST_AUTHORS,
            &[
                &cohort_from,
                &cohort_to,
                &return_from,
                &return_to,
                &self.excluded_user_ids,
            ],
        )
    }

    fn count_eligible_users(&self) -> Result<i64, ApiError> {
        self.count(COUNT_ELIGIBLE_USERS, &[&self.excluded_user_ids])
    }

    fn count_likes_on_live_posts(&self) -> Result<i64, ApiError> {
        self.count(COUNT_LIKES_ON_LIVE_POSTS, &[&self.excluded_user_ids])
    }
}
