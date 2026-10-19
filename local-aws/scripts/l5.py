#!/usr/bin/env python3
"""L5 안정성 시험 S1~S6·S8: 실제 AWS 흐름(S3 → SQS → Lambda → 콜백)이 붙은 로컬 스택에서 백엔드(rust | java)를 같은 주입으로 시험한다.

계획 7.2 의 기준과 구현은 docs/local-aws/report-l5.md. 보통은 scripts/l5.sh 가 스택을 새로 올린 뒤 이 스크립트를 돈다.

  l5.py <rust|java> [--fresh] S1 S2 ...   시나리오 실행. --fresh 는 down -v 로 비우고 스택을 올려 시드까지 넣는다(없으면 떠 있는 스택을 쓴다)
  l5.py compare [시나리오...]       .state/l5 의 가장 최근 rust·java 결과를 나란히 놓고 A/B 기준(p99 ≤ Java, 응답 형식 같음)을 판정

시나리오
  S1  정상 부하 30분: 혼합 부하(edge 경유 고정 RPS) + 업로드 흐름 + 앱 한 대 역할의 게시물 작성(canary)
  S2  업로드 몰림: 1분에 300건. 모두 7분 안에 닫히는지, 고아 ISSUED·VALIDATING 0
  S3  Lambda 콜백 중복·역순, 같은 S3 이벤트 두 번(SQS 중복 전달)
  S4  부하 중 백엔드 재시작(정상 종료 한 번, SIGKILL 한 번): SQS 재전달로 회복, 확인된 쓰기 손실 0
  S5  DB 지연 200ms, DB 경로 10초 끊김(toxiproxy): 풀 대기 타임아웃 응답 형식, 회복
  S6  S3 장애(HeadObject 5xx·403·404·지연, Lambda 의 GET 5xx·결과 경로 403): 응답과 재시도 상한 뒤 실패 콜백
  S8  느린 클라이언트: 요청 없이 열린 연결, 느린 헤더·본문, 느린 업로드 중에도 서버가 멈추지 않는지 (+ loadgen stress slow/idle)

환경변수(짧게 돌려 보기용): S1_MINUTES(30), S1_RPS(100), S1_UPLOAD_RATE(0.3), S2_RATE(5), S2_SECONDS(60), S2_CANARIES(10), S4_UPLOAD_SECONDS(150),
  S5_RPS(30), S5_LATENCY_SECONDS(120), S8_IDLE(800), S8_SECONDS(60), 그리고 e2e_image 의 HOST_IP, TIMEOUT_SECONDS
"""

import json
import os
import re
import socket
import ssl
import sys
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # python3 -I 는 스크립트 폴더를 경로에 넣지 않는다
from l5_lib import *  # noqa: E402,F401,F403
import l5_lib as L  # noqa: E402

KST_DAY_SECONDS = 86400
SCENARIOS = {}


def scenario(fn):
    SCENARIOS[fn.__name__.upper()] = fn
    return fn


def envf(name, default):
    return float(os.environ.get(name, default))


def run_dir(stack, name):
    path = STATE / f"run-{stack.backend}-{name}-{time.strftime('%Y%m%d-%H%M%S')}"
    path.mkdir(parents=True, exist_ok=True)
    return path


def ms(us):
    return None if us is None else round(us / 1000.0, 1)


def run_summary(data):
    """loadgen run 결과 JSON 에서 비교에 쓰는 수치."""
    if not data:
        return {}
    total = data["total"]
    out = {"requests": total["count"], "errors": total["errors"], "rps": round(total["rps"], 1), "p50_ms": ms(total["p50_us"]),
           "p95_ms": ms(total["p95_us"]), "p99_ms": ms(total["p99_us"]), "max_ms": ms(total["max_us"])}
    if "system" in data:
        out.update({"srv_rss_mb_max": round(data["system"]["rss_mb_max"], 1), "srv_cpu_cores": round(data["system"]["cpu_cores"], 2),
                    "srv_fds_max": data["system"]["fds_max"], "srv_threads_max": data["system"]["threads_max"]})
    if "db" in data:
        out["db_connections_max"] = data["db"]["connections_max"]
    out["error_samples"] = data.get("error_samples", [])[:3]
    return out


def uploads_summary(data):
    if not data:
        return {}
    return {"started": data["started"], "ready": data["ready"], "rejected": data["rejected"], "stuck": data["stuck"], "errors": data["errors"],
            "presign_p99_ms": ms(data["presign"]["p99_us"]), "put_p99_ms": ms(data["put"]["p99_us"]),
            "callback_wait_p50_ms": ms(data["callback_wait"]["p50_us"]), "callback_wait_p99_ms": ms(data["callback_wait"]["p99_us"]),
            "callback_wait_max_ms": ms(data["callback_wait"]["max_us"]), "total_p99_ms": ms(data["total"]["p99_us"]),
            "wall_secs": round(data["wall_secs"], 1), "error_samples": data.get("error_samples", [])[:3]}


def canary_summary(results):
    ok = [c for c in results if c["ok"]]
    done = [c["done_s"] for c in ok]
    return {"n": len(results), "ok": len(ok), "done_p50_s": percentile(done, 0.5), "done_max_s": max(done) if done else None,
            "errors": [c.get("error") for c in results if not c["ok"]][:5]}


class Canaries(threading.Thread):
    """백그라운드로 앱 한 대의 게시물 작성 흐름을 주기적으로 돈다."""

    def __init__(self, every, first_after=30, limit=100):
        super().__init__(daemon=True)
        self.every, self.first_after, self.limit = every, first_after, limit
        self.results = []
        self.stop_flag = threading.Event()

    def run(self):
        if self.stop_flag.wait(self.first_after):
            return
        topic = todays_topic()
        while not self.stop_flag.is_set() and len(self.results) < self.limit:
            user = mint_users(1, 1)[0]
            self.results.append(canary_post(topic, user))
            if self.stop_flag.wait(self.every):
                break

    def finish(self):
        self.stop_flag.set()
        self.join(timeout=240)
        return self.results


def edge_windows(entries):
    """edge 접근 로그를 부하 발생기 경로(평문 8081)와 api 호스트(Lambda 콜백·앱 HTTPS)로 나눠 요약."""
    return {"edge_http": edge_summary(entries, host=LOADGEN_HOSTS), "edge_api": edge_summary(entries, host=f"api.{HOST_IP}.sslip.io"),
            "edge_5xx_total": sum(1 for x in entries if x["status"] >= 500)}


def runtime_summary(backend, since):
    rows_ = runtime_log(backend, since)
    out = {"snapshots": len(rows_)}
    if rows_:
        out["hikariActive_max"] = max(r.get("hikariActive", 0) for r in rows_)
        out["hikariPending_max"] = max(r.get("hikariPending", 0) for r in rows_)
        pend = [r.get("hikariPending", 0) for r in rows_]
        out["hikariPending_nonzero_snapshots"] = sum(1 for p in pend if p > 0)
        run = best = 0
        for p in pend:
            run = run + 1 if p > 0 else 0
            best = max(best, run)
        out["hikariPending_consecutive_max"] = best
        out["threads_max"] = max(r.get("threads", 0) for r in rows_)
        for key in ("rssBytes", "heapUsedBytes"):
            values = [r[key] for r in rows_ if key in r]
            if values:
                out[f"{key}_first_mb"] = round(values[0] / 1048576, 1)
                out[f"{key}_last_mb"] = round(values[-1] / 1048576, 1)
                out[f"{key}_max_mb"] = round(max(values) / 1048576, 1)
    return out


def sample_summary(samples, skip=0.2):
    out = {"samples": len(samples)}
    for key, floor in (("rss_mb", 10.0), ("threads", 5.0), ("fds", 5.0)):
        pts = series(samples, key, skip)
        if pts:
            verdict = trend_verdict(pts, floor)
            out[key] = {"first": pts[0][1], "last": pts[-1][1], "max": max(v for _, v in pts), **verdict}
    pg = [s["pg_app"] for s in samples if s.get("pg_app") is not None]
    idle = [s["pg_idle_in_tx"] for s in samples if s.get("pg_idle_in_tx") is not None]
    out["pg_app_max"] = max(pg) if pg else None
    out["pg_idle_in_tx_max"] = max(idle) if idle else None
    load = [s["load1"] for s in samples if s.get("load1") is not None]
    out["host_load1_max"] = max(load) if load else None
    return out


