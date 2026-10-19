//! 원본 `Notification` 엔티티와 `NotificationType`, `NotificationSourceType`, `PublishStatus`.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

const APPROVED_TITLE: &str = "게시물이 승인되었습니다.";
const APPROVED_BODY: &str = "내 사진이 피드에 공개되었습니다.";
const REJECTED_TITLE: &str = "게시물이 반려되었습니다.";
const REJECTED_BODY: &str = "반려 사유를 확인해 주세요.";
const MAX_REJECTION_REASON_LENGTH: usize = 500;
/// 알림함에 보이는 기간(`Notification.RETENTION`, 30일).
const RETENTION: SignedDuration = SignedDuration::from_hours(30 * 24);

/// 알림 종류. DB 문자열이자 JSON 값(Java enum 이름).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationType {
    PostApproved,
    PostRejected,
    /// 오늘의 주제.
    Topic,
    /// 관리자 공지.
    Notice,
}

impl NotificationType {
    pub const fn as_str(self) -> &'static str {
        match self {
            NotificationType::PostApproved => "POST_APPROVED",
            NotificationType::PostRejected => "POST_REJECTED",
            NotificationType::Topic => "TOPIC",
            NotificationType::Notice => "NOTICE",
        }
    }

    pub fn parse(text: &str) -> Option<NotificationType> {
        match text {
            "POST_APPROVED" => Some(NotificationType::PostApproved),
            "POST_REJECTED" => Some(NotificationType::PostRejected),
            "TOPIC" => Some(NotificationType::Topic),
            "NOTICE" => Some(NotificationType::Notice),
            _ => None,
        }
    }
}

/// 알림이 가리키는 대상의 종류(`NotificationSourceType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSourceType {
    Post,
    Topic,
    Notice,
}

impl NotificationSourceType {
    pub const fn as_str(self) -> &'static str {
        match self {
            NotificationSourceType::Post => "POST",
            NotificationSourceType::Topic => "TOPIC",
            NotificationSourceType::Notice => "NOTICE",
        }
    }

    pub fn parse(text: &str) -> Option<NotificationSourceType> {
        match text {
            "POST" => Some(NotificationSourceType::Post),
            "TOPIC" => Some(NotificationSourceType::Topic),
            "NOTICE" => Some(NotificationSourceType::Notice),
            _ => None,
        }
    }
}

/// SQS 발행 상태(`PublishStatus`, 컬럼 `sqs_publish_status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishStatus {
    NotRequired,
    Pending,
    Published,
    Expired,
    Failed,
}

