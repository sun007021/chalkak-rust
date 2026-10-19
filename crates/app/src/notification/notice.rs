//! 공지. 원본 `Notice`, `NoticeStatus`, `NoticeRepositoryImpl`·`NoticeJpaRepository`의 JPQL, 회원 알림함 배포의 SQL
//! (`NotificationJpaRepository.insertNoticeNotifications`).
//!
//! 공지는 예약 시각에 회원 알림함으로 배포된다. 상태(`SCHEDULED`·`DISTRIBUTED`)는 배포 시각으로 정해지므로 저장하지 않고
//! 계산한다. 삭제는 소프트 삭제로, 배포 전 삭제는 발송 취소이고 배포 후 삭제는 회원 알림함에서 숨김이다.
//! 연결(또는 트랜잭션)은 호출 측이 넘긴다.

use std::time::Duration;

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::Db;
use chalkak_db::DbError;
use jiff::{SignedDuration, Timestamp};
use postgres::Row;
use uuid::Uuid;

use crate::admin::binding::{code_points, has_text, java_trim};

pub const MAX_TITLE_LENGTH: usize = 50;
pub const MAX_BODY_LENGTH: usize = 500;
/// 예약 시각이 현재보다 이만큼까지 과거인 것은 허용한다(즉시 발송은 현재 시각을 보낸다).
const PAST_SCHEDULE_TOLERANCE: Duration = Duration::from_secs(5 * 60);

const TITLE_MESSAGE: &str = "공지 제목은 1자 이상 50자 이하여야 합니다.";
const BODY_MESSAGE: &str = "공지 본문은 1자 이상 500자 이하여야 합니다.";

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

/// DB가 마이크로초까지 저장하므로 저장값·감사 기록·변경 비교가 같은 값을 쓰도록 맞춘다(`truncatedTo(MICROS)`).
fn truncate_to_micros(at: Timestamp) -> Timestamp {
    Timestamp::from_microsecond(at.as_microsecond()).unwrap_or(at)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeStatus {
    Scheduled,
    Distributed,
}

impl NoticeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            NoticeStatus::Scheduled => "SCHEDULED",
            NoticeStatus::Distributed => "DISTRIBUTED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// 저장하기 전에는 `None`.
    pub id: Option<Uuid>,
    pub title: String,
    pub body: String,
    pub scheduled_at: Timestamp,
    pub distributed_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
}

