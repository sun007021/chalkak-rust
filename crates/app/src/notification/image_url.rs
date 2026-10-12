//! 저장소 키 -> 공개 이미지 URL. 원본 `ImageUrlProvider`와 그 구현 `CloudFrontUrlProvider`.
//!
//! 알림함이 사진 썸네일·원본 URL을 만들 때 쓴다. 사진 도메인이 같은 변환을 필요로 하면 이 모듈의 것을 쓰거나 합친다.

use std::fmt;

use crate::web::ApiError;
use crate::web::binding::is_java_whitespace;

/// 키를 URL로 바꿀 수 없다(원본 `IllegalArgumentException`). 처리되지 않은 예외라 500이 된다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUrlError(pub String);

impl fmt::Display for ImageUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImageUrlError {}

impl From<ImageUrlError> for ApiError {
    fn from(e: ImageUrlError) -> Self {
        ApiError::Internal(e.0)
    }
}

pub trait ImageUrlProvider: Send + Sync {
    /// `getUrl`: 키가 없으면 `None`.
    fn url(&self, storage_key: Option<&str>) -> Result<Option<String>, ImageUrlError>;
}

/// `CloudFrontUrlProvider`
#[derive(Clone, Debug)]
pub struct CloudFrontUrlProvider {
    /// `scheme://authority`
    origin: String,
    /// 기준 주소에 이미 있는 경로(없으면 빈 문자열).
    base_path: String,
    /// 기준 주소의 `?query`와 `#fragment`(그대로 뒤에 붙는다).
    tail: String,
    origin_path: String,
}

impl CloudFrontUrlProvider {
    /// `base_url`은 `CLOUDFRONT_BASE_URL`, `origin_path`는 `CLOUDFRONT_ORIGIN_PATH`.
    pub fn new(base_url: &str, origin_path: &str) -> Result<Self, ImageUrlError> {
        let invalid = || {
            ImageUrlError(format!(
                "CloudFront 기준 주소가 올바르지 않습니다: {base_url}"
            ))
        };
        let (scheme, rest) = base_url.split_once("://").ok_or_else(invalid)?;
        if scheme.is_empty() {
            return Err(invalid());
        }
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, after) = rest.split_at(authority_end);
        if authority.is_empty() {
            return Err(invalid());
        }
        let tail_start = after.find(['?', '#']).unwrap_or(after.len());
        Ok(CloudFrontUrlProvider {
            origin: format!("{scheme}://{authority}"),
            base_path: after[..tail_start].to_owned(),
            tail: after[tail_start..].to_owned(),
            origin_path: origin_path.to_owned(),
        })
    }

    fn remove_origin_path(&self, storage_key: &str) -> Result<String, ImageUrlError> {
        let origin = normalize_path(&self.origin_path);
        if is_blank(&origin) {
            return Ok(storage_key.to_owned());
        }
        match storage_key.strip_prefix(&format!("{origin}/")) {
            Some(rest) => Ok(rest.to_owned()),
            None => Err(ImageUrlError(format!(
                "스토리지 키는 원본 경로로 시작해야 합니다: /{origin}"
            ))),
        }
    }
}

impl ImageUrlProvider for CloudFrontUrlProvider {
    fn url(&self, storage_key: Option<&str>) -> Result<Option<String>, ImageUrlError> {
        let Some(storage_key) = storage_key else {
            return Ok(None);
        };
        let normalized = normalize_path(storage_key);
        if is_blank(&normalized) {
            return Err(ImageUrlError(
                "스토리지 키는 비어 있을 수 없습니다.".to_owned(),
            ));
        }
        let public_path = self.remove_origin_path(&normalized)?;
        let mut url = format!("{}{}", self.origin, self.base_path);
        for segment in public_path.split('/').filter(|s| !is_blank(s)) {
            url.push('/');
            encode_path_segment(segment, &mut url);
        }
        url.push_str(&self.tail);
        Ok(Some(url))
    }
}

fn is_blank(s: &str) -> bool {
    s.chars().all(is_java_whitespace)
}

/// `path.trim().replaceAll("^/+|/+$", "")`. Java `trim`은 U+0020 이하 문자를 뗀다.
fn normalize_path(path: &str) -> String {
    path.trim_matches(|c: char| c <= ' ')
        .trim_matches('/')
        .to_owned()
}

