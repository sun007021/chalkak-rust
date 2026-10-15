//! 게시물 저장소. 원본 `PostRepositoryImpl`과 `PostJpaRepository`의 JPQL을 손으로 쓴 SQL로 옮겼다.
//!
//! 잠금은 Hibernate PostgreSQL 방언의 번역을 따른다: `PESSIMISTIC_WRITE` -> `FOR NO KEY UPDATE`,
//! `PESSIMISTIC_READ` -> `FOR SHARE`. 사진을 함께 불러오는(`JOIN FETCH`) 잠금 조회도
//! 게시물 행만 잠근다(`OF p`, 원본 문장 로그의 `for share of p1_0`). 사진 갱신은 항상 게시물 잠금을 잡은 뒤에 한다.
//! 연결(또는 트랜잭션)은 호출 측이 넘긴다. 원본의 `@Transactional` 경계가 서비스에 있듯 여기서는 트랜잭션을 열지 않는다.

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::Timestamp;
use jiff::civil::Date;
use postgres::Row;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::domain::{ModerationStatus, Post, PostTitle};
use crate::photo::domain::Photo;
use crate::photo::repository::PhotoRepository;
use crate::topic::domain::ParticipationPeriod;
use crate::web::ApiError;

/// 중복 생성으로 보는 제약 이름(`DUPLICATE_CONSTRAINT_NAMES`).
const DUPLICATE_CONSTRAINT_NAMES: [&str; 3] = [
    "ux_posts_user_topic_active",
    "ux_posts_photo_id",
    "ux_photos_original_storage_key",
];

/// 게시물과 사진을 함께 읽는 열. `ph_`는 사진 쪽이다.
const POST_COLUMNS: &str = "p.id, p.user_id, p.topic_id, p.photo_id, p.post_image_upload_id, p.title, \
     p.moderation_status::text AS moderation_status, p.moderated_at, p.created_at, p.updated_at, \
     p.deleted_at, ph.original_storage_key AS ph_original_storage_key, \
     ph.thumbnail_storage_key AS ph_thumbnail_storage_key, ph.metadata::text AS ph_metadata, \
     ph.deleted_at AS ph_deleted_at";

const POST_FROM: &str = "FROM posts p JOIN photos ph ON ph.id = p.photo_id";

/// 읽은 행 -> 도메인 게시물. 저장된 제목은 이미 검증을 통과한 값이다.
fn post_from_row(row: &Row) -> Result<Post, DbError> {
    let status_text: String = row.try_get("moderation_status")?;
    let moderation_status = ModerationStatus::parse(&status_text)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 moderation_status: {status_text}")))?;
    let title: Option<String> = row.try_get("title")?;
    let title = match title {
        Some(t) => PostTitle::from(Some(&t)).map_err(|e| {
            DbError::Config(format!("저장된 제목이 올바르지 않습니다: {}", e.message))
        })?,
        None => None,
    };
    let metadata_text: String = row.try_get("ph_metadata")?;
    let metadata = parse_metadata(&metadata_text)?;
    Ok(Post {
        id: Some(row.try_get("id")?),
        author_id: row.try_get("user_id")?,
        topic_id: row.try_get("topic_id")?,
        photo: Photo {
            id: Some(row.try_get("photo_id")?),
            original_storage_key: row.try_get("ph_original_storage_key")?,
            thumbnail_storage_key: row.try_get("ph_thumbnail_storage_key")?,
            metadata,
            deleted_at: row.try_get("ph_deleted_at")?,
        },
        post_image_upload_id: row.try_get("post_image_upload_id")?,
        title,
        moderation_status,
        moderated_at: row.try_get("moderated_at")?,
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
        deleted_at: row.try_get("deleted_at")?,
    })
}

pub(crate) fn parse_metadata(text: &str) -> Result<Map<String, Value>, DbError> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(DbError::Config("jsonb 열이 객체가 아닙니다".to_owned())),
    }
}

