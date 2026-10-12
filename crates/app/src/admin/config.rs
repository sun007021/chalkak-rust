//! 관리자 도메인이 쓰는 설정과 이미지 주소 변환(`CloudFrontUrlProvider`).

use std::time::Duration;

use crate::web::binding::is_java_whitespace;

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
    /// `CLOUDFRONT_BASE_URL`
    pub cloudfront_base_url: String,
    /// `CLOUDFRONT_ORIGIN_PATH`
    pub cloudfront_origin_path: String,
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
            cloudfront_base_url: "https://cdn.test.chalkak".to_owned(),
            cloudfront_origin_path: "/chalkak".to_owned(),
        }
    }

    pub fn image_urls(&self) -> ImageUrlProvider {
        ImageUrlProvider {
            base_url: self.cloudfront_base_url.clone(),
            origin_path: self.cloudfront_origin_path.clone(),
        }
    }
}

/// 원본 `CloudFrontUrlProvider`: 스토리지 키를 공개 주소로 바꾼다.
#[derive(Clone, Debug)]
pub struct ImageUrlProvider {
    base_url: String,
    origin_path: String,
}

/// 스토리지 키를 주소로 만들 수 없을 때(원본 `IllegalArgumentException`). 응답은 500이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidStorageKey(pub String);

/// `path.trim().replaceAll("^/+|/+$", "")`
fn normalize_path(path: &str) -> &str {
    path.trim_matches(|c: char| c <= ' ').trim_matches('/')
}

impl ImageUrlProvider {
    pub fn new(base_url: &str, origin_path: &str) -> ImageUrlProvider {
        ImageUrlProvider {
            base_url: base_url.to_owned(),
            origin_path: origin_path.to_owned(),
        }
    }

    /// `getUrl`. 키가 없으면 `None`.
    pub fn url(&self, storage_key: Option<&str>) -> Result<Option<String>, InvalidStorageKey> {
        let Some(storage_key) = storage_key else {
            return Ok(None);
        };
        let normalized = normalize_path(storage_key);
        if normalized.chars().all(is_java_whitespace) {
            return Err(InvalidStorageKey(
                "스토리지 키는 비어 있을 수 없습니다.".to_owned(),
            ));
        }
        let public_path = self.remove_origin_path(normalized)?;
        let mut url = self
            .base_url
            .strip_suffix('/')
            .unwrap_or(&self.base_url)
            .to_owned();
        for segment in public_path
            .split('/')
            .filter(|s| !s.chars().all(is_java_whitespace))
        {
            url.push('/');
            encode_path_segment(segment, &mut url);
        }
        Ok(Some(url))
    }

    fn remove_origin_path<'a>(&self, storage_key: &'a str) -> Result<&'a str, InvalidStorageKey> {
        let origin = normalize_path(&self.origin_path);
        if origin.chars().all(is_java_whitespace) {
            return Ok(storage_key);
        }
        match storage_key
            .strip_prefix(origin)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            Some(rest) => Ok(rest),
            None => Err(InvalidStorageKey(format!(
                "스토리지 키는 원본 경로로 시작해야 합니다: /{origin}"
            ))),
        }
    }
}

/// Spring `HierarchicalUriComponents.Type.PATH_SEGMENT`: unreserved, sub-delims, `:`, `@`만 그대로 둔다.
fn encode_path_segment(segment: &str, out: &mut String) {
    for byte in segment.bytes() {
        let allowed = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
            );
        if allowed {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ImageUrlProvider {
        ImageUrlProvider::new("https://cdn.test.chalkak", "/chalkak")
    }

    // CloudFrontUrlProvider: 하네스(java-a)가 같은 설정으로 돌려준 값이 기준이다.
    #[test]
    fn 원본_경로를_떼고_기본_주소에_붙인다() {
        assert_eq!(
            provider()
                .url(Some("chalkak/signatures/u1/original.png"))
                .unwrap()
                .as_deref(),
            Some("https://cdn.test.chalkak/signatures/u1/original.png")
        );
    }

    #[test]
    fn 키가_없으면_none이다() {
        assert_eq!(provider().url(None).unwrap(), None);
    }

    #[test]
    fn 앞뒤_슬래시와_빈_세그먼트는_정리한다() {
        assert_eq!(
            provider()
                .url(Some(" /chalkak//a///b/ "))
                .unwrap()
                .as_deref(),
            Some("https://cdn.test.chalkak/a/b")
        );
    }

    #[test]
    fn 세그먼트는_퍼센트_인코딩한다() {
        assert_eq!(
            provider()
                .url(Some("chalkak/a b/한글/x@y:z!$&'()*+,;=~.-_?#%"))
                .unwrap()
                .as_deref(),
            Some("https://cdn.test.chalkak/a%20b/%ED%95%9C%EA%B8%80/x@y:z!$&'()*+,;=~.-_%3F%23%25")
        );
    }

    #[test]
    fn 기본_주소의_끝_슬래시는_하나_떼고_붙인다() {
        let p = ImageUrlProvider::new("https://cdn.test.chalkak/", "chalkak");
        assert_eq!(
            p.url(Some("chalkak/x.png")).unwrap().as_deref(),
            Some("https://cdn.test.chalkak/x.png")
        );
    }

    #[test]
    fn 비어_있거나_원본_경로로_시작하지_않으면_오류다() {
        for key in ["", "  ", "/", "///"] {
            assert_eq!(
                provider().url(Some(key)).unwrap_err().0,
                "스토리지 키는 비어 있을 수 없습니다."
            );
        }
        assert_eq!(
            provider().url(Some("other/x.png")).unwrap_err().0,
            "스토리지 키는 원본 경로로 시작해야 합니다: /chalkak"
        );
        // 접두 일치만으로는 부족하다: 뒤에 '/'가 와야 한다.
        assert!(provider().url(Some("chalkakx/y.png")).is_err());
        assert!(provider().url(Some("chalkak")).is_err());
    }

    #[test]
    fn 원본_경로가_비어_있으면_키를_그대로_쓴다() {
        let p = ImageUrlProvider::new("https://cdn.test.chalkak", "");
        assert_eq!(
            p.url(Some("a/b.png")).unwrap().as_deref(),
            Some("https://cdn.test.chalkak/a/b.png")
        );
    }
}
