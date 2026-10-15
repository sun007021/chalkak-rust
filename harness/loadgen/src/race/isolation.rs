//! 트랜잭션 격리·원자성 시험(`race`의 일부). 같은 시험을 Java와 Rust 서버에 돌려 상태 코드 분포와 DB 최종 상태가
//! 같은지, 불변식이 지켜지는지 본다.
//!
//! - 원자성: 트랜잭션 뒤쪽 문장이 쓰는 테이블에 `RAISE EXCEPTION` 트리거를 걸어 요청을 일부러 실패시키고, 앞쪽 문장의
//!   변경이 남지 않았는지 본다(부분 커밋 없음). 트리거를 지운 뒤 같은 요청이 성공하는지도 본다.
//! - 가시성: 쓰기 트랜잭션 안에 `pg_sleep` 트리거를 넣어 커밋을 늦추고, 그동안 다른 연결의 GET이 예전 값을, 커밋 응답 뒤의
//!   GET이 새 값을 보는지 본다(READ COMMITTED).
//! - 순서: 잠금을 잡는 요청이 몰릴 때 고아 행·잃어버린 갱신이 없는지, 리프레시 토큰 동시 재사용, 가입 토큰 동시 사용,
//!   계보 잠금 순서(교착) 시험.
//!
//! 시험마다 만든 트리거·함수·행은 끝나기 전에 지운다. 시작할 때는 앞선 실행이 남긴 트리거를 먼저 치우고(`purge_leftovers`),
//! 마지막 시험(`isolation-clean-slate`)이 아무것도 남지 않았는지 확인한다. 트리거는 시험 대상 행에만 걸린다(`WHEN` 조건).

use std::thread;
use std::time::{Duration, Instant};

use postgres::Client as Pg;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Call, Case, Env, admin_moderate, expect, fire, make_pending, now, post, scalar_i64};
use crate::client::{Client, Request};
use crate::token::{Scope, hs256_jwt, user_uuid};

type Scenario = fn(&Env, &mut Pg) -> Case;

pub fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        (
            "atomic-moderate-notification-fail",
            atomic_moderate_notification,
        ),
        ("atomic-moderate-audit-fail", atomic_moderate_audit),
        ("atomic-admin-delete-audit-fail", atomic_admin_delete),
        ("atomic-author-delete-likes-fail", atomic_author_delete),
        ("atomic-withdraw-user-update-fail", atomic_withdraw),
        ("atomic-refresh-rotate-fail", atomic_refresh_user),
        ("atomic-admin-refresh-rotate-fail", atomic_refresh_admin),
        ("atomic-admin-ban-audit-fail", atomic_ban),
        ("visibility-title-update", visibility_title),
        ("visibility-like-count", visibility_like),
        ("like-vs-author-delete-no-orphan", like_vs_author_delete),
        ("admin-ban-x8", admin_ban_x8),
        ("admin-ban-unban-mix", admin_ban_unban_mix),
        ("refresh-reuse-x20", refresh_reuse_user),
        ("admin-refresh-reuse-x20", refresh_reuse_admin),
        ("signup-same-token-x10", signup_same_token),
        ("signup-same-identity-x4", signup_same_identity),
        ("withdraw-lock-order-x12", withdraw_lock_order),
        ("withdraw-duplicate-x12", withdraw_duplicate),
        ("withdraw-vs-refresh", withdraw_vs_refresh),
        ("isolation-clean-slate", clean_slate),
    ]
}

// ---------------------------------------------------------------------------------------------
// 순수 판정 (단위 시험 대상)
// ---------------------------------------------------------------------------------------------

/// 기대 값과 실제 값이 다른 항목을 메시지로 만든다. 항목은 `(설명, 기대, 실제)`.
fn mismatches(items: &[(&str, i64, i64)]) -> Vec<String> {
    items
        .iter()
        .filter(|(_, want, got)| want != got)
        .map(|(what, want, got)| format!("{what}: 기대 {want}, 실제 {got}"))
        .collect()
}

/// 리프레시 토큰 행의 상태(회전됨, 폐기됨).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenRow {
    rotated: bool,
    revoked: bool,
}

/// 같은 리프레시 토큰으로 `n`개가 동시에 들어온 결과. 계보 잠금이 요청을 한 줄로 세우므로 하나만 회전에 성공하고, 나머지는
/// 이미 회전된 토큰의 재사용으로 보고 계보를 폐기한 뒤 401을 받는다. 그래서 DB에는 처음 토큰(회전됨)과 후속 토큰 둘이 남고
/// 둘 다 폐기돼 있다.
fn judge_refresh_race(n: usize, statuses: &[u16], rows: &[TokenRow]) -> Vec<String> {
    let mut out = Vec::new();
    let ok = statuses.iter().filter(|s| **s == 200).count();
    let unauthorized = statuses.iter().filter(|s| **s == 401).count();
    if ok != 1 {
        out.push(format!("성공(200)은 정확히 하나: {ok}"));
    }
    if unauthorized != n.saturating_sub(1) {
        out.push(format!(
            "나머지는 모두 401: {unauthorized}/{}",
            n.saturating_sub(1)
        ));
    }
    if rows.len() != 2 {
        out.push(format!("계보의 행은 둘(처음, 후속): {}", rows.len()));
    }
    let rotated = rows.iter().filter(|r| r.rotated).count();
    if rotated != 1 {
        out.push(format!("회전된 행은 하나: {rotated}"));
    }
    if n > 1 && rows.iter().any(|r| !r.revoked) {
        out.push("재사용이 감지되면 계보 전체가 폐기돼야 한다".to_owned());
    }
    out
}

/// 커밋 전에는 예전 값, 커밋 응답 뒤에는 새 값이 보여야 한다. `write_ms`는 쓰기 요청의 응답 시간으로, 트리거의 지연보다
/// 짧으면 지연이 트랜잭션 안에서 일어나지 않은 것이라 시험이 의미가 없다.
fn judge_visibility(
    old: &str,
    new: &str,
    during: &str,
    during_db: &str,
    after: &str,
    write_ms: u128,
    min_write_ms: u128,
) -> Vec<String> {
    let mut out = Vec::new();
    if write_ms < min_write_ms {
        out.push(format!(
            "쓰기 응답이 트리거 지연({min_write_ms}ms)보다 빨랐다: {write_ms}ms"
        ));
    }
    if during != old {
        out.push(format!(
            "커밋 전 GET은 예전 값 {old:?}이어야 한다: {during:?}"
        ));
    }
    if during_db != old {
        out.push(format!(
            "커밋 전 다른 연결의 SELECT는 예전 값 {old:?}이어야 한다: {during_db:?}"
        ));
    }
    if after != new {
        out.push(format!("커밋 뒤 GET은 새 값 {new:?}이어야 한다: {after:?}"));
    }
    out
}

/// 차단·해제 요청이 섞여 들어와도 한 번에 하나씩 성공한다. 시작이 ACTIVE이므로 성공 횟수가 홀수면 BANNED로 끝나고, 감사 로그는
/// 성공마다 하나다.
fn judge_ban_toggle(ok: usize, final_banned: bool, audits: i64) -> Vec<String> {
    let mut out = Vec::new();
    if final_banned != (ok % 2 == 1) {
        out.push(format!(
            "성공 {ok}번 뒤의 상태가 맞지 않다(차단 여부 {final_banned})"
        ));
    }
    out.extend(mismatches(&[(
        "감사 로그는 성공마다 하나",
        ok as i64,
        audits,
    )]));
    out
}

/// 가입 동시 요청의 DB 최종 상태. 회원·소셜 계정은 하나이고, 리프레시 토큰은 200 응답마다 하나다(가입이 끝난 뒤 들어온
/// 요청도 기존 회원으로 토큰을 새로 받는다).
fn judge_signup(ok: usize, users: i64, socials: i64, tokens: i64) -> Vec<String> {
    mismatches(&[
        ("회원은 하나", 1, users),
        ("소셜 계정은 하나", 1, socials),
        ("리프레시 토큰은 200 응답마다 하나", ok as i64, tokens),
    ])
}

// ---------------------------------------------------------------------------------------------
// 트리거
// ---------------------------------------------------------------------------------------------

/// 시험 전용 트리거. 이름은 `race_iso_`로 시작해야 한다(잔여물 정리가 그 접두사로 찾는다).
struct Trigger {
    name: &'static str,
    table: &'static str,
    /// `BEFORE INSERT`, `AFTER UPDATE` 같은 시점과 사건.
    event: &'static str,
    /// `WHEN (...)` 안의 조건. 시험 대상 행에만 걸리게 한다.
    when: String,
    /// plpgsql 문장(세미콜론 없이).
    body: String,
}

const RAISE_BODY: &str = "RAISE EXCEPTION 'race_iso: 시험용으로 일부러 실패시킨다'";

