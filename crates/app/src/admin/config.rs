//! 관리자 도메인이 쓰는 설정. 이미지 주소 변환은 `photo::url::CloudFrontUrlProvider`를 함께 쓴다.

use std::time::Duration;

/// 서버가 환경에서 읽어 `AppContext`에 넣어 주는 관리자 설정.
#[derive(Clone, Debug)]
pub struct AdminConfig {
    /// 액세스 토큰 HS256 키.
    pub access_token_key: Vec<u8>,
    pub access_token_issuer: String,
    pub access_token_audience: String,
    pub access_token_expiration: Duration,
    /// 리프레시 토큰 비활동 만료(`REFRESH_TOKEN_INACTIVITY_EXPIRATION`).
    pub refresh_token_inactivity: Duration,
    /// 리프레시 토큰 절대 만료(`REFRESH_TOKEN_ABSOLUTE_EXPIRATION`).
    pub refresh_token_absolute: Duration,
    /// `chalkak.admin.authentication.development-bypass-enabled`.
    pub development_bypass: bool,
}

impl AdminConfig {
    /// 테스트·하네스용 기본값. 키는 호출 측이 정한다.
    pub fn for_tests(access_token_key: Vec<u8>) -> AdminConfig {
        AdminConfig {
            access_token_key,
            access_token_issuer: "chalkak-backend".to_owned(),
            access_token_audience: "chalkak-access".to_owned(),
            access_token_expiration: Duration::from_secs(15 * 60),
            refresh_token_inactivity: Duration::from_secs(30 * 24 * 3600),
            refresh_token_absolute: Duration::from_secs(90 * 24 * 3600),
            development_bypass: false,
        }
    }
}
