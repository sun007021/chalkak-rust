use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwsError {
    /// 잘못된 인자(빈 버킷·키, 허용 범위를 넘는 만료, 해석할 수 없는 엔드포인트 등).
    InvalidInput(String),
    /// 어느 공급자에서도 자격증명을 찾지 못했다.
    NoCredentials(String),
    /// HTTP 호출 자체가 실패했다(연결·타임아웃).
    Transport(String),
    /// IMDS가 오류 상태를 돌려줬다.
    Imds { status: u16, message: String },
    /// 응답을 해석하지 못했다.
    Parse(String),
}

impl fmt::Display for AwsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "invalid input: {m}"),
            Self::NoCredentials(m) => write!(f, "no AWS credentials: {m}"),
            Self::Transport(m) => write!(f, "transport error: {m}"),
            Self::Imds { status, message } => write!(f, "IMDS returned {status}: {message}"),
            Self::Parse(m) => write!(f, "parse error: {m}"),
        }
    }
}

impl std::error::Error for AwsError {}