fn sleep_body(secs: f64) -> String {
    format!("PERFORM pg_sleep({secs})")
}

impl Trigger {
    fn function_name(&self) -> String {
        format!("race_iso_{}_fn", self.name)
    }

    fn trigger_name(&self) -> String {
        format!("race_iso_{}", self.name)
    }

    fn install_sql(&self) -> String {
        format!(
            "CREATE OR REPLACE FUNCTION {f}() RETURNS trigger LANGUAGE plpgsql AS $race$ \
             BEGIN {body}; RETURN NEW; END $race$; \
             CREATE TRIGGER {t} {event} ON {table} FOR EACH ROW WHEN ({when}) EXECUTE FUNCTION {f}()",
            f = self.function_name(),
            t = self.trigger_name(),
            body = self.body,
            event = self.event,
            table = self.table,
            when = self.when,
        )
    }

    fn remove_sql(&self) -> String {
        format!(
            "DROP TRIGGER IF EXISTS {} ON {}; DROP FUNCTION IF EXISTS {}()",
            self.trigger_name(),
            self.table,
            self.function_name()
        )
    }

    fn install(&self, db: &mut Pg) -> Result<(), String> {
        db.batch_execute(&self.install_sql())
            .map_err(|e| format!("트리거 {} 설치 실패: {e}", self.name))
    }

    fn remove(&self, db: &mut Pg) {
        let _ = db.batch_execute(&self.remove_sql());
    }
}

/// 앞선 실행이 죽어 남긴 `race_iso_` 트리거와 함수를 지운다. 지운 개수를 돌려준다.
pub fn purge_leftovers(db: &mut Pg) -> Result<usize, String> {
    let mut dropped = 0;
    let triggers = db
        .query(
            "SELECT 'DROP TRIGGER IF EXISTS ' || quote_ident(t.tgname) || ' ON ' || c.oid::regclass \
             FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             WHERE NOT t.tgisinternal AND t.tgname LIKE 'race\\_iso\\_%'",
            &[],
        )
        .map_err(|e| e.to_string())?;
    for row in triggers {
        let sql: String = row.get(0);
        db.batch_execute(&sql).map_err(|e| e.to_string())?;
        dropped += 1;
    }
    let functions = db
        .query(
            "SELECT 'DROP FUNCTION IF EXISTS ' || p.oid::regprocedure FROM pg_proc p \
             WHERE p.proname LIKE 'race\\_iso\\_%'",
            &[],
        )
        .map_err(|e| e.to_string())?;
    for row in functions {
        let sql: String = row.get(0);
        db.batch_execute(&sql).map_err(|e| e.to_string())?;
        dropped += 1;
    }
    Ok(dropped)
}

// ---------------------------------------------------------------------------------------------
// 공통 도구
// ---------------------------------------------------------------------------------------------

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 문장을 실행하고 실패하면 시험 실패로 적는다.
fn run(c: &mut Case, db: &mut Pg, sql: &str) {
    if let Err(e) = db.batch_execute(sql) {
        c.failures.push(format!("준비·정리 SQL 실패: {e}"));
    }
}

fn text(db: &mut Pg, sql: &str) -> String {
    db.query_one(sql, &[])
        .ok()
        .and_then(|r| r.try_get::<_, Option<String>>(0).ok().flatten())
        .unwrap_or_default()
}

/// 요청 하나를 보내고 상태 코드와 JSON 본문(앞부분)을 돌려준다.
fn send_full(env: &Env, call: &Call) -> (u16, Value) {
    let token = call
        .who
        .as_ref()
        .map(|(id, scope)| env.signer.token(id, *scope, now() - 5, 900, "race"));
    let mut client = Client::new(env.addr, true);
    match client.send(&Request {
        method: call.method,
        path: &call.path,
        bearer: token.as_deref(),
        body: call.body.as_deref(),
    }) {
        Ok(r) => (
            r.status,
            serde_json::from_slice(&r.body_head).unwrap_or(Value::Null),
        ),
        Err(_) => (0, Value::Null),
    }
}

fn status_of(env: &Env, call: &Call) -> u16 {
    send_full(env, call).0
}

/// 트리거를 걸고 요청 하나를 보낸 뒤 바로 트리거를 지운다. 응답 코드를 돌려주고, 기대(500)와 다르면 실패로 적는다.
/// 5xx가 정답인 시험이라 `absorb` 대신 `record`를 쓴다.
fn fail_on_purpose(env: &Env, db: &mut Pg, c: &mut Case, trigger: &Trigger, call: &Call) -> u16 {
    if let Err(e) = trigger.install(db) {
        c.failures.push(e);
        return 0;
    }
    let status = status_of(env, call);
    trigger.remove(db);
    c.record(&[status]);
    expect(&mut c.failures, status == 500, || {
        format!("트리거로 실패시킨 요청은 500이어야 한다: {status}")
    });
    status
}

fn user_call(method: &'static str, path: String, user: &str, body: Option<&str>) -> Call {
    Call {
        method,
        path,
        who: Some((user.to_owned(), Scope::User)),
        body: body.map(str::to_owned),
    }
}

fn admin_call(env: &Env, method: &'static str, path: String, body: Option<&str>) -> Call {
    Call {
        method,
        path,
        who: Some((env.admin_id.clone(), Scope::Admin)),
        body: body.map(str::to_owned),
    }
}

fn anonymous_post(path: &str, body: String) -> Call {
    Call {
        method: "POST",
        path: path.to_owned(),
        who: None,
        body: Some(body),
    }
}

// ---- 전용 회원·토큰 ----

fn iso_user_id(n: u64) -> String {
    format!("01990000-0000-7000-8009-{n:012x}")
}

/// 전용 회원과 그에 딸린 행을 지운다(없어도 된다).
fn purge_user(db: &mut Pg, id: &str) -> Result<(), String> {
    db.batch_execute(&format!(
        "DELETE FROM user_refresh_tokens WHERE user_id = '{id}';\
         DELETE FROM apple_authorizations WHERE social_account_id IN (SELECT id FROM social_accounts WHERE user_id = '{id}');\
         DELETE FROM social_accounts WHERE user_id = '{id}';\
         DELETE FROM post_likes WHERE user_id = '{id}';\
         DELETE FROM notifications WHERE user_id = '{id}';\
         DELETE FROM admin_audit_logs WHERE target_id = '{id}';\
         DELETE FROM users WHERE id = '{id}'"
    ))
    .map_err(|e| format!("전용 회원 정리 실패: {e}"))
}

/// 소셜 계정이 연결된 전용 회원과 계보 `sessions`개(계보마다 리프레시 토큰 하나)를 만든다. 리프레시 토큰 평문을 계보 순서로
/// 돌려준다(DB에는 해시만 들어간다).
fn make_user(db: &mut Pg, n: u64, sessions: usize) -> Result<Vec<String>, String> {
    make_user_with(db, n, sessions, true)
}

/// `with_social`이 거짓이면 소셜 계정 없이 만든다(탈퇴가 소셜 계정을 지우는 문장을 건너뛴다).
fn make_user_with(
    db: &mut Pg,
    n: u64,
    sessions: usize,
    with_social: bool,
) -> Result<Vec<String>, String> {
    let id = iso_user_id(n);
    purge_user(db, &id)?;
    let mut sql = format!(
        "INSERT INTO users (id, email, status, signature_original_storage_key, signature_thumbnail_storage_key) \
         VALUES ('{id}', 'iso{n}@test.chalkak', 'ACTIVE', 'chalkak/signatures/iso{n}/original.png', 'chalkak/signatures/iso{n}/thumbnail.png');"
    );
    if with_social {
        sql.push_str(&format!(
            "INSERT INTO social_accounts (user_id, provider, subject_hmac) VALUES ('{id}', 'GOOGLE', '{}');",
            sha256_hex(&format!("iso-subject-{n}"))
        ));
    }
    let mut tokens = Vec::new();
    for s in 0..sessions {
        let plain = format!("iso-refresh-{n}-{s}-{}", nanos());
        sql.push_str(&format!(
            "INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, absolute_expires_at) \
             VALUES ('{id}', '{}', '{}', now() + interval '30 days', now() + interval '90 days');",
            iso_session_id(n, s),
            sha256_hex(&plain)
        ));
        tokens.push(plain);
    }
    db.batch_execute(&sql)
        .map_err(|e| format!("전용 회원 준비 실패: {e}"))?;
    Ok(tokens)
}

fn iso_session_id(n: u64, s: usize) -> String {
    format!("01990000-0000-7000-800a-{:012x}", n * 100 + s as u64)
}

fn live_tokens(db: &mut Pg, user_id: &str) -> i64 {
    scalar_i64(
        db,
        &format!(
            "SELECT count(*) FROM user_refresh_tokens WHERE user_id = '{user_id}' AND revoked_at IS NULL"
        ),
    )
}

