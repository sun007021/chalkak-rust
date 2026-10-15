//! 원본 `TopicService`/`TopicDetail`. 날짜로 주제를 조회한다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Pool;
use jiff::Timestamp;
use jiff::civil::Date;
use uuid::Uuid;

use super::domain::{Topic, TopicPhase, kst_date};
use super::repository::TopicRepository;
use crate::web::ApiError;
use crate::web::binding::LocalDate;

/// PostgreSQL `date`의 가장 이른 값(기원전 4713년 = 연도 -4712). 이보다 이른 날짜는 질의 자체가 DB 오류가 된다.
const POSTGRES_MIN_YEAR: i16 = -4712;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicDetail {
    pub id: Uuid,
    pub title: String,
    pub topic_date: Date,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub phase: TopicPhase,
}

impl TopicDetail {
    pub fn from_topic(topic: &Topic, now: Timestamp) -> TopicDetail {
        let period = topic.participation_period;
        TopicDetail {
            id: topic.id.expect("저장된 주제에는 식별자가 있다"),
            title: topic.title.clone(),
            topic_date: topic.topic_date,
            starts_at: period.starts_at(),
            ends_at: period.ends_at(),
            phase: period.phase_at(now),
        }
    }
}

#[derive(Clone)]
pub struct TopicService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    repository: TopicRepository,
}

impl TopicService {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>) -> Self {
        TopicService {
            pool,
            clock,
            repository: TopicRepository,
        }
    }

    /// `getTopic`: 미래(한국 날짜 기준) 주제는 조회할 수 없다. 단계(phase)는 지금 시각으로 정한다.
    pub fn get_topic(&self, topic_date: LocalDate) -> Result<TopicDetail, ApiError> {
        let now = self.clock.now();
        let date = validate_not_future(topic_date, now)?;
        let mut conn = self.pool.get()?;
        let topic = self.repository.find_active_by_topic_date(&mut conn, date)?;
        let topic = topic.ok_or_else(|| {
            ApiError::App(AppError::not_found(
                ErrorCode::BUSINESS_ERROR,
                "주제를 찾을 수 없습니다.",
            ))
        })?;
        Ok(TopicDetail::from_topic(&topic, now))
    }
}

/// `validateNotFuture`. 통과한 날짜를 DB 질의에 쓸 수 있는 형태로 돌려준다.
fn validate_not_future(topic_date: LocalDate, now: Timestamp) -> Result<Date, ApiError> {
    let date = match topic_date {
        LocalDate::AfterMax => return Err(future_error()),
        LocalDate::BeforeMin => return Err(out_of_range()),
        LocalDate::Valid(date) => date,
    };
    if date > kst_date(now) {
        return Err(future_error());
    }
    if date.year() < POSTGRES_MIN_YEAR {
        return Err(out_of_range());
    }
    Ok(date)
}

fn future_error() -> ApiError {
    ApiError::App(AppError::business(
        ErrorCode::BUSINESS_ERROR,
        "미래의 주제는 조회할 수 없습니다.",
    ))
}

/// 원본은 JDBC 파라미터 변환 단계에서 DB 오류가 나 500으로 끝난다.
fn out_of_range() -> ApiError {
    ApiError::Internal("date out of range for PostgreSQL".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    // 원본 TopicService.validateNotFuture: 한국 날짜 기준. UTC 15:00부터 한국은 다음 날이다.
    #[test]
    fn 한국_날짜로_미래를_판정한다() {
        let now = ts("2026-10-12T14:59:59Z"); // KST 2026-10-12 23:59:59
        assert!(validate_not_future(LocalDate::Valid(date(2026, 10, 12)), now).is_ok());
        let err = validate_not_future(LocalDate::Valid(date(2026, 10, 13)), now).unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(err.message(), "미래의 주제는 조회할 수 없습니다.");

        let now = ts("2026-10-12T15:00:00Z"); // KST 2026-10-13 00:00:00
        assert!(validate_not_future(LocalDate::Valid(date(2026, 10, 13)), now).is_ok());
    }

    #[test]
    fn 범위_밖_날짜는_방향으로_판정한다() {
        let now = ts("2026-10-12T00:00:00Z");
        assert_eq!(
            validate_not_future(LocalDate::AfterMax, now)
                .unwrap_err()
                .status(),
            400
        );
        assert_eq!(
            validate_not_future(LocalDate::BeforeMin, now)
                .unwrap_err()
                .status(),
            500
        );
        assert_eq!(
            validate_not_future(LocalDate::Valid(date(-4713, 1, 1)), now)
                .unwrap_err()
                .status(),
            500
        );
        assert!(validate_not_future(LocalDate::Valid(date(-4712, 1, 1)), now).is_ok());
    }
}
