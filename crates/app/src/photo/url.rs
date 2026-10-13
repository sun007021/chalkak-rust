//! 원본 `ImageUrlProvider`/`CloudFrontUrlProvider`. 스토리지 키를 공개 CDN 주소로 바꾼다.

/// 스토리지 키가 잘못돼 주소를 만들 수 없을 때. 원본은 `IllegalArgumentException`이라 응답은 500이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidStorageKey(pub String);

impl std::fmt::Display for InvalidStorageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidStorageKey {}

/// 원본 `ImageUrlProvider`. 알림함처럼 구현을 바꿔 끼워 시험해야 하는 쪽이 이 인터페이스로 받는다.
pub trait ImageUrlProvider: Send + Sync {
    /// `getUrl`: 키가 없으면 `None`.
    fn get_url(&self, storage_key: Option<&str>) -> Result<Option<String>, InvalidStorageKey>;
}

impl ImageUrlProvider for CloudFrontUrlProvider {
    fn get_url(&self, storage_key: Option<&str>) -> Result<Option<String>, InvalidStorageKey> {
        CloudFrontUrlProvider::get_url(self, storage_key)
    }
}

#[derive(Clone, Debug)]
pub struct CloudFrontUrlProvider {
    /// 스킴·권한 부분(`https://cdn.example.com`). 기준 주소 자체의 경로는 `base_path`에 따로 둔다.
    origin: String,
    base_path: String,
    origin_path: String,
}

impl CloudFrontUrlProvider {
    /// `base_url`은 `scheme://authority[/path]` 형태여야 한다(`URI.create`로 만든 값과 같다).
    pub fn new(base_url: &str, origin_path: &str) -> CloudFrontUrlProvider {
        // 질의·조각은 `UriComponentsBuilder.fromUri(...).pathSegment(...)`로 만든 주소에 그대로 붙지만,
        // 설정값에 쓰는 일이 없어 다루지 않는다.
        let (origin, base_path) = match base_url.split_once("://") {
            Some((scheme, rest)) => match rest.find('/') {
                Some(i) => (format!("{scheme}://{}", &rest[..i]), rest[i..].to_owned()),
                None => (base_url.to_owned(), String::new()),
            },
            None => (base_url.to_owned(), String::new()),
        };
        CloudFrontUrlProvider {
            origin,
            base_path,
            origin_path: origin_path.to_owned(),
        }
    }

    /// `getUrl`: 키가 없으면 `None`. 비어 있거나 원본 경로로 시작하지 않으면 오류.
    pub fn get_url(&self, storage_key: Option<&str>) -> Result<Option<String>, InvalidStorageKey> {
        let Some(storage_key) = storage_key else {
            return Ok(None);
        };
        let normalized = normalize_path(storage_key);
        if normalized.chars().all(is_java_whitespace_char) {
            return Err(InvalidStorageKey(
                "스토리지 키는 비어 있을 수 없습니다.".to_owned(),
            ));
        }
        let public_path = self.remove_origin_path(normalized)?;
        let mut url = format!("{}{}", self.origin, self.base_path);
        for segment in public_path
            .split('/')
            .filter(|s| !s.chars().all(is_java_whitespace_char))
        {
            url.push('/');
            url.push_str(&encode_path_segment(segment));
        }
        Ok(Some(url))
    }

    fn remove_origin_path<'a>(&self, storage_key: &'a str) -> Result<&'a str, InvalidStorageKey> {
        let origin = normalize_path(&self.origin_path);
        if origin.chars().all(is_java_whitespace_char) {
            return Ok(storage_key);
        }
        match storage_key
            .strip_prefix(origin)
            .and_then(|r| r.strip_prefix('/'))
        {
            Some(rest) => Ok(rest),
            None => Err(InvalidStorageKey(format!(
                "스토리지 키는 원본 경로로 시작해야 합니다: /{origin}"
            ))),
        }
    }
}

fn is_java_whitespace_char(c: char) -> bool {
    crate::web::binding::is_java_whitespace(c)
}

/// `path.trim().replaceAll("^/+|/+$", "")`. Java `String.trim`은 U+0020 이하 문자를 뗀다.
fn normalize_path(path: &str) -> &str {
    path.trim_matches(|c: char| c <= ' ').trim_matches('/')
}

/// Spring `HierarchicalUriComponents`의 PATH_SEGMENT 인코딩: 비예약 문자와 하위 구분자, `:`, `@`만 그대로 둔다.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        let keep = byte.is_ascii_alphanumeric()
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
        if keep {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> CloudFrontUrlProvider {
        CloudFrontUrlProvider::new("https://cdn.example.com", "/chalkak")
    }

    // CloudFrontUrlProviderTest#getUrl_storageKeyWithOriginPath_returnsCloudFrontUrl
    #[test]
    fn get_url_storage_key_with_origin_path_returns_cloud_front_url() {
        let url = provider().get_url(Some("chalkak/dev/posts/photo-id/original.jpg"));
        assert_eq!(
            url.unwrap().as_deref(),
            Some("https://cdn.example.com/dev/posts/photo-id/original.jpg")
        );
    }

    // CloudFrontUrlProviderTest#getUrl_storageKeyWithoutOriginPath_throwsIllegalArgumentException
    #[test]
    fn get_url_storage_key_without_origin_path_throws_illegal_argument_exception() {
        let err = provider()
            .get_url(Some("dev/posts/photo-id/thumbnail.jpg"))
            .unwrap_err();
        assert_eq!(err.0, "스토리지 키는 원본 경로로 시작해야 합니다: /chalkak");
    }

    // CloudFrontUrlProviderTest#getUrl_koreanAndSpaceStorageKey_returnsEncodedUrl
    #[test]
    fn get_url_korean_and_space_storage_key_returns_encoded_url() {
        let url = provider().get_url(Some("chalkak/posts/original/테스트 사진.webp"));
        assert_eq!(
            url.unwrap().as_deref(),
            Some(
                "https://cdn.example.com/posts/original/%ED%85%8C%EC%8A%A4%ED%8A%B8%20%EC%82%AC%EC%A7%84.webp"
            )
        );
    }

    // CloudFrontUrlProviderTest#getUrl_nullStorageKey_returnsNull
    #[test]
    fn get_url_null_storage_key_returns_null() {
        assert_eq!(provider().get_url(None).unwrap(), None);
    }

    // CloudFrontUrlProviderTest#getUrl_blankStorageKey_throwsIllegalArgumentException
    #[test]
    fn get_url_blank_storage_key_throws_illegal_argument_exception() {
        let err = provider().get_url(Some(" / ")).unwrap_err();
        assert_eq!(err.0, "스토리지 키는 비어 있을 수 없습니다.");
    }

    #[test]
    fn 원본_경로가_비어_있으면_키를_그대로_쓰고_빈_구간은_건너뛴다() {
        let p = CloudFrontUrlProvider::new("https://cdn.example.com", "");
        assert_eq!(
            p.get_url(Some("/a//b/c.webp")).unwrap().as_deref(),
            Some("https://cdn.example.com/a/b/c.webp")
        );
    }
}