/// 관리자 계보 하나(세션 id는 `n`으로 정한다)와 리프레시 토큰 평문.
fn make_admin_token(db: &mut Pg, env: &Env, n: u64) -> Result<String, String> {
    let session = iso_session_id(n, 99);
    let plain = format!("iso-admin-refresh-{n}-{}", nanos());
    db.batch_execute(&format!(
        "DELETE FROM admin_refresh_tokens WHERE session_id = '{session}';\
         INSERT INTO admin_refresh_tokens (admin_id, session_id, token_hash, expires_at, absolute_expires_at) \
         VALUES ('{}', '{session}', '{}', now() + interval '30 days', now() + interval '90 days')",
        env.admin_id,
        sha256_hex(&plain)
    ))
    .map_err(|e| format!("관리자 토큰 준비 실패: {e}"))?;
    Ok(plain)
}

fn token_rows(db: &mut Pg, table: &str, session: &str) -> Vec<TokenRow> {
    db.query(
        &format!(
            "SELECT rotated_at IS NOT NULL, revoked_at IS NOT NULL FROM {table} WHERE session_id = '{session}'"
        ),
        &[],
    )
    .map(|rows| {
        rows.iter()
            .map(|r| TokenRow {
                rotated: r.get(0),
                revoked: r.get(1),
            })
            .collect()
    })
    .unwrap_or_default()
}

fn refresh_call(admin: bool, token: &str) -> Call {
    anonymous_post(
        if admin {
            "/api/v1/admin/auth/refresh"
        } else {
            "/api/v1/auth/refresh"
        },
        json!({ "refreshToken": token }).to_string(),
    )
}

// ---------------------------------------------------------------------------------------------
// 원자성: 뒤쪽 문장이 실패하면 앞쪽 변경이 남지 않는다
// ---------------------------------------------------------------------------------------------

/// 관리자 검수: 게시물 UPDATE -> 감사 로그 INSERT -> 알림 INSERT. `fail_table`의 INSERT를 실패시킨다.
fn atomic_moderate_on(
    env: &Env,
    db: &mut Pg,
    name: &'static str,
    short: &'static str,
    fail_table: &'static str,
    column: &'static str,
    target_user: u64,
) -> Case {
    let mut c = Case::new(name);
    c.compare_statuses = true;
    let target = post(8, target_user);
    make_pending(db, &target);
    let trigger = Trigger {
        name: short,
        table: fail_table,
        event: "BEFORE INSERT",
        when: format!("NEW.{column} = '{target}'"),
        body: RAISE_BODY.to_owned(),
    };
    let call = admin_moderate(env, &target, r#"{"status":"APPROVED"}"#);
    fail_on_purpose(env, db, &mut c, &trigger, &call);

    let pending = scalar_i64(
        db,
        &format!(
            "SELECT count(*) FROM posts WHERE id = '{target}' AND moderation_status = 'PENDING' AND moderated_at IS NULL"
        ),
    );
    let audit = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{target}'"),
    );
    let notes = scalar_i64(
        db,
        &format!("SELECT count(*) FROM notifications WHERE post_id = '{target}'"),
    );
    c.failures.extend(mismatches(&[
        ("실패 뒤에도 게시물은 PENDING·미검수", 1, pending),
        ("감사 로그는 롤백", 0, audit),
        ("알림은 롤백", 0, notes),
    ]));

    // 트리거를 지우면 같은 요청이 한 번에 끝나야 한다(롤백이 잠금이나 상태를 남기지 않았다).
    let retry = status_of(env, &call);
    c.record(&[retry]);
    let audit_after = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{target}'"),
    );
    let notes_after = scalar_i64(
        db,
        &format!("SELECT count(*) FROM notifications WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, retry == 200, || {
        format!("트리거를 지운 뒤 재시도는 200: {retry}")
    });
    c.failures.extend(mismatches(&[
        ("재시도 뒤 감사 로그 한 건", 1, audit_after),
        ("재시도 뒤 알림 한 건", 1, notes_after),
    ]));
    c.state = json!({
        "pending_after_failure": pending, "audit_after_failure": audit, "notifications_after_failure": notes,
        "retry_status": retry, "audit_after_retry": audit_after, "notifications_after_retry": notes_after,
    });
    c.detail = c.state.clone();
    // 다른 시험이 쓰는 시드 상태(승인된 게시물, 알림·감사 로그 없음)로 되돌린다.
    run(
        &mut c,
        db,
        &format!(
            "DELETE FROM notifications WHERE post_id = '{target}';\
             DELETE FROM admin_audit_logs WHERE target_id = '{target}';\
             UPDATE posts SET moderation_status = 'APPROVED', moderated_at = now() WHERE id = '{target}'"
        ),
    );
    c
}

fn atomic_moderate_notification(env: &Env, db: &mut Pg) -> Case {
    atomic_moderate_on(
        env,
        db,
        "atomic-moderate-notification-fail",
        "mod_notif",
        "notifications",
        "post_id",
        70,
    )
}

fn atomic_moderate_audit(env: &Env, db: &mut Pg) -> Case {
    atomic_moderate_on(
        env,
        db,
        "atomic-moderate-audit-fail",
        "mod_audit",
        "admin_audit_logs",
        "target_id",
        71,
    )
}

/// 관리자 게시물 삭제: 게시물 UPDATE -> 사진 UPDATE -> 좋아요 DELETE -> 감사 로그 INSERT. 감사 로그를 실패시킨다.
fn atomic_admin_delete(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("atomic-admin-delete-audit-fail");
    c.compare_statuses = true;
    let target = post(8, 75);
    run(
        &mut c,
        db,
        &format!(
            "UPDATE posts SET deleted_at = NULL, moderation_status = 'APPROVED' WHERE id = '{target}';\
             UPDATE photos SET deleted_at = NULL WHERE id = (SELECT photo_id FROM posts WHERE id = '{target}');\
             DELETE FROM admin_audit_logs WHERE target_id = '{target}'"
        ),
    );
    let likes_before = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, likes_before > 0, || {
        "시드에 좋아요가 있어야 롤백을 확인할 수 있다".to_owned()
    });
    let trigger = Trigger {
        name: "del_audit",
        table: "admin_audit_logs",
        event: "BEFORE INSERT",
        when: format!("NEW.target_id = '{target}'"),
        body: RAISE_BODY.to_owned(),
    };
    let call = admin_call(
        env,
        "DELETE",
        format!("/api/v1/admin/posts/{target}"),
        Some(r#"{"reason":"격리 시험"}"#),
    );
    fail_on_purpose(env, db, &mut c, &trigger, &call);
    let post_alive = scalar_i64(
        db,
        &format!("SELECT count(*) FROM posts WHERE id = '{target}' AND deleted_at IS NULL"),
    );
    let photo_alive = scalar_i64(
        db,
        &format!(
            "SELECT count(*) FROM photos WHERE id = (SELECT photo_id FROM posts WHERE id = '{target}') AND deleted_at IS NULL"
        ),
    );
    let likes_after = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    c.failures.extend(mismatches(&[
        ("게시물 삭제 표시는 롤백", 1, post_alive),
        ("사진 삭제 표시는 롤백", 1, photo_alive),
        ("좋아요 삭제는 롤백", likes_before, likes_after),
    ]));
    c.state = json!({ "post_alive": post_alive, "photo_alive": photo_alive, "likes_kept": likes_after == likes_before });
    c.detail = json!({ "likes_before": likes_before, "likes_after": likes_after });
    c
}

/// 작성자 게시물 삭제: 게시물 UPDATE -> 좋아요 DELETE. 좋아요 DELETE를 실패시킨다.
fn atomic_author_delete(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("atomic-author-delete-likes-fail");
    c.compare_statuses = true;
    let target = post(8, 76);
    run(
        &mut c,
        db,
        &format!(
            "UPDATE posts SET deleted_at = NULL WHERE id = '{target}';\
             UPDATE photos SET deleted_at = NULL WHERE id = (SELECT photo_id FROM posts WHERE id = '{target}')"
        ),
    );
    let likes_before = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    expect(&mut c.failures, likes_before > 0, || {
        "시드에 좋아요가 있어야 롤백을 확인할 수 있다".to_owned()
    });
    let trigger = Trigger {
        name: "author_del_likes",
        table: "post_likes",
        event: "BEFORE DELETE",
        when: format!("OLD.post_id = '{target}'"),
        body: RAISE_BODY.to_owned(),
    };
    let call = user_call(
        "DELETE",
        format!("/api/v1/posts/{target}"),
        &user_uuid(76),
        None,
    );
    fail_on_purpose(env, db, &mut c, &trigger, &call);
    let post_alive = scalar_i64(
        db,
        &format!("SELECT count(*) FROM posts WHERE id = '{target}' AND deleted_at IS NULL"),
    );
    let likes_after = scalar_i64(
        db,
        &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
    );
    c.failures.extend(mismatches(&[
        ("게시물 삭제 표시는 롤백", 1, post_alive),
        ("좋아요는 그대로", likes_before, likes_after),
    ]));
    c.state = json!({ "post_alive": post_alive, "likes_kept": likes_after == likes_before });
    c.detail = json!({ "likes_before": likes_before, "likes_after": likes_after });
    c
}

