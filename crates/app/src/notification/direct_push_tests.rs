//! 직접 발행 공통 테스트. 원본 `DirectPushPublisherTest`에 대응한다.

use chalkak_core::FixedClock;
use jiff::SignedDuration;

use super::*;
use crate::notification::push_test_support::{FakePublisher, Lines, failure, logger, ts};

const OCCURRED_AT: &str = "2026-10-22T12:00:00Z";
const EXPIRES_AT: &str = "2026-10-22T12:30:00Z";

fn user(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn occurrence() -> DirectPushOccurrence {
    DirectPushOccurrence {
        event_id: Uuid::from_u128(0xe1),
        kind: NotificationType::Topic,
        source_type: NotificationSourceType::Topic,
        source_id: Uuid::from_u128(0x50),
        occurred_at: ts(OCCURRED_AT),
        stage_prefix: "sample",
    }
}

struct Fixture {
    clock: Arc<FixedClock>,
    lines: Lines,
    fake: Arc<FakePublisher>,
    publisher: DirectPushPublisher,
}

fn fixture() -> Fixture {
    let clock = Arc::new(FixedClock::new(ts(OCCURRED_AT)));
    let lines = Lines::default();
    let fake = FakePublisher::accepting();
    let publisher = DirectPushPublisher::new(fake.clone(), clock.clone(), logger(&lines, &clock));
    Fixture {
        clock,
        lines,
        fake,
        publisher,
    }
}

#[test]
fn 회원마다_회차의_공통_값과_30분_뒤_만료_시각을_담아_발행하고_수락을_센다() {
    let f = fixture();
    let summary = f.publisher.publish(&occurrence(), &[user(1), user(2)]);
    let messages = f.fake.calls();
    assert_eq!(
        messages.iter().map(|m| m.user_id).collect::<Vec<_>>(),
        vec![user(1), user(2)]
    );
    for message in &messages {
        assert_eq!(message.event_id, Uuid::from_u128(0xe1));
        assert_eq!(message.notification_id, None);
        assert_eq!(message.kind, NotificationType::Topic);
        assert_eq!(message.source_type, Some(NotificationSourceType::Topic));
        assert_eq!(message.source_id, Some(Uuid::from_u128(0x50)));
        assert_eq!(message.occurred_at, ts(OCCURRED_AT));
        assert_eq!(message.expires_at, ts(EXPIRES_AT));
    }
    assert_eq!(
        summary,
        DirectPublicationSummary {
            expires_at: ts(EXPIRES_AT),
            target_count: 2,
            accepted_count: 2,
            failed_count: 0,
            unknown_count: 0,
            expired_count: 0,
        }
    );
    assert!(f.lines.events().is_empty());
}

#[test]
fn 실패와_결과_불명을_구분해_세고_접두사가_붙은_결과_로그를_남긴다() {
    let f = fixture();
    f.fake.queue(vec![
        Ok(PushPublicationResult::retry("RequestThrottled")),
        Ok(PushPublicationResult::unknown("SQS_CLIENT_ERROR")),
    ]);
    let summary = f.publisher.publish(&occurrence(), &[user(1), user(2)]);
    assert_eq!(summary.failed_count, 1);
    assert_eq!(summary.unknown_count, 1);
    let results = f.lines.at_stage("sample_publication_result");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["userId"], user(1).to_string());
    assert_eq!(results[0]["result"], "FAILED");
    assert_eq!(results[0]["errorCode"], "RequestThrottled");
    assert_eq!(results[1]["userId"], user(2).to_string());
    assert_eq!(results[1]["result"], "UNKNOWN");
    assert_eq!(results[1]["errorCode"], "SQS_CLIENT_ERROR");
    for log in &results {
        assert_eq!(log["type"], "notification");
        assert_eq!(log["eventId"], Uuid::from_u128(0xe1).to_string());
        assert_eq!(log["sourceId"], Uuid::from_u128(0x50).to_string());
        assert_eq!(log["notificationType"], "TOPIC");
        assert_eq!(log["logger_name"], LOGGER);
        assert_eq!(log["message"], "푸시 SQS 직접 발행 결과");
    }
}

#[test]
fn 발행_중_예외는_결과_불명으로_세고_다음_회원을_계속_발행한다() {
    let f = fixture();
    f.fake.queue(vec![
        Err(failure("IllegalStateException")),
        Ok(PushPublicationResult::accepted("sqs-message")),
    ]);
    let summary = f.publisher.publish(&occurrence(), &[user(1), user(2)]);
    assert_eq!(f.fake.calls().len(), 2);
    assert_eq!(summary.unknown_count, 1);
    assert_eq!(summary.accepted_count, 1);
    let logs = f.lines.at_stage("sample_publication_unknown");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["userId"], user(1).to_string());
    assert_eq!(logs[0]["result"], "UNKNOWN");
    assert_eq!(logs[0]["errorCode"], "IllegalStateException");
    assert_eq!(logs[0]["message"], "푸시 SQS 직접 발행 결과 확인 실패");
}

#[test]
fn 발행_도중_기한에_도달하면_남은_회원은_기한_생략으로_센다() {
    let f = fixture();
    let clock = f.clock.clone();
    f.fake.on_publish(move |_| clock.set(ts(EXPIRES_AT)));
    let summary = f
        .publisher
        .publish(&occurrence(), &[user(1), user(2), user(3)]);
    assert_eq!(f.fake.calls().len(), 1);
    assert_eq!(summary.accepted_count, 1);
    assert_eq!(summary.expired_count, 2);
}

#[test]
fn 시작_시점이_기한_직전이면_발행하고_기한_정각이면_아무도_발행하지_않는다() {
    let f = fixture();
    f.clock.set(
        ts(EXPIRES_AT)
            .checked_sub(SignedDuration::from_nanos(1))
            .unwrap(),
    );
    let before = f.publisher.publish(&occurrence(), &[user(1)]);
    f.clock.set(ts(EXPIRES_AT));
    let at = f.publisher.publish(&occurrence(), &[user(1)]);
    assert_eq!(f.fake.calls().len(), 1);
    assert_eq!(before.accepted_count, 1);
    assert_eq!(at.accepted_count, 0);
    assert_eq!(at.expired_count, 1);
}

#[test]
fn 회원이_없으면_발행하지_않고_모든_건수를_0으로_센다() {
    let f = fixture();
    let summary = f.publisher.publish(&occurrence(), &[]);
    assert!(f.fake.calls().is_empty());
    assert_eq!(
        summary,
        DirectPublicationSummary {
            expires_at: ts(EXPIRES_AT),
            target_count: 0,
            accepted_count: 0,
            failed_count: 0,
            unknown_count: 0,
            expired_count: 0,
        }
    );
}
