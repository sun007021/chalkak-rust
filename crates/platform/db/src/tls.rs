//! DB 연결 TLS. 원본 JDBC URL에는 `sslmode`가 없어 pgjdbc 기본값 `prefer`가 적용된다:
//! 서버가 TLS를 받으면 쓰고(`SSLRequest`에 `S`), 아니면 평문으로 이어간다.
//! `prefer`·`require`는 서버 인증서를 검증하지 않는다(pgjdbc `NonValidatingFactory`와 같다).
//! RDS처럼 `rds.force_ssl`이 켜진 서버는 평문을 거절하므로, 이것이 없으면 접속할 수 없다.
//!
//! `postgres::Config`의 `ssl_mode`는 기본이 `Prefer`라서 연결자만 TLS를 할 줄 알면 같은 동작이 된다.

use std::sync::Arc;

use postgres::Client;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

/// 서버 인증서를 검증하지 않는 검증기.
#[derive(Debug)]
struct AcceptAnyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `prefer`용 TLS 연결자.
pub(crate) fn connector() -> MakeRustlsConnect {
    let provider = Arc::new(ring::default_provider());
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("TLS 1.2/1.3 프로토콜을 쓸 수 있어야 한다")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert { provider }))
        .with_no_client_auth();
    MakeRustlsConnect::new(config)
}

/// 서버가 TLS를 받으면 TLS로, 아니면 평문으로 연결한다(pgjdbc `sslmode=prefer`).
pub fn connect(config: &postgres::Config) -> Result<Client, postgres::Error> {
    config.connect(connector())
}
