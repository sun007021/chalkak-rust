//! 주제 푸시 발송 시간표(원본 `TopicPushSchedule`, `TopicPushScheduleRepositoryImpl`·`TopicPushScheduleJpaRepository`).
//!
//! 발송 시각은 KST 기준 분 단위 시각이고 같은 시각은 하나만 둔다(`ux_topic_push_schedules_send_time`). 이 시간표로 주제 푸시를
//! 발행하는 일(SQS)은 이 모듈의 범위가 아니다. 여기서는 관리자 API가 쓰는 규칙과 저장소, 발행 작업이 쓸
//! [`TopicPushSchedule::find_due_occurrence`]만 둔다.

use std::time::Duration;

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use postgres::Row;
use uuid::Uuid;

const SEND_TIME_UNIQUE_CONSTRAINT: &str = "ux_topic_push_schedules_send_time";
/// 푸시를 보낼 수 있는 기간(`PushDeadlinePolicy.LIFETIME`). 회차 시각부터 이 시간이 지나면 보내지 않는다.
const PUSH_LIFETIME: Duration = Duration::from_secs(30 * 60);

pub const INVALID_SEND_TIME_MESSAGE: &str =
    "주제 푸시 발송 시각은 분 단위의 KST 시각이어야 합니다.";
pub const DUPLICATE_SEND_TIME_MESSAGE: &str = "해당 시각의 주제 푸시 설정이 이미 존재합니다.";

pub fn duplicate_send_time_error() -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, DUPLICATE_SEND_TIME_MESSAGE)
}