# =============================================================================================
# S1 정상 부하
# =============================================================================================
@scenario
def s1(stack, R):
    minutes, rps, up_rate = envf("S1_MINUTES", 30), int(envf("S1_RPS", 100)), envf("S1_UPLOAD_RATE", 0.3)
    duration = f"{int(minutes * 60)}s"
    out = run_dir(stack, "S1")
    reset_faults()
    say(f"S1: {minutes:g}분, 혼합 부하 {rps} req/s(edge 경유) + 업로드 {up_rate}건/초 + canary(3분마다)")
    t0 = time.time()
    restarts_before = stack.restart_count()
    sampler = Sampler(stack, csv_path=out / "samples.csv").start()
    run_proc, run_log = loadgen_run(mix_args(stack, rps, duration, out / "run.json", f"{stack.backend}-s1", interval="60", pid=stack.pid()), out / "run.log")
    up_proc, up_log = loadgen_run(uploads_args(up_rate, duration, out / "uploads.json", f"{stack.backend}-s1", timeout="7m"), out / "uploads.log")
    canaries = Canaries(every=180)
    canaries.start()
    codes = wait_all([run_proc, up_proc], minutes * 60 + 20 + 7 * 60 + 120)
    canary_results = canaries.finish()
    samples = sampler.stop()
    t1 = time.time()
    run_log.close()
    up_log.close()

    run_data, up_data = load_json(out / "run.json"), load_json(out / "uploads.json")
    summary = run_summary(run_data)
    R.metric("mix", summary)
    R.metric("uploads", uploads_summary(up_data))
    R.metric("canary", canary_summary(canary_results))
    entries = edge_entries(t0, t1)
    R.metric("edge", edge_windows(entries))
    R.metric("runtime_log", runtime_summary(stack.backend, t0))
    R.metric("samples", sample_summary(samples))
    errors, error_samples = backend_error_lines(stack.backend, t0)
    R.metric("backend_error_lines", {"count": errors, "samples": error_samples[:5]})
    R.metric("out_dir", str(out))

    R.check("혼합 부하 오류 0 (5xx·연결 실패·타임아웃 없음)", run_data is not None and summary["errors"] == 0, f"codes={codes[0]} errors={summary.get('errors')} {summary.get('error_samples')}")
    edge = R.metrics["edge"]
    R.check("edge 접근 로그 5xx 0 (api 호스트 + 부하 경로)", edge["edge_5xx_total"] == 0, json.dumps({k: v["status"] for k, v in edge.items() if isinstance(v, dict)}))
    rt, sm = R.metrics["runtime_log"], R.metrics["samples"]
    pending = rt.get("hikariPending_max")
    # 풀 고갈 = 대기가 이어짐(1분 표본이 연달아 2번 이상 pending > 0)이거나 대기 시간 초과 오류. 한 표본만 튀는 것은 분 경계 작업이나 호스트 지연의 순간 대기라 수치만 남긴다.
    R.check("커넥션 풀 고갈 없음 (대기 연속 표본 ≤ 1, 대기 시간 초과 없음, 앱 DB 연결 ≤ 풀 10, idle in transaction 0)",
            (rt.get("hikariPending_consecutive_max") or 0) <= 1 and (sm["pg_app_max"] or 0) <= POOL_SIZE and (sm["pg_idle_in_tx_max"] or 0) == 0,
            f"pending_max={pending} nonzero={rt.get('hikariPending_nonzero_snapshots')}/{rt.get('snapshots')} consecutive={rt.get('hikariPending_consecutive_max')} active_max={rt.get('hikariActive_max')} pg_app_max={sm['pg_app_max']} idle_in_tx_max={sm['pg_idle_in_tx_max']}")
    rss = sm.get("rss_mb")
    R.check(f"RSS 안정 ({stack.backend}: 기울기 ≤ max(평균 5%, 10MB)/시간 이거나 마지막이 앞 절반 최댓값+10MB 이하)", bool(rss and rss["ok"]),
            json.dumps(rss) if rss else "표본 없음")
    R.check("스레드·FD 안정", all(sm.get(k, {}).get("ok", True) for k in ("threads", "fds")), json.dumps({k: sm.get(k) for k in ("threads", "fds")}))
    up = R.metrics["uploads"]
    R.check("업로드 흐름: 모두 닫힘(stuck 0), 반려·오류 0", bool(up) and up["stuck"] == 0 and up["rejected"] == 0 and up["errors"] == 0, json.dumps(up))
    can = R.metrics["canary"]
    R.check("canary(게시물 작성 → 콜백 → PENDING) 모두 성공", can["n"] > 0 and can["ok"] == can["n"], json.dumps(can))
    R.check("백엔드가 재시작되지 않음", stack.restart_count() == restarts_before)
    R.note("p99 ≤ Java 는 compare 가 두 백엔드 결과를 놓고 판정한다.")


# =============================================================================================
# S2 업로드 몰림
# =============================================================================================
@scenario
def s2(stack, R):
    rate, seconds, n_canary = envf("S2_RATE", 5), int(envf("S2_SECONDS", 60)), int(envf("S2_CANARIES", 10))
    out = run_dir(stack, "S2")
    reset_faults()
    say(f"S2: {rate:g}건/초 x {seconds}초 = 약 {int(rate * seconds)}건 + 그 사이 게시물 작성 {n_canary}건, 완료 제한 7분")
    topic = todays_topic()
    users = mint_users(n_canary, 2)
    # 앞 시나리오의 혼합 부하가 PUT 없이 presign 만 한 행(정상적으로 ISSUED 로 남는다)을 치워 고아 판정을 깨끗이 한다.
    psql("DELETE FROM post_image_uploads WHERE status = 'ISSUED' AND claimed_at IS NULL")
    t0 = time.time()
    sampler = Sampler(stack, csv_path=out / "samples.csv").start()
    up_proc, up_log = loadgen_run(uploads_args(rate, f"{seconds}s", out / "uploads.json", f"{stack.backend}-s2", users=100, conns=48, timeout="7m"), out / "uploads.log")
    results = []

    def one(i):
        time.sleep(i * seconds / max(1, n_canary))
        results.append(canary_post(topic, users[i], timeout=420))

    with ThreadPoolExecutor(max(1, n_canary)) as pool:
        list(pool.map(one, range(n_canary)))
    codes = wait_all([up_proc], seconds + 7 * 60 + 120)
    up_log.close()
    # 큐가 빌 때까지 조금 더 본다.
    deadline = time.time() + 60
    while time.time() < deadline and queue_depth() > 0:
        time.sleep(2)
    samples = sampler.stop()
    t1 = time.time()
    data = load_json(out / "uploads.json")
    up = uploads_summary(data)
    R.metric("uploads", up)
    R.metric("canary", canary_summary(results))
    R.metric("edge", edge_windows(edge_entries(t0, t1)))
    R.metric("runtime_log", runtime_summary(stack.backend, t0))
    R.metric("samples", sample_summary(samples, skip=0.0))
    R.metric("esm_concurrency", stack.env["ESM_CONCURRENCY"])
    issued_now = int(psql("SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED'"))
    issued_old = int(psql("SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED' AND created_at < now() - interval '7 minutes'"))
    validating = int(psql(f"SELECT count(*) FROM posts WHERE moderation_status = 'VALIDATING' AND created_at > to_timestamp({t0})"))
    depth = queue_depth()
    R.metric("db_after", {"issued": issued_now, "issued_older_than_7m": issued_old, "validating_posts": validating, "image_queue_depth": depth})
    rejected_posts = int(psql(f"SELECT count(*) FROM posts WHERE moderation_status = 'REJECTED' AND created_at > to_timestamp({t0})"))
    R.metric("lambda_events", lambda_log_events(t0))
    R.check("업로드가 모두 완료 또는 반려로 닫힘 (loadgen stuck 0, 오류 0)", bool(up) and up["stuck"] == 0 and up["errors"] == 0, json.dumps(up))
    R.check("처리 시한 7분 안: 콜백 대기 최댓값 < 420초", bool(up) and (up["callback_wait_max_ms"] or 0) < 420_000, f"max={up.get('callback_wait_max_ms')}ms")
    R.check("고아 PROCESSING 0 (ISSUED 0, 7분 넘은 ISSUED 0, VALIDATING 게시물 0, 큐 비어 있음)", issued_now == 0 and issued_old == 0 and validating == 0 and depth == 0,
            json.dumps(R.metrics["db_after"]))
    can = R.metrics["canary"]
    R.check("몰리는 동안의 게시물 작성도 모두 PENDING 으로 닫힘", can["ok"] == can["n"], json.dumps(can))
    R.check("5xx 0 (edge)", R.metrics["edge"]["edge_5xx_total"] == 0, json.dumps(R.metrics["edge"]))
    R.check("반려(REJECTED) 0 — 정상 이미지가 반려되면 처리 오류", up.get("rejected", 0) == 0 and rejected_posts == 0, f"rejected={up.get('rejected')} posts={rejected_posts}")


# =============================================================================================
# S3 Lambda 콜백 중복·역순
# =============================================================================================
META = {"width": 1600, "height": 1200, "byteSize": 21082}


def upload_row(upload_id):
    r = rows(f"SELECT status, coalesce(rejection_reason,''), coalesce(image_metadata::text,''), extract(epoch from updated_at)::text "
             f"FROM post_image_uploads WHERE id = '{upload_id}'")
    return {"status": r[0][0], "reason": r[0][1], "metadata_set": bool(r[0][2]), "updated": r[0][3]} if r else None


def s3_event_body(key, size):
    return json.dumps({"Records": [{"eventVersion": "2.1", "eventSource": "aws:s3", "awsRegion": "ap-northeast-2", "eventName": "ObjectCreated:Put",
                                    "s3": {"bucket": {"name": "techcourse-project-2026"}, "object": {"key": key, "size": size}}}]})


