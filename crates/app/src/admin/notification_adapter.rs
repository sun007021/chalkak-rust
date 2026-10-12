//! 임시 알림 어댑터. 원본 `UserNotificationService.createForModeration`이 하는 일(검수 결과 알림 한 줄 저장)을
//! 알림 모듈(#20)이 들어오기 전까지 관리자 모듈 안에 둔다.
//!
//! 알림 모듈이 같은 일을 하는 함수를 제공하면 이 파일을 지우고 호출만 바꾼다. 시그니처는 그때 바꿔 끼우기 쉽게 원본의
//! 인자(수신자, 게시물, 이벤트 키 = 감사 로그 식별자, 결정, 반려 사유, 발생 시각)를 그대로 따른다.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use postgres::GenericClient;
use uuid::Uuid;

use crate::admin::binding::has_text;
use crate::web::ApiError;

const APPROVED_TITLE: &str = "게시물이 승인되었습니다.";
const APPROVED_BODY: &str = "내 사진이 피드에 공개되었습니다.";
const REJECTED_TITLE: &str = "게시물이 반려되었습니다.";
const REJECTED_BODY: &str = "반려 사유를 확인해 주세요.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModerationDecision {
    Approved,
    Rejected,
}

/// `UserNotificationService.createForModeration` + `Notification.approved/rejected`.
/// 호출한 트랜잭션 안에서 INSERT 한 문장을 실행한다(`Propagation.MANDATORY`).
pub fn create_for_moderation(
    tx: &mut impl GenericClient,
    user_id: Uuid,
    post_id: Uuid,
    event_key: Uuid,
    decision: ModerationDecision,
    rejection_reason: Option<&str>,
    occurred_at: Timestamp,
) -> Result<(), ApiError> {
    let (kind, title, body, reason) = match decision {
        ModerationDecision::Approved => ("POST_APPROVED", APPROVED_TITLE, APPROVED_BODY, None),
        ModerationDecision::Rejected => {
            let Some(reason) = rejection_reason.filter(|r| has_text(r)) else {
                return Err(ApiError::App(AppError::business(
                    ErrorCode::BUSINESS_ERROR,
                    "반려 알림에는 사유가 필요합니다.",
                )));
            };
            ("POST_REJECTED", REJECTED_TITLE, REJECTED_BODY, Some(reason))
        }
    };
    tx.execute(
        "INSERT INTO notifications (user_id, post_id, event_key, type, title, body, rejection_reason, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        &[&user_id, &post_id, &event_key, &kind, &title, &body, &reason, &occurred_at],
    )
    .map_err(chalkak_db::DbError::from)?;
    Ok(())
}