/// 회원 탈퇴: 소셜 계정 DELETE -> 리프레시 토큰 폐기(계보 잠금 포함) -> 회원 UPDATE. 회원 UPDATE를 실패시킨다.
fn atomic_withdraw(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("atomic-withdraw-user-update-fail");
    c.compare_statuses = true;
    let id = iso_user_id(1);
    if let Err(e) = make_user(db, 1, 2) {
        c.failures.push(e);
        return c;
    }
    let trigger = Trigger {
        name: "withdraw_user",
        table: "users",
        event: "BEFORE UPDATE",
        when: format!("OLD.id = '{id}'"),
        body: RAISE_BODY.to_owned(),
    };
    let call = user_call("DELETE", "/api/v1/users/me".into(), &id, None);
    fail_on_purpose(env, db, &mut c, &trigger, &call);
    let active = scalar_i64(
        db,
        &format!("SELECT count(*) FROM users WHERE id = '{id}' AND deleted_at IS NULL"),
    );
    let socials = scalar_i64(
        db,
        &format!("SELECT count(*) FROM social_accounts WHERE user_id = '{id}'"),
    );
    let live = live_tokens(db, &id);
    c.failures.extend(mismatches(&[
        ("회원은 탈퇴하지 않은 채", 1, active),
        ("소셜 계정 삭제는 롤백", 1, socials),
        ("리프레시 토큰 폐기는 롤백", 2, live),
    ]));
    // 트리거를 지우면 같은 요청이 끝까지 처리된다.
    let retry = status_of(env, &call);
    c.record(&[retry]);
    let withdrawn = scalar_i64(
        db,
        &format!("SELECT count(*) FROM users WHERE id = '{id}' AND deleted_at IS NOT NULL"),
    );
    let socials_after = scalar_i64(
        db,
        &format!("SELECT count(*) FROM social_accounts WHERE user_id = '{id}'"),
    );
    let live_after = live_tokens(db, &id);
    expect(&mut c.failures, (200..300).contains(&retry), || {
        format!("트리거를 지운 뒤 재시도는 2xx: {retry}")
    });
    c.failures.extend(mismatches(&[
        ("재시도 뒤 탈퇴 처리", 1, withdrawn),
        ("재시도 뒤 소셜 계정 없음", 0, socials_after),
        ("재시도 뒤 살아 있는 토큰 없음", 0, live_after),
    ]));
    c.state = json!({
        "active_after_failure": active, "socials_after_failure": socials, "live_tokens_after_failure": live,
        "retry_status": retry, "withdrawn_after_retry": withdrawn, "socials_after_retry": socials_after, "live_tokens_after_retry": live_after,
    });
    c.detail = c.state.clone();
    let _ = purge_user(db, &id);
    c
}

/// 리프레시 토큰 회전: 기존 토큰 회전 표시 UPDATE -> 후속 토큰 INSERT. 후속 토큰 INSERT를 실패시킨다.
fn atomic_refresh(env: &Env, db: &mut Pg, name: &'static str, admin: bool) -> Case {
    let mut c = Case::new(name);
    c.compare_statuses = true;
    let n = if admin { 3 } else { 2 };
    let (table, owner_cond, session) = if admin {
        (
            "admin_refresh_tokens",
            format!("NEW.admin_id = '{}'", env.admin_id),
            iso_session_id(n, 99),
        )
    } else {
        (
            "user_refresh_tokens",
            format!("NEW.user_id = '{}'", iso_user_id(n)),
            iso_session_id(n, 0),
        )
    };
    let token = if admin {
        make_admin_token(db, env, n)
    } else {
        make_user(db, n, 1).map(|mut t| t.remove(0))
    };
    let token = match token {
        Ok(t) => t,
        Err(e) => {
            c.failures.push(e);
            return c;
        }
    };
    let trigger = Trigger {
        name: if admin { "admin_rotate" } else { "user_rotate" },
        table,
        event: "BEFORE INSERT",
        when: format!("{owner_cond} AND NEW.session_id = '{session}'"),
        body: RAISE_BODY.to_owned(),
    };
    let call = refresh_call(admin, &token);
    fail_on_purpose(env, db, &mut c, &trigger, &call);
    let rows = token_rows(db, table, &session);
    let untouched = [TokenRow {
        rotated: false,
        revoked: false,
    }];
    expect(&mut c.failures, rows == untouched, || {
        format!("회전 표시가 롤백돼 토큰이 그대로여야 한다: {rows:?}")
    });
    // 롤백된 토큰은 아직 쓸 수 있다.
    let retry = status_of(env, &call);
    c.record(&[retry]);
    let rows_after = token_rows(db, table, &session);
    expect(&mut c.failures, retry == 200, || {
        format!("트리거를 지운 뒤 같은 토큰으로 재시도는 200: {retry}")
    });
    expect(&mut c.failures, rows_after.len() == 2, || {
        format!("재시도 뒤 행은 둘: {rows_after:?}")
    });
    c.state = json!({
        "rows_after_failure": rows.len(), "rotated_after_failure": rows.iter().filter(|r| r.rotated).count(),
        "retry_status": retry, "rows_after_retry": rows_after.len(),
        "rotated_after_retry": rows_after.iter().filter(|r| r.rotated).count(),
    });
    c.detail = c.state.clone();
    if admin {
        let _ = db.batch_execute(&format!(
            "DELETE FROM admin_refresh_tokens WHERE session_id = '{session}'"
        ));
    } else {
        let _ = purge_user(db, &iso_user_id(n));
    }
    c
}

fn atomic_refresh_user(env: &Env, db: &mut Pg) -> Case {
    atomic_refresh(env, db, "atomic-refresh-rotate-fail", false)
}

fn atomic_refresh_admin(env: &Env, db: &mut Pg) -> Case {
    atomic_refresh(env, db, "atomic-admin-refresh-rotate-fail", true)
}

fn ban_call(env: &Env, id: &str, status: &str) -> Call {
    admin_call(
        env,
        "PATCH",
        format!("/api/v1/admin/users/{id}/status"),
        Some(&json!({ "status": status, "reason": "격리 시험" }).to_string()),
    )
}

/// 관리자 회원 차단: 회원 행 잠금·UPDATE -> 감사 로그 INSERT. 감사 로그를 실패시킨다.
fn atomic_ban(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("atomic-admin-ban-audit-fail");
    c.compare_statuses = true;
    let id = iso_user_id(4);
    if let Err(e) = make_user(db, 4, 0) {
        c.failures.push(e);
        return c;
    }
    let trigger = Trigger {
        name: "ban_audit",
        table: "admin_audit_logs",
        event: "BEFORE INSERT",
        when: format!("NEW.target_id = '{id}'"),
        body: RAISE_BODY.to_owned(),
    };
    let call = ban_call(env, &id, "BANNED");
    fail_on_purpose(env, db, &mut c, &trigger, &call);
    let active = scalar_i64(
        db,
        &format!("SELECT count(*) FROM users WHERE id = '{id}' AND status = 'ACTIVE'"),
    );
    let audit = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{id}'"),
    );
    c.failures.extend(mismatches(&[
        ("차단은 롤백돼 ACTIVE 유지", 1, active),
        ("감사 로그 없음", 0, audit),
    ]));
    let retry = status_of(env, &call);
    c.record(&[retry]);
    let banned = scalar_i64(
        db,
        &format!("SELECT count(*) FROM users WHERE id = '{id}' AND status = 'BANNED'"),
    );
    let audit_after = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{id}'"),
    );
    expect(&mut c.failures, retry == 200, || {
        format!("트리거를 지운 뒤 재시도는 200: {retry}")
    });
    c.failures.extend(mismatches(&[
        ("재시도 뒤 차단됨", 1, banned),
        ("재시도 뒤 감사 로그 한 건", 1, audit_after),
    ]));
    c.state = json!({
        "active_after_failure": active, "audit_after_failure": audit,
        "retry_status": retry, "banned_after_retry": banned, "audit_after_retry": audit_after,
    });
    c.detail = c.state.clone();
    let _ = purge_user(db, &id);
    c
}

// ---------------------------------------------------------------------------------------------
// 가시성: 커밋 전에는 안 보이고, 커밋 응답 뒤에는 바로 보인다
// ---------------------------------------------------------------------------------------------

const TRIGGER_DELAY_SECS: f64 = 2.0;

