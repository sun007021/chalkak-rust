//! 외부 호출용 HTTP/1.1 클라이언트(동기). `https`는 rustls(ring) + webpki-roots, `http`도 지원한다(하네스 fakes, IMDS).
//!
//! 원본 기준(Spring `RestClient` + `JdkClientHttpRequestFactory`):
//! - 리다이렉트를 따르지 않는다(`followRedirects(NEVER)`).
//! - 기본 연결 2초·읽기 5초(`AppleTokenClientConfig`). Slack은 설정값(`chalkak.admin.notification.slack`, 기본 2초/3초)을
//!   [`HttpClientBuilder`]로 넘긴다. Nimbus JWKS 조회는 라이브러리 기본값(연결·읽기 각 500ms, [`JWKS_TIMEOUT`])이다.
//! - `RestClient.retrieve()`는 4xx·5xx에서 예외를 던지지만, 이 클라이언트는 상태코드와 무관하게 응답을 돌려주고
//!   판단은 호출 측이 한다(Apple 오류 본문을 읽어야 하므로).
//!
//! 연결은 요청마다 새로 맺고(`Connection: close`) 풀링하지 않는다. 호출량이 적고 원본도 대부분 단발 호출이다.
//! 프록시(`HTTPS_PROXY`)는 지원하지 않는다.

mod error;
mod response;
mod url;

use std::io::{BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

pub use error::ClientError;
pub use response::ClientResponse;
use url::Url;

/// 기본 연결 제한 시간(원본 `AppleTokenClientConfig.CONNECT_TIMEOUT`).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// 기본 읽기 제한 시간(원본 `AppleTokenClientConfig.READ_TIMEOUT`).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Nimbus `JWKSourceBuilder.DEFAULT_HTTP_CONNECT_TIMEOUT`·`DEFAULT_HTTP_READ_TIMEOUT` (OIDC JWKS 조회).
pub const JWKS_TIMEOUT: Duration = Duration::from_millis(500);
/// 기본 응답 본문 상한.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// [`HttpClient`] 설정.
pub struct HttpClientBuilder {
    connect_timeout: Duration,
    read_timeout: Duration,
    max_response_bytes: usize,
    extra_roots: Vec<CertificateDer<'static>>,
    tls_config: Option<Arc<ClientConfig>>,
}

impl Default for HttpClientBuilder {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            extra_roots: Vec::new(),
            tls_config: None,
        }
    }
}

impl HttpClientBuilder {
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }

    /// 소켓 읽기·쓰기 한 번에 허용하는 무응답 시간.
    pub fn read_timeout(mut self, d: Duration) -> Self {
        self.read_timeout = d;
        self
    }

    pub fn max_response_bytes(mut self, n: usize) -> Self {
        self.max_response_bytes = n;
        self
    }

    /// webpki-roots 외에 신뢰할 루트 인증서(DER)를 더한다(사설 CA, 테스트 서버).
    pub fn add_root_certificate_der(mut self, der: Vec<u8>) -> Self {
        self.extra_roots.push(CertificateDer::from(der));
        self
    }

    /// PEM 텍스트의 인증서 전부를 루트로 더한다.
    pub fn add_root_certificates_pem(mut self, pem: &[u8]) -> Result<Self, ClientError> {
        let mut found = false;
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|e| ClientError::Tls(format!("PEM 인증서 해석 실패: {e}")))?;
            self.extra_roots.push(cert.into_owned());
            found = true;
        }
        if !found {
            return Err(ClientError::Tls("PEM에 인증서가 없다".into()));
        }
        Ok(self)
    }

    /// 미리 만든 rustls 설정을 그대로 쓴다. 루트 인증서 설정은 무시된다.
    pub fn tls_config(mut self, config: Arc<ClientConfig>) -> Self {
        self.tls_config = Some(config);
        self
    }

    pub fn build(self) -> Result<HttpClient, ClientError> {
        let tls = match self.tls_config {
            Some(c) => c,
            None => Arc::new(tls_config(&self.extra_roots)?),
        };
        Ok(HttpClient {
            tls,
            connect_timeout: self.connect_timeout,
            read_timeout: self.read_timeout,
            max_response_bytes: self.max_response_bytes,
        })
    }
}

