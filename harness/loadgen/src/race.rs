//! 동시성 시험(`race`). 실행 중인 서버(Java 또는 Rust)에 같은 자원을 향한 요청을 동시에 보내고, 응답 코드와 DB 불변식을
//! 확인한다. 같은 시험을 두 구현에 돌려 불변식이 모두 지켜지는지, 결과가 같은지 본다(전환 계획 4.4 동시성).
//!
//! 시험마다 전용 게시물·회원을 쓰고 시작 때 DB 상태를 직접 맞춘다(시드 데이터 위에서 돈다). 5xx는 항상 실패다.

use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use postgres::Client as Pg;
use serde_json::{Value, json};

use crate::args::Args;
use crate::client::{Client, Request};
use crate::run_cmd::{connect_db, db_err};
use crate::sql;
use crate::token::{Scope, Signer, user_uuid};

mod isolation;

#[derive(Clone)]
struct Env {
    addr: SocketAddr,
    signer: Arc<Signer>,
    db_url: String,
    admin_id: String,
    /// 가짜 외부 서버(S3 HeadObject 응답 지정용). 없으면 가입 시험을 건너뛴다.
    fakes: Option<SocketAddr>,
    /// 회원가입 토큰 서명 키(`SOCIAL_SIGNUP_TOKEN_SECRET`의 바이트). 없으면 가입 시험을 건너뛴다.
    signup_key: Option<Arc<Vec<u8>>>,
}

