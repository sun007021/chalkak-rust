//! 집계 쿼리의 DB 통합 시험(원본 `UserMetricsAggregationServiceTest`). 테스트마다 새 DB라 회원·좋아요 스냅샷도 그대로 센다.
//!
//! 로그 지표는 항상 비어 있는 리더로 끄고 DB 지표만 본다. 로그 지표는 `service`·`cloudwatch` 시험이 다룬다.

use std::sync::Arc;

use jiff::civil::Date;
use postgres::Client;
use uuid::Uuid;

use super::repository::PgUserMetricsRepository;
use super::service::{
    AccessLogMetrics, AccessLogMetricsReader, UserMetricsAggregationService, WeeklyRetention,
};
use super::testing::{Lines, logger};
use crate::testing::TestDb;
use crate::web::ApiError;

fn date(s: &str) -> Date {
    s.parse().unwrap()
}

const METRIC_DATE: &str = "2030-03-15";
const SUNDAY_METRIC_DATE: &str = "2030-03-17";

struct NoLogs;

impl AccessLogMetricsReader for NoLogs {
    fn read(&self, _: Date) -> Result<Option<AccessLogMetrics>, ApiError> {
        Ok(None)
    }
}

struct Env {
    db: TestDb,
    client: Client,
    lines: Lines,
    /// 게시물마다 새 주제를 만든다. 주제의 참여 기간은 서로 겹치지 못하므로 날짜를 하루씩 밀어 쓴다.
    sequence: i64,
}

fn env() -> Option<Env> {
    let db = TestDb::create()?;
    let client = db.client();
    Some(Env {
        db,
        client,
        lines: Lines::default(),
        sequence: 0,
    })
}

impl Env {
    fn service(&self, excluded: &[Uuid]) -> UserMetricsAggregationService {
        UserMetricsAggregationService::new(
            Arc::new(PgUserMetricsRepository::new(
                self.db.pool(2),
                excluded.to_vec(),
            )),
            Arc::new(NoLogs),
            logger(&self.lines),
        )
    }

    fn user(&mut self, slug: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.client
            .execute(
                "INSERT INTO users (id, email, status, signature_original_storage_key) \
                 VALUES ($1, $2, 'ACTIVE', $3)",
                &[
                    &id,
                    &format!("{slug}-{id}@example.com"),
                    &format!("chalkak/dev/signatures/{id}.png"),
                ],
            )
            .unwrap();
        id
    }

    fn topic(&mut self) -> Uuid {
        let day = date("2041-01-01")
            .checked_add(jiff::Span::new().days(self.sequence))
            .unwrap();
        self.sequence += 1;
        let next = day.tomorrow().unwrap();
        self.client
            .query_one(
                "INSERT INTO topics (title, topic_date, starts_at, ends_at) VALUES \
                 ('지표 집계 주제', $1::text::date, ($1::text::date)::timestamp AT TIME ZONE 'Asia/Seoul', \
                 ($2::text::date)::timestamp AT TIME ZONE 'Asia/Seoul') RETURNING id",
                &[&day.to_string(), &next.to_string()],
            )
            .unwrap()
            .get(0)
    }

    /// `created_at`은 KST 시각 문자열(`2030-03-15T10:00:00+09:00`).
    fn post(&mut self, author: Uuid, created_at: &str, status: &str, deleted: bool) -> Uuid {
        let topic = self.topic();
        let photo: Uuid = self
            .client
            .query_one(
                "INSERT INTO photos (original_storage_key) VALUES ($1) RETURNING id",
                &[&format!(
                    "chalkak/dev/posts/original/{}.webp",
                    Uuid::new_v4()
                )],
            )
            .unwrap()
            .get(0);
        self.client
            .query_one(
                "INSERT INTO posts (user_id, topic_id, photo_id, moderation_status, created_at, updated_at, deleted_at) \
                 VALUES ($1, $2, $3, $4::text::moderation_status, $5::text::timestamptz, $5::text::timestamptz, \
                 CASE WHEN $6 THEN CURRENT_TIMESTAMP END) RETURNING id",
                &[&author, &topic, &photo, &status, &created_at, &deleted],
            )
            .unwrap()
            .get(0)
    }

