//! 푸시 발송 기록의 도메인 시험. 원본 `PushDeliveryTest`와 `PushDeliveryLocksTest`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use jiff::Timestamp;
use uuid::Uuid;

use super::push_delivery::{
    DeviceDeliveryStatus, DeviceResult, PushDelivery, PushDeliveryLocks, PushDeliveryStatus,
};
use super::push_test_support::{plus, ts};

const NOW: &str = "2026-10-26T00:00:00Z";

fn now() -> Timestamp {
    ts(NOW)
}

fn sending(count: i32) -> Result<DeviceResult, chalkak_core::AppError> {
    DeviceResult::new(DeviceDeliveryStatus::Sending, count, None, now(), None)
}

fn snapshot(results: BTreeMap<Uuid, DeviceResult>) -> PushDelivery {
    PushDelivery {
        id: Uuid::new_v4(),
        event_id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        status: PushDeliveryStatus::Pending,
        device_results: results,
        completed_at: None,
        created_at: now(),
        updated_at: now(),
    }
}

const ALL: [DeviceDeliveryStatus; 7] = [
    DeviceDeliveryStatus::Sending,
    DeviceDeliveryStatus::Accepted,
    DeviceDeliveryStatus::Retryable,
    DeviceDeliveryStatus::Unknown,
    DeviceDeliveryStatus::InvalidToken,
    DeviceDeliveryStatus::PermanentFailure,
    DeviceDeliveryStatus::Skipped,
];

// PushDeliveryTest#startDevice_firstAttempt_returnsSendingRecordAndKeepsSnapshot
#[test]
fn 첫_발송_시작을_기록하되_저장_전에는_스냅샷을_바꾸지_않는다() {
    let device = Uuid::new_v4();
    let delivery = snapshot(BTreeMap::new());

    let started = delivery.start_device(device, now()).unwrap();

    assert_eq!(started.status, DeviceDeliveryStatus::Sending);
    assert_eq!(started.attempt_count, 1);
    assert!(delivery.device_results.is_empty());
}

// PushDeliveryTest#startDevice_retryableHistory_incrementsAttemptCount
#[test]
fn 미확정_일시_실패_기록은_별도_횟수_상한_없이_다음_시도를_기록한다() {
    for status in [
        DeviceDeliveryStatus::Sending,
        DeviceDeliveryStatus::Unknown,
        DeviceDeliveryStatus::Retryable,
    ] {
        let device = Uuid::new_v4();
        let previous =
            DeviceResult::new(status, 10, Some("UNAVAILABLE".to_owned()), now(), None).unwrap();
        let delivery = snapshot(BTreeMap::from([(device, previous)]));

        let started = delivery.start_device(device, plus(now(), 60)).unwrap();

        assert_eq!(started.attempt_count, 11, "{status:?}");
        assert_eq!(started.send_started_at, plus(now(), 60));
        assert_eq!(started.last_error_code, None);
    }
}

// PushDeliveryTest#startDevice_terminalHistory_rejectsNewAttempt
#[test]
fn 종료된_기기_결과는_다시_발송을_시작하지_않는다() {
    for status in [
        DeviceDeliveryStatus::Accepted,
        DeviceDeliveryStatus::PermanentFailure,
        DeviceDeliveryStatus::InvalidToken,
        DeviceDeliveryStatus::Skipped,
    ] {
        let device = Uuid::new_v4();
        let result = sending(1).unwrap().finish(status, None, now()).unwrap();
        let delivery = snapshot(BTreeMap::from([(device, result)]));

        assert!(!delivery.can_send_to(device), "{status:?}");
        assert_eq!(
            delivery.has_accepted_device(),
            status == DeviceDeliveryStatus::Accepted
        );
        assert!(delivery.start_device(device, now()).is_err());
    }
}

// PushDeliveryTest#finish_sendingAttempt_preservesCountAndSetsAcceptanceTime
#[test]
fn 발송_중인_시도에_결과를_기록하며_수락_결과에만_수락_시각을_남긴다() {
    for status in ALL
        .into_iter()
        .filter(|s| *s != DeviceDeliveryStatus::Sending)
    {
        let result = sending(2)
            .unwrap()
            .finish(status, Some("RESULT"), plus(now(), 1))
            .unwrap();

        assert_eq!(result.status, status);
        assert_eq!(result.attempt_count, 2);
        assert_eq!(result.send_started_at, now());
        assert_eq!(result.last_error_code.as_deref(), Some("RESULT"));
        if status == DeviceDeliveryStatus::Accepted {
            assert_eq!(result.accepted_at, Some(plus(now(), 1)));
        } else {
            assert_eq!(result.accepted_at, None, "{status:?}");
        }
    }
}

// PushDeliveryTest#constructor_nonPositiveAttemptCount_rejectsRecord
#[test]
fn 발송_시작_기록의_횟수는_양수여야_한다() {
    for count in [-1, 0] {
        assert!(sending(count).is_err(), "{count}");
    }
}

// PushDeliveryTest#constructor_inconsistentResult_rejectsRecord
#[test]
fn 수락_상태와_수락_시각이_서로_맞지_않으면_거절한다() {
    assert!(DeviceResult::new(DeviceDeliveryStatus::Accepted, 1, None, now(), None).is_err());
    assert!(
        DeviceResult::new(DeviceDeliveryStatus::Retryable, 1, None, now(), Some(now())).is_err()
    );
}

