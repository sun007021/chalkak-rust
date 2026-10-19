#!/usr/bin/env python3
"""S7 푸시 경로 안정성 시험: 승인 몰림, 가짜 FCM 5xx·429(Retry-After), SQS 지연, 30분 기한, DLQ 이동.

계획 7.2의 S7. 앱 없이 HTTP·SQL 로 돌린다(e2e_image.py 의 헬퍼를 쓴다). 백엔드는 떠 있는 것(rust 또는 java)을 자동으로 찾는다.
세부 절차와 해석은 docs/local-aws/report-s7.md. 보통은 scripts/s7.sh 로 스택을 맞춰 띄운 뒤 돈다.

시나리오(이름을 인자로 준다. 기본은 all)
  redrive    큐 설정(visibility 60, 보관 14일, redrive 10)을 읽고, 같은 설정의 임시 큐 쌍에서 10회 수신 뒤 DLQ 이동을 확인
  burst      게시물 N건(기본 300)을 동시에 승인: PENDING -> PUBLISHED, Worker 소비, 중복 없음
  fcm503     가짜 FCM 이 5xx(503)를 주다가 회복: SDK 재시도, 큐 재수신, 회복 뒤 정확히 한 번 수락
  expire     30분 기한: Worker 수신 시점 / 메시지 시각 / Relay 발행 시점 (시계 대신 DB created_at 을 31분 앞당긴다)
  sqsdelay   SQS 응답 지연(toxiproxy): 2초(5초 제한 안), 6초(제한 밖: SQS_UNKNOWN -> 중복 발행)
  dlq        가짜 FCM 이 429+Retry-After / 503 을 계속 주면 같은 메시지가 10회 수신된 뒤 DLQ 로 가는지 (약 10분)

환경변수: HOST_IP, BACKEND(rust|java, 기본 자동), S7_BURST(300), S7_OUT(결과 JSON 경로), FAKES_PORT(19000),
  TOXIPROXY_PORT(8474), AWS_DEBUG_PORT(4566), S7_DLQ_TIMEOUT(초, 기본 1500)
Java 는 Worker 가 없어서(FCM 주소를 바꿀 수 없다) 발송 단계 시나리오(fcm503)는 건너뛴다.
"""

import json
import os
import re
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # python3 -I 는 스크립트 폴더를 경로에 넣지 않는다
import e2e_image as e  # noqa: E402
from e2e_image import Failure, api  # noqa: E402


def psql(statement):
    """e2e_image.psql 과 같고 오류 때 psql 의 메시지를 보여 준다."""
    result = subprocess.run(
        ["docker", "compose", "exec", "-T", "postgres", "psql", "-q", "-tA", "-U", "chalkak", "-d", "chalkak", "-v", "ON_ERROR_STOP=1",
         "-c", statement], cwd=e.COMPOSE_DIR, capture_output=True, text=True)
    if result.returncode != 0:
        raise Failure(f"psql 실패: {result.stderr.strip()[:400]}")
    return result.stdout.strip()


COMPOSE_DIR = e.COMPOSE_DIR
FAKES = f"http://127.0.0.1:{os.environ.get('FAKES_PORT', '19000')}"
TOXI = f"http://127.0.0.1:{os.environ.get('TOXIPROXY_PORT', '8474')}"
SQS_URL = f"http://127.0.0.1:{os.environ.get('AWS_DEBUG_PORT', '4566')}/"
QUEUE = "http://localhost:4566/000000000000/chalkak-dev-push"
DLQ = "http://localhost:4566/000000000000/chalkak-dev-push-dlq"
SQS_AUTH = "AWS4-HMAC-SHA256 Credential=test/20260101/ap-northeast-2/sqs/aws4_request, SignedHeaders=host, Signature=x"
RESULTS = {}
_plain = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def say(message):
    print(f"[s7 {time.strftime('%H:%M:%S')}] {message}", flush=True)


def check(name, ok, detail=""):
    """통과 기준 하나. 결과 JSON 의 checks 에 모아 마지막에 요약한다."""
    RESULTS.setdefault("checks", []).append({"name": name, "ok": bool(ok), "detail": detail})
    say(f"{'통과' if ok else '실패'}: {name}{(' - ' + detail) if detail else ''}")
    return ok


# ---- 외부 호출 -----------------------------------------------------------------------------------
def raw(method, url, payload=None, headers=None, timeout=30):
    body = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with _plain.open(request, timeout=timeout) as response:
            data = response.read()
            return response.status, (json.loads(data) if data else None)
    except urllib.error.HTTPError as error:
        data = error.read()
        try:
            return error.code, json.loads(data) if data else None
        except ValueError:
            return error.code, data.decode(errors="replace")


def sqs(action, payload):
    status, body = raw("POST", SQS_URL, payload, {
        "X-Amz-Target": f"AmazonSQS.{action}", "Content-Type": "application/x-amz-json-1.0", "Authorization": SQS_AUTH})
    if status >= 300:
        raise Failure(f"SQS {action} -> {status}: {body}")
    return body


def depth(queue_url=QUEUE):
    attrs = sqs("GetQueueAttributes", {"QueueUrl": queue_url, "AttributeNames": ["All"]})["Attributes"]
    return sum(int(attrs[k]) for k in ("ApproximateNumberOfMessages", "ApproximateNumberOfMessagesNotVisible",
                                       "ApproximateNumberOfMessagesDelayed"))


