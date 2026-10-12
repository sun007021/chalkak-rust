//! 주제 도메인. 원본 `topic.domain` 패키지(`Topic`, `ParticipationPeriod`, `TopicPhase`)를 옮겼다.
//!
//! 시각 경계는 원본과 같다: 시작 시각은 참여 중, 종료 시각은 참여 종료. 주제 날짜는 참여 시작 시각의 **한국 날짜**여야 한다.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

const MAX_TITLE_LENGTH: usize = 255;

/// 한국 시간대(`ZoneId.of("Asia/Seoul")`).
pub fn kst() -> TimeZone {
    TimeZone::get("Asia/Seoul").expect("Asia/Seoul은 번들된 tzdb에 있다")
}

/// 시각의 한국 날짜(`LocalDate.ofInstant(instant, KST)`).
pub fn kst_date(at: Timestamp) -> Date {
    Zoned::new(at, kst()).date()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicPhase {
    BeforeOpen,
    Open,
    Closed,
}

impl TopicPhase {
    /// JSON·DB에 쓰는 이름(Java enum 이름).
    pub const fn as_str(self) -> &'static str {
        match self {
            TopicPhase::BeforeOpen => "BEFORE_OPEN",
            TopicPhase::Open => "OPEN",
            TopicPhase::Closed => "CLOSED",
        }
    }
}

/// 참여 기간. 종료 시각이 시작 시각보다 뒤여야 한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParticipationPeriod {
    starts_at: Timestamp,
    ends_at: Timestamp,
}

fn invalid_period() -> AppError {
    AppError::business(
        ErrorCode::BUSINESS_ERROR,
        "주제 참여 기간이 올바르지 않습니다.",
    )
}

impl ParticipationPeriod {
    pub fn new(starts_at: Timestamp, ends_at: Timestamp) -> Result<Self, AppError> {
        if ends_at <= starts_at {
            return Err(invalid_period());
        }
        Ok(ParticipationPeriod { starts_at, ends_at })
    }

    /// 요청 DTO처럼 경계가 비어 있을 수 있는 입력용. 둘 중 하나라도 없으면 오류다.
    pub fn from_optional(
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
    ) -> Result<Self, AppError> {
        match (starts_at, ends_at) {
            (Some(starts_at), Some(ends_at)) => Self::new(starts_at, ends_at),
            _ => Err(invalid_period()),
        }
    }

    /// 저장소에서 읽은 값을 그대로 담는다. JPA가 `@Embedded` 필드를 검증 없이 채우는 것과 같다.
    pub(crate) fn restore(starts_at: Timestamp, ends_at: Timestamp) -> Self {
        ParticipationPeriod { starts_at, ends_at }
    }

    pub fn starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn ends_at(&self) -> Timestamp {
        self.ends_at
    }

    /// 시작 전이면 BEFORE_OPEN, 시작 시각부터 종료 시각 직전까지 OPEN, 종료 시각부터 CLOSED.
    pub fn phase_at(&self, now: Timestamp) -> TopicPhase {
        if now < self.starts_at {
            TopicPhase::BeforeOpen
        } else if now < self.ends_at {
            TopicPhase::Open
        } else {
            TopicPhase::Closed
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topic {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub title: String,
    pub topic_date: Date,
    pub participation_period: ParticipationPeriod,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}

fn invalid_topic() -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, "주제 정보가 올바르지 않습니다.")
}

fn state_changed(message: &str) -> AppError {
    AppError::business(ErrorCode::RESOURCE_STATE_CHANGED, message)
}

/// Java `String.trim()`: 코드 포인트 U+0020 이하의 앞뒤 문자를 뗀다.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

impl Topic {
    /// 새 주제. 공개 전(`BEFORE_OPEN`) 기간으로만 만들 수 있다.
    pub fn create(
        title: &str,
        topic_date: Date,
        participation_period: ParticipationPeriod,
        now: Timestamp,
    ) -> Result<Topic, AppError> {
        validate_business_state(title, topic_date, &participation_period, now)?;
        Ok(Topic {
            id: None,
            title: java_trim(title).to_owned(),
            topic_date,
            participation_period,
            created_at: None,
            updated_at: None,
            deleted_at: None,
        })
    }

    /// 공개 전 주제의 제목·날짜·기간을 바꾼다. 바뀐 것이 없으면 `RESOURCE_STATE_CHANGED`.
    pub fn update(
        &mut self,
        title: &str,
        topic_date: Date,
        participation_period: ParticipationPeriod,
        now: Timestamp,
    ) -> Result<(), AppError> {
        self.validate_editable(now)?;
        validate_business_state(title, topic_date, &participation_period, now)?;
        let normalized_title = java_trim(title);
        if self.title == normalized_title
            && self.topic_date == topic_date
            && self.participation_period == participation_period
        {
            return Err(state_changed("변경된 주제 정보가 없습니다."));
        }
        self.title = normalized_title.to_owned();
        self.topic_date = topic_date;
        self.participation_period = participation_period;
        Ok(())
    }

