//! 지표 모델과 집계 서비스(원본 `UserMetricsAggregationService`, `DailyUserMetrics`, `AccessLogMetrics`,
//! `WeeklyRetention`, `AccessLogMetricsReader`, `UserMetricsRepository`).
//!
//! 집계 결과는 `chalkak.aggregation` 로거의 INFO 로그 한 줄이다. 키 순서는 대시보드·알람이 읽는 모양 그대로다:
//! `type`, `metricDate`, `dau`, `posts`, `wau`, `eligibleUsers`, `likesTotal`, (접근 로그 지표 6개), (일요일이면 주차별
//! 유지율).

use std::sync::{Arc, LazyLock};

use chalkak_core::log::{Logger, fields};
use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

use crate::web::ApiError;

/// 원본 로거 이름 `LoggerFactory.getLogger("chalkak.aggregation")`.
pub const AGGREGATION_LOGGER: &str = "chalkak.aggregation";

const WEEK_DAYS: i64 = 7;
const RETENTION_WEEKS: i32 = 4;

pub static KST: LazyLock<TimeZone> =
    LazyLock::new(|| TimeZone::get("Asia/Seoul").expect("번들 시간대 데이터에 Asia/Seoul이 있다"));

/// KST 날짜의 0시 시각(`date.atStartOfDay(KST).toInstant()`).
pub fn start_of_day(date: Date) -> Timestamp {
    date.to_zoned(KST.clone())
        .expect("집계 날짜는 표현 가능한 범위 안이다")
        .timestamp()
}

/// 한 KST 날짜의 접근 로그에서 센 지표.
///
/// `visitors`(userId가 남은 요청을 보낸 회원 수), `topic_viewers`(주제 조회에 성공한 회원 수), `post_creations`(게시물 생성에
/// 성공한 요청 수)는 제외 회원을 뺀다. `requests`, `server_5xx`, `latency_avg_ms`, `latency_p95_ms`는 서버 상태를 보는
/// 값이라 제외 회원을 빼지 않는다. `post_creations`는 DB의 `posts`와 맞춰 보는 교차 검증용이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessLogMetrics {
    pub visitors: i64,
    pub topic_viewers: i64,
    pub requests: i64,
    pub server_5xx: i64,
    pub latency_avg_ms: i64,
    pub latency_p95_ms: i64,
    pub post_creations: i64,
}

/// 기준 주보다 `weeks_after`주 앞선 코호트 주(`cohort_week_start`부터 7일)의 게시물 작성 회원 중, 기준 주에도 게시물을 올린
/// 회원 수.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeeklyRetention {
    pub weeks_after: i32,
    pub cohort_week_start: Date,
    pub cohort_size: i64,
    pub retained_users: i64,
}

impl WeeklyRetention {
    /// 유지율(%)을 소수 첫째 자리까지 반올림한 값(HALF_UP). 코호트가 비면 `None`.
    pub fn rate_percent(&self) -> Option<f64> {
        if self.cohort_size == 0 {
            return None;
        }
        // BigDecimal(retained * 100) / cohort를 소수 첫째 자리 HALF_UP으로 나눈 값. 정수 연산으로 같은 결과를 낸다:
        // tenths = round_half_up(retained * 1000 / cohort).
        let numerator = i128::from(self.retained_users) * 1000;
        let denominator = i128::from(self.cohort_size);
        let tenths = (numerator * 2 + denominator) / (denominator * 2);
        Some(tenths as f64 / 10.0)
    }
}

/// 한 KST 날짜의 사용자 지표.
///
/// `dau`(그날 게시물을 올린 회원 수), `posts`(그날 생성된 게시물 수), `wau`(그날까지 7일 동안 게시물을 올린 회원 수)는 게시물
/// 생성 시각으로 정해지므로 같은 날짜를 다시 집계해도 같다. `eligible_users`(활동 가능한 회원 수)와 `likes_total`(삭제되지
/// 않은 게시물의 좋아요 수)은 집계를 돌린 시점의 스냅샷이라 다시 돌리면 달라질 수 있다. `access_log_metrics`는 로그 지표 관측
/// 시작일 이전 날짜에는 `None`이다. `weekly_retentions`는 일요일 기준일에만 W+1~W+4 순서로 채우고 다른 요일에는 비어 있다.
#[derive(Clone, Debug, PartialEq)]
pub struct DailyUserMetrics {
    pub metric_date: Date,
    pub dau: i64,
    pub posts: i64,
    pub wau: i64,
    pub eligible_users: i64,
    pub likes_total: i64,
    pub access_log_metrics: Option<AccessLogMetrics>,
    pub weekly_retentions: Vec<WeeklyRetention>,
}