def normalize_body(body):
    """오류 본문에서 비교할 모양(키 이름, errorCode, message)만."""
    if isinstance(body, dict):
        return {k: (v if k in ("errorCode", "message", "code") else "…") for k, v in sorted(body.items())}
    return body if body is None else str(body)[:120]


@scenario
def s3(stack, R):
    reset_faults()
    steps = []

    def step(name, status, upload_id=None, expect=None, extra=None):
        row = upload_row(upload_id) if upload_id else None
        entry = {"step": name, "status": status, "db": {k: row[k] for k in ("status", "reason", "metadata_set")} if row else None, **(extra or {})}
        steps.append(entry)
        if expect is not None:
            R.check(f"S3 {name}", status == expect, f"HTTP {status} (기대 {expect}) db={entry['db']}")
        return row

    user = mint_users(1, 3)[0]
    token = user[1]
    topic = todays_topic()

    # --- A: 정상 처리된 업로드에 완료 콜백 재전송, 늦은 실패 콜백 ---
    a_id, url, ctype = new_upload(token)
    e.presigned_put(url, webp(), ctype)
    state, took = wait_upload_closed(a_id, 90)
    R.check("A 기준: 실제 흐름으로 READY (Lambda → complete 콜백 1회)", state == "READY", f"{state} {took:.1f}s")
    base = upload_row(a_id)
    s, _ = signed_callback(f"{a_id}/complete", {**META})
    row = step("A1 complete 재전송", s, a_id, expect=204)
    R.check("A1 상태·updated_at 불변 (상태 변화는 처음 1회뿐)", row["updated"] == base["updated"] and row["status"] == "READY", f"{base['updated']} -> {row['updated']}")
    s, _ = signed_callback(f"{a_id}/complete", {**META})
    row = step("A2 complete 한 번 더", s, a_id, expect=204)
    R.check("A2 updated_at 불변", row["updated"] == base["updated"])
    s, _ = signed_callback(f"{a_id}/failed", {"reason": "PROCESSING_ERROR"})
    row = step("A3 늦은 failed(완료 뒤)", s, a_id, expect=204)
    R.check("A3 READY 유지(실패 콜백이 완료된 업로드를 뒤집지 않음), 사유 없음", row["status"] == "READY" and row["reason"] == "" and row["updated"] == base["updated"], json.dumps(row))
    try:
        _, post = api("POST", "/api/v1/posts", {"topicId": topic, "photoUploadId": a_id, "title": "s3 a"}, token=token)
        step("A4 중복 콜백 뒤에도 게시물 작성", 201, a_id, extra={"post_status": post.get("moderationStatus")})
        R.check("A4 게시물이 PENDING 으로 만들어짐", post.get("moderationStatus") == "PENDING", post.get("moderationStatus"))
    except Failure as failure:
        step("A4 중복 콜백 뒤에도 게시물 작성", -1, a_id, extra={"error": str(failure)[:200]})
        R.check("A4 게시물이 PENDING 으로 만들어짐", False, str(failure)[:200])

    # --- B: 실패가 먼저, 완료가 나중(역순) ---
    b_id, _, _ = new_upload(mint_users(1, 3)[0][1])
    s, _ = signed_callback(f"{b_id}/failed", {"reason": "TOO_LARGE"})
    row = step("B1 failed(TOO_LARGE) 먼저", s, b_id, expect=204)
    R.check("B1 REJECTED + 사유 TOO_LARGE", row["status"] == "REJECTED" and row["reason"] == "TOO_LARGE", json.dumps(row))
    changed = row["updated"]
    s, _ = signed_callback(f"{b_id}/complete", {**META})
    row = step("B2 complete 가 나중에 도착", s, b_id, expect=204)
    R.check("B2 거절된 업로드를 되살리지 않음(REJECTED 유지, 메타데이터 없음)", row["status"] == "REJECTED" and not row["metadata_set"] and row["updated"] == changed, json.dumps(row))
    s, _ = signed_callback(f"{b_id}/failed", {"reason": "PROCESSING_ERROR"})
    row = step("B3 failed 를 다른 사유로 한 번 더", s, b_id, expect=204)
    R.check("B3 첫 사유 유지", row["reason"] == "TOO_LARGE", json.dumps(row))
    b_user = mint_users(1, 3)[0]
    b2_id, _, _ = new_upload(b_user[1])
    signed_callback(f"{b2_id}/failed", {"reason": "TOO_LARGE"})
    try:
        api("POST", "/api/v1/posts", {"topicId": topic, "photoUploadId": b2_id, "title": "s3 b"}, token=b_user[1], expect=(400, 404, 409, 422))
        step("B4 반려된 업로드로 게시물 작성", "4xx")
    except Failure as failure:
        step("B4 반려된 업로드로 게시물 작성", -1, extra={"error": str(failure)[:200]})
        R.check("B4 반려된 업로드로 게시물 작성은 4xx", False, str(failure)[:200])
    st, body = http("POST", e.API + "/api/v1/posts", json.dumps({"topicId": topic, "photoUploadId": b2_id, "title": "s3 b"}).encode(),
                    {"Content-Type": "application/json", "Authorization": f"Bearer {b_user[1]}"})
    steps[-1]["status"] = st
    steps[-1]["body"] = normalize_body(json.loads(body) if body else None)

    # --- C: 완료 뒤 실패 ---
    c_id, _, _ = new_upload(mint_users(1, 3)[0][1])
    s, _ = signed_callback(f"{c_id}/complete", {**META})
    row = step("C1 complete(ISSUED)", s, c_id, expect=204)
    R.check("C1 READY", row["status"] == "READY", json.dumps(row))
    first = row["updated"]
    s, _ = signed_callback(f"{c_id}/failed", {"reason": "PROCESSING_ERROR"})
    row = step("C2 failed 가 나중에", s, c_id, expect=204)
    R.check("C2 READY 유지", row["status"] == "READY" and row["updated"] == first, json.dumps(row))

    # --- D: 같은 S3 이벤트가 SQS 로 두 번 더 전달됨 ---
    d_user = mint_users(1, 3)[0]
    d_id, url, ctype = new_upload(d_user[1])
    e.presigned_put(url, webp(), ctype)
    state, _ = wait_upload_closed(d_id, 90)
    R.check("D 기준: READY", state == "READY", state)
    d_before = upload_row(d_id)
    key = f"chalkak/staging/dev/posts/{d_id}.webp"
    set_visibility(10)
    since = time.time()
    try:
        for _ in range(2):
            sqs("SendMessage", {"QueueUrl": IMAGE_QUEUE, "MessageBody": s3_event_body(key, 21082)})
        deadline = time.time() + 150
        while time.time() < deadline and queue_depth() > 0:
            time.sleep(2)
    finally:
        set_visibility(360, settle=False)
    rows_ = poller_rows(since)
    kept = [r for r in rows_ if r["event"] == "message_kept"]
    deleted = [r for r in rows_ if r["event"] == "message_deleted"]
    d_after = upload_row(d_id)
    lam = lambda_log_events(since)
    R.metric("duplicate_event", {"deleted": len(deleted), "kept": len(kept), "max_receive_count": max([int(r.get("receiveCount", 1)) for r in kept] or [0]),
                                 "queue_depth_after": queue_depth(), "lambda": lam})
    steps.append({"step": "D 같은 S3 이벤트 2번 추가 전달", "status": "queue", "db": {k: d_after[k] for k in ("status", "reason", "metadata_set")},
                  "queue_empty": queue_depth() == 0, "lambda_abandoned": lam["image_processing_abandoned"]})
    R.check("D 중복 전달 뒤 상태 불변(READY, updated_at 그대로)", d_after["status"] == "READY" and d_after["updated"] == d_before["updated"], json.dumps({"before": d_before, "after": d_after}))
    R.check("D 중복 메시지가 큐에 남지 않음(처리·삭제 또는 상한 후 포기)", queue_depth() == 0, json.dumps(R.metrics["duplicate_event"]))

    # --- E: 인증·형식 ---
    unknown = str(uuid.uuid4())
    for name, path, body, kwargs, expect in (
        ("E1 모르는 uploadId complete", f"{unknown}/complete", {**META}, {}, 204),
        ("E2 모르는 uploadId failed", f"{unknown}/failed", {"reason": "TOO_LARGE"}, {}, 204),
        ("E3 잘못된 서명", f"{c_id}/complete", {**META}, {"tamper": True}, 401),
        ("E4 10분 지난 타임스탬프", f"{c_id}/complete", {**META}, {"timestamp": int(time.time()) - 600}, 401),
        ("E5 다른 환경(prod)으로 서명", f"{c_id}/complete", {**META}, {"environment": "prod"}, 401),
        ("E6 UUID 형식이 아닌 경로", "not-a-uuid/complete", {**META}, {}, 400),
        ("E7 본문 없는 failed", f"{c_id}/failed", None, {}, 400),
        ("E8 알 수 없는 사유의 failed", f"{unknown}/failed", {"reason": "WHATEVER"}, {}, None),
    ):
        status, resp = signed_callback(path, body, **kwargs)
        steps.append({"step": name, "status": status, "body": normalize_body(resp)})
        if expect is not None:
            R.check(f"S3 {name}", status == expect, f"HTTP {status} (기대 {expect}) {normalize_body(resp)}")
    status, resp = signed_callback(f"{unknown}/upload-urls", None)
    steps.append({"step": "E9 모르는 uploadId upload-urls", "status": status, "body": normalize_body(resp)})
    R.metric("steps", steps)
    R.note("각 단계의 상태 코드·DB 상태·오류 본문은 metrics.steps 에 남는다. compare 가 Rust 와 Java 의 steps 를 단계별로 대조한다.")


