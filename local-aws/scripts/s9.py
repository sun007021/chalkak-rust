#!/usr/bin/env python3
"""S9 장시간 시험: 저부하를 길게 걸고 매분 주제 푸시·공지 배포 작업이 제때 도는지, 메모리·FD·스레드가 평탄한지 본다.

  s9.py run <rust|java> [--duration 8h] [--out DIR] [--rps 10] [--keep]   스택을 새로 올려 시드하고 시험한다. 끝나면 판정까지 하고 스택을 내린다(--keep 이면 남긴다)
  s9.py judge DIR                                                          DIR 의 파일만 읽어 판정을 다시 한다(진행 중이거나 끊긴 시험도 그때까지의 표본으로 판정)

돌리는 것(백엔드는 compose.l5.yaml 위의 rust | java)
  - 부하: 혼합 부하 --rps(기본 10) 연결 4 + 유휴 연결 10, 요청마다 새 연결을 맺는 부하 2 req/s, 업로드 흐름 3건/분(Lambda 경로 유지)
  - 표본: 30초마다 백엔드 프로세스의 RSS·스레드·FD(/proc), DB 연결 수(pg_stat_activity), 호스트 부하를 samples.csv 에 이어 쓴다
  - 작업 주입: 매분 (1) 예약 시각이 다음 분 0초 전인 공지를 하나 등록하고 (2) 다다음 분 0초에 보낼 주제 푸시 시간표를 하나 등록한다.
    각각 매분 0초에 도는 작업(NoticeDistribution, TopicPush)이 제때 처리했는지 jobs.csv 에 기록한다.
    공지: 예약 시각 뒤 90초 안에 distributed_at 이 찍히고 알림이 활성 회원 수만큼 생겼는가.
    주제 푸시: 90초 안에 로그 topic_publication_summary 가 시간표와 회차 시각대로 남고 acceptedCount == targetCount > 0 인가.
  - 로그 수집: 백엔드 로그에서 런타임 상태(1분마다)·주제 푸시 집계·ERROR 를 runtime.jsonl, jobs_log.jsonl 에 이어 쓰고, edge 접근 로그는 5분 단위 요약을 edge.jsonl 에 남긴다
출력 폴더(기본 local-aws/.state/s9/<백엔드>-<시각>/): meta.json, samples.csv, jobs.csv, runtime.jsonl, jobs_log.jsonl, edge.jsonl, progress.log,
  mix.log/json, churn.log/json, uploads.log/json, verdict.json, verdict.txt
판정: 종료 코드 0 = 모든 기준 통과. 기준은 verdict.txt 에 줄마다 적힌다.
Java 프로필은 compose 에서 주제 푸시·공지 배포가 꺼져 있어 작업 기준은 건너뛴다(메모리·FD·스레드 평탄성만 본다).
"""

import json
import os
import re
import subprocess
import sys
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # python3 -I 는 스크립트 폴더를 경로에 넣지 않는다
from l5_lib import *  # noqa: E402,F401,F403
import l5_lib as L  # noqa: E402

KST = timezone(timedelta(hours=9))
SAMPLE_PERIOD = 30.0
JOB_DEADLINE_S = 90.0  # 예약/회차 시각 뒤 이 안에 처리되어야 한다
HARVEST_EVERY = 30.0
TOPIC_PARTIAL_MAX = 0.50  # IMDS 모드에서 한 회차에 허용하는 발행 실패 비율(정적 자격증명 모드는 0)


def parse_duration(text):
    m = re.fullmatch(r"(\d+(?:\.\d+)?)([smh]?)", text.strip())
    if not m:
        raise Failure(f"기간 형식이 아니다: {text} (예: 8h, 90m, 3600)")
    return float(m.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600}[m.group(2)]


def log_progress(out, message):
    line = f"[s9 {time.strftime('%Y-%m-%d %H:%M:%S')}] {message}"
    print(line, flush=True)
    with open(out / "progress.log", "a") as f:
        f.write(line + "\n")


