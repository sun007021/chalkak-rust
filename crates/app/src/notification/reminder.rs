//! 관리자 리마인더. 원본 `Reminder`, `ReminderRepeat`, `ReminderTarget`, `ReminderRepositoryImpl`·`ReminderJpaRepository`.
//!
//! 관리자가 등록한 푸시 문구 원본이다. 회원별 알림함 행은 만들지 않고 푸시로만 보낸다. 발행은
//! [`crate::notification::reminder_push`], 관리 API는 [`crate::admin::reminder`]가 맡는다.
//! 연결(또는 트랜잭션)은 호출 측이 넘긴다.

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::{Db, DbError};
use jiff::Timestamp;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use postgres::Row;
use uuid::Uuid;

use super::domain::{is_push_expired_at, push_expires_at};
use crate::admin::binding::{code_points, has_text, java_trim};

pub const MAX_TITLE_LENGTH: usize = 50;
pub const MAX_BODY_LENGTH: usize = 500;

const TITLE_MESSAGE: &str = "리마인더 제목은 1자 이상 50자 이하여야 합니다.";
const BODY_MESSAGE: &str = "리마인더 본문은 1자 이상 500자 이하여야 합니다.";
const PAST_MESSAGE: &str = "리마인더 발송 시각은 현재 시각보다 과거일 수 없습니다.";

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

/// DB가 마이크로초까지 저장하므로 저장값·감사 기록·변경 비교가 같은 값을 쓰도록 맞춘다(`truncatedTo(MICROS)`).
fn truncate_to_micros(at: Timestamp) -> Timestamp {
    Timestamp::from_microsecond(at.as_microsecond()).unwrap_or(at)
}

fn kst() -> TimeZone {
    TimeZone::get("Asia/Seoul").expect("Asia/Seoul 시간대는 항상 있다")
}

/// 반복 방식(`ReminderRepeat`). 선언 순서가 원본 열거형의 서수 순서다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReminderRepeat {
    /// 발송 시각에 한 번, 발송 전에만 수정
    Once,
    /// 발송 시각의 날짜부터 매일 같은 KST 시각, 언제든 수정
    Daily,
}

impl ReminderRepeat {
    pub const ALL: [ReminderRepeat; 2] = [ReminderRepeat::Once, ReminderRepeat::Daily];

    pub const fn as_str(self) -> &'static str {
        match self {
            ReminderRepeat::Once => "ONCE",
            ReminderRepeat::Daily => "DAILY",
        }
    }

    pub fn parse(text: &str) -> Option<ReminderRepeat> {
        ReminderRepeat::ALL.into_iter().find(|r| r.as_str() == text)
    }

    /// `canUpdate`
    pub fn can_update(self, scheduled_at: Timestamp, now: Timestamp) -> bool {
        match self {
            ReminderRepeat::Once => now < scheduled_at,
            ReminderRepeat::Daily => true,
        }
    }

    /// `findOccurrence`: 지금 기준으로 보낼 수 있는 회차 시각. 없으면 `None`.
    pub fn find_occurrence(self, scheduled_at: Timestamp, now: Timestamp) -> Option<Timestamp> {
        match self {
            ReminderRepeat::Once => Some(scheduled_at),
            ReminderRepeat::Daily => find_today_occurrence(scheduled_at, now),
        }
    }
}

/// 오늘 KST 날짜의 같은 시각이다. 시작 시각 전이면 아직 시작하지 않았으므로 회차가 없다.
fn find_today_occurrence(scheduled_at: Timestamp, now: Timestamp) -> Option<Timestamp> {
    let zone = kst();
    let today = now.to_zoned(zone.clone()).date();
    let time: Time = scheduled_at.to_zoned(zone.clone()).time();
    let occurrence = zone.to_timestamp(today.to_datetime(time)).ok()?;
    if occurrence < scheduled_at {
        return None;
    }
    Some(occurrence)
}

/// 발송 대상(`ReminderTarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReminderTarget {
    /// 전체 회원
    All,
    /// 오늘 주제 참여자
    TopicParticipants,
    /// 오늘 주제 미참여자
    TopicNonParticipants,
}