/// 단건 상세와 좋아요 검증이 쓰는 조회 결과: 게시물, 주제, 사진, 작성자 서명.
#[derive(Clone, Debug, PartialEq)]
pub struct VisiblePost {
    pub id: Uuid,
    pub title: Option<String>,
    pub topic_id: Uuid,
    pub topic_title: String,
    pub topic_date: Date,
    pub original_storage_key: String,
    pub thumbnail_storage_key: Option<String>,
    pub signature_original_storage_key: String,
}

/// 목록 한 줄.
#[derive(Clone, Debug, PartialEq)]
pub struct PostListRow {
    pub id: Uuid,
    pub author_id: Uuid,
    pub original_storage_key: String,
    pub thumbnail_storage_key: Option<String>,
    pub signature_original_storage_key: String,
    pub signature_thumbnail_storage_key: Option<String>,
    pub title: Option<String>,
    pub created_at: Timestamp,
}

/// `PostSlice`
#[derive(Clone, Debug, PartialEq)]
pub struct PostSlice {
    pub posts: Vec<PostListRow>,
    pub has_next: bool,
}

/// 달력 한 줄(`findCalendarPostsByAuthorIdAndTopicDateBetween`).
#[derive(Clone, Debug, PartialEq)]
pub struct CalendarPostRow {
    pub topic_date: Date,
    pub post_id: Uuid,
    pub thumbnail_storage_key: Option<String>,
    pub status: ModerationStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListOrder<'a> {
    Recent,
    Popular,
    Random(&'a str),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PostRepository {
    photos: PhotoRepository,
}

const VISIBLE_CONDITIONS: &str = "p.moderation_status = 'APPROVED' AND p.deleted_at IS NULL \
     AND t.deleted_at IS NULL AND ph.deleted_at IS NULL AND u.deleted_at IS NULL";

impl PostRepository {
    /// `save`: 사진과 게시물을 INSERT한다. 중복 제약 위반은 업무 오류(`이미 사용된 게시물 생성 정보입니다.`)로 바꾼다.
    pub fn save(&self, db: &mut impl Db, post: &mut Post, now: Timestamp) -> Result<(), ApiError> {
        match self.insert(db, post, now) {
            Ok(()) => Ok(()),
            Err(error) => {
                if is_duplicate_constraint(&error) {
                    return Err(ApiError::App(AppError::business(
                        ErrorCode::BUSINESS_ERROR,
                        "이미 사용된 게시물 생성 정보입니다.",
                    )));
                }
                Err(ApiError::from(error))
            }
        }
    }

    fn insert(&self, db: &mut impl Db, post: &mut Post, now: Timestamp) -> Result<(), DbError> {
        self.photos.insert(db, &mut post.photo, now)?;
        let title = post.title_value().map(str::to_owned);
        let status = post.moderation_status.as_str();
        let row = db.query_one(
            "INSERT INTO posts (user_id, topic_id, photo_id, post_image_upload_id, title, \
             moderation_status, moderated_at, created_at, updated_at, deleted_at) \
             VALUES ($1, $2, $3, $4, $5, $6::text::moderation_status, $7, $8, $8, $9) \
             RETURNING id",
            &[
                &post.author_id,
                &post.topic_id,
                &post.photo.id,
                &post.post_image_upload_id,
                &title,
                &status,
                &post.moderated_at,
                &now,
                &post.deleted_at,
            ],
        )?;
        post.id = Some(row.try_get("id")?);
        post.created_at = Some(now);
        post.updated_at = Some(now);
        Ok(())
    }

