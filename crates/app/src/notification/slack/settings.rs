//! 관리자 알림 설정. 원본 `NotificationProperties`, `SlackNotificationProperties`와 `application*.yml`의
//! `chalkak.admin.notification.*`.
//!
//! - `ADMIN_NOTIFICATION_DELIVERY_ENABLED`: local은 기본 꺼짐, dev·prod는 필수. 꺼져 있으면 아래 값은 읽지도 검증하지도 않는다
//!   (원본은 꺼진 동안 알림 Bean을 만들지 않는다).
//! - `ADMIN_WEB_BASE_URL`: 경로 없는 HTTPS Origin. local 기본값은 `http://localhost:3000`이라 켜기 전에 바꿔야 한다.
//! - `ADMIN_SLACK_WEBHOOK_URL`: `https://hooks.slack.com/services/<a>/<b>/<c>`만 허용한다.
//!   local 기본값은 자리표시자라 켜기 전에 바꿔야 한다.
//! - 연결·응답 제한 시간은 `application.yml`의 고정값(2초, 3초)이다.

use std::time::Duration;

use chalkak_core::config::{ConfigError, Env};

const LOCAL_ADMIN_WEB_BASE_URL: &str = "http://localhost:3000";
const LOCAL_WEBHOOK_URL: &str = "https://REPLACE_WITH_SLACK_WEBHOOK_URL";
const SLACK_WEBHOOK_HOST: &str = "hooks.slack.com";
const SLACK_WEBHOOK_PATH_PREFIX: &str = "/services/";

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// `java.net.URI`에서 이 설정 검증에 필요한 부분만 뗀 것.
#[derive(Debug, PartialEq, Eq)]
struct ParsedUri<'a> {
    scheme: &'a str,
    host: Option<&'a str>,
    has_port: bool,
    has_user_info: bool,
    has_query: bool,
    has_fragment: bool,
    path: &'a str,
}

/// 계층형 절대 URI(`scheme://authority/path?query#fragment`)만 읽는다. 공백·제어 문자가 있으면 `URISyntaxException`처럼 실패.
fn parse_uri(text: &str) -> Option<ParsedUri<'_>> {
    if text
        .chars()
        .any(|c| c.is_control() || c == ' ' || c == '"' || c == '<' || c == '>')
    {
        return None;
    }
    let (scheme, rest) = text.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((r, f)) => (r, Some(f)),
        None => (rest, None),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let (user_info, host_port) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (host, has_port) = match host_port.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, !p.is_empty()),
        Some(_) => return None,
        None => (host_port, false),
    };
    // 서버 기반 권한(호스트 이름)이 아니면 `getHost()`가 null이다. 밑줄은 호스트 이름에 쓸 수 없다.
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    Some(ParsedUri {
        scheme,
        host: host_ok.then_some(host),
        has_port,
        has_user_info: user_info.is_some(),
        has_query: query.is_some(),
        has_fragment: fragment.is_some(),
        path,
    })
}

/// `NotificationProperties`: 경로 없는 HTTPS Origin인가. 끝 슬래시도 경로이므로 거절한다.
pub fn validate_admin_web_base_url(url: &str) -> Result<(), String> {
    let invalid = || "관리자 웹 기본 주소는 경로 없는 HTTPS Origin이어야 합니다.".to_owned();
    let uri = parse_uri(url).ok_or_else(invalid)?;
    let ok = uri.scheme.eq_ignore_ascii_case("https")
        && uri.host.is_some()
        && !uri.has_user_info
        && !uri.has_query
        && !uri.has_fragment
        && uri.path.is_empty();
    if ok { Ok(()) } else { Err(invalid()) }
}