# =============================================================================================
# S4 백엔드 재시작
# =============================================================================================
def wait_healthy(deadline_s):
    started = time.monotonic()
    while time.monotonic() - started < deadline_s:
        status, _ = raw("GET", f"http://{EDGE_HTTP}/actuator/health", timeout=3)
        if status == 200:
            return time.monotonic() - started
        time.sleep(0.2)
    return None


@scenario
def s4(stack, R):
    seconds = int(envf("S4_UPLOAD_SECONDS", 150))
    out = run_dir(stack, "S4")
    reset_faults()
    run_id = uuid.uuid4().hex[:8]
    say(f"S4: 부하 중 정상 재시작 1번(40초), SIGKILL 1번(100초). 업로드 {seconds}초 동안, 재전달 360초 대기 포함 약 10분")
    t0 = time.time()
    sampler = Sampler(stack, csv_path=out / "samples.csv").start()
    mix_proc, mix_log = loadgen_run(mix_args(stack, 40, f"{seconds + 60}s", out / "run.json", f"{stack.backend}-s4", interval="30"), out / "run.log")
    up_proc, up_log = loadgen_run(uploads_args(1.0, f"{seconds}s", out / "uploads.json", f"{stack.backend}-s4", users=60, conns=16, timeout="11m"), out / "uploads.log")

    # 확인된 쓰기 장부: 피드백 작성. 2xx 로 확인받은 마커는 반드시 DB 에 있어야 한다.
    writer_user = seed_user_id(SEED_USERS - 1)
    token = mint_token(writer_user, ttl=7200)
    acked, attempts, statuses = [], [], {}
    stop = threading.Event()

    def writer():
        n = 0
        while not stop.is_set():
            marker = f"s4-{run_id}-{n}"
            for attempt in range(120):
                status, _ = raw("POST", f"http://{EDGE_HTTP}/api/v1/feedbacks", {"content": marker}, {"Content-Type": "application/json", "Authorization": f"Bearer {token}"}, timeout=10)
                statuses[status] = statuses.get(status, 0) + 1
                attempts.append((time.time(), status))
                if status in (200, 201, 204):
                    acked.append(marker)
                    break
                time.sleep(0.5)
            n += 1
            stop.wait(0.5)

    wt = threading.Thread(target=writer, daemon=True)
    wt.start()
    events = {}
    time.sleep(40 - (time.time() - t0) if time.time() - t0 < 40 else 0)
    events["restart_at"] = time.time()
    sh(["docker", "restart", container(stack.backend)], timeout=120)
    events["restart_healthy_s"] = wait_healthy(180)
    time.sleep(max(0, 100 - (time.time() - t0)))
    events["kill_at"] = time.time()
    sh(["docker", "kill", "-s", "KILL", container(stack.backend)])
    time.sleep(8)
    if sh(["docker", "inspect", "-f", "{{.State.Running}}", container(stack.backend)]).stdout.strip() != "true":
        sh(["docker", "start", container(stack.backend)])
        events["kill_manual_start"] = True
    events["kill_healthy_s"] = wait_healthy(180)
    R.metric("events", {k: (round(v, 1) if isinstance(v, float) else v) for k, v in events.items()})
    say(f"재시작 후 UP: 정상 종료 {events['restart_healthy_s']}초, SIGKILL {events['kill_healthy_s']}초")
    codes = wait_all([up_proc, mix_proc], seconds + 11 * 60 + 60)
    stop.set()
    wt.join(timeout=30)
    deadline = time.time() + 30
    while time.time() < deadline and queue_depth() > 0:
        time.sleep(2)
    samples = sampler.stop()
    t1 = time.time()
    up_log.close()
    mix_log.close()

    up = uploads_summary(load_json(out / "uploads.json"))
    R.metric("uploads", up)
    R.metric("mix", run_summary(load_json(out / "run.json")))
    R.metric("samples", sample_summary(samples, skip=0.0))
    present = {r[0] for r in rows(f"SELECT content FROM feedbacks WHERE content LIKE 's4-{run_id}-%'")}
    missing = [m for m in acked if m not in present]
    R.metric("writes", {"acked": len(acked), "present_in_db": len(present), "acked_missing": len(missing), "http_statuses": {str(k): v for k, v in sorted(statuses.items())}})
    rows_ = poller_rows(t0)
    kept = [r for r in rows_ if r["event"] == "message_kept"]
    R.metric("redelivery", {"messages_kept_by_poller": len(kept), "max_receive_count": max([int(r.get("receiveCount", 1)) for r in kept] or [0]),
                            "queue_depth_end": queue_depth(), "lambda": lambda_log_events(t0)})
    R.metric("edge", edge_windows(edge_entries(t0, t1)))
    issued = int(psql("SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED'"))
    R.check("재시작·SIGKILL 뒤 모두 UP (180초 안)", events["restart_healthy_s"] is not None and events["kill_healthy_s"] is not None, json.dumps(R.metrics["events"]))
    R.check("업로드 모두 닫힘: SQS 재전달로 회복 (stuck 0, 반려 0, 큐 비어 있음. 혼합 부하의 PUT 없는 presign 은 ISSUED 로 남는 것이 정상이라 센 ISSUED 는 참고용)", up["stuck"] == 0 and up["rejected"] == 0 and queue_depth() == 0,
            json.dumps({**up, "issued_db": issued, "depth": queue_depth()}))
    R.check("받은 업로드는 전부 READY (started - errors == ready, 오류는 백엔드가 내려가 있던 동안의 요청뿐)", up["ready"] == up["started"] - up["errors"],
            f"started={up['started']} errors={up['errors']} ready={up['ready']}")
    R.check("확인된(2xx) 쓰기 손실 0", len(missing) == 0 and len(acked) > 0, json.dumps(R.metrics["writes"]))
    sm = R.metrics["samples"]
    R.check("끝난 뒤 앱 DB 연결 ≤ 10, idle in transaction 0", (sm["pg_app_max"] or 0) <= POOL_SIZE and (sm["pg_idle_in_tx_max"] or 0) == 0, json.dumps({k: sm[k] for k in ("pg_app_max", "pg_idle_in_tx_max")}))
    R.note(f"재전달 대기: poller 가 지우지 않고 둔 메시지 {len(kept)}건(visibility 360초 뒤 재전달). 업로드 오류 {up['errors']}건은 내려가 있는 동안의 presign/PUT 실패(앱의 재시도 대상).")


# =============================================================================================
# S5 DB 지연·끊김
# =============================================================================================
class Probe:
    """초당 N번 DB 를 쓰는 요청(오늘의 내 게시물)을 건다. 느린 응답이 다음 시도를 막지 않게 스레드 풀로."""

    def __init__(self, token, per_second=2, path="/api/v1/posts/today"):
        self.token, self.per_second, self.path = token, per_second, path
        self.records = []
        self.health = []
        self.stop_flag = threading.Event()
        self.pool = ThreadPoolExecutor(96)
        self.thread = threading.Thread(target=self._loop, daemon=True)

    def _one(self):
        started = time.time()
        status, body = raw("GET", f"http://{EDGE_HTTP}{self.path}", headers={"Authorization": f"Bearer {self.token}"}, timeout=75)
        self.records.append({"t": started, "status": status, "ms": (time.time() - started) * 1000, "body": body if status >= 400 or status == 0 else None})

    def _health(self):
        started = time.time()
        status, _ = raw("GET", f"http://{EDGE_HTTP}/actuator/health", timeout=20)
        self.health.append({"t": started, "status": status, "ms": (time.time() - started) * 1000})

    def _loop(self):
        n = 0
        while not self.stop_flag.is_set():
            self.pool.submit(self._one)
            if n % self.per_second == 0:
                self.pool.submit(self._health)
            n += 1
            self.stop_flag.wait(1.0 / self.per_second)

    def start(self):
        self.thread.start()
        return self

    def stop(self):
        self.stop_flag.set()
        self.thread.join(timeout=5)
        self.pool.shutdown(wait=True, cancel_futures=False)


def window(records, a, b):
    sel = [r for r in records if a <= r["t"] < b]
    times = [r["ms"] for r in sel]
    statuses = {}
    for r in sel:
        statuses[str(r["status"])] = statuses.get(str(r["status"]), 0) + 1
    return {"n": len(sel), "status": dict(sorted(statuses.items())), "p50_ms": round(percentile(times, 0.5), 1) if times else None,
            "p99_ms": round(percentile(times, 0.99), 1) if times else None, "max_ms": round(max(times), 1) if times else None}