/// 쓰기 요청을 따로 돌리고 `observe`를 그 사이(쓰기 트랜잭션이 트리거에서 멈춰 있는 동안)에 호출한다. 쓰기 응답과 그 시간을
/// 돌려준다.
fn during_write<T>(env: &Env, write: Call, observe: impl FnOnce() -> T) -> ((u16, u128), T) {
    let write_env = env.clone();
    let handle = thread::spawn(move || {
        let t0 = Instant::now();
        let status = status_of(&write_env, &write);
        (status, t0.elapsed().as_millis())
    });
    // 쓰기 요청이 UPDATE까지 가서 트리거에 멈출 시간을 준다.
    thread::sleep(Duration::from_millis(700));
    let seen = observe();
    (handle.join().unwrap_or((0, 0)), seen)
}

fn get_detail(env: &Env, post_id: &str, viewer: u64) -> (u16, Value) {
    send_full(
        env,
        &user_call(
            "GET",
            format!("/api/v1/posts/{post_id}"),
            &user_uuid(viewer),
            None,
        ),
    )
}

fn visibility_title(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("visibility-title-update");
    c.compare_statuses = true;
    let target = post(0, 52);
    let old = "격리-예전".to_owned();
    let new = format!("격리-{}", now() % 1_000_000); // 제목은 10자 이하
    run(
        &mut c,
        db,
        &format!("UPDATE posts SET title = '{old}' WHERE id = '{target}'"),
    );
    let trigger = Trigger {
        name: "title_sleep",
        table: "posts",
        event: "AFTER UPDATE",
        when: format!("NEW.id = '{target}'"),
        body: sleep_body(TRIGGER_DELAY_SECS),
    };
    if let Err(e) = trigger.install(db) {
        c.failures.push(e);
        return c;
    }
    let write = user_call(
        "PUT",
        format!("/api/v1/posts/{target}"),
        &user_uuid(52),
        Some(&json!({ "title": new }).to_string()),
    );
    let mut observer = match crate::run_cmd::connect_db(&env.db_url) {
        Ok(o) => o,
        Err(e) => {
            trigger.remove(db);
            c.failures.push(e);
            return c;
        }
    };
    let ((put_status, put_ms), (get_status, during_body, during_db)) =
        during_write(env, write, || {
            let (s, b) = get_detail(env, &target, 52);
            let d = text(
                &mut observer,
                &format!("SELECT title FROM posts WHERE id = '{target}'"),
            );
            (s, b, d)
        });
    trigger.remove(db);
    let (after_status, after_body) = get_detail(env, &target, 52);
    c.record(&[put_status, get_status, after_status]);
    let during = during_body["title"].as_str().unwrap_or("").to_owned();
    let after = after_body["title"].as_str().unwrap_or("").to_owned();
    expect(&mut c.failures, put_status == 200, || {
        format!("쓰기 응답은 200: {put_status}")
    });
    expect(
        &mut c.failures,
        get_status == 200 && after_status == 200,
        || format!("GET은 200: {get_status}, {after_status}"),
    );
    c.failures.extend(judge_visibility(
        &old,
        &new,
        &during,
        &during_db,
        &after,
        put_ms,
        (TRIGGER_DELAY_SECS * 900.0) as u128,
    ));
    c.state = json!({
        "during_is_old": during == old, "during_db_is_old": during_db == old, "after_is_new": after == new,
    });
    c.detail = json!({ "put_ms": put_ms, "during": during, "after": after });
    run(
        &mut c,
        db,
        &format!("UPDATE posts SET title = '글52' WHERE id = '{target}'"),
    );
    c
}

fn visibility_like(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("visibility-like-count");
    c.compare_statuses = true;
    let target = post(0, 53);
    let liker = user_uuid(200);
    run(
        &mut c,
        db,
        &format!("DELETE FROM post_likes WHERE post_id = '{target}' AND user_id = '{liker}'"),
    );
    let before = get_detail(env, &target, 53).1["likeCount"]
        .as_i64()
        .unwrap_or(-1);
    let trigger = Trigger {
        name: "like_sleep",
        table: "post_likes",
        event: "AFTER INSERT",
        when: format!("NEW.post_id = '{target}' AND NEW.user_id = '{liker}'"),
        body: sleep_body(TRIGGER_DELAY_SECS),
    };
    if let Err(e) = trigger.install(db) {
        c.failures.push(e);
        return c;
    }
    let write = user_call("PUT", format!("/api/v1/posts/{target}/likes"), &liker, None);
    let ((put_status, put_ms), (get_status, during_body)) =
        during_write(env, write, || get_detail(env, &target, 53));
    trigger.remove(db);
    let (after_status, after_body) = get_detail(env, &target, 53);
    c.record(&[put_status, get_status, after_status]);
    let during = during_body["likeCount"].as_i64().unwrap_or(-1);
    let after = after_body["likeCount"].as_i64().unwrap_or(-1);
    expect(&mut c.failures, put_status == 200, || {
        format!("쓰기 응답은 200: {put_status}")
    });
    // DB 쪽 관찰은 제목 시험이 하므로 여기서는 API 응답만 본다(같은 값을 두 번 넣는다).
    c.failures.extend(judge_visibility(
        &before.to_string(),
        &(before + 1).to_string(),
        &during.to_string(),
        &before.to_string(),
        &after.to_string(),
        put_ms,
        (TRIGGER_DELAY_SECS * 900.0) as u128,
    ));
    c.state = json!({
        "during_is_before": during == before, "after_is_before_plus_one": after == before + 1,
    });
    c.detail = json!({ "put_ms": put_ms, "before": before, "during": during, "after": after });
    run(
        &mut c,
        db,
        &format!("DELETE FROM post_likes WHERE post_id = '{target}' AND user_id = '{liker}'"),
    );
    c
}

// ---------------------------------------------------------------------------------------------
// 잠금 순서·잃어버린 갱신
// ---------------------------------------------------------------------------------------------

/// 좋아요(게시물 행 공유 잠금 -> INSERT)와 작성자 삭제(게시물 행 배타 잠금 -> 좋아요 DELETE)가 겹쳐도 삭제된 게시물에 좋아요가
/// 남지 않는다. 좋아요 수를 세는 컬럼이 없어(응답의 `likeCount`는 매번 COUNT) 카운터 잃어버린 갱신 시험은 대상이 없다.
fn like_vs_author_delete(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("like-vs-author-delete-no-orphan");
    let target = post(8, 74);
    let mut orphans = 0;
    let mut deleted_rounds = 0;
    let mut delete_statuses = Vec::new();
    for _ in 0..3 {
        run(
            &mut c,
            db,
            &format!(
                "UPDATE posts SET deleted_at = NULL WHERE id = '{target}';\
                 UPDATE photos SET deleted_at = NULL WHERE id = (SELECT photo_id FROM posts WHERE id = '{target}');\
                 DELETE FROM post_likes WHERE post_id = '{target}'"
            ),
        );
        let mut calls: Vec<Call> = (100..120)
            .map(|u| {
                user_call(
                    "PUT",
                    format!("/api/v1/posts/{target}/likes"),
                    &user_uuid(u),
                    None,
                )
            })
            .collect();
        calls.push(user_call(
            "DELETE",
            format!("/api/v1/posts/{target}"),
            &user_uuid(74),
            None,
        ));
        let st = fire(env, calls);
        c.absorb(&st);
        let (likes, delete) = st.split_at(st.len() - 1);
        delete_statuses.push(delete[0]);
        expect(
            &mut c.failures,
            likes.iter().all(|s| *s == 200 || *s == 404),
            || {
                format!(
                    "좋아요는 200(삭제 전) 또는 404(삭제 뒤): {:?}",
                    tally_of(likes)
                )
            },
        );
        expect(
            &mut c.failures,
            delete[0] == 204 || delete[0] == 200,
            || format!("삭제 응답: {}", delete[0]),
        );
        orphans += scalar_i64(
            db,
            &format!("SELECT count(*) FROM post_likes WHERE post_id = '{target}'"),
        );
        deleted_rounds += scalar_i64(
            db,
            &format!("SELECT count(*) FROM posts WHERE id = '{target}' AND deleted_at IS NOT NULL"),
        );
    }
    expect(&mut c.failures, orphans == 0, || {
        format!("삭제된 게시물에 좋아요가 남았다: {orphans}")
    });
    expect(&mut c.failures, deleted_rounds == 3, || {
        format!("삭제는 매번 반영: {deleted_rounds}/3")
    });
    c.state = json!({ "orphan_likes": orphans, "deleted_rounds": deleted_rounds });
    c.detail = json!({ "delete_statuses": delete_statuses });
    run(
        &mut c,
        db,
        &format!(
            "UPDATE posts SET deleted_at = NULL WHERE id = '{target}';\
             UPDATE photos SET deleted_at = NULL WHERE id = (SELECT photo_id FROM posts WHERE id = '{target}')"
        ),
    );
    c
}

