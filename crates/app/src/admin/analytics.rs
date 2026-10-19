//! 사용자 지표 수동 집계(원본 `AdminAnalyticsController`, `AdminUserMetricsAggregationRequest`):
//! `POST /api/v1/admin/analytics/aggregations`.
//!
//! 요청 본문 `{"metricDate":"yyyy-MM-dd"}`로 끝난 날짜(KST)를 받아 백그라운드 집계를 시작하고 본문 없는 202로 답한다.
//! 집계 로그를 다시 남기는 요청이라 과거 그래프의 값은 바뀌지 않는다.

use std::sync::Arc;

use chalkak_http::Response;

use crate::admin::binding::{MUST_NOT_BE_NULL, body_local_date, validation};
use crate::admin::support::{AdminDeps, resolve_current_admin};
use crate::analytics::UserMetricsAggregationRunner;
use crate::web::Routes;
use crate::web::binding::read_json_object;

pub fn register(
    routes: Routes,
    deps: &AdminDeps,
    runner: Arc<UserMetricsAggregationRunner>,
) -> Routes {
    let deps = deps.clone();
    routes.post("/api/v1/admin/analytics/aggregations", move |req| {
        let admin_id = resolve_current_admin(&deps)?;
        // `@NotNull LocalDate metricDate`: 형식 오류는 본문 파싱 오류, 없으면 검증 오류.
        let body = read_json_object(req)?;
        let Some(metric_date) = body_local_date(&body, "metricDate")? else {
            return Err(validation(MUST_NOT_BE_NULL));
        };
        runner.start(admin_id, metric_date.date())?;
        Ok(Response::new(202))
    })
}

#[cfg(test)]
mod tests {
    use chalkak_core::FixedClock;
    use chalkak_http::{Handler, Method, Request};
    use serde_json::{Value, json};

    use super::*;
    use crate::admin::testing::{deps, insert_admin};
    use crate::analytics::TaskExecutor;
    use crate::analytics::testing::{NoAccessLogs, QueueExecutor};
    use crate::analytics::{PgUserMetricsRepository, UserMetricsAggregationService};
    use crate::security::{self, AUTHORITY_ADMIN, Principal};
    use crate::testing::TestDb;

    const PATH: &str = "/api/v1/admin/analytics/aggregations";

    struct Api {
        router: chalkak_http::Router,
        admin: uuid::Uuid,
        executor: Arc<QueueExecutor>,
        _db: TestDb,
    }

    fn api() -> Option<Api> {
        let db = TestDb::create()?;
        let deps = deps(&db);
        let admin = insert_admin(&mut db.client(), "operator");
        let executor = Arc::new(QueueExecutor::default());
        // UTC 2030-01-02 16:30은 KST 2030-01-03 01:30이다.
        let clock = Arc::new(FixedClock::new("2030-01-02T16:30:00Z".parse().unwrap()));
        let service = Arc::new(UserMetricsAggregationService::new(
            Arc::new(PgUserMetricsRepository::new(db.pool(2), Vec::new())),
            Arc::new(NoAccessLogs),
            deps.logger.clone(),
        ));
        let runner = Arc::new(UserMetricsAggregationRunner::new(
            service,
            executor.clone() as Arc<dyn TaskExecutor>,
            clock,
            deps.logger.clone(),
        ));
        let router = register(Routes::new(deps.logger.clone()), &deps, runner).into_router();
        Some(Api {
            router,
            admin,
            executor,
            _db: db,
        })
    }

    impl Api {
        fn post(&self, body: &str) -> (u16, Value, chalkak_http::Response) {
            let _guard = security::enter(Principal {
                subject: self.admin.hyphenated().to_string(),
                authorities: vec![AUTHORITY_ADMIN.to_owned()],
            });
            let mut req = Request::new(Method::Post, PATH);
            req.headers.append("Content-Type", "application/json");
            req.body = body.as_bytes().to_vec();
            let response = self.router.handle(req);
            let json = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
            (response.status, json, response)
        }
    }

    // AdminAnalyticsControllerTest#startUserMetricsAggregation_validDate_returnsAcceptedWithoutBody
    #[test]
    fn 날짜를_보내면_본문_없는_202로_응답하고_러너를_시작한다() {
        let Some(api) = api() else { return };

        let (status, _, raw) = api.post(r#"{"metricDate":"2030-01-02"}"#);

        assert_eq!(status, 202);
        assert!(raw.body.is_empty());
        assert_eq!(api.executor.len(), 1);
    }

    // AdminAnalyticsControllerTest#startUserMetricsAggregation_missingOrMalformedDate_returnsBadRequest
    #[test]
    fn 날짜가_없거나_형식이_틀리면_400으로_거절하고_러너를_시작하지_않는다() {
        let Some(api) = api() else { return };

        for body in [
            r#"{}"#,
            r#"{"metricDate":null}"#,
            r#"{"metricDate":"2030/01/02"}"#,
        ] {
            let (status, json, _) = api.post(body);
            assert_eq!(status, 400, "{body}");
            assert_eq!(json["errorCode"], "BUSINESS_ERROR", "{body}");
        }
        assert_eq!(api.executor.len(), 0);
    }

    // AdminAnalyticsControllerTest#startUserMetricsAggregation_unfinishedDay_returnsBadRequest
    #[test]
    fn 끝나지_않은_날짜는_business_error_400이다() {
        let Some(api) = api() else { return };

        let (status, json, _) = api.post(r#"{"metricDate":"2030-01-03"}"#);

        assert_eq!(status, 400);
        assert_eq!(json["errorCode"], "BUSINESS_ERROR");
        assert_eq!(
            json["message"],
            "아직 끝나지 않은 날짜는 집계할 수 없습니다."
        );
    }

    // AdminAnalyticsControllerTest#startUserMetricsAggregation_inProgress_returnsInProgressErrorCode
    #[test]
    fn 진행_중이면_진행_중_에러_코드_400이다() {
        let Some(api) = api() else { return };
        assert_eq!(api.post(r#"{"metricDate":"2030-01-02"}"#).0, 202);

        let (status, json, _) = api.post(&json!({"metricDate": "2030-01-02"}).to_string());

        assert_eq!(status, 400);
        assert_eq!(json["errorCode"], "USER_METRICS_AGGREGATION_IN_PROGRESS");
        assert_eq!(json["message"], "이미 사용자 지표 집계가 진행 중입니다.");
        assert_eq!(api.executor.len(), 1);
    }

    // AdminSecurityFilterChainTest: 관리자 권한이 없으면 컨트롤러에 닿지 않는다.
    #[test]
    fn 관리자_인증_주체가_없으면_401이고_러너를_시작하지_않는다() {
        let Some(api) = api() else { return };
        let mut req = Request::new(Method::Post, PATH);
        req.headers.append("Content-Type", "application/json");
        req.body = br#"{"metricDate":"2030-01-02"}"#.to_vec();

        assert_eq!(api.router.handle(req).status, 401);
        assert_eq!(api.executor.len(), 0);
    }
}
