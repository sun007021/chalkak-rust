//! FCM·OAuth 호출이 쓰는 HTTP 전송. 단위 시험은 이 경계에 가짜를 꽂고, 운영은 `chalkak-http`의 `HttpClient`를 쓴다.

use std::time::Duration;

use chalkak_http::client::{ClientError, HttpClient, Method};

/// 요청 하나. 본문은 항상 POST로 보낸다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExchangeRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 응답 하나. 상태 코드가 4xx·5xx여도 응답이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExchangeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ExchangeResponse {
    /// 이름이 같은(대소문자 무시) 첫 헤더 값. HTTP 스택처럼 앞뒤 공백은 뗀다.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    }
}

/// 응답을 받지 못한 이유. 원본 SDK가 예외 종류로 `ErrorCode`를 정하므로 그 종류만 구별한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExchangeError {
    /// 연결·읽기 시간 초과(`SocketTimeoutException`)
    Timeout,
    /// 호스트 이름을 찾지 못함(`UnknownHostException`)
    UnknownHost,
    /// 그 밖의 연결·입출력 오류
    Other,
}

pub trait HttpExchange: Send + Sync {
    fn execute(&self, request: &ExchangeRequest) -> Result<ExchangeResponse, ExchangeError>;
}

/// `HttpClient`로 보내는 전송.
pub struct ClientExchange {
    client: HttpClient,
}

impl ClientExchange {
    pub fn new(connect_timeout: Duration, read_timeout: Duration) -> Result<Self, String> {
        let client = HttpClient::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
}

impl HttpExchange for ClientExchange {
    fn execute(&self, request: &ExchangeRequest) -> Result<ExchangeResponse, ExchangeError> {
        let headers: Vec<(&str, &str)> = request
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        match self
            .client
            .request(Method::Post, &request.url, &headers, Some(&request.body))
        {
            Ok(response) => Ok(ExchangeResponse {
                status: response.status,
                headers: response.headers,
                body: response.body,
            }),
            Err(ClientError::ConnectTimeout | ClientError::ReadTimeout) => {
                Err(ExchangeError::Timeout)
            }
            Err(ClientError::Resolve(_)) => Err(ExchangeError::UnknownHost),
            Err(_) => Err(ExchangeError::Other),
        }
    }
}