/// `SlackNotificationProperties`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackNotificationSettings {
    pub webhook_url: String,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl SlackNotificationSettings {
    pub fn new(
        webhook_url: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, String> {
        if connect_timeout.is_zero() {
            return Err("Slack 연결 제한 시간은 0보다 커야 합니다.".to_owned());
        }
        if read_timeout.is_zero() {
            return Err("Slack 응답 제한 시간은 0보다 커야 합니다.".to_owned());
        }
        if !is_slack_webhook(webhook_url) {
            return Err("Slack Webhook은 hooks.slack.com의 HTTPS URL이어야 합니다.".to_owned());
        }
        Ok(SlackNotificationSettings {
            webhook_url: webhook_url.to_owned(),
            connect_timeout,
            read_timeout,
        })
    }
}

fn is_slack_webhook(url: &str) -> bool {
    let Some(uri) = parse_uri(url) else {
        return false;
    };
    if !uri.scheme.eq_ignore_ascii_case("https")
        || !uri
            .host
            .is_some_and(|h| h.eq_ignore_ascii_case(SLACK_WEBHOOK_HOST))
        || uri.has_port
        || uri.has_user_info
        || uri.has_query
        || uri.has_fragment
    {
        return false;
    }
    let Some(rest) = uri.path.strip_prefix(SLACK_WEBHOOK_PATH_PREFIX) else {
        return false;
    };
    let segments: Vec<&str> = rest.split('/').collect();
    segments.len() == 3
        && segments
            .iter()
            .all(|s| !s.chars().all(crate::web::binding::is_java_whitespace))
}

/// 전달이 켜졌을 때 필요한 값 전체.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminNotificationSettings {
    pub admin_web_base_url: String,
    pub slack: SlackNotificationSettings,
}