#[derive(Clone)]
struct Call {
    method: &'static str,
    path: String,
    /// `Some((id, scope))`면 그 주체로 인증한다.
    who: Option<(String, Scope)>,
    body: Option<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn send(env: &Env, client: &mut Client, call: &Call) -> u16 {
    let token = call
        .who
        .as_ref()
        .map(|(id, scope)| env.signer.token(id, *scope, now() - 5, 900, "race"));
    client
        .send(&Request {
            method: call.method,
            path: &call.path,
            bearer: token.as_deref(),
            body: call.body.as_deref(),
        })
        .map(|r| r.status)
        .unwrap_or(0)
}

/// 호출들을 같은 순간에 시작한다(호출마다 스레드·연결 하나). 상태 코드를 호출 순서대로 돌려준다.
fn fire(env: &Env, calls: Vec<Call>) -> Vec<u16> {
    let barrier = Arc::new(Barrier::new(calls.len()));
    let handles: Vec<_> = calls
        .into_iter()
        .map(|call| {
            let barrier = Arc::clone(&barrier);
            let env = env.clone();
            thread::spawn(move || {
                let mut client = Client::new(env.addr, true);
                // 연결을 미리 맺어 두고(요청은 보내지 않는다: 헬스체크도 풀 연결을 쓴다) 동시에 보낸다.
                let _ = client.warm();
                barrier.wait();
                send(&env, &mut client, &call)
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
}

fn tally(statuses: &[u16]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for s in statuses {
        *m.entry(s.to_string()).or_insert(0) += 1;
    }
    m
}

/// 조건이 거짓이면 실패 메시지를 남긴다. 메시지가 `Case`의 다른 필드(상태 코드)를 읽을 수 있게 `failures`만 빌린다.
fn expect(failures: &mut Vec<String>, cond: bool, msg: impl FnOnce() -> String) {
    if !cond {
        failures.push(msg());
    }
}

struct Case {
    name: &'static str,
    failures: Vec<String>,
    statuses: BTreeMap<String, usize>,
    detail: Value,
    /// 끝난 뒤 DB 상태 중 요청 순서와 무관하게 정해지는 값. Java와 Rust의 보고서 대조(`report`)가 이 값을 비교한다.
    state: Value,
    /// 상태 코드 분포가 요청 순서와 무관하게 정해지는 시험이면 참. 보고서가 두 구현의 분포를 비교한다.
    compare_statuses: bool,
    /// 두 구현의 결과가 달라도 알려진 차이면 그 설명(보고서가 다름으로 표시하되 게이트에서 뺀다).
    known_diff: Option<&'static str>,
    millis: u128,
}

impl Case {
    fn new(name: &'static str) -> Case {
        Case {
            name,
            failures: Vec::new(),
            statuses: BTreeMap::new(),
            detail: Value::Null,
            state: Value::Null,
            compare_statuses: false,
            known_diff: None,
            millis: 0,
        }
    }

    /// 상태 코드를 합치기만 한다(5xx를 실패로 세지 않는다). 실패가 정답인 시험(트리거로 일부러 터뜨리는 원자성 시험)이 쓴다.
    fn record(&mut self, statuses: &[u16]) {
        for (k, v) in tally(statuses) {
            *self.statuses.entry(k).or_insert(0) += v;
        }
    }

    /// 상태 코드를 합치고 5xx·연결 실패(0)를 실패로 센다.
    fn absorb(&mut self, statuses: &[u16]) {
        for (k, v) in tally(statuses) {
            *self.statuses.entry(k).or_insert(0) += v;
        }
        let bad = statuses.iter().filter(|s| **s == 0 || **s >= 500).count();
        expect(&mut self.failures, bad == 0, || {
            format!("서버 오류/연결 실패 {bad}건")
        });
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "passed": self.failures.is_empty(),
            "failures": self.failures,
            "statuses": self.statuses,
            "detail": self.detail,
            "state": self.state,
            "compare_statuses": self.compare_statuses,
            "known_diff": self.known_diff,
            "millis": self.millis,
        })
    }
}

fn scalar_i64(db: &mut Pg, query: &str) -> i64 {
    db.query_one(query, &[]).map(|r| r.get(0)).unwrap_or(-1)
}

fn post(day: u64, user: u64) -> String {
    crate::seed::post_uuid(day, user)
}

fn like_call(method: &'static str, post_id: &str, user: u64) -> Call {
    Call {
        method,
        path: format!("/api/v1/posts/{post_id}/likes"),
        who: Some((user_uuid(user), Scope::User)),
        body: None,
    }
}

fn admin_moderate(env: &Env, post_id: &str, body: &str) -> Call {
    Call {
        method: "PUT",
        path: format!("/api/v1/admin/posts/{post_id}/moderation"),
        who: Some((env.admin_id.clone(), Scope::Admin)),
        body: Some(body.to_owned()),
    }
}

fn like_same_user(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("like-same-user");
    let target = post(9, 40);
    let _ = db.batch_execute(&format!(
        "DELETE FROM post_likes WHERE post_id = '{target}'"
    ));
    let st = fire(env, (0..32).map(|_| like_call("PUT", &target, 7)).collect());
    c.absorb(&st);
    expect(&mut c.failures, st.iter().all(|s| *s == 200), || {
        format!("모두 200이어야 한다: {:?}", c.statuses)
    });
    let rows = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, rows == 1, || {
        format!("(게시물, 회원) 좋아요는 한 행: {rows}")
    });
    c.detail = json!({ "rows": rows });
    c
}

fn like_many_users(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("like-many-users");
    let target = post(9, 41);
    let _ = db.batch_execute(&format!(
        "DELETE FROM post_likes WHERE post_id = '{target}'"
    ));
    let st = fire(
        env,
        (1..=64).map(|u| like_call("PUT", &target, u)).collect(),
    );
    c.absorb(&st);
    expect(&mut c.failures, st.iter().all(|s| *s == 200), || {
        format!("모두 200이어야 한다: {:?}", c.statuses)
    });
    let rows = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, rows == 64, || {
        format!("서로 다른 회원 64명의 좋아요는 64행: {rows}")
    });
    c.detail = json!({ "rows": rows });
    c
}

fn like_unlike_race(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("like-unlike-same-user");
    let target = post(9, 42);
    let _ = db.batch_execute(&format!(
        "DELETE FROM post_likes WHERE post_id = '{target}'"
    ));
    let calls = (0..40)
        .map(|i| like_call(if i % 2 == 0 { "PUT" } else { "DELETE" }, &target, 9))
        .collect();
    let st = fire(env, calls);
    c.absorb(&st);
    expect(
        &mut c.failures,
        st.iter().all(|s| (200..300).contains(s)),
        || format!("모두 2xx여야 한다: {:?}", c.statuses),
    );
    let rows = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, rows == 0 || rows == 1, || {
        format!("최종 상태는 0 또는 1행: {rows}")
    });
    c.detail = json!({ "rows": rows });
    c
}

