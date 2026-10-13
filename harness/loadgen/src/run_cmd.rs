//! `seed`, `run`, `sql` 명령.

use std::net::{SocketAddr, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

use postgres::{Client as Pg, NoTls};
use serde_json::{Value, json};

use crate::args::Args;
use crate::client::{Client, Request};
use crate::hist::Hist;
use crate::load::{LoadCfg, Running, Stats};
use crate::mix::{self, Ctx, Rng, WEIGHTS};
use crate::seed::{self, SeedSize};
use crate::sql;
use crate::sys::{self, Watcher};
use crate::token::{Scope, Signer, user_uuid};

/// `postgres::Error`의 Display는 "db error"뿐이라 서버가 준 메시지를 꺼낸다.
pub fn db_err(e: postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => format!("{}: {}", d.code().code(), d.message()),
        None => e.to_string(),
    }
}

pub fn connect_db(url: &str) -> Result<Pg, String> {
    Pg::connect(url, NoTls).map_err(|e| format!("DB 접속 실패: {e}"))
}

fn resolve(base: &str) -> Result<SocketAddr, String> {
    let host = base
        .strip_prefix("http://")
        .unwrap_or(base)
        .trim_end_matches('/');
    host.to_socket_addrs()
        .map_err(|e| format!("주소를 풀 수 없습니다 ({host}): {e}"))?
        .next()
        .ok_or_else(|| format!("주소가 없습니다: {host}"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn seed_size(args: &Args) -> Result<SeedSize, String> {
    let d = SeedSize::default();
    Ok(SeedSize {
        users: args.parsed_or("users", d.users)?,
        days: args.parsed_or("days", d.days)?,
        posts_per_topic: args.parsed_or("posts-per-topic", d.posts_per_topic)?,
        likes_per_post: args.parsed_or("likes-per-post", d.likes_per_post)?,
        notifications_per_user: args
            .parsed_or("notifications-per-user", d.notifications_per_user)?,
    })
}

pub fn seed(args: &Args) -> Result<u8, String> {
    let size = seed_size(args)?;
    let mut db = connect_db(args.require("db")?)?;
    let started = Instant::now();
    seed::run(&mut db, &size).map_err(|e| format!("시드 실패: {}", db_err(e)))?;
    let counts = db
        .query_one(
            "SELECT (SELECT count(*) FROM users), (SELECT count(*) FROM topics), (SELECT count(*) FROM posts), \
             (SELECT count(*) FROM post_likes), (SELECT count(*) FROM notifications)",
            &[],
        )
        .map_err(db_err)?;
    println!(
        "시드 완료 ({:.1}초): 회원 {} 주제 {} 게시물 {} 좋아요 {} 알림 {}",
        started.elapsed().as_secs_f64(),
        counts.get::<_, i64>(0),
        counts.get::<_, i64>(1),
        counts.get::<_, i64>(2),
        counts.get::<_, i64>(3),
        counts.get::<_, i64>(4),
    );
    Ok(0)
}

fn pct(hist: &Hist, q: f64) -> u64 {
    hist.quantile(q)
}

pub fn latency_json(hist: &Hist, errors: u64, secs: f64) -> Value {
    json!({
        "count": hist.count(),
        "errors": errors,
        "rps": if secs > 0.0 { hist.count() as f64 / secs } else { 0.0 },
        "mean_us": hist.mean(),
        "p50_us": pct(hist, 0.50),
        "p95_us": pct(hist, 0.95),
        "p99_us": pct(hist, 0.99),
        "max_us": hist.max(),
    })
}

fn stats_json(stats: &Stats, secs: f64) -> Value {
    let mut eps = serde_json::Map::new();
    for (i, spec) in WEIGHTS.iter().enumerate() {
        let ep = &stats.eps[i];
        if ep.hist.count() + ep.errors > 0 {
            eps.insert(
                spec.name.to_owned(),
                latency_json(&ep.hist, ep.errors, secs),
            );
        }
    }
    json!({
        "total": latency_json(&stats.all, stats.errors, secs),
        "endpoints": Value::Object(eps),
        "connects": stats.connects,
        "error_samples": stats.error_samples,
    })
}

fn interval_line(
    elapsed: Duration,
    s: &Stats,
    secs: f64,
    watcher: Option<&Watcher>,
    conns: Option<i64>,
) -> String {
    let (rss, fds, threads) = watcher.map_or((0, 0, 0), |w| {
        let p = w.peek();
        (p.rss_kb_last / 1024, p.fds_last, p.threads_max)
    });
    format!(
        "[{:>6.0}s] rps {:>7.0}  err {:>4}  p50 {:>6.1}ms  p99 {:>7.1}ms  max {:>8.1}ms  rss {rss}MB  fd {fds}  threads {threads}  db연결 {}  load {:.1}",
        elapsed.as_secs_f64(),
        s.all.count() as f64 / secs,
        s.errors,
        pct(&s.all, 0.5) as f64 / 1000.0,
        pct(&s.all, 0.99) as f64 / 1000.0,
        s.all.max() as f64 / 1000.0,
        conns.map_or("-".to_owned(), |c| c.to_string()),
        sys::loadavg()[0],
    )
}

pub fn run(args: &Args) -> Result<u8, String> {
    let size = seed_size(args)?;
    let addr = resolve(args.require("base")?)?;
    let secret = args.require("secret")?.to_owned();
    let only = match args.str("only") {
        Some(n) => Some(mix::index_of(n).ok_or_else(|| format!("알 수 없는 엔드포인트: {n}"))?),
        None => None,
    };
    let rps: Option<f64> = args.parsed("rps")?;
    let cfg = LoadCfg {
        addr,
        conns: args.parsed_or("conns", 32usize)?,
        duration: args.duration_or("duration", Duration::from_secs(30))?,
        warmup: args.duration_or("warmup", Duration::from_secs(10))?,
        rps: rps.filter(|r| *r > 0.0),
        only,
        keep_alive: !args.flag("no-keepalive"),
        idle_conns: args.parsed_or("idle-conns", 0usize)?,
        secret_hex: secret,
        ctx: Ctx::new(size.users, size.days, size.posts_per_topic, now_unix()),
        seed: args.parsed_or("seed", 1u64)?,
    };
    let interval = args.duration_or("interval", Duration::ZERO)?;
    let mut db = match args.str("db") {
        Some(url) => Some(connect_db(url)?),
        None => None,
    };
    let pid: Option<u32> = args.parsed("pid")?;

    let load_before = sys::loadavg();
    let running = Running::start(&cfg);
    if !cfg.warmup.is_zero() {
        println!("워밍업 {}초...", cfg.warmup.as_secs());
        thread::sleep(cfg.warmup);
    }
    if let Some(db) = db.as_mut() {
        sql::reset(db).map_err(db_err)?;
    }
    running.begin_measuring();
    let watcher = pid.map(Watcher::start);
    let started = Instant::now();
    let mut db_conns_max = 0i64;
    let mut db_conns_last = None;
    let mut last_tick = Instant::now();
    while started.elapsed() < cfg.duration {
        let left = cfg.duration.saturating_sub(started.elapsed());
        let step = if interval.is_zero() {
            left
        } else {
            left.min(interval)
        };
        thread::sleep(step.min(Duration::from_secs(1)));
        let due = if interval.is_zero() {
            Duration::from_secs(5)
        } else {
            interval
        };
        if last_tick.elapsed() >= due && started.elapsed() < cfg.duration {
            if let Some(Ok(c)) = db.as_mut().map(sql::connection_count) {
                db_conns_max = db_conns_max.max(c);
                db_conns_last = Some(c);
            }
            if !interval.is_zero() {
                let snap = running.shared.take_interval();
                let secs = last_tick.elapsed().as_secs_f64();
                println!(
                    "{}",
                    interval_line(
                        started.elapsed(),
                        &snap,
                        secs,
                        watcher.as_ref(),
                        db_conns_last
                    )
                );
            }
            last_tick = Instant::now();
        }
    }
    let secs = started.elapsed().as_secs_f64();
    let sys_summary = watcher.map(Watcher::finish);
    let db_after = match db.as_mut() {
        Some(db) => {
            let conns = sql::connection_count(db).ok();
            if let Some(c) = conns {
                db_conns_max = db_conns_max.max(c);
            }
            let stmts = sql::snapshot(db).map_err(db_err)?;
            Some((conns, stmts))
        }
        None => None,
    };
    let stats = running.finish();
    let load_after = sys::loadavg();

    let mut out = stats_json(&stats, secs);
    let o = out.as_object_mut().unwrap();
    o.insert("kind".into(), json!("run"));
    o.insert("label".into(), json!(args.str("label").unwrap_or("")));
    o.insert(
        "mode".into(),
        json!(if cfg.rps.is_some() { "rps" } else { "max" }),
    );
    o.insert("target_rps".into(), json!(cfg.rps));
    o.insert("conns".into(), json!(cfg.conns));
    o.insert("idle_conns".into(), json!(cfg.idle_conns));
    o.insert("keep_alive".into(), json!(cfg.keep_alive));
    o.insert("only".into(), json!(only.map(|i| WEIGHTS[i].name)));
    o.insert("duration_s".into(), json!(secs));
    o.insert("load_before".into(), json!(load_before));
    o.insert("load_after".into(), json!(load_after));
    if let Some(s) = sys_summary {
        o.insert(
            "system".into(),
            json!({
                "cpu_secs": s.cpu_secs,
                "cpu_cores": if secs > 0.0 { s.cpu_secs / secs } else { 0.0 },
                "rss_mb_max": s.rss_kb_max as f64 / 1024.0,
                "rss_mb_last": s.rss_kb_last as f64 / 1024.0,
                "fds_max": s.fds_max,
                "fds_last": s.fds_last,
                "threads_max": s.threads_max,
            }),
        );
    }
    if let Some((conns, stmts)) = db_after {
        let t = sql::totals(&stmts);
        let requests = (stats.all.count() + stats.errors).max(1) as f64;
        o.insert(
            "db".into(),
            json!({
                "connections_after": conns,
                "connections_max": db_conns_max,
                "data_statements_per_request": t.data_calls as f64 / requests,
                "control_statements_per_request": t.control_calls as f64 / requests,
                "db_ms_per_request": t.db_ms / requests,
            }),
        );
    }
    let text = serde_json::to_string_pretty(&out).unwrap();
    if let Some(path) = args.str("out") {
        std::fs::write(path, &text).map_err(|e| format!("{path}: {e}"))?;
    }
    print_summary(&out);
    Ok(if stats.errors > 0 { 1 } else { 0 })
}

fn print_summary(out: &Value) {
    let t = &out["total"];
    println!(
        "{} [{}] 연결 {}: {:.0} req/s  오류 {}  p50 {:.1}ms  p95 {:.1}ms  p99 {:.1}ms  max {:.1}ms  (부하 {:.1} -> {:.1})",
        out["label"].as_str().unwrap_or(""),
        out["mode"].as_str().unwrap_or(""),
        out["conns"],
        t["rps"].as_f64().unwrap_or(0.0),
        t["errors"],
        t["p50_us"].as_f64().unwrap_or(0.0) / 1000.0,
        t["p95_us"].as_f64().unwrap_or(0.0) / 1000.0,
        t["p99_us"].as_f64().unwrap_or(0.0) / 1000.0,
        t["max_us"].as_f64().unwrap_or(0.0) / 1000.0,
        out["load_before"][0].as_f64().unwrap_or(0.0),
        out["load_after"][0].as_f64().unwrap_or(0.0),
    );
    if let Some(s) = out.get("system") {
        println!(
            "  CPU {:.2}코어  RSS {:.0}MB(최대)  FD {}(최대)  스레드 {}",
            s["cpu_cores"].as_f64().unwrap_or(0.0),
            s["rss_mb_max"].as_f64().unwrap_or(0.0),
            s["fds_max"],
            s["threads_max"],
        );
    }
    if let Some(samples) = out["error_samples"].as_array() {
        for s in samples.iter().take(3) {
            println!("  오류 예: {}", s.as_str().unwrap_or(""));
        }
    }
}

/// 엔드포인트마다 K번 순서대로 불러 요청당 SQL 수와 DB 시간을 잰다.
pub fn sql(args: &Args) -> Result<u8, String> {
    let size = seed_size(args)?;
    let addr = resolve(args.require("base")?)?;
    let signer = Signer::from_hex(args.require("secret")?)?;
    let mut db = connect_db(args.require("db")?)?;
    let k: usize = args.parsed_or("requests", 40usize)?;
    let warm: usize = args.parsed_or("warmup-requests", 10usize)?;
    let ctx = Ctx::new(size.users, size.days, size.posts_per_topic, now_unix());
    let only = args.str("only");
    let mut rng = Rng::new(args.parsed_or("seed", 99u64)?);
    let mut client = Client::new(addr, true);
    let mut endpoints = serde_json::Map::new();

    for spec in WEIGHTS.iter().filter(|s| only.is_none_or(|o| o == s.name)) {
        let mut send = |rng: &mut Rng| -> Result<u16, String> {
            let b = (spec.build)(rng, &ctx);
            let token = b
                .user
                .map(|u| signer.token(&user_uuid(u), Scope::User, now_unix() - 5, 900, "sql"));
            client
                .send(&Request {
                    method: b.method,
                    path: &b.path,
                    bearer: token.as_deref(),
                    body: b.body.as_deref(),
                })
                .map(|r| r.status)
                .map_err(|e| e.to_string())
        };
        for _ in 0..warm {
            send(&mut rng)?;
        }
        sql::reset(&mut db).map_err(db_err)?;
        let mut not_ok = 0;
        for _ in 0..k {
            if !(200..300).contains(&send(&mut rng)?) {
                not_ok += 1;
            }
        }
        let stmts = sql::snapshot(&mut db).map_err(db_err)?;
        let t = sql::totals(&stmts);
        let per = |n: i64| n as f64 / k as f64;
        println!(
            "{:<45} 데이터 {:>5.2}  제어 {:>5.2}  DB {:>6.3}ms  비정상 응답 {not_ok}/{k}",
            spec.name,
            per(t.data_calls),
            per(t.control_calls),
            t.db_ms / k as f64,
        );
        let statements: Vec<Value> = stmts
            .iter()
            .map(|s| {
                json!({
                    "query": s.query,
                    "calls_per_request": per(s.calls),
                    "ms_per_request": s.total_ms / k as f64,
                    "kind": match sql::classify(&s.query) { sql::Kind::Data => "data", sql::Kind::Control => "control" },
                })
            })
            .collect();
        endpoints.insert(
            spec.name.to_owned(),
            json!({
                "requests": k,
                "not_ok": not_ok,
                "data_per_request": per(t.data_calls),
                "control_per_request": per(t.control_calls),
                "db_ms_per_request": t.db_ms / k as f64,
                "statements": statements,
            }),
        );
    }
    let out = json!({
        "kind": "sql",
        "label": args.str("label").unwrap_or(""),
        "endpoints": Value::Object(endpoints),
    });
    if let Some(path) = args.str("out") {
        std::fs::write(path, serde_json::to_string_pretty(&out).unwrap())
            .map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(0)
}