impl PublishStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            PublishStatus::NotRequired => "NOT_REQUIRED",
            PublishStatus::Pending => "PENDING",
            PublishStatus::Published => "PUBLISHED",
            PublishStatus::Expired => "EXPIRED",
            PublishStatus::Failed => "FAILED",
        }
    }

    pub fn parse(text: &str) -> Option<PublishStatus> {
        match text {
            "NOT_REQUIRED" => Some(PublishStatus::NotRequired),
            "PENDING" => Some(PublishStatus::Pending),
            "PUBLISHED" => Some(PublishStatus::Published),
            "EXPIRED" => Some(PublishStatus::Expired),
            "FAILED" => Some(PublishStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub user_id: Uuid,
    pub source_type: Option<NotificationSourceType>,
    pub source_id: Option<Uuid>,
    pub event_key: Uuid,
    pub kind: NotificationType,
    pub title: String,
    pub body: String,
    /// 반려 사유. DB에는 `payload` JSON(`{"rejectionReason": ...}`)으로 저장한다.
    pub rejection_reason: Option<String>,
    pub read_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub publish_status: PublishStatus,
    pub next_attempt_at: Option<Timestamp>,
    pub published_at: Option<Timestamp>,
}

/// `Notification.getRetentionThreshold`: 알림함에 보이는 가장 이른 사건 시각. 정확히 이 시각인 알림은 보인다.
pub fn retention_threshold(now: Timestamp) -> Timestamp {
    now.checked_sub(RETENTION).unwrap_or(Timestamp::MIN)
}

impl Notification {
    /// `Notification.approved`. 필수 값 검증(`validateRequired`)은 타입이 맡는다.
    pub fn approved(
        user_id: Uuid,
        post_id: Uuid,
        event_key: Uuid,
        created_at: Timestamp,
        push_enabled: bool,
    ) -> Notification {
        Notification::new(
            user_id,
            post_id,
            event_key,
            NotificationType::PostApproved,
            (APPROVED_TITLE, APPROVED_BODY),
            None,
            created_at,
            push_enabled,
        )
    }

    /// `Notification.rejected`. 사유가 없거나 공백뿐이면, 500자(코드 포인트)를 넘으면 거절한다. 사유는 그대로(떼지 않고)
    /// 보관한다.
    pub fn rejected(
        user_id: Uuid,
        post_id: Uuid,
        event_key: Uuid,
        rejection_reason: Option<&str>,
        created_at: Timestamp,
        push_enabled: bool,
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
        if reason.chars().count() > MAX_REJECTION_REASON_LENGTH {
            return Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                "반려 사유는 500자 이하여야 합니다.",
            ));
        }
        Ok(Notification::new(
            user_id,
            post_id,
            event_key,
            NotificationType::PostRejected,
            (REJECTED_TITLE, REJECTED_BODY),
            Some(reason.to_owned()),
            created_at,
            push_enabled,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        user_id: Uuid,
        post_id: Uuid,
        event_key: Uuid,
        kind: NotificationType,
        (title, body): (&str, &str),
        rejection_reason: Option<String>,
        created_at: Timestamp,
        push_enabled: bool,
    ) -> Notification {
        // `initializePublication`: 푸시를 받지 않는 회원의 알림은 발행하지 않는다. 받는 회원이면 곧바로 발행 대상이다.
        let (publish_status, next_attempt_at) = if push_enabled {
            (PublishStatus::Pending, Some(created_at))
        } else {
            (PublishStatus::NotRequired, None)
        };
        Notification {
            id: None,
            user_id,
            source_type: Some(NotificationSourceType::Post),
            source_id: Some(post_id),
            event_key,
            kind,
            title: title.to_owned(),
            body: body.to_owned(),
            rejection_reason,
            read_at: None,
            created_at,
            publish_status,
            next_attempt_at,
            published_at: None,
        }
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
        let n = Notification::approved(USER, POST, EVENT, at(), false);
        assert_eq!(n.kind, NotificationType::PostApproved);
        assert_eq!(n.title, "게시물이 승인되었습니다.");
        assert_eq!(n.body, "내 사진이 피드에 공개되었습니다.");
        assert_eq!(n.source_type, Some(NotificationSourceType::Post));
        assert_eq!(n.source_id, Some(POST));
        assert_eq!(n.rejection_reason, None);
        assert_eq!(n.read_at, None);
        assert_eq!(n.created_at, at());
    }

    // Notification.rejected
    #[test]
    fn 반려_알림은_고정_문구와_사유를_가진다() {
        let n =
            Notification::rejected(USER, POST, EVENT, Some(" 사진 품질 "), at(), false).unwrap();
        assert_eq!(n.kind, NotificationType::PostRejected);
        assert_eq!(n.title, "게시물이 반려되었습니다.");
        assert_eq!(n.body, "반려 사유를 확인해 주세요.");
        assert_eq!(n.source_type, Some(NotificationSourceType::Post));
        assert_eq!(n.rejection_reason.as_deref(), Some(" 사진 품질 "));
    }

    #[test]
    fn 반려_알림에_사유가_없으면_거절한다() {
        for reason in [None, Some(""), Some(" \t\n")] {
            let err = Notification::rejected(USER, POST, EVENT, reason, at(), false).unwrap_err();
            assert_eq!(err.message, "반려 알림에는 사유가 필요합니다.");
        }
    }

    #[test]
    fn 반려_사유는_500자까지_허용한다() {
        let ok = "가".repeat(500);
        assert!(Notification::rejected(USER, POST, EVENT, Some(&ok), at(), false).is_ok());
        let too_long = "가".repeat(501);
        let err =
            Notification::rejected(USER, POST, EVENT, Some(&too_long), at(), false).unwrap_err();
        assert_eq!(err.message, "반려 사유는 500자 이하여야 합니다.");
        // 보충 문자는 코드 포인트 하나로 센다(`codePointCount`).
        let emoji = "😀".repeat(500);
        assert!(Notification::rejected(USER, POST, EVENT, Some(&emoji), at(), false).is_ok());
    }

    #[test]
    fn 푸시를_받는_회원의_알림은_발행_대기로_시작한다() {
        let pending = Notification::approved(USER, POST, EVENT, at(), true);
        assert_eq!(pending.publish_status, PublishStatus::Pending);
        assert_eq!(pending.next_attempt_at, Some(at()));
        let skipped = Notification::approved(USER, POST, EVENT, at(), false);
        assert_eq!(skipped.publish_status, PublishStatus::NotRequired);
        assert_eq!(skipped.next_attempt_at, None);
        assert_eq!(skipped.published_at, None);
    }

    #[test]
    fn 알림함_보관_경계는_30일이고_경계_시각도_보인다() {
        let now: Timestamp = "2026-10-30T09:00:00Z".parse().unwrap();
        let threshold = retention_threshold(now);
        assert_eq!(
            threshold,
            "2026-09-30T09:00:00Z".parse::<Timestamp>().unwrap()
        );
    }

    #[test]
    fn 알림_종류는_db_문자열과_왕복한다() {
        for kind in [
            NotificationType::PostApproved,
            NotificationType::PostRejected,
            NotificationType::Topic,
            NotificationType::Notice,
        ] {
            assert_eq!(NotificationType::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(NotificationType::parse("POST_DELETED"), None);
    }

    #[test]
    fn 대상_종류와_발행_상태도_db_문자열과_왕복한다() {
        for kind in [
            NotificationSourceType::Post,
            NotificationSourceType::Topic,
            NotificationSourceType::Notice,
        ] {
            assert_eq!(NotificationSourceType::parse(kind.as_str()), Some(kind));
        }
        for status in [
            PublishStatus::NotRequired,
            PublishStatus::Pending,
            PublishStatus::Published,
            PublishStatus::Expired,
            PublishStatus::Failed,
        ] {
            assert_eq!(PublishStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(PublishStatus::parse("DONE"), None);
    }
}
