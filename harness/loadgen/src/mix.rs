//! 부하 구성: 엔드포인트별 비율과 요청 만드는 법.
//!
//! 실제 운영 호출 수는 아직 없다(배포 전). 그래서 앱 화면 흐름에서 비율을 가정했다.
//! 앱을 열면 오늘 주제와 내 오늘 게시물 상태를 읽고, 공개 목록을 넘겨 보고, 가끔 상세·캘린더·알림을 본다.
//! 쓰기(좋아요·피드백·제목 수정·업로드 URL)는 전체의 약 14%다. 비율은 `WEIGHTS`에서 한눈에 바꾼다.

use crate::token::kst_date_string;

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// 시드의 크기와 오늘 날짜. 요청 경로를 만드는 데 쓴다.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub users: u64,
    pub days: u64,
    pub posts_per_topic: u64,
    pub today: String,
    pub year: i32,
    pub month: u32,
    pub unix_secs: i64,
}

impl Ctx {
    pub fn new(users: u64, days: u64, posts_per_topic: u64, unix_secs: i64) -> Ctx {
        let (year, month, _) = crate::token::kst_date(unix_secs);
        Ctx {
            users,
            days,
            posts_per_topic,
            today: kst_date_string(unix_secs, 0),
            year,
            month,
            unix_secs,
        }
    }
}

pub struct Built {
    pub method: &'static str,
    pub path: String,
    /// 이 회원으로 인증한다(없으면 익명).
    pub user: Option<u64>,
    pub body: Option<String>,
}

pub struct Spec {
    /// 결과 표의 이름. `메서드 경로 틀`.
    pub name: &'static str,
    pub weight: u32,
    pub build: fn(&mut Rng, &Ctx) -> Built,
}

fn member(rng: &mut Rng, ctx: &Ctx) -> u64 {
    rng.below(ctx.users) + 1
}

/// 임의의 게시물 id(최근 이틀 안쪽에 몰린다: 사람들은 최신 글을 본다).
fn some_post(rng: &mut Rng, ctx: &Ctx, recent_only: bool) -> String {
    let day = if recent_only || rng.below(10) < 7 {
        rng.below(2.min(ctx.days))
    } else {
        rng.below(ctx.days)
    };
    crate::seed::post_uuid(day, rng.below(ctx.posts_per_topic) + 1)
}

fn get(path: String, user: Option<u64>) -> Built {
    Built {
        method: "GET",
        path,
        user,
        body: None,
    }
}

fn random_seed(rng: &mut Rng) -> String {
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..30)
        .map(|_| A[rng.below(A.len() as u64) as usize] as char)
        .collect()
}