    /// 공개 전 주제만 삭제(소프트 삭제)할 수 있다.
    pub fn delete(&mut self, now: Timestamp) -> Result<(), AppError> {
        self.validate_editable(now)?;
        self.deleted_at = Some(now);
        Ok(())
    }

    pub fn phase_at(&self, now: Timestamp) -> TopicPhase {
        self.participation_period.phase_at(now)
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    pub fn validate_editable(&self, now: Timestamp) -> Result<(), AppError> {
        if self.is_deleted() || self.phase_at(now) != TopicPhase::BeforeOpen {
            return Err(state_changed("공개 전 주제만 변경할 수 있습니다."));
        }
        Ok(())
    }
}

fn validate_business_state(
    title: &str,
    topic_date: Date,
    period: &ParticipationPeriod,
    now: Timestamp,
) -> Result<(), AppError> {
    let blank = title.chars().all(is_java_whitespace);
    if blank || java_trim(title).chars().count() > MAX_TITLE_LENGTH {
        return Err(invalid_topic());
    }
    if topic_date != kst_date(period.starts_at()) || period.phase_at(now) != TopicPhase::BeforeOpen
    {
        return Err(invalid_topic());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::error::ErrorKind;
    use jiff::civil::date;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    mod participation_period {
        use super::*;

        // ParticipationPeriodTest
        const STARTS_AT: &str = "2026-08-12T00:00:00Z";
        const ENDS_AT: &str = "2026-08-13T00:00:00Z";

        fn period() -> ParticipationPeriod {
            ParticipationPeriod::new(ts(STARTS_AT), ts(ENDS_AT)).unwrap()
        }

        // ParticipationPeriodTest#phase_at_beforeStartsAt_returnsBeforeOpen
        #[test]
        fn phase_at_before_starts_at_returns_before_open() {
            assert_eq!(
                period().phase_at(ts("2026-08-11T23:59:59Z")),
                TopicPhase::BeforeOpen
            );
        }

        // ParticipationPeriodTest#phase_at_withinPeriod_returnsOpen
        #[test]
        fn phase_at_within_period_returns_open() {
            assert_eq!(
                period().phase_at(ts("2026-08-12T12:00:00Z")),
                TopicPhase::Open
            );
        }

        // ParticipationPeriodTest#phase_at_afterEndsAt_returnsClosed
        #[test]
        fn phase_at_after_ends_at_returns_closed() {
            assert_eq!(
                period().phase_at(ts("2026-08-13T00:00:01Z")),
                TopicPhase::Closed
            );
        }

        // ParticipationPeriodTest#phase_at_exactlyStartsAt_returnsOpen
        #[test]
        fn phase_at_exactly_starts_at_returns_open() {
            assert_eq!(period().phase_at(ts(STARTS_AT)), TopicPhase::Open);
        }

        // ParticipationPeriodTest#phase_at_exactlyEndsAt_returnsClosed
        #[test]
        fn phase_at_exactly_ends_at_returns_closed() {
            assert_eq!(period().phase_at(ts(ENDS_AT)), TopicPhase::Closed);
        }

        // ParticipationPeriodTest#create_invalidRange_throwsBusinessException
        #[test]
        fn create_invalid_range_throws_business_exception() {
            let err = ParticipationPeriod::new(ts(STARTS_AT), ts(STARTS_AT)).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Business);
            assert_eq!(err.message, "주제 참여 기간이 올바르지 않습니다.");
            assert!(ParticipationPeriod::new(ts(ENDS_AT), ts(STARTS_AT)).is_err());
        }

        // ParticipationPeriodTest#create_nullBoundary_throwsBusinessException
        #[test]
        fn create_null_boundary_throws_business_exception() {
            let err = ParticipationPeriod::from_optional(None, Some(ts(ENDS_AT))).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Business);
            assert!(ParticipationPeriod::from_optional(Some(ts(STARTS_AT)), None).is_err());
            assert!(ParticipationPeriod::from_optional(None, None).is_err());
        }
    }

    // TopicTest
    const NOW: &str = "2026-08-28T03:00:00Z";
    const STARTS_AT: &str = "2026-08-29T15:00:00Z";
    const ENDS_AT: &str = "2026-08-30T15:00:00Z";

    fn topic_date() -> Date {
        date(2026, 8, 30)
    }

    fn period(starts: &str, ends: &str) -> ParticipationPeriod {
        ParticipationPeriod::new(ts(starts), ts(ends)).unwrap()
    }

    fn topic() -> Topic {
        Topic::create(
            "오늘의 주제",
            topic_date(),
            period(STARTS_AT, ENDS_AT),
            ts(NOW),
        )
        .unwrap()
    }