/// `TopicPushSchedule.validateSendTime`: 초와 나노초가 0인 시각만 받는다. DB(`TIME(0)`)가 반올림하게 두지 않고 거절한다.
pub fn validate_send_time(send_time: Time) -> Result<(), AppError> {
    if send_time.second() != 0 || send_time.subsec_nanosecond() != 0 {
        return Err(AppError::business(
            ErrorCode::BUSINESS_ERROR,
            INVALID_SEND_TIME_MESSAGE,
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicPushSchedule {
    /// 저장하기 전에는 `None`.
    pub id: Option<Uuid>,
    pub send_time: Time,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl TopicPushSchedule {
    pub fn new(send_time: Time) -> Result<TopicPushSchedule, AppError> {
        validate_send_time(send_time)?;
        Ok(TopicPushSchedule {
            id: None,
            send_time,
            created_at: None,
            updated_at: None,
        })
    }

    pub fn update_send_time(&mut self, send_time: Time) -> Result<(), AppError> {
        validate_send_time(send_time)?;
        self.send_time = send_time;
        Ok(())
    }

    /// `findDueOccurrence`: `now`가 속한 KST 날짜의 오늘 회차가 지금 보낼 차례이면 그 시각을 돌려준다. 생성·수정 시각 이후에
    /// 도래한 회차만 실행하고(지난 시각으로 만든 설정은 소급하지 않는다), 회차 시각에서 30분이 지났으면 건너뛴다.
    pub fn find_due_occurrence(&self, now: Timestamp) -> Option<Timestamp> {
        let kst = TimeZone::get("Asia/Seoul").ok()?;
        let today = now.to_zoned(kst.clone()).date();
        let scheduled_at = today.at(
            self.send_time.hour(),
            self.send_time.minute(),
            self.send_time.second(),
            self.send_time.subsec_nanosecond(),
        );
        let scheduled_at = kst.to_timestamp(scheduled_at).ok()?;
        let updated_at = self.updated_at?;
        if now < scheduled_at || scheduled_at <= updated_at {
            return None;
        }
        let lifetime = SignedDuration::try_from(PUSH_LIFETIME).ok()?;
        let expires_at = scheduled_at.checked_add(lifetime).ok()?;
        if now >= expires_at {
            return None;
        }
        Some(scheduled_at)
    }
}

const COLUMNS: &str = "id, send_time, created_at, updated_at";

fn from_row(row: &Row) -> Result<TopicPushSchedule, DbError> {
    Ok(TopicPushSchedule {
        id: Some(row.try_get("id")?),
        send_time: row.try_get("send_time")?,
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
    })
}

/// 저장 결과. 발송 시각 중복은 업무 오류로 바꾸고 그 밖의 DB 오류는 그대로 전파한다.
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
pub struct TopicPushScheduleRepository;

impl TopicPushScheduleRepository {
    /// `findAllByOrderBySendTimeAsc`
    pub fn find_all_order_by_send_time(
        &self,
        db: &mut impl Db,
    ) -> Result<Vec<TopicPushSchedule>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM topic_push_schedules ORDER BY send_time ASC");
        db.query(&sql, &[])?.iter().map(from_row).collect()
    }

    /// `findById`
    pub fn find_by_id(
        &self,
        db: &mut impl Db,
        schedule_id: Uuid,
    ) -> Result<Option<TopicPushSchedule>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM topic_push_schedules WHERE id = $1");
        db.query_opt(&sql, &[&schedule_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findByIdForUpdate`: 비관적 쓰기 락(`for no key update`). 트랜잭션 안에서 불러야 한다.
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl Db,
        schedule_id: Uuid,
    ) -> Result<Option<TopicPushSchedule>, DbError> {
        let sql =
            format!("SELECT {COLUMNS} FROM topic_push_schedules WHERE id = $1 FOR NO KEY UPDATE");
        db.query_opt(&sql, &[&schedule_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `existsBySendTime`
    pub fn exists_by_send_time(&self, db: &mut impl Db, send_time: Time) -> Result<bool, DbError> {
        Ok(db
            .query_opt(
                "SELECT 1 FROM topic_push_schedules WHERE send_time = $1 LIMIT 1",
                &[&send_time],
            )?
            .is_some())
    }

    /// `saveAndFlush`: 새 설정이면 INSERT, 이미 저장된 설정이면 UPDATE.
    /// `created_at`·`updated_at`은 JPA의 `@CreationTimestamp`·`@UpdateTimestamp`처럼 앱 시각(`now`)으로 채운다.
    pub fn save_and_flush(
        &self,
        db: &mut impl Db,
        schedule: &mut TopicPushSchedule,
        now: Timestamp,
    ) -> Result<(), SaveError> {
        let result = match schedule.id {
            None => db.query_one(
                "INSERT INTO topic_push_schedules (send_time, created_at, updated_at) \
                 VALUES ($1, $2, $2) RETURNING id, created_at, updated_at",
                &[&schedule.send_time, &now],
            ),
            Some(id) => db.query_one(
                "UPDATE topic_push_schedules SET send_time = $2, updated_at = $3 WHERE id = $1 \
                 RETURNING id, created_at, updated_at",
                &[&id, &schedule.send_time, &now],
            ),
        };
        match result {
            Ok(row) => {
                schedule.id = Some(row.try_get("id").map_err(DbError::from)?);
                schedule.created_at = Some(row.try_get("created_at").map_err(DbError::from)?);
                schedule.updated_at = Some(row.try_get("updated_at").map_err(DbError::from)?);
                Ok(())
            }
            Err(error) => {
                let constraint = error
                    .as_db_error()
                    .and_then(postgres::error::DbError::constraint);
                if constraint == Some(SEND_TIME_UNIQUE_CONSTRAINT) {
                    Err(SaveError::Business(duplicate_send_time_error()))
                } else {
                    Err(SaveError::Db(DbError::Postgres(error)))
                }
            }
        }
    }

    /// `delete`
    pub fn delete(&self, db: &mut impl Db, schedule_id: Uuid) -> Result<(), DbError> {
        db.execute(
            "DELETE FROM topic_push_schedules WHERE id = $1",
            &[&schedule_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(hour: i8, minute: i8, second: i8, nanos: i32) -> Time {
        Time::new(hour, minute, second, nanos).unwrap()
    }

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn plus(at: Timestamp, seconds: i64) -> Timestamp {
        at.checked_add(SignedDuration::from_secs(seconds)).unwrap()
    }

    fn schedule_updated_at(send_time: Time, updated_at: Timestamp) -> TopicPushSchedule {
        let mut schedule = TopicPushSchedule::new(send_time).unwrap();
        schedule.updated_at = Some(updated_at);
        schedule
    }

    #[test]
    fn 하루의_시작_중간_마지막_분에_설정을_생성할_수_있다() {
        for send_time in [time(0, 0, 0, 0), time(12, 0, 0, 0), time(23, 59, 0, 0)] {
            assert_eq!(
                TopicPushSchedule::new(send_time).unwrap().send_time,
                send_time
            );
        }
    }

    #[test]
    fn 분_경계_전후의_초_단위_시각은_거절한다() {
        for send_time in [time(11, 59, 59, 0), time(12, 0, 1, 0), time(23, 59, 59, 0)] {
            assert_eq!(
                TopicPushSchedule::new(send_time).unwrap_err().message,
                INVALID_SEND_TIME_MESSAGE
            );
        }
    }

    #[test]
    fn 초_미만의_시각은_db에서_반올림해_저장하지_않고_거절한다() {
        for nanos in [1, 999_999_999] {
            assert!(TopicPushSchedule::new(time(12, 0, 0, nanos)).is_err());
        }
    }

    #[test]
    fn 하루의_시작_중간_마지막_분으로_발송_시각을_수정할_수_있다() {
        for send_time in [time(0, 0, 0, 0), time(12, 0, 0, 0), time(23, 59, 0, 0)] {
            let mut schedule = TopicPushSchedule::new(time(9, 0, 0, 0)).unwrap();
            schedule.update_send_time(send_time).unwrap();
            assert_eq!(schedule.send_time, send_time);
        }
    }

    #[test]
    fn 잘못된_시각으로_수정하면_기존_시각을_유지한다() {
        for send_time in [
            time(11, 59, 59, 0),
            time(12, 0, 1, 0),
            time(12, 0, 0, 1),
            time(12, 0, 0, 999_999_999),
        ] {
            let mut schedule = TopicPushSchedule::new(time(9, 0, 0, 0)).unwrap();
            assert!(schedule.update_send_time(send_time).is_err());
            assert_eq!(schedule.send_time, time(9, 0, 0, 0));
        }
    }

    #[test]
    fn 예정_시각부터_30분_직전까지만_원래_회차를_반환한다() {
        let scheduled_at = ts("2026-10-12T00:00:00Z");
        let schedule = schedule_updated_at(time(9, 0, 0, 0), plus(scheduled_at, -60));
        for seconds in [-1, 0, 1, 1799, 1800, 1801] {
            let occurrence = schedule.find_due_occurrence(plus(scheduled_at, seconds));
            if !(0..1800).contains(&seconds) {
                assert_eq!(occurrence, None, "{seconds}");
            } else {
                assert_eq!(occurrence, Some(scheduled_at), "{seconds}");
            }
        }
    }

    #[test]
    fn 생성_수정_시각_이후에_도래한_회차만_실행한다() {
        let scheduled_at = ts("2026-10-12T00:00:00Z");
        for seconds in [-1, 0, 1] {
            let schedule = schedule_updated_at(time(9, 0, 0, 0), plus(scheduled_at, seconds));
            let occurrence = schedule.find_due_occurrence(plus(scheduled_at, 60));
            if seconds >= 0 {
                assert_eq!(occurrence, None, "{seconds}");
            } else {
                assert_eq!(occurrence, Some(scheduled_at), "{seconds}");
            }
        }
    }

    #[test]
    fn 지난_시각으로_생성_수정한_설정은_소급_실행하지_않고_다음_날_실행한다() {
        let scheduled_at = ts("2026-10-12T00:00:00Z");
        let schedule = schedule_updated_at(time(9, 0, 0, 0), plus(scheduled_at, 1));
        assert_eq!(schedule.find_due_occurrence(plus(scheduled_at, 60)), None);
        assert_eq!(
            schedule.find_due_occurrence(plus(scheduled_at, 86_400)),
            Some(plus(scheduled_at, 86_400))
        );
    }

    #[test]
    fn utc_날짜가_달라도_kst_자정의_회차를_반환한다() {
        let midnight = ts("2026-10-12T15:00:00Z");
        let schedule = schedule_updated_at(time(0, 0, 0, 0), plus(midnight, -60));
        assert_eq!(schedule.find_due_occurrence(midnight), Some(midnight));
    }

    #[test]
    fn kst_날짜가_바뀌면_전날_마지막_회차를_소급_실행하지_않는다() {
        let midnight = ts("2026-10-12T15:00:00Z");
        let schedule = schedule_updated_at(time(23, 59, 0, 0), plus(midnight, -3600));
        assert_eq!(schedule.find_due_occurrence(midnight), None);
    }
}
