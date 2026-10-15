//! 원본 `PostQueryService`: 게시물 단건·목록과 본인 게시물 조회. 상태를 바꾸지 않고 잠금도 쓰지 않는다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Db;
use chalkak_db::Pool;
use jiff::Timestamp;
use jiff::civil::Date;
use uuid::Uuid;

use super::domain::ModerationStatus;
use super::policy::PostProcessingPolicy;
use super::repository::{ListOrder, PostListRow, PostRepository, PostSlice};
use super::support::require_active_user;
use crate::like::repository::PostLikeRepository;
use crate::photo::url::CloudFrontUrlProvider;
use crate::topic::domain::kst_date;
use crate::topic::repository::TopicRepository;
use crate::web::ApiError;
use crate::web::binding::LocalDate;

/// 정렬 방식(`PostSort`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostSort {
    Recent,
    Popular,
    Random,
}

impl PostSort {
    /// `PostSort.from`: 소문자 이름만 받는다.
    pub fn from_name(value: &str) -> Result<PostSort, AppError> {
        match value {
            "recent" => Ok(PostSort::Recent),
            "popular" => Ok(PostSort::Popular),
            "random" => Ok(PostSort::Random),
            _ => Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "조회 조건이 올바르지 않습니다.",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicBrief {
    pub id: Uuid,
    pub title: String,
    pub topic_date: Date,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostDetail {
    pub id: Uuid,
    pub topic: TopicBrief,
    pub original_image_url: Option<String>,
    pub thumbnail_image_url: Option<String>,
    pub signature_original_image_url: Option<String>,
    pub title: Option<String>,
    pub like_count: i64,
    pub is_liked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodayPostStatus {
    pub topic_date: Date,
    pub is_posted: bool,
    pub post_id: Option<Uuid>,
    pub moderation_status: Option<ModerationStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarPost {
    pub topic_date: Date,
    pub post_id: Uuid,
    pub thumbnail_image_url: Option<String>,
    pub status: ModerationStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostCalendarResult {
    pub year: i32,
    pub month: i32,
    pub posts: Vec<CalendarPost>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostSummary {
    pub id: Uuid,
    pub original_image_url: Option<String>,
    pub thumbnail_image_url: Option<String>,
    pub signature_original_image_url: Option<String>,
    pub signature_thumbnail_image_url: Option<String>,
    pub title: Option<String>,
    pub submitted_at: Timestamp,
    pub like_count: i64,
    pub is_liked: bool,
    pub is_mine: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostListResult {
    pub current_page: i32,
    pub page_size: i32,
    pub has_next: bool,
    pub random_seed: Option<String>,
    pub posts: Vec<PostSummary>,
}

/// `RandomSeedGenerator`: 4바이트 난수를 소문자 16진수 8자리로.
pub fn generate_random_seed() -> String {
    let bytes = Uuid::new_v4();
    bytes.as_bytes()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone)]
pub struct PostQueryService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    urls: CloudFrontUrlProvider,
    policy: PostProcessingPolicy,
    posts: PostRepository,
    topics: TopicRepository,
    likes: PostLikeRepository,
}

fn not_found(message: &str) -> ApiError {
    ApiError::App(AppError::not_found(ErrorCode::BUSINESS_ERROR, message))
}

fn business(message: &str) -> ApiError {
    ApiError::App(AppError::business(ErrorCode::BUSINESS_ERROR, message))
}

impl PostQueryService {
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        urls: CloudFrontUrlProvider,
        policy: PostProcessingPolicy,
    ) -> Self {
        PostQueryService {
            pool,
            clock,
            urls,
            policy,
            posts: PostRepository::default(),
            topics: TopicRepository,
            likes: PostLikeRepository,
        }
    }

    fn url(&self, storage_key: Option<&str>) -> Result<Option<String>, ApiError> {
        self.urls
            .get_url(storage_key)
            .map_err(|e| ApiError::Internal(e.to_string()))
    }

    /// `getPost`
    pub fn get_post(&self, post_id: Uuid, user_id: Uuid) -> Result<PostDetail, ApiError> {
        let mut conn = self.pool.get()?;
        require_active_user(&mut conn, user_id)?;
        let post = self
            .posts
            .find_visible_by_id(&mut conn, post_id, false)?
            .ok_or_else(|| not_found("게시물을 찾을 수 없습니다."))?;
        let like_count = self.likes.count_by_post_id(&mut conn, post_id)?;
        let is_liked = self
            .likes
            .exists_by_post_id_and_user_id(&mut conn, post_id, user_id)?;
        Ok(PostDetail {
            id: post.id,
            topic: TopicBrief {
                id: post.topic_id,
                title: post.topic_title,
                topic_date: post.topic_date,
            },
            original_image_url: self.url(Some(&post.original_storage_key))?,
            thumbnail_image_url: self.url(post.thumbnail_storage_key.as_deref())?,
            signature_original_image_url: self.url(Some(&post.signature_original_storage_key))?,
            title: post.title,
            like_count,
            is_liked,
        })
    }

    /// `getMyTodayPostStatus`: 지금 참여할 수 있는 주제에 새 게시물을 쓸 수 있는지. 주제는 날짜가 아니라 요청 시각의
    /// 참여 기간으로 고른다.
    pub fn get_my_today_post_status(&self, user_id: Uuid) -> Result<TodayPostStatus, ApiError> {
        let mut conn = self.pool.get()?;
        require_active_user(&mut conn, user_id)?;
        let now = self.clock.now();
        let topic = self
            .topics
            .find_active_open_at(&mut conn, now)?
            .ok_or_else(|| not_found("참여할 수 있는 주제가 없습니다."))?;
        let topic_date = topic.topic_date;
        let topic_id = topic.id.expect("저장된 주제에는 식별자가 있다");
        let post = self
            .posts
            .find_active_by_author_and_topic(&mut conn, user_id, topic_id, false)?;
        // 이미지 처리 대기 시간을 넘긴 게시물은 생성 흐름이 거절 처리하고 재작성을 열어 주므로 여기서도 막지 않는다.
        let blocking = post.filter(|post| {
            !post.is_validating()
                || !post
                    .created_at
                    .is_some_and(|created| self.policy.is_processing_timed_out(created, now))
        });
        Ok(match blocking {
            Some(post) => TodayPostStatus {
                topic_date,
                is_posted: true,
                post_id: post.id,
                moderation_status: Some(post.moderation_status),
            },
            None => TodayPostStatus {
                topic_date,
                is_posted: false,
                post_id: None,
                moderation_status: None,
            },
        })
    }

    /// `getMyPostCalendar`
    pub fn get_my_post_calendar(
        &self,
        user_id: Uuid,
        year: i32,
        month: i32,
    ) -> Result<PostCalendarResult, ApiError> {
        let mut conn = self.pool.get()?;
        require_active_user(&mut conn, user_id)?;
        let first = Date::new(
            i16::try_from(year).map_err(|_| ApiError::Internal("year".to_owned()))?,
            i8::try_from(month).map_err(|_| ApiError::Internal("month".to_owned()))?,
            1,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
        let last = first.last_of_month();
        let rows = self
            .posts
            .find_calendar_posts(&mut conn, user_id, first, last)?;
        let posts = rows
            .into_iter()
            .map(|row| {
                Ok(CalendarPost {
                    topic_date: row.topic_date,
                    post_id: row.post_id,
                    thumbnail_image_url: self.url(row.thumbnail_storage_key.as_deref())?,
                    status: row.status,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        Ok(PostCalendarResult { year, month, posts })
    }

    /// `getMyPostCalendarMonths`
    pub fn get_my_post_calendar_months(&self, user_id: Uuid) -> Result<Vec<(i32, i32)>, ApiError> {
        let mut conn = self.pool.get()?;
        require_active_user(&mut conn, user_id)?;
        Ok(self.posts.find_calendar_months(&mut conn, user_id)?)
    }

    /// `getPosts`: 검사 순서는 원본과 같다. 시드 조합, 미래 날짜, 로그인 회원 확인, 주제 조회.
    pub fn get_posts(
        &self,
        topic_date: LocalDate,
        sort: PostSort,
        random_seed: Option<&str>,
        page: i32,
        page_size: i32,
        user_id: Option<Uuid>,
    ) -> Result<PostListResult, ApiError> {
        validate_random_seed_combination(sort, random_seed, page)?;
        let date = self.validate_topic_date(topic_date)?;
        let mut conn = self.pool.get()?;
        if let Some(user_id) = user_id {
            require_active_user(&mut conn, user_id)?;
        }
        let topic = self
            .topics
            .find_active_by_topic_date(&mut conn, date)?
            .ok_or_else(|| not_found("해당 날짜의 주제를 찾을 수 없습니다."))?;
        let topic_id = topic.id.expect("저장된 주제에는 식별자가 있다");

        let effective_seed = match sort {
            PostSort::Random => Some(
                random_seed
                    .map(str::to_owned)
                    .unwrap_or_else(generate_random_seed),
            ),
            _ => None,
        };
        let order = match (&sort, &effective_seed) {
            (PostSort::Random, Some(seed)) => ListOrder::Random(seed),
            (PostSort::Popular, _) => ListOrder::Popular,
            _ => ListOrder::Recent,
        };
        // Spring Data는 오프셋을 int로 줄여 쓴다. 넘치면 요청이 실패한다.
        let offset = i64::from(page - 1) * i64::from(page_size);
        if offset > i64::from(i32::MAX) {
            return Err(ApiError::Internal(
                "Page offset exceeds Integer.MAX_VALUE".to_owned(),
            ));
        }
        let slice = self.posts.find_visible_slice(
            &mut conn,
            topic_id,
            order,
            i64::from(page - 1),
            i64::from(page_size),
        )?;
        self.create_list_result(&mut conn, slice, page, page_size, effective_seed, user_id)
    }

    fn create_list_result(
        &self,
        db: &mut impl Db,
        slice: PostSlice,
        page: i32,
        page_size: i32,
        random_seed: Option<String>,
        user_id: Option<Uuid>,
    ) -> Result<PostListResult, ApiError> {
        let post_ids: Vec<Uuid> = slice.posts.iter().map(|p| p.id).collect();
        let (like_counts, liked) = if post_ids.is_empty() {
            (Default::default(), Default::default())
        } else {
            let counts = self.likes.count_by_post_ids(db, &post_ids)?;
            let liked = match user_id {
                Some(user_id) => self.likes.find_liked_post_ids(db, &post_ids, user_id)?,
                None => Default::default(),
            };
            (counts, liked)
        };
        let posts = slice
            .posts
            .iter()
            .map(|post| self.summary(post, &like_counts, &liked, user_id))
            .collect::<Result<Vec<_>, ApiError>>()?;
        Ok(PostListResult {
            current_page: page,
            page_size,
            has_next: slice.has_next,
            random_seed,
            posts,
        })
    }

    fn summary(
        &self,
        post: &PostListRow,
        like_counts: &std::collections::HashMap<Uuid, i64>,
        liked: &std::collections::HashSet<Uuid>,
        viewer: Option<Uuid>,
    ) -> Result<PostSummary, ApiError> {
        Ok(PostSummary {
            id: post.id,
            original_image_url: self.url(Some(&post.original_storage_key))?,
            thumbnail_image_url: self.url(post.thumbnail_storage_key.as_deref())?,
            signature_original_image_url: self.url(Some(&post.signature_original_storage_key))?,
            signature_thumbnail_image_url: self
                .url(post.signature_thumbnail_storage_key.as_deref())?,
            title: post.title.clone(),
            submitted_at: post.created_at,
            like_count: like_counts.get(&post.id).copied().unwrap_or(0),
            is_liked: liked.contains(&post.id),
            is_mine: viewer == Some(post.author_id),
        })
    }

    /// `validateTopicDate`: 한국 날짜 기준 미래는 조회할 수 없다. 통과한 날짜를 질의에 쓸 수 있는 형태로 돌려준다.
    fn validate_topic_date(&self, topic_date: LocalDate) -> Result<Date, ApiError> {
        const POSTGRES_MIN_YEAR: i16 = -4712;
        let future = || business("미래 날짜의 게시물은 조회할 수 없습니다.");
        let date = match topic_date {
            LocalDate::AfterMax => return Err(future()),
            // 원본은 JDBC 파라미터 변환 단계에서 DB 오류가 나 500으로 끝난다.
            LocalDate::BeforeMin => {
                return Err(ApiError::Internal(
                    "date out of range for PostgreSQL".to_owned(),
                ));
            }
            LocalDate::Valid(date) => date,
        };
        if date > kst_date(self.clock.now()) {
            return Err(future());
        }
        if date.year() < POSTGRES_MIN_YEAR {
            return Err(ApiError::Internal(
                "date out of range for PostgreSQL".to_owned(),
            ));
        }
        Ok(date)
    }
}

/// `validateRandomSeedCombination`
fn validate_random_seed_combination(
    sort: PostSort,
    random_seed: Option<&str>,
    page: i32,
) -> Result<(), ApiError> {
    let seed_without_random_sort = sort != PostSort::Random && random_seed.is_some();
    let seed_missing_after_first_page =
        sort == PostSort::Random && page > 1 && random_seed.is_none();
    if seed_without_random_sort || seed_missing_after_first_page {
        return Err(business("조회 조건이 올바르지 않습니다."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // RandomSeedGeneratorTest
    #[test]
    fn generate_random_seed_returns_eight_character_hex_string() {
        let seed = generate_random_seed();
        assert_eq!(seed.len(), 8);
        assert!(
            seed.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(generate_random_seed(), generate_random_seed());
    }

    #[test]
    fn 정렬_이름은_소문자만_받는다() {
        assert_eq!(PostSort::from_name("recent").unwrap(), PostSort::Recent);
        assert_eq!(PostSort::from_name("popular").unwrap(), PostSort::Popular);
        assert_eq!(PostSort::from_name("random").unwrap(), PostSort::Random);
        for bad in ["RECENT", "Recent", "", "latest", " recent"] {
            assert_eq!(
                PostSort::from_name(bad).unwrap_err().message,
                "조회 조건이 올바르지 않습니다."
            );
        }
    }

    // PostQueryService.validateRandomSeedCombination
    #[test]
    fn 시드_조합은_랜덤_정렬의_첫_쪽_밖에서만_필수다() {
        use PostSort::*;
        assert!(validate_random_seed_combination(Random, None, 1).is_ok());
        assert!(validate_random_seed_combination(Random, Some("abc"), 1).is_ok());
        assert!(validate_random_seed_combination(Random, Some("abc"), 2).is_ok());
        assert!(validate_random_seed_combination(Random, None, 2).is_err());
        assert!(validate_random_seed_combination(Recent, Some("abc"), 1).is_err());
        assert!(validate_random_seed_combination(Popular, Some("abc"), 3).is_err());
        assert!(validate_random_seed_combination(Recent, None, 5).is_ok());
    }
}
