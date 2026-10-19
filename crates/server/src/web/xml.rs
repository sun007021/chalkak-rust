//! JSON 본문을 XML로 바꾼다. `Accept: application/xml`·`text/xml`을 받은 응답(Spring이 `jackson-dataformat-xml`로 쓴다).
//!
//! 원본에는 `jackson-dataformat-xml`이 클래스패스에 있어 XML 컨버터가 켜져 있다. 출력 모양은 java-1에 `Accept:
//! application/xml`을 보내 얻은 것이다:
//! - 루트 요소 이름은 응답 클래스의 단순 이름이다(`NotificationListResponse`, `ErrorResponse`, Boot 오류 본문은 `Map`).
//!   컨트롤러 응답은 내장 OpenAPI 문서의 `메서드 + 경로 + 상태`에서 스키마 이름을 찾는다.
//! - 목록은 필드 이름의 요소로 한 번 감싸고 항목도 같은 이름이다(`<auditLogs><auditLogs>…</auditLogs></auditLogs>`).
//!   `LocalDate`의 `[연, 월, 일]`은 감싸지 않고 같은 이름의 요소가 셋 이어진다.
//! - `null`과 빈 목록·빈 객체는 자기 닫는 요소(`<x/>`)다.
//! - 날짜·시각 숫자 표현은 YAML과 같다([`super::yaml`]).

use std::sync::OnceLock;

use super::yaml::{Node, parse_retyped};

/// 루트 요소 이름을 정하는 방법.
pub enum Root<'a> {
    /// 컨트롤러가 돌려준 값: OpenAPI 문서에서 찾는다(못 찾으면 본문 모양으로 추정한다).
    Controller { method: &'a str, route: &'a str },
    /// 예외 처리기가 쓴 `ErrorResponse`.
    Error,
    /// Boot 오류 본문(`LinkedHashMap`).
    Map,
}

fn docs() -> &'static serde_json::Value {
    static DOCS: OnceLock<serde_json::Value> = OnceLock::new();
    DOCS.get_or_init(|| {
        serde_json::from_str(super::springdoc::API_DOCS).unwrap_or(serde_json::Value::Null)
    })
}

/// 문서에서 `메서드 + 경로 + 상태`의 응답 스키마 이름.
fn schema_name(method: &str, route: &str, status: u16) -> Option<String> {
    let reference = docs()
        .get("paths")?
        .get(route)?
        .get(method.to_ascii_lowercase())?
        .get("responses")?
        .get(status.to_string())?
        .get("content")?
        .as_object()?
        .values()
        .next()?
        .get("schema")?
        .get("$ref")?
        .as_str()?;
    reference.rsplit('/').next().map(str::to_owned)
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c => out.push(c),
        }
    }
}

fn write_element(name: &str, node: &Node, out: &mut String) {
    match node {
        Node::Null => out.push_str(&format!("<{name}/>")),
        Node::Bool(b) => out.push_str(&format!("<{name}>{b}</{name}>")),
        Node::Number(n) => out.push_str(&format!("<{name}>{n}</{name}>")),
        Node::Text(s) => {
            out.push_str(&format!("<{name}>"));
            escape(s, out);
            out.push_str(&format!("</{name}>"));
        }
        Node::Map(pairs) if pairs.is_empty() => out.push_str(&format!("<{name}/>")),
        Node::Map(pairs) => {
            out.push_str(&format!("<{name}>"));
            for (key, value) in pairs {
                write_element(key, value, out);
            }
            out.push_str(&format!("</{name}>"));
        }
        Node::List(items) if items.is_empty() => out.push_str(&format!("<{name}/>")),
        Node::List(items) => {
            out.push_str(&format!("<{name}>"));
            for item in items {
                write_element(name, item, out);
            }
            out.push_str(&format!("</{name}>"));
        }
        Node::Flat(items) => {
            for item in items {
                write_element(name, item, out);
            }
        }
    }
}

/// JSON 본문을 XML 문서로 바꾼다. JSON으로 읽을 수 없으면 `None`.
pub fn from_json(json: &[u8], root: &Root, status: u16) -> Option<Vec<u8>> {
    let node = parse_retyped(json)?;
    let name = match root {
        Root::Error => "ErrorResponse".to_owned(),
        Root::Map => "Map".to_owned(),
        Root::Controller { method, route } => {
            schema_name(method, route, status).unwrap_or_else(|| match &node {
                Node::Map(pairs) if pairs.iter().any(|(k, _)| k == "errorCode") => {
                    "ErrorResponse".to_owned()
                }
                _ => "Map".to_owned(),
            })
        }
    };
    let mut out = String::new();
    write_element(&name, &node, &mut out);
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml(json: &str, root: &Root) -> String {
        String::from_utf8(from_json(json.as_bytes(), root, 200).unwrap()).unwrap()
    }

    // java-1: Accept application/xml 오류 응답
    #[test]
    fn 오류_본문은_루트가_error_response다() {
        assert_eq!(
            xml(
                r#"{"errorCode":"BUSINESS_ERROR","message":"a<b&c"}"#,
                &Root::Error
            ),
            "<ErrorResponse><errorCode>BUSINESS_ERROR</errorCode><message>a&lt;b&amp;c</message></ErrorResponse>"
        );
    }

    #[test]
    fn 목록은_필드_이름으로_감싸고_빈_값은_자기_닫는다() {
        let root = Root::Controller {
            method: "GET",
            route: "/api/v1/notifications",
        };
        assert_eq!(
            xml(
                r#"{"currentPage":1,"hasNext":false,"notifications":[],"x":null}"#,
                &root
            ),
            "<NotificationListResponse><currentPage>1</currentPage><hasNext>false</hasNext><notifications/><x/></NotificationListResponse>"
        );
        assert_eq!(
            xml(r#"{"a":[{"b":1},{"b":2}]}"#, &Root::Map),
            "<Map><a><a><b>1</b></a><a><b>2</b></a></a></Map>"
        );
    }

    #[test]
    fn 날짜와_시각은_yaml과_같은_숫자다() {
        assert_eq!(
            xml(
                r#"{"topicDate":"2026-10-20","startsAt":"2026-10-12T00:00:01.5Z"}"#,
                &Root::Map
            ),
            "<Map><topicDate>2026</topicDate><topicDate>10</topicDate><topicDate>20</topicDate><startsAt>1791763201.500000000</startsAt></Map>"
        );
    }

    #[test]
    fn json이_아니면_none이다() {
        assert!(from_json(b"{", &Root::Map, 200).is_none());
    }
}
