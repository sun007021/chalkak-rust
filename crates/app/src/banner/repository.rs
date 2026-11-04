//! 원본 `BannerRepositoryImpl`·`BannerJpaRepository`의 질의. 연결(또는 트랜잭션)은 호출 측이 넘긴다.

use chalkak_db::{Db, DbError};
use jiff::Timestamp;
use postgres::Row;
use uuid::Uuid;

use super::domain::Banner;
use crate::admin::support::limit_offset;

/// 웹뷰 URL 중복을 막는 부분 고유 인덱스(`ux_banners_web_view_url_digest`).
const UNIQUE_WEB_VIEW_URL: &str = "ux_banners_web_view_url_digest";

const COLUMNS: &str =
    "id, title, subtitle, web_view_url, thumbnail_url, created_at, updated_at, deleted_at";

/// 목록 한 쪽(`BannerPage`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BannerPage {
    pub items: Vec<Banner>,
    pub current_page: i32,
    pub page_size: i32,
    pub has_next: bool,
}

/// `save` 실패. 웹뷰 URL 중복은 업무 오류로 바꾸고, 나머지는 DB 오류 그대로 둔다.
#[derive(Debug)]
pub enum SaveError {
    DuplicateWebViewUrl,
    Db(DbError),
}

impl From<DbError> for SaveError {
    fn from(error: DbError) -> Self {
        SaveError::Db(error)
    }
}

fn from_row(row: &Row) -> Result<Banner, DbError> {
    Ok(Banner {
        id: Some(row.try_get("id")?),
        title: row.try_get("title")?,
        subtitle: row.try_get("subtitle")?,
        web_view_url: row.try_get("web_view_url")?,
        thumbnail_url: row.try_get("thumbnail_url")?,
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
        deleted_at: row.try_get("deleted_at")?,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BannerRepository;

impl BannerRepository {
    /// `existsByWebViewUrl`: 삭제하지 않은 배너 가운데 전체 문자열이 같은 URL이 있는지.
    pub fn exists_by_web_view_url(
        &self,
        db: &mut impl Db,
        web_view_url: &str,
    ) -> Result<bool, DbError> {
        let row = db.query_one(
            "SELECT EXISTS (SELECT 1 FROM banners WHERE web_view_url = $1 AND deleted_at IS NULL)",
            &[&web_view_url],
        )?;
        Ok(row.try_get(0)?)
    }

    /// `save`(새 배너 INSERT): DB가 만든 식별자와 생성 시각을 바로 받는다.
    /// `created_at`·`updated_at`은 JPA의 `@CreationTimestamp`·`@UpdateTimestamp`처럼 앱 시각(`now`)으로 채운다.
    pub fn insert(
        &self,
        db: &mut impl Db,
        banner: &mut Banner,
        now: Timestamp,
    ) -> Result<(), SaveError> {
        let result = db.query_one(
            "INSERT INTO banners (title, subtitle, web_view_url, thumbnail_url, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $5) RETURNING id, created_at, updated_at",
            &[
                &banner.title,
                &banner.subtitle,
                &banner.web_view_url,
                &banner.thumbnail_url,
                &now,
            ],
        );
        match result {
            Ok(row) => {
                banner.id = Some(row.try_get("id").map_err(DbError::from)?);
                banner.created_at = Some(row.try_get("created_at").map_err(DbError::from)?);
                banner.updated_at = Some(row.try_get("updated_at").map_err(DbError::from)?);
                Ok(())
            }
            Err(error) => {
                let constraint = error
                    .as_db_error()
                    .and_then(postgres::error::DbError::constraint);
                if constraint == Some(UNIQUE_WEB_VIEW_URL) {
                    Err(SaveError::DuplicateWebViewUrl)
                } else {
                    Err(SaveError::Db(DbError::Postgres(error)))
                }
            }
        }
    }

    /// 삭제 시각 저장(더티 체킹의 UPDATE). `@UpdateTimestamp`처럼 `updated_at`도 앱 시각으로 갱신한다.
    pub fn save_deletion(
        &self,
        db: &mut impl Db,
        banner: &mut Banner,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let id = banner.id;
        let row = db.query_one(
            "UPDATE banners SET deleted_at = $2, updated_at = $3 WHERE id = $1 RETURNING updated_at",
            &[&id, &banner.deleted_at, &now],
        )?;
        banner.updated_at = Some(row.try_get("updated_at")?);
        Ok(())
    }

    /// `findBanners`: 삭제하지 않은 배너를 등록 최신순(생성 시각, 식별자 내림차순)으로 한 쪽 읽는다.
    pub fn find_banners(
        &self,
        db: &mut impl Db,
        page: i32,
        page_size: i32,
    ) -> Result<BannerPage, DbError> {
        let (limit, offset) = limit_offset(page, page_size);
        let sql = format!(
            "SELECT {COLUMNS} FROM banners WHERE deleted_at IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT $1 OFFSET $2"
        );
        let rows = db.query(&sql, &[&limit, &offset])?;
        let items = rows.iter().map(from_row).collect::<Result<Vec<_>, _>>()?;
        let page = crate::admin::support::AdminPage::from_overfetched(items, page, page_size);
        Ok(BannerPage {
            items: page.items,
            current_page: page.current_page,
            page_size: page.page_size,
            has_next: page.has_next,
        })
    }

    /// `findByIdForUpdate`: 삭제 여부와 관계없이 같은 행을 잠가, 재삭제 요청도 최초 삭제 결과를 읽게 한다.
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl Db,
        banner_id: Uuid,
    ) -> Result<Option<Banner>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM banners WHERE id = $1 FOR NO KEY UPDATE");
        db.query_opt(&sql, &[&banner_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }
}
