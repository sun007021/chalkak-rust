//! 원본 `Banner` 엔티티의 도메인 규칙. 제목·부제목·URL의 정규화와 검증.

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use uuid::Uuid;

use super::uri::is_https_server_uri;
use crate::web::binding::is_java_whitespace;

pub const MAX_TITLE_LENGTH: usize = 50;
pub const MAX_SUBTITLE_LENGTH: usize = 100;
pub const MAX_URL_LENGTH: usize = 2_000;

const TITLE_MESSAGE: &str = "배너 제목은 1자 이상 50자 이하여야 합니다.";
const SUBTITLE_MESSAGE: &str = "배너 부제목은 1자 이상 100자 이하여야 합니다.";
const WEB_VIEW_URL_FIELD: &str = "배너 웹뷰 URL";
const THUMBNAIL_URL_FIELD: &str = "배너 썸네일 URL";

fn business(message: &str) -> AppError {
    AppError::business(ErrorCode::BUSINESS_ERROR, message)
}

/// 저장된 배너(`banners` 행).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Banner {
    /// 저장 전에는 `None`. DB가 `uuidv7()`로 만든다.
    pub id: Option<Uuid>,
    pub title: String,
    pub subtitle: String,
    pub web_view_url: String,
    pub thumbnail_url: String,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}

impl Banner {
    /// `Banner.create`: 네 항목의 앞뒤 공백(`trim().strip()`)만 떼고 길이·URL을 검사한다. URL 본문은 바꾸지 않는다.
    pub fn create(
        title: Option<&str>,
        subtitle: Option<&str>,
        web_view_url: Option<&str>,
        thumbnail_url: Option<&str>,
    ) -> Result<Banner, AppError> {
        Ok(Banner {
            id: None,
            title: normalize_text(title, MAX_TITLE_LENGTH, TITLE_MESSAGE)?,
            subtitle: normalize_text(subtitle, MAX_SUBTITLE_LENGTH, SUBTITLE_MESSAGE)?,
            web_view_url: normalize_url(web_view_url, WEB_VIEW_URL_FIELD)?,
            thumbnail_url: normalize_url(thumbnail_url, THUMBNAIL_URL_FIELD)?,
            created_at: None,
            updated_at: None,
            deleted_at: None,
        })
    }

