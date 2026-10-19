//! CloudWatch Logs Insights로 접근 로그 지표를 센다(원본 `CloudWatchLogsAccessLogMetricsReader`, `CloudWatchLogsConfig`).
//!
//! 관측 시작일 이전이거나 하루 전체가 로그 보존 기간 안에 들지 않는 날짜는 쿼리 없이 빈 값을 돌려준다. 보존 기간이 지난
//! 날짜를 조회하면 원본 로그가 없어 0으로 센 가짜 지표가 되기 때문이다. 동시에 돌릴 수 있는 쿼리 수가 제한돼 있어 쿼리를 하나씩
//! 순서대로 실행한다. 쿼리가 실패하거나 시간 안에 끝나지 않으면 값을 비워 두지 않고 오류를 돌려준다. 평균 응답 시간은 소수점
//! 첫째 자리에서 반올림한다.
//!
//! AWS SDK 없이 CloudWatch Logs JSON 1.1 API(`StartQuery`, `GetQueryResults`, `StopQuery`)를 SigV4로 직접 호출한다
//! ([`chalkak_aws::logs`]). 자격증명은 S3와 같이 환경변수 -> IMDSv2 순서다. 호출 경계는 [`LogsInsightsClient`] 트레이트라
//! 시험은 가짜 클라이언트를 쓴다.

use std::sync::Arc;
use std::time::Duration;

use chalkak_aws::{ChainCredentialsProvider, CredentialsProvider, LogsEndpoint, sign_json_request};
use chalkak_core::Clock;
use chalkak_http::client::{HttpClient, Method};
use jiff::civil::Date;
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use uuid::Uuid;

use super::service::{AccessLogMetrics, AccessLogMetricsReader, start_of_day};
use super::settings::AnalyticsSettings;
use crate::user::storage::ClientTransport;
use crate::web::ApiError;

/// 쿼리 결과를 기다리는 간격과 최대 시간(원본 `POLL_INTERVAL`, `MAX_WAIT`).
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_WAIT: Duration = Duration::from_secs(60);

pub const VISITORS_QUERY: &str = "filter type = \"access\" and ispresent(userId)";
pub const TOPIC_VIEWERS_QUERY: &str = "filter type = \"access\" and method = \"GET\" and route = \"/api/v1/topics\" and status = 200 and ispresent(userId)";
pub const POST_CREATIONS_QUERY: &str =
    "filter type = \"access\" and method = \"POST\" and route = \"/api/v1/posts\" and status = 201";

const VISITORS_STATS: &str = "| stats count_distinct(userId) as visitors";
const TOPIC_VIEWERS_STATS: &str = "| stats count_distinct(userId) as topicViewers";
const REQUEST_SUMMARY_QUERY: &str = "filter type = \"access\"\n| stats count(*) as requests, avg(durationMs) as latencyAvg, pct(durationMs, 95) as latencyP95";
const SERVER_5XX_QUERY: &str =
    "filter type = \"access\" and status >= 500\n| stats count(*) as server5xx";
const POST_CREATIONS_STATS: &str = "| stats count(*) as postCreations";

// ---------------------------------------------------------------------------------------------
// 호출 경계
// ---------------------------------------------------------------------------------------------

/// `StartQuery` 요청(원본 `StartQueryRequest`). 시각은 epoch 초이고 `end_time`은 포함 경계다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartQuery {
    pub log_group: String,
    pub start_time: i64,
    pub end_time: i64,
    pub query_string: String,
}

/// 쿼리 상태(원본 `QueryStatus`). 모르는 값과 응답에 없는 값은 [`QueryStatus::Unknown`]이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryStatus {
    Scheduled,
    Running,
    Complete,
    Failed,
    Cancelled,
    Timeout,
    Unknown(String),
}

impl QueryStatus {
    fn parse(value: Option<&str>) -> QueryStatus {
        match value {
            Some("Scheduled") => QueryStatus::Scheduled,
            Some("Running") => QueryStatus::Running,
            Some("Complete") => QueryStatus::Complete,
            Some("Failed") => QueryStatus::Failed,
            Some("Cancelled") => QueryStatus::Cancelled,
            Some("Timeout") => QueryStatus::Timeout,
            Some(other) => QueryStatus::Unknown(other.to_owned()),
            None => QueryStatus::Unknown("null".to_owned()),
        }
    }
}

