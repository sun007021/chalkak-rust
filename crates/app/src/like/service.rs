//! 원본 `PostLikeService`: 공개된 게시물에 좋아요를 누르고 취소한다. 두 동작 모두 멱등이다.

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Pool;
use uuid::Uuid;

use super::repository::PostLikeRepository;
use crate::post::repository::PostRepository;
use crate::post::support::{in_transaction, require_active_user};
use crate::web::ApiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostLikeResult {
    pub post_id: Uuid,
    pub like_count: i64,
    pub is_liked: bool,
}

#[derive(Clone)]
pub struct PostLikeService {
    pool: Pool,
    likes: PostLikeRepository,
    posts: PostRepository,
}

impl PostLikeService {
    pub fn new(pool: Pool) -> Self {
        PostLikeService {
            pool,
            likes: PostLikeRepository,
            posts: PostRepository::default(),
        }
    }

    /// `likePost`
    pub fn like_post(&self, post_id: Uuid, user_id: Uuid) -> Result<PostLikeResult, ApiError> {
        in_transaction(&self.pool, |tx| {
            require_active_user(tx, user_id)?;
            self.validate_post(tx, post_id)?;
            self.likes.create_if_absent(tx, post_id, user_id)?;
            Ok(PostLikeResult {
                post_id,
                like_count: self.likes.count_by_post_id(tx, post_id)?,
                is_liked: true,
            })
        })
    }

    /// `unlikePost`
    pub fn unlike_post(&self, post_id: Uuid, user_id: Uuid) -> Result<PostLikeResult, ApiError> {
        in_transaction(&self.pool, |tx| {
            require_active_user(tx, user_id)?;
            self.validate_post(tx, post_id)?;
            self.likes
                .delete_by_post_id_and_user_id(tx, post_id, user_id)?;
            Ok(PostLikeResult {
                post_id,
                like_count: self.likes.count_by_post_id(tx, post_id)?,
                is_liked: false,
            })
        })
    }

    /// 공개된 게시물만 좋아요를 받는다. 게시물 행을 공유 잠금으로 읽어 삭제와 겹치지 않게 한다.
    fn validate_post(&self, tx: &mut impl chalkak_db::Db, post_id: Uuid) -> Result<(), ApiError> {
        self.posts
            .find_visible_by_id(tx, post_id, true)?
            .map(|_| ())
            .ok_or_else(|| {
                ApiError::App(AppError::not_found(
                    ErrorCode::BUSINESS_ERROR,
                    "게시물을 찾을 수 없습니다.",
                ))
            })
    }
}