def first_normal_after(records, t, window_ok=5, limit_ms=1000):
    """t 이후 연속 window_ok 개 요청이 모두 2xx 이고 limit_ms 안에 끝난 첫 시각까지 걸린 초(없으면 None)."""
    sel = sorted((r for r in records if r["t"] >= t), key=lambda r: r["t"])
    for i in range(len(sel) - window_ok + 1):
        chunk = sel[i:i + window_ok]
        if all(200 <= r["status"] < 300 and r["ms"] < limit_ms for r in chunk):
            return round(chunk[0]["t"] - t, 1)
    return None


@scenario
def s5(stack, R):
    rps, lat1_s, lat2_s = int(envf("S5_RPS", 40)), int(envf("S5_LATENCY_SECONDS", 60)), int(envf("S5_LATENCY2_SECONDS", 100))
    conns = int(envf("S5_CONNS", 300))
    out = run_dir(stack, "S5")
    reset_faults()
    baseline_s, recover_s, cut_s = 45, 60, 10
    total = baseline_s + lat1_s + 45 + lat2_s + recover_s + cut_s + recover_s + 15
    say(f"S5: {rps} req/s(연결 {conns}) 중 기준선 {baseline_s}초 → DB 지연 200ms {lat1_s}초 → 45초 → 지연 500ms {lat2_s}초(풀 대기 30초 한도를 넘기려고) → 회복 {recover_s}초 → DB 경로 {cut_s}초 끊김 → 회복 {recover_s}초")
    token = mint_token(seed_user_id(1), ttl=7200)
    t0 = time.time()
    started_at, restarts = stack.started_at(), stack.restart_count()
    sampler = Sampler(stack, period=2.0, pg_every=1, csv_path=out / "samples.csv").start()
    mix_proc, mix_log = loadgen_run(mix_args(stack, rps, f"{total}s", out / "run.json", f"{stack.backend}-s5", conns=conns, warmup="5s", interval="10"), out / "run.log")
    probe = Probe(token).start()
    marks = {}
    time.sleep(baseline_s)
    marks["lat1_on"] = time.time()
    toxic_latency(200, "postgres", stream="downstream")
    time.sleep(lat1_s)
    toxic_clear("postgres")
    marks["lat1_off"] = time.time()
    time.sleep(45)
    marks["lat2_on"] = time.time()
    toxic_latency(500, "postgres", stream="downstream")
    time.sleep(lat2_s)
    toxic_clear("postgres")
    marks["lat2_off"] = time.time()
    time.sleep(recover_s)
    marks["cut_on"] = time.time()
    proxy_enabled(False, "postgres")
    time.sleep(cut_s)
    proxy_enabled(True, "postgres")
    marks["cut_off"] = time.time()
    time.sleep(recover_s)
    marks["end"] = time.time()
    probe.stop()
    wait_all([mix_proc], 180)
    mix_log.close()
    time.sleep(3)
    samples = sampler.stop()
    t1 = time.time()
    toxic_clear("postgres")

    rec = probe.records
    phases = {"baseline": (t0 + 5, marks["lat1_on"]), "latency_200ms": (marks["lat1_on"], marks["lat1_off"]), "recovery1": (marks["lat1_off"], marks["lat2_on"]),
              "latency_500ms": (marks["lat2_on"], marks["lat2_off"]), "recovery2": (marks["lat2_off"], marks["cut_on"]), "cut_10s": (marks["cut_on"], marks["cut_off"]),
              "recovery3": (marks["cut_off"], marks["end"])}
    R.metric("probe_phases", {name: window(rec, a, b) for name, (a, b) in phases.items()})
    entries = edge_entries(t0, t1)
    R.metric("edge_phases", {name: edge_summary([x for x in entries if a <= x["ts"] < b], host=LOADGEN_HOSTS) for name, (a, b) in phases.items()})
    R.metric("time_to_normal_after_latency_s", first_normal_after(rec, marks["lat2_off"]))
    R.metric("time_to_normal_after_cut_s", first_normal_after(rec, marks["cut_off"]))
    # 풀 대기 타임아웃 응답: 연결 풀 30초 대기를 넘긴 5xx. 부하 발생기가 받은 응답은 edge 로그 상태 코드로, 본문은 probe 로 본다.
    def forms_of(selection):
        forms = {}
        for r in selection:
            key = json.dumps({"status": r["status"], "body": normalize_body(r["body"])}, ensure_ascii=False, sort_keys=True)
            forms[key] = forms.get(key, 0) + 1
        return forms

    slow_errors = [r for r in rec if r["status"] >= 400 and r["ms"] >= 25_000]
    other_errors = [r for r in rec if r["status"] >= 400 and r["ms"] < 25_000]
    edge_slow_5xx = [x for x in entries if x["host"] in LOADGEN_HOSTS and x["status"] >= 500 and x["ms"] >= 25_000]
    R.metric("pool_wait_timeout_responses", {"probe_count": len(slow_errors), "forms": forms_of(slow_errors), "edge_5xx_over_25s": len(edge_slow_5xx),
                                              "edge_5xx_over_25s_ms": [round(x["ms"]) for x in edge_slow_5xx[:5]]})
    R.metric("other_error_responses", {"count": len(other_errors), "forms": dict(sorted(forms_of(other_errors).items(), key=lambda kv: -kv[1])[:6])})
    # 부하 발생기의 오류 표본에서 본문 모양을 얻는다(풀 대기 타임아웃은 loadgen 쪽에서 더 많이 잡힌다).
    run_data = load_json(out / "run.json") or {}
    R.metric("loadgen_error_samples", run_data.get("error_samples", [])[:8])
    health_bad = [h for h in probe.health if h["status"] != 200]
    by_status = {}
    for h in probe.health:
        by_status[str(h["status"])] = by_status.get(str(h["status"]), 0) + 1
    R.metric("health", {"n": len(probe.health), "non200": len(health_bad), "by_status": dict(sorted(by_status.items())),
                        "status_0_means": "20초 안에 응답 없음(클라이언트가 포기)",
                        "non200_phases": sorted({n for n, (a, b) in phases.items() for h in health_bad if a <= h["t"] < b})})
    R.metric("mix", run_summary(run_data))
    R.metric("samples", sample_summary(samples, skip=0.0))
    R.metric("runtime_log", runtime_summary(stack.backend, t0))
    last_conns = [s["pg_app"] for s in samples[-6:] if s.get("pg_app") is not None]
    R.metric("final", {"pg_app_last": last_conns[-1] if last_conns else None, "pg_idle_in_tx_last": [s.get("pg_idle_in_tx") for s in samples[-3:]]})
    final_window = window(rec, marks["end"] - 30, marks["end"])
    baseline = R.metrics["probe_phases"]["baseline"]
    R.check("기준선: 오류 0", baseline["status"] == {"200": baseline["n"]}, json.dumps(baseline))
    for name in ("latency_200ms", "latency_500ms"):
        lat = R.metrics["probe_phases"][name]
        R.check(f"{name}: 서버가 멈추지 않음(요청마다 응답이 돌아옴, 연결 실패 0)", lat["n"] > 0 and "0" not in lat["status"], json.dumps(lat))
    R.check(f"지연 해소 뒤 {recover_s}초 안에 정상(연속 5건 2xx, 1초 미만)", R.metrics["time_to_normal_after_latency_s"] is not None and R.metrics["time_to_normal_after_latency_s"] < recover_s,
            f"{R.metrics['time_to_normal_after_latency_s']}초")
    R.check(f"끊김 해소 뒤 {recover_s}초 안에 정상", R.metrics["time_to_normal_after_cut_s"] is not None and R.metrics["time_to_normal_after_cut_s"] < recover_s,
            f"{R.metrics['time_to_normal_after_cut_s']}초")
    R.check("마지막 30초 오류 0, p99 < 1초", final_window["status"] == {"200": final_window["n"]} and (final_window["p99_ms"] or 0) < 1000, json.dumps(final_window))
    R.check("백엔드 프로세스 재시작 없음", stack.started_at() == started_at and stack.restart_count() == restarts)
    R.check("끝난 뒤 앱 DB 연결 ≤ 10, 5초 넘은 idle in transaction 0",
            (R.metrics["final"]["pg_app_last"] or 0) <= POOL_SIZE and (R.metrics["samples"]["pg_idle_in_tx_max"] or 0) == 0, json.dumps({**R.metrics["final"], "idle_in_tx_max": R.metrics["samples"]["pg_idle_in_tx_max"]}))
    timeouts = R.metrics["pool_wait_timeout_responses"]
    R.check("풀 대기 타임아웃 응답이 관찰됨(응답 형식 비교 대상이 있음)", timeouts["probe_count"] + timeouts["edge_5xx_over_25s"] > 0, json.dumps(timeouts))
    R.note(f"풀 대기 타임아웃 응답: probe {len(slow_errors)}건, edge 5xx(25초 이상) {len(edge_slow_5xx)}건. 응답 형식이 Java 와 같은지는 compare 가 판정한다.")


