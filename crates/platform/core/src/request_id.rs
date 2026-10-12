//! 요청별 requestId.
//!
//! 원본: `RequestIdFilter`가 요청마다 `UUID.randomUUID()`(v4)를 만들어 MDC `requestId`에 넣고 응답 헤더
//! `X-Request-Id`로 돌려준다. `CurrentRequestId`는 MDC 값이 UUID 형식이면 그 값을, 아니면(스케줄러 등 요청 밖)
//! 새 UUID를 돌려준다.
//!
//! MDC는 스레드에 묶인 값이라 여기서도 스레드 로컬이다. 다른 스레드로 일을 넘기면 값이 따라가지 않는다
//! (원본 MDC와 같은 성질). 요청을 처리하는 워커는 [`enter`]로 범위를 열고, 가드를 버리면 이전 값으로 돌아간다.

use std::cell::RefCell;

use uuid::Uuid;

/// 응답 헤더 이름. 원본 `LogFields.REQUEST_ID_HEADER`.
pub const HEADER: &str = "X-Request-Id";

thread_local! {
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 새 requestId(UUID v4). 문자열로는 소문자 하이픈 형식이다(`Uuid::to_string`).
pub fn generate() -> Uuid {
    Uuid::new_v4()
}

/// 현재 스레드의 requestId 범위. 버리면 이전 값(없었으면 없음)으로 돌아간다. MDC의 put/remove 쌍과 같다.
#[must_use = "가드를 버리면 requestId 범위가 바로 끝난다"]
pub struct RequestIdGuard {
    previous: Option<String>,
    // 스레드 로컬을 되돌리므로 다른 스레드로 옮길 수 없게 한다.
    _not_send: std::marker::PhantomData<*const ()>,
}

/// 현재 스레드에 requestId를 지정한다.
pub fn enter(id: impl ToString) -> RequestIdGuard {
    let previous = CURRENT.with(|c| c.borrow_mut().replace(id.to_string()));
    RequestIdGuard {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for RequestIdGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// 현재 스레드의 requestId 원문(MDC `requestId`). UUID가 아닌 값도 그대로 돌려준다.
pub fn current() -> Option<String> {
    CURRENT.with(|c| c.borrow().clone())
}

/// 원본 `CurrentRequestId.resolve`: 현재 값이 UUID 형식이면 그 값, 없거나 형식이 다르면 새 UUID.
pub fn resolve_current() -> Uuid {
    current()
        .as_deref()
        .and_then(parse_hyphenated_uuid)
        .unwrap_or_else(generate)
}

/// 원본 정규식 `[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-...-[0-9a-fA-F]{12}`(대소문자 허용, 하이픈 형식만).
fn parse_hyphenated_uuid(s: &str) -> Option<Uuid> {
    if s.len() != 36 {
        return None;
    }
    Uuid::try_parse(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 헤더_이름() {
        assert_eq!(HEADER, "X-Request-Id");
    }

    // AccessLogTest: UUID.fromString(requestId).toString()이 requestId와 같아야 한다 -> 소문자 하이픈 36자
    #[test]
    fn 생성한_값은_소문자_하이픈_uuid_v4다() {
        let id = generate();
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(text, text.to_lowercase());
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(Uuid::parse_str(&text).unwrap().to_string(), text);
    }

    #[test]
    fn 생성할_때마다_다르다() {
        assert_ne!(generate(), generate());
    }

    #[test]
    fn 범위_밖에서는_값이_없다() {
        assert_eq!(current(), None);
    }

    #[test]
    fn 가드가_살아_있는_동안만_값이_있고_중첩은_이전_값으로_돌아간다() {
        {
            let _outer = enter("outer");
            assert_eq!(current().as_deref(), Some("outer"));
            {
                let _inner = enter("inner");
                assert_eq!(current().as_deref(), Some("inner"));
            }
            assert_eq!(current().as_deref(), Some("outer"));
        }
        assert_eq!(current(), None);
    }

    #[test]
    fn 값은_스레드에_묶인다_mdc와_같다() {
        let _guard = enter("main-thread");
        let seen = std::thread::spawn(current).join().unwrap();
        assert_eq!(seen, None);
    }

    // CurrentRequestId.resolve
    #[test]
    fn resolve_는_uuid_형식이면_그_값을_쓴다() {
        let id = generate();
        let _guard = enter(id);
        assert_eq!(resolve_current(), id);
        let _upper = enter(id.to_string().to_uppercase());
        assert_eq!(resolve_current(), id);
    }

    #[test]
    fn resolve_는_없거나_형식이_다르면_새_uuid() {
        let fresh = resolve_current();
        assert_eq!(fresh.get_version_num(), 4);
        for bad in [
            "request-id-1",
            "",
            "0b9d6f0e2a4b4a5e9f3a1c2d3e4f5a6b",
            "{0b9d6f0e-2a4b-4a5e-9f3a-1c2d3e4f5a6b}",
        ] {
            let _guard = enter(bad);
            let resolved = resolve_current();
            assert_ne!(resolved.to_string(), bad);
            assert_eq!(resolved.get_version_num(), 4);
        }
    }
}