fn list_recent(rng: &mut Rng, ctx: &Ctx) -> Built {
    get(
        format!(
            "/api/v1/posts?topicDate={}&page={}",
            ctx.today,
            rng.below(4) + 1
        ),
        None,
    )
}
fn list_popular(rng: &mut Rng, ctx: &Ctx) -> Built {
    get(
        format!(
            "/api/v1/posts?topicDate={}&sort=popular&page={}",
            ctx.today,
            rng.below(3) + 1
        ),
        Some(member(rng, ctx)),
    )
}
fn list_random(rng: &mut Rng, ctx: &Ctx) -> Built {
    let seed = random_seed(rng);
    get(
        format!(
            "/api/v1/posts?topicDate={}&sort=random&randomSeed={seed}&page={}",
            ctx.today,
            rng.below(3) + 1
        ),
        None,
    )
}
fn list_by_date(rng: &mut Rng, ctx: &Ctx) -> Built {
    let day = rng.below(ctx.days.min(14)) as i64;
    get(
        format!(
            "/api/v1/posts?topicDate={}",
            kst_date_string(ctx.unix_secs, day)
        ),
        None,
    )
}
fn detail(rng: &mut Rng, ctx: &Ctx) -> Built {
    let post = some_post(rng, ctx, false);
    get(format!("/api/v1/posts/{post}"), Some(member(rng, ctx)))
}
fn today(rng: &mut Rng, ctx: &Ctx) -> Built {
    get("/api/v1/posts/today".into(), Some(member(rng, ctx)))
}
fn calendar(rng: &mut Rng, ctx: &Ctx) -> Built {
    get(
        format!(
            "/api/v1/posts/calendar?year={}&month={}",
            ctx.year, ctx.month
        ),
        Some(member(rng, ctx)),
    )
}
fn calendar_months(rng: &mut Rng, ctx: &Ctx) -> Built {
    get(
        "/api/v1/posts/calendar/months".into(),
        Some(member(rng, ctx)),
    )
}
fn topic(rng: &mut Rng, ctx: &Ctx) -> Built {
    // 로그인 여부와 무관하게 읽는다: 절반은 익명.
    let user = if rng.below(2) == 0 {
        None
    } else {
        Some(member(rng, ctx))
    };
    get(format!("/api/v1/topics?date={}", ctx.today), user)
}
fn notifications(rng: &mut Rng, ctx: &Ctx) -> Built {
    get(
        "/api/v1/notifications".into(),
        Some(rng.below(100.min(ctx.users)) + 1),
    )
}
fn unread_status(rng: &mut Rng, ctx: &Ctx) -> Built {
    get(
        "/api/v1/notifications/unread-status".into(),
        Some(rng.below(100.min(ctx.users)) + 1),
    )
}
fn like(rng: &mut Rng, ctx: &Ctx) -> Built {
    let post = some_post(rng, ctx, true);
    Built {
        method: "PUT",
        path: format!("/api/v1/posts/{post}/likes"),
        user: Some(member(rng, ctx)),
        body: None,
    }
}
fn unlike(rng: &mut Rng, ctx: &Ctx) -> Built {
    let post = some_post(rng, ctx, true);
    Built {
        method: "DELETE",
        path: format!("/api/v1/posts/{post}/likes"),
        user: Some(member(rng, ctx)),
        body: None,
    }
}
fn edit_title(rng: &mut Rng, ctx: &Ctx) -> Built {
    // 오늘 주제의 내 게시물. 작성자 u의 게시물은 (오늘, u)다.
    let u = rng.below(ctx.posts_per_topic) + 1;
    Built {
        method: "PUT",
        path: format!("/api/v1/posts/{}", crate::seed::post_uuid(0, u)),
        user: Some(u),
        body: Some(format!(r#"{{"title":"글{}"}}"#, rng.below(1000))),
    }
}
fn feedback(rng: &mut Rng, ctx: &Ctx) -> Built {
    Built {
        method: "POST",
        path: "/api/v1/feedbacks".into(),
        user: Some(member(rng, ctx)),
        body: Some(r#"{"content":"부하 시험 피드백입니다."}"#.into()),
    }
}
fn upload_url(rng: &mut Rng, ctx: &Ctx) -> Built {
    Built {
        method: "POST",
        path: "/api/v1/posts/uploads".into(),
        user: Some(member(rng, ctx)),
        body: None,
    }
}
fn read_all(rng: &mut Rng, ctx: &Ctx) -> Built {
    Built {
        method: "PATCH",
        path: "/api/v1/notifications/read-all".into(),
        user: Some(rng.below(100.min(ctx.users)) + 1),
        body: None,
    }
}

pub const WEIGHTS: &[Spec] = &[
    Spec {
        name: "GET /api/v1/posts",
        weight: 14,
        build: list_recent,
    },
    Spec {
        name: "GET /api/v1/posts?sort=popular",
        weight: 5,
        build: list_popular,
    },
    Spec {
        name: "GET /api/v1/posts?sort=random",
        weight: 3,
        build: list_random,
    },
    Spec {
        name: "GET /api/v1/posts?topicDate=past",
        weight: 3,
        build: list_by_date,
    },
    Spec {
        name: "GET /api/v1/posts/{postId}",
        weight: 10,
        build: detail,
    },
    Spec {
        name: "GET /api/v1/posts/today",
        weight: 12,
        build: today,
    },
    Spec {
        name: "GET /api/v1/posts/calendar",
        weight: 6,
        build: calendar,
    },
    Spec {
        name: "GET /api/v1/posts/calendar/months",
        weight: 3,
        build: calendar_months,
    },
    Spec {
        name: "GET /api/v1/topics",
        weight: 15,
        build: topic,
    },
    Spec {
        name: "GET /api/v1/notifications",
        weight: 3,
        build: notifications,
    },
    Spec {
        name: "GET /api/v1/notifications/unread-status",
        weight: 5,
        build: unread_status,
    },
    Spec {
        name: "PUT /api/v1/posts/{postId}/likes",
        weight: 5,
        build: like,
    },
    Spec {
        name: "DELETE /api/v1/posts/{postId}/likes",
        weight: 3,
        build: unlike,
    },
    Spec {
        name: "PUT /api/v1/posts/{postId}",
        weight: 1,
        build: edit_title,
    },
    Spec {
        name: "POST /api/v1/feedbacks",
        weight: 2,
        build: feedback,
    },
    Spec {
        name: "POST /api/v1/posts/uploads",
        weight: 1,
        build: upload_url,
    },
    Spec {
        name: "PATCH /api/v1/notifications/read-all",
        weight: 1,
        build: read_all,
    },
];

/// 가중치에 따라 하나 고른다. `only`가 있으면 그 이름만.
pub fn pick(rng: &mut Rng, only: Option<usize>) -> usize {
    if let Some(i) = only {
        return i;
    }
    let total: u32 = WEIGHTS.iter().map(|s| s.weight).sum();
    let mut roll = rng.below(u64::from(total)) as u32;
    for (i, s) in WEIGHTS.iter().enumerate() {
        if roll < s.weight {
            return i;
        }
        roll -= s.weight;
    }
    WEIGHTS.len() - 1
}

pub fn index_of(name: &str) -> Option<usize> {
    WEIGHTS.iter().position(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_774_000; // 2026-10-12T03:00:00Z

    fn ctx() -> Ctx {
        Ctx::new(300, 60, 150, NOW)
    }

    #[test]
    fn 쓰기_비율은_열넷_안팎이다() {
        let total: u32 = WEIGHTS.iter().map(|s| s.weight).sum();
        let writes: u32 = WEIGHTS
            .iter()
            .filter(|s| !s.name.starts_with("GET "))
            .map(|s| s.weight)
            .sum();
        let pct = f64::from(writes) / f64::from(total) * 100.0;
        assert!((10.0..=18.0).contains(&pct), "{pct}");
    }

    #[test]
    fn 이름이_모두_다르고_메서드로_시작한다() {
        for (i, s) in WEIGHTS.iter().enumerate() {
            assert!(
                ["GET ", "PUT ", "POST ", "DELETE ", "PATCH "]
                    .iter()
                    .any(|m| s.name.starts_with(m)),
                "{}",
                s.name
            );
            assert_eq!(index_of(s.name), Some(i), "이름은 유일하다");
        }
    }

    #[test]
    fn 모든_요청이_시드_범위_안에서_만들어진다() {
        let ctx = ctx();
        let mut rng = Rng::new(7);
        for _ in 0..2000 {
            for spec in WEIGHTS {
                let b = (spec.build)(&mut rng, &ctx);
                assert!(b.path.starts_with("/api/v1/"));
                assert!(
                    spec.name.starts_with(b.method),
                    "{} vs {}",
                    spec.name,
                    b.method
                );
                if let Some(u) = b.user {
                    assert!((1..=300).contains(&u), "{u}");
                }
                if b.path.starts_with("/api/v1/posts?") {
                    // 원본 PostListRequest: topicDate 필수, page는 1부터.
                    assert!(b.path.contains("topicDate="), "{}", b.path);
                    assert!(!b.path.contains("page=0"), "{}", b.path);
                }
            }
        }
    }

    #[test]
    fn 요청_구성은_가중치를_따른다() {
        let mut rng = Rng::new(1);
        let mut counts = vec![0u32; WEIGHTS.len()];
        for _ in 0..100_000 {
            counts[pick(&mut rng, None)] += 1;
        }
        let total: u32 = WEIGHTS.iter().map(|s| s.weight).sum();
        for (i, s) in WEIGHTS.iter().enumerate() {
            let want = f64::from(s.weight) / f64::from(total);
            let got = f64::from(counts[i]) / 100_000.0;
            assert!((want - got).abs() < 0.01, "{}: {want} vs {got}", s.name);
        }
    }

    #[test]
    fn 한_엔드포인트만_고를_수_있다() {
        let mut rng = Rng::new(3);
        let i = index_of("GET /api/v1/topics").unwrap();
        assert!((0..50).all(|_| pick(&mut rng, Some(i)) == i));
    }

    #[test]
    fn 오늘_주제_날짜는_한국_기준이다() {
        let c = ctx();
        assert_eq!(c.today, "2026-10-12");
        assert_eq!((c.year, c.month), (2026, 10));
        let mut rng = Rng::new(5);
        let b = topic(&mut rng, &c);
        assert_eq!(b.path, "/api/v1/topics?date=2026-10-12");
    }
}