/// 사용자 지표 집계용 조회. 지표에서 제외할 회원은 구현체가 걸러낸다. 구간은 모두 `[from, to)`이다.
pub trait UserMetricsRepository: Send + Sync {
    /// 구간에 게시물을 만든 회원 수. 삭제됐거나 거절된 게시물도 센다.
    fn count_post_authors(&self, from: Timestamp, to: Timestamp) -> Result<i64, ApiError>;

    /// 구간에 만들어진 게시물 수. 삭제됐거나 거절된 게시물도 센다.
    fn count_posts(&self, from: Timestamp, to: Timestamp) -> Result<i64, ApiError>;

    /// 코호트 구간에 게시물을 만든 회원 중 복귀 구간에도 게시물을 만든 회원 수.
    fn count_returning_post_authors(
        &self,
        cohort_from: Timestamp,
        cohort_to: Timestamp,
        return_from: Timestamp,
        return_to: Timestamp,
    ) -> Result<i64, ApiError>;

    /// 탈퇴하지 않았고 정지되지 않은 회원 수.
    fn count_eligible_users(&self) -> Result<i64, ApiError>;

    /// 삭제되지 않은 게시물에 달린 좋아요 수.
    fn count_likes_on_live_posts(&self) -> Result<i64, ApiError>;
}

/// 접근 로그 집계 조회. 지표에서 제외할 회원은 구현체가 걸러낸다. 조회에 실패하면 오류를 돌려준다.
pub trait AccessLogMetricsReader: Send + Sync {
    /// KST 하루(`metric_date` 0시부터 다음 날 0시 전까지)의 접근 로그에서 지표를 센다. `None`은 관측 시작일 이전이거나 로그
    /// 보존 기간이 지나 접근 로그 지표가 없다는 뜻이다.
    fn read(&self, metric_date: Date) -> Result<Option<AccessLogMetrics>, ApiError>;
}

pub struct UserMetricsAggregationService {
    repository: Arc<dyn UserMetricsRepository>,
    access_log_reader: Arc<dyn AccessLogMetricsReader>,
    logger: Arc<Logger>,
}

impl UserMetricsAggregationService {
    pub fn new(
        repository: Arc<dyn UserMetricsRepository>,
        access_log_reader: Arc<dyn AccessLogMetricsReader>,
        logger: Arc<Logger>,
    ) -> Self {
        UserMetricsAggregationService {
            repository,
            access_log_reader,
            logger,
        }
    }

    /// KST 하루의 사용자 지표를 DB에서 세어 한 줄로 남긴다.
    ///
    /// 게시물 생성 시각으로 정해지는 `dau`, `posts`, `wau`는 같은 날짜를 다시 집계해도 같다. `eligible_users`,
    /// `likes_total`은 집계 시점의 스냅샷이라 다시 돌린 시각에 따라 달라질 수 있다. 게시물 생성은 업로드 요청이 성공한 시점이므로
    /// 삭제됐거나 거절된 게시물도 센다.
    ///
    /// 로그 지표 관측 시작일 이후 날짜는 접근 로그 지표도 같은 줄에 남기고, 로그의 게시물 생성 수가 DB의 `posts`와 다르면 경고를
    /// 남긴다. 일요일 기준일은 그 주(월~일)를 기준으로 1~4주 전 주에 게시물을 올린 회원 중 기준 주에도 올린 회원 수와 유지율을
    /// `retentionW{k}Cohort/Retained/Rate`로 남기며, 코호트가 비면 Rate는 남기지 않는다. 로그 조회가 하나라도 실패하면 성공 로그
    /// 없이 오류를 돌려준다.
    pub fn aggregate(&self, metric_date: Date) -> Result<DailyUserMetrics, ApiError> {
        let day_start = start_of_day(metric_date);
        let day_end = start_of_day(plus_days(metric_date, 1));
        let week_start = start_of_day(plus_days(metric_date, -(WEEK_DAYS - 1)));

        let dau = self.repository.count_post_authors(day_start, day_end)?;
        let posts = self.repository.count_posts(day_start, day_end)?;
        let wau = self.repository.count_post_authors(week_start, day_end)?;
        let eligible_users = self.repository.count_eligible_users()?;
        let likes_total = self.repository.count_likes_on_live_posts()?;
        let weekly_retentions = self.weekly_retentions(metric_date)?;
        let access_log_metrics = self.access_log_reader.read(metric_date)?;

        let metrics = DailyUserMetrics {
            metric_date,
            dau,
            posts,
            wau,
            eligible_users,
            likes_total,
            access_log_metrics,
            weekly_retentions,
        };
        self.warn_if_post_counts_differ(&metrics);
        self.log(&metrics);
        Ok(metrics)
    }