/// Spring `HierarchicalUriComponents`의 경로 세그먼트 인코딩: unreserved, sub-delims, `:`, `@`만 그대로 둔다.
fn encode_path_segment(segment: &str, out: &mut String) {
    for &b in segment.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
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
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base: &str, origin: &str) -> CloudFrontUrlProvider {
        CloudFrontUrlProvider::new(base, origin).unwrap()
    }

    fn url(p: &CloudFrontUrlProvider, key: &str) -> String {
        p.url(Some(key)).unwrap().unwrap()
    }

    // CloudFrontUrlProvider#getUrl
    #[test]
    fn 원본_경로를_떼고_기준_주소_뒤에_붙인다() {
        let p = provider("https://cdn.test.chalkak", "/chalkak");
        assert_eq!(
            url(&p, "chalkak/posts/a/original.webp"),
            "https://cdn.test.chalkak/posts/a/original.webp"
        );
        assert_eq!(
            url(&p, "/chalkak/posts/a/original.webp/"),
            "https://cdn.test.chalkak/posts/a/original.webp"
        );
    }

    #[test]
    fn 키가_없으면_url도_없다() {
        assert_eq!(
            provider("https://cdn.test.chalkak", "/chalkak").url(None),
            Ok(None)
        );
    }

    #[test]
    fn 원본_경로로_시작하지_않으면_오류다() {
        let p = provider("https://cdn.test.chalkak", "/chalkak");
        let err = p.url(Some("other/posts/a.webp")).unwrap_err();
        assert_eq!(err.0, "스토리지 키는 원본 경로로 시작해야 합니다: /chalkak");
        // 경로 경계까지 맞아야 한다.
        assert!(p.url(Some("chalkak-x/a.webp")).is_err());
    }

    #[test]
    fn 빈_키는_오류다() {
        let p = provider("https://cdn.test.chalkak", "/chalkak");
        for key in ["", "  ", "/", " / "] {
            assert_eq!(
                p.url(Some(key)).unwrap_err().0,
                "스토리지 키는 비어 있을 수 없습니다.",
                "{key:?}"
            );
        }
    }

    #[test]
    fn 원본_경로가_비면_키_전체를_쓴다() {
        let p = provider("https://cdn.test.chalkak", "/");
        assert_eq!(url(&p, "a/b.webp"), "https://cdn.test.chalkak/a/b.webp");
    }

    #[test]
    fn 세그먼트는_스프링처럼_퍼센트_인코딩한다() {
        let p = provider("http://localhost:4566", "chalkak");
        assert_eq!(
            url(&p, "chalkak/a b/한글.webp/x%y/q?z#w/s;t=1,2:3@4"),
            "http://localhost:4566/a%20b/%ED%95%9C%EA%B8%80.webp/x%25y/q%3Fz%23w/s;t=1,2:3@4"
        );
    }

    #[test]
    fn 빈_세그먼트는_버린다() {
        let p = provider("https://cdn.test.chalkak", "chalkak");
        assert_eq!(
            url(&p, "chalkak/a//b/ /c"),
            "https://cdn.test.chalkak/a/b/c"
        );
    }

    #[test]
    fn 기준_주소의_경로와_질의는_보존한다() {
        let p = provider("https://cdn.test.chalkak/img?v=1", "chalkak");
        assert_eq!(
            url(&p, "chalkak/a.webp"),
            "https://cdn.test.chalkak/img/a.webp?v=1"
        );
        // UriComponentsBuilder.pathSegment는 기준 경로의 끝 슬래시 뒤에 "/"를 다시 붙인다.
        let p = provider("https://cdn.test.chalkak/", "chalkak");
        assert_eq!(
            url(&p, "chalkak/a.webp"),
            "https://cdn.test.chalkak//a.webp"
        );
    }

    #[test]
    fn 기준_주소가_깨지면_거절한다() {
        assert!(CloudFrontUrlProvider::new("cdn.test.chalkak", "/chalkak").is_err());
        assert!(CloudFrontUrlProvider::new("https://", "/chalkak").is_err());
    }
}