impl AdminNotificationSettings {
    /// 꺼져 있으면 `None`. 켜져 있는데 값이 잘못됐으면 기동을 막는 오류다.
    /// `local`은 local 프로필이다(기본값이 있다).
    pub fn from_env(env: &Env, local: bool) -> Result<Option<Self>, ConfigError> {
        let enabled = if local {
            env.bool_or("ADMIN_NOTIFICATION_DELIVERY_ENABLED", false)?
        } else {
            env.required_parsed::<bool>("ADMIN_NOTIFICATION_DELIVERY_ENABLED")?
        };
        if !enabled {
            return Ok(None);
        }
        let (base_url, webhook_url) = if local {
            (
                env.string_or("ADMIN_WEB_BASE_URL", LOCAL_ADMIN_WEB_BASE_URL),
                env.string_or("ADMIN_SLACK_WEBHOOK_URL", LOCAL_WEBHOOK_URL),
            )
        } else {
            (
                env.required("ADMIN_WEB_BASE_URL")?,
                env.required("ADMIN_SLACK_WEBHOOK_URL")?,
            )
        };
        let invalid = |key: &str| {
            let key = key.to_owned();
            move |reason: String| ConfigError::Invalid { key, reason }
        };
        validate_admin_web_base_url(base_url).map_err(invalid("ADMIN_WEB_BASE_URL"))?;
        // 오류 문구에 Webhook 주소(비밀)를 싣지 않는다.
        let slack = SlackNotificationSettings::new(webhook_url, CONNECT_TIMEOUT, READ_TIMEOUT)
            .map_err(invalid("ADMIN_SLACK_WEBHOOK_URL"))?;
        Ok(Some(AdminNotificationSettings {
            admin_web_base_url: base_url.to_owned(),
            slack,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEBHOOK: &str = "https://hooks.slack.com/services/T000/B000/XXXX";

    // NotificationPropertiesTest
    #[test]
    fn 경로_없는_https_origin을_받는다() {
        assert!(validate_admin_web_base_url("https://admin.example.com").is_ok());
        assert!(validate_admin_web_base_url("HTTPS://admin.example.com:8443").is_ok());
    }

    #[test]
    fn 경로가_붙은_관리자_웹_주소를_거부한다() {
        for url in [
            "https://admin.example.com/",
            "https://admin.example.com/admin",
            "http://admin.example.com",
            "https://user@admin.example.com",
            "https://admin.example.com?x=1",
            "https://admin.example.com#top",
            "admin.example.com",
            "https://",
            "https://admin example.com",
        ] {
            assert_eq!(
                validate_admin_web_base_url(url).unwrap_err(),
                "관리자 웹 기본 주소는 경로 없는 HTTPS Origin이어야 합니다.",
                "{url}"
            );
        }
    }

    // SlackNotificationPropertiesTest
    #[test]
    fn 공식_webhook과_양수_제한_시간을_받는다() {
        let s = SlackNotificationSettings::new(WEBHOOK, CONNECT_TIMEOUT, READ_TIMEOUT).unwrap();
        assert_eq!(s.webhook_url, WEBHOOK);
        assert_eq!(s.connect_timeout, Duration::from_secs(2));
        assert_eq!(s.read_timeout, Duration::from_secs(3));
    }

    #[test]
    fn 양수가_아닌_제한_시간을_거부한다() {
        let err =
            SlackNotificationSettings::new(WEBHOOK, Duration::ZERO, READ_TIMEOUT).unwrap_err();
        assert_eq!(err, "Slack 연결 제한 시간은 0보다 커야 합니다.");
        let err =
            SlackNotificationSettings::new(WEBHOOK, CONNECT_TIMEOUT, Duration::ZERO).unwrap_err();
        assert_eq!(err, "Slack 응답 제한 시간은 0보다 커야 합니다.");
    }

    #[test]
    fn slack이_아닌_webhook을_거부한다() {
        for url in [
            "https://hooks.slack.test/services/T/B/X",
            "http://hooks.slack.com/services/T/B/X",
            "https://hooks.slack.com:443/services/T/B/X",
            "https://user@hooks.slack.com/services/T/B/X",
            "https://hooks.slack.com/services/T/B/X?a=1",
            "https://hooks.slack.com/services/T/B/X#f",
            "https://hooks.slack.com/other/T/B/X",
            "https://hooks.slack.com/services/T/B",
            "https://hooks.slack.com/services/T/B/X/Y",
            "https://hooks.slack.com/services/T//X",
            "https://hooks.slack.com/services/T/ /X",
            "https://REPLACE_WITH_SLACK_WEBHOOK_URL",
        ] {
            assert_eq!(
                SlackNotificationSettings::new(url, CONNECT_TIMEOUT, READ_TIMEOUT).unwrap_err(),
                "Slack Webhook은 hooks.slack.com의 HTTPS URL이어야 합니다.",
                "{url}"
            );
        }
        assert!(
            SlackNotificationSettings::new(
                "HTTPS://HOOKS.SLACK.COM/services/T/B/X",
                CONNECT_TIMEOUT,
                READ_TIMEOUT
            )
            .is_ok(),
            "스킴과 호스트는 대소문자를 구분하지 않는다"
        );
    }

    #[test]
    fn 마지막_슬래시가_붙은_webhook을_거부한다() {
        assert!(
            SlackNotificationSettings::new(
                "https://hooks.slack.com/services/T/B/X/",
                CONNECT_TIMEOUT,
                READ_TIMEOUT
            )
            .is_err()
        );
    }

    // application-local.yml / application-dev.yml: delivery-enabled
    #[test]
    fn 전달이_꺼져_있으면_다른_값은_읽지_않는다() {
        let env = Env::from_pairs([("ADMIN_WEB_BASE_URL", "not a url")]);
        assert_eq!(
            AdminNotificationSettings::from_env(&env, true).unwrap(),
            None
        );
        let env = Env::from_pairs([("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "false")]);
        assert_eq!(
            AdminNotificationSettings::from_env(&env, false).unwrap(),
            None
        );
    }

    #[test]
    fn dev와_prod는_전달_여부가_필수다() {
        assert!(
            AdminNotificationSettings::from_env(&Env::from_pairs::<&str, &str>([]), false).is_err()
        );
    }

    #[test]
    fn 켜면_관리자_웹_주소와_webhook을_검증한다() {
        let env = Env::from_pairs([
            ("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "true"),
            ("ADMIN_WEB_BASE_URL", "https://admin.example.com"),
            ("ADMIN_SLACK_WEBHOOK_URL", WEBHOOK),
        ]);
        let s = AdminNotificationSettings::from_env(&env, false)
            .unwrap()
            .unwrap();
        assert_eq!(s.admin_web_base_url, "https://admin.example.com");
        assert_eq!(s.slack.webhook_url, WEBHOOK);

        // local의 기본값은 켜기 전에 바꿔야 하는 자리표시자다.
        let env = Env::from_pairs([("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "true")]);
        assert!(AdminNotificationSettings::from_env(&env, true).is_err());

        let env = Env::from_pairs([
            ("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "true"),
            ("ADMIN_WEB_BASE_URL", "https://admin.example.com/path"),
            ("ADMIN_SLACK_WEBHOOK_URL", WEBHOOK),
        ]);
        assert!(AdminNotificationSettings::from_env(&env, false).is_err());
    }

    #[test]
    fn 설정_오류에_webhook_주소를_싣지_않는다() {
        let secret = "https://hooks.slack.test/services/SECRET/SECRET/SECRET";
        let env = Env::from_pairs([
            ("ADMIN_NOTIFICATION_DELIVERY_ENABLED", "true"),
            ("ADMIN_WEB_BASE_URL", "https://admin.example.com"),
            ("ADMIN_SLACK_WEBHOOK_URL", secret),
        ]);
        let message = AdminNotificationSettings::from_env(&env, false)
            .unwrap_err()
            .to_string();
        assert!(!message.contains("SECRET"), "{message}");
    }
}