# =============================================================================================
# S6 S3 장애
# =============================================================================================
def xml_error(code, message):
    return f'<?xml version="1.0" encoding="UTF-8"?><Error><Code>{code}</Code><Message>{message}</Message><RequestId>l5</RequestId></Error>'


@scenario
def s6(stack, R):
    reset_faults()
    topic = todays_topic()
    t0 = time.time()
    # ---- A: 게시물 작성 때의 HeadObject. poller 를 멈춰 업로드가 ISSUED 로 남게 한다(Lambda 가 먼저 처리하면 S3 를 확인하지 않는다).
    sh(["docker", "pause", container("esm-poller")])
    cases = [
        ("head_200", []),
        ("head_500", [{"name": "head500", "method": "HEAD", "path": "/staging/dev/posts/", "status": 500, "body": xml_error("InternalError", "injected")}]),
        ("head_503_slowdown", [{"name": "head503", "method": "HEAD", "path": "/staging/dev/posts/", "status": 503, "body": xml_error("SlowDown", "injected")}]),
        ("head_403", [{"name": "head403", "method": "HEAD", "path": "/staging/dev/posts/", "status": 403, "body": ""}]),
        ("head_404", [{"name": "head404", "method": "HEAD", "path": "/staging/dev/posts/", "status": 404, "body": ""}]),
        ("head_delay_3s", [{"name": "delay3", "method": "HEAD", "path": "/staging/dev/posts/", "delay_ms": 3000}]),
        ("head_delay_12s", [{"name": "delay12", "method": "HEAD", "path": "/staging/dev/posts/", "delay_ms": 12000}]),
    ]
    head = []
    try:
        users = mint_users(len(cases), 6)
        for (name, rules), user in zip(cases, users):
            upload_id, url, ctype = new_upload(user[1])
            e.presigned_put(url, webp(), ctype)
            fault_rules(rules)
            started = time.time()
            status, data = http("POST", e.API + "/api/v1/posts", json.dumps({"topicId": topic, "photoUploadId": upload_id, "title": f"s6-{len(head)}"}).encode(),
                                {"Content-Type": "application/json", "Authorization": f"Bearer {user[1]}"})
            elapsed = time.time() - started
            body = json.loads(data) if data else None
            hits = fault_stats()
            fault_rules([])
            entry = {"case": name, "status": status, "seconds": round(elapsed, 1), "body": normalize_body(body), "fault_hits": sum(r["hits"] for r in hits["rules"])}
            head.append(entry)
            say(f"HeadObject {name}: HTTP {status} {elapsed:.1f}s {entry['body']} (주입 {entry['fault_hits']}회)")
    finally:
        fault_rules([])
        sh(["docker", "unpause", container("esm-poller")])
    R.metric("headobject", head)
    # poller 를 다시 돌려 A 단계에서 쌓인 업로드를 먼저 비운다(B 단계의 장애 규칙이 남의 업로드에 걸리지 않게).
    deadline = time.time() + 180
    while time.time() < deadline and (int(psql(f"SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED' AND created_at > to_timestamp({t0})")) > 0 or queue_depth() > 0):
        time.sleep(2)
    by = {h["case"]: h for h in head}
    R.check("HeadObject 정상: 게시물 작성 성공(201)", by["head_200"]["status"] in (200, 201), json.dumps(by["head_200"]))
    R.check("HeadObject 403/404 는 '사진을 찾을 수 없음'(404)으로 응답(원본 S3PostImageStorage: 403·404 → 없음)",
            by["head_403"]["status"] == 404 and by["head_404"]["status"] == 404, json.dumps([by["head_403"], by["head_404"]]))
    R.check("HeadObject 5xx 는 서버 오류(5xx)로 응답(원본: 그 외 S3Exception 은 다시 던짐)", by["head_500"]["status"] >= 500 and by["head_503_slowdown"]["status"] >= 500,
            json.dumps([by["head_500"], by["head_503_slowdown"]]))
    R.check("HeadObject 지연 3초·12초는 기다렸다 성공", by["head_delay_3s"]["status"] in (200, 201) and by["head_delay_12s"]["status"] in (200, 201),
            json.dumps([by["head_delay_3s"], by["head_delay_12s"]]))

    # ---- B: Lambda 쪽. visibility 를 10초로 줄여 재시도 상한(5회)까지 1분 안에 보게 한다.
    lam = []
    set_visibility(10)
    try:
        cases_b = [  # 규칙은 (업로드 ID) -> 규칙 목록. 그 업로드의 경로에만 걸리게 한다.
            # boto3 는 5xx 를 SDK 안에서 5번(최초 + 재시도 4) 시도한다. 12번 실패시키면 Lambda 호출이 두 번 실패하고 세 번째에 성공한다.
            ("lambda_get_500_x12", lambda uid: [{"name": "get500", "method": "GET", "path": f"/staging/dev/posts/{uid}", "status": 500, "body": xml_error("InternalError", "injected"), "count": 12}], "READY", 240),
            ("lambda_get_delay_8s", lambda uid: [{"name": "getdelay", "method": "GET", "path": f"/staging/dev/posts/{uid}", "delay_ms": 8000}], "READY", 120),
            ("lambda_result_put_403", lambda uid: [{"name": "put403", "method": "PUT", "path": f"/posts/dev/(original|thumbnail)/{uid}", "status": 403, "body": xml_error("AccessDenied", "injected")}], "REJECTED", 300),
        ]
        for name, make_rules, expect, limit in cases_b:
            user = mint_users(1, 7)[0]
            since = time.time()
            upload_id, url, ctype = new_upload(user[1])
            fault_rules(make_rules(upload_id))
            e.presigned_put(url, webp(), ctype)
            state, took = wait_upload_closed(upload_id, limit)
            stats = fault_stats()
            fault_rules([])
            rows_ = poller_rows(since)
            kept = [r for r in rows_ if r["event"] == "message_kept"]
            lam_events = lambda_log_events(since)
            row = upload_row(upload_id)
            entry = {"case": name, "state": state, "seconds": round(took, 1), "reason": row["reason"] if row else None, "fault_hits": sum(r["hits"] for r in stats["rules"]),
                     "max_receive_count_kept": max([int(r.get("receiveCount", 1)) for r in kept] or [0]), "messages_kept": len(kept), "lambda": lam_events}
            lam.append(entry)
            say(f"Lambda {name}: {state} {took:.0f}초 {entry}")
            # 마지막 메시지가 정리될 때까지
            deadline = time.time() + 40
            while time.time() < deadline and queue_depth() > 0:
                time.sleep(2)
            R.check(f"Lambda {name}: 최종 {expect}", state == expect, json.dumps(entry))
        by_l = {c["case"]: c for c in lam}
        put403 = by_l["lambda_result_put_403"]
        R.check("결과 경로 403: 재시도 상한(수신 5회: 실패 5번, 앞 4번은 메시지를 남김) 후 포기하고 실패 콜백(PROCESSING_ERROR)",
                put403["state"] == "REJECTED" and put403["reason"] == "PROCESSING_ERROR" and put403["lambda"]["image_processing_failed"] == 5
                and put403["messages_kept"] == 4 and put403["lambda"]["image_processing_abandoned"] == 1, json.dumps(put403))
        get500 = by_l["lambda_get_500_x12"]
        R.check("S3 GET 5xx: boto3 재시도 뒤에도 실패하면 SQS 재전달로 이어 가고 결국 READY(Lambda 호출 실패 2번)", get500["state"] == "READY" and get500["lambda"]["image_processing_failed"] == 2
                and get500["lambda"]["image_processing_abandoned"] == 0, json.dumps(get500))
        # 반려된 업로드로 게시물을 만들면 사용자에게 이유를 알려 준다
        # 복구: 규칙 없이 새 업로드는 정상
        user = mint_users(1, 7)[0]
        upload_id, url, ctype = new_upload(user[1])
        e.presigned_put(url, webp(), ctype)
        state, took = wait_upload_closed(upload_id, 90)
        R.check("장애 해소 뒤 새 업로드는 정상 처리(READY)", state == "READY", f"{state} {took:.1f}s")
    finally:
        fault_rules([])
        set_visibility(360, settle=False)
        sh(["docker", "unpause", container("esm-poller")], check=False)
    R.metric("lambda_cases", lam)
    # 멈춘 동안 쌓인 업로드(A 에서 만든 것)가 poller 재개 뒤 처리되어 닫혔다.
    left = int(psql(f"SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED' AND created_at > to_timestamp({t0})"))
    R.check("poller 재개 뒤 A 단계에서 쌓인 업로드도 모두 닫힘(ISSUED 0)", left == 0, f"남은 ISSUED {left}")
    R.check("큐 비어 있음", queue_depth() == 0, queue_counts())
    R.note("Lambda 재시도 간격은 시험을 위해 visibility 360초 → 10초로 줄였다(상한 5회는 그대로). 끝나면 360초로 되돌린다.")