    /// 일요일 기준일에만, 기준 주(월~일)에 대한 1~4주 전 코호트의 유지 회원 수를 센다.
    fn weekly_retentions(&self, metric_date: Date) -> Result<Vec<WeeklyRetention>, ApiError> {
        if metric_date.weekday() != Weekday::Sunday {
            return Ok(Vec::new());
        }
        let target_week_start = plus_days(metric_date, -(WEEK_DAYS - 1));
        let target_start = start_of_day(target_week_start);
        let target_end = start_of_day(plus_days(metric_date, 1));
        let mut retentions = Vec::new();
        for weeks_after in 1..=RETENTION_WEEKS {
            let cohort_week_start =
                plus_days(target_week_start, -WEEK_DAYS * i64::from(weeks_after));
            let cohort_start = start_of_day(cohort_week_start);
            let cohort_end = start_of_day(plus_days(cohort_week_start, WEEK_DAYS));
            retentions.push(WeeklyRetention {
                weeks_after,
                cohort_week_start,
                cohort_size: self
                    .repository
                    .count_post_authors(cohort_start, cohort_end)?,
                retained_users: self.repository.count_returning_post_authors(
                    cohort_start,
                    cohort_end,
                    target_start,
                    target_end,
                )?,
            });
        }
        Ok(retentions)
    }

    fn warn_if_post_counts_differ(&self, metrics: &DailyUserMetrics) {
        let Some(access) = &metrics.access_log_metrics else {
            return;
        };
        if access.post_creations == metrics.posts {
            return;
        }
        self.logger
            .warn(AGGREGATION_LOGGER)
            .field(fields::TYPE, fields::TYPE_AGGREGATION)
            .field(fields::EVENT, "posts_mismatch")
            .field(fields::METRIC_DATE, metrics.metric_date.to_string())
            .field("dbPosts", metrics.posts)
            .field("logPosts", access.post_creations)
            .log("DB와 접근 로그의 게시물 수가 다름");
    }

    fn log(&self, metrics: &DailyUserMetrics) {
        let mut event = self
            .logger
            .info(AGGREGATION_LOGGER)
            .field(fields::TYPE, fields::TYPE_AGGREGATION)
            .field(fields::METRIC_DATE, metrics.metric_date.to_string())
            .field("dau", metrics.dau)
            .field("posts", metrics.posts)
            .field("wau", metrics.wau)
            .field("eligibleUsers", metrics.eligible_users)
            .field("likesTotal", metrics.likes_total);
        if let Some(access) = &metrics.access_log_metrics {
            event = event
                .field("visitors", access.visitors)
                .field("topicViewers", access.topic_viewers)
                .field("requests", access.requests)
                .field("server5xx", access.server_5xx)
                .field("latencyAvgMs", access.latency_avg_ms)
                .field("latencyP95Ms", access.latency_p95_ms);
        }
        for retention in &metrics.weekly_retentions {
            let prefix = format!("retentionW{}", retention.weeks_after);
            event = event
                .field(&format!("{prefix}Cohort"), retention.cohort_size)
                .field(&format!("{prefix}Retained"), retention.retained_users);
            if let Some(rate) = retention.rate_percent() {
                event = event.field(&format!("{prefix}Rate"), rate);
            }
        }
        event.log("사용자 지표 집계");
    }
}

