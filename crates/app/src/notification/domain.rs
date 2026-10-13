//! 원본 `Notification` 엔티티와 `NotificationType`.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

const APPROVED_TITLE: &str = "게시물이 승인되었습니다.";
const APPROVED_BODY: &str = "내 사진이 피드에 공개되었습니다.";
const REJECTED_TITLE: &str = "게시물이 반려되었습니다.";
const REJECTED_BODY: &str = "반려 사유를 확인해 주세요.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationType {
    PostApproved,
    PostRejected,
}

impl NotificationType {
    /// DB 문자열이자 JSON 값(Java enum 이름).
    pub const fn as_str(self) -> &'static str {
        match self {
            NotificationType::PostApproved => "POST_APPROVED",
            NotificationType::PostRejected => "POST_REJECTED",
        }
    }

    pub fn parse(text: &str) -> Option<NotificationType> {
        match text {
            "POST_APPROVED" => Some(NotificationType::PostApproved),
            "POST_REJECTED" => Some(NotificationType::PostRejected),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub user_id: Uuid,
    pub post_id: Uuid,
    pub event_key: Uuid,
    pub kind: NotificationType,
    pub title: String,
    pub body: String,
    pub rejection_reason: Option<String>,
    pub read_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

impl Notification {
    /// `Notification.approved`. 필수 값 검증(`validateRequired`)은 타입이 맡는다.
    pub fn approved(
        user_id: Uuid,
        post_id: Uuid,
        event_key: Uuid,
        created_at: Timestamp,
    ) -> Notification {
        Notification {
            id: None,
            user_id,
            post_id,
            event_key,
            kind: NotificationType::PostApproved,
            title: APPROVED_TITLE.to_owned(),
            body: APPROVED_BODY.to_owned(),
            rejection_reason: None,
            read_at: None,
            created_at,
        }
    }

    /// `Notification.rejected`. 사유가 없거나 공백뿐이면 거절한다. 사유는 그대로(떼지 않고) 보관한다.
    pub fn rejected(
        user_id: Uuid,
        post_id: Uuid,
        event_key: Uuid,
        rejection_reason: Option<&str>,
        created_at: Timestamp,
    ) -> Result<Notification, AppError> {
        let reason = match rejection_reason {
            // Java `String.isBlank`는 `Character.isWhitespace`로 판정한다.
            Some(r) if !r.chars().all(is_java_whitespace) => r,
            _ => {
                return Err(AppError::business(
                    ErrorCode::BUSINESS_ERROR,
                    "반려 알림에는 사유가 필요합니다.",
                ));
            }
        };
        Ok(Notification {
            id: None,
            user_id,
            post_id,
            event_key,
            kind: NotificationType::PostRejected,
            title: REJECTED_TITLE.to_owned(),
            body: REJECTED_BODY.to_owned(),
            rejection_reason: Some(reason.to_owned()),
            read_at: None,
            created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Uuid = Uuid::from_u128(1);
    const POST: Uuid = Uuid::from_u128(2);
    const EVENT: Uuid = Uuid::from_u128(3);

    fn at() -> Timestamp {
        "2026-09-29T09:00:00Z".parse().unwrap()
    }

    // Notification.approved
    #[test]
    fn 승인_알림은_고정_문구이고_사유가_없다() {
        let n = Notification::approved(USER, POST, EVENT, at());
        assert_eq!(n.kind, NotificationType::PostApproved);
        assert_eq!(n.title, "게시물이 승인되었습니다.");
        assert_eq!(n.body, "내 사진이 피드에 공개되었습니다.");
        assert_eq!(n.rejection_reason, None);
        assert_eq!(n.read_at, None);
        assert_eq!(n.created_at, at());
    }

    // Notification.rejected
    #[test]
    fn 반려_알림은_고정_문구와_사유를_가진다() {
        let n = Notification::rejected(USER, POST, EVENT, Some(" 사진 품질 "), at()).unwrap();
        assert_eq!(n.kind, NotificationType::PostRejected);
        assert_eq!(n.title, "게시물이 반려되었습니다.");
        assert_eq!(n.body, "반려 사유를 확인해 주세요.");
        assert_eq!(n.rejection_reason.as_deref(), Some(" 사진 품질 "));
    }

    #[test]
    fn 반려_알림에_사유가_없으면_거절한다() {
        for reason in [None, Some(""), Some(" \t\n")] {
            let err = Notification::rejected(USER, POST, EVENT, reason, at()).unwrap_err();
            assert_eq!(err.message, "반려 알림에는 사유가 필요합니다.");
        }
    }

    #[test]
    fn 알림_종류는_db_문자열과_왕복한다() {
        for kind in [
            NotificationType::PostApproved,
            NotificationType::PostRejected,
        ] {
            assert_eq!(NotificationType::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(NotificationType::parse("POST_DELETED"), None);
    }
}