    fn like(&mut self, post: Uuid, user: Uuid) {
        self.client
            .execute(
                "INSERT INTO post_likes (post_id, user_id) VALUES ($1, $2)",
                &[&post, &user],
            )
            .unwrap();
    }
}

fn kst(day: &str, time: &str) -> String {
    format!("{day}T{time}+09:00")
}

fn shift(day: &str, days: i64) -> String {
    date(day)
        .checked_add(jiff::Span::new().days(days))
        .unwrap()
        .to_string()
}

// aggregate_dayBoundary_countsOnlyPostsInsideKstDay
#[test]
fn kst_하루의_시작은_포함하고_직전과_다음_날_0시는_제외한다() {
    let Some(mut e) = env() else { return };
    let (a, b, c, d) = (
        e.user("start"),
        e.user("end"),
        e.user("before"),
        e.user("after"),
    );
    e.post(a, &kst(METRIC_DATE, "00:00:00"), "APPROVED", false);
    e.post(b, &kst(METRIC_DATE, "23:59:59"), "APPROVED", false);
    e.post(
        c,
        &kst(&shift(METRIC_DATE, -1), "23:59:59"),
        "APPROVED",
        false,
    );
    e.post(
        d,
        &kst(&shift(METRIC_DATE, 1), "00:00:00"),
        "APPROVED",
        false,
    );

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(metrics.dau, 2);
    assert_eq!(metrics.posts, 2);
}

// aggregate_sameUserTwoPosts_countsDauOnce
#[test]
fn 같은_회원이_같은_날_올린_게시물은_dau에_한_번만_세고_posts에는_모두_센다() {
    let Some(mut e) = env() else { return };
    let user = e.user("twice");
    e.post(user, &kst(METRIC_DATE, "09:00:00"), "APPROVED", false);
    e.post(user, &kst(METRIC_DATE, "21:00:00"), "REJECTED", false);

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(metrics.dau, 1);
    assert_eq!(metrics.posts, 2);
}

// aggregate_deletedAndRejectedPosts_areCounted
#[test]
fn 삭제됐거나_거절된_게시물도_업로드로_보고_dau와_posts에_센다() {
    let Some(mut e) = env() else { return };
    let (deleted, rejected) = (e.user("deleted-post"), e.user("rejected-post"));
    e.post(deleted, &kst(METRIC_DATE, "10:00:00"), "APPROVED", true);
    e.post(rejected, &kst(METRIC_DATE, "11:00:00"), "REJECTED", false);

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(metrics.dau, 2);
    assert_eq!(metrics.posts, 2);
}

// aggregate_weekWindow_coversSixDaysBeforeThroughMetricDate
#[test]
fn wau는_기준일_6일_전_0시부터_기준일_끝까지_7일을_센다() {
    let Some(mut e) = env() else { return };
    let users: Vec<Uuid> = ["before", "first", "middle", "last", "after"]
        .iter()
        .map(|s| e.user(s))
        .collect();
    e.post(
        users[0],
        &kst(&shift(METRIC_DATE, -7), "23:59:59"),
        "APPROVED",
        false,
    );
    e.post(
        users[1],
        &kst(&shift(METRIC_DATE, -6), "00:00:00"),
        "APPROVED",
        false,
    );
    e.post(
        users[2],
        &kst(&shift(METRIC_DATE, -3), "12:00:00"),
        "APPROVED",
        false,
    );
    e.post(users[3], &kst(METRIC_DATE, "23:59:59"), "APPROVED", false);
    e.post(
        users[4],
        &kst(&shift(METRIC_DATE, 1), "00:00:00"),
        "APPROVED",
        false,
    );

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(metrics.wau, 3);
    assert_eq!(metrics.dau, 1);
}

