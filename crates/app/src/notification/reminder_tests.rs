//! 리마인더 도메인·저장소 테스트. 원본 `ReminderTest`, `ReminderRepeatTest`에 대응한다(DB 테스트는
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다).

use jiff::SignedDuration;

use super::*;
use crate::testing::TestDb;

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn plus(at: Timestamp, seconds: i64) -> Timestamp {
    at.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

fn now() -> Timestamp {
    ts("2026-10-21T09:00:00Z")
}

const TITLE: &str = "오늘이 얼마 남지 않았어요";
const BODY: &str = "사진을 자랑해 주세요!";

fn create_at(repeat: ReminderRepeat, scheduled_at: Timestamp) -> Reminder {
    Reminder::create(TITLE, BODY, repeat, ReminderTarget::All, scheduled_at, now()).unwrap()
}

fn create_text(title: &str, body: &str) -> Result<Reminder, AppError> {
    Reminder::create(
        title,
        body,
        ReminderRepeat::Once,
        ReminderTarget::All,
        plus(now(), 3600),
        now(),
    )
}

// ----- ReminderRepeatTest -----

#[test]
fn 일회성은_발송_시각_직전까지만_수정할_수_있다() {
    let at = ts("2026-10-21T12:00:00Z");
    for (offset_nanos, expected) in [(-1i64, true), (0, false), (1, false)] {
        let now = at
            .checked_add(SignedDuration::from_nanos(offset_nanos))
            .unwrap();
        assert_eq!(ReminderRepeat::Once.can_update(at, now), expected);
    }
}

#[test]
fn 매일_반복은_첫_발송_시각이_지난_뒤에도_수정할_수_있다() {
    let at = ts("2026-10-21T12:00:00Z");
    assert!(ReminderRepeat::Daily.can_update(at, plus(at, -1)));
    assert!(ReminderRepeat::Daily.can_update(at, plus(at, 86_400 * 30)));
}

// ----- ReminderTest -----

#[test]
fn 생성하면_앞뒤_공백을_제거하고_일정을_저장한다() {
    let scheduled_at = plus(now(), 3600);
    let reminder = Reminder::create(
        &format!("  {TITLE}  "),
        &format!("\n{BODY} "),
        ReminderRepeat::Daily,
        ReminderTarget::TopicNonParticipants,
        scheduled_at,
        now(),
    )
    .unwrap();
    assert_eq!(reminder.title, TITLE);
    assert_eq!(reminder.body, BODY);
    assert_eq!(reminder.repeat, ReminderRepeat::Daily);
    assert_eq!(reminder.target, ReminderTarget::TopicNonParticipants);
    assert_eq!(reminder.scheduled_at, scheduled_at);
}

#[test]
fn 제목은_50자까지_허용하고_51자는_거절한다() {
    let fifty = "가".repeat(50);
    assert_eq!(create_text(&format!(" {fifty} "), BODY).unwrap().title, fifty);
    assert_eq!(
        create_text(&"가".repeat(51), BODY).unwrap_err().message,
        "리마인더 제목은 1자 이상 50자 이하여야 합니다."
    );
}

#[test]
fn 본문은_500자까지_허용하고_501자는_거절한다() {
    let max = "가".repeat(500);
    assert_eq!(create_text(TITLE, &format!(" {max} ")).unwrap().body, max);
    assert_eq!(
        create_text(TITLE, &"가".repeat(501)).unwrap_err().message,
        "리마인더 본문은 1자 이상 500자 이하여야 합니다."
    );
}

#[test]
fn 이모지는_utf16_길이가_아니라_문자_하나로_센다() {
    assert!(create_text(&"😀".repeat(50), BODY).is_ok());
    assert!(create_text(TITLE, &"😀".repeat(500)).is_ok());
    assert!(create_text(&"😀".repeat(51), BODY).is_err());
}

#[test]
fn 제목과_본문이_공백뿐이면_거절한다() {
    for title in ["", "   ", "\n\t"] {
        assert_eq!(
            create_text(title, BODY).unwrap_err().message,
            "리마인더 제목은 1자 이상 50자 이하여야 합니다."
        );
    }
    for body in ["", "   ", "\n\t"] {
        assert_eq!(
            create_text(TITLE, body).unwrap_err().message,
            "리마인더 본문은 1자 이상 500자 이하여야 합니다."
        );
    }
}

#[test]
fn 반복_방식과_관계없이_현재_시각부터는_허용하고_과거는_거절한다() {
    for repeat in ReminderRepeat::ALL {
        assert_eq!(
            create_at(repeat, plus(now(), 1)).scheduled_at,
            plus(now(), 1)
        );
        assert_eq!(create_at(repeat, now()).scheduled_at, now());
        let past = now().checked_sub(SignedDuration::from_nanos(1)).unwrap();
        let error = Reminder::create(TITLE, BODY, repeat, ReminderTarget::All, past, now())
            .unwrap_err();
        assert_eq!(
            error.message,
            "리마인더 발송 시각은 현재 시각보다 과거일 수 없습니다."
        );
    }
}

#[test]
fn 지난_시각_검사가_문구_검사보다_먼저다() {
    let error = Reminder::create(
        "",
        BODY,
        ReminderRepeat::Once,
        ReminderTarget::All,
        plus(now(), -1),
        now(),
    )
    .unwrap_err();
    assert_eq!(
        error.message,
        "리마인더 발송 시각은 현재 시각보다 과거일 수 없습니다."
    );
}

#[test]
fn 발송_시각은_마이크로초까지만_남긴다() {
    let reminder = create_at(ReminderRepeat::Once, ts("2026-10-21T10:00:00.123456789Z"));
    assert_eq!(reminder.scheduled_at, ts("2026-10-21T10:00:00.123456Z"));
}

#[test]
fn 제목_판단은_생성의_제목_검증과_같은_결과를_낸다() {
    for length in [0, 1, 50, 51] {
        let title = format!(" {} ", "가".repeat(length));
        assert_eq!(
            Reminder::is_valid_title(&title),
            create_text(&title, BODY).is_ok(),
            "{length}"
        );
    }
}

#[test]
fn 수정하면_공백을_제거하고_내용을_바꾼다() {
    let mut reminder = create_at(ReminderRepeat::Once, plus(now(), 3600));
    let new_at = plus(now(), 7200);
    reminder
        .update(
            " 새 제목 ",
            " 새 본문 ",
            ReminderRepeat::Daily,
            ReminderTarget::TopicParticipants,
            new_at,
            now(),
        )
        .unwrap();
    assert_eq!(reminder.title, "새 제목");
    assert_eq!(reminder.body, "새 본문");
    assert_eq!(reminder.repeat, ReminderRepeat::Daily);
    assert_eq!(reminder.target, ReminderTarget::TopicParticipants);
    assert_eq!(reminder.scheduled_at, new_at);
}

#[test]
fn 발송_시각이_지난_일회성은_수정할_수_없고_기존_내용을_유지한다() {
    let mut reminder = create_at(ReminderRepeat::Once, now());
    let after = plus(now(), 60);
    let error = reminder
        .update(
            "새 제목",
            BODY,
            ReminderRepeat::Once,
            ReminderTarget::All,
            plus(after, 3600),
            after,
        )
        .unwrap_err();
    assert_eq!(
        error.message,
        "발송 시각이 지난 일회성 리마인더는 수정할 수 없습니다."
    );
    assert_eq!(reminder.title, TITLE);
}

#[test]
fn 시작일이_지난_매일_반복도_일정이_같으면_과거_검증_없이_수정한다() {
    let mut reminder = create_at(ReminderRepeat::Daily, now());
    reminder
        .update(
            "새 제목",
            "새 본문",
            ReminderRepeat::Daily,
            ReminderTarget::TopicNonParticipants,
            now(),
            plus(now(), 86_400 * 10),
        )
        .unwrap();
    assert_eq!(reminder.title, "새 제목");
    assert_eq!(reminder.target, ReminderTarget::TopicNonParticipants);
    assert_eq!(reminder.scheduled_at, now());
}

#[test]
fn 시작일이_지난_매일_반복의_발송_시각을_과거로_바꾸면_거절한다() {
    let mut reminder = create_at(ReminderRepeat::Daily, now());
    let error = reminder
        .update(
            "새 제목",
            BODY,
            ReminderRepeat::Daily,
            ReminderTarget::All,
            plus(now(), 3600),
            plus(now(), 86_400 * 10),
        )
        .unwrap_err();
    assert_eq!(
        error.message,
        "리마인더 발송 시각은 현재 시각보다 과거일 수 없습니다."
    );
    assert_eq!(reminder.title, TITLE);
    assert_eq!(reminder.scheduled_at, now());
}

#[test]
fn 반복_방식을_바꾸면_발송_시각이_같아도_과거_검증을_한다() {
    let mut reminder = create_at(ReminderRepeat::Daily, now());
    let error = reminder
        .update(
            TITLE,
            BODY,
            ReminderRepeat::Once,
            ReminderTarget::All,
            now(),
            plus(now(), 86_400 * 10),
        )
        .unwrap_err();
    assert_eq!(
        error.message,
        "리마인더 발송 시각은 현재 시각보다 과거일 수 없습니다."
    );
    assert_eq!(reminder.repeat, ReminderRepeat::Daily);
}

#[test]
fn 저장_정밀도_아래_나노초만_다르면_바뀐_일정으로_보지_않는다() {
    let mut reminder = create_at(ReminderRepeat::Daily, now());
    reminder
        .update(
            "새 제목",
            BODY,
            ReminderRepeat::Daily,
            ReminderTarget::All,
            now().checked_add(SignedDuration::from_nanos(999)).unwrap(),
            plus(now(), 86_400 * 10),
        )
        .unwrap();
    assert_eq!(reminder.title, "새 제목");
    assert_eq!(reminder.scheduled_at, now());
}

#[test]
fn 수정_입력이_올바르지_않으면_기존_내용을_유지한다() {
    for (title, body) in [("", "본문"), ("제목", "")] {
        let mut reminder = create_at(ReminderRepeat::Once, plus(now(), 3600));
        assert!(
            reminder
                .update(
                    title,
                    body,
                    ReminderRepeat::Daily,
                    ReminderTarget::TopicParticipants,
                    plus(now(), 7200),
                    now(),
                )
                .is_err()
        );
        assert_eq!(reminder.title, TITLE);
        assert_eq!(reminder.body, BODY);
        assert_eq!(reminder.repeat, ReminderRepeat::Once);
        assert_eq!(reminder.target, ReminderTarget::All);
        assert_eq!(reminder.scheduled_at, plus(now(), 3600));
    }
}

#[test]
fn 삭제하면_삭제_시각을_마이크로초까지_기록한다() {
    let mut reminder = create_text(TITLE, BODY).unwrap();
    reminder
        .delete(ts("2026-10-21T10:00:00.123456789Z"))
        .unwrap();
    assert!(reminder.is_deleted());
    assert_eq!(reminder.deleted_at, Some(ts("2026-10-21T10:00:00.123456Z")));
}

#[test]
fn 이미_삭제한_리마인더는_다시_삭제할_수_없고_최초_삭제_시각을_유지한다() {
    let mut reminder = create_text(TITLE, BODY).unwrap();
    reminder.delete(ts("2026-10-21T10:00:00Z")).unwrap();
    let error = reminder.delete(ts("2026-10-21T11:00:00Z")).unwrap_err();
    assert_eq!(error.message, "이미 삭제된 리마인더입니다.");
    assert_eq!(reminder.deleted_at, Some(ts("2026-10-21T10:00:00Z")));
}

#[test]
fn 삭제한_리마인더는_수정할_수_없다() {
    let mut reminder = create_text(TITLE, BODY).unwrap();
    reminder.delete(ts("2026-10-21T10:00:00Z")).unwrap();
    let error = reminder
        .update(
            TITLE,
            BODY,
            ReminderRepeat::Once,
            ReminderTarget::All,
            plus(now(), 3600),
            now(),
        )
        .unwrap_err();
    assert_eq!(error.message, "삭제된 리마인더는 수정할 수 없습니다.");
}

#[test]
fn 일회성은_발송_시각부터_30분_직전까지만_회차를_돌려준다() {
    let at = ts("2026-10-21T12:00:00Z");
    let reminder = create_at(ReminderRepeat::Once, at);
    for (offset, due) in [(-1, false), (0, true), (1799, true), (1800, false)] {
        let occurrence = reminder.find_due_occurrence(plus(at, offset));
        assert_eq!(occurrence, due.then_some(at), "{offset}");
    }
}

#[test]
fn 현재_시각으로_등록한_일회성은_바로_회차가_된다() {
    let reminder = create_at(ReminderRepeat::Once, now());
    assert_eq!(reminder.find_due_occurrence(plus(now(), 1)), Some(now()));
}

#[test]
fn 매일_반복은_오늘_kst_날짜의_같은_시각을_회차로_돌려준다() {
    let reminder = create_at(ReminderRepeat::Daily, ts("2026-10-21T12:00:00Z"));
    assert_eq!(
        reminder.find_due_occurrence(ts("2026-10-25T12:10:00Z")),
        Some(ts("2026-10-25T12:00:00Z"))
    );
}

#[test]
fn 매일_반복은_시작일_전에는_회차가_없고_시작일_당일부터_있다() {
    let reminder = create_at(ReminderRepeat::Daily, ts("2026-10-22T12:00:00Z"));
    assert_eq!(reminder.find_due_occurrence(ts("2026-10-21T12:10:00Z")), None);
    assert_eq!(
        reminder.find_due_occurrence(ts("2026-10-22T12:10:00Z")),
        Some(ts("2026-10-22T12:00:00Z"))
    );
}

#[test]
fn 매일_반복은_회차_뒤에_문구를_고쳐도_그날_회차를_그대로_돌려준다() {
    let mut reminder = create_at(ReminderRepeat::Daily, ts("2026-10-21T12:00:00Z"));
    let edited_at = ts("2026-10-25T12:00:30Z");
    reminder
        .update(
            "고친 제목",
            "고친 본문",
            ReminderRepeat::Daily,
            ReminderTarget::All,
            ts("2026-10-21T12:00:00Z"),
            edited_at,
        )
        .unwrap();
    assert_eq!(
        reminder.find_due_occurrence(plus(edited_at, 30)),
        Some(ts("2026-10-25T12:00:00Z"))
    );
}

#[test]
fn 매일_반복은_kst_자정이_지나면_전날_회차를_돌려주지_않는다() {
    let reminder = create_at(ReminderRepeat::Daily, ts("2026-10-21T14:50:00Z"));
    assert_eq!(
        reminder.find_due_occurrence(ts("2026-10-22T14:55:00Z")),
        Some(ts("2026-10-22T14:50:00Z"))
    );
    assert_eq!(reminder.find_due_occurrence(ts("2026-10-22T15:05:00Z")), None);
}

#[test]
fn 일회성은_kst_자정을_넘겨도_기한_안이면_예약_시각을_회차로_돌려준다() {
    let at = ts("2026-10-22T14:59:30Z");
    let reminder = create_at(ReminderRepeat::Once, at);
    assert_eq!(
        reminder.find_due_occurrence(ts("2026-10-22T15:00:00Z")),
        Some(at)
    );
}

#[test]
fn 삭제한_리마인더는_회차가_없다() {
    let mut reminder = create_at(ReminderRepeat::Once, now());
    reminder.delete(plus(now(), 1)).unwrap();
    assert_eq!(reminder.find_due_occurrence(plus(now(), 1)), None);
}

// ----- 저장소 -----

#[test]
fn 저장소는_등록_수정_삭제_조회를_왕복한다() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let mut client = db.client();
    let repo = ReminderRepository;
    let mut first = create_at(ReminderRepeat::Once, plus(now(), 3600));
    repo.save_and_flush(&mut client, &mut first, now()).unwrap();
    let first_id = first.id.unwrap();
    assert_eq!(first.created_at, Some(now()));

    let mut second = create_at(ReminderRepeat::Daily, plus(now(), 7200));
    repo.save_and_flush(&mut client, &mut second, plus(now(), 1))
        .unwrap();

    // 등록 최신순 + 다음 쪽 여부
    let page = repo.find_reminders(&mut client, 1, 1).unwrap();
    assert!(page.has_next);
    assert_eq!(page.reminders[0].id, second.id);
    let page = repo.find_reminders(&mut client, 2, 1).unwrap();
    assert!(!page.has_next);
    assert_eq!(page.reminders[0].id, Some(first_id));

    // 수정
    first.title = "바뀐 제목".to_owned();
    repo.save_and_flush(&mut client, &mut first, plus(now(), 5))
        .unwrap();
    let read = repo.find_active_by_id(&mut client, first_id).unwrap().unwrap();
    assert_eq!(read.title, "바뀐 제목");
    assert_eq!(read.updated_at, Some(plus(now(), 5)));
    assert_eq!(read.created_at, Some(now()));

    // 삭제하면 목록·활성 조회에서 빠지고 잠금 조회로는 읽힌다
    first.delete(plus(now(), 6)).unwrap();
    repo.save_and_flush(&mut client, &mut first, plus(now(), 6))
        .unwrap();
    assert!(repo.find_active_by_id(&mut client, first_id).unwrap().is_none());
    assert_eq!(repo.find_active_reminders(&mut client).unwrap().len(), 1);
    assert_eq!(repo.find_reminders(&mut client, 1, 20).unwrap().reminders.len(), 1);
    let locked = repo
        .find_by_id_for_update(&mut client, first_id)
        .unwrap()
        .unwrap();
    assert_eq!(locked.deleted_at, Some(plus(now(), 6)));
}
