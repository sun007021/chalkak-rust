//! 좋아요 집계 값 객체. 원본 `PostLikes`.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

/// 게시물별 좋아요 수와 조회자가 좋아요한 게시물 집합. 집계에 없는 게시물은 `0`과 `false`다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostLikes {
    counts: HashMap<Uuid, i64>,
    liked_post_ids: HashSet<Uuid>,
}

impl PostLikes {
    pub fn new(counts: HashMap<Uuid, i64>, liked_post_ids: HashSet<Uuid>) -> PostLikes {
        PostLikes {
            counts,
            liked_post_ids,
        }
    }

    /// `createEmpty`
    pub fn create_empty() -> PostLikes {
        PostLikes::default()
    }

    pub fn is_liked(&self, post_id: Uuid) -> bool {
        self.liked_post_ids.contains(&post_id)
    }

    pub fn like_count(&self, post_id: Uuid) -> i64 {
        self.counts.get(&post_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PostLikesTest#getLikeCount_knownAndUnknownPost_returnsCountAndViewerState
    #[test]
    fn 게시물별_좋아요를_조회하고_없는_집계는_0과_false로_반환한다() {
        let post_id = Uuid::new_v4();
        let unknown = Uuid::new_v4();
        let likes = PostLikes::new(HashMap::from([(post_id, 2)]), HashSet::from([post_id]));
        assert_eq!(likes.like_count(post_id), 2);
        assert!(likes.is_liked(post_id));
        assert_eq!(likes.like_count(unknown), 0);
        assert!(!likes.is_liked(unknown));
    }

    // PostLikesTest#createEmpty_anyPost_returnsZeroAndFalse
    #[test]
    fn 빈_좋아요_집계는_모든_게시물에_0과_false를_반환한다() {
        let likes = PostLikes::create_empty();
        let post_id = Uuid::new_v4();
        assert_eq!(likes.like_count(post_id), 0);
        assert!(!likes.is_liked(post_id));
    }
}