fn update_same_post(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("update-same-post");
    let target = post(0, 50);
    let calls = (0..16)
        .map(|i| Call {
            method: "PUT",
            path: format!("/api/v1/posts/{target}"),
            who: Some((user_uuid(50), Scope::User)),
            body: Some(format!(r#"{{"title":"동시{i}"}}"#)),
        })
        .collect();
    let st = fire(env, calls);
    c.absorb(&st);
    expect(&mut c.failures, st.iter().all(|s| *s == 200), || {
        format!("모두 200이어야 한다: {:?}", c.statuses)
    });
    let title: String = db
        .query_one(
            &format!("SELECT title FROM posts WHERE id = '{target}'"),
            &[],
        )
        .map(|r| r.get(0))
        .unwrap_or_default();
    expect(&mut c.failures, title.starts_with("동시"), || {
        format!("최종 제목은 제출한 값 중 하나: {title:?}")
    });
    c.detail = json!({ "title": title });
    c
}

fn update_vs_delete(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("update-vs-delete");
    let target = post(0, 51);
    let _ = db.batch_execute(&format!(
        "UPDATE posts SET deleted_at = NULL WHERE id = '{target}'"
    ));
    let mut calls: Vec<Call> = (0..8)
        .map(|i| Call {
            method: "PUT",
            path: format!("/api/v1/posts/{target}"),
            who: Some((user_uuid(51), Scope::User)),
            body: Some(format!(r#"{{"title":"수정{i}"}}"#)),
        })
        .collect();
    calls.push(Call {
        method: "DELETE",
        path: format!("/api/v1/posts/{target}"),
        who: Some((user_uuid(51), Scope::User)),
        body: None,
    });
    let st = fire(env, calls);
    c.absorb(&st);
    let delete_status = *st.last().unwrap_or(&0);
    expect(
        &mut c.failures,
        delete_status == 204 || delete_status == 200,
        || format!("삭제 응답: {delete_status}"),
    );
    let deleted = scalar_i64(
        db,
        &format!("SELECT count(*) FROM posts WHERE id = '{target}' AND deleted_at IS NOT NULL"),
    );
    expect(&mut c.failures, deleted == 1, || {
        "삭제한 게시물은 deleted_at이 채워져야 한다".to_owned()
    });
    c.detail = json!({ "delete_status": delete_status, "deleted": deleted });
    // 다음 시험을 위해 되돌린다.
    let _ = db.batch_execute(&format!(
        "UPDATE posts SET deleted_at = NULL WHERE id = '{target}'"
    ));
    c
}

/// 검수 대기 게시물 하나를 만든다(감사 로그·알림 정리 포함).
fn make_pending(db: &mut Pg, target: &str) {
    let _ = db.batch_execute(&format!(
        "DELETE FROM notifications WHERE post_id = '{target}';\
         DELETE FROM admin_audit_logs WHERE target_id = '{target}';\
         UPDATE posts SET moderation_status = 'PENDING', moderated_at = NULL, deleted_at = NULL WHERE id = '{target}'"
    ));
}

fn moderate_same(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("moderate-approve-x8");
    let target = post(8, 60);
    make_pending(db, &target);
    let calls = (0..8)
        .map(|_| admin_moderate(env, &target, r#"{"status":"APPROVED"}"#))
        .collect();
    let st = fire(env, calls);
    c.absorb(&st);
    let ok = st.iter().filter(|s| (200..300).contains(*s)).count();
    expect(&mut c.failures, ok == 1, || {
        format!("PENDING에서 한 번만 처리된다(2xx 1건): {:?}", c.statuses)
    });
    let audit = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{target}'"),
    );
    let notes = scalar_i64(
        db,
        &format!("SELECT count(*) FROM notifications WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, audit == 1, || {
        format!("감사 로그는 한 건: {audit}")
    });
    expect(&mut c.failures, notes == 1, || {
        format!("알림은 한 건: {notes}")
    });
    c.detail = json!({ "audit": audit, "notifications": notes });
    c
}

fn moderate_approve_vs_reject(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("moderate-approve-vs-reject");
    let target = post(8, 61);
    make_pending(db, &target);
    let mut calls: Vec<Call> = (0..4)
        .map(|_| admin_moderate(env, &target, r#"{"status":"APPROVED"}"#))
        .collect();
    calls.extend((0..4).map(|_| {
        admin_moderate(
            env,
            &target,
            r#"{"status":"REJECTED","rejectionReason":"부적절"}"#,
        )
    }));
    let st = fire(env, calls);
    c.absorb(&st);
    let ok = st.iter().filter(|s| (200..300).contains(*s)).count();
    expect(&mut c.failures, ok == 1, || {
        format!("승인과 반려 중 하나만 이긴다(2xx 1건): {:?}", c.statuses)
    });
    let status: String = db
        .query_one(
            &format!("SELECT moderation_status::text FROM posts WHERE id = '{target}'"),
            &[],
        )
        .map(|r| r.get(0))
        .unwrap_or_default();
    let audit_action: Vec<String> = db
        .query(
            &format!("SELECT action::text FROM admin_audit_logs WHERE target_id = '{target}'"),
            &[],
        )
        .map(|rows| rows.iter().map(|r| r.get(0)).collect())
        .unwrap_or_default();
    let note_type: Vec<String> = db
        .query(
            &format!("SELECT type FROM notifications WHERE post_id = '{target}'"),
            &[],
        )
        .map(|rows| rows.iter().map(|r| r.get(0)).collect())
        .unwrap_or_default();
    let want = if status == "APPROVED" {
        "POST_APPROVED"
    } else {
        "POST_REJECTED"
    };
    expect(&mut c.failures, audit_action == [want], || {
        format!("게시물 상태 {status}와 감사 로그가 같아야 한다: {audit_action:?}")
    });
    expect(&mut c.failures, note_type == [want], || {
        format!("알림도 최종 상태와 같아야 한다: {note_type:?}")
    });
    c.detail = json!({ "final": status, "audit": audit_action, "notifications": note_type });
    c
}

fn moderate_vs_update(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("moderate-vs-owner-update");
    let target = post(8, 62);
    make_pending(db, &target);
    let mut calls = vec![admin_moderate(env, &target, r#"{"status":"APPROVED"}"#)];
    calls.extend((0..6).map(|i| Call {
        method: "PUT",
        path: format!("/api/v1/posts/{target}"),
        who: Some((user_uuid(62), Scope::User)),
        body: Some(format!(r#"{{"title":"변경{i}"}}"#)),
    }));
    let st = fire(env, calls);
    c.absorb(&st);
    expect(&mut c.failures, (200..300).contains(&st[0]), || {
        format!("검수는 성공해야 한다: {}", st[0])
    });
    let status: String = db
        .query_one(
            &format!("SELECT moderation_status::text FROM posts WHERE id = '{target}'"),
            &[],
        )
        .map(|r| r.get(0))
        .unwrap_or_default();
    expect(&mut c.failures, status == "APPROVED", || {
        format!("최종 상태는 승인: {status}")
    });
    let audit = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{target}'"),
    );
    expect(&mut c.failures, audit == 1, || {
        format!("감사 로그는 한 건: {audit}")
    });
    c.detail = json!({ "final": status, "audit": audit });
    c
}

fn feedback_burst(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("feedback-burst");
    let before = scalar_i64(db, "SELECT count(*) FROM feedbacks");
    let calls = (0..100)
        .map(|i| Call {
            method: "POST",
            path: "/api/v1/feedbacks".into(),
            who: Some((user_uuid(1 + i % 20), Scope::User)),
            body: Some(r#"{"content":"동시성 시험"}"#.into()),
        })
        .collect();
    let st = fire(env, calls);
    c.absorb(&st);
    expect(&mut c.failures, st.iter().all(|s| *s == 201), || {
        format!("모두 201이어야 한다: {:?}", c.statuses)
    });
    let after = scalar_i64(db, "SELECT count(*) FROM feedbacks");
    expect(&mut c.failures, after - before == 100, || {
        format!("행이 100개 늘어야 한다: {}", after - before)
    });
    c.detail = json!({ "inserted": after - before });
    c
}

/// 풀을 다 쓰게 만들고(테이블 잠금 아래에서 INSERT가 멈춘다) 풀이 회복하는지 본다.
/// `hold`가 연결 대기 한도(30초)보다 짧으면 모두 성공해야 하고, 길면 대기하던 요청이 오류로 끝나야 한다.
fn pool_exhaustion(env: &Env, hold: Duration, name: &'static str) -> Case {
    let mut c = Case::new(name);
    let mut locker = match connect_db(&env.db_url) {
        Ok(db) => db,
        Err(e) => {
            c.failures.push(e);
            return c;
        }
    };
    let mut observer = connect_db(&env.db_url).expect("관찰용 연결");
    if let Err(e) = locker.batch_execute("BEGIN; LOCK TABLE feedbacks IN ACCESS EXCLUSIVE MODE") {
        c.failures.push(db_err(e));
        return c;
    }
    // 풀(10)보다 많은 쓰기 30개가 잠금에서 멈추고, 그동안 읽기 요청도 연결을 기다린다.
    let writers: Vec<Call> = (0..30)
        .map(|i| Call {
            method: "POST",
            path: "/api/v1/feedbacks".into(),
            who: Some((user_uuid(1 + i), Scope::User)),
            body: Some(r#"{"content":"풀 고갈 시험"}"#.into()),
        })
        .collect();
    let readers: Vec<Call> = (0..10)
        .map(|_| Call {
            method: "GET",
            path: "/api/v1/posts/today".into(),
            who: Some((user_uuid(3), Scope::User)),
            body: None,
        })
        .collect();
    let started = Instant::now();
    let (e1, e2) = (clone_env(env), clone_env(env));
    let h_w = thread::spawn(move || fire(&e1, writers));
    thread::sleep(Duration::from_millis(500));
    let h_r = thread::spawn(move || fire(&e2, readers));
    thread::sleep(hold);
    let held_connections = sql::connection_count(&mut observer).unwrap_or(-1);
    let _ = locker.batch_execute("COMMIT");
    let write_status = h_w.join().unwrap_or_default();
    let read_status = h_r.join().unwrap_or_default();
    let elapsed = started.elapsed();

    c.statuses = tally(&write_status);
    for (k, v) in tally(&read_status) {
        *c.statuses.entry(format!("read-{k}")).or_insert(0) += v;
    }
    let timed_out = hold > Duration::from_secs(30);
    if timed_out {
        // 대기 한도(30초)를 넘기면 연결을 못 얻은 요청은 5xx(Hikari 타임아웃)로 끝난다. 멈추거나 연결이 끊기면 안 된다.
        let reads_failed = read_status.iter().filter(|s| **s >= 500).count();
        expect(&mut c.failures, reads_failed == read_status.len(), || {
            format!("풀이 비어도 대기 한도 뒤엔 오류 응답이어야 한다: {read_status:?}")
        });
        expect(&mut c.failures, read_status.iter().all(|s| *s != 0), || {
            "연결이 끊기지 않고 응답해야 한다".to_owned()
        });
    } else {
        expect(
            &mut c.failures,
            write_status.iter().all(|s| *s == 201),
            || {
                format!(
                    "쓰기는 모두 201: {c_status:?}",
                    c_status = tally(&write_status)
                )
            },
        );
        expect(
            &mut c.failures,
            read_status.iter().all(|s| *s == 200),
            || format!("읽기는 모두 200: {:?}", tally(&read_status)),
        );
    }
    // 잠금을 풀면 회복해야 한다: 연결이 늘어나지 않고(누수 없음) 새 요청이 바로 처리된다.
    thread::sleep(Duration::from_millis(500));
    let after_connections = sql::connection_count(&mut observer).unwrap_or(-1);
    expect(&mut c.failures, after_connections <= 12, || {
        format!("DB 연결은 풀 크기(10) 안쪽: {after_connections}")
    });
    let mut probe = Client::new(env.addr, true);
    let t0 = Instant::now();
    let probe_status = send(
        env,
        &mut probe,
        &Call {
            method: "GET",
            path: "/api/v1/posts/today".into(),
            who: Some((user_uuid(3), Scope::User)),
            body: None,
        },
    );
    let probe_ms = t0.elapsed().as_millis();
    expect(&mut c.failures, probe_status == 200, || {
        format!("회복 뒤 요청은 200: {probe_status}")
    });
    expect(&mut c.failures, probe_ms < 2000, || {
        format!("회복 뒤 요청은 바로 처리: {probe_ms}ms")
    });
    let mut health = Client::new(env.addr, true);
    let health_status = send(
        env,
        &mut health,
        &Call {
            method: "GET",
            path: "/actuator/health".into(),
            who: None,
            body: None,
        },
    );
    expect(&mut c.failures, health_status == 200, || {
        format!("헬스체크는 회복 뒤 UP: {health_status}")
    });
    c.detail = json!({
        "hold_secs": hold.as_secs_f64(),
        "elapsed_secs": elapsed.as_secs_f64(),
        "db_connections_while_held": held_connections,
        "db_connections_after": after_connections,
        "probe_ms": probe_ms,
        "write_statuses": tally(&write_status),
        "read_statuses": tally(&read_status),
    });
    c
}

fn clone_env(env: &Env) -> Env {
    env.clone()
}

pub fn command(args: &Args) -> Result<u8, String> {
    let host = args
        .require("base")?
        .trim_start_matches("http://")
        .to_owned();
    let addr = host
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or("주소가 없습니다")?;
    let signer = Arc::new(Signer::from_hex(args.require("secret")?)?);
    let db_url = args.require("db")?.to_owned();
    let mut db = connect_db(&db_url)?;
    let admin_id: String = db
        .query_one(
            "SELECT id::text FROM admins ORDER BY created_at LIMIT 1",
            &[],
        )
        .map_err(|e| {
            format!(
                "관리자 계정이 없습니다(앱이 한 번 떠서 만들어야 한다): {}",
                db_err(e)
            )
        })?
        .get(0);
    let fakes = match args.str("fakes") {
        Some(f) => Some(
            f.trim_start_matches("http://")
                .to_socket_addrs()
                .map_err(|e| format!("{f}: {e}"))?
                .next()
                .ok_or("가짜 서버 주소가 없습니다")?,
        ),
        None => None,
    };
    let signup_key = match args.str("signup-secret") {
        Some(hex) => Some(Arc::new(crate::token::hex_decode(hex)?)),
        None => None,
    };
    let env = Env {
        addr,
        signer,
        db_url,
        admin_id,
        fakes,
        signup_key,
    };

    let only = args.str("only");
    let slow = !args.flag("skip-slow");
    type Scenario = fn(&Env, &mut Pg) -> Case;
    let scenarios: Vec<(&str, Scenario)> = vec![
        ("like-same-user", like_same_user),
        ("like-many-users", like_many_users),
        ("like-unlike-same-user", like_unlike_race),
        ("update-same-post", update_same_post),
        ("update-vs-delete", update_vs_delete),
        ("moderate-approve-x8", moderate_same),
        ("moderate-approve-vs-reject", moderate_approve_vs_reject),
        ("moderate-vs-owner-update", moderate_vs_update),
        ("feedback-burst", feedback_burst),
    ];
    let scenarios: Vec<(&str, Scenario)> = scenarios
        .into_iter()
        .chain(isolation::scenarios())
        .collect();
    // 앞선 실행이 중간에 죽어 남긴 격리 시험 트리거를 먼저 치운다(다음 시험이 영향받지 않게).
    if let Err(e) = isolation::purge_leftovers(&mut db) {
        eprintln!("격리 시험 잔여물 정리 실패: {e}");
    }
    let mut cases = Vec::new();
    for (name, f) in scenarios {
        if only.is_some_and(|o| o != name) {
            continue;
        }
        let t0 = Instant::now();
        let mut case = f(&env, &mut db);
        case.millis = t0.elapsed().as_millis();
        cases.push(case);
    }
    for (name, hold) in [
        ("pool-exhaustion-recovers", Duration::from_secs(5)),
        ("pool-exhaustion-timeout", Duration::from_secs(35)),
    ] {
        if only.is_some_and(|o| o != name) || (name.ends_with("timeout") && !slow) {
            continue;
        }
        let t0 = Instant::now();
        let mut case = pool_exhaustion(&env, hold, name);
        case.millis = t0.elapsed().as_millis();
        cases.push(case);
    }

    let mut failed = 0;
    for c in &cases {
        let mark = if c.failures.is_empty() {
            "통과"
        } else {
            "실패"
        };
        println!("{mark} {:<30} {:?}  {}", c.name, c.statuses, c.detail);
        for f in &c.failures {
            println!("     - {f}");
        }
        failed += usize::from(!c.failures.is_empty());
    }
    let out = json!({
        "kind": "race",
        "label": args.str("label").unwrap_or(""),
        "passed": failed == 0,
        "cases": cases.iter().map(Case::to_json).collect::<Vec<_>>(),
    });
    if let Some(path) = args.str("out") {
        std::fs::write(path, serde_json::to_string_pretty(&out).unwrap())
            .map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 상태_코드를_센다() {
        let t = tally(&[200, 200, 400, 0]);
        assert_eq!(t["200"], 2);
        assert_eq!(t["400"], 1);
        assert_eq!(t["0"], 1);
    }

    #[test]
    fn 오류와_연결_실패는_실패로_센다() {
        let mut c = Case::new("x");
        c.absorb(&[200, 201]);
        assert!(c.failures.is_empty());
        c.absorb(&[200, 500]);
        assert_eq!(c.failures.len(), 1);
        c.absorb(&[0]);
        assert_eq!(c.failures.len(), 2);
        assert_eq!(c.to_json()["passed"], false);
    }

    #[test]
    fn expect는_조건이_거짓일_때만_기록한다() {
        let mut c = Case::new("x");
        expect(&mut c.failures, true, || "안 쓰임".into());
        expect(&mut c.failures, false, || "기록".into());
        assert_eq!(c.failures, ["기록"]);
    }
}