impl ReminderTarget {
    pub const ALL: [ReminderTarget; 3] = [
        ReminderTarget::All,
        ReminderTarget::TopicParticipants,
        ReminderTarget::TopicNonParticipants,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ReminderTarget::All => "ALL",
            ReminderTarget::TopicParticipants => "TOPIC_PARTICIPANTS",
            ReminderTarget::TopicNonParticipants => "TOPIC_NON_PARTICIPANTS",
        }
    }

    pub fn parse(text: &str) -> Option<ReminderTarget> {
        ReminderTarget::ALL.into_iter().find(|t| t.as_str() == text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    /// 저장하기 전에는 `None`.
    pub id: Option<Uuid>,
    pub title: String,
    pub body: String,
    pub repeat: ReminderRepeat,
    pub target: ReminderTarget,
    /// 일회성은 발송 시각, 매일 반복은 첫 발송 날짜와 매일 보낼 시각이다.
    pub scheduled_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl Reminder {
    /// `Reminder.create`. 지난 시각 검사가 먼저이고 문구 검사가 뒤다(원본 순서).
    pub fn create(
        title: &str,
        body: &str,
        repeat: ReminderRepeat,
        target: ReminderTarget,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<Reminder, AppError> {
        validate_not_past(scheduled_at, now)?;
        let content = Content::normalize(title, body, scheduled_at)?;
        Ok(Reminder {
            id: None,
            title: content.title,
            body: content.body,
            repeat,
            target,
            scheduled_at: content.scheduled_at,
            deleted_at: None,
            created_at: None,
            updated_at: None,
        })
    }

    /// `Reminder.update`. 이미 시작한 매일 반복의 문구를 고칠 수 있도록 과거 검증은 발송 일정이 바뀔 때만 한다.
    pub fn update(
        &mut self,
        title: &str,
        body: &str,
        repeat: ReminderRepeat,
        target: ReminderTarget,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), AppError> {
        if self.is_deleted() {
            return Err(business("삭제된 리마인더는 수정할 수 없습니다."));
        }
        if !self.repeat.can_update(self.scheduled_at, now) {
            return Err(business("발송 시각이 지난 일회성 리마인더는 수정할 수 없습니다."));
        }
        if self.is_schedule_changed(repeat, scheduled_at) {
            validate_not_past(scheduled_at, now)?;
        }
        let content = Content::normalize(title, body, scheduled_at)?;
        self.title = content.title;
        self.body = content.body;
        self.repeat = repeat;
        self.target = target;
        self.scheduled_at = content.scheduled_at;
        Ok(())
    }

    /// `Reminder.delete`. 감사 기록의 삭제 시각이 DB 저장값과 같도록 저장 정밀도(마이크로초)에 맞춘다.
    pub fn delete(&mut self, deleted_at: Timestamp) -> Result<(), AppError> {
        if self.is_deleted() {
            return Err(business("이미 삭제된 리마인더입니다."));
        }
        self.deleted_at = Some(truncate_to_micros(deleted_at));
        Ok(())
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// `findDueOccurrence`: 회차 시각부터 30분 기한 안일 때만 보낸다. 등록·일정 변경 때 지난 시각을 거절하므로 이미 보낸
    /// 회차를 다시 지정할 수 없어 마지막 수정 시각과 비교하지 않으며, 문구만 고쳐도 그날 회차는 그대로 보낸다.
    pub fn find_due_occurrence(&self, now: Timestamp) -> Option<Timestamp> {
        if self.is_deleted() {
            return None;
        }
        let occurrence = self.repeat.find_occurrence(self.scheduled_at, now)?;
        if now < occurrence || is_push_expired_at(push_expires_at(occurrence), now) {
            return None;
        }
        Some(occurrence)
    }

    /// `Reminder.isValidTitle`: 감사 로그 상태 검사가 쓴다.
    pub fn is_valid_title(title: &str) -> bool {
        is_valid_normalized_text(java_trim(title), MAX_TITLE_LENGTH)
    }

    fn is_schedule_changed(&self, repeat: ReminderRepeat, scheduled_at: Timestamp) -> bool {
        self.repeat != repeat || self.scheduled_at != truncate_to_micros(scheduled_at)
    }
}

/// 발송 시각이 없으면 컨트롤러 검증이 먼저 거절한다.
fn validate_not_past(scheduled_at: Timestamp, now: Timestamp) -> Result<(), AppError> {
    if scheduled_at < now {
        return Err(business(PAST_MESSAGE));
    }
    Ok(())
}

struct Content {
    title: String,
    body: String,
    scheduled_at: Timestamp,
}

impl Content {
    fn normalize(title: &str, body: &str, scheduled_at: Timestamp) -> Result<Content, AppError> {
        Ok(Content {
            title: normalize_text(title, MAX_TITLE_LENGTH, TITLE_MESSAGE)?,
            body: normalize_text(body, MAX_BODY_LENGTH, BODY_MESSAGE)?,
            scheduled_at: truncate_to_micros(scheduled_at),
        })
    }
}

fn normalize_text(value: &str, max_length: usize, message: &str) -> Result<String, AppError> {
    let normalized = java_trim(value);
    if !is_valid_normalized_text(normalized, max_length) {
        return Err(business(message));
    }
    Ok(normalized.to_owned())
}

fn is_valid_normalized_text(normalized: &str, max_length: usize) -> bool {
    has_text(normalized) && code_points(normalized) <= max_length
}

/// 목록 한 쪽(`ReminderSlice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderSlice {
    pub reminders: Vec<Reminder>,
    pub has_next: bool,
}

const COLUMNS: &str =
    "id, title, body, repeat, target, scheduled_at, deleted_at, created_at, updated_at";

fn from_row(row: &Row) -> Result<Reminder, DbError> {
    let repeat: String = row.try_get("repeat")?;
    let target: String = row.try_get("target")?;
    Ok(Reminder {
        id: Some(row.try_get("id")?),
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        repeat: ReminderRepeat::parse(&repeat)
            .ok_or_else(|| DbError::Config(format!("알 수 없는 리마인더 반복 방식: {repeat}")))?,
        target: ReminderTarget::parse(&target)
            .ok_or_else(|| DbError::Config(format!("알 수 없는 리마인더 대상: {target}")))?,
        scheduled_at: row.try_get("scheduled_at")?,
        deleted_at: row.try_get("deleted_at")?,
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReminderRepository;

impl ReminderRepository {
    /// `save`/`saveAndFlush`: 새 리마인더면 INSERT, 이미 저장된 리마인더면 UPDATE.
    /// `created_at`·`updated_at`은 JPA의 `@CreationTimestamp`·`@UpdateTimestamp`처럼 앱 시각(`now`)으로 채운다.
    pub fn save_and_flush(
        &self,
        db: &mut impl Db,
        reminder: &mut Reminder,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let row = match reminder.id {
            None => db.query_one(
                "INSERT INTO reminders (title, body, repeat, target, scheduled_at, deleted_at, \
                 created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
                 RETURNING id, created_at, updated_at",
                &[
                    &reminder.title,
                    &reminder.body,
                    &reminder.repeat.as_str(),
                    &reminder.target.as_str(),
                    &reminder.scheduled_at,
                    &reminder.deleted_at,
                    &now,
                ],
            )?,
            Some(id) => db.query_one(
                "UPDATE reminders SET title = $2, body = $3, repeat = $4, target = $5, \
                 scheduled_at = $6, deleted_at = $7, updated_at = $8 WHERE id = $1 \
                 RETURNING id, created_at, updated_at",
                &[
                    &id,
                    &reminder.title,
                    &reminder.body,
                    &reminder.repeat.as_str(),
                    &reminder.target.as_str(),
                    &reminder.scheduled_at,
                    &reminder.deleted_at,
                    &now,
                ],
            )?,
        };
        reminder.id = Some(row.try_get("id")?);
        reminder.created_at = Some(row.try_get("created_at")?);
        reminder.updated_at = Some(row.try_get("updated_at")?);
        Ok(())
    }

    /// `findByIdForUpdate`: 비관적 쓰기 락(`for no key update`). 삭제한 행도 읽는다. 트랜잭션 안에서 불러야 한다.
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl Db,
        reminder_id: Uuid,
    ) -> Result<Option<Reminder>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM reminders WHERE id = $1 FOR NO KEY UPDATE");
        db.query_opt(&sql, &[&reminder_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findReminders`: 삭제하지 않은 리마인더를 등록 최신순으로 한 쪽(`pageSize + 1`건을 읽어 다음 쪽 여부를 안다).
    pub fn find_reminders(
        &self,
        db: &mut impl Db,
        page: i32,
        page_size: i32,
    ) -> Result<ReminderSlice, DbError> {
        let (limit, offset) = crate::admin::support::limit_offset(page, page_size);
        let sql = format!(
            "SELECT {COLUMNS} FROM reminders WHERE deleted_at IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT $1 OFFSET $2"
        );
        let mut reminders = db
            .query(&sql, &[&limit, &offset])?
            .iter()
            .map(from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let size = usize::try_from(page_size).unwrap_or(0);
        let has_next = reminders.len() > size;
        reminders.truncate(size);
        Ok(ReminderSlice {
            reminders,
            has_next,
        })
    }

    /// `findActiveReminders`: 삭제하지 않은 리마인더 전체(ID 순).
    pub fn find_active_reminders(&self, db: &mut impl Db) -> Result<Vec<Reminder>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM reminders WHERE deleted_at IS NULL ORDER BY id");
        db.query(&sql, &[])?.iter().map(from_row).collect()
    }

    /// `findActiveById`
    pub fn find_active_by_id(
        &self,
        db: &mut impl Db,
        reminder_id: Uuid,
    ) -> Result<Option<Reminder>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM reminders WHERE id = $1 AND deleted_at IS NULL");
        db.query_opt(&sql, &[&reminder_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findActiveForPublication`: 정시 발행은 수정·삭제 중인 리마인더를 기다리지 않고 건너뛴 뒤 다음 확인 때 새 내용으로
    /// 발행한다(`FOR UPDATE SKIP LOCKED`). 트랜잭션 안에서 불러야 한다.
    pub fn find_active_for_publication(
        &self,
        db: &mut impl Db,
        reminder_id: Uuid,
    ) -> Result<Option<Reminder>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM reminders WHERE id = $1 AND deleted_at IS NULL \
             FOR UPDATE SKIP LOCKED"
        );
        db.query_opt(&sql, &[&reminder_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }
}

#[cfg(test)]
#[path = "reminder_tests.rs"]
mod tests;