impl Notice {
    /// `Notice.create`. 제목·본문은 앞뒤 공백을 떼고 길이(코드 포인트)를 검사한다. 예약 시각이 없는 요청은 컨트롤러의
    /// 검증이 먼저 거절하므로 여기서는 받지 않는다.
    pub fn create(
        title: &str,
        body: &str,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<Notice, AppError> {
        let content = Content::normalize(title, body, scheduled_at, now)?;
        Ok(Notice {
            id: None,
            title: content.title,
            body: content.body,
            scheduled_at: content.scheduled_at,
            distributed_at: None,
            deleted_at: None,
            created_at: None,
            updated_at: None,
        })
    }

    /// `Notice.update`. 삭제했거나 배포된 공지는 바꿀 수 없다(배포된 공지는 회원에게 이미 전달됐다).
    pub fn update(
        &mut self,
        title: &str,
        body: &str,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<(), AppError> {
        if self.deleted_at.is_some() {
            return Err(business("삭제된 공지는 수정할 수 없습니다."));
        }
        if self.distributed_at.is_some() {
            return Err(business("배포된 공지는 수정할 수 없습니다."));
        }
        let content = Content::normalize(title, body, scheduled_at, now)?;
        self.title = content.title;
        self.body = content.body;
        self.scheduled_at = content.scheduled_at;
        Ok(())
    }

    /// `Notice.delete`. 감사 기록의 삭제 시각이 DB 저장값과 같도록 마이크로초로 맞춘다.
    pub fn delete(&mut self, deleted_at: Timestamp) -> Result<(), AppError> {
        if self.deleted_at.is_some() {
            return Err(business("이미 삭제된 공지입니다."));
        }
        self.deleted_at = Some(truncate_to_micros(deleted_at));
        Ok(())
    }

    /// `Notice.distribute`
    pub fn distribute(&mut self, distributed_at: Timestamp) -> Result<(), AppError> {
        if self.deleted_at.is_some() {
            return Err(business("삭제된 공지는 배포할 수 없습니다."));
        }
        if self.distributed_at.is_some() {
            return Err(business("예약 상태의 공지만 배포할 수 있습니다."));
        }
        self.distributed_at = Some(distributed_at);
        Ok(())
    }

    pub fn status(&self) -> NoticeStatus {
        if self.distributed_at.is_none() {
            NoticeStatus::Scheduled
        } else {
            NoticeStatus::Distributed
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// `Notice.isValidTitle`: 감사 로그 상태 검사가 쓴다.
    pub fn is_valid_title(title: &str) -> bool {
        is_valid_normalized_text(java_trim(title), MAX_TITLE_LENGTH)
    }
}

struct Content {
    title: String,
    body: String,
    scheduled_at: Timestamp,
}

impl Content {
    fn normalize(
        title: &str,
        body: &str,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<Content, AppError> {
        let title = normalize_text(title, MAX_TITLE_LENGTH, TITLE_MESSAGE)?;
        let body = normalize_text(body, MAX_BODY_LENGTH, BODY_MESSAGE)?;
        let tolerance = SignedDuration::try_from(PAST_SCHEDULE_TOLERANCE)
            .expect("허용 오차는 SignedDuration 범위 안이다");
        if now
            .checked_sub(tolerance)
            .is_ok_and(|earliest| scheduled_at < earliest)
        {
            return Err(business(
                "공지 예약 시각은 현재 시각보다 5분 넘게 과거일 수 없습니다.",
            ));
        }
        Ok(Content {
            title,
            body,
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

/// 배포 대상 공지와, 다음 묶음을 이어서 조회할 위치(예약 시각·ID)(`DistributableNotice`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistributableNotice {
    pub notice_id: Uuid,
    pub scheduled_at: Timestamp,
}

/// 목록 한 쪽(`NoticeSlice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeSlice {
    pub notices: Vec<Notice>,
    pub has_next: bool,
}

const COLUMNS: &str =
    "id, title, body, scheduled_at, distributed_at, deleted_at, created_at, updated_at";

fn from_row(row: &Row) -> Result<Notice, DbError> {
    Ok(Notice {
        id: Some(row.try_get("id")?),
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        scheduled_at: row.try_get("scheduled_at")?,
        distributed_at: row.try_get("distributed_at")?,
        deleted_at: row.try_get("deleted_at")?,
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoticeRepository;

impl NoticeRepository {
    /// `saveAndFlush`: 새 공지면 INSERT, 이미 저장된 공지면 UPDATE.
    /// `created_at`·`updated_at`은 JPA의 `@CreationTimestamp`·`@UpdateTimestamp`처럼 앱 시각(`now`)으로 채운다.
    pub fn save_and_flush(
        &self,
        db: &mut impl Db,
        notice: &mut Notice,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let row = match notice.id {
            None => db.query_one(
                "INSERT INTO notices (title, body, scheduled_at, distributed_at, deleted_at, \
                 created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $6) \
                 RETURNING id, created_at, updated_at",
                &[
                    &notice.title,
                    &notice.body,
                    &notice.scheduled_at,
                    &notice.distributed_at,
                    &notice.deleted_at,
                    &now,
                ],
            )?,
            Some(id) => db.query_one(
                "UPDATE notices SET title = $2, body = $3, scheduled_at = $4, distributed_at = $5, \
                 deleted_at = $6, updated_at = $7 WHERE id = $1 \
                 RETURNING id, created_at, updated_at",
                &[
                    &id,
                    &notice.title,
                    &notice.body,
                    &notice.scheduled_at,
                    &notice.distributed_at,
                    &notice.deleted_at,
                    &now,
                ],
            )?,
        };
        notice.id = Some(row.try_get("id")?);
        notice.created_at = Some(row.try_get("created_at")?);
        notice.updated_at = Some(row.try_get("updated_at")?);
        Ok(())
    }

    /// `findById`: 삭제한 공지도 읽는다.
    pub fn find_by_id(&self, db: &mut impl Db, notice_id: Uuid) -> Result<Option<Notice>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM notices WHERE id = $1");
        db.query_opt(&sql, &[&notice_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findByIdForUpdate`: 배포 처리와 겹치지 않도록 공지 행을 잠근다(`for no key update`). 배포 중이면 끝날 때까지 기다린
    /// 뒤 배포 결과를 읽는다. 잠금이 없으면 수정이 배포 전 상태를 읽은 채 UPDATE해 배포 시각을 되돌린다.
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl Db,
        notice_id: Uuid,
    ) -> Result<Option<Notice>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM notices WHERE id = $1 FOR NO KEY UPDATE");
        db.query_opt(&sql, &[&notice_id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findNotices`: 등록 최신순. 삭제한 공지도 포함한다.
    pub fn find_notices(
        &self,
        db: &mut impl Db,
        page: i32,
        page_size: i32,
    ) -> Result<NoticeSlice, DbError> {
        let (limit, offset) = crate::admin::support::limit_offset(page, page_size);
        let sql = format!(
            "SELECT {COLUMNS} FROM notices ORDER BY created_at DESC, id DESC LIMIT $1 OFFSET $2"
        );
        let rows = db.query(&sql, &[&limit, &offset])?;
        let mut notices = rows.iter().map(from_row).collect::<Result<Vec<_>, _>>()?;
        let has_next = notices.len() > usize::try_from(page_size).unwrap_or(0);
        notices.truncate(usize::try_from(page_size).unwrap_or(0));
        Ok(NoticeSlice { notices, has_next })
    }

    /// `findDistributableNotices`: 예약 시각이 지난 미배포·미삭제 공지를 (예약 시각, ID) 순으로. `after`가 있으면 그 다음부터.
    pub fn find_distributable_notices(
        &self,
        db: &mut impl Db,
        now: Timestamp,
        after: Option<DistributableNotice>,
        limit: i64,
    ) -> Result<Vec<DistributableNotice>, DbError> {
        let rows = match after {
            None => db.query(
                "SELECT id, scheduled_at FROM notices \
                 WHERE distributed_at IS NULL AND deleted_at IS NULL AND scheduled_at <= $1 \
                 ORDER BY scheduled_at, id LIMIT $2",
                &[&now, &limit],
            )?,
            Some(after) => db.query(
                "SELECT id, scheduled_at FROM notices \
                 WHERE distributed_at IS NULL AND deleted_at IS NULL AND scheduled_at <= $1 \
                 AND (scheduled_at > $2 OR (scheduled_at = $2 AND id > $3)) \
                 ORDER BY scheduled_at, id LIMIT $4",
                &[&now, &after.scheduled_at, &after.notice_id, &limit],
            )?,
        };
        rows.iter()
            .map(|row| {
                Ok(DistributableNotice {
                    notice_id: row.try_get("id")?,
                    scheduled_at: row.try_get("scheduled_at")?,
                })
            })
            .collect()
    }

    /// `findDistributableForUpdate`: 다른 배포가 잡은 공지는 건너뛴다(`SKIP LOCKED`).
    pub fn find_distributable_for_update(
        &self,
        db: &mut impl Db,
        notice_id: Uuid,
        now: Timestamp,
    ) -> Result<Option<Notice>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM notices \
             WHERE id = $1 AND distributed_at IS NULL AND deleted_at IS NULL AND scheduled_at <= $2 \
             FOR UPDATE SKIP LOCKED"
        );
        db.query_opt(&sql, &[&notice_id, &now])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `createNoticeNotifications`: 탈퇴하지 않은 회원 전체(정지 회원 포함)에게 공지 알림을 한 번에 만든다. 공지 ID를 사건
    /// 키로 써서 회원당 하나만 생긴다. 만든 알림 수를 돌려준다.
    pub fn create_notice_notifications(
        &self,
        db: &mut impl Db,
        notice: &Notice,
        created_at: Timestamp,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "INSERT INTO notifications (user_id, source_type, source_id, event_key, type, title, \
             body, created_at, sqs_publish_status, next_attempt_at) \
             SELECT u.id, 'NOTICE', $1, $1, 'NOTICE', $2, $3, $4, 'PENDING', $4 \
             FROM users u WHERE u.deleted_at IS NULL",
            &[&notice.id, &notice.title, &notice.body, &created_at],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TITLE: &str = "서비스 점검 안내";
    const BODY: &str = "10월 20일 02:00부터 점검합니다.";

    fn now() -> Timestamp {
        "2026-10-12T09:00:00Z".parse().unwrap()
    }

    fn plus(at: Timestamp, seconds: i64) -> Timestamp {
        at.checked_add(SignedDuration::from_secs(seconds)).unwrap()
    }

    fn plus_nanos(at: Timestamp, nanos: i64) -> Timestamp {
        at.checked_add(SignedDuration::from_nanos(nanos)).unwrap()
    }

    fn notice() -> Notice {
        Notice::create(TITLE, BODY, now(), now()).unwrap()
    }

    fn message<T: std::fmt::Debug>(result: Result<T, AppError>) -> String {
        result.unwrap_err().message
    }

    #[test]
    fn 공지를_생성하면_제목_본문의_앞뒤_공백을_제거하고_예약_시각을_저장한다() {
        let scheduled_at = plus(now(), 3600);
        let notice = Notice::create(
            &format!("  {TITLE}  "),
            &format!("\n{BODY} "),
            scheduled_at,
            now(),
        )
        .unwrap();
        assert_eq!(notice.title, TITLE);
        assert_eq!(notice.body, BODY);
        assert_eq!(notice.scheduled_at, scheduled_at);
    }

    #[test]
    fn 제목은_공백_제거_후_50자까지_허용하고_51자는_거절한다() {
        let fifty = "가".repeat(50);
        let created = Notice::create(&format!(" {fifty} "), BODY, now(), now()).unwrap();
        assert_eq!(created.title, fifty);
        assert_eq!(
            message(Notice::create(&"가".repeat(51), BODY, now(), now())),
            "공지 제목은 1자 이상 50자 이하여야 합니다."
        );
    }

    #[test]
    fn 본문은_공백_제거_후_500자까지_허용하고_501자는_거절한다() {
        let full = "가".repeat(500);
        assert_eq!(
            Notice::create(TITLE, &full, now(), now()).unwrap().body,
            full
        );
        assert_eq!(
            message(Notice::create(TITLE, &"가".repeat(501), now(), now())),
            "공지 본문은 1자 이상 500자 이하여야 합니다."
        );
    }

    #[test]
    fn 이모지는_utf16_길이가_아니라_문자_하나로_센다() {
        let fifty = "📢".repeat(50);
        assert_eq!(
            Notice::create(&fifty, BODY, now(), now()).unwrap().title,
            fifty
        );
        assert!(Notice::create(&"📢".repeat(51), BODY, now(), now()).is_err());
    }

    #[test]
    fn 제목이_없거나_공백뿐이면_거절한다() {
        for title in ["", "   ", "\n\t"] {
            assert_eq!(
                message(Notice::create(title, BODY, now(), now())),
                "공지 제목은 1자 이상 50자 이하여야 합니다.",
                "{title:?}"
            );
        }
    }

    #[test]
    fn 본문이_없거나_공백뿐이면_거절한다() {
        for body in ["", "   ", "\n\t"] {
            assert_eq!(
                message(Notice::create(TITLE, body, now(), now())),
                "공지 본문은 1자 이상 500자 이하여야 합니다.",
                "{body:?}"
            );
        }
    }

    #[test]
    fn 현재보다_정확히_5분_전까지는_허용하고_그보다_과거는_거절한다() {
        let five_minutes_ago = plus(now(), -300);
        assert_eq!(
            Notice::create(TITLE, BODY, plus(five_minutes_ago, 1), now())
                .unwrap()
                .scheduled_at,
            plus(five_minutes_ago, 1)
        );
        assert_eq!(
            Notice::create(TITLE, BODY, five_minutes_ago, now())
                .unwrap()
                .scheduled_at,
            five_minutes_ago
        );
        assert_eq!(
            message(Notice::create(
                TITLE,
                BODY,
                plus_nanos(five_minutes_ago, -1),
                now()
            )),
            "공지 예약 시각은 현재 시각보다 5분 넘게 과거일 수 없습니다."
        );
    }

    #[test]
    fn 미래_예약_시각은_기간_제한_없이_허용한다() {
        let next_year = plus(now(), 365 * 86_400);
        assert_eq!(
            Notice::create(TITLE, BODY, next_year, now())
                .unwrap()
                .scheduled_at,
            next_year
        );
    }

    #[test]
    fn 제목_판단은_앞뒤_공백을_제거한_뒤_50자까지_허용하고_51자는_거절한다() {
        assert!(Notice::is_valid_title(&format!(" {} ", "가".repeat(50))));
        assert!(!Notice::is_valid_title(&"가".repeat(51)));
        for blank in ["", "   ", "\n\t"] {
            assert!(!Notice::is_valid_title(blank), "{blank:?}");
        }
    }

    #[test]
    fn 제목_판단은_공지_생성의_제목_검증과_같은_결과를_낸다() {
        for length in [1, 50, 51] {
            let title = "가".repeat(length);
            assert_eq!(
                Notice::is_valid_title(&title),
                Notice::create(&title, BODY, now(), now()).is_ok(),
                "{length}"
            );
        }
    }

    #[test]
    fn 공지를_생성하면_예약_상태이고_배포_시각이_없다() {
        let notice = notice();
        assert_eq!(notice.status(), NoticeStatus::Scheduled);
        assert_eq!(notice.distributed_at, None);
    }

    #[test]
    fn 예약_상태의_공지를_배포하면_배포_상태와_배포_시각을_기록한다() {
        let mut notice = notice();
        let distributed_at = plus(now(), 60);
        notice.distribute(distributed_at).unwrap();
        assert_eq!(notice.status(), NoticeStatus::Distributed);
        assert_eq!(notice.distributed_at, Some(distributed_at));
    }

    #[test]
    fn 이미_배포한_공지는_다시_배포할_수_없고_최초_배포_시각을_유지한다() {
        let mut notice = notice();
        notice.distribute(now()).unwrap();
        assert_eq!(
            message(notice.distribute(plus(now(), 60))),
            "예약 상태의 공지만 배포할 수 있습니다."
        );
        assert_eq!(notice.distributed_at, Some(now()));
    }

    #[test]
    fn 예약_시각은_db_저장_정밀도인_마이크로초까지만_남긴다() {
        let notice = Notice::create(TITLE, BODY, plus_nanos(now(), 1_234_567), now()).unwrap();
        assert_eq!(notice.scheduled_at, plus_nanos(now(), 1_234_000));
    }

    #[test]
    fn 예약_상태의_공지는_제목_본문의_앞뒤_공백을_제거해_수정하고_예약_시각을_바꾼다() {
        let mut notice = notice();
        let scheduled_at = plus(now(), 86_400);
        notice
            .update(" 수정한 안내 ", "\n수정한 본문 ", scheduled_at, now())
            .unwrap();
        assert_eq!(notice.title, "수정한 안내");
        assert_eq!(notice.body, "수정한 본문");
        assert_eq!(notice.scheduled_at, scheduled_at);
    }

    #[test]
    fn 배포된_공지는_수정할_수_없고_내용을_유지한다() {
        let mut notice = notice();
        notice.distribute(now()).unwrap();
        assert_eq!(
            message(notice.update("수정한 안내", BODY, now(), now())),
            "배포된 공지는 수정할 수 없습니다."
        );
        assert_eq!(notice.title, TITLE);
    }

    #[test]
    fn 수정_입력은_작성과_같은_기준으로_검증하고_실패하면_기존_내용을_유지한다() {
        let cases = [
            (" ", "본문", 0, "공지 제목은 1자 이상 50자 이하여야 합니다."),
            (
                "제목",
                " ",
                0,
                "공지 본문은 1자 이상 500자 이하여야 합니다.",
            ),
            (
                "제목",
                "본문",
                301,
                "공지 예약 시각은 현재 시각보다 5분 넘게 과거일 수 없습니다.",
            ),
        ];
        for (title, body, seconds_ago, expected) in cases {
            let mut notice = notice();
            assert_eq!(
                message(notice.update(title, body, plus(now(), -seconds_ago), now())),
                expected
            );
            assert_eq!(notice.title, TITLE);
            assert_eq!(notice.body, BODY);
            assert_eq!(notice.scheduled_at, now());
        }
    }

    #[test]
    fn 예약_상태의_공지를_삭제하면_삭제_시각을_마이크로초까지_기록하고_예약_상태를_유지한다() {
        let mut notice = notice();
        notice.delete(plus_nanos(now(), 1_234_567)).unwrap();
        assert!(notice.is_deleted());
        assert_eq!(notice.deleted_at, Some(plus_nanos(now(), 1_234_000)));
        assert_eq!(notice.status(), NoticeStatus::Scheduled);
    }

    #[test]
    fn 배포된_공지도_삭제할_수_있고_배포_시각을_유지한다() {
        let mut notice = notice();
        notice.distribute(now()).unwrap();
        notice.delete(plus(now(), 60)).unwrap();
        assert_eq!(notice.deleted_at, Some(plus(now(), 60)));
        assert_eq!(notice.distributed_at, Some(now()));
    }

    #[test]
    fn 이미_삭제된_공지는_다시_삭제할_수_없고_최초_삭제_시각을_유지한다() {
        let mut notice = notice();
        notice.delete(now()).unwrap();
        assert_eq!(
            message(notice.delete(plus(now(), 60))),
            "이미 삭제된 공지입니다."
        );
        assert_eq!(notice.deleted_at, Some(now()));
    }

    #[test]
    fn 삭제된_공지는_수정할_수_없다() {
        let mut notice = notice();
        notice.delete(now()).unwrap();
        assert_eq!(
            message(notice.update("수정한 안내", BODY, now(), now())),
            "삭제된 공지는 수정할 수 없습니다."
        );
    }

    #[test]
    fn 삭제된_공지는_배포할_수_없다() {
        let mut notice = notice();
        notice.delete(now()).unwrap();
        assert_eq!(
            message(notice.distribute(now())),
            "삭제된 공지는 배포할 수 없습니다."
        );
        assert_eq!(notice.distributed_at, None);
    }
}
