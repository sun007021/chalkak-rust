//! 주제 저장소. 원본 `TopicRepository` 인터페이스와 `TopicRepositoryImpl`·`TopicJpaRepository`의 JPQL을 손으로 쓴 SQL로 옮겼다.
//!
//! 모든 조회는 삭제되지 않은 주제(`deleted_at IS NULL`)만 대상으로 한다(소프트 삭제). 연결(또는 트랜잭션)은 호출 측이
//! 넘긴다: 원본의 `@Transactional` 경계가 서비스에 있듯 여기서는 트랜잭션을 열지 않는다.
//!
//! 쓰기(`save_and_flush`)는 DB 제약 위반 중 두 가지만 업무 오류로 바꾸고 나머지는 그대로 전파한다.
//! - `ux_topics_topic_date_active`(활성 주제 날짜 중복) -> `해당 날짜의 주제가 이미 존재합니다.`
//! - `ex_topics_active_participation_period`(참여 기간 겹침) -> `다른 주제의 참여 기간과 겹칩니다.`

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::DbError;
use jiff::Timestamp;
use jiff::civil::Date;
use postgres::{GenericClient, Row};
use uuid::Uuid;

use super::domain::{ParticipationPeriod, Topic};

const COLUMNS: &str =
    "id, title, topic_date, starts_at, ends_at, created_at, updated_at, deleted_at";

const TOPIC_DATE_UNIQUE_INDEX: &str = "ux_topics_topic_date_active";
const TOPIC_PERIOD_EXCLUSION_CONSTRAINT: &str = "ex_topics_active_participation_period";

/// 저장 결과. 업무 오류로 바뀐 제약 위반과 그 밖의 DB 오류를 나눈다.
#[derive(Debug)]
pub enum SaveError {
    Business(AppError),
    Db(DbError),
}