    /// `delete`: 이미 삭제했으면 최초 삭제 시각을 유지한다. 감사 기록의 삭제 시각이 DB 저장값과 같도록 마이크로초로 맞춘다.
    pub fn delete(&mut self, deleted_at: Timestamp) {
        if self.is_deleted() {
            return;
        }
        self.deleted_at = Some(truncate_to_micros(deleted_at));
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

fn truncate_to_micros(at: Timestamp) -> Timestamp {
    let micros = at.as_microsecond();
    Timestamp::from_microsecond(micros).unwrap_or(at)
}

/// `value.trim().strip()` 뒤 비었거나 코드 포인트 수가 한도를 넘으면 거절한다.
fn normalize_text(
    value: Option<&str>,
    max_length: usize,
    message: &str,
) -> Result<String, AppError> {
    let normalized = value
        .unwrap_or_default()
        .trim_matches(|c: char| c <= ' ')
        .trim_matches(is_java_whitespace);
    if normalized.is_empty() || normalized.chars().count() > max_length {
        return Err(business(message));
    }
    Ok(normalized.to_owned())
}

fn normalize_url(value: Option<&str>, field_name: &str) -> Result<String, AppError> {
    let normalized = normalize_text(
        value,
        MAX_URL_LENGTH,
        &format!("{field_name}은 1자 이상 2,000자 이하여야 합니다."),
    )?;
    if !is_https_server_uri(&normalized) {
        return Err(business(&format!(
            "{field_name}은 호스트가 있는 HTTPS 절대 주소여야 합니다."
        )));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEB_VIEW_URL: &str = "https://chalkak.vercel.app/event";
    const THUMBNAIL_URL: &str = "https://cdn.example.com/banner.png";

    fn create(field: &str, value: Option<&str>) -> Result<Banner, AppError> {
        let pick = |name: &'static str, default: &'static str| {
            if field == name { value } else { Some(default) }
        };
        Banner::create(
            pick("title", "제목"),
            pick("subtitle", "부제목"),
            pick("webViewUrl", WEB_VIEW_URL),
            pick("thumbnailUrl", THUMBNAIL_URL),
        )
    }

    fn message(result: Result<Banner, AppError>) -> String {
        result.unwrap_err().message
    }

    // BannerTest#create_surroundingWhitespace_trimsWithoutRewritingUrl
    #[test]
    fn 배너_생성은_네_항목의_앞뒤_공백만_제거하고_url_쿼리와_프래그먼트를_보존한다() {
        let banner = Banner::create(
            Some(" 제목 "),
            Some(" 부제목 "),
            Some(&format!(" {WEB_VIEW_URL}?source=home#camera ")),
            Some(&format!(" {THUMBNAIL_URL} ")),
        )
        .unwrap();
        assert_eq!(banner.title, "제목");
        assert_eq!(banner.subtitle, "부제목");
        assert_eq!(
            banner.web_view_url,
            format!("{WEB_VIEW_URL}?source=home#camera")
        );
        assert_eq!(banner.thumbnail_url, THUMBNAIL_URL);
    }

    // BannerTest#create_emptyField_throwsBusinessException
    #[test]
    fn 네_항목의_null_빈_값_공백은_거절한다() {
        for field in ["title", "subtitle", "webViewUrl", "thumbnailUrl"] {
            for value in [None, Some(""), Some(" "), Some("\u{2003}")] {
                let error = create(field, value).unwrap_err();
                assert_eq!(error.status(), 400, "{field} {value:?}");
            }
        }
    }

    // BannerTest#create_textWithinBoundary_acceptsCodePointLength, create_textOverBoundary_throwsBusinessException
    #[test]
    fn 제목_부제목은_이모지도_한_글자로_세어_경계까지_허용한다() {
        for (field, max) in [("title", 50), ("subtitle", 100)] {
            for length in [1, 2, max - 1, max] {
                let text = "📷".repeat(length);
                let banner = create(field, Some(&text)).unwrap();
                let stored = if field == "title" {
                    banner.title
                } else {
                    banner.subtitle
                };
                assert_eq!(stored.chars().count(), length);
            }
            assert!(create(field, Some(&"📷".repeat(max + 1))).is_err());
        }
        assert_eq!(
            message(create("title", Some(&"가".repeat(51)))),
            "배너 제목은 1자 이상 50자 이하여야 합니다."
        );
        assert_eq!(
            message(create("subtitle", Some(&"가".repeat(101)))),
            "배너 부제목은 1자 이상 100자 이하여야 합니다."
        );
    }

    // BannerTest#create_urlWithinBoundary_acceptsNormalizedLength, create_urlOverBoundary_throwsBusinessException
    #[test]
    fn url은_공백_제거_후_2000자까지_허용하고_2001자는_거절한다() {
        for field in ["webViewUrl", "thumbnailUrl"] {
            for length in [1999, 2000] {
                let url = format!("{WEB_VIEW_URL}{}", "가".repeat(length - WEB_VIEW_URL.len()));
                let padded = format!(" {url} ");
                let banner = create(field, Some(&padded)).unwrap();
                let stored = if field == "webViewUrl" {
                    banner.web_view_url
                } else {
                    banner.thumbnail_url
                };
                assert_eq!(stored, url);
            }
            let over = format!("{WEB_VIEW_URL}{}", "a".repeat(2001 - WEB_VIEW_URL.len()));
            assert!(create(field, Some(&over)).is_err());
        }
        let over = format!("{WEB_VIEW_URL}{}", "a".repeat(2001 - WEB_VIEW_URL.len()));
        assert_eq!(
            message(create("webViewUrl", Some(&over))),
            "배너 웹뷰 URL은 1자 이상 2,000자 이하여야 합니다."
        );
    }

    // BannerTest#create_invalidUrl_throwsBusinessException, create_invalidUrl_reportsFieldName
    #[test]
    fn 비https_상대_주소_호스트_누락_문법_오류는_필드를_구분한_메시지로_거절한다() {
        for url in [
            "http://example.com",
            "/event",
            "//example.com/event",
            "https:///event",
            "javascript:alert(1)",
            "https://",
            "https://exa mple.com",
            "https://example.com/%xx",
            "https://example.com:invalid/event",
            "https://example.com\\event",
        ] {
            for field in ["webViewUrl", "thumbnailUrl"] {
                assert!(create(field, Some(url)).is_err(), "{field} {url}");
            }
        }
        assert_eq!(
            message(create("webViewUrl", Some("http://example.com"))),
            "배너 웹뷰 URL은 호스트가 있는 HTTPS 절대 주소여야 합니다."
        );
        assert_eq!(
            message(create("thumbnailUrl", Some("http://example.com"))),
            "배너 썸네일 URL은 호스트가 있는 HTTPS 절대 주소여야 합니다."
        );
    }

    // BannerTest#create_validHttpsUrl_preservesInput
    #[test]
    fn https_주소는_변환_없이_허용한다() {
        for url in [
            "https://example.com/event",
            "https://other.example.org/event?id=1",
            "HTTPS://EXAMPLE.COM/event",
            "https://example.com/안내#camera",
        ] {
            assert_eq!(create("webViewUrl", Some(url)).unwrap().web_view_url, url);
        }
    }

    // BannerTest#create_unicodeWhitespaceAroundText_normalizesBeforeLengthValidation
    #[test]
    fn 유니코드_공백을_제거한_뒤_길이를_검사한다() {
        for (field, length) in [("title", 50), ("subtitle", 100)] {
            let text = "가".repeat(length);
            let padded = format!("\u{2003}{text}\u{2003}");
            let banner = create(field, Some(&padded)).unwrap();
            assert_eq!(
                if field == "title" {
                    banner.title
                } else {
                    banner.subtitle
                },
                text
            );
        }
        let url = format!("{WEB_VIEW_URL}{}", "가".repeat(2000 - WEB_VIEW_URL.len()));
        let padded = format!("\u{2003}{url}\u{2003}");
        assert_eq!(
            create("webViewUrl", Some(&padded)).unwrap().web_view_url,
            url
        );
    }

    // BannerTest#create_portWithinRange_preservesUrl, create_portOverRange_throwsBusinessException
    #[test]
    fn 포트는_65535까지_허용한다() {
        for port in [65534, 65535] {
            let url = format!("https://example.com:{port}/event");
            assert_eq!(
                create("thumbnailUrl", Some(&url)).unwrap().thumbnail_url,
                url
            );
        }
        for port in ["65536", "2147483647", "99999999999"] {
            assert!(
                create(
                    "webViewUrl",
                    Some(&format!("https://example.com:{port}/event"))
                )
                .is_err()
            );
        }
    }

    // Banner#delete
    #[test]
    fn 삭제는_마이크로초로_맞추고_재삭제는_최초_시각을_유지한다() {
        let mut banner = create("title", Some("제목")).unwrap();
        let first: Timestamp = "2026-10-19T09:00:00.123456789Z".parse().unwrap();
        banner.delete(first);
        assert_eq!(
            banner.deleted_at,
            Some("2026-10-19T09:00:00.123456Z".parse().unwrap())
        );
        banner.delete("2026-10-20T00:00:00Z".parse().unwrap());
        assert_eq!(
            banner.deleted_at,
            Some("2026-10-19T09:00:00.123456Z".parse().unwrap())
        );
        assert!(banner.is_deleted());
    }
}
