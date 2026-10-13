//! Spring Boot `DefaultErrorAttributes` 모양의 오류 응답. 서블릿 컨테이너(Tomcat)나 보안 방화벽이 컨트롤러 앞에서 끊은 요청에
//! `/error` 디스패치가 만들어 주는 본문이다(`{"timestamp","status","error","path"}`). 애플리케이션 오류 응답
//! (`{"errorCode","message"}`)과 모양이 다르다.

use chalkak_app::web::marker;
use chalkak_core::Clock;
use chalkak_http::Response;
use jiff::Timestamp;

/// Jackson이 `Date`를 쓰는 모양: 밀리초 3자리 고정, `Z`.
pub fn format_timestamp(at: Timestamp) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("문자열은 항상 직렬화된다")
}

/// 본문만. `status`가 응답 상태와 다를 수 있어(`/error`의 999) 따로 받는다.
pub fn body(at: Timestamp, status: u16, error: &str, path: Option<&str>) -> String {
    let mut out = format!(
        "{{\"timestamp\":\"{}\",\"status\":{status},\"error\":{}",
        format_timestamp(at),
        json_string(error)
    );
    if let Some(path) = path {
        out.push_str(",\"path\":");
        out.push_str(&json_string(path));
    }
    out.push('}');
    out
}

/// `Content-Type: application/json`(charset 없음) 오류 응답. 콘텐츠 협상 계층이 `Accept`에 맞춰 바로잡는다.
pub fn error_response(clock: &dyn Clock, status: u16, reason: &str, path: &str) -> Response {
    Response::new(status)
        .with_header("Content-Type", "application/json")
        .with_header(marker::ORIGIN, marker::BOOT)
        .with_body(body(clock.now(), status, reason, Some(path)).into_bytes())
}

/// 응답 뒤에 연결을 닫게 한다(Tomcat이 오류 응답 뒤에 그러듯이).
pub fn closing(response: Response) -> Response {
    response.with_header("Connection", "close")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chalkak_core::FixedClock;

    #[test]
    fn 타임스탬프는_밀리초_3자리로_쓴다() {
        let at: Timestamp = "2026-10-12T17:40:00.185Z".parse().unwrap();
        assert_eq!(format_timestamp(at), "2026-10-12T17:40:00.185Z");
        let at: Timestamp = "2026-10-12T17:40:00Z".parse().unwrap();
        assert_eq!(format_timestamp(at), "2026-10-12T17:40:00.000Z");
    }

    // java-a에 TRACE /api/v1/topics를 보낸 결과
    #[test]
    fn 본문은_timestamp_status_error_path_순서다() {
        let clock = FixedClock::new("2026-10-12T17:40:00.185Z".parse().unwrap());
        let r = error_response(&clock, 405, "Method Not Allowed", "/api/v1/topics");
        assert_eq!(r.status, 405);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            r#"{"timestamp":"2026-10-12T17:40:00.185Z","status":405,"error":"Method Not Allowed","path":"/api/v1/topics"}"#
        );
    }

    #[test]
    fn path가_없으면_생략한다() {
        let at: Timestamp = "2026-10-12T00:00:01.855Z".parse().unwrap();
        assert_eq!(
            body(at, 999, "None", None),
            r#"{"timestamp":"2026-10-12T00:00:01.855Z","status":999,"error":"None"}"#
        );
    }
}