/// `LocalDate.plusDays`. 집계 날짜는 현실적인 범위라 범위를 넘으면 가장 가까운 끝값에서 멈춘다.
fn plus_days(date: Date, days: i64) -> Date {
    date.checked_add(Span::new().days(days)).unwrap_or(date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::testing::{Lines, logger};

    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }

    /// 저장소를 대신하는 값. `posts`는 `count_posts` 값이고 그 밖은 모두 같은 값이다.
    struct FakeRepository {
        posts: i64,
        authors: i64,
        returning: i64,
    }

    impl UserMetricsRepository for FakeRepository {
        fn count_post_authors(&self, _: Timestamp, _: Timestamp) -> Result<i64, ApiError> {
            Ok(self.authors)
        }
        fn count_posts(&self, _: Timestamp, _: Timestamp) -> Result<i64, ApiError> {
            Ok(self.posts)
        }
        fn count_returning_post_authors(
            &self,
            _: Timestamp,
            _: Timestamp,
            _: Timestamp,
            _: Timestamp,
        ) -> Result<i64, ApiError> {
            Ok(self.returning)
        }
        fn count_eligible_users(&self) -> Result<i64, ApiError> {
            Ok(10)
        }
        fn count_likes_on_live_posts(&self) -> Result<i64, ApiError> {
            Ok(20)
        }
    }

    struct FakeReader(Result<Option<AccessLogMetrics>, String>);

    impl AccessLogMetricsReader for FakeReader {
        fn read(&self, _: Date) -> Result<Option<AccessLogMetrics>, ApiError> {
            self.0.clone().map_err(ApiError::Internal)
        }
    }

    const DB_POSTS: i64 = 3;
    const START_DATE: &str = "2026-10-02";
    const SUNDAY: &str = "2026-10-04";

    fn log_metrics(post_creations: i64) -> AccessLogMetrics {
        AccessLogMetrics {
            visitors: 5,
            topic_viewers: 4,
            requests: 120,
            server_5xx: 2,
            latency_avg_ms: 35,
            latency_p95_ms: 210,
            post_creations,
        }
    }

    fn service(
        authors: i64,
        returning: i64,
        reader: Result<Option<AccessLogMetrics>, String>,
        lines: &Lines,
    ) -> UserMetricsAggregationService {
        UserMetricsAggregationService::new(
            Arc::new(FakeRepository {
                posts: DB_POSTS,
                authors,
                returning,
            }),
            Arc::new(FakeReader(reader)),
            logger(lines),
        )
    }

    // WeeklyRetentionTest#ratePercent_roundsHalfUpToOneDecimal
    #[test]
    fn 유지율은_소수_첫째_자리까지_반올림한다() {
        let rate = |size, retained| {
            WeeklyRetention {
                weeks_after: 1,
                cohort_week_start: date("2030-03-04"),
                cohort_size: size,
                retained_users: retained,
            }
            .rate_percent()
        };
        assert_eq!(rate(16, 1), Some(6.3));
        assert_eq!(rate(3, 2), Some(66.7));
        assert_eq!(rate(8, 1), Some(12.5));
        assert_eq!(rate(3, 1), Some(33.3));
        assert_eq!(rate(4, 4), Some(100.0));
        assert_eq!(rate(4, 0), Some(0.0));
    }

    // WeeklyRetentionTest#ratePercent_emptyCohort_isNull
    #[test]
    fn 코호트가_비면_유지율은_없다() {
        let retention = WeeklyRetention {
            weeks_after: 1,
            cohort_week_start: date("2030-03-04"),
            cohort_size: 0,
            retained_users: 0,
        };
        assert_eq!(retention.rate_percent(), None);
    }

    #[test]
    fn kst_하루의_시작은_전날_utc_15시다() {
        // 원본 CloudWatch 리더 시험의 1_790_866_800: 2026-10-02 00:00 KST
        assert_eq!(start_of_day(date("2026-10-02")).as_second(), 1_790_866_800);
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_emptyLogMetrics_logsDbMetricsOnly
    #[test]
    fn 접근_로그_지표가_비어_있으면_로그_지표_키를_남기지_않고_db_지표만_남긴다() {
        let lines = Lines::default();
        let metrics = service(0, 0, Ok(None), &lines)
            .aggregate(date(START_DATE).yesterday().unwrap())
            .unwrap();

        assert_eq!(metrics.access_log_metrics, None);
        let events = lines.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            lines.keys(0),
            [
                "type",
                "metricDate",
                "dau",
                "posts",
                "wau",
                "eligibleUsers",
                "likesTotal"
            ]
        );
        assert_eq!(events[0]["message"], "사용자 지표 집계");
        assert_eq!(events[0]["logger_name"], "chalkak.aggregation");
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_onStartDate_logsLogMetricsInSameLine
    #[test]
    fn 접근_로그_지표가_있으면_같은_줄에_지표를_남긴다() {
        let lines = Lines::default();
        let metrics = service(0, 0, Ok(Some(log_metrics(DB_POSTS))), &lines)
            .aggregate(date(START_DATE))
            .unwrap();

        assert_eq!(metrics.access_log_metrics.unwrap().requests, 120);
        let events = lines.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            lines.keys(0),
            [
                "type",
                "metricDate",
                "dau",
                "posts",
                "wau",
                "eligibleUsers",
                "likesTotal",
                "visitors",
                "topicViewers",
                "requests",
                "server5xx",
                "latencyAvgMs",
                "latencyP95Ms"
            ]
        );
        for (key, value) in [
            ("visitors", 5),
            ("topicViewers", 4),
            ("requests", 120),
            ("server5xx", 2),
            ("latencyAvgMs", 35),
            ("latencyP95Ms", 210),
        ] {
            assert_eq!(events[0][key], value, "{key}");
        }
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_postCountMismatch_logsWarnAndSucceeds
    #[test]
    fn 로그의_게시물_생성_수가_db와_다르면_경고를_남기고_집계는_성공한다() {
        let lines = Lines::default();
        let result =
            service(0, 0, Ok(Some(log_metrics(DB_POSTS + 1))), &lines).aggregate(date(START_DATE));

        assert!(result.is_ok());
        let events = lines.events();
        assert_eq!(events.len(), 2);
        let warn = &events[0];
        assert_eq!(warn["level"], "WARN");
        assert_eq!(warn["type"], "aggregation");
        assert_eq!(warn["event"], "posts_mismatch");
        assert_eq!(warn["metricDate"], "2026-10-02");
        assert_eq!(warn["dbPosts"], DB_POSTS);
        assert_eq!(warn["logPosts"], DB_POSTS + 1);
        assert_eq!(events[1]["level"], "INFO");
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_postCountMatches_noWarn
    #[test]
    fn 게시물_수가_같으면_경고를_남기지_않는다() {
        let lines = Lines::default();
        service(0, 0, Ok(Some(log_metrics(DB_POSTS))), &lines)
            .aggregate(date(START_DATE))
            .unwrap();

        assert!(lines.events().iter().all(|e| e["level"] != "WARN"));
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_readerFails_throwsWithoutSuccessLine
    #[test]
    fn 로그_조회가_실패하면_오류를_돌려주고_성공_로그를_남기지_않는다() {
        let lines = Lines::default();
        let result = service(0, 0, Err("Logs Insights 쿼리 실패".into()), &lines)
            .aggregate(date(START_DATE));

        assert!(matches!(result, Err(ApiError::Internal(_))));
        assert!(lines.events().is_empty());
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_sunday_logsRetentionKeys
    #[test]
    fn 일요일_기준일은_코호트_인원_유지_회원_수_유지율을_같은_줄에_남긴다() {
        let lines = Lines::default();
        service(3, 2, Ok(None), &lines)
            .aggregate(date(SUNDAY))
            .unwrap();

        let events = lines.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            lines.keys(0),
            [
                "type",
                "metricDate",
                "dau",
                "posts",
                "wau",
                "eligibleUsers",
                "likesTotal",
                "retentionW1Cohort",
                "retentionW1Retained",
                "retentionW1Rate",
                "retentionW2Cohort",
                "retentionW2Retained",
                "retentionW2Rate",
                "retentionW3Cohort",
                "retentionW3Retained",
                "retentionW3Rate",
                "retentionW4Cohort",
                "retentionW4Retained",
                "retentionW4Rate"
            ]
        );
        assert_eq!(events[0]["retentionW1Cohort"], 3);
        assert_eq!(events[0]["retentionW1Retained"], 2);
        assert_eq!(events[0]["retentionW1Rate"], 66.7);
    }

    // UserMetricsAggregationLogMetricsTest#aggregate_emptyCohort_omitsRateKey
    #[test]
    fn 코호트가_비면_유지율_키는_남기지_않고_인원과_유지_회원_수는_0으로_남긴다() {
        let lines = Lines::default();
        service(0, 0, Ok(None), &lines)
            .aggregate(date(SUNDAY))
            .unwrap();

        let events = lines.events();
        assert_eq!(events[0]["retentionW1Cohort"], 0);
        assert_eq!(events[0]["retentionW1Retained"], 0);
        assert!(lines.keys(0).iter().all(|k| !k.ends_with("Rate")));
    }

    #[test]
    fn 일요일이_아니면_유지율을_계산하지_않고_로그에도_남기지_않는다() {
        let lines = Lines::default();
        let metrics = service(3, 2, Ok(None), &lines)
            .aggregate(date("2026-10-05"))
            .unwrap();

        assert!(metrics.weekly_retentions.is_empty());
        assert!(lines.keys(0).iter().all(|k| !k.starts_with("retention")));
    }

    #[test]
    fn 코호트_주는_기준_주보다_1주에서_4주_앞이다() {
        let lines = Lines::default();
        let metrics = service(1, 1, Ok(None), &lines)
            .aggregate(date("2030-03-17"))
            .unwrap();

        let starts: Vec<(i32, String)> = metrics
            .weekly_retentions
            .iter()
            .map(|r| (r.weeks_after, r.cohort_week_start.to_string()))
            .collect();
        assert_eq!(
            starts,
            [
                (1, "2030-03-04".to_owned()),
                (2, "2030-02-25".to_owned()),
                (3, "2030-02-18".to_owned()),
                (4, "2030-02-11".to_owned())
            ]
        );
    }
}
