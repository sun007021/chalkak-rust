//! 푸시 사건 ID 정책(원본 `PushEventIdPolicy`).
//!
//! 주제·리마인더처럼 예약 설정으로 회차마다 발행하는 푸시는 회차를 식별하는 값으로 사건 ID를 만든다. 같은 종류·설정·회차는 다시
//! 발행해도 같은 사건 ID를 쓴다(`UUID.nameUUIDFromBytes`: MD5, 버전 3).

use chalkak_aws::sqs::md5_hex;
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::NotificationType;
use crate::web::json::format_instant;

const OCCURRENCE_NAMESPACE: &str = "chalkak:push-occurrence:v1:";

/// `UUID.nameUUIDFromBytes`
fn name_uuid_from_bytes(bytes: &[u8]) -> Uuid {
    let hex = md5_hex(bytes);
    let mut digest = [0u8; 16];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("md5_hex는 소문자 16진수 32자다");
    }
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    Uuid::from_bytes(digest)
}

/// `createForOccurrence`: 실행 시각이나 문구가 달라도 같은 설정의 같은 회차는 같은 사건으로 식별한다.
pub fn create_for_occurrence(
    kind: NotificationType,
    schedule_id: Uuid,
    occurred_at: Timestamp,
) -> Uuid {
    let key = format!(
        "{OCCURRENCE_NAMESPACE}{}:{}:{}",
        kind.as_str(),
        schedule_id.hyphenated(),
        format_instant(occurred_at)
    );
    name_uuid_from_bytes(key.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OCCURRED_AT: &str = "2026-10-26T00:00:00Z";

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    // 원본 PushEventIdPolicyTest: 배포 후에도 같은 회차를 식별하도록 생성 형식을 유지한다.
    #[test]
    fn 알려진_회차의_사건_id_형식을_유지한다() {
        assert_eq!(
            create_for_occurrence(NotificationType::Topic, Uuid::from_u128(1), at(OCCURRED_AT)),
            Uuid::parse_str("aa1144dc-4e9a-37cc-9484-bb1f0e809bd5").unwrap()
        );
    }

    #[test]
    fn 같은_종류_설정_예정_시각은_같은_사건_id를_쓴다() {
        for kind in [NotificationType::Topic, NotificationType::Reminder] {
            assert_eq!(
                create_for_occurrence(kind, Uuid::from_u128(1), at(OCCURRED_AT)),
                create_for_occurrence(kind, Uuid::from_u128(1), at(OCCURRED_AT))
            );
        }
    }

    #[test]
    fn 같은_시각과_id여도_주제와_리마인더는_다른_사건이다() {
        assert_ne!(
            create_for_occurrence(NotificationType::Topic, Uuid::from_u128(1), at(OCCURRED_AT)),
            create_for_occurrence(
                NotificationType::Reminder,
                Uuid::from_u128(1),
                at(OCCURRED_AT)
            )
        );
    }

    #[test]
    fn 설정_id가_다르면_다른_사건이다() {
        assert_ne!(
            create_for_occurrence(NotificationType::Topic, Uuid::from_u128(1), at(OCCURRED_AT)),
            create_for_occurrence(NotificationType::Topic, Uuid::from_u128(2), at(OCCURRED_AT))
        );
    }

    #[test]
    fn 같은_설정이어도_다른_시각과_다음_날_회차는_다른_사건이다() {
        for seconds in [60, 86_400] {
            let other = at(OCCURRED_AT)
                .checked_add(jiff::SignedDuration::from_secs(seconds))
                .unwrap();
            assert_ne!(
                create_for_occurrence(
                    NotificationType::Reminder,
                    Uuid::from_u128(1),
                    at(OCCURRED_AT)
                ),
                create_for_occurrence(NotificationType::Reminder, Uuid::from_u128(1), other)
            );
        }
    }
}