impl From<DbError> for SaveError {
    fn from(e: DbError) -> Self {
        SaveError::Db(e)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TopicRepository;

fn from_row(row: &Row) -> Result<Topic, DbError> {
    Ok(Topic {
        id: Some(row.try_get("id")?),
        title: row.try_get("title")?,
        topic_date: row.try_get("topic_date")?,
        participation_period: ParticipationPeriod::restore(
            row.try_get("starts_at")?,
            row.try_get("ends_at")?,
        ),
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
        deleted_at: row.try_get("deleted_at")?,
    })
}

impl TopicRepository {
    /// `findActiveById`
    pub fn find_active_by_id(
        &self,
        db: &mut impl GenericClient,
        topic_id: Uuid,
    ) -> Result<Option<Topic>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM topics WHERE id = $1 AND deleted_at IS NULL");
        db.query_opt(&sql, &[&topic_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findActiveByTopicDate`
    pub fn find_active_by_topic_date(
        &self,
        db: &mut impl GenericClient,
        topic_date: Date,
    ) -> Result<Option<Topic>, DbError> {
        let sql =
            format!("SELECT {COLUMNS} FROM topics WHERE topic_date = $1 AND deleted_at IS NULL");
        db.query_opt(&sql, &[&topic_date])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findActiveOpenAt`: 그 시각에 참여할 수 있는 주제. 날짜가 아니라 참여 기간으로 고른다.
    pub fn find_active_open_at(
        &self,
        db: &mut impl GenericClient,
        now: Timestamp,
    ) -> Result<Option<Topic>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM topics \
             WHERE starts_at <= $1 AND ends_at > $1 AND deleted_at IS NULL"
        );
        db.query_opt(&sql, &[&now])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findActiveByIdForUpdate`: 비관적 쓰기 락(Hibernate PostgreSQL 방언의 `for no key update`). 트랜잭션 안에서 불러야 한다.
    pub fn find_active_by_id_for_update(
        &self,
        db: &mut impl GenericClient,
        topic_id: Uuid,
    ) -> Result<Option<Topic>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM topics WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE"
        );
        db.query_opt(&sql, &[&topic_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `existsActiveByTopicDate`
    pub fn exists_active_by_topic_date(
        &self,
        db: &mut impl GenericClient,
        topic_date: Date,
    ) -> Result<bool, DbError> {
        exists(
            db,
            "SELECT 1 FROM topics WHERE topic_date = $1 AND deleted_at IS NULL LIMIT 1",
            &[&topic_date],
        )
    }

    /// `existsActiveByTopicDateExcludingId`
    pub fn exists_active_by_topic_date_excluding_id(
        &self,
        db: &mut impl GenericClient,
        topic_date: Date,
        topic_id: Uuid,
    ) -> Result<bool, DbError> {
        exists(
            db,
            "SELECT 1 FROM topics WHERE topic_date = $1 AND deleted_at IS NULL AND id <> $2 LIMIT 1",
            &[&topic_date, &topic_id],
        )
    }

    /// `existsActiveOverlappingPeriod`: 반열린 구간 `[starts, ends)`가 겹치는 활성 주제가 있는가.
    pub fn exists_active_overlapping_period(
        &self,
        db: &mut impl GenericClient,
        starts_at: Timestamp,
        ends_at: Timestamp,
    ) -> Result<bool, DbError> {
        exists(
            db,
            "SELECT 1 FROM topics \
             WHERE deleted_at IS NULL AND starts_at < $2 AND ends_at > $1 LIMIT 1",
            &[&starts_at, &ends_at],
        )
    }

    /// `existsActiveOverlappingPeriodExcludingId`
    pub fn exists_active_overlapping_period_excluding_id(
        &self,
        db: &mut impl GenericClient,
        starts_at: Timestamp,
        ends_at: Timestamp,
        topic_id: Uuid,
    ) -> Result<bool, DbError> {
        exists(
            db,
            "SELECT 1 FROM topics \
             WHERE deleted_at IS NULL AND starts_at < $2 AND ends_at > $1 AND id <> $3 LIMIT 1",
            &[&starts_at, &ends_at, &topic_id],
        )
    }

    /// `saveAndFlush`: 새 주제면 INSERT, 이미 저장된 주제면 UPDATE.
    /// `created_at`·`updated_at`은 JPA의 `@CreationTimestamp`·`@UpdateTimestamp`처럼 앱 시각(`now`)으로 채운다.
    pub fn save_and_flush(
        &self,
        db: &mut impl GenericClient,
        topic: &mut Topic,
        now: Timestamp,
    ) -> Result<(), SaveError> {
        let period = topic.participation_period;
        let result = match topic.id {
            None => db.query_one(
                "INSERT INTO topics (title, topic_date, starts_at, ends_at, created_at, updated_at, deleted_at) \
                 VALUES ($1, $2, $3, $4, $5, $5, $6) RETURNING id, created_at, updated_at",
                &[
                    &topic.title,
                    &topic.topic_date,
                    &period.starts_at(),
                    &period.ends_at(),
                    &now,
                    &topic.deleted_at,
                ],
            ),
            Some(id) => db.query_one(
                "UPDATE topics SET title = $2, topic_date = $3, starts_at = $4, ends_at = $5, \
                 updated_at = $6, deleted_at = $7 WHERE id = $1 RETURNING id, created_at, updated_at",
                &[
                    &id,
                    &topic.title,
                    &topic.topic_date,
                    &period.starts_at(),
                    &period.ends_at(),
                    &now,
                    &topic.deleted_at,
                ],
            ),
        };
        match result {
            Ok(row) => {
                topic.id = Some(row.try_get("id").map_err(DbError::from)?);
                topic.created_at = Some(row.try_get("created_at").map_err(DbError::from)?);
                topic.updated_at = Some(row.try_get("updated_at").map_err(DbError::from)?);
                Ok(())
            }
            Err(error) => Err(translate_constraint(error)),
        }
    }
}

fn exists(
    db: &mut impl GenericClient,
    sql: &str,
    params: &[&(dyn postgres::types::ToSql + Sync)],
) -> Result<bool, DbError> {
    Ok(db.query_opt(sql, params)?.is_some())
}

/// 제약 이름으로 업무 오류를 고른다. 모르는 무결성 오류는 날짜 중복으로 오인하지 않고 그대로 전파한다.
fn translate_constraint(error: postgres::Error) -> SaveError {
    let constraint = error
        .as_db_error()
        .and_then(postgres::error::DbError::constraint);
    match constraint {
        Some(TOPIC_DATE_UNIQUE_INDEX) => SaveError::Business(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            "해당 날짜의 주제가 이미 존재합니다.",
        )),
        Some(TOPIC_PERIOD_EXCLUSION_CONSTRAINT) => SaveError::Business(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            "다른 주제의 참여 기간과 겹칩니다.",
        )),
        _ => SaveError::Db(DbError::Postgres(error)),
    }
}