fn tally_of(statuses: &[u16]) -> std::collections::BTreeMap<String, usize> {
    super::tally(statuses)
}

/// 같은 회원을 동시에 8번 차단: 회원 행 잠금이 요청을 한 줄로 세우므로 한 번만 성공한다.
fn admin_ban_x8(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("admin-ban-x8");
    c.compare_statuses = true;
    let id = iso_user_id(5);
    if let Err(e) = make_user(db, 5, 0) {
        c.failures.push(e);
        return c;
    }
    let st = fire(env, (0..8).map(|_| ban_call(env, &id, "BANNED")).collect());
    c.absorb(&st);
    let ok = st.iter().filter(|s| (200..300).contains(*s)).count();
    expect(&mut c.failures, ok == 1, || {
        format!("차단은 한 번만 성공(2xx 1건): {:?}", c.statuses)
    });
    expect(
        &mut c.failures,
        st.iter()
            .all(|s| (200..300).contains(s) || (400..500).contains(s)),
        || format!("나머지는 4xx: {:?}", c.statuses),
    );
    let audit = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{id}'"),
    );
    let banned = scalar_i64(
        db,
        &format!("SELECT count(*) FROM users WHERE id = '{id}' AND status = 'BANNED'"),
    );
    c.failures.extend(mismatches(&[
        ("감사 로그는 한 건", 1, audit),
        ("차단됨", 1, banned),
    ]));
    c.state = json!({ "ok": ok, "audit": audit, "banned": banned });
    c.detail = c.state.clone();
    let _ = purge_user(db, &id);
    c
}

/// 차단과 해제가 섞여 동시에 들어와도 성공마다 감사 로그가 하나이고 최종 상태가 성공 횟수와 맞는다(잃어버린 갱신 없음).
fn admin_ban_unban_mix(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("admin-ban-unban-mix");
    let id = iso_user_id(6);
    if let Err(e) = make_user(db, 6, 0) {
        c.failures.push(e);
        return c;
    }
    let calls = (0..8)
        .map(|i| ban_call(env, &id, if i % 2 == 0 { "BANNED" } else { "ACTIVE" }))
        .collect();
    let st = fire(env, calls);
    c.absorb(&st);
    let ok = st.iter().filter(|s| (200..300).contains(*s)).count();
    let audit = scalar_i64(
        db,
        &format!("SELECT count(*) FROM admin_audit_logs WHERE target_id = '{id}'"),
    );
    let banned = scalar_i64(
        db,
        &format!("SELECT count(*) FROM users WHERE id = '{id}' AND status = 'BANNED'"),
    ) == 1;
    c.failures.extend(judge_ban_toggle(ok, banned, audit));
    expect(&mut c.failures, ok >= 1, || {
        "하나는 성공해야 한다".to_owned()
    });
    c.state = json!({ "consistent": judge_ban_toggle(ok, banned, audit).is_empty() });
    c.detail = json!({ "ok": ok, "audit": audit, "banned": banned });
    let _ = purge_user(db, &id);
    c
}

// ---------------------------------------------------------------------------------------------
// 리프레시 토큰 동시 재사용
// ---------------------------------------------------------------------------------------------

fn refresh_reuse(env: &Env, db: &mut Pg, name: &'static str, admin: bool) -> Case {
    let mut c = Case::new(name);
    c.compare_statuses = true;
    const N: usize = 20;
    let n = if admin { 8 } else { 7 };
    let (table, session) = if admin {
        ("admin_refresh_tokens", iso_session_id(n, 99))
    } else {
        ("user_refresh_tokens", iso_session_id(n, 0))
    };
    let token = if admin {
        make_admin_token(db, env, n)
    } else {
        make_user(db, n, 1).map(|mut t| t.remove(0))
    };
    let token = match token {
        Ok(t) => t,
        Err(e) => {
            c.failures.push(e);
            return c;
        }
    };
    let st = fire(env, (0..N).map(|_| refresh_call(admin, &token)).collect());
    c.absorb(&st);
    let rows = token_rows(db, table, &session);
    c.failures.extend(judge_refresh_race(N, &st, &rows));
    // 폐기된 계보는 처음 토큰을 다시 내도 살아나지 않는다.
    let again = status_of(env, &refresh_call(admin, &token));
    expect(&mut c.failures, again == 401, || {
        format!("재사용 감지 뒤에는 401: {again}")
    });
    c.state = json!({
        "rows": rows.len(), "rotated": rows.iter().filter(|r| r.rotated).count(),
        "revoked": rows.iter().filter(|r| r.revoked).count(), "again_status": again,
    });
    c.detail = c.state.clone();
    if admin {
        let _ = db.batch_execute(&format!(
            "DELETE FROM admin_refresh_tokens WHERE session_id = '{session}'"
        ));
    } else {
        let _ = purge_user(db, &iso_user_id(n));
    }
    c
}

fn refresh_reuse_user(env: &Env, db: &mut Pg) -> Case {
    refresh_reuse(env, db, "refresh-reuse-x20", false)
}

fn refresh_reuse_admin(env: &Env, db: &mut Pg) -> Case {
    refresh_reuse(env, db, "admin-refresh-reuse-x20", true)
}

// ---------------------------------------------------------------------------------------------
// 가입 토큰 동시 사용
// ---------------------------------------------------------------------------------------------

/// 해시에서 UUID 모양의 문자열(36자)을 만든다.
fn uuid_from(seed: &str) -> String {
    let h = sha256_hex(seed);
    format!(
        "{}-{}-4{}-8{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        &h[17..20],
        &h[20..32]
    )
}

struct SignupToken {
    value: String,
    jti: String,
    upload_id: String,
}

fn signup_token(key: &[u8], subject: &str, email: &str, seed: &str) -> SignupToken {
    let jti = uuid_from(&format!("jti-{seed}"));
    let upload_id = uuid_from(&format!("upload-{seed}"));
    let iat = now() - 5;
    let claims = json!({
        "sub": subject, "aud": "chalkak-social-signup", "uploadId": upload_id,
        "purpose": "SOCIAL_SIGNUP", "provider": "GOOGLE", "iss": "chalkak-backend",
        "exp": iat + 300, "iat": iat, "jti": jti, "email": email,
    });
    SignupToken {
        value: hs256_jwt(key, &claims),
        jti,
        upload_id,
    }
}

/// 가짜 S3가 가입 토큰들의 사인 이미지(원본·썸네일)를 올라간 것으로 답하게 한다. 빈 목록이면 지정을 비운다.
fn fake_uploaded(env: &Env, upload_ids: &[&str]) -> Result<(), String> {
    let addr = env.fakes.ok_or("가짜 서버 주소가 없다")?;
    let mut s3 = serde_json::Map::new();
    for id in upload_ids {
        for kind in ["original", "thumbnail"] {
            s3.insert(
                format!("chalkak/signatures/local/{kind}/{id}.png"),
                json!({ "size": 1024, "content_type": "image/png" }),
            );
        }
    }
    let body = json!({ "s3": s3 }).to_string();
    let mut client = Client::new(addr, false);
    let status = client
        .send(&Request {
            method: "PUT",
            path: "/__admin/responses",
            bearer: None,
            body: Some(&body),
        })
        .map(|r| r.status)
        .map_err(|e| format!("가짜 서버 호출 실패: {e}"))?;
    if status == 200 {
        Ok(())
    } else {
        Err(format!("가짜 서버가 S3 응답 지정을 거절했다: {status}"))
    }
}

fn skipped(name: &'static str, why: &str) -> Case {
    let mut c = Case::new(name);
    c.detail = json!({ "skipped": why });
    c.state = json!({ "skipped": true });
    c
}

fn signup_cleanup(env: &Env, db: &mut Pg, email: &str, jtis: &[&str]) {
    if let Ok(Some(row)) = db.query_opt("SELECT id::text FROM users WHERE email = $1", &[&email]) {
        let id: String = row.get(0);
        let _ = purge_user(db, &id);
    }
    for jti in jtis {
        let _ = db.execute("DELETE FROM consumed_signup_tokens WHERE jti = $1", &[jti]);
    }
    let _ = fake_uploaded(env, &[]);
}

fn signup_counts(db: &mut Pg, email: &str) -> (i64, i64, i64) {
    let q = |db: &mut Pg, sql: &str| -> i64 {
        db.query_one(sql, &[&email]).map(|r| r.get(0)).unwrap_or(-1)
    };
    (
        q(db, "SELECT count(*) FROM users WHERE email = $1"),
        q(
            db,
            "SELECT count(*) FROM social_accounts WHERE user_id IN (SELECT id FROM users WHERE email = $1)",
        ),
        q(
            db,
            "SELECT count(*) FROM user_refresh_tokens WHERE user_id IN (SELECT id FROM users WHERE email = $1)",
        ),
    )
}