# =============================================================================================
# S8 느린 클라이언트
# =============================================================================================
def open_conn(host, port):
    s = socket.create_connection((host, port), timeout=10)
    s.settimeout(None)
    return s


class Attackers:
    """느린 클라이언트들. 종류별로 서버가 연결을 끊거나 응답한 시각을 기록한다."""

    def __init__(self):
        self.socks = []
        self.results = {}
        self.threads = []
        self.stop = threading.Event()

    def idle(self, kind, host, port, n):
        opened = 0
        for _ in range(n):
            try:
                self.socks.append(open_conn(host, port))
                opened += 1
            except OSError:
                break
        self.results[kind] = {"requested": n, "opened": opened}

    def drip(self, kind, host, port, n, payload_fn, interval):
        """payload_fn(i) 의 바이트를 interval 초에 한 바이트씩 보낸다. 서버가 닫거나 응답하면 기록한다."""
        out = {"requested": n, "opened": 0, "closed_by_server": 0, "responded": {}, "close_seconds": []}
        self.results[kind] = out

        def one(i):
            try:
                s = open_conn(host, port)
            except OSError:
                return
            out["opened"] += 1
            self.socks.append(s)
            data = payload_fn(i)
            started = time.time()
            s.settimeout(0.05)
            sent = 0
            try:
                while not self.stop.is_set() and sent < len(data):
                    s.sendall(data[sent:sent + 1])
                    sent += 1
                    deadline = time.time() + interval
                    while time.time() < deadline and not self.stop.is_set():
                        try:
                            chunk = s.recv(4096)
                        except socket.timeout:
                            continue
                        if chunk:
                            code = chunk.split(b" ", 2)[1].decode(errors="replace") if chunk.startswith(b"HTTP/") else "data"
                            out["responded"][code] = out["responded"].get(code, 0) + 1
                        out["closed_by_server"] += 1
                        out["close_seconds"].append(round(time.time() - started, 1))
                        return
            except OSError:
                out["closed_by_server"] += 1
                out["close_seconds"].append(round(time.time() - started, 1))

        for i in range(n):
            t = threading.Thread(target=one, args=(i,), daemon=True)
            t.start()
            self.threads.append(t)

    def close(self):
        self.stop.set()
        for t in self.threads:
            t.join(timeout=5)
        for s in self.socks:
            try:
                s.close()
            except OSError:
                pass