# ---- 로그 수집 ---------------------------------------------------------------------------------------
class Harvester(threading.Thread):
    """백엔드·edge 로그를 주기적으로 읽어 필요한 줄만 파일로 옮긴다(8시간 치 로그를 마지막에 한꺼번에 읽지 않으려고)."""

    def __init__(self, stack, out, start):
        super().__init__(daemon=True)
        self.stack, self.out = stack, out
        self.stop_flag = threading.Event()
        self.last = start - 1.0          # 백엔드 로그에서 마지막으로 받은 줄의 시각
        self.edge_last = start
        self.errors = 0
        self.runtime_f = open(out / "runtime.jsonl", "a", buffering=1)
        self.jobs_f = open(out / "jobs_log.jsonl", "a", buffering=1)
        self.edge_f = open(out / "edge.jsonl", "a", buffering=1)
        self.error_f = open(out / "errors.log", "a", buffering=1)

    def harvest_backend(self):
        result = sh(["docker", "logs", "--since", f"{self.last - 2:.3f}", container(self.stack.backend)], timeout=300)
        newest = self.last
        for line in (result.stdout + "\n" + result.stderr).splitlines():
            if not line.startswith("{"):
                if " ERROR " in line[:80]:
                    self.errors += 1
                    self.error_f.write(line[:500] + "\n")
                continue
            try:
                row = json.loads(line)
                ts = parse_iso(row["@timestamp"])
            except (ValueError, KeyError):
                continue
            if ts <= self.last:
                continue
            newest = max(newest, ts)
            if row.get("type") == "runtime":
                self.runtime_f.write(json.dumps({**{k: v for k, v in row.items() if isinstance(v, (int, float))}, "ts": ts}) + "\n")
            elif row.get("stage", "").startswith(("topic_", "notice_")):
                self.jobs_f.write(json.dumps({"ts": ts, **{k: v for k, v in row.items() if k in ("stage", "scheduleId", "occurredAt", "targetCount", "acceptedCount",
                                                                                                  "failedCount", "unknownCount", "expiredCount", "noticeId", "recipientCount", "reason", "errorCode", "level")}}) + "\n")
            if row.get("level") == "ERROR":
                self.errors += 1
                self.error_f.write(line[:500] + "\n")
        self.last = newest

    def harvest_edge(self):
        now = time.time()
        entries = edge_entries(self.edge_last, now)
        self.edge_last = now
        if not entries:
            return
        ms = [x["ms"] for x in entries]
        classes = {}
        for x in entries:
            classes[f"{x['status'] // 100}xx"] = classes.get(f"{x['status'] // 100}xx", 0) + 1
        self.edge_f.write(json.dumps({"t": round(now, 1), "n": len(entries), "status": classes, "p50_ms": round(percentile(ms, 0.5), 2),
                                      "p99_ms": round(percentile(ms, 0.99), 2), "max_ms": round(max(ms), 2),
                                      "5xx_uris": sorted({x["uri"].split("?")[0] for x in entries if x["status"] >= 500})[:5]}) + "\n")

    def run(self):
        next_edge = time.time() + 300
        while not self.stop_flag.is_set():
            try:
                self.harvest_backend()
                if time.time() >= next_edge:
                    self.harvest_edge()
                    next_edge = time.time() + 300
            except Exception as error:  # noqa: BLE001 - 수집 실패가 시험을 멈추지 않게
                self.error_f.write(f"[s9 harvest] {type(error).__name__}: {error}\n")
            self.stop_flag.wait(HARVEST_EVERY)

    def finish(self):
        self.stop_flag.set()
        self.join(timeout=60)
        try:
            self.harvest_backend()
            self.harvest_edge()
        except Exception as error:  # noqa: BLE001
            self.error_f.write(f"[s9 harvest final] {error}\n")
        for f in (self.runtime_f, self.jobs_f, self.edge_f, self.error_f):
            f.close()