/// 같은 가입 토큰으로 10개가 동시에 가입을 끝내려 한다. `consumed_signup_tokens`의 `ON CONFLICT DO NOTHING`이 한 번만 소진시키므로
/// 회원은 하나다. 가입이 커밋된 뒤 들어온 요청은 소셜 계정이 이미 있어 기존 회원으로 로그인 결과를 받는다.
fn signup_same_token(env: &Env, db: &mut Pg) -> Case {
    let name = "signup-same-token-x10";
    let (Some(_), Some(key)) = (env.fakes, env.signup_key.clone()) else {
        return skipped(name, "--fakes와 --signup-secret이 필요하다");
    };
    let mut c = Case::new(name);
    let email = format!("iso-signup-{}@test.chalkak", nanos());
    let token = signup_token(&key, &format!("iso-sub-{}", nanos()), &email, &email);
    if let Err(e) = fake_uploaded(env, &[&token.upload_id]) {
        c.failures.push(e);
        return c;
    }
    let body = json!({ "signupToken": token.value }).to_string();
    let st = fire(
        env,
        (0..10)
            .map(|_| anonymous_post("/api/v1/auth/social-signup", body.clone()))
            .collect(),
    );
    c.absorb(&st);
    let ok = st.iter().filter(|s| **s == 200).count();
    let (users, socials, tokens) = signup_counts(db, &email);
    let consumed = scalar_i64(
        db,
        &format!(
            "SELECT count(*) FROM consumed_signup_tokens WHERE jti = '{}'",
            token.jti
        ),
    );
    expect(&mut c.failures, ok >= 1, || {
        format!("하나 이상 가입에 성공해야 한다: {:?}", c.statuses)
    });
    expect(
        &mut c.failures,
        st.iter().all(|s| *s == 200 || *s == 400 || *s == 409),
        || format!("나머지는 사용된 토큰 거절(4xx): {:?}", c.statuses),
    );
    c.failures.extend(judge_signup(ok, users, socials, tokens));
    c.failures
        .extend(mismatches(&[("가입 토큰은 한 번만 소진", 1, consumed)]));
    c.state = json!({ "users": users, "socials": socials, "consumed": consumed, "tokens_match_ok": tokens == ok as i64 });
    c.detail = json!({ "ok": ok, "refresh_tokens": tokens });
    signup_cleanup(env, db, &email, &[&token.jti]);
    c
}

/// 같은 소셜 신원의 가입 토큰이 서로 다르게 4개 동시에 쓰인다(업로드 URL을 여러 번 받은 경우). 회원·소셜 계정은 하나이고, 진 쪽의
/// 트랜잭션은 회원 행까지 통째로 롤백돼 고아 회원이 남지 않는다. 진 쪽의 응답 코드(유일 제약 위반의 오류 매핑)는 구현별로
/// 보고서에서 대조한다.
fn signup_same_identity(env: &Env, db: &mut Pg) -> Case {
    let name = "signup-same-identity-x4";
    let (Some(_), Some(key)) = (env.fakes, env.signup_key.clone()) else {
        return skipped(name, "--fakes와 --signup-secret이 필요하다");
    };
    let mut c = Case::new(name);
    let stamp = nanos();
    let email = format!("iso-signup2-{stamp}@test.chalkak");
    let subject = format!("iso-sub2-{stamp}");
    let tokens: Vec<SignupToken> = (0..4)
        .map(|i| signup_token(&key, &subject, &email, &format!("{stamp}-{i}")))
        .collect();
    let ids: Vec<&str> = tokens.iter().map(|t| t.upload_id.as_str()).collect();
    if let Err(e) = fake_uploaded(env, &ids) {
        c.failures.push(e);
        return c;
    }
    let st = fire(
        env,
        tokens
            .iter()
            .map(|t| {
                anonymous_post(
                    "/api/v1/auth/social-signup",
                    json!({ "signupToken": t.value }).to_string(),
                )
            })
            .collect(),
    );
    c.record(&st);
    let ok = st.iter().filter(|s| **s == 200).count();
    let server_errors = st.iter().filter(|s| **s == 0 || **s >= 500).count();
    let (users, socials, refresh_rows) = signup_counts(db, &email);
    expect(&mut c.failures, ok >= 1, || {
        format!("하나 이상 가입에 성공해야 한다: {:?}", c.statuses)
    });
    c.failures
        .extend(judge_signup(ok, users, socials, refresh_rows));
    c.state =
        json!({ "users": users, "socials": socials, "tokens_match_ok": refresh_rows == ok as i64 });
    c.detail = json!({ "ok": ok, "server_errors": server_errors, "refresh_tokens": refresh_rows });
    let jtis: Vec<&str> = tokens.iter().map(|t| t.jti.as_str()).collect();
    signup_cleanup(env, db, &email, &jtis);
    c
}

// ---------------------------------------------------------------------------------------------
// 계보 잠금 순서(교착)
// ---------------------------------------------------------------------------------------------

/// 같은 회원의 탈퇴 12개가 동시에 계보 4개를 잠근다. 계보 잠금을 모든 경로가 같은 순서(세션 id 오름차순)로 잡으므로 교착이 없다.
/// 교착이 나면 PostgreSQL이 한쪽을 죽여(40P01) 5xx가 된다. 소셜 계정이 없는 회원이라 계보 잠금과 회원 행 갱신만 겹친다.
fn withdraw_lock_order(env: &Env, db: &mut Pg) -> Case {
    withdraw_x12(env, db, "withdraw-lock-order-x12", false)
}

/// 소셜 계정이 있는 회원의 탈퇴 12개가 동시에 들어온다. 원본(Java)은 소셜 계정을 엔티티로 읽어 한 건씩 지우므로, 먼저 커밋한 탈퇴가
/// 지운 행을 뒤따른 탈퇴가 다시 지우다 `StaleStateException`(삭제 행 수 0)으로 500이 된다. Rust는 `DELETE ... WHERE user_id`
/// 한 문장이라 오류 없이 끝난다. 이 알려진 차이 때문에 5xx를 실패로 세지 않고 `any_server_error`를 최종 상태로 남겨 보고서가 대조한다.
fn withdraw_duplicate(env: &Env, db: &mut Pg) -> Case {
    withdraw_x12(env, db, "withdraw-duplicate-x12", true)
}

fn withdraw_x12(env: &Env, db: &mut Pg, name: &'static str, with_social: bool) -> Case {
    let mut c = Case::new(name);
    let n = if with_social { 11 } else { 9 };
    let id = iso_user_id(n);
    let mut live = 0;
    let mut active = 0;
    let mut rounds_ok = 0;
    let mut server_errors = 0;
    for _ in 0..3 {
        if let Err(e) = make_user_with(db, n, 4, with_social) {
            c.failures.push(e);
            return c;
        }
        let st = fire(
            env,
            (0..12)
                .map(|_| user_call("DELETE", "/api/v1/users/me".into(), &id, None))
                .collect(),
        );
        if with_social {
            c.record(&st);
        } else {
            c.absorb(&st);
        }
        server_errors += st.iter().filter(|s| **s == 0 || **s >= 500).count();
        rounds_ok += st.iter().filter(|s| (200..300).contains(*s)).count();
        live += live_tokens(db, &id);
        active += scalar_i64(
            db,
            &format!("SELECT count(*) FROM users WHERE id = '{id}' AND deleted_at IS NULL"),
        );
    }
    expect(&mut c.failures, rounds_ok >= 3, || {
        format!("라운드마다 하나 이상 탈퇴에 성공해야 한다: {rounds_ok}")
    });
    c.failures.extend(mismatches(&[
        ("탈퇴 뒤 살아 있는 토큰 없음", 0, live),
        ("탈퇴 처리된 회원", 0, active),
    ]));
    c.state = json!({ "live_tokens": live, "still_active": active });
    if with_social {
        c.state["any_server_error"] = json!(server_errors > 0);
        c.known_diff = Some(
            "Java는 동시 탈퇴에서 social_accounts 엔티티 삭제가 StaleStateException(행 수 0)으로 500, Rust는 집합 삭제라 오류 없음",
        );
    }
    c.detail = json!({ "successes": rounds_ok, "server_errors": server_errors });
    let _ = purge_user(db, &id);
    c
}