/// webpki-roots(Mozilla 루트)에 `extra`를 더한 rustls 클라이언트 설정. TLS 1.2·1.3, ring provider.
pub fn tls_config(extra: &[CertificateDer<'static>]) -> Result<ClientConfig, ClientError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for der in extra {
        roots
            .add(der.clone())
            .map_err(|e| ClientError::Tls(format!("루트 인증서 추가 실패: {e}")))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ClientError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

/// 동기 HTTP 클라이언트. 복제 비용이 작고 스레드 사이에서 공유할 수 있다.
#[derive(Clone)]
pub struct HttpClient {
    tls: Arc<ClientConfig>,
    connect_timeout: Duration,
    read_timeout: Duration,
    max_response_bytes: usize,
}

impl HttpClient {
    /// 기본값(연결 2초, 읽기 5초)으로 만든다.
    pub fn new() -> Result<Self, ClientError> {
        HttpClientBuilder::default().build()
    }

    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }

    /// 요청 하나를 보내고 응답을 읽는다. 상태코드가 4xx·5xx여도 `Ok`로 돌려준다.
    ///
    /// `Host`, `Content-Length`, `Connection`, `Transfer-Encoding`은 클라이언트가 정하므로 `headers`에서 무시한다.
    pub fn request(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<ClientResponse, ClientError> {
        let url = Url::parse(url)?;
        let head = build_request_head(method, &url, headers, body)?;
        let tcp = self.connect(&url)?;
        if url.https {
            let name = ServerName::try_from(url.host.clone()).map_err(|_| {
                ClientError::InvalidUrl(format!("TLS 서버 이름으로 쓸 수 없다: {}", url.host))
            })?;
            let conn = ClientConnection::new(self.tls.clone(), name)
                .map_err(|e| ClientError::Tls(e.to_string()))?;
            self.exchange(StreamOwned::new(conn, tcp), &head, body, method)
        } else {
            self.exchange(tcp, &head, body, method)
        }
    }

    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<ClientResponse, ClientError> {
        self.request(Method::Get, url, headers, None)
    }

    pub fn head(&self, url: &str, headers: &[(&str, &str)]) -> Result<ClientResponse, ClientError> {
        self.request(Method::Head, url, headers, None)
    }

    /// `application/x-www-form-urlencoded` 본문(Apple 토큰 교환·철회). 인코딩은 Java `URLEncoder`와 같다.
    pub fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        fields: &[(&str, &str)],
    ) -> Result<ClientResponse, ClientError> {
        let body = form_encode(fields);
        let mut all = vec![("Content-Type", "application/x-www-form-urlencoded")];
        all.extend_from_slice(headers);
        self.request(Method::Post, url, &all, Some(body.as_bytes()))
    }

    /// 직렬화된 JSON 본문(Slack 웹훅).
    pub fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        json: &str,
    ) -> Result<ClientResponse, ClientError> {
        let mut all = vec![("Content-Type", "application/json")];
        all.extend_from_slice(headers);
        self.request(Method::Post, url, &all, Some(json.as_bytes()))
    }

    /// 본문 없는 PUT(IMDSv2 토큰 발급)과 일반 PUT.
    pub fn put(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<ClientResponse, ClientError> {
        self.request(Method::Put, url, headers, body)
    }

    fn connect(&self, url: &Url) -> Result<TcpStream, ClientError> {
        let deadline = Instant::now() + self.connect_timeout;
        let addrs = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(ClientError::Connect)?;
        let mut last: Option<ClientError> = None;
        for addr in addrs {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::ConnectTimeout);
            }
            match TcpStream::connect_timeout(&addr, remaining) {
                Ok(s) => {
                    s.set_read_timeout(Some(self.read_timeout))
                        .map_err(ClientError::Io)?;
                    s.set_write_timeout(Some(self.read_timeout))
                        .map_err(ClientError::Io)?;
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    last = Some(ClientError::ConnectTimeout);
                }
                Err(e) => last = Some(ClientError::Connect(e)),
            }
        }
        Err(last
            .unwrap_or_else(|| ClientError::Connect(std::io::Error::other("해석된 주소가 없다"))))
    }

    fn exchange<S: std::io::Read + Write>(
        &self,
        mut stream: S,
        head: &str,
        body: Option<&[u8]>,
        method: Method,
    ) -> Result<ClientResponse, ClientError> {
        stream.write_all(head.as_bytes()).map_err(error::from_io)?;
        if let Some(b) = body {
            stream.write_all(b).map_err(error::from_io)?;
        }
        stream.flush().map_err(error::from_io)?;
        let mut reader = BufReader::new(stream);
        response::read_response(&mut reader, method == Method::Head, self.max_response_bytes)
    }
}