def fakes_set(fcm_rules):
    status, body = raw("PUT", f"{FAKES}/__admin/responses", {"fcm": fcm_rules})
    if status != 200:
        raise Failure(f"fakes 규칙 지정 실패 {status}: {body}")


def fakes_requests():
    return raw("GET", f"{FAKES}/__admin/requests")[1]


def fcm_sends(prefix):
    """prefix 로 시작하는 토큰으로 간 FCM 전송 요청: [(seq, token)]."""
    out = []
    for record in fakes_requests():
        if record["service"] == "fcm_send":
            token = json.loads(record["body"])["message"]["token"]
            if token.startswith(prefix):
                out.append((record["seq"], token))
    return out


def toxic(latency_ms=None):
    """SQS 경로(toxiproxy 'aws')의 응답 지연. None 이면 제거."""
    raw("DELETE", f"{TOXI}/proxies/aws/toxics/s7-latency")
    if latency_ms:
        status, body = raw("POST", f"{TOXI}/proxies/aws/toxics", {
            "name": "s7-latency", "type": "latency", "stream": "downstream", "attributes": {"latency": latency_ms}})
        if status >= 300:
            raise Failure(f"toxiproxy 지연 지정 실패 {status}: {body} (스택을 --profile chaos 와 SQS_ENDPOINT_URL 로 띄웠는가?)")


def reset_world():
    """시나리오 사이를 비운다: 가짜 외부 서버 기록·규칙, 지연, 큐, 남은 PENDING 알림."""
    toxic(None)
    raw("POST", f"{FAKES}/__admin/reset")
    for queue_url in (QUEUE, DLQ):
        sqs("PurgeQueue", {"QueueUrl": queue_url})
    psql("UPDATE notifications SET sqs_publish_status='NOT_REQUIRED', next_attempt_at=NULL, sqs_published_at=NULL "
         "WHERE sqs_publish_status='PENDING'")
    time.sleep(1)


def backend():
    forced = os.environ.get("BACKEND")
    if forced:
        return forced
    for name in ("rust", "java"):
        out = subprocess.run(["docker", "compose", "ps", "-q", name], cwd=COMPOSE_DIR, capture_output=True, text=True).stdout
        if out.strip():
            return name
    raise Failure("rust 도 java 도 떠 있지 않다")


BACKEND = None


def logs(since):
    """백엔드 JSON 로그 줄(since 이후). 알림 로그만 골라 dict 로."""
    out = subprocess.run(["docker", "compose", "logs", BACKEND, "--no-log-prefix", "--since", since],
                         cwd=COMPOSE_DIR, capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        if '"type":"notification"' not in line:
            continue
        try:
            row = json.loads(line)
        except ValueError:
            continue
        row["_t"] = parse_ts(row["@timestamp"])
        rows.append(row)
    return rows


def parse_ts(text):
    text = re.sub(r"(\.\d{6})\d+", r"\1", text)
    return datetime.fromisoformat(text).timestamp()


def unknown_count(cohort, since):
    """Relay 의 수락 불명(SQS_UNKNOWN) 건수. Rust 는 구조화 로그에서, Java 는 콘솔 로그에 결과 필드가 없어 DB 로 센다
    (PENDING 이면서 다음 시도가 30초 넘게 남은 알림 = 실패·불명으로 재시도가 예약된 것)."""
    if BACKEND == "rust":
        return sum(1 for r in logs(since) if r.get("result") == "SQS_UNKNOWN")
    return int(psql("SELECT count(*) FROM notifications n JOIN users u ON u.id = n.user_id "
                    f"WHERE u.email LIKE 's7-{cohort.run}-%' AND n.sqs_publish_status = 'PENDING' "
                    "AND n.next_attempt_at > now() + interval '30 seconds'"))


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


# ---- 데이터 준비 ---------------------------------------------------------------------------------
_last_base = 0


class Cohort:
    """회원·로그인 세션·푸시 기기·PENDING 게시물을 SQL 로 한 번에 만든다(이미지 업로드 흐름은 e2e-image 가 이미 확인한다)."""

    def __init__(self, count):
        global _last_base
        self.base = max(int(time.time()) << 12, _last_base + 4096)
        _last_base = self.base
        self.run = format(self.base, "x")
        self.count = count
        self.prefix = f"s7-{self.run}-"
        topic_id = e.todays_topic_id()  # e2e_image.psql 사용
        b = self.base

        def uid(kind, i):
            return f"('01990000-0000-7000-87{kind:02x}-' || lpad(to_hex({b} + {i}), 12, '0'))::uuid"

        sql = f"""
INSERT INTO users (id, email, status, signature_original_storage_key, signature_thumbnail_storage_key)
SELECT {uid(0, 'g')}, 's7-{self.run}-' || g || '@test.chalkak', 'ACTIVE', 'chalkak/signatures/s7/{self.run}/' || g || '/o.png', 'chalkak/signatures/s7/{self.run}/' || g || '/t.png'
FROM generate_series(1, {count}) g;
INSERT INTO user_refresh_tokens (user_id, session_id, token_hash, expires_at, absolute_expires_at)
SELECT {uid(0, 'g')}, {uid(1, 'g')}, encode(sha256(('rt-{self.run}-' || g)::bytea), 'hex'),
       now() + interval '30 days', now() + interval '90 days'
FROM generate_series(1, {count}) g;
INSERT INTO push_devices (id, user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at)
SELECT {uid(2, 'g')}, {uid(0, 'g')}, {uid(1, 'g')}, 's7-{self.run}-' || g,
       encode(sha256(('s7-{self.run}-' || g)::bytea), 'hex'), now(), now()
FROM generate_series(1, {count}) g;
INSERT INTO photos (id, original_storage_key, thumbnail_storage_key, metadata)
SELECT {uid(3, 'g')}, 'chalkak/posts/s7/{self.run}/original/' || g || '.webp', 'chalkak/posts/s7/{self.run}/thumbnail/' || g || '.webp',
       '{{"width":1600,"height":1200,"byteSize":21082}}'::jsonb
FROM generate_series(1, {count}) g;
INSERT INTO posts (id, user_id, topic_id, photo_id, title, moderation_status, created_at, updated_at)
SELECT {uid(4, 'g')}, {uid(0, 'g')}, '{topic_id}', {uid(3, 'g')}, 's7 ' || g, 'PENDING', now(), now()
FROM generate_series(1, {count}) g;
"""
        psql(sql)
        self.post_ids = [f"01990000-0000-7000-8704-{self.base + i:012x}" for i in range(1, count + 1)]
        self.tokens = [f"{self.prefix}{i}" for i in range(1, count + 1)]

    def token(self, i):
        return self.tokens[i - 1]

    def approve(self, indexes=None, workers=16):
        """관리자 승인(PUT moderation)을 동시에 건다. (요청 지연 목록 ms, 걸린 시간 s, 실패 수)."""
        admin = e.admin_login()
        posts = self.post_ids if indexes is None else [self.post_ids[i - 1] for i in indexes]
        latencies, failures = [], []

        def one(post_id):
            started = time.monotonic()
            try:
                api("PUT", f"/api/v1/admin/posts/{post_id}/moderation", {"status": "APPROVED"}, token=admin)
                latencies.append((time.monotonic() - started) * 1000)
            except Failure as failure:
                failures.append(str(failure)[:200])

        started = time.monotonic()
        with ThreadPoolExecutor(workers) as pool:
            list(pool.map(one, posts))
        return latencies, time.monotonic() - started, failures

    def statuses(self):
        rows = psql("SELECT n.sqs_publish_status, count(*) FROM notifications n JOIN users u ON u.id = n.user_id "
                    f"WHERE u.email LIKE 's7-{self.run}-%' GROUP BY 1")
        return {line.split("|")[0]: int(line.split("|")[1]) for line in rows.splitlines() if line}

    def notification_ids(self):
        rows = psql("SELECT n.id, u.email FROM notifications n JOIN users u ON u.id = n.user_id "
                    f"WHERE u.email LIKE 's7-{self.run}-%'")
        return {line.split("|")[1].split("-")[2].split("@")[0]: line.split("|")[0] for line in rows.splitlines() if line}

    def sends(self):
        return fcm_sends(self.prefix)


def percentile(values, q):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * q))]


