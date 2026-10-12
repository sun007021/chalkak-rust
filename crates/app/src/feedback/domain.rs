//! 원본 `Feedback` 엔티티의 규칙.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use uuid::Uuid;

use crate::web::binding::is_java_whitespace;

pub const MAX_CONTENT_LENGTH: usize = 1000;

const INVALID_CONTENT_MESSAGE: &str = "피드백 내용이 올바르지 않습니다.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feedback {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub user_id: Uuid,
    pub content: String,
    pub created_at: Option<Timestamp>,
}

/// Java `String.strip()`: `Character.isWhitespace`인 앞뒤 문자를 뗀다.
pub fn java_strip(s: &str) -> &str {
    s.trim_matches(is_java_whitespace)
}

/// `Feedback.isWithinMaxLength`: 앞뒤 공백을 뗀 뒤의 코드 포인트 수가 제한 안인가.
pub fn is_within_max_length(content: &str) -> bool {
    java_strip(content).chars().count() <= MAX_CONTENT_LENGTH
}

impl Feedback {
    /// `Feedback.create`
    pub fn create(user_id: Uuid, content: Option<&str>) -> Result<Feedback, AppError> {
        let invalid = || AppError::business(ErrorCode::BUSINESS_ERROR, INVALID_CONTENT_MESSAGE);
        let content = content.ok_or_else(invalid)?;
        if content.chars().all(is_java_whitespace) || !is_within_max_length(content) {
            return Err(invalid());
        }
        Ok(Feedback {
            id: None,
            user_id,
            content: java_strip(content).to_owned(),
            created_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Uuid = Uuid::from_u128(1);

    #[test]
    fn 앞뒤_공백을_떼어_보관한다() {
        let f = Feedback::create(USER, Some("  좋아요\n")).unwrap();
        assert_eq!(f.content, "좋아요");
    }

    #[test]
    fn 비었거나_공백뿐이면_거절한다() {
        for content in [None, Some(""), Some("  \t\n")] {
            let err = Feedback::create(USER, content).unwrap_err();
            assert_eq!(err.message, "피드백 내용이 올바르지 않습니다.");
        }
    }

    // 길이는 코드 포인트로 센다(이모지 1개 = 1자). 공백을 뗀 뒤 1000자까지 허용.
    #[test]
    fn 길이는_공백을_뗀_코드포인트_수로_잰다() {
        let ok = format!(" {} ", "😀".repeat(1000));
        assert!(Feedback::create(USER, Some(&ok)).is_ok());
        let too_long = "😀".repeat(1001);
        assert!(Feedback::create(USER, Some(&too_long)).is_err());
        assert!(is_within_max_length(&"a".repeat(1000)));
        assert!(!is_within_max_length(&"a".repeat(1001)));
    }
}
