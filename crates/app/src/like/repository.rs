//! 좋아요 저장소. 원본 `PostLikeRepositoryImpl`·`PostLikeJpaRepository`.

use std::collections::{HashMap, HashSet};

use chalkak_db::DbError;
use postgres::GenericClient;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default)]
pub struct PostLikeRepository;

impl PostLikeRepository {
    /// `createIfAbsent`: 이미 있으면 아무것도 하지 않는다. `created_at`은 DB 기본값이다.
    pub fn create_if_absent(
        &self,
        db: &mut impl GenericClient,
        post_id: Uuid,
        user_id: Uuid,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "INSERT INTO post_likes (post_id, user_id) VALUES ($1, $2) \
             ON CONFLICT (post_id, user_id) DO NOTHING",
            &[&post_id, &user_id],
        )?)
    }

    pub fn delete_by_post_id_and_user_id(
        &self,
        db: &mut impl GenericClient,
        post_id: Uuid,
        user_id: Uuid,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM post_likes WHERE post_id = $1 AND user_id = $2",
            &[&post_id, &user_id],
        )?)
    }

    pub fn delete_by_post_id(
        &self,
        db: &mut impl GenericClient,
        post_id: Uuid,
    ) -> Result<u64, DbError> {
        Ok(db.execute("DELETE FROM post_likes WHERE post_id = $1", &[&post_id])?)
    }

    pub fn count_by_post_id(
        &self,
        db: &mut impl GenericClient,
        post_id: Uuid,
    ) -> Result<i64, DbError> {
        let row = db.query_one(
            "SELECT COUNT(*) AS count FROM post_likes WHERE post_id = $1",
            &[&post_id],
        )?;
        Ok(row.try_get("count")?)
    }

    pub fn exists_by_post_id_and_user_id(
        &self,
        db: &mut impl GenericClient,
        post_id: Uuid,
        user_id: Uuid,
    ) -> Result<bool, DbError> {
        Ok(db
            .query_opt(
                "SELECT 1 FROM post_likes WHERE post_id = $1 AND user_id = $2 LIMIT 1",
                &[&post_id, &user_id],
            )?
            .is_some())
    }

    /// `countByPostIds`: 좋아요가 하나도 없는 게시물은 결과에 없다.
    pub fn count_by_post_ids(
        &self,
        db: &mut impl GenericClient,
        post_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, i64>, DbError> {
        let rows = db.query(
            "SELECT post_id, COUNT(*) AS count FROM post_likes WHERE post_id = ANY($1) \
             GROUP BY post_id",
            &[&post_ids],
        )?;
        rows.iter()
            .map(|row| Ok((row.try_get("post_id")?, row.try_get("count")?)))
            .collect()
    }

    pub fn find_liked_post_ids(
        &self,
        db: &mut impl GenericClient,
        post_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<HashSet<Uuid>, DbError> {
        let rows = db.query(
            "SELECT post_id FROM post_likes WHERE post_id = ANY($1) AND user_id = $2",
            &[&post_ids, &user_id],
        )?;
        rows.iter()
            .map(|row| row.try_get("post_id"))
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }
}