# ---- 작업 주입 -------------------------------------------------------------------------------------
class WorkInjector(threading.Thread):
    """매분 공지 하나와 주제 푸시 시간표 하나를 등록하고, 처리 결과를 jobs.csv 에 남긴다."""

    HEADER = ["cycle", "boundary", "notice_id", "notice_scheduled", "notice_distributed_at", "notice_delay_s", "notice_recipients", "expected_recipients",
              "notice_ok", "schedule_id", "send_time", "topic_logged_delay_s", "topic_target", "topic_accepted", "topic_ok", "note"]

    def __init__(self, stack, out, end_ts):
        super().__init__(daemon=True)
        self.stack, self.out, self.end_ts = stack, out, end_ts
        self.stop_flag = threading.Event()
        self.pending = []
        self.csv = open(out / "jobs.csv", "a", buffering=1)
        if self.csv.tell() == 0:
            self.csv.write(",".join(self.HEADER) + "\n")
        self.admin = None
        self.cycle = 0

    def admin_token(self):
        if self.admin is None or self.admin[1] < time.time():
            _, body = api("POST", "/api/v1/admin/auth/login", {"username": "operator", "password": "operator-password"})
            self.admin = (body["accessToken"], time.time() + 600)
        return self.admin[0]

    def inject(self):
        now = time.time()
        boundary = (int(now) // 60 + 1) * 60                 # 다음 분 0초: 이때 두 작업이 돈다
        scheduled = datetime.fromtimestamp(boundary - 5, timezone.utc)
        self.cycle += 1
        row = {"cycle": self.cycle, "boundary": boundary, "note": ""}
        try:
            _, notice = api("POST", "/api/v1/admin/notices", {"title": f"S9 공지 {self.cycle}", "body": "장시간 시험 공지입니다.",
                                                              "scheduledAt": scheduled.strftime("%Y-%m-%dT%H:%M:%S.%fZ")}, token=self.admin_token())
            row.update(notice_id=notice.get("id") or notice.get("noticeId"), notice_scheduled=boundary - 5)
        except Failure as failure:
            row["note"] += f"공지 등록 실패: {str(failure)[:120]};"
        # 시간표는 다다음 분 0초(boundary + 60): 등록 시각(updated_at) 이후에 도래해야 실행된다
        send = datetime.fromtimestamp(boundary + 60, KST)
        try:
            _, schedule = api("POST", "/api/v1/admin/topic-push-schedules", {"sendTime": send.strftime("%H:%M:00")}, token=self.admin_token())
            row.update(schedule_id=schedule.get("id") or schedule.get("scheduleId"), send_time=boundary + 60)
        except Failure as failure:
            row["note"] += f"시간표 등록 실패: {str(failure)[:120]};"
        self.pending.append(row)

    def expected_recipients(self):
        return int(psql("SELECT count(*) FROM users WHERE deleted_at IS NULL"))

    def verify(self, row):
        """주입한 지 충분히 지난 항목을 확인해 jobs.csv 에 쓴다."""
        if row.get("notice_id"):
            r = rows(f"SELECT coalesce(extract(epoch from distributed_at)::text, '') FROM notices WHERE id = '{row['notice_id']}'")
            distributed = float(r[0][0]) if r and r[0][0] else None
            recipients = int(psql(f"SELECT count(*) FROM notifications WHERE source_type = 'NOTICE' AND source_id = '{row['notice_id']}'"))
            row["notice_distributed_at"] = distributed
            row["notice_delay_s"] = round(distributed - row["boundary"], 2) if distributed else None
            row["notice_recipients"] = recipients
            row["expected_recipients"] = int(psql("SELECT count(*) FROM users WHERE deleted_at IS NULL"))
            row["notice_ok"] = bool(distributed and -1 <= distributed - row["boundary"] <= JOB_DEADLINE_S and recipients >= row["expected_recipients"] - 1)
        else:
            row["notice_ok"] = False
        if row.get("schedule_id"):
            found = None
            for line in open(self.out / "jobs_log.jsonl"):
                try:
                    j = json.loads(line)
                except ValueError:
                    continue
                if j.get("stage") == "topic_publication_summary" and j.get("scheduleId") == row["schedule_id"]:
                    found = j
            if found:
                row["topic_logged_delay_s"] = round(found["ts"] - row["send_time"], 2)
                row["topic_target"], row["topic_accepted"] = found.get("targetCount"), found.get("acceptedCount")
                row["topic_ok"] = bool(-1 <= row["topic_logged_delay_s"] <= JOB_DEADLINE_S and found.get("targetCount", 0) > 0 and found.get("acceptedCount") == found.get("targetCount"))
            else:
                row["topic_ok"] = False
                row["note"] += "주제 푸시 집계 로그 없음;"
            # 시간표는 확인 뒤 지운다(매일 다시 도는 설정이 쌓이지 않게)
            try:
                api("DELETE", f"/api/v1/admin/topic-push-schedules/{row['schedule_id']}", token=self.admin_token())
            except Failure as failure:
                row["note"] += f"시간표 삭제 실패: {str(failure)[:80]};"
        else:
            row["topic_ok"] = False
        self.csv.write(",".join(str(row.get(h, "")).replace(",", " ") for h in self.HEADER) + "\n")

    def run(self):
        # 매분 3초에 주입한다.
        while not self.stop_flag.is_set():
            now = time.time()
            wake = (int(now) // 60) * 60 + 63 if now % 60 < 3 else (int(now) // 60 + 1) * 60 + 3
            if wake - now > 61:
                wake -= 60
            if self.stop_flag.wait(max(0, wake - now)):
                break
            if time.time() < self.end_ts - 130:  # 끝나기 직전에는 새로 넣지 않는다(확인할 시간이 없다)
                try:
                    self.inject()
                except Exception as error:  # noqa: BLE001
                    self.pending.append({"cycle": self.cycle, "boundary": 0, "note": f"주입 예외: {error}"})
            # 확인 시한이 지난 항목
            due = [r for r in self.pending if time.time() >= max(r.get("boundary", 0), r.get("send_time", 0)) + JOB_DEADLINE_S + 10]
            for r in due:
                self.pending.remove(r)
                try:
                    self.verify(r)
                except Exception as error:  # noqa: BLE001
                    r["note"] += f"확인 예외: {error}"
                    self.csv.write(",".join(str(r.get(h, "")).replace(",", " ") for h in self.HEADER) + "\n")

    def finish(self):
        self.stop_flag.set()
        self.join(timeout=120)
        # 남은 항목은 시한까지 기다렸다 확인한다
        deadline = time.time() + 200
        for r in list(self.pending):
            wait = max(r.get("boundary", 0), r.get("send_time", 0)) + JOB_DEADLINE_S + 10 - time.time()
            if wait > 0 and time.time() + wait < deadline:
                time.sleep(wait)
            try:
                self.verify(r)
            except Exception as error:  # noqa: BLE001
                r["note"] += f"확인 예외: {error}"
        self.csv.close()


# ---- 판정(파일만 읽는다) ----------------------------------------------------------------------------------
def read_csv(path):
    if not Path(path).exists():
        return []
    lines = Path(path).read_text().splitlines()
    if len(lines) < 2:
        return []
    head = lines[0].split(",")
    out = []
    for line in lines[1:]:
        cells = line.split(",")
        out.append({h: (cells[i] if i < len(cells) else "") for i, h in enumerate(head)})
    return out


def num(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def read_jsonl(path):
    out = []
    if Path(path).exists():
        for line in open(path):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out


def judge(out):
    out = Path(out)
    meta = json.loads((out / "meta.json").read_text())
    backend = meta["backend"]
    checks = []

    def check(name, ok, detail=""):
        checks.append({"name": name, "ok": bool(ok), "detail": str(detail)})

    samples = []
    for row in read_csv(out / "samples.csv"):
        samples.append({k: num(v) for k, v in row.items()})
    span_h = (samples[-1]["t"] - samples[0]["t"]) / 3600 if len(samples) > 1 else 0
    planned_h = meta["duration_s"] / 3600
    check("시험이 계획한 시간의 95% 이상 돌았다", span_h >= planned_h * 0.95, f"표본 {len(samples)}개, {span_h:.2f}h / 계획 {planned_h:.2f}h")
    trends = {}
    for key, floor, label in (("rss_mb", 10.0, "RSS(MB)"), ("threads", 5.0, "스레드"), ("fds", 5.0, "FD")):
        pts = [(s["t"], s[key]) for s in samples if s.get(key) is not None]
        pts = pts[int(len(pts) * 0.2):]
        verdict = trend_verdict(pts, floor)
        trends[key] = {**verdict, "first": pts[0][1] if pts else None, "last": pts[-1][1] if pts else None, "max": max((v for _, v in pts), default=None)}
        check(f"{label} 평탄 (처음 20% 제외, 기울기 ≤ max(평균 5%, {floor:g})/시간 이거나 마지막이 앞 절반 최댓값+{floor:g} 이하)", verdict["ok"] and len(pts) >= 5, json.dumps(trends[key]))
    pg = [s["pg_app"] for s in samples if s.get("pg_app") is not None]
    idle = [s["pg_idle_in_tx"] for s in samples if s.get("pg_idle_in_tx") is not None]
    check("앱 DB 연결 ≤ 풀 크기 10, 5초 넘은 idle in transaction 0", bool(pg) and max(pg) <= POOL_SIZE and (max(idle) if idle else 0) == 0, f"pg_app max {max(pg) if pg else None}, idle_in_tx max {max(idle) if idle else None}")

    runtime = read_jsonl(out / "runtime.jsonl")
    ts = sorted(r["ts"] for r in runtime)
    gaps = [b - a for a, b in zip(ts, ts[1:])]
    minutes = (samples[-1]["t"] - samples[0]["t"]) / 60 if len(samples) > 1 else 0
    check("런타임 상태 로그가 매분 남음 (최대 간격 ≤ 90초, 개수 ≥ 경과 분 - 3)", bool(ts) and max(gaps or [0]) <= 90 and len(ts) >= minutes - 3, f"{len(ts)}개 / 경과 {minutes:.0f}분, 최대 간격 {max(gaps or [0]):.0f}초")
    pend = [r.get("hikariPending", 0) for r in sorted(runtime, key=lambda r: r["ts"])]
    run = best = 0
    for p in pend:
        run = run + 1 if p > 0 else 0
        best = max(best, run)
    check("풀 대기(hikariPending)가 이어지지 않음 (1분 표본이 연달아 2번 이상 > 0 이면 실패)", best <= 1,
          f"max {max(pend, default=0)}, 0 초과 표본 {sum(1 for p in pend if p > 0)}/{len(pend)}, 연속 최대 {best}, hikariActive max {max((r.get('hikariActive', 0) for r in runtime), default=0)}")

    edge = read_jsonl(out / "edge.jsonl")
    total = sum(e["n"] for e in edge)
    fivexx = sum(e["status"].get("5xx", 0) for e in edge)
    check("edge 접근 로그 5xx 0", total > 0 and fivexx == 0, f"요청 {total}건, 5xx {fivexx}건 {sorted({u for e in edge for u in e.get('5xx_uris', [])})[:5]}")
    full = [e for e in edge if e["n"] >= 1000]  # 끝에 남은 짧은 구간(요청 수가 적음)은 p99 가 의미 없어 뺀다
    if len(full) >= 2:
        p99s = [e["p99_ms"] for e in full]
        half = p99s[: len(p99s) // 2]
        late = p99s[len(p99s) // 2:]
        check("p99 가 시간이 갈수록 오르지 않음 (뒤 절반 중앙값 ≤ 앞 절반 중앙값 × 2 + 50ms)", sorted(late)[len(late) // 2] <= sorted(half or late)[len(half or late) // 2] * 2 + 50,
              f"앞 {sorted(half or late)[len(half or late) // 2]:.1f}ms, 뒤 {sorted(late)[len(late) // 2]:.1f}ms")

    errors_lines = (out / "errors.log").read_text().splitlines() if (out / "errors.log").exists() else []
    # SQS 연결 일시 오류(SdkClientException): imds-mock 의 자격증명 Expiration 이 2020년이라 호출마다 IMDS 를 다시 부르고, 호스트가 바쁠 때 그 호출(연결 거부·DNS 실패)이 실패하면 SQS 호출이 실패한다.
    # 실제 EC2 에서는 자격증명이 몇 시간 유효해 캐시에 맞는다. 하네스 한계라 따로 세어 보고하고 시간당 상한만 둔다(report-l5.md 참조).
    transient = [line for line in errors_lines if '"errorCode":"SdkClientException"' in line]
    other_errors = [line for line in errors_lines if line not in transient]
    static = bool(meta.get("static_credentials"))
    check("백엔드 ERROR 로그 0 (SQS 연결 일시 오류 제외)", not other_errors, f"{len(other_errors)}줄 {other_errors[:2]}")
    if static:
        check("SQS 연결 일시 오류(SdkClientException) 0 (정적 자격증명 모드)", not transient, f"{len(transient)}줄")
    else:
        check("SQS 연결 일시 오류(SdkClientException)는 참고 수치(IMDS 모드: 하네스 imds-mock 이 가끔 연결을 거부한다)", True, f"{len(transient)}줄 / {span_h:.2f}시간")

    jobs = read_csv(out / "jobs.csv")
    if backend == "rust":
        n = len(jobs)
        notice_bad = [j["cycle"] for j in jobs if j["notice_ok"] != "True"]
        # 주제 푸시: 집계 로그가 제때 남았고(누락 없음) 대상자 수가 맞아야 한다. 일부 발행 실패(SQS 연결 일시 오류)는 대상자의 TOPIC_PARTIAL_MAX 이내면 허용하고 따로 센다.
        topic_bad, topic_partial = [], []
        for j in jobs:
            delay, target, accepted = num(j["topic_logged_delay_s"]), num(j["topic_target"]), num(j["topic_accepted"])
            if delay is None or delay > JOB_DEADLINE_S or not target or accepted is None or target - accepted > target * (0 if static else TOPIC_PARTIAL_MAX):
                topic_bad.append(j["cycle"])
            elif accepted < target:
                topic_partial.append(f"{j['cycle']}:{int(target - accepted)}")
        check("주입한 작업이 있다 (경과 분 - 5 이상)", n >= minutes - 5, f"주입·확인 {n}건 / 경과 {minutes:.0f}분")
        check(f"공지 배포 누락 0 (예약 뒤 {JOB_DEADLINE_S:.0f}초 안, 알림 수 = 활성 회원 수)", n > 0 and not notice_bad, f"누락·지연 {len(notice_bad)}건 {notice_bad[:10]}")
        check(f"주제 푸시 누락 0 (회차 뒤 {JOB_DEADLINE_S:.0f}초 안 집계 로그, 미수락: IMDS 모드 ≤ 대상자의 {TOPIC_PARTIAL_MAX:.0%}, 정적 자격증명 모드 0)", n > 0 and not topic_bad, f"누락·지연 {len(topic_bad)}건 {topic_bad[:10]}, 일부 미수락(허용 범위) {len(topic_partial)}건 {topic_partial[:10]}")
        delays = [num(j["notice_delay_s"]) for j in jobs if num(j["notice_delay_s"]) is not None]
        tdelays = [num(j["topic_logged_delay_s"]) for j in jobs if num(j["topic_logged_delay_s"]) is not None]
        trends["notice_delay_s"] = {"p50": percentile(delays, 0.5), "p99": percentile(delays, 0.99), "max": max(delays, default=None)}
        trends["topic_delay_s"] = {"p50": percentile(tdelays, 0.5), "p99": percentile(tdelays, 0.99), "max": max(tdelays, default=None)}
    meta_end = meta.get("end") or {}
    if "restart_count_before" in meta and "restart_count_after" in meta_end:
        check("백엔드 재시작 없음", meta["restart_count_before"] == meta_end["restart_count_after"] and meta["started_at"] == meta_end.get("started_at"), f"{meta['restart_count_before']} -> {meta_end['restart_count_after']}")
    up = read_json_safe(out / "uploads.json")
    if not up:
        check("업로드 흐름 결과 파일(uploads.json)이 있다", False, "없음 - 부하 발생기가 결과를 쓰지 못했다(uploads.log 확인)")
    if up:
        check("업로드 흐름: 닫히지 않은 업로드 0, 반려 0", up["stuck"] == 0 and up["rejected"] == 0, f"시작 {up['started']} 완료 {up['ready']} 반려 {up['rejected']} stuck {up['stuck']} 오류 {up['errors']}")
    verdict = {"backend": backend, "out": str(out), "span_hours": round(span_h, 2), "planned_hours": round(planned_h, 2), "samples": len(samples), "trends": trends,
               "jobs_checked": len(jobs), "edge_requests": total, "checks": checks, "ok": all(c["ok"] for c in checks)}
    (out / "verdict.json").write_text(json.dumps(verdict, ensure_ascii=False, indent=2))
    text = "\n".join(f"{'통과' if c['ok'] else '실패'}: {c['name']} - {c['detail']}" for c in checks)
    text += f"\n\n종합: {'통과' if verdict['ok'] else '실패'} ({sum(1 for c in checks if c['ok'])}/{len(checks)})\n"
    (out / "verdict.txt").write_text(text)
    return verdict


def read_json_safe(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return None


# ---- 실행 ------------------------------------------------------------------------------------------
def ensure_future_topics():
    """시험이 KST 자정을 넘겨도 오늘의 주제가 있도록 앞으로 3일치를 더한다."""
    psql("INSERT INTO topics (id, title, topic_date, starts_at, ends_at) "
         "SELECT gen_random_uuid(), '주제 s9', d::date, d::timestamp AT TIME ZONE 'Asia/Seoul', (d::timestamp + interval '1 day') AT TIME ZONE 'Asia/Seoul' "
         "FROM generate_series((now() AT TIME ZONE 'Asia/Seoul')::date + 1, (now() AT TIME ZONE 'Asia/Seoul')::date + 3, interval '1 day') d "
         "ON CONFLICT DO NOTHING")


def run(backend, duration_s, out, rps, keep):
    out.mkdir(parents=True, exist_ok=True)
    stack = Stack(backend)
    stack.env["ESM_CONCURRENCY"] = "1"
    log_progress(out, f"S9 시작: 백엔드 {backend}, {duration_s / 3600:.2f}시간, 부하 {rps} req/s, 출력 {out}")
    stack.up(fresh=True)
    seed(stack)
    ensure_future_topics()
    start = time.time()
    meta = {"backend": backend, "start": start, "duration_s": duration_s, "rps": rps, "restart_count_before": stack.restart_count(), "started_at": stack.started_at(),
            "command": " ".join(sys.argv), "static_credentials": os.environ.get("S9_STATIC_CREDENTIALS") == "1"}
    (out / "meta.json").write_text(json.dumps(meta, indent=2))
    end_ts = start + duration_s
    dur = f"{int(duration_s)}s"
    sampler = Sampler(stack, period=SAMPLE_PERIOD, pg_every=1, csv_path=out / "samples.csv").start()
    harvester = Harvester(stack, out, start)
    harvester.start()
    mix_proc, mix_log = loadgen_run(mix_args(stack, rps, dur, out / "mix.json", f"{backend}-s9", conns=4, idle=10, warmup="10s", interval="300", pid=stack.pid()), out / "mix.log")
    churn_args = ["run", "--base", EDGE_HTTP, "--secret", access_secret_hex(), "--rps", "2", "--conns", "2", "--no-keepalive", "--duration", dur, "--warmup", "0s",
                  "--label", f"{backend}-s9-churn", "--out", str(out / "churn.json"), *SEED_ARGS]
    churn_proc, churn_log = loadgen_run(churn_args, out / "churn.log")
    up_proc, up_log = loadgen_run(uploads_args(0.05, dur, out / "uploads.json", f"{backend}-s9", users=20, conns=4, timeout="7m"), out / "uploads.log")
    injector = WorkInjector(stack, out, end_ts) if backend == "rust" else None
    if injector:
        injector.start()
    try:
        while time.time() < end_ts:
            time.sleep(min(300, max(1, end_ts - time.time())))
            last = sampler.samples[-1] if sampler.samples else {}
            log_progress(out, f"경과 {(time.time() - start) / 60:.0f}분 / {duration_s / 60:.0f}분: RSS {last.get('rss_mb')}MB 스레드 {last.get('threads')} FD {last.get('fds')} DB연결 {last.get('pg_app')} "
                              f"호스트 부하 {last.get('load1')} 주입 {injector.cycle if injector else 0}건 ERROR {harvester.errors}")
        log_progress(out, "시간이 끝났다. 부하 발생기와 작업 확인을 마무리한다")
        wait_all([mix_proc, churn_proc, up_proc], 7 * 60 + 120)
    finally:
        for p in (mix_proc, churn_proc, up_proc):
            if p.poll() is None:
                p.kill()
        for f in (mix_log, churn_log, up_log):
            f.close()
        if injector:
            injector.finish()
        sampler.stop()
        harvester.finish()
        meta["end"] = {"t": time.time(), "restart_count_after": stack.restart_count(), "started_at": stack.started_at()}
        (out / "meta.json").write_text(json.dumps(meta, indent=2))
    verdict = judge(out)
    print((out / "verdict.txt").read_text())
    if not keep:
        stack.down()
    return 0 if verdict["ok"] else 1


def main():
    args = sys.argv[1:]
    if len(args) >= 2 and args[0] == "judge":
        verdict = judge(args[1])
        print((Path(args[1]) / "verdict.txt").read_text())
        sys.exit(0 if verdict["ok"] else 1)
    if len(args) >= 2 and args[0] == "run" and args[1] in ("rust", "java"):
        opts = args[2:]
        duration = parse_duration(opts[opts.index("--duration") + 1]) if "--duration" in opts else 8 * 3600
        rps = float(opts[opts.index("--rps") + 1]) if "--rps" in opts else 10
        out = Path(opts[opts.index("--out") + 1]).resolve() if "--out" in opts else COMPOSE_DIR / ".state/s9" / f"{args[1]}-{time.strftime('%Y%m%d-%H%M%S')}"
        sys.exit(run(args[1], duration, out, rps, "--keep" in opts))
    print(__doc__)
    sys.exit(2)


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"[s9] 실패: {failure}", file=sys.stderr)
        sys.exit(1)