// aggregate_excludedUser_isRemovedFromEveryMetric
#[test]
fn 제외_회원은_dau_posts_wau_eligible_users_likes_total에서_모두_빠진다() {
    let Some(mut e) = env() else { return };
    let (member, excluded) = (e.user("member"), e.user("team"));
    let member_post = e.post(member, &kst(METRIC_DATE, "10:00:00"), "APPROVED", false);
    e.post(excluded, &kst(METRIC_DATE, "10:00:00"), "APPROVED", false);
    e.like(member_post, member);
    e.like(member_post, excluded);

    let all = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();
    let filtered = e.service(&[excluded]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(all.dau - filtered.dau, 1);
    assert_eq!(all.posts - filtered.posts, 1);
    assert_eq!(all.wau - filtered.wau, 1);
    assert_eq!(all.eligible_users - filtered.eligible_users, 1);
    assert_eq!(all.likes_total - filtered.likes_total, 1);
}

// aggregate_bannedAndWithdrawnUsers_areNotEligible
#[test]
fn eligible_users는_정지된_회원과_탈퇴한_회원을_센_결과에서_뺀다() {
    let Some(mut e) = env() else { return };
    let before = e
        .service(&[])
        .aggregate(date(METRIC_DATE))
        .unwrap()
        .eligible_users;
    e.user("active");
    let banned = e.user("banned");
    let withdrawn = e.user("withdrawn");
    e.client
        .execute(
            "UPDATE users SET status = 'BANNED' WHERE id = $1",
            &[&banned],
        )
        .unwrap();
    e.client
        .execute(
            "UPDATE users SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1",
            &[&withdrawn],
        )
        .unwrap();

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(metrics.eligible_users - before, 1);
}

// aggregate_likesOnDeletedPost_areNotCounted
#[test]
fn likes_total은_삭제된_게시물에_달린_좋아요를_세지_않는다() {
    let Some(mut e) = env() else { return };
    let before = e
        .service(&[])
        .aggregate(date(METRIC_DATE))
        .unwrap()
        .likes_total;
    let (author, liker) = (e.user("like-author"), e.user("like-liker"));
    let live = e.post(author, &kst(METRIC_DATE, "10:00:00"), "APPROVED", false);
    let deleted = e.post(author, &kst(METRIC_DATE, "11:00:00"), "APPROVED", true);
    e.like(live, liker);
    e.like(deleted, liker);

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(metrics.likes_total - before, 1);
}

// aggregate_success_logsOneAggregationLine
#[test]
fn 집계_결과를_aggregation_로그_한_줄에_type과_metric_date와_지표_값으로_남긴다() {
    let Some(mut e) = env() else { return };
    let user = e.user("log-line");
    let post = e.post(user, &kst(METRIC_DATE, "10:00:00"), "APPROVED", false);
    e.like(post, user);

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    let events = e.lines.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["message"], "사용자 지표 집계");
    assert_eq!(event["type"], "aggregation");
    assert_eq!(event["metricDate"], "2030-03-15");
    assert_eq!(event["dau"], 1);
    assert_eq!(event["posts"], 1);
    assert_eq!(event["wau"], 1);
    assert_eq!(event["eligibleUsers"], metrics.eligible_users);
    assert_eq!(event["likesTotal"], metrics.likes_total);
}

// aggregate_sameDateTwice_returnsSameDbDerivedValues
#[test]
fn 같은_날짜를_두_번_집계하면_게시물_기반_지표가_같다() {
    let Some(mut e) = env() else { return };
    let user = e.user("idempotent");
    e.post(user, &kst(METRIC_DATE, "10:00:00"), "APPROVED", false);
    let service = e.service(&[]);

    let first = service.aggregate(date(METRIC_DATE)).unwrap();
    let second = service.aggregate(date(METRIC_DATE)).unwrap();

    assert_eq!(second, first);
}