    // TopicTest#create_validState_createsBeforeOpenTopic
    #[test]
    fn create_valid_state_creates_before_open_topic() {
        let topic = Topic::create(
            "  오늘 가장 기억에 남은 순간  ",
            topic_date(),
            period(STARTS_AT, ENDS_AT),
            ts(NOW),
        )
        .unwrap();
        assert_eq!(topic.title, "오늘 가장 기억에 남은 순간");
        assert_eq!(topic.topic_date, topic_date());
        assert_eq!(topic.phase_at(ts(NOW)), TopicPhase::BeforeOpen);
    }

    // TopicTest#create_mismatchedTopicDate_throwsBusinessException
    #[test]
    fn create_mismatched_topic_date_throws_business_exception() {
        let err = Topic::create(
            "오늘의 주제",
            date(2026, 8, 29),
            period(STARTS_AT, ENDS_AT),
            ts(NOW),
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Business);
        assert_eq!(err.code, ErrorCode::BUSINESS_ERROR);
        assert_eq!(err.message, "주제 정보가 올바르지 않습니다.");
    }

    // TopicTest#create_startedPeriod_throwsBusinessException
    #[test]
    fn create_started_period_throws_business_exception() {
        let now = ts(NOW);
        let err = Topic::create(
            "오늘의 주제",
            date(2026, 8, 28),
            ParticipationPeriod::new(now, now + jiff::SignedDuration::from_secs(3600)).unwrap(),
            now,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Business);
    }

    // TopicTest#update_beforeOpen_changesBusinessState
    #[test]
    fn update_before_open_changes_business_state() {
        let mut topic = topic();
        let day = jiff::SignedDuration::from_secs(86_400);
        let new_starts = ts(STARTS_AT) + day;
        let new_ends = ts(ENDS_AT) + day;
        topic
            .update(
                "수정한 주제",
                date(2026, 8, 31),
                ParticipationPeriod::new(new_starts, new_ends).unwrap(),
                ts(NOW),
            )
            .unwrap();
        assert_eq!(topic.title, "수정한 주제");
        assert_eq!(topic.topic_date, date(2026, 8, 31));
        assert_eq!(topic.participation_period.starts_at(), new_starts);
    }

    // TopicTest#update_openTopic_throwsStateChanged
    #[test]
    fn update_open_topic_throws_state_changed() {
        let mut topic = topic();
        let err = topic
            .update(
                "수정한 주제",
                topic_date(),
                period(STARTS_AT, ENDS_AT),
                ts(STARTS_AT),
            )
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Business);
        assert_eq!(err.code, ErrorCode::RESOURCE_STATE_CHANGED);
    }

    // Topic#update의 "변경 없음" 검사(원본 TopicTest에는 없고 관리자 수정 API가 의존한다)
    #[test]
    fn update_unchanged_throws_state_changed() {
        let mut topic = topic();
        let err = topic
            .update(
                " 오늘의 주제 ",
                topic_date(),
                period(STARTS_AT, ENDS_AT),
                ts(NOW),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::RESOURCE_STATE_CHANGED);
        assert_eq!(err.message, "변경된 주제 정보가 없습니다.");
    }

    // TopicTest#delete_beforeOpen_recordsDeletedAt
    #[test]
    fn delete_before_open_records_deleted_at() {
        let mut topic = topic();
        topic.delete(ts(NOW)).unwrap();
        assert_eq!(topic.deleted_at, Some(ts(NOW)));
        assert!(topic.is_deleted());
    }

    // TopicTest#delete_closedTopic_throwsStateChanged
    #[test]
    fn delete_closed_topic_throws_state_changed() {
        let mut topic = topic();
        let err = topic.delete(ts(ENDS_AT)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Business);
        assert_eq!(err.code, ErrorCode::RESOURCE_STATE_CHANGED);
        assert_eq!(err.message, "공개 전 주제만 변경할 수 있습니다.");
    }

    #[test]
    fn create_title_validation() {
        let make =
            |title: &str| Topic::create(title, topic_date(), period(STARTS_AT, ENDS_AT), ts(NOW));
        assert!(make("").is_err());
        assert!(make("   ").is_err());
        assert!(
            make("\u{3000}").is_err(),
            "Character.isWhitespace 공백뿐인 제목"
        );
        assert!(make(&"가".repeat(255)).is_ok());
        assert!(make(&"가".repeat(256)).is_err(), "코드 포인트 255자까지");
        assert!(
            make(&format!(" {} ", "가".repeat(255))).is_ok(),
            "앞뒤 공백은 세지 않는다"
        );
    }

    #[test]
    fn kst_date는_한국_날짜다() {
        assert_eq!(kst_date(ts("2026-10-12T15:00:00Z")), date(2026, 10, 13));
        assert_eq!(kst_date(ts("2026-10-12T14:59:59Z")), date(2026, 10, 12));
    }
}
