//! 검수 대기 알림 연결점. 원본 `PostModerationPendingEvent`와 `@TransactionalEventListener(AFTER_COMMIT)` 구독자에 해당한다.

use jiff::Timestamp;
use uuid::Uuid;

/// 게시물이 검수 대기(`PENDING`)가 된 것을 받는 쪽. 게시물을 저장한 트랜잭션이 **커밋된 뒤에** 호출한다.
/// 구현은 실패해도 요청을 망치지 않아야 한다(원본 Slack 발송은 실패를 로그로만 남긴다).
pub trait PostModerationPendingListener: Send + Sync {
    fn on_pending(&self, post_id: Uuid, occurred_at: Timestamp);
}

/// 아무것도 하지 않는 구독자. 알림 구현이 붙기 전 조립에 쓴다.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopPostModerationPendingListener;

impl PostModerationPendingListener for NoopPostModerationPendingListener {
    fn on_pending(&self, _post_id: Uuid, _occurred_at: Timestamp) {}
}
