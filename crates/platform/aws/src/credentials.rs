//! 자격증명과 공급자. 환경변수 → IMDSv2 순서(원본 `DefaultCredentialsProvider`가 EC2에서 거치는 길)로 찾는다.

use std::fmt;

use crate::error::AwsError;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

// 비밀 값이 로그에 새지 않게 Debug에서 가린다.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

pub trait CredentialsProvider: Send + Sync {
    fn credentials(&self) -> Result<Credentials, AwsError>;
}