impl std::fmt::Display for QueryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryStatus::Scheduled => f.write_str("SCHEDULED"),
            QueryStatus::Running => f.write_str("RUNNING"),
            QueryStatus::Complete => f.write_str("COMPLETE"),
            QueryStatus::Failed => f.write_str("FAILED"),
            QueryStatus::Cancelled => f.write_str("CANCELLED"),
            QueryStatus::Timeout => f.write_str("TIMEOUT"),
            QueryStatus::Unknown(_) => f.write_str("UNKNOWN_TO_SDK_VERSION"),
        }
    }
}

/// 결과 필드 하나(원본 `ResultField`). Logs Insights는 값을 문자열로 돌려준다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultField {
    pub field: String,
    pub value: Option<String>,
}

impl ResultField {
    pub fn new(field: &str, value: &str) -> ResultField {
        ResultField {
            field: field.to_owned(),
            value: Some(value.to_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryResults {
    pub status: QueryStatus,
    pub results: Vec<Vec<ResultField>>,
}

/// 호출 실패. 문구는 로그에만 남는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogsError(pub String);

impl std::fmt::Display for LogsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LogsError {}

/// 원본 `CloudWatchLogsClient`의 쓰는 부분.
pub trait LogsInsightsClient: Send + Sync {
    fn start_query(&self, request: &StartQuery) -> Result<String, LogsError>;
    fn get_query_results(&self, query_id: &str) -> Result<QueryResults, LogsError>;
    fn stop_query(&self, query_id: &str) -> Result<(), LogsError>;
}

// ---------------------------------------------------------------------------------------------
// 리더
// ---------------------------------------------------------------------------------------------

/// 대기 함수(시험이 시간을 쓰지 않게 바꿔 끼운다).
pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

pub struct CloudWatchLogsAccessLogMetricsReader {
    client: Arc<dyn LogsInsightsClient>,
    log_group: String,
    log_metrics_start_date: Date,
    log_retention: SignedDuration,
    clock: Arc<dyn Clock>,
    excluded_user_filter: String,
    sleeper: Sleeper,
    poll_interval: Duration,
    max_polls: u64,
}

impl CloudWatchLogsAccessLogMetricsReader {
    /// 서버 조립용. AWS 엔드포인트(또는 `AWS_ENDPOINT_URL_CLOUDWATCH_LOGS`)로 호출한다.
    pub fn from_settings(
        settings: &AnalyticsSettings,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, String> {
        let client = HttpLogsInsightsClient::new(&settings.region, Arc::clone(&clock))?;
        Ok(Self::new(
            Arc::new(client),
            settings,
            clock,
            Arc::new(std::thread::sleep),
            POLL_INTERVAL,
            MAX_WAIT,
        ))
    }

    pub fn new(
        client: Arc<dyn LogsInsightsClient>,
        settings: &AnalyticsSettings,
        clock: Arc<dyn Clock>,
        sleeper: Sleeper,
        poll_interval: Duration,
        max_wait: Duration,
    ) -> Self {
        let max_polls = u64::try_from(max_wait.as_millis() / poll_interval.as_millis().max(1))
            .unwrap_or(u64::MAX);
        CloudWatchLogsAccessLogMetricsReader {
            client,
            log_group: settings.log_group.clone(),
            log_metrics_start_date: settings.log_metrics_start_date,
            log_retention: SignedDuration::from_hours(
                settings.log_retention_days.saturating_mul(24),
            ),
            clock,
            excluded_user_filter: excluded_user_filter(&settings.excluded_user_ids),
            sleeper,
            poll_interval,
            max_polls,
        }
    }

    /// 관측 시작일 이후이고 하루 전체가 로그 보존 기간 안에 드는 날짜인가. 하루가 일부만 만료돼도 지표가 모자라므로 없는 것으로
    /// 본다.
    fn has_logs(&self, metric_date: Date, day_start: Timestamp) -> bool {
        let retention_start = self
            .clock
            .now()
            .checked_sub(self.log_retention)
            .unwrap_or(Timestamp::MIN);
        metric_date >= self.log_metrics_start_date && day_start >= retention_start
    }

    /// 쿼리를 시작하고 끝날 때까지 기다려 첫 결과 행의 필드를 돌려준다. 결과 행이 없으면 빈 목록이다.
    fn query(
        &self,
        from: Timestamp,
        to: Timestamp,
        query_string: &str,
    ) -> Result<Vec<ResultField>, ApiError> {
        let query_id = self
            .client
            .start_query(&StartQuery {
                log_group: self.log_group.clone(),
                start_time: from.as_second(),
                // Logs Insights의 endTime은 포함 경계라, 다음 날 0시 정각의 로그가 섞이지 않게 1초를 뺀다.
                end_time: to.as_second() - 1,
                query_string: query_string.to_owned(),
            })
            .map_err(internal)?;
        for _ in 0..self.max_polls {
            let response = self.client.get_query_results(&query_id).map_err(internal)?;
            match response.status {
                QueryStatus::Complete => {
                    return Ok(response.results.into_iter().next().unwrap_or_default());
                }
                QueryStatus::Running | QueryStatus::Scheduled => {}
                other => {
                    return Err(ApiError::Internal(format!(
                        "Logs Insights 쿼리 실패: status={other}, queryId={query_id}"
                    )));
                }
            }
            (self.sleeper)(self.poll_interval);
        }
        // 시간 초과 오류를 돌려줄 것이므로 중단 실패는 남기기만 한다.
        if let Err(error) = self.client.stop_query(&query_id) {
            eprintln!("Logs Insights 쿼리 중단 실패: queryId={query_id}, {error}");
        }
        Err(ApiError::Internal(format!(
            "Logs Insights 쿼리 시간 초과: queryId={query_id}"
        )))
    }
}

impl AccessLogMetricsReader for CloudWatchLogsAccessLogMetricsReader {
    fn read(&self, metric_date: Date) -> Result<Option<AccessLogMetrics>, ApiError> {
        let from = start_of_day(metric_date);
        if !self.has_logs(metric_date, from) {
            return Ok(None);
        }
        let to = start_of_day(
            metric_date
                .tomorrow()
                .map_err(|e| ApiError::Internal(e.to_string()))?,
        );
        let filter = &self.excluded_user_filter;
        let visitors = self.query(
            from,
            to,
            &format!("{VISITORS_QUERY}{filter}\n{VISITORS_STATS}"),
        )?;
        let topic_viewers = self.query(
            from,
            to,
            &format!("{TOPIC_VIEWERS_QUERY}{filter}\n{TOPIC_VIEWERS_STATS}"),
        )?;
        let summary = self.query(from, to, REQUEST_SUMMARY_QUERY)?;
        let server_5xx = self.query(from, to, SERVER_5XX_QUERY)?;
        let post_creations = self.query(
            from,
            to,
            &format!("{POST_CREATIONS_QUERY}{filter}\n{POST_CREATIONS_STATS}"),
        )?;
        Ok(Some(AccessLogMetrics {
            visitors: single(&visitors, "visitors")?,
            topic_viewers: single(&topic_viewers, "topicViewers")?,
            requests: single(&summary, "requests")?,
            server_5xx: single(&server_5xx, "server5xx")?,
            latency_avg_ms: single(&summary, "latencyAvg")?,
            latency_p95_ms: single(&summary, "latencyP95")?,
            post_creations: single(&post_creations, "postCreations")?,
        }))
    }
}

fn internal(error: LogsError) -> ApiError {
    ApiError::Internal(error.0)
}

/// 제외 회원이 있으면 회원 기준 쿼리에 붙이는 `| filter userId not in [...]` 줄.
fn excluded_user_filter(excluded_user_ids: &[Uuid]) -> String {
    if excluded_user_ids.is_empty() {
        return String::new();
    }
    let ids = excluded_user_ids
        .iter()
        .map(|id| format!("\"{}\"", id.hyphenated()))
        .collect::<Vec<_>>()
        .join(",");
    format!("\n| filter userId not in [{ids}]")
}

/// 필드가 없으면 0이다. Logs Insights는 값을 문자열로 돌려주고, 일치하는 로그가 없으면 행을 비운다.
fn single(row: &[ResultField], field: &str) -> Result<i64, ApiError> {
    for result_field in row {
        if result_field.field == field {
            return parse(result_field.value.as_deref());
        }
    }
    Ok(0)
}

/// 값이 없거나 비었거나 NaN이면 0, 아니면 `Math.round(Double.parseDouble(값))`.
fn parse(value: Option<&str>) -> Result<i64, ApiError> {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return Ok(0);
    };
    let number: f64 = value.trim().parse().map_err(|_| {
        ApiError::Internal(format!("Logs Insights 결과 값이 숫자가 아닙니다: {value}"))
    })?;
    if number.is_nan() {
        return Ok(0);
    }
    Ok(java_round(number))
}

/// `Math.round(double)`: 반올림은 +무한대 쪽(0.5는 올림, -0.5는 0)이고 범위를 넘으면 long의 끝값이다.
fn java_round(value: f64) -> i64 {
    let floor = value.floor();
    let rounded = if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    };
    rounded as i64
}

// ---------------------------------------------------------------------------------------------
// HTTP 구현
// ---------------------------------------------------------------------------------------------

/// SDK 기본 재시도처럼 처음 1번과 재시도 3번.
const MAX_ATTEMPTS: u32 = 4;
const BASE_BACKOFF: Duration = Duration::from_millis(100);

/// CloudWatch Logs를 SigV4로 직접 호출한다(원본 `CloudWatchLogsConfig`의 클라이언트).
pub struct HttpLogsInsightsClient {
    http: HttpClient,
    credentials: Arc<dyn CredentialsProvider>,
    region: String,
    endpoint: LogsEndpoint,
    clock: Arc<dyn Clock>,
}

impl HttpLogsInsightsClient {
    pub fn new(region: &str, clock: Arc<dyn Clock>) -> Result<Self, String> {
        let endpoint = LogsEndpoint::from_process_env(region).map_err(|e| e.to_string())?;
        // SDK 기본 제한 시간(연결 2초, 응답 30초).
        let http = HttpClient::builder()
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let transport = Arc::new(ClientTransport::new()?);
        let now_clock = Arc::clone(&clock);
        let credentials = Arc::new(ChainCredentialsProvider::default_chain(
            transport,
            Arc::new(move || now_clock.now()),
        ));
        Ok(HttpLogsInsightsClient {
            http,
            credentials,
            region: region.to_owned(),
            endpoint,
            clock,
        })
    }

    /// 동작 하나를 보내고 JSON 응답을 돌려준다. 5xx·제한 초과·전송 오류는 최대 4번 시도한다.
    fn call(&self, action: &str, body: &str) -> Result<Value, LogsError> {
        let mut last = LogsError(format!("CloudWatch Logs {action} 호출 실패"));
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(BASE_BACKOFF * 2u32.pow(attempt - 1));
            }
            let credentials = self
                .credentials
                .credentials()
                .map_err(|e| LogsError(format!("AWS 자격증명을 찾을 수 없습니다: {e}")))?;
            let signed = sign_json_request(
                &credentials,
                &self.region,
                &self.endpoint,
                action,
                body.as_bytes(),
                self.clock.now(),
            );
            let headers: Vec<(&str, &str)> = signed
                .headers
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str()))
                .collect();
            match self
                .http
                .request(Method::Post, &signed.url, &headers, Some(body.as_bytes()))
            {
                Ok(response) if response.is_success() => {
                    return serde_json::from_slice(&response.body).map_err(|e| {
                        LogsError(format!(
                            "CloudWatch Logs {action} 응답을 읽을 수 없습니다: {e}"
                        ))
                    });
                }
                Ok(response) => {
                    let text = response.body_text();
                    last = LogsError(format!(
                        "CloudWatch Logs {action} 실패: status={}, {text}",
                        response.status
                    ));
                    let throttled = response.status == 429 || text.contains("ThrottlingException");
                    if response.status < 500 && !throttled {
                        return Err(last);
                    }
                }
                Err(e) => {
                    last = LogsError(format!("CloudWatch Logs {action} 전송 실패: {e}"));
                }
            }
        }
        Err(last)
    }
}