@scenario
def s8(stack, R):
    idle_n, seconds = int(envf("S8_IDLE", 800)), int(envf("S8_SECONDS", 60))
    out = run_dir(stack, "S8")
    reset_faults()
    host, port = BACKEND_DEBUG.split(":")
    ehost, eport = EDGE_HTTP.split(":")
    token = mint_token(seed_user_id(1), ttl=7200)
    user = mint_users(1, 8)[0]
    topic = todays_topic()
    say(f"S8: 기준선 30초 → 느린 클라이언트(유휴 {idle_n}+200, 느린 헤더 100, 느린 본문 60, 느린 업로드 10) {seconds}초 → 정리 후 확인")
    sampler = Sampler(stack, period=2.0, pg_every=2, csv_path=out / "samples.csv").start()
    probe = Probe(token, per_second=5).start()
    time.sleep(30)
    base_proc = stack.pid()
    base = read_proc(base_proc)
    t_attack = time.time()
    atk = Attackers()
    atk.idle("idle_backend", host, int(port), idle_n)
    atk.idle("idle_edge_tls_no_hello", "127.0.0.1", 443, 200)
    request_line = b"GET /api/v1/posts/today HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer " + token.encode() + b"\r\nX-Slow: " + b"a" * 400 + b"\r\n\r\n"
    atk.drip("slow_header_backend", host, int(port), 100, lambda i: request_line, 3.0)
    body = b'{"content":"' + b"s" * 190 + b'"}'
    head = (b"POST /api/v1/feedbacks HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nAuthorization: Bearer " + token.encode()
            + b"\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n")
    atk.drip("slow_body_backend", host, int(port), 60, lambda i: head + body, 2.0)
    # 느린 업로드(앱이 모바일 회선에서 올리는 상황): 서명된 S3 PUT 을 edge TLS 로 조금씩 보낸다. 끝나면 Lambda 처리까지 이어진다.
    slow_uploads = []

    def slow_put(i):
        try:
            upload_id, url, ctype = new_upload(user[1])
            from urllib.parse import parse_qs, urlparse
            parsed = urlparse(url)
            ctx = ssl.create_default_context(cafile=str(e.CA_FILE))
            raw_sock = socket.create_connection((ehost, 443), timeout=10)
            tls = ctx.wrap_socket(raw_sock, server_hostname=parsed.hostname)
            signed = parse_qs(parsed.query).get("X-Amz-SignedHeaders", ["host"])[0].split(";")
            values = {"content-type": ctype, "cache-control": "public, max-age=86400", "if-none-match": "*"}
            headers = "".join(f"{n}: {values[n]}\r\n" for n in signed if n in values)
            data = webp()
            tls.sendall((f"PUT {parsed.path}?{parsed.query} HTTP/1.1\r\nHost: {parsed.hostname}\r\nContent-Length: {len(data)}\r\n{headers}\r\n").encode())
            started = time.time()
            chunk = max(1, len(data) // (seconds * 2))
            sent = 0
            while sent < len(data) and not atk.stop.is_set():
                tls.sendall(data[sent:sent + chunk])
                sent += chunk
                time.sleep(0.5)
            tls.settimeout(30)
            response = tls.recv(4096)
            tls.close()
            slow_uploads.append({"upload_id": upload_id, "put_status": response.split(b" ")[1].decode() if response.startswith(b"HTTP/") else "?",
                                 "seconds": round(time.time() - started, 1)})
        except Exception as error:  # noqa: BLE001
            slow_uploads.append({"error": f"{type(error).__name__}: {error}"[:200]})

    up_threads = [threading.Thread(target=slow_put, args=(i,), daemon=True) for i in range(10)]
    for t in up_threads:
        t.start()
    time.sleep(5)
    during = read_proc(stack.pid())
    # 공격 중에도 앱 한 대의 정상 흐름이 끝난다
    canary = canary_post(topic, user, timeout=120)
    time.sleep(max(0, seconds - (time.time() - t_attack)))
    mid = read_proc(stack.pid())
    t_end = time.time()
    # 서버가 제한 시간으로 끊는 연결을 마저 기다린 뒤 정리
    time.sleep(max(0, 75 - (time.time() - t_attack)))
    atk.close()
    for t in up_threads:
        t.join(timeout=60)
    time.sleep(30)
    after = read_proc(stack.pid())
    probe.stop()
    samples = sampler.stop()

    phases = {"baseline": (t_attack - 30, t_attack), "attack": (t_attack, t_end), "after": (t_end, time.time())}
    R.metric("probe_phases", {name: window(probe.records, a, b) for name, (a, b) in phases.items()})
    R.metric("attackers", {k: ({**v, "close_seconds": (percentile(v["close_seconds"], 0.5), max(v["close_seconds"], default=None))} if "close_seconds" in v else v)
                            for k, v in atk.results.items()})
    R.metric("slow_uploads", slow_uploads)
    R.metric("canary_during_attack", canary)
    R.metric("proc", {"baseline": base, "during": during, "mid": mid, "after": after, "unit": "(rss_kb, threads, fds)"})
    R.metric("samples", sample_summary(samples, skip=0.0))
    b, a = R.metrics["probe_phases"]["baseline"], R.metrics["probe_phases"]["attack"]
    R.check("공격 중에도 정상 요청 오류 0 (서버가 멈추지 않음)", a["n"] > 0 and a["status"] == {"200": a["n"]}, json.dumps(a))
    limit = max((b["p99_ms"] or 0) * 3, (b["p99_ms"] or 0) + 100)
    R.check("공격 중 p99 ≤ max(기준선 p99 × 3, +100ms)", (a["p99_ms"] or 0) <= limit, f"기준선 {b['p99_ms']}ms, 공격 {a['p99_ms']}ms, 한도 {limit:.0f}ms")
    R.check("공격 중 앱 한 대의 게시물 작성 흐름이 끝남(PENDING)", canary["ok"], json.dumps(canary))
    ok_uploads = [u for u in slow_uploads if u.get("put_status") == "200"]
    R.check("느린 S3 업로드 10건 모두 200", len(ok_uploads) == 10, json.dumps(slow_uploads)[:400])
    time.sleep(10)
    states = [upload_status(u["upload_id"])[0] for u in ok_uploads]
    R.check("느린 업로드도 Lambda 처리 뒤 READY", all(s == "READY" for s in states) and len(states) == len(ok_uploads), states)
    if base and after:
        R.check("정리 뒤 FD 가 시작 수준으로(+16 이내)", after[2] <= base[2] + 16, f"{base[2]} -> {after[2]}")
        thread_slack = 8 if stack.backend == "rust" else 210
        R.check(f"정리 뒤 스레드가 시작 수준으로(+{thread_slack} 이내)", after[1] <= base[1] + thread_slack, f"{base[1]} -> {after[1]}")
    sm = R.metrics["samples"]
    R.check("앱 DB 연결 ≤ 10, idle in transaction 0", (sm["pg_app_max"] or 0) <= POOL_SIZE and (sm["pg_idle_in_tx_max"] or 0) == 0, json.dumps({k: sm[k] for k in ("pg_app_max", "pg_idle_in_tx_max")}))

    # 하네스의 stress 케이스(느린 헤더 400개, 유휴 연결 2000개)를 같은 스택에서 한 번 더
    cid = sh(["docker", "inspect", "-f", "{{.Id}}", container(stack.backend)]).stdout.strip()
    pgid = sh(["docker", "inspect", "-f", "{{.Id}}", container("postgres")]).stdout.strip()
    stress = {}
    for case in ("slow", "idle:2000"):
        out_json = out / f"stress-{case.replace(':', '-')}.json"
        res = sh([str(LOADGEN), "stress", "--case", case, "--base", BACKEND_DEBUG, "--secret", access_secret_hex(), "--db", DB_KV, "--label", stack.backend,
                  "--container", cid, "--db-container", pgid, "--out", str(out_json), *SEED_ARGS], timeout=900)
        data = load_json(out_json) or {}
        stress[case] = {"exit": res.returncode, "result": data}
        (out / f"stress-{case.replace(':', '-')}.log").write_text(res.stdout + res.stderr)
        R.check(f"loadgen stress {case} 통과(누수 검사 포함)", res.returncode == 0, (res.stdout.strip().splitlines() or [""])[-1][:300])
    R.metric("stress_exit_codes", {k: v["exit"] for k, v in stress.items()})
    R.metric("out_dir", str(out))


# =============================================================================================
# 진입점 · 비교
# =============================================================================================
def latest(backend, scenario_name):
    files = sorted(STATE.glob(f"{backend}-{scenario_name}-*.json"))
    return json.loads(files[-1].read_text()) if files else None


def compare(names):
    """가장 최근 rust·java 결과를 나란히 놓는다. A/B 기준(S1 p99 ≤ Java, S3 단계별 일치, S5 풀 대기 타임아웃 응답 형식, S6 응답)을 판정한다."""
    verdicts = []
    for name in names:
        r, j = latest("rust", name), latest("java", name)
        if not r or not j:
            print(f"== {name}: 결과 부족 (rust={'있음' if r else '없음'}, java={'있음' if j else '없음'})")
            continue
        print(f"== {name}")
        flat = lambda d, prefix="": {k2: v2 for k, v in d.items() for k2, v2 in (flat(v, f"{prefix}{k}.") .items() if isinstance(v, dict) else [(f"{prefix}{k}", v)])}  # noqa: E731
        fr, fj = flat(r["metrics"]), flat(j["metrics"])
        for key in sorted(set(fr) | set(fj)):
            if isinstance(fr.get(key, fj.get(key)), (int, float)) and not isinstance(fr.get(key), bool):
                print(f"  {key:60s} rust={fr.get(key)!s:>12}  java={fj.get(key)!s:>12}")
        if name == "S1":
            rp, jp = r["metrics"]["mix"].get("p99_ms"), j["metrics"]["mix"].get("p99_ms")
            ep, ejp = r["metrics"]["edge"]["edge_http"]["p99_ms"], j["metrics"]["edge"]["edge_http"]["p99_ms"]
            verdicts.append(("S1 p99(loadgen) Rust ≤ Java", rp is not None and jp is not None and rp <= jp, f"rust {rp}ms / java {jp}ms"))
            verdicts.append(("S1 p99(edge 접근 로그) Rust ≤ Java", ep is not None and ejp is not None and ep <= ejp, f"rust {ep}ms / java {ejp}ms"))
        if name == "S3":
            sr, sj = {s["step"]: s for s in r["metrics"]["steps"]}, {s["step"]: s for s in j["metrics"]["steps"]}
            diffs = [(k, sr[k], sj.get(k)) for k in sr if json.dumps(sr[k], sort_keys=True) != json.dumps(sj.get(k), sort_keys=True)]
            for k, a, b in diffs:
                print(f"  차이 {k}\n    rust={json.dumps(a, ensure_ascii=False)}\n    java={json.dumps(b, ensure_ascii=False)}")
            verdicts.append(("S3 단계별 응답·DB 상태가 Java 와 같음", not diffs, f"차이 {len(diffs)}건"))
        if name == "S5":
            fr_, fj_ = r["metrics"]["pool_wait_timeout_responses"], j["metrics"]["pool_wait_timeout_responses"]
            print(f"  풀 대기 타임아웃 응답 rust={fr_}\n  풀 대기 타임아웃 응답 java={fj_}")
            er = set(re.findall(r"-> (\d{3}) (\{[^}]*\})", " ".join(r["metrics"]["loadgen_error_samples"])))
            ej = set(re.findall(r"-> (\d{3}) (\{[^}]*\})", " ".join(j["metrics"]["loadgen_error_samples"])))
            print(f"  loadgen 오류 본문 rust={sorted(er)}\n  loadgen 오류 본문 java={sorted(ej)}")
            verdicts.append(("S5 풀 대기 타임아웃 오류 응답(상태·본문)이 Java 와 같음", bool(er) and er == ej, f"rust {sorted(er)} / java {sorted(ej)}"))
        if name == "S6":
            hr, hj = {h["case"]: h for h in r["metrics"]["headobject"]}, {h["case"]: h for h in j["metrics"]["headobject"]}
            diffs = [(k, hr[k], hj.get(k)) for k in hr if (hr[k]["status"], hr[k]["body"]) != ((hj.get(k) or {}).get("status"), (hj.get(k) or {}).get("body"))]
            for k, a, b in diffs:
                print(f"  HeadObject 차이 {k}\n    rust={a}\n    java={b}")
            verdicts.append(("S6 HeadObject 실패 응답(상태·본문)이 Java 와 같음", not diffs, f"차이 {len(diffs)}건"))
            lr, lj = {c["case"]: c for c in r["metrics"]["lambda_cases"]}, {c["case"]: c for c in j["metrics"]["lambda_cases"]}
            diffs = [(k, lr[k]["state"], (lj.get(k) or {}).get("state")) for k in lr if lr[k]["state"] != (lj.get(k) or {}).get("state")]
            verdicts.append(("S6 Lambda 장애 케이스 최종 상태가 Java 와 같음", not diffs, f"차이 {diffs}"))
        for label, which in (("Rust", r), ("Java", j)):
            bad = [c for c in which["checks"] if not c["ok"]]
            print(f"  {label}: 통과 {len(which['checks']) - len(bad)} / 실패 {len(bad)}")
            for c in bad:
                print(f"    실패: {c['name']} - {c['detail'][:200]}")
    print("== A/B 판정")
    for label, ok, detail in verdicts:
        print(f"  {'통과' if ok else '실패'}: {label} - {detail}")
    return 0 if all(ok for _, ok, _ in verdicts) else 1


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "compare":
        names = [a.upper() for a in sys.argv[2:]] or ["S1", "S3", "S5", "S6"]
        sys.exit(compare(names))
    if len(sys.argv) < 3 or sys.argv[1] not in ("rust", "java"):
        print(__doc__)
        sys.exit(2)
    backend = sys.argv[1]
    flags = [a for a in sys.argv[2:] if a.startswith("--")]
    names = [a.upper() for a in sys.argv[2:] if not a.startswith("--")]
    stack = Stack(backend)
    if "--fresh" in flags:
        say(f"스택을 새로 올린다(백엔드 {backend}, ESM_CONCURRENCY={stack.env['ESM_CONCURRENCY']}): down -v → up → 시드")
        stack.up(fresh=True)
        seed(stack)
    if not e.CA_FILE.exists():
        raise Failure(f"로컬 CA 루트가 없다: {e.CA_FILE} (스택이 떠 있어야 한다)")
    exit_code = 0
    for name in names:
        if name not in SCENARIOS:
            raise Failure(f"알 수 없는 시나리오: {name} (가능: {', '.join(SCENARIOS)})")
        R = Results(backend)
        started = time.monotonic()
        say(f"===== {name} ({backend}) 시작")
        try:
            SCENARIOS[name](stack, R)
        except Exception as error:  # noqa: BLE001 - 한 시나리오의 오류가 나머지를 막지 않게
            R.check(f"{name} 실행 중 예외 없음", False, f"{type(error).__name__}: {error}")
            import traceback
            traceback.print_exc()
        finally:
            reset_faults()
        R.metric("duration_s", round(time.monotonic() - started, 1))
        path = save_result(R, name)
        failed = R.failed()
        say(f"===== {name} ({backend}) 통과 {len(R.checks) - len(failed)} / 실패 {len(failed)}  결과: {path}")
        for c in failed:
            say(f"  실패: {c['name']} - {c['detail'][:300]}")
        exit_code = exit_code or (1 if failed else 0)
    sys.exit(exit_code)


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"[l5] 실패: {failure}", file=sys.stderr)
        sys.exit(1)
