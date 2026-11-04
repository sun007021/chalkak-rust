//! AI 리뷰 결과 알림(원본 `AiReviewOutcome`, `PushEventIdPolicy.createForAiReview`,
//! `NotificationService.createForAiReview`).

use chalkak_db::{Db, DbError};
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::{Notification, NotificationType};
use super::repository::NotificationRepository;

/// AI 리뷰 결과 알림이 알리는 결과. 알림 종류와 고정 문구를 결정한다. 실패는 다시 요청할 수 있는지로만 나눈다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiReviewOutcome {
    Completed,
    FailedRetryable,
    FailedFinal,
}

impl AiReviewOutcome {
    pub const fn kind(self) -> NotificationType {
        match self {
            AiReviewOutcome::Completed => NotificationType::AiReviewCompleted,
            AiReviewOutcome::FailedRetryable | AiReviewOutcome::FailedFinal => {
                NotificationType::AiReviewFailed
            }
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            AiReviewOutcome::Completed => "AI 피드백이 도착했어요",
            AiReviewOutcome::FailedRetryable | AiReviewOutcome::FailedFinal => {
                "AI 피드백을 만들지 못했어요"
            }
        }
    }

    pub const fn body(self) -> &'static str {
        match self {
            AiReviewOutcome::Completed => "사진의 구도 점수와 피드백을 확인해 보세요.",
            AiReviewOutcome::FailedRetryable => "잠시 후 다시 요청해 주세요.",
            AiReviewOutcome::FailedFinal => "이 사진은 더 이상 AI 피드백을 요청할 수 없어요.",
        }
    }
}

/// `UUID.nameUUIDFromBytes`: MD5 해시에 버전 3과 변형 비트를 넣은 UUID.
pub fn name_uuid_from_bytes(bytes: &[u8]) -> Uuid {
    let hex = chalkak_aws::sqs::md5_hex(bytes);
    let mut digest = [0u8; 16];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("md5_hex는 소문자 16진수 32자다");
    }
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    Uuid::from_bytes(digest)
}

/// `PushEventIdPolicy.createForAiReview`: 같은 리뷰 행을 회차마다 재사용하므로 리뷰 ID와 회차로 사건을 구분한다.
pub fn event_key_for_ai_review(ai_review_id: Uuid, round: i32) -> Uuid {
    let key = format!("AI_REVIEW:{}:{round}", ai_review_id.hyphenated());
    name_uuid_from_bytes(key.as_bytes())
}

/// `NotificationService.createForAiReview`: 열린 트랜잭션 안에서 부른다(`Propagation.MANDATORY`). 같은 리뷰의 같은 회차 알림은
/// 한 번만 들어간다. 탈퇴했거나 AI 리뷰 푸시를 끈 회원의 알림은 알림함에만 남고 발행하지 않는다.
pub fn create_for_ai_review(
    tx: &mut impl Db,
    user_id: Uuid,
    post_id: Uuid,
    ai_review_id: Uuid,
    round: i32,
    outcome: AiReviewOutcome,
    occurred_at: Timestamp,
) -> Result<(), DbError> {
    // `userRepository.findById(userId).orElseThrow()`: 회원이 없으면 오류다.
    let row = tx.query_one(
        "SELECT deleted_at IS NULL AND ai_review_push_enabled AS push_enabled FROM users WHERE id = $1",
        &[&user_id],
    )?;
    let push_enabled: bool = row.try_get("push_enabled")?;
    let notification = Notification::ai_review(
        user_id,
        post_id,
        event_key_for_ai_review(ai_review_id, round),
        outcome,
        occurred_at,
        push_enabled,
    );
    NotificationRepository.insert_if_absent(tx, &notification)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // JDK: UUID.nameUUIDFromBytes("".getBytes()) == d41d8cd9-8f00-3204-a980-0998ecf8427e
    #[test]
    fn name_uuid_matches_jdk_for_empty_input() {
        assert_eq!(
            name_uuid_from_bytes(b"").to_string(),
            "d41d8cd9-8f00-3204-a980-0998ecf8427e"
        );
    }

    // PushEventIdPolicy: 같은 리뷰·회차는 같은 키, 회차나 리뷰가 다르면 다른 키.
    #[test]
    fn event_key_is_stable_per_review_and_round() {
        let review = Uuid::from_u128(7);
        assert_eq!(
            event_key_for_ai_review(review, 1),
            event_key_for_ai_review(review, 1)
        );
        assert_ne!(
            event_key_for_ai_review(review, 1),
            event_key_for_ai_review(review, 2)
        );
        assert_ne!(
            event_key_for_ai_review(review, 1),
            event_key_for_ai_review(Uuid::from_u128(8), 1)
        );
        assert_eq!(event_key_for_ai_review(review, 1).get_version_num(), 3);
    }

    // AiReviewOutcome 문구
    #[test]
    fn outcome_texts_match_original() {
        assert_eq!(AiReviewOutcome::Completed.title(), "AI 피드백이 도착했어요");
        assert_eq!(
            AiReviewOutcome::Completed.body(),
            "사진의 구도 점수와 피드백을 확인해 보세요."
        );
        assert_eq!(
            AiReviewOutcome::FailedRetryable.kind(),
            NotificationType::AiReviewFailed
        );
        assert_eq!(
            AiReviewOutcome::FailedRetryable.body(),
            "잠시 후 다시 요청해 주세요."
        );
        assert_eq!(
            AiReviewOutcome::FailedFinal.body(),
            "이 사진은 더 이상 AI 피드백을 요청할 수 없어요."
        );
    }
}