impl LogsInsightsClient for HttpLogsInsightsClient {
    fn start_query(&self, request: &StartQuery) -> Result<String, LogsError> {
        let body = format!(
            "{{\"logGroupName\":{},\"startTime\":{},\"endTime\":{},\"queryString\":{}}}",
            json_string(&request.log_group),
            request.start_time,
            request.end_time,
            json_string(&request.query_string),
        );
        let response = self.call("StartQuery", &body)?;
        response["queryId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| LogsError("StartQuery 응답에 queryId가 없습니다".to_owned()))
    }

    fn get_query_results(&self, query_id: &str) -> Result<QueryResults, LogsError> {
        let body = format!("{{\"queryId\":{}}}", json_string(query_id));
        let response = self.call("GetQueryResults", &body)?;
        let status = QueryStatus::parse(response["status"].as_str());
        let results = response["results"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        row.as_array()
                            .map(|fields| {
                                fields
                                    .iter()
                                    .map(|f| ResultField {
                                        field: f["field"].as_str().unwrap_or_default().to_owned(),
                                        value: f["value"].as_str().map(str::to_owned),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(QueryResults { status, results })
    }

    fn stop_query(&self, query_id: &str) -> Result<(), LogsError> {
        let body = format!("{{\"queryId\":{}}}", json_string(query_id));
        self.call("StopQuery", &body).map(|_| ())
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("문자열은 항상 JSON으로 쓸 수 있다")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use chalkak_core::FixedClock;

    use super::*;

    const LOG_GROUP: &str = "/chalkak/test/application";
    const RETENTION_DAYS: i64 = 7;

    fn metric_date() -> Date {
        "2026-10-02".parse().unwrap()
    }

    fn instant(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[derive(Default)]
    struct FakeClient {
        started: Mutex<Vec<StartQuery>>,
        stopped: Mutex<Vec<String>>,
        get_calls: Mutex<Vec<String>>,
        /// 쿼리 시작마다 돌려줄 ID(없으면 "q").
        ids: Mutex<VecDeque<String>>,
        /// 쿼리 ID별 결과 순서. 마지막 값은 계속 돌려준다. 없는 ID는 `fallback`.
        results: Mutex<Vec<(String, VecDeque<QueryResults>)>>,
        fallback: Mutex<Option<QueryResults>>,
    }

    impl LogsInsightsClient for FakeClient {
        fn start_query(&self, request: &StartQuery) -> Result<String, LogsError> {
            self.started.lock().unwrap().push(request.clone());
            Ok(self
                .ids
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "q".into()))
        }

        fn get_query_results(&self, query_id: &str) -> Result<QueryResults, LogsError> {
            self.get_calls.lock().unwrap().push(query_id.to_owned());
            let mut results = self.results.lock().unwrap();
            if let Some((_, queue)) = results.iter_mut().find(|(id, _)| id == query_id) {
                let next = if queue.len() > 1 {
                    queue.pop_front().unwrap()
                } else {
                    queue.front().unwrap().clone()
                };
                return Ok(next);
            }
            Ok(self
                .fallback
                .lock()
                .unwrap()
                .clone()
                .expect("결과를 정하지 않았다"))
        }

        fn stop_query(&self, query_id: &str) -> Result<(), LogsError> {
            self.stopped.lock().unwrap().push(query_id.to_owned());
            Ok(())
        }
    }

    fn row(pairs: &[(&str, &str)]) -> Vec<ResultField> {
        pairs.iter().map(|(f, v)| ResultField::new(f, v)).collect()
    }

    fn complete(rows: Vec<Vec<ResultField>>) -> QueryResults {
        QueryResults {
            status: QueryStatus::Complete,
            results: rows,
        }
    }

    fn complete_all(value: &str) -> QueryResults {
        complete(vec![row(&[
            ("visitors", value),
            ("topicViewers", value),
            ("requests", value),
            ("server5xx", value),
            ("latencyAvg", value),
            ("latencyP95", value),
            ("postCreations", value),
        ])])
    }

    fn status_only(status: QueryStatus) -> QueryResults {
        QueryResults {
            status,
            results: Vec::new(),
        }
    }

    struct Fixture {
        client: Arc<FakeClient>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    fn fixture() -> Fixture {
        Fixture {
            client: Arc::new(FakeClient::default()),
            sleeps: Arc::default(),
        }
    }

    impl Fixture {
        fn stub_all(&self, results: QueryResults) {
            *self.client.fallback.lock().unwrap() = Some(results);
        }

        fn reader_at(
            &self,
            excluded: &[Uuid],
            now: Timestamp,
        ) -> CloudWatchLogsAccessLogMetricsReader {
            let sleeps = Arc::clone(&self.sleeps);
            CloudWatchLogsAccessLogMetricsReader::new(
                self.client.clone(),
                &AnalyticsSettings {
                    excluded_user_ids: excluded.to_vec(),
                    log_group: LOG_GROUP.to_owned(),
                    log_metrics_start_date: metric_date(),
                    log_retention_days: RETENTION_DAYS,
                    region: "ap-northeast-2".to_owned(),
                },
                Arc::new(FixedClock::new(now)),
                Arc::new(move |d| sleeps.lock().unwrap().push(d)),
                Duration::from_secs(1),
                Duration::from_secs(60),
            )
        }

        /// 기준일 다음 날 0시(KST)가 현재다.
        fn reader(&self, excluded: &[Uuid]) -> CloudWatchLogsAccessLogMetricsReader {
            self.reader_at(excluded, instant("2026-10-02T15:00:00Z"))
        }

        fn started(&self) -> Vec<StartQuery> {
            self.client.started.lock().unwrap().clone()
        }
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_startsQueriesWithLogGroupAndEpochSecondWindow
    #[test]
    fn 로그_그룹과_kst_하루의_epoch_초_구간으로_쿼리를_시작한다() {
        let f = fixture();
        f.stub_all(complete_all("0"));

        f.reader(&[]).read(metric_date()).unwrap();

        let started = f.started();
        assert_eq!(started.len(), 5);
        for request in &started {
            assert_eq!(request.log_group, LOG_GROUP);
            assert_eq!(request.start_time, 1_790_866_800);
            assert_eq!(request.end_time, 1_790_953_199);
        }
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_queryStrings_containExpectedFilters
    #[test]
    fn 쿼리_문자열에_기대한_필터와_집계가_들어간다() {
        let f = fixture();
        f.stub_all(complete_all("0"));

        f.reader(&[]).read(metric_date()).unwrap();

        let queries: Vec<String> = f.started().into_iter().map(|r| r.query_string).collect();
        assert_eq!(
            queries,
            [
                "filter type = \"access\" and ispresent(userId)\n| stats count_distinct(userId) as visitors",
                "filter type = \"access\" and method = \"GET\" and route = \"/api/v1/topics\" and status = 200 and ispresent(userId)\n| stats count_distinct(userId) as topicViewers",
                "filter type = \"access\"\n| stats count(*) as requests, avg(durationMs) as latencyAvg, pct(durationMs, 95) as latencyP95",
                "filter type = \"access\" and status >= 500\n| stats count(*) as server5xx",
                "filter type = \"access\" and method = \"POST\" and route = \"/api/v1/posts\" and status = 201\n| stats count(*) as postCreations",
            ]
        );
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_excludedUsers_filtersOnlyUserBasedQueries
    #[test]
    fn 제외_회원이_있으면_회원_기준_쿼리에만_제외_조건을_붙인다() {
        let f = fixture();
        f.stub_all(complete_all("0"));
        let first: Uuid = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let second: Uuid = "00000000-0000-0000-0000-000000000002".parse().unwrap();

        f.reader(&[first, second]).read(metric_date()).unwrap();

        let clause = format!("| filter userId not in [\"{first}\",\"{second}\"]");
        let queries: Vec<String> = f.started().into_iter().map(|r| r.query_string).collect();
        assert!(queries[0].contains(&clause));
        assert!(queries[1].contains(&clause));
        assert!(!queries[2].contains("not in"));
        assert!(!queries[3].contains("not in"));
        assert!(queries[4].contains(&clause));
        // 제외 조건은 집계 앞 줄에 들어간다.
        assert!(queries[0].ends_with(&format!(
            "{clause}\n| stats count_distinct(userId) as visitors"
        )));
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_noExcludedUsers_hasNoExcludeClause
    #[test]
    fn 제외_회원이_없으면_제외_조건을_붙이지_않는다() {
        let f = fixture();
        f.stub_all(complete_all("0"));

        f.reader(&[]).read(metric_date()).unwrap();

        assert!(
            f.started()
                .iter()
                .all(|r| !r.query_string.contains("not in"))
        );
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_parsesResultFields
    #[test]
    fn 결과_필드를_지표로_매핑하고_평균_응답_시간은_반올림한다() {
        let f = fixture();
        *f.client.ids.lock().unwrap() = ["q1", "q2", "q3", "q4", "q5"]
            .map(String::from)
            .into_iter()
            .collect();
        *f.client.results.lock().unwrap() = vec![
            (
                "q1".into(),
                [complete(vec![row(&[("visitors", "7")])])].into(),
            ),
            (
                "q2".into(),
                [complete(vec![row(&[("topicViewers", "6")])])].into(),
            ),
            (
                "q3".into(),
                [complete(vec![row(&[
                    ("requests", "1200"),
                    ("latencyAvg", "12.6"),
                    ("latencyP95", "80.4"),
                ])])]
                .into(),
            ),
            (
                "q4".into(),
                [complete(vec![row(&[("server5xx", "3")])])].into(),
            ),
            (
                "q5".into(),
                [complete(vec![row(&[("postCreations", "9")])])].into(),
            ),
        ];

        let metrics = f.reader(&[]).read(metric_date()).unwrap().unwrap();

        assert_eq!(
            metrics,
            AccessLogMetrics {
                visitors: 7,
                topic_viewers: 6,
                requests: 1200,
                server_5xx: 3,
                latency_avg_ms: 13,
                latency_p95_ms: 80,
                post_creations: 9,
            }
        );
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_emptyResults_returnsZeros
    #[test]
    fn 결과_행이_없으면_모든_지표를_0으로_센다() {
        let f = fixture();
        f.stub_all(complete(Vec::new()));

        let metrics = f.reader(&[]).read(metric_date()).unwrap().unwrap();

        assert_eq!(
            metrics,
            AccessLogMetrics {
                visitors: 0,
                topic_viewers: 0,
                requests: 0,
                server_5xx: 0,
                latency_avg_ms: 0,
                latency_p95_ms: 0,
                post_creations: 0,
            }
        );
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_blankOrNanValues_areZero
    #[test]
    fn 값이_비었거나_nan이면_0으로_센다() {
        let f = fixture();
        f.stub_all(complete(vec![row(&[
            ("visitors", ""),
            ("topicViewers", "NaN"),
            ("requests", "5"),
        ])]));

        let metrics = f.reader(&[]).read(metric_date()).unwrap().unwrap();

        assert_eq!(metrics.visitors, 0);
        assert_eq!(metrics.topic_viewers, 0);
        assert_eq!(metrics.requests, 5);
    }

    #[test]
    fn 숫자가_아닌_값은_오류다() {
        let f = fixture();
        f.stub_all(complete(vec![row(&[("visitors", "many")])]));

        assert!(matches!(
            f.reader(&[]).read(metric_date()),
            Err(ApiError::Internal(_))
        ));
    }

    #[test]
    fn 반올림은_0_5를_올리고_음수_0_5는_0이다() {
        assert_eq!(java_round(12.5), 13);
        assert_eq!(java_round(12.4), 12);
        assert_eq!(java_round(-0.5), 0);
        assert_eq!(java_round(-1.5), -1);
        assert_eq!(java_round(f64::INFINITY), i64::MAX);
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_runningThenComplete_pollsWithInterval
    #[test]
    fn running_상태면_간격을_두고_다시_조회해_complete를_받는다() {
        let f = fixture();
        *f.client.results.lock().unwrap() = vec![(
            "q".into(),
            [
                status_only(QueryStatus::Running),
                status_only(QueryStatus::Running),
                complete_all("1"),
            ]
            .into(),
        )];

        f.reader(&[]).read(metric_date()).unwrap();

        // 첫 쿼리가 Running 두 번 뒤 Complete, 나머지 네 쿼리는 바로 Complete(마지막 값을 계속 돌려준다)
        assert_eq!(*f.sleeps.lock().unwrap(), [Duration::from_secs(1); 2]);
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_terminalFailureStatus_throws
    #[test]
    fn failed_cancelled_timeout_unknown_상태면_오류다() {
        for status in [
            QueryStatus::Failed,
            QueryStatus::Cancelled,
            QueryStatus::Timeout,
            QueryStatus::Unknown("Surprise".into()),
        ] {
            let f = fixture();
            f.stub_all(status_only(status));

            let error = f.reader(&[]).read(metric_date()).unwrap_err();

            match error {
                ApiError::Internal(message) => assert!(message.contains("queryId=q"), "{message}"),
                other => panic!("{other}"),
            }
        }
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_exceedsMaxWait_stopsQueryAndThrows
    #[test]
    fn 대기_한도를_넘기면_쿼리를_중단하고_오류를_돌려준다() {
        let f = fixture();
        *f.client.ids.lock().unwrap() = VecDeque::from(["slow".to_owned()]);
        f.stub_all(status_only(QueryStatus::Running));
        *f.client.results.lock().unwrap() = Vec::new();

        let error = f.reader(&[]).read(metric_date()).unwrap_err();

        match error {
            ApiError::Internal(message) => assert!(message.contains("시간 초과"), "{message}"),
            other => panic!("{other}"),
        }
        // 60초 한도, 1초 간격이면 60번 조회하고 60번 기다린다.
        assert_eq!(f.client.get_calls.lock().unwrap().len(), 60);
        assert_eq!(f.sleeps.lock().unwrap().len(), 60);
        assert_eq!(*f.client.stopped.lock().unwrap(), ["slow"]);
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_firstQueryFails_stopsRemainingQueries
    #[test]
    fn 한_쿼리가_실패하면_이후_쿼리를_시작하지_않는다() {
        let f = fixture();
        f.stub_all(status_only(QueryStatus::Failed));

        assert!(f.reader(&[]).read(metric_date()).is_err());

        assert_eq!(f.started().len(), 1);
        assert!(f.client.stopped.lock().unwrap().is_empty());
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_beforeStartDate_returnsEmptyWithoutQueries
    #[test]
    fn 관측_시작일_이전_날짜는_쿼리를_시작하지_않고_빈_값을_돌려준다() {
        let f = fixture();

        let before = metric_date().yesterday().unwrap();
        assert_eq!(f.reader(&[]).read(before).unwrap(), None);
        assert!(f.started().is_empty());
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_onStartDate_runsQueries
    #[test]
    fn 관측_시작일_당일부터는_쿼리를_실행한다() {
        let f = fixture();
        f.stub_all(complete_all("1"));

        assert!(f.reader(&[]).read(metric_date()).unwrap().is_some());
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_dayStartAtRetentionBoundary_runsQueries
    #[test]
    fn 하루의_시작이_보존_기간_경계와_정확히_같으면_쿼리를_실행한다() {
        let f = fixture();
        f.stub_all(complete_all("1"));
        // 현재가 10-09 00:00 KST이고 보존 7일이면 경계는 10-02 00:00 KST이며, 10-02의 시작과 같다
        let reader = f.reader_at(&[], instant("2026-10-08T15:00:00Z"));

        assert!(reader.read(metric_date()).unwrap().is_some());
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_dayStartJustBeforeRetentionBoundary_returnsEmptyWithoutQueries
    #[test]
    fn 하루의_시작이_보존_기간_경계보다_1초라도_앞서면_쿼리_없이_빈_값을_돌려준다() {
        let f = fixture();
        // 현재가 10-09 00:00:01 KST이면 경계는 10-02 00:00:01 KST라 10-02 00:00:00은 1초 만료됐다
        let reader = f.reader_at(&[], instant("2026-10-08T15:00:01Z"));

        assert_eq!(reader.read(metric_date()).unwrap(), None);
        assert!(f.started().is_empty());
    }

    // CloudWatchLogsAccessLogMetricsReaderTest#read_fullyExpiredDate_returnsEmptyWithoutQueries
    #[test]
    fn 보존_기간이_완전히_지난_날짜는_쿼리_없이_빈_값을_돌려준다() {
        let f = fixture();
        // 현재가 10-30 00:00 KST이면 경계는 10-23이라 10-02는 보존 기간보다 3주 전이다
        let reader = f.reader_at(&[], instant("2026-10-29T15:00:00Z"));

        assert_eq!(reader.read(metric_date()).unwrap(), None);
        assert!(f.started().is_empty());
    }

    #[test]
    fn 쿼리_상태_문구는_원본_enum_이름이다() {
        assert_eq!(QueryStatus::parse(Some("Complete")), QueryStatus::Complete);
        assert_eq!(
            QueryStatus::parse(Some("Scheduled")),
            QueryStatus::Scheduled
        );
        assert_eq!(QueryStatus::Failed.to_string(), "FAILED");
        assert_eq!(
            QueryStatus::parse(Some("Surprise")).to_string(),
            "UNKNOWN_TO_SDK_VERSION"
        );
        assert_eq!(
            QueryStatus::parse(None),
            QueryStatus::Unknown("null".into())
        );
    }
}