    /// `findActiveByAuthorIdAndTopicIdForUpdate`/`findActiveByAuthorIdAndTopicId`: 거절되지 않은 살아 있는 게시물.
    pub fn find_active_by_author_and_topic(
        &self,
        db: &mut impl Db,
        author_id: Uuid,
        topic_id: Uuid,
        for_update: bool,
    ) -> Result<Option<Post>, DbError> {
        let lock = if for_update {
            " FOR NO KEY UPDATE OF p"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {POST_COLUMNS} {POST_FROM} \
             WHERE p.user_id = $1 AND p.topic_id = $2 AND p.moderation_status <> 'REJECTED' \
             AND p.deleted_at IS NULL{lock}"
        );
        db.query_opt(&sql, &[&author_id, &topic_id])?
            .as_ref()
            .map(post_from_row)
            .transpose()
    }

    /// `findActiveByIdForUpdate`: 삭제되지 않은 게시물을 잠그고 읽는다.
    pub fn find_active_by_id_for_update(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
    ) -> Result<Option<Post>, DbError> {
        let sql = format!(
            "SELECT {POST_COLUMNS} {POST_FROM} WHERE p.id = $1 AND p.deleted_at IS NULL \
             FOR NO KEY UPDATE OF p"
        );
        db.query_opt(&sql, &[&post_id])?
            .as_ref()
            .map(post_from_row)
            .transpose()
    }

    /// `findByIdForUpdate`: 삭제된 게시물도 읽는다(삭제는 멱등이다). 게시물 행만 잠근다.
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
    ) -> Result<Option<Post>, DbError> {
        let sql =
            format!("SELECT {POST_COLUMNS} {POST_FROM} WHERE p.id = $1 FOR NO KEY UPDATE OF p");
        db.query_opt(&sql, &[&post_id])?
            .as_ref()
            .map(post_from_row)
            .transpose()
    }

    /// `findValidatingByPostImageUploadIdForUpdate`: 이미지 처리를 기다리는 게시물.
    pub fn find_validating_by_upload_id_for_update(
        &self,
        db: &mut impl Db,
        upload_id: Uuid,
    ) -> Result<Option<Post>, DbError> {
        let sql = format!(
            "SELECT {POST_COLUMNS} {POST_FROM} \
             WHERE p.post_image_upload_id = $1 AND p.moderation_status = 'VALIDATING' \
             AND p.deleted_at IS NULL AND ph.deleted_at IS NULL FOR NO KEY UPDATE OF p"
        );
        db.query_opt(&sql, &[&upload_id])?
            .as_ref()
            .map(post_from_row)
            .transpose()
    }

    /// 주제가 열려 있는지 판정할 참여 기간. 삭제된 주제도 읽는다(게시물이 가리키는 주제).
    pub fn find_topic_period(
        &self,
        db: &mut impl Db,
        topic_id: Uuid,
    ) -> Result<Option<ParticipationPeriod>, DbError> {
        let row = db.query_opt(
            "SELECT starts_at, ends_at FROM topics WHERE id = $1",
            &[&topic_id],
        )?;
        row.map(|r| {
            Ok(ParticipationPeriod::restore(
                r.try_get("starts_at")?,
                r.try_get("ends_at")?,
            ))
        })
        .transpose()
    }

    /// 제목 변경을 반영한다(`@UpdateTimestamp`처럼 `updated_at`도 갱신).
    pub fn update_title(
        &self,
        db: &mut impl Db,
        post: &Post,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let title = post.title_value().map(str::to_owned);
        db.execute(
            "UPDATE posts SET title = $2, updated_at = $3 WHERE id = $1",
            &[&post.id, &title, &now],
        )?;
        Ok(())
    }

    /// 검수 상태 변경(`moderation_status`, `moderated_at`)을 반영한다.
    pub fn update_moderation(
        &self,
        db: &mut impl Db,
        post: &Post,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let status = post.moderation_status.as_str();
        db.execute(
            "UPDATE posts SET moderation_status = $2::text::moderation_status, moderated_at = $3, \
             updated_at = $4 WHERE id = $1",
            &[&post.id, &status, &post.moderated_at, &now],
        )?;
        Ok(())
    }