// PushDeliveryTest#finish_notSendingOrMissingResult_rejectsTransition
#[test]
fn 완료_결과를_덮어쓰거나_결과_없이_발송_중_상태로_마치는_것을_거절한다() {
    let accepted = sending(1)
        .unwrap()
        .finish(DeviceDeliveryStatus::Accepted, None, now())
        .unwrap();

    assert!(
        accepted
            .finish(DeviceDeliveryStatus::Retryable, Some("UNAVAILABLE"), now())
            .is_err()
    );
    assert!(
        sending(1)
            .unwrap()
            .finish(DeviceDeliveryStatus::Sending, None, now())
            .is_err()
    );
}

// PushDeliveryTest#recordDeviceResult_acceptedResult_updatesCurrentSnapshot
#[test]
fn 저장한_기기_결과를_반영하면_같은_실행에서도_수락_기기를_다시_시작하지_않는다() {
    let device = Uuid::new_v4();
    let mut delivery = snapshot(BTreeMap::new());
    let accepted = delivery
        .start_device(device, now())
        .unwrap()
        .finish(DeviceDeliveryStatus::Accepted, None, now())
        .unwrap();

    delivery.record_device_result(device, accepted.clone());

    assert!(!delivery.can_send_to(device));
    assert_eq!(delivery.device_results.get(&device), Some(&accepted));
    assert!(delivery.start_device(device, now()).is_err());
}

#[test]
fn 기기_결과는_jackson_모양_json으로_왕복한다() {
    let result = sending(3)
        .unwrap()
        .finish(DeviceDeliveryStatus::Accepted, None, plus(now(), 1))
        .unwrap();

    let json = result.to_json();

    assert_eq!(
        json.to_string(),
        r#"{"status":"ACCEPTED","attemptCount":3,"lastErrorCode":null,"sendStartedAt":"2026-10-26T00:00:00Z","acceptedAt":"2026-10-26T00:00:01Z"}"#
    );
}

// PushDeliveryLocksTest#tryAcquire_sameKeyInSameThread_rejectsNestedEntryAndAllowsAfterRelease
#[test]
fn 같은_키의_중첩_진입을_거절하고_해제_후에는_다시_등록한다() {
    let locks = PushDeliveryLocks::default();
    let (event, user) = (Uuid::new_v4(), Uuid::new_v4());

    let first = locks.try_acquire(event, user).unwrap();
    assert!(locks.try_acquire(event, user).is_none());
    // 거절된 진입이 바깥 처리의 키를 지우지 않는다.
    assert!(locks.is_held(event, user));
    drop(first);

    assert!(!locks.is_held(event, user));
    assert!(locks.try_acquire(event, user).is_some());
}

// PushDeliveryLocksTest#tryAcquire_differentEventOrUser_isIndependent
#[test]
fn 사건이나_회원이_다르면_동시에_등록하고_각_키를_독립적으로_제거한다() {
    let locks = PushDeliveryLocks::default();
    let (event, other_event) = (Uuid::new_v4(), Uuid::new_v4());
    let (user, other_user) = (Uuid::new_v4(), Uuid::new_v4());

    let a = locks.try_acquire(event, user).unwrap();
    let b = locks.try_acquire(other_event, user).unwrap();
    let c = locks.try_acquire(event, other_user).unwrap();

    drop(b);
    assert!(locks.is_held(event, user));
    assert!(!locks.is_held(other_event, user));
    assert!(locks.is_held(event, other_user));
    drop(a);
    drop(c);
    assert!(!locks.is_held(event, user));
}

#[test]
fn 패닉으로_끝나도_키가_남지_않는다() {
    let locks = Arc::new(PushDeliveryLocks::default());
    let (event, user) = (Uuid::new_v4(), Uuid::new_v4());
    let worker = Arc::clone(&locks);

    let outcome = std::thread::spawn(move || {
        let _guard = worker.try_acquire(event, user).unwrap();
        panic!("처리 중 패닉");
    })
    .join();

    assert!(outcome.is_err());
    assert!(locks.try_acquire(event, user).is_some());
}

// PushDeliveryLocksTest#tryAcquire_concurrentAcquireAndRelease_allowsOneThreadPerKey
#[test]
fn 동시_획득과_해제_재획득이_반복돼도_같은_키의_임계_구역에는_한_스레드만_들어간다() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 200;
    let locks = Arc::new(PushDeliveryLocks::default());
    let (event, user) = (Uuid::new_v4(), Uuid::new_v4());
    let inside = Arc::new(AtomicUsize::new(0));
    let max_inside = Arc::new(AtomicUsize::new(0));
    let acquired = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let (locks, inside, max_inside, acquired, start) = (
                Arc::clone(&locks),
                Arc::clone(&inside),
                Arc::clone(&max_inside),
                Arc::clone(&acquired),
                Arc::clone(&start),
            );
            std::thread::spawn(move || {
                start.wait();
                for _ in 0..ROUNDS {
                    if let Some(guard) = locks.try_acquire(event, user) {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_inside.fetch_max(now, Ordering::SeqCst);
                        acquired.fetch_add(1, Ordering::SeqCst);
                        std::thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                        drop(guard);
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(max_inside.load(Ordering::SeqCst), 1);
    assert!(acquired.load(Ordering::SeqCst) > 0);
    assert!(!locks.is_held(event, user));
}
