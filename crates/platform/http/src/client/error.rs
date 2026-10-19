//! 클라이언트 오류.

use std::fmt;
use std::io;

/// 요청 하나가 실패한 이유.
#[derive(Debug)]
pub enum ClientError {
    /// URL을 해석할 수 없다(스킴·호스트·포트 누락 등).
    InvalidUrl(String),
    /// `http`, `https` 외의 스킴.
    UnsupportedScheme(String),
    /// 헤더 이름·값에 허용되지 않는 문자(CR, LF 등)가 있다.
    InvalidHeader(String),
    /// 연결 시간 초과.
    ConnectTimeout,
    /// 읽기(또는 쓰기) 시간 초과.
    ReadTimeout,
    /// 호스트 이름을 주소로 바꾸지 못했다(DNS 실패).
    Resolve(io::Error),
    /// 연결 거부 등 연결 단계 오류.
    Connect(io::Error),
    /// TLS 핸드셰이크·인증서 검증 실패.
    Tls(String),
    /// 그 밖의 입출력 오류.
    Io(io::Error),
    /// 응답이 HTTP/1.1 형식에 맞지 않는다(본문이 중간에 끊긴 경우 포함).
    Protocol(String),
    /// 응답 본문이 설정한 상한을 넘었다.
    BodyTooLarge { limit: usize },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(m) => write!(f, "잘못된 URL: {m}"),
            Self::UnsupportedScheme(s) => write!(f, "지원하지 않는 스킴: {s}"),
            Self::InvalidHeader(m) => write!(f, "잘못된 헤더: {m}"),
            Self::ConnectTimeout => write!(f, "연결 시간 초과"),
            Self::ReadTimeout => write!(f, "읽기 시간 초과"),
            Self::Resolve(e) => write!(f, "이름 해석 실패: {e}"),
            Self::Connect(e) => write!(f, "연결 실패: {e}"),
            Self::Tls(m) => write!(f, "TLS 오류: {m}"),
            Self::Io(e) => write!(f, "입출력 오류: {e}"),
            Self::Protocol(m) => write!(f, "HTTP 프로토콜 오류: {m}"),
            Self::BodyTooLarge { limit } => write!(f, "응답 본문이 상한({limit}바이트)을 넘었다"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resolve(e) | Self::Connect(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// 읽기·쓰기 중 난 `io::Error`를 시간 초과, TLS 오류, 일반 오류로 나눈다.
pub(crate) fn from_io(e: io::Error) -> ClientError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ClientError::ReadTimeout,
        io::ErrorKind::UnexpectedEof => ClientError::Protocol("연결이 응답 도중 끊겼다".into()),
        _ => match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
            Some(tls) => ClientError::Tls(tls.to_string()),
            None => ClientError::Io(e),
        },
    }
}