/// 탈퇴(계보 전체 폐기)와 같은 계보의 리프레시가 겹쳐도, 탈퇴가 커밋된 뒤에는 살아 있는 토큰이 하나도 없다. 회전이 먼저 끝나면
/// 후속 토큰을 탈퇴가 폐기하고, 탈퇴가 먼저면 회전이 폐기된 토큰을 거절한다.
fn withdraw_vs_refresh(env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("withdraw-vs-refresh");
    let id = iso_user_id(10);
    let mut live = 0;
    let mut withdrawn = 0;
    let mut withdraw_statuses = Vec::new();
    for _ in 0..5 {
        let tokens = match make_user(db, 10, 3) {
            Ok(t) => t,
            Err(e) => {
                c.failures.push(e);
                return c;
            }
        };
        let mut calls = vec![user_call("DELETE", "/api/v1/users/me".into(), &id, None)];
        for t in &tokens {
            calls.push(refresh_call(false, t));
            calls.push(refresh_call(false, t));
        }
        let st = fire(env, calls);
        c.absorb(&st);
        withdraw_statuses.push(st[0]);
        expect(&mut c.failures, (200..300).contains(&st[0]), || {
            format!("탈퇴는 성공: {}", st[0])
        });
        expect(
            &mut c.failures,
            st[1..].iter().all(|s| *s == 200 || *s == 401),
            || format!("리프레시는 200 또는 401: {:?}", tally_of(&st[1..])),
        );
        live += live_tokens(db, &id);
        withdrawn += scalar_i64(
            db,
            &format!("SELECT count(*) FROM users WHERE id = '{id}' AND deleted_at IS NOT NULL"),
        );
    }
    c.failures.extend(mismatches(&[
        ("탈퇴가 커밋된 뒤 살아 있는 토큰 없음", 0, live),
        ("탈퇴 처리된 회원(5라운드)", 5, withdrawn),
    ]));
    c.state = json!({ "live_tokens": live, "withdrawn_rounds": withdrawn });
    c.detail = json!({ "withdraw_statuses": withdraw_statuses });
    let _ = purge_user(db, &id);
    c
}

// ---------------------------------------------------------------------------------------------
// 정리 확인
// ---------------------------------------------------------------------------------------------

/// 앞선 격리 시험이 트리거·함수·전용 행을 하나도 남기지 않았는지 확인한다.
fn clean_slate(_env: &Env, db: &mut Pg) -> Case {
    let mut c = Case::new("isolation-clean-slate");
    c.compare_statuses = true;
    let triggers = scalar_i64(
        db,
        "SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal AND tgname LIKE 'race\\_iso\\_%'",
    );
    let functions = scalar_i64(
        db,
        "SELECT count(*) FROM pg_proc WHERE proname LIKE 'race\\_iso\\_%'",
    );
    let users = scalar_i64(
        db,
        "SELECT count(*) FROM users WHERE id::text LIKE '01990000-0000-7000-8009-%'",
    );
    let sessions = scalar_i64(
        db,
        "SELECT (SELECT count(*) FROM user_refresh_tokens WHERE session_id::text LIKE '01990000-0000-7000-800a-%') \
         + (SELECT count(*) FROM admin_refresh_tokens WHERE session_id::text LIKE '01990000-0000-7000-800a-%')",
    );
    c.failures.extend(mismatches(&[
        ("남은 시험 트리거", 0, triggers),
        ("남은 시험 함수", 0, functions),
        ("남은 전용 회원", 0, users),
        ("남은 전용 리프레시 토큰", 0, sessions),
    ]));
    c.state =
        json!({ "triggers": triggers, "functions": functions, "users": users, "tokens": sessions });
    c.detail = c.state.clone();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(rotated: bool, revoked: bool) -> TokenRow {
        TokenRow { rotated, revoked }
    }

    #[test]
    fn 기대와_다른_항목만_메시지로_만든다() {
        assert!(mismatches(&[("a", 1, 1), ("b", 0, 0)]).is_empty());
        let m = mismatches(&[("a", 1, 2), ("b", 0, 0)]);
        assert_eq!(m, ["a: 기대 1, 실제 2"]);
    }

    #[test]
    fn 리프레시_동시_재사용은_하나만_성공하고_계보가_폐기된다() {
        let mut st = vec![401; 19];
        st.push(200);
        let rows = [row(true, true), row(false, true)];
        assert!(judge_refresh_race(20, &st, &rows).is_empty());
    }

    #[test]
    fn 리프레시_성공이_둘이면_실패다() {
        let st = [200, 200, 401];
        let rows = [row(true, true), row(false, true)];
        let f = judge_refresh_race(3, &st, &rows);
        assert!(f.iter().any(|m| m.contains("정확히 하나")), "{f:?}");
        assert!(f.iter().any(|m| m.contains("모두 401")), "{f:?}");
    }

    #[test]
    fn 리프레시_재사용_뒤_폐기되지_않은_행이_남으면_실패다() {
        let st = [200, 401];
        let rows = [row(true, false), row(false, false)];
        let f = judge_refresh_race(2, &st, &rows);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("폐기"));
    }

    #[test]
    fn 리프레시_요청이_하나뿐이면_폐기를_요구하지_않는다() {
        let rows = [row(true, false), row(false, false)];
        assert!(judge_refresh_race(1, &[200], &rows).is_empty());
    }

    #[test]
    fn 리프레시_행_수와_회전_수를_본다() {
        let f = judge_refresh_race(2, &[200, 401], &[row(true, true)]);
        assert!(f.iter().any(|m| m.contains("행은 둘")), "{f:?}");
        let f = judge_refresh_race(2, &[200, 401], &[row(false, true), row(false, true)]);
        assert!(f.iter().any(|m| m.contains("회전된 행")), "{f:?}");
    }

    #[test]
    fn 가시성은_전에는_예전_값_뒤에는_새_값() {
        assert!(judge_visibility("a", "b", "a", "a", "b", 2000, 1800).is_empty());
        let f = judge_visibility("a", "b", "b", "a", "b", 2000, 1800);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("커밋 전 GET"));
        let f = judge_visibility("a", "b", "a", "b", "a", 2000, 1800);
        assert_eq!(f.len(), 2, "{f:?}");
    }

    #[test]
    fn 쓰기가_트리거_지연보다_빠르면_시험이_무효다() {
        let f = judge_visibility("a", "b", "a", "a", "b", 100, 1800);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("트리거 지연"));
    }

    #[test]
    fn 차단_해제_혼합은_성공_횟수의_홀짝이_최종_상태를_정한다() {
        assert!(judge_ban_toggle(3, true, 3).is_empty());
        assert!(judge_ban_toggle(4, false, 4).is_empty());
        assert!(judge_ban_toggle(0, false, 0).is_empty());
        assert_eq!(judge_ban_toggle(3, false, 3).len(), 1);
        assert_eq!(judge_ban_toggle(2, false, 1).len(), 1);
    }

    #[test]
    fn 가입_결과는_회원_소셜_계정_하나와_성공마다_토큰_하나() {
        assert!(judge_signup(3, 1, 1, 3).is_empty());
        assert_eq!(judge_signup(3, 2, 1, 3).len(), 1);
        assert_eq!(judge_signup(3, 1, 1, 2).len(), 1);
        assert_eq!(judge_signup(0, 0, 0, 0).len(), 2, "회원 0은 실패");
    }

    #[test]
    fn 트리거_문장에_조건과_정리가_들어간다() {
        let t = Trigger {
            name: "x",
            table: "notifications",
            event: "BEFORE INSERT",
            when: "NEW.post_id = 'p'".into(),
            body: RAISE_BODY.into(),
        };
        let install = t.install_sql();
        assert!(install.contains("CREATE TRIGGER race_iso_x BEFORE INSERT ON notifications"));
        assert!(install.contains("WHEN (NEW.post_id = 'p')"));
        assert!(install.contains("RAISE EXCEPTION"));
        assert!(install.contains("race_iso_x_fn()"));
        let remove = t.remove_sql();
        assert!(remove.contains("DROP TRIGGER IF EXISTS race_iso_x ON notifications"));
        assert!(remove.contains("DROP FUNCTION IF EXISTS race_iso_x_fn()"));
    }

    #[test]
    fn 시험_트리거_이름은_모두_접두사로_시작한다() {
        let t = Trigger {
            name: "a",
            table: "t",
            event: "AFTER UPDATE",
            when: "true".into(),
            body: sleep_body(1.5),
        };
        assert!(t.trigger_name().starts_with("race_iso_"));
        assert!(t.function_name().starts_with("race_iso_"));
        assert!(t.install_sql().contains("pg_sleep(1.5)"));
    }

    #[test]
    fn 해시에서_만든_uuid는_36자이고_입력마다_다르다() {
        let a = uuid_from("a");
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
        assert_ne!(a, uuid_from("b"));
        assert_eq!(a, uuid_from("a"));
    }

    #[test]
    fn 전용_회원과_계보_식별자는_정리_검사의_접두사를_따른다() {
        assert!(iso_user_id(7).starts_with("01990000-0000-7000-8009-"));
        assert!(iso_session_id(7, 2).starts_with("01990000-0000-7000-800a-"));
        assert_ne!(iso_session_id(7, 0), iso_session_id(7, 1));
        assert_ne!(iso_session_id(7, 99), iso_session_id(8, 0));
    }

    #[test]
    fn 토큰_해시는_저장소_형식_64자리_소문자_16진수() {
        let h = sha256_hex("abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
