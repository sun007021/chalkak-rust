//! requestId 밖의 MDC 항목(예: SQS 푸시 Worker의 `sqsMessageId`, `receiveCount`).
//!
//! 원본은 SLF4J MDC(스레드에 묶인 문자열 맵)에 값을 넣고, 로그 JSON이 MDC 항목을 `requestId` 다음에 최상위 키로 싣는다.
//! 여기서도 스레드 로컬이라 다른 스레드로 일을 넘기면 값이 따라가지 않는다. `requestId`는 [`crate::request_id`]가 맡는다.
//! 항목 순서는 넣은 순서다(Logback MDC 맵의 순회 순서와 같음: `requestId`·`sqsMessageId`가 같은 칸이고 `receiveCount`가 뒤 칸).

use std::cell::RefCell;

thread_local! {
    static ENTRIES: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// 항목 범위. 버리면 들어오기 전 항목으로 돌아간다(`MDC.clear()` 뒤 `setContextMap(previous)`와 같다).
#[must_use = "가드를 버리면 MDC 범위가 바로 끝난다"]
pub struct MdcGuard {
    previous: Vec<(String, String)>,
    // 스레드 로컬을 되돌리므로 다른 스레드로 옮길 수 없게 한다.
    _not_send: std::marker::PhantomData<*const ()>,
}

/// 현재 스레드의 MDC에 항목을 넣는다(같은 키가 있으면 값을 바꾼다). 값이 `None`이면 넣지 않는다.
pub fn enter(entries: &[(&str, Option<&str>)]) -> MdcGuard {
    let previous = ENTRIES.with(|e| e.borrow().clone());
    ENTRIES.with(|e| {
        let mut current = e.borrow_mut();
        for (key, value) in entries {
            let Some(value) = value else { continue };
            match current.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = (*value).to_owned(),
                None => current.push(((*key).to_owned(), (*value).to_owned())),
            }
        }
    });
    MdcGuard {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for MdcGuard {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        ENTRIES.with(|e| *e.borrow_mut() = previous);
    }
}

/// `MDC.get`
pub fn get(key: &str) -> Option<String> {
    ENTRIES.with(|e| {
        e.borrow()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    })
}

/// 지금 스레드의 항목 전부(넣은 순서). 로그 이벤트가 만들어질 때 쓴다.
pub fn snapshot() -> Vec<(String, String)> {
    ENTRIES.with(|e| e.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 범위_밖에서는_항목이_없다() {
        assert!(snapshot().is_empty());
        assert_eq!(get("sqsMessageId"), None);
    }

    #[test]
    fn 가드를_버리면_들어오기_전_항목으로_돌아가고_같은_키는_덮어쓴다() {
        let _outer = enter(&[("sqsMessageId", Some("previous"))]);
        {
            let _inner = enter(&[
                ("sqsMessageId", Some("sqs-id")),
                ("receiveCount", Some("2")),
            ]);
            assert_eq!(get("sqsMessageId").as_deref(), Some("sqs-id"));
            assert_eq!(
                snapshot(),
                vec![
                    ("sqsMessageId".to_owned(), "sqs-id".to_owned()),
                    ("receiveCount".to_owned(), "2".to_owned())
                ]
            );
        }
        assert_eq!(get("sqsMessageId").as_deref(), Some("previous"));
        assert_eq!(get("receiveCount"), None);
    }

    #[test]
    fn 값이_없는_항목은_넣지_않는다() {
        let _guard = enter(&[("receiveCount", None)]);
        assert_eq!(get("receiveCount"), None);
    }

    #[test]
    fn 항목은_스레드에_묶인다_mdc와_같다() {
        let _guard = enter(&[("sqsMessageId", Some("main"))]);
        assert_eq!(std::thread::spawn(snapshot).join().unwrap(), vec![]);
    }
}
