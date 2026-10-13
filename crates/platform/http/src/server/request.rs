//! 핸들러가 받는 요청.

use std::net::{Ipv4Addr, SocketAddr};

use super::headers::Headers;
use super::method::Method;

/// 파싱이 끝난 요청. 본문은 `Content-Length`·`chunked` 모두 디코딩된 바이트다.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    /// 퍼센트 디코딩된 경로(쿼리 제외). 끝 슬래시를 그대로 보존한다.
    pub path: String,
    /// 디코딩 전 경로(쿼리 제외). Tomcat `getRequestURI()`에 해당한다.
    pub raw_path: String,
    /// 디코딩된 쿼리. 같은 이름이 여러 번 나오면 순서대로 모두 남는다.
    pub query: Vec<(String, String)>,
    pub headers: Headers,
    pub body: Vec<u8>,
    pub remote_addr: SocketAddr,
    /// 라우터가 채우는 경로 변수(`/posts/{postId}` → `("postId", "7")`).
    /// 계약(implementation-design.md 3장)에 없던 필드다: `Handler::handle`이 `Request`만 받으므로
    /// 경로 변수를 넘길 자리가 필요해 추가했다.
    pub params: Vec<(String, String)>,
}

impl Request {
    /// 테스트·미들웨어 합성용 간이 생성자. `remote_addr`은 `0.0.0.0:0`.
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        let path = path.into();
        Request {
            method,
            raw_path: path.clone(),
            path,
            query: Vec::new(),
            headers: Headers::new(),
            body: Vec::new(),
            remote_addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            params: Vec::new(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    /// 첫 번째 쿼리 값.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// 같은 이름의 모든 쿼리 값.
    pub fn query_params<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// 경로 변수.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}