fn build_request_head(
    method: Method,
    url: &Url,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<String, ClientError> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n",
        method.as_str(),
        url.path_and_query,
        url.host_header()
    );
    for (name, value) in headers {
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if !valid_name {
            return Err(ClientError::InvalidHeader(format!("이름: {name:?}")));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(ClientError::InvalidHeader(format!("{name} 값에 제어 문자")));
        }
        const MANAGED: [&str; 4] = ["host", "content-length", "connection", "transfer-encoding"];
        if MANAGED.iter().any(|m| name.eq_ignore_ascii_case(m)) {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    match body {
        Some(b) => head.push_str(&format!("Content-Length: {}\r\n", b.len())),
        None if matches!(method, Method::Post | Method::Put) => {
            head.push_str("Content-Length: 0\r\n")
        }
        None => {}
    }
    head.push_str("\r\n");
    Ok(head)
}

/// `application/x-www-form-urlencoded` 인코딩(Java `URLEncoder`: `A-Za-z0-9.-*_` 유지, 공백은 `+`, 나머지 UTF-8 `%XX`).
pub fn form_encode(fields: &[(&str, &str)]) -> String {
    fn enc(s: &str, out: &mut String) {
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        enc(k, &mut out);
        out.push('=');
        enc(v, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_인코딩은_java_url_encoder와_같다() {
        assert_eq!(
            form_encode(&[("a b", "c&d=e"), ("k", "한")]),
            "a+b=c%26d%3De&k=%ED%95%9C"
        );
        assert_eq!(form_encode(&[("x", "A-z_0.*~")]), "x=A-z_0.*%7E");
        assert_eq!(form_encode(&[]), "");
    }

    #[test]
    fn 요청_헤드에_관리_헤더와_길이가_들어간다() {
        let url = Url::parse("http://h:81/p?q=1").unwrap();
        let head = build_request_head(
            Method::Post,
            &url,
            &[("X-A", "1"), ("host", "evil"), ("Content-Length", "9")],
            Some(b"abc"),
        )
        .unwrap();
        assert!(head.starts_with("POST /p?q=1 HTTP/1.1\r\nHost: h:81\r\n"));
        assert!(head.contains("X-A: 1\r\n") && head.contains("Content-Length: 3\r\n"));
        assert!(!head.contains("evil") && !head.contains("Content-Length: 9"));
        let put = build_request_head(Method::Put, &url, &[], None).unwrap();
        assert!(put.contains("Content-Length: 0\r\n"));
        let get = build_request_head(Method::Get, &url, &[], None).unwrap();
        assert!(!get.contains("Content-Length"));
    }

    #[test]
    fn 헤더_주입을_막는다() {
        let url = Url::parse("http://h/").unwrap();
        assert!(matches!(
            build_request_head(Method::Get, &url, &[("X", "a\r\nB: c")], None),
            Err(ClientError::InvalidHeader(_))
        ));
        assert!(matches!(
            build_request_head(Method::Get, &url, &[("X Y", "a")], None),
            Err(ClientError::InvalidHeader(_))
        ));
    }

    #[test]
    fn tls_설정은_webpki_루트로_만들어진다() {
        let cfg = tls_config(&[]).unwrap();
        assert!(cfg.alpn_protocols.is_empty());
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    }

    #[test]
    fn 잘못된_루트_인증서를_거부한다() {
        assert!(matches!(
            tls_config(&[CertificateDer::from(vec![1, 2, 3])]),
            Err(ClientError::Tls(_))
        ));
        assert!(matches!(
            HttpClient::builder().add_root_certificates_pem(b"not pem"),
            Err(ClientError::Tls(_))
        ));
        assert!(
            HttpClient::builder()
                .add_root_certificate_der(vec![9])
                .build()
                .is_err()
        );
    }

    #[test]
    fn 기본값은_원본_apple_설정이다() {
        assert_eq!(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs(2));
        assert_eq!(DEFAULT_READ_TIMEOUT, Duration::from_secs(5));
    }
}