def wait_for(description, probe, timeout, interval=1.0):
    """probe 가 참(값)을 돌려줄 때까지. (값, 걸린 초). 시간 초과면 (None, timeout)."""
    started = time.monotonic()
    while time.monotonic() - started < timeout:
        value = probe()
        if value:
            return value, time.monotonic() - started
        time.sleep(interval)
    say(f"시간 초과 {timeout:.0f}초: {description}")
    return None, timeout


def fcm_error(status, code, details=None):
    error = {"code": status, "message": "s7 injected", "status": code}
    if details:
        error["details"] = [{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError", "errorCode": details}]
    return json.dumps({"error": error})


# ---- 시나리오 ------------------------------------------------------------------------------------
def scenario_redrive():
    say("== redrive: 큐 설정과 DLQ 이동")
    main = sqs("GetQueueAttributes", {"QueueUrl": QUEUE, "AttributeNames": ["All"]})["Attributes"]
    dead = sqs("GetQueueAttributes", {"QueueUrl": DLQ, "AttributeNames": ["All"]})["Attributes"]
    policy = json.loads(main["RedrivePolicy"])
    info = {"visibility": main["VisibilityTimeout"], "retention": main["MessageRetentionPeriod"],
            "wait": main["ReceiveMessageWaitTimeSeconds"], "redrive": policy, "dlq_retention": dead["MessageRetentionPeriod"]}
    RESULTS["redrive_config"] = info
    check("원본 큐 visibility 60초", main["VisibilityTimeout"] == "60", main["VisibilityTimeout"])
    check("원본 큐 redrive maxReceiveCount=10, 대상 DLQ", str(policy["maxReceiveCount"]) == "10"
          and policy["deadLetterTargetArn"].endswith(":chalkak-dev-push-dlq"), main["RedrivePolicy"])
    check("DLQ 보관 14일", dead["MessageRetentionPeriod"] == "1209600", dead["MessageRetentionPeriod"])
    check("원본 큐 Receive wait time 20초(원본 문서 표)", main["ReceiveMessageWaitTimeSeconds"] == "20", main["ReceiveMessageWaitTimeSeconds"])

    # 같은 설정의 임시 큐 쌍으로 이동 시점을 본다(진짜 큐는 Worker 가 소비 중이라 수신 횟수가 섞인다).
    name = f"s7-probe-{uuid.uuid4().hex[:6]}"
    dlq_url = sqs("CreateQueue", {"QueueName": f"{name}-dlq"})["QueueUrl"]
    dlq_arn = sqs("GetQueueAttributes", {"QueueUrl": dlq_url, "AttributeNames": ["QueueArn"]})["Attributes"]["QueueArn"]
    url = sqs("CreateQueue", {"QueueName": name, "Attributes": {
        "VisibilityTimeout": "60", "RedrivePolicy": json.dumps({"deadLetterTargetArn": dlq_arn, "maxReceiveCount": 10})}})["QueueUrl"]
    try:
        sqs("SendMessage", {"QueueUrl": url, "MessageBody": "s7-probe"})
        returned = 0
        for attempt in range(1, 16):
            got = sqs("ReceiveMessage", {"QueueUrl": url, "MaxNumberOfMessages": 1, "VisibilityTimeout": 0,
                                         "WaitTimeSeconds": 0, "AttributeNames": ["ApproximateReceiveCount"]})
            if got.get("Messages"):
                returned += 1
                last = got["Messages"][0]["Attributes"]["ApproximateReceiveCount"]
            else:
                say(f"수신 시도 {attempt}회째: 메시지 없음")
                break
            time.sleep(0.2)
        in_dlq = depth(dlq_url)
        RESULTS["redrive_probe"] = {"receives_that_returned_message": returned, "dlq_depth": in_dlq}
        check("임시 큐: 메시지가 정확히 10번 수신되고 이후 DLQ 로 이동", returned == 10 and in_dlq == 1,
              f"수신 {returned}회(마지막 ApproximateReceiveCount={last}), DLQ {in_dlq}건")
    finally:
        sqs("DeleteQueue", {"QueueUrl": url})
        sqs("DeleteQueue", {"QueueUrl": dlq_url})


def scenario_burst(count):
    say(f"== burst: 게시물 {count}건 동시 승인")
    reset_world()
    worker = BACKEND == "rust"
    cohort = Cohort(count)
    since = now_iso()
    t0 = time.monotonic()
    latencies, took, failures = cohort.approve()
    check("승인 요청 모두 성공", not failures and len(latencies) == count, f"실패 {len(failures)}")
    marks = {}

    def probe():
        statuses = cohort.statuses()
        if "published_all" not in marks and statuses.get("PUBLISHED", 0) == count:
            marks["published_all"] = time.monotonic() - t0
        if worker:
            sends = cohort.sends()
            if "sent_all" not in marks and len({t for _, t in sends}) == count:
                marks["sent_all"] = time.monotonic() - t0
            if "published_all" in marks and "sent_all" in marks and depth() == 0:
                marks["queue_empty"] = time.monotonic() - t0
                return True
            return False
        return "published_all" in marks

    wait_for("burst 완료", probe, 600, 1.0)
    statuses = cohort.statuses()
    sends = cohort.sends()
    per_token = Counter(t for _, t in sends)
    rows = logs(since)
    results = Counter(r.get("result") for r in rows if r.get("stage") == "relay")
    metrics = {
        "count": count, "approve_total_s": round(took, 2), "approve_p50_ms": round(percentile(latencies, 0.5) or 0),
        "approve_p99_ms": round(percentile(latencies, 0.99) or 0), "statuses": statuses,
        "published_all_s": round(marks.get("published_all", -1), 2), "relay_results": dict(results),
        "queue_depth_end": depth(), "dlq_depth": depth(DLQ),
    }
    if worker:
        metrics.update(sent_all_s=round(marks.get("sent_all", -1), 2), queue_empty_s=round(marks.get("queue_empty", -1), 2),
                       fcm_requests=len(sends), fcm_distinct_tokens=len(per_token),
                       fcm_duplicates=sum(v - 1 for v in per_token.values()))
    RESULTS["burst"] = metrics
    say(json.dumps(metrics, ensure_ascii=False))
    check("전 알림 PENDING -> PUBLISHED", statuses == {"PUBLISHED": count}, str(statuses))
    if worker:  # Java 콘솔 로그에는 결과 필드가 없다
        check("Relay 결과가 SQS_ACCEPTED 로만 구성(재시도 없음)", results.get("SQS_ACCEPTED") == count and
              not (results.keys() - {"REQUESTED", "SQS_ACCEPTED"}), str(dict(results)))
    if worker:
        check("FCM 이 모든 기기에 정확히 한 번 수락(중복 0)", len(per_token) == count and metrics["fcm_duplicates"] == 0,
              f"요청 {len(sends)}, 기기 {len(per_token)}")
        check("큐가 비고 DLQ 0", metrics["queue_depth_end"] == 0 and metrics["dlq_depth"] == 0)
    else:
        check("Java: 발행된 건수만큼 메시지가 큐에 남는다(Worker 없음)", metrics["queue_depth_end"] == count,
              f"큐 {metrics['queue_depth_end']}")


def scenario_fcm503(count=10):
    say(f"== fcm503: 가짜 FCM 503 -> 회복 (기기 {count})")
    if BACKEND != "rust":
        say("건너뜀: Java 는 FCM 주소를 바꿀 수 없다")
        RESULTS["fcm503"] = {"skipped": "java: FCM 주소를 바꿀 수 없다"}
        return
    reset_world()
    cohort = Cohort(count)
    fakes_set([{"contains": cohort.prefix, "status": 503, "body": fcm_error(503, "UNAVAILABLE")}])
    since = now_iso()
    t0 = time.monotonic()
    cohort.approve()

    def first_pass_done():
        per = Counter(t for _, t in cohort.sends())
        return len(per) == count and min(per.values()) >= 5

    ok, first_pass = wait_for("모든 기기가 SDK 재시도 5회를 마침", first_pass_done, 300, 2)
    per_failed = Counter(t for _, t in cohort.sends())
    recover_seq = len(fakes_requests())
    fakes_set([])  # 회복
    t_recover = time.monotonic()

    def all_accepted():
        accepted = {t for seq, t in cohort.sends() if seq > recover_seq}
        return len(accepted) == count and depth() == 0

    done, after = wait_for("회복 뒤 모두 수락", all_accepted, 400, 2)
    sends = cohort.sends()
    accepted = Counter(t for seq, t in sends if seq > recover_seq)
    rows = logs(since)
    receive_counts = Counter(r.get("receiveCount") for r in rows if r.get("stage") == "worker_received")
    pending = [r for r in rows if r.get("stage") == "worker_retry_pending"]
    metrics = {
        "count": count, "first_pass_s": round(first_pass, 1), "requests_per_message_first_pass": sorted(set(per_failed.values())),
        "recover_to_all_accepted_s": round(after, 1), "accepted_per_device": sorted(set(accepted.values())),
        "receive_counts": dict(receive_counts), "retry_pending_logs": len(pending),
        "retry_after_seconds_logged": sorted({r.get("retryAfterSeconds") for r in pending}),
        "dlq_depth": depth(DLQ), "statuses": cohort.statuses(),
    }
    RESULTS["fcm503"] = metrics
    say(json.dumps(metrics, ensure_ascii=False))
    check("503 동안 기기마다 SDK 가 5번(재시도 4번)까지 보낸다", ok and set(per_failed.values()) == {5}, str(sorted(set(per_failed.values()))))
    check("회복 뒤 모든 기기에 정확히 한 번 수락, 큐 비움", bool(done) and set(accepted.values()) == {1})
    check("재수신은 수신 횟수 2 로 끝나고 DLQ 로 가지 않는다", dict(receive_counts) == {"1": count, "2": count} and metrics["dlq_depth"] == 0,
          str(dict(receive_counts)))
    check("Retry-After 없는 503 은 최소 60초 대기를 로그에 남긴다", metrics["retry_after_seconds_logged"] == [60],
          str(metrics["retry_after_seconds_logged"]))
    check("알림 행은 PUBLISHED 그대로(발송 결과는 DB 에 반영하지 않는다)", metrics["statuses"] == {"PUBLISHED": count}, str(metrics["statuses"]))


def scenario_expire():
    say("== expire: 30분 기한 (시계를 기다리지 않고 DB created_at 을 31분 앞당긴다)")
    reset_world()
    worker = BACKEND == "rust"
    out = {}
    # (c) Relay 발행 시점: SQS 가 6초 늦어 UNKNOWN 이 된 PENDING 알림이 기한을 넘기면 SQS 를 부르지 않고 EXPIRED.
    cohort = Cohort(2)
    since = now_iso()
    toxic(6000)
    cohort.approve()
    ok, _ = wait_for("SQS_UNKNOWN 으로 PENDING 재시도 대기",
                     lambda: unknown_count(cohort, since) >= 2, 90, 2)
    pend = psql(f"SELECT count(*) FROM notifications n JOIN users u ON u.id=n.user_id WHERE u.email LIKE 's7-{cohort.run}-%' "
                "AND n.sqs_publish_status='PENDING' AND n.next_attempt_at > now() + interval '30 seconds'")
    out["relay_unknown_pending_with_next_attempt"] = int(pend)
    check("(c) SQS 6초 지연: 발행이 UNKNOWN 으로 PENDING 에 남고 다음 시도가 약 1분 뒤", ok and int(pend) == 2, f"{pend}건")
    toxic(None)
    psql("UPDATE notifications SET created_at = created_at - interval '31 minutes', next_attempt_at = now() - interval '1 second' "
         f"WHERE sqs_publish_status = 'PENDING' AND user_id IN (SELECT id FROM users WHERE email LIKE 's7-{cohort.run}-%')")
    ok, took = wait_for("Relay 가 만료 처리", lambda: cohort.statuses() == {"EXPIRED": 2}, 60, 1)
    rows = [r for r in logs(since) if r.get("stage") == "relay" and r.get("result") == "EXPIRED"]
    row = psql(f"SELECT n.sqs_publish_status, n.sqs_published_at IS NULL, n.next_attempt_at IS NULL FROM notifications n "
               f"JOIN users u ON u.id=n.user_id WHERE u.email LIKE 's7-{cohort.run}-%' LIMIT 1")
    out["relay_expired_s"] = round(took, 1)
    check("(c) 기한 초과 PENDING 은 발행하지 않고 EXPIRED(발행 시각·다음 시도 비움)", ok and row == "EXPIRED|t|t" and (len(rows) == 2 or BACKEND != "rust"), row)
    # UNKNOWN 시도에서 큐가 실제로 받은 메시지는 Worker 가 곧바로 소비해 FCM 으로 보낸다(Java 는 Worker 가 없어 큐에 남는다).
    # 만료 처리는 "더 보내지 않는다"는 뜻일 뿐 이미 나간 한 번을 거두지는 못한다.
    if worker:
        wait_for("남은 메시지 소비", lambda: depth() == 0, 120, 2)
        out["unknown_attempt_fcm_sends"] = len(cohort.sends())
    else:
        out["unknown_attempt_queue_depth"] = depth()
    reset_world()

    if worker:
        # (a) 메시지 본문의 expiresAt 이 지난 경우: 형식은 맞고 기한만 지난 메시지를 큐에 직접 넣는다.
        since = now_iso()
        occurred = datetime.fromtimestamp(time.time() - 31 * 60, timezone.utc)
        expires = datetime.fromtimestamp(time.time() - 1 * 60, timezone.utc)
        fmt = lambda d: d.strftime("%Y-%m-%dT%H:%M:%S.%fZ")  # noqa: E731
        body = {"eventId": str(uuid.uuid4()), "notificationId": str(uuid.uuid4()), "userId": str(uuid.uuid4()),
                "type": "POST_APPROVED", "sourceType": "POST", "sourceId": str(uuid.uuid4()),
                "occurredAt": fmt(occurred), "expiresAt": fmt(expires)}
        sqs("SendMessage", {"QueueUrl": QUEUE, "MessageBody": json.dumps(body)})
        ok, _ = wait_for("만료 메시지 소비", lambda: depth() == 0, 150, 1)
        skipped = [r for r in logs(since) if r.get("stage") == "worker_skipped" and r.get("reason") == "EXPIRED"]
        check("(a) 본문 expiresAt 이 지난 메시지는 FCM 을 부르지 않고 EXPIRED 로 지운다", ok and len(skipped) == 1 and not fakes_requests_of("fcm_send"),
              f"EXPIRED 로그 {len(skipped)}")

        # (b) 재시도 대기 중 기한이 지난 경우: 429+Retry-After 로 숨은 메시지가 다시 오면 DB 시각 기준으로 만료.
        cohort = Cohort(1)
        fakes_set([{"contains": cohort.prefix, "status": 429, "headers": {"retry-after": "7"},
                    "body": fcm_error(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED")}])
        since = now_iso()
        cohort.approve()
        ok, _ = wait_for("첫 FCM 429", lambda: len(cohort.sends()) >= 1, 60, 1)
        ok2, _ = wait_for("재시도 대기 로그", lambda: any(r.get("stage") == "worker_retry_pending" for r in logs(since)), 30, 1)
        psql("UPDATE notifications SET created_at = created_at - interval '31 minutes' "
             f"WHERE user_id IN (SELECT id FROM users WHERE email LIKE 's7-{cohort.run}-%')")
        t_shift = time.monotonic()
        ok3, took = wait_for("두 번째 수신에서 만료로 삭제", lambda: depth() == 0, 150, 2)
        rows = logs(since)
        skipped = [r for r in rows if r.get("stage") == "worker_skipped" and r.get("reason") == "EXPIRED"]
        out["retry_then_expired_s_after_shift"] = round(time.monotonic() - t_shift, 1)
        check("(b) 429 재시도 대기 중 기한(DB 시각)을 넘기면 다음 수신에서 EXPIRED 로 지우고 다시 보내지 않는다",
              ok and ok2 and ok3 and len(skipped) == 1 and len(cohort.sends()) == 1 and depth(DLQ) == 0,
              f"FCM 요청 {len(cohort.sends())}, EXPIRED {len(skipped)}, 시각 이동 후 {out['retry_then_expired_s_after_shift']}초")
        reset_world()
    RESULTS["expire"] = out
    say(json.dumps(out, ensure_ascii=False))


def fakes_requests_of(service):
    return [r for r in fakes_requests() if r["service"] == service]


def scenario_sqsdelay(slow_count=20, unknown_total=5):
    say("== sqsdelay: SQS 응답 지연 (toxiproxy)")
    worker = BACKEND == "rust"
    out = {}
    # (a) 2초 지연(5초 제한 안): 느려질 뿐 모두 SQS_ACCEPTED.
    reset_world()
    cohort = Cohort(slow_count)
    since = now_iso()
    toxic(2000)
    t0 = time.monotonic()
    cohort.approve()
    ok, took = wait_for("느린 SQS 에서 전부 PUBLISHED", lambda: cohort.statuses().get("PUBLISHED") == slow_count, 300, 1)
    toxic(None)
    results = Counter(r.get("result") for r in logs(since) if r.get("stage") == "relay")
    out["delay_2s"] = {"count": slow_count, "published_all_s": round(time.monotonic() - t0, 1),
                       "per_notification_s": round((time.monotonic() - t0) / slow_count, 2), "relay_results": dict(results)}
    if worker:
        check("(a) 2초 지연: 모두 PUBLISHED, SQS_ACCEPTED 만(UNKNOWN 0)", ok and results.get("SQS_UNKNOWN", 0) == 0
              and results.get("SQS_ACCEPTED") == slow_count, str(dict(results)))
    else:
        check("(a) 2초 지연: 모두 PUBLISHED, 큐에 알림당 1건(중복 없음)", ok and depth() == slow_count, f"큐 {depth()}")
    if worker:
        wait_for("소비", lambda: depth() == 0 and len({t for _, t in cohort.sends()}) == slow_count, 300, 2)
        per = Counter(t for _, t in cohort.sends())
        out["delay_2s"]["fcm_duplicates"] = sum(v - 1 for v in per.values())
        check("(a) 2초 지연 + Worker: 기기마다 한 번씩 수락", len(per) == slow_count and out["delay_2s"]["fcm_duplicates"] == 0)

    # (b) 6초 지연(제한 밖): 응답이 늦어 UNKNOWN. 서버는 이미 받았으므로 다음 시도에서 같은 사건이 한 번 더 간다.
    reset_world()
    cohort = Cohort(unknown_total)
    since = now_iso()
    toxic(6000)
    t0 = time.monotonic()
    cohort.approve()
    ok, took = wait_for("모두 SQS_UNKNOWN", lambda: unknown_count(cohort, since) >= unknown_total, 120, 2)
    unknown_after = {"statuses": cohort.statuses(), "unknown_seen": unknown_count(cohort, since), "queue_depth": depth()}
    out["delay_6s_during"] = unknown_after
    check("(b) 6초 지연: 발행은 UNKNOWN 으로 PENDING, 그런데 큐에는 이미 들어가 있다(수락 불명의 실제 모습)",
          ok and unknown_after["statuses"].get("PENDING") == unknown_total
          and (unknown_after["queue_depth"] >= unknown_total or (worker and len(cohort.sends()) > 0)), json.dumps(unknown_after))
    toxic(None)
    ok, took = wait_for("지연 해소 뒤 PUBLISHED", lambda: cohort.statuses().get("PUBLISHED") == unknown_total, 200, 2)
    out["delay_6s_recovery_s"] = round(took, 1)
    if worker:
        wait_for("소비 안정", lambda: depth() == 0, 120, 2)
        time.sleep(5)
    rows = logs(since)
    received = Counter(r.get("notificationId") for r in rows if r.get("stage") == "worker_received")
    accepted_logs = Counter(r.get("notificationId") for r in rows if r.get("stage") == "relay" and r.get("result") == "SQS_ACCEPTED")
    unknown_logs = Counter(r.get("notificationId") for r in rows if r.get("stage") == "relay" and r.get("result") == "SQS_UNKNOWN")
    out["delay_6s_after"] = {"relay_unknown_per_notification": sorted(set(unknown_logs.values())),
                             "relay_accepted_per_notification": sorted(set(accepted_logs.values())), "statuses": cohort.statuses()}
    if worker:
        per = Counter(t for _, t in cohort.sends())
        out["delay_6s_after"].update(fcm_requests_per_device=sorted(set(per.values())), worker_received_per_notification=sorted(set(received.values())),
                                     failures=dict(Counter(r.get("stage") for r in rows if r.get("stage") in (
                                         "worker_poll_failed", "worker_processing_failed", "worker_visibility_failed"))))
        check("(b) 중복 범위: UNKNOWN 이 한 번이면 같은 알림이 큐에 2번, FCM 에도 최대 2번(원본 문서의 '중복 발행 가능' 범위)",
              set(per.values()) <= {1, 2} and max(per.values(), default=0) == 2, f"기기별 FCM 요청 {sorted(set(per.values()))}")
    else:
        # Java 는 Worker 가 없어 큐에 쌓인 메시지 수가 곧 발행 횟수다.
        out["delay_6s_after"]["queue_depth"] = depth()
        check("(b) Java: UNKNOWN 시도 뒤 재시도가 한 번 더 들어가 같은 알림이 큐에 2건씩", depth() == unknown_total * 2,
              f"큐 {depth()}건(알림 {unknown_total}건)")
    check("(b) 지연이 풀리면 모두 PUBLISHED", ok and cohort.statuses() == {"PUBLISHED": unknown_total}, str(cohort.statuses()))
    RESULTS["sqsdelay"] = out
    say(json.dumps(out, ensure_ascii=False))
    reset_world()


def scenario_dlq():
    say("== dlq: 429+Retry-After / 503 이 계속되면 10회 수신 뒤 DLQ (실시간 약 10분)")
    reset_world()
    timeout = int(os.environ.get("S7_DLQ_TIMEOUT", "1500"))
    worker = BACKEND == "rust" or os.environ.get("JAVA_NOTIFICATION_WORKER") == "true"
    if not worker:
        RESULTS["dlq"] = {"skipped": "Worker 없음(Java 기본). JAVA_NOTIFICATION_WORKER=true 로 띄우면 FCM 도달 불가 상태의 재수신·DLQ 를 본다"}
        say(RESULTS["dlq"]["skipped"])
        return
    cohort = Cohort(5)
    a, b, c, d, f = (cohort.token(i) for i in range(1, 6))
    if BACKEND == "rust":
        fakes_set([
            {"contains": a, "status": 429, "headers": {"retry-after": "7"}, "body": fcm_error(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED")},
            {"contains": b, "status": 429, "headers": {"retry-after": "120"}, "body": fcm_error(429, "RESOURCE_EXHAUSTED", "QUOTA_EXCEEDED")},
            {"contains": c, "status": 503, "body": fcm_error(503, "UNAVAILABLE")},
            {"contains": f, "status": 429, "headers": {"retry-after": "7"}, "body": fcm_error(429, "RESOURCE_EXHAUSTED")},  # details 없는 429
        ])
    since = now_iso()
    cohort.approve()
    ids = cohort.notification_ids()
    names = {"A": ids["1"], "B": ids["2"], "C": ids["3"], "D": ids["4"], "F": ids["5"]}
    t0 = time.monotonic()
    marks = {}

    def watch():
        dlq = depth(DLQ)
        marks["dlq_depth"] = dlq
        if dlq >= 2 and "two_in_dlq" not in marks:
            marks["two_in_dlq"] = time.monotonic() - t0
        return dlq >= 2 if BACKEND == "rust" else dlq >= 1

    wait_for("A·C 가 DLQ 로 이동", watch, timeout, 5)
    rows = logs(since)
    recv = {}
    for label, nid in names.items():
        times = sorted(r["_t"] for r in rows if r.get("stage") == "worker_received" and r.get("notificationId") == nid)
        counts = [r.get("receiveCount") for r in sorted((r for r in rows if r.get("stage") == "worker_received"
                                                          and r.get("notificationId") == nid), key=lambda r: r["_t"])]
        recv[label] = {"receives": len(times), "max_receive_count": max((int(c) for c in counts), default=0),
                       "intervals_s": [round(y - x, 1) for x, y in zip(times, times[1:])]}
    dlq_bodies = []
    got = sqs("ReceiveMessage", {"QueueUrl": DLQ, "MaxNumberOfMessages": 10, "VisibilityTimeout": 0, "WaitTimeSeconds": 0})
    for m in got.get("Messages", []):
        dlq_bodies.append(json.loads(m["Body"]).get("notificationId"))
    per = Counter(t for _, t in cohort.sends())
    last_recv = {k: max((r["_t"] for r in rows if r.get("stage") == "worker_received" and r.get("notificationId") == v), default=None)
                 for k, v in names.items()}
    metrics = {"elapsed_s": round(time.monotonic() - t0, 1), "two_in_dlq_s": round(marks.get("two_in_dlq", -1), 1),
               "receives": recv, "fcm_requests": {k: per.get(t, 0) for k, t in zip("ABCDF", (a, b, c, d, f))},
               "dlq_notification_ids": [next((k for k, v in names.items() if v == n), n) for n in dlq_bodies],
               "queue_depth": depth(), "dlq_depth": depth(DLQ), "statuses": cohort.statuses(),
               "retry_after_logged": dict(Counter(
                   next((k for k, v in names.items() if v == r.get("notificationId")), "?") + ":" + str(r.get("retryAfterSeconds"))
                   for r in rows if r.get("stage") == "worker_retry_pending"))}
    if last_recv.get("A"):
        metrics["a_last_receive_to_dlq_note"] = "DLQ 도착 시각은 마지막(10번째) 수신의 숨김이 끝난 직후로 본다"
    RESULTS["dlq"] = metrics
    say(json.dumps(metrics, ensure_ascii=False))
    if BACKEND == "rust":
        check("A(429, Retry-After 7): 정확히 10회 수신 뒤 DLQ, FCM 요청도 10번(429 는 SDK 가 재시도하지 않는다)",
              recv["A"]["max_receive_count"] == 10 and recv["A"]["receives"] == 10 and metrics["fcm_requests"]["A"] == 10 and "A" in metrics["dlq_notification_ids"],
              json.dumps(recv["A"]))
        check("C(503, Retry-After 없음): 10회 수신 뒤 DLQ, 수신마다 SDK 가 5번 보낸다(요청 50)",
              recv["C"]["max_receive_count"] == 10 and metrics["fcm_requests"]["C"] == 50 and "C" in metrics["dlq_notification_ids"],
              json.dumps(recv["C"]))
        check("수신 간격: 재시도 대기 ≥ 61초(최소 60초 + 1) (A)", all(x >= 60 for x in recv["A"]["intervals_s"]), str(recv["A"]["intervals_s"]))
        check("B(Retry-After 120): 간격이 121초 안팎이라 같은 시간에 수신이 훨씬 적다", 1 <= recv["B"]["receives"] < 10,
              json.dumps(recv["B"]))
        check("D(대조군): 한 번 수락, 한 번 수신", metrics["fcm_requests"]["D"] == 1 and recv["D"]["receives"] == 1, json.dumps(recv["D"]))
        check("F(details 없는 429): 영구 실패로 한 번만 보내고 지운다(재시도·DLQ 없음)",
              metrics["fcm_requests"]["F"] == 1 and recv["F"]["receives"] == 1 and "F" not in metrics["dlq_notification_ids"],
              json.dumps(recv["F"]))
        check("DLQ 이동 뒤에도 알림 행은 PUBLISHED(영구 격리는 큐 쪽 일)", cohort.statuses().get("PUBLISHED") == 5, str(cohort.statuses()))
    else:
        check("Java: 도달할 수 없는 FCM 에서 같은 메시지가 DLQ 로 이동", len(dlq_bodies) >= 1, json.dumps(recv))
    reset_world()


SCENARIOS = {
    "redrive": scenario_redrive,
    "burst": lambda: scenario_burst(int(os.environ.get("S7_BURST", "300"))),
    "fcm503": scenario_fcm503,
    "expire": scenario_expire,
    "sqsdelay": scenario_sqsdelay,
    "dlq": scenario_dlq,
}


def main():
    global BACKEND
    BACKEND = backend()
    names = sys.argv[1:] or ["all"]
    if names == ["all"]:
        names = ["redrive", "burst", "fcm503", "expire", "sqsdelay", "dlq"]
    RESULTS.update(backend=BACKEND, started=now_iso(), scenarios=names)
    say(f"백엔드 {BACKEND}, 시나리오 {names}")
    try:
        for name in names:
            if name not in SCENARIOS:
                raise Failure(f"알 수 없는 시나리오: {name} (가능: {', '.join(SCENARIOS)})")
            t = time.monotonic()
            SCENARIOS[name]()
            RESULTS.setdefault("durations_s", {})[name] = round(time.monotonic() - t, 1)
    finally:
        try:
            toxic(None)
        except Exception:  # noqa: BLE001 - 정리 실패는 결과를 가리지 않는다
            pass
        out = Path(os.environ.get("S7_OUT", COMPOSE_DIR / ".state/s7" / f"{BACKEND}-{time.strftime('%Y%m%d-%H%M%S')}.json"))
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(RESULTS, ensure_ascii=False, indent=2))
        say(f"결과: {out}")
    failed = [c for c in RESULTS.get("checks", []) if not c["ok"]]
    say(f"통과 {len(RESULTS.get('checks', [])) - len(failed)} / 실패 {len(failed)}")
    for c in failed:
        say(f"  실패: {c['name']} {c['detail']}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"[s7] 실패: {failure}", file=sys.stderr)
        sys.exit(1)