    /// 소프트 삭제를 반영한다. 사진도 같은 시각으로 지워진다(`Post.delete*`가 `Photo.delete`를 부른다).
    pub fn update_deleted(
        &self,
        db: &mut impl Db,
        post: &Post,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE posts SET deleted_at = $2, updated_at = $3 WHERE id = $1",
            &[&post.id, &post.deleted_at, &now],
        )?;
        if let (Some(photo_id), Some(deleted_at)) = (post.photo.id, post.photo.deleted_at) {
            self.photos
                .update_deleted_at(db, photo_id, deleted_at, now)?;
        }
        Ok(())
    }

    /// 사진 처리 결과를 반영한다.
    pub fn update_photo_processing(
        &self,
        db: &mut impl Db,
        post: &Post,
        now: Timestamp,
    ) -> Result<(), DbError> {
        if let Some(photo_id) = post.photo.id {
            self.photos
                .update_processing(db, photo_id, &post.photo, now)?;
        }
        Ok(())
    }

    /// `findVisibleById`/`findVisibleByIdForShare`: 공개된(`APPROVED`) 게시물. 주제·사진·작성자도 삭제되지 않아야 한다.
    pub fn find_visible_by_id(
        &self,
        db: &mut impl Db,
        post_id: Uuid,
        for_share: bool,
    ) -> Result<Option<VisiblePost>, DbError> {
        let lock = if for_share { " FOR SHARE OF p" } else { "" };
        let sql = format!(
            "SELECT p.id, p.title, t.id AS topic_id, t.title AS topic_title, t.topic_date, \
             ph.original_storage_key, ph.thumbnail_storage_key, u.signature_original_storage_key \
             FROM posts p JOIN topics t ON t.id = p.topic_id JOIN photos ph ON ph.id = p.photo_id \
             JOIN users u ON u.id = p.user_id \
             WHERE p.id = $1 AND {VISIBLE_CONDITIONS}{lock}"
        );
        db.query_opt(&sql, &[&post_id])?
            .map(|row| {
                Ok(VisiblePost {
                    id: row.try_get("id")?,
                    title: row.try_get("title")?,
                    topic_id: row.try_get("topic_id")?,
                    topic_title: row.try_get("topic_title")?,
                    topic_date: row.try_get("topic_date")?,
                    original_storage_key: row.try_get("original_storage_key")?,
                    thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
                    signature_original_storage_key: row
                        .try_get("signature_original_storage_key")?,
                })
            })
            .transpose()
    }

    /// `findCalendarPostsByAuthorIdAndTopicDateBetween`: 검수 대기·승인 게시물을 주제 날짜, 게시물 id 순으로.
    pub fn find_calendar_posts(
        &self,
        db: &mut impl Db,
        author_id: Uuid,
        start_date: Date,
        end_date: Date,
    ) -> Result<Vec<CalendarPostRow>, DbError> {
        let rows = db.query(
            "SELECT t.topic_date, p.id, ph.thumbnail_storage_key, \
             p.moderation_status::text AS moderation_status \
             FROM posts p JOIN topics t ON t.id = p.topic_id JOIN photos ph ON ph.id = p.photo_id \
             WHERE p.user_id = $1 AND t.topic_date BETWEEN $2 AND $3 \
             AND p.moderation_status IN ('PENDING', 'APPROVED') \
             AND p.deleted_at IS NULL AND t.deleted_at IS NULL AND ph.deleted_at IS NULL \
             ORDER BY t.topic_date ASC, p.id ASC",
            &[&author_id, &start_date, &end_date],
        )?;
        rows.iter()
            .map(|row| {
                let status: String = row.try_get("moderation_status")?;
                Ok(CalendarPostRow {
                    topic_date: row.try_get("topic_date")?,
                    post_id: row.try_get("id")?,
                    thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
                    status: ModerationStatus::parse(&status).ok_or_else(|| {
                        DbError::Config(format!("알 수 없는 moderation_status: {status}"))
                    })?,
                })
            })
            .collect()
    }

    /// `findCalendarMonthsByAuthorId`: 게시물이 있는 연월을 최근 순으로.
    pub fn find_calendar_months(
        &self,
        db: &mut impl Db,
        author_id: Uuid,
    ) -> Result<Vec<(i32, i32)>, DbError> {
        let rows = db.query(
            "SELECT DISTINCT EXTRACT(YEAR FROM t.topic_date)::int4 AS year, \
             EXTRACT(MONTH FROM t.topic_date)::int4 AS month \
             FROM posts p JOIN topics t ON t.id = p.topic_id JOIN photos ph ON ph.id = p.photo_id \
             JOIN users u ON u.id = p.user_id \
             WHERE u.id = $1 AND p.moderation_status IN ('PENDING', 'APPROVED') \
             AND p.deleted_at IS NULL AND t.deleted_at IS NULL AND ph.deleted_at IS NULL \
             AND u.deleted_at IS NULL \
             ORDER BY year DESC, month DESC",
            &[&author_id],
        )?;
        rows.iter()
            .map(|row| Ok((row.try_get("year")?, row.try_get("month")?)))
            .collect()
    }

    /// 공개된 게시물 목록의 한 쪽. `page`는 0부터, 다음 쪽 유무를 알기 위해 한 건 더 읽는다(Spring `Slice`).
    pub fn find_visible_slice(
        &self,
        db: &mut impl Db,
        topic_id: Uuid,
        order: ListOrder<'_>,
        page: i64,
        page_size: i64,
    ) -> Result<PostSlice, DbError> {
        let order_by = match order {
            ListOrder::Recent => "p.created_at DESC, p.id DESC",
            ListOrder::Popular => {
                "(SELECT COUNT(*) FROM post_likes l WHERE l.post_id = p.id) DESC, \
                 p.created_at DESC, p.id ASC"
            }
            ListOrder::Random(_) => "md5(concat(p.id::text, $4::text)), p.id ASC",
        };
        let sql = format!(
            "SELECT p.id, p.user_id, ph.original_storage_key, ph.thumbnail_storage_key, \
             u.signature_original_storage_key, u.signature_thumbnail_storage_key, p.title, \
             p.created_at \
             FROM posts p JOIN topics t ON t.id = p.topic_id JOIN photos ph ON ph.id = p.photo_id \
             JOIN users u ON u.id = p.user_id \
             WHERE t.id = $1 AND {VISIBLE_CONDITIONS} \
             ORDER BY {order_by} LIMIT $2 OFFSET $3"
        );
        let limit = page_size + 1;
        let offset = page * page_size;
        let rows = match order {
            ListOrder::Random(seed) => db.query(&sql, &[&topic_id, &limit, &offset, &seed])?,
            _ => db.query(&sql, &[&topic_id, &limit, &offset])?,
        };
        let mut posts = rows
            .iter()
            .map(|row| {
                Ok(PostListRow {
                    id: row.try_get("id")?,
                    author_id: row.try_get("user_id")?,
                    original_storage_key: row.try_get("original_storage_key")?,
                    thumbnail_storage_key: row.try_get("thumbnail_storage_key")?,
                    signature_original_storage_key: row
                        .try_get("signature_original_storage_key")?,
                    signature_thumbnail_storage_key: row
                        .try_get("signature_thumbnail_storage_key")?,
                    title: row.try_get("title")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        let has_next = i64::try_from(posts.len()).unwrap_or(i64::MAX) > page_size;
        posts.truncate(usize::try_from(page_size).unwrap_or(usize::MAX));
        Ok(PostSlice { posts, has_next })
    }
}

fn is_duplicate_constraint(error: &DbError) -> bool {
    let DbError::Postgres(error) = error else {
        return false;
    };
    error
        .as_db_error()
        .and_then(postgres::error::DbError::constraint)
        .is_some_and(|name| DUPLICATE_CONSTRAINT_NAMES.contains(&name))
}
