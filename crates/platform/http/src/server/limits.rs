//! 서버 한도.

use std::time::Duration;

/// 요청 크기·시간·연결 수 한도. 기본값은 Tomcat(Spring Boot) 기본값에 가깝게 잡았다.
#[derive(Clone, Debug)]
pub struct Limits {
    /// 요청 줄 + 헤더 전체(끝의 빈 줄 포함) 바이트 상한. 넘으면 400(Tomcat과 같다).
    pub max_header_bytes: usize,
    /// 헤더 줄 개수 상한(트레일러 포함). 넘으면 400.
    pub max_headers: usize,
    /// 디코딩된 본문 바이트 상한. 넘으면 413.
    pub max_body_bytes: usize,
    /// 요청을 읽는 도중 새 바이트가 이만큼 오지 않으면 408로 닫는다. 응답 쓰기 제한 시간이기도 하다.
    pub read_timeout: Duration,
    /// 요청 사이(또는 첫 요청 전) 유휴 연결을 이만큼 두었다가 조용히 닫는다.
    pub keep_alive_timeout: Duration,
    /// 동시에 열어 둘 연결 수(요청을 기다리는 유휴 keep-alive 연결 포함). 넘는 연결은 503으로 즉시 닫는다.
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_header_bytes: 8 * 1024,
            max_headers: 100,
            max_body_bytes: 2 * 1024 * 1024,
            read_timeout: Duration::from_secs(20),
            keep_alive_timeout: Duration::from_secs(60),
            max_connections: 8192,
        }
    }
}