// aggregate_sunday_countsWeeklyRetentionPerCohort
#[test]
fn 일요일_기준일은_1_4주_전_코호트별_인원과_유지_회원_수를_주_경계에_맞춰_센다() {
    // 기준 주 2030-03-11(월)~03-17(일), 코호트 W+1=03-04, W+2=02-25, W+3=02-18, W+4=02-11
    let Some(mut e) = env() else { return };
    let names = [
        "returned",
        "cohort-only",
        "monday-edge",
        "too-late",
        "deleted",
        "far",
        "target-only",
        "previous-sunday",
        "excluded",
    ];
    let u: Vec<Uuid> = names.iter().map(|n| e.user(n)).collect();
    let (
        returned,
        cohort_only,
        monday_edge,
        too_late,
        deleted,
        far,
        target_only,
        prev_sunday,
        excluded,
    ) = (u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8]);
    let mut post = |user, day: &str, time: &str, status: &str, del: bool| {
        e.post(user, &kst(day, time), status, del);
    };
    post(returned, "2030-03-05", "10:00:00", "APPROVED", false);
    post(returned, "2030-03-01", "10:00:00", "APPROVED", false);
    post(returned, "2030-03-12", "10:00:00", "APPROVED", false);
    post(cohort_only, "2030-03-06", "10:00:00", "APPROVED", false);
    post(monday_edge, "2030-03-04", "00:00:00", "APPROVED", false);
    post(monday_edge, "2030-03-11", "00:00:00", "APPROVED", false);
    post(too_late, "2030-03-10", "23:59:59", "APPROVED", false);
    post(too_late, "2030-03-18", "00:00:00", "APPROVED", false);
    post(deleted, "2030-02-20", "10:00:00", "APPROVED", true);
    post(deleted, "2030-03-14", "10:00:00", "REJECTED", false);
    post(far, "2030-02-12", "10:00:00", "APPROVED", false);
    post(far, "2030-03-13", "10:00:00", "APPROVED", false);
    post(target_only, "2030-03-11", "00:00:00", "APPROVED", false);
    post(prev_sunday, "2030-03-03", "23:59:59", "APPROVED", false);
    post(excluded, "2030-03-05", "10:00:00", "APPROVED", false);
    post(excluded, "2030-03-12", "10:00:00", "APPROVED", false);

    let metrics = e
        .service(&[excluded])
        .aggregate(date(SUNDAY_METRIC_DATE))
        .unwrap();

    let want = |weeks_after, start: &str, size, retained| WeeklyRetention {
        weeks_after,
        cohort_week_start: date(start),
        cohort_size: size,
        retained_users: retained,
    };
    assert_eq!(
        metrics.weekly_retentions,
        [
            want(1, "2030-03-04", 4, 2),
            want(2, "2030-02-25", 2, 1),
            want(3, "2030-02-18", 1, 1),
            want(4, "2030-02-11", 1, 1),
        ]
    );
}

// aggregate_sundayWithoutExclusion_countsEveryUser
#[test]
fn 제외_회원이_없으면_제외_회원도_코호트와_유지_회원_수에_센다() {
    let Some(mut e) = env() else { return };
    let member = e.user("retention-member");
    e.post(member, &kst("2030-03-05", "10:00:00"), "APPROVED", false);
    e.post(member, &kst("2030-03-12", "10:00:00"), "APPROVED", false);

    let metrics = e.service(&[]).aggregate(date(SUNDAY_METRIC_DATE)).unwrap();

    assert_eq!(
        metrics.weekly_retentions[0],
        WeeklyRetention {
            weeks_after: 1,
            cohort_week_start: date("2030-03-04"),
            cohort_size: 1,
            retained_users: 1,
        }
    );
}

// aggregate_nonSunday_hasNoRetention
#[test]
fn 일요일이_아닌_기준일은_유지율을_계산하지_않고_로그에도_남기지_않는다() {
    let Some(e) = env() else { return };

    let metrics = e.service(&[]).aggregate(date(METRIC_DATE)).unwrap();

    assert!(metrics.weekly_retentions.is_empty());
    assert!(e.lines.keys(0).iter().all(|k| !k.starts_with("retention")));
}
