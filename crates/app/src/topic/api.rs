//! 원본 `TopicController`(`GET /api/v1/topics?date=`)와 `TopicDetailResponse`.

use chalkak_http::Request;
use serde::Serialize;

use super::service::{TopicDetail, TopicService};
use crate::AppContext;
use crate::web::json::serialize_instant;
use crate::web::{ApiResult, Routes, binding, ok_json};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TopicDetailResponse {
    id: String,
    title: String,
    topic_date: String,
    #[serde(serialize_with = "serialize_instant")]
    starts_at: jiff::Timestamp,
    #[serde(serialize_with = "serialize_instant")]
    ends_at: jiff::Timestamp,
    phase: &'static str,
}

impl From<TopicDetail> for TopicDetailResponse {
    fn from(d: TopicDetail) -> Self {
        TopicDetailResponse {
            id: d.id.to_string(),
            title: d.title,
            topic_date: d.topic_date.to_string(),
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            phase: d.phase.as_str(),
        }
    }
}

pub fn register(routes: Routes, ctx: &AppContext) -> Routes {
    let service = TopicService::new(ctx.pool.clone(), ctx.clock.clone());
    routes.get("/api/v1/topics", move |req| get_topic(&service, req))
}

fn get_topic(service: &TopicService, req: &Request) -> ApiResult {
    let date = binding::required_date_query(req, "date")?;
    let detail = service.get_topic(date)?;
    Ok(ok_json(&TopicDetailResponse::from(detail)))
}

#[cfg(test)]
mod tests {
    use chalkak_http::Method;
    use jiff::civil::date;

    use super::*;
    use crate::topic::domain::TopicPhase;

    fn body_of(detail: TopicDetail) -> serde_json::Value {
        let response = ok_json(&TopicDetailResponse::from(detail));
        assert_eq!(response.status, 200);
        serde_json::from_slice(&response.body).unwrap()
    }

    // TopicControllerTest#getTopic_openedTopic_returnsTopicDetail
    #[test]
    fn get_topic_opened_topic_returns_topic_detail() {
        let body = body_of(TopicDetail {
            id: "0198f6c1-62ba-7d30-8b12-0f733b6570b2".parse().unwrap(),
            title: "오늘 가장 기억에 남은 순간".to_owned(),
            topic_date: date(2026, 8, 12),
            starts_at: "2026-08-11T15:00:00Z".parse().unwrap(),
            ends_at: "2026-08-12T15:00:00Z".parse().unwrap(),
            phase: TopicPhase::Open,
        });
        assert_eq!(body["id"], "0198f6c1-62ba-7d30-8b12-0f733b6570b2");
        assert_eq!(body["title"], "오늘 가장 기억에 남은 순간");
        assert_eq!(body["topicDate"], "2026-08-12");
        assert_eq!(body["startsAt"], "2026-08-11T15:00:00Z");
        assert_eq!(body["endsAt"], "2026-08-12T15:00:00Z");
        assert_eq!(body["phase"], "OPEN");
    }

    // TopicControllerTest#getTopic_invalidDateFormat_returnsBadRequest
    #[test]
    fn get_topic_invalid_date_format_returns_bad_request() {
        let mut req = Request::new(Method::Get, "/api/v1/topics");
        req.query = vec![("date".to_owned(), "2026-13-99".to_owned())];
        let err = binding::required_date_query(&req, "date").unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(err.message(), "date: 요청 값의 형식이 올바르지 않습니다.");
        assert_eq!(err.code().as_str(), "BUSINESS_ERROR");
    }
}
