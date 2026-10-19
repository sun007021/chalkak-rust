"""L5(안정성 시험)의 공통 도구: 스택 제어, 표본 수집, edge 접근 로그·런타임 로그 분석, 부하 발생기 실행, 결과 기록.

l5.py(S1~S6·S8)와 s9.py(S9)가 쓴다. 표준 라이브러리만 쓴다. e2e_image.py 의 HTTPS 클라이언트와 헬퍼를 그대로 가져온다.
"""

import base64
import hashlib
import hmac
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # python3 -I 는 스크립트 폴더를 경로에 넣지 않는다
import e2e_image as e  # noqa: E402
from e2e_image import Failure, api, http  # noqa: E402

COMPOSE_DIR = e.COMPOSE_DIR
REPO = e.REPO_ROOT
HOST_IP = e.HOST_IP
PROJECT = "chalkak-local-aws"
STATE = COMPOSE_DIR / ".state" / "l5"
LOADGEN = Path(os.environ.get("LOADGEN", REPO / "target/release/chalkak-loadgen"))

ENV_FILE = COMPOSE_DIR / "env/local-aws.env"
CALLBACK_SECRET = "local-aws-callback-secret-with-enough-length"  # env/local-aws.env 의 IMAGE_PROCESSOR_CALLBACK_SECRET
DB_URL = f"postgres://chalkak:local-aws-password@127.0.0.1:{os.environ.get('POSTGRES_PORT', '15440')}/chalkak"
EDGE_HTTP = f"127.0.0.1:{os.environ.get('EDGE_HTTP_PORT', '18081')}"
BACKEND_DEBUG = f"127.0.0.1:{os.environ.get('BACKEND_DEBUG_PORT', '18080')}"
S3_DEBUG = f"127.0.0.1:{os.environ.get('AWS_DEBUG_PORT', '4566')}"
TOXI = f"http://127.0.0.1:{os.environ.get('TOXIPROXY_PORT', '8474')}"
FAULT = f"http://127.0.0.1:{os.environ.get('FAULT_PROXY_PORT', '18090')}"
FAKES = f"http://127.0.0.1:{os.environ.get('FAKES_PORT', '19000')}"
IMAGE_QUEUE = "http://localhost:4566/000000000000/chalkak-image-processing"
PUSH_QUEUE = "http://localhost:4566/000000000000/chalkak-dev-push"
# 시드 크기. run(혼합 부하)에도 같은 값을 줘야 요청 경로가 시드와 맞는다.
# loadgen stress 는 시드의 기본 크기(SeedSize::default)를 가정한다. 같은 값을 쓴다.
SEED_ARGS = ["--users", "300", "--days", "60", "--posts-per-topic", "150", "--likes-per-post", "12", "--notifications-per-user", "6"]
SEED_USERS, SEED_PPT = 300, 150
DB_KV = f"host=127.0.0.1 port={os.environ.get('POSTGRES_PORT', '15440')} user=chalkak password=local-aws-password dbname=chalkak"  # stress 는 키=값 형식
POOL_SIZE = 10

_plain = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def say(message):
    print(f"[l5 {time.strftime('%H:%M:%S')}] {message}", flush=True)


def access_secret_hex():
    for line in ENV_FILE.read_text().splitlines():
        if line.startswith("ACCESS_TOKEN_SECRET="):
            return line.split("=", 1)[1].strip()
    raise Failure("ACCESS_TOKEN_SECRET 이 env 에 없다")


# ---- 결과 기록 -----------------------------------------------------------------------------------
class Results:
    """한 시나리오의 결과: 통과 기준(checks), 수치(metrics), 메모(notes)."""

    def __init__(self, backend):
        self.backend = backend
        self.checks = []
        self.metrics = {}
        self.notes = []

    def check(self, name, ok, detail=""):
        self.checks.append({"name": name, "ok": bool(ok), "detail": str(detail)})
        say(f"{'통과' if ok else '실패'}: {name}{(' - ' + str(detail)) if detail else ''}")
        return bool(ok)

    def metric(self, key, value):
        self.metrics[key] = value

    def note(self, text):
        self.notes.append(text)
        say(f"참고: {text}")

    def failed(self):
        return [c for c in self.checks if not c["ok"]]


# ---- 프로세스 실행 -------------------------------------------------------------------------------
def sh(cmd, timeout=None, check=False, input_text=None):
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, input=input_text)
    if check and result.returncode != 0:
        raise Failure(f"{' '.join(map(str, cmd))[:200]} -> {result.returncode}: {result.stderr.strip()[:400]}")
    return result


def container(service):
    return f"{PROJECT}-{service}-1"


def psql(statement, timeout=60):
    """DB 컨테이너에서 직접 psql(compose exec 보다 가볍다)."""
    result = sh(["docker", "exec", container("postgres"), "psql", "-q", "-tA", "-U", "chalkak", "-d", "chalkak", "-v", "ON_ERROR_STOP=1",
                 "-c", statement], timeout=timeout)
    if result.returncode != 0:
        raise Failure(f"psql 실패: {result.stderr.strip()[:400]}")
    return result.stdout.strip()


def rows(statement):
    return [line.split("|") for line in psql(statement).splitlines() if line]


def raw(method, url, payload=None, headers=None, timeout=30):
    """프록시 없이 평문 HTTP. (상태, 본문(JSON이면 파싱))."""
    body = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with _plain.open(request, timeout=timeout) as response:
            data = response.read()
            status = response.status
    except urllib.error.HTTPError as error:
        data, status = error.read(), error.code
    except (urllib.error.URLError, OSError) as error:
        return 0, f"연결 실패: {error}"
    try:
        return status, (json.loads(data) if data else None)
    except ValueError:
        return status, data.decode(errors="replace")


# ---- 스택 -----------------------------------------------------------------------------------------
class Stack:
    def __init__(self, backend):
        self.backend = backend
        self.env = {**os.environ, "HOST_IP": HOST_IP, "ESM_CONCURRENCY": os.environ.get("ESM_CONCURRENCY", "4")}

    def compose_cmd(self, *args):
        return ["docker", "compose", "-f", "compose.yaml", "-f", "compose.l5.yaml", "--profile", "chaos", "--profile", "rust", "--profile", "java",
                *args]

    def compose(self, *args, timeout=600, check=True):
        result = subprocess.run(self.compose_cmd(*args), cwd=COMPOSE_DIR, env=self.env, capture_output=True, text=True, timeout=timeout)
        if check and result.returncode != 0:
            raise Failure(f"docker compose {' '.join(args)} 실패: {result.stderr.strip()[-600:]}")
        return result

    def down(self):
        self.compose("down", "-v", "--remove-orphans", check=False)

    def up(self, fresh=True):
        """스택을 새로 올린다(DB·큐를 비운다). 백엔드는 rust|java 하나만 뜬다."""
        if fresh:
            self.down()
        services = ["postgres", "aws", "edge", "imds-mock", "fakes", "fault-proxy", "toxiproxy", "lambda-image", "esm-poller", self.backend]
        self.compose("up", "-d", "--wait", "--wait-timeout", "420", *services, timeout=900)
        self.wait_ready()

    def wait_ready(self):
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            status, _ = raw("GET", f"http://{EDGE_HTTP}/actuator/health", timeout=5)
            if status == 200:
                return
            time.sleep(1)
        raise Failure("백엔드가 edge 를 통해 UP 이 되지 않는다")

    def pid(self):
        out = sh(["docker", "inspect", "-f", "{{.State.Pid}}", container(self.backend)]).stdout.strip()
        return int(out) if out.isdigit() and int(out) > 0 else None

    def started_at(self):
        return sh(["docker", "inspect", "-f", "{{.State.StartedAt}}", container(self.backend)]).stdout.strip()

    def restart_count(self):
        out = sh(["docker", "inspect", "-f", "{{.RestartCount}}", container(self.backend)]).stdout.strip()
        return int(out) if out.isdigit() else -1

    def toxiproxy_ip(self):
        fmt = "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}"
        return sh(["docker", "inspect", "-f", fmt, container("toxiproxy")]).stdout.strip()


# ---- 시드 · 회원 · 토큰 -----------------------------------------------------------------------------
def seed(stack):
    """결정적 시드(회원 300, 주제 60일, 주제당 게시물 150). Rust·Java 모두 같은 데이터가 된다."""
    result = sh([str(LOADGEN), "seed", "--db", DB_URL, *SEED_ARGS], timeout=600)
    if result.returncode != 0:
        raise Failure(f"시드 실패: {result.stdout[-300:]} {result.stderr[-300:]}")
    say(result.stdout.strip().splitlines()[-1])


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def mint_token(user_id, scope="USER", ttl=3600):
    """로그인 없이 액세스 토큰을 만든다(harness/loadgen/src/token.rs 와 같은 HS256 클레임)."""
    now = int(time.time())
    header = b64url(b'{"typ":"JWT","alg":"HS256"}')
    claims = {"iss": "chalkak-backend", "aud": ["chalkak-access"], "sub": user_id, "iat": now, "exp": now + ttl,
              "jti": uuid.uuid4().hex, "purpose": "ACCESS", "scope": scope}
    payload = b64url(json.dumps(claims, separators=(",", ":")).encode())
    key = bytes.fromhex(access_secret_hex())
    signature = hmac.new(key, f"{header}.{payload}".encode(), hashlib.sha256).digest()
    return f"{header}.{payload}.{b64url(signature)}"


def seed_user_id(n):
    return f"01990000-0000-7000-8000-{n:012x}"


def mint_users(count, tag):
    """게시물을 쓸 새 회원 count 명을 SQL 로 만들고 [(id, 토큰)]을 돌려준다. 회원마다 오늘 주제에 게시물 하나만 쓸 수 있어서 시험마다 새로 만든다."""
    base = (int(time.time() * 1000) << 8) % (1 << 44)
    ids = [f"01990000-0000-7000-8a{tag % 256:02x}-{(base + i) % (1 << 48):012x}" for i in range(count)]
    values = ",".join(
        f"('{uid}', 'l5-{tag}-{uid[-8:]}@test.chalkak', 'ACTIVE', 'chalkak/signatures/l5/{uid}/o.png', 'chalkak/signatures/l5/{uid}/t.png')"
        for uid in ids)
    psql(f"INSERT INTO users (id, email, status, signature_original_storage_key, signature_thumbnail_storage_key) VALUES {values}")
    return [(uid, mint_token(uid)) for uid in ids]


_image_cache = {}


def webp():
    """게시물 시험 이미지(EXIF 가 든 1600x1200 WebP)."""
    if "webp" not in _image_cache:
        _image_cache["webp"] = (REPO / "harness/loadgen/fixtures/post.webp").read_bytes()
    return _image_cache["webp"]


# ---- 게시물 업로드 흐름(앱이 하는 일) ----------------------------------------------------------------
def todays_topic():
    return e.todays_topic_id()


def new_upload(token):
    """업로드 URL 발급. (uploadId, uploadUrl, contentType)."""
    _, body = api("POST", "/api/v1/posts/uploads", token=token)
    return body["uploadId"], body["uploadUrl"], body.get("contentType", "image/webp")


def upload_status(upload_id):
    rows_ = rows(f"SELECT status, rejection_reason FROM post_image_uploads WHERE id = '{upload_id}'")
    return (rows_[0][0], rows_[0][1] or None) if rows_ else (None, None)


def wait_upload_closed(upload_id, timeout):
    """업로드가 ISSUED 를 벗어날 때까지. (상태, 걸린 초) — 시간 초과면 (ISSUED, timeout)."""
    started = time.monotonic()
    while time.monotonic() - started < timeout:
        status, _ = upload_status(upload_id)
        if status and status != "ISSUED":
            return status, time.monotonic() - started
        time.sleep(0.5)
    return "ISSUED", float(timeout)


def canary_post(topic_id, user, timeout=120):
    """앱 한 대가 하는 일: 업로드 URL → WebP PUT → 게시물 작성 → 처리 콜백으로 PENDING. 단계별 시간을 돌려준다(실패해도 예외 대신 결과로)."""
    _, token = user
    result = {"ok": False}
    started = time.monotonic()
    try:
        upload_id, url, content_type = new_upload(token)
        result["upload_id"] = upload_id
        e.presigned_put(url, webp(), content_type)
        put_at = time.monotonic()
        result["put_s"] = round(put_at - started, 2)
        _, post = api("POST", "/api/v1/posts", {"topicId": topic_id, "photoUploadId": upload_id, "title": f"l5 {upload_id[-6:]}"}, token=token)
        result["post_id"] = post.get("postId") or post.get("id")
        result["created_status"] = post.get("moderationStatus")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            _, today = api("GET", "/api/v1/posts/today", token=token)
            if today.get("moderationStatus") not in ("VALIDATING", None):
                result["final_status"] = today["moderationStatus"]
                break
            time.sleep(1)
        result["done_s"] = round(time.monotonic() - started, 2)
        result["callback_s"] = round(time.monotonic() - put_at, 2)
        result["ok"] = result.get("final_status") == "PENDING"
        if not result["ok"]:
            result["error"] = f"최종 상태 {result.get('final_status')}"
    except Failure as failure:
        result["error"] = str(failure)[:300]
    except Exception as error:  # noqa: BLE001 - 시험 스레드가 죽지 않게
        result["error"] = f"{type(error).__name__}: {error}"[:300]
    return result


# ---- SQS (LocalStack 디버그 포트) --------------------------------------------------------------------
SQS_AUTH = "AWS4-HMAC-SHA256 Credential=test/20260101/ap-northeast-2/sqs/aws4_request, SignedHeaders=host, Signature=x"


def sqs(action, payload):
    status, body = raw("POST", f"http://{S3_DEBUG}/", payload, {
        "X-Amz-Target": f"AmazonSQS.{action}", "Content-Type": "application/x-amz-json-1.0", "Authorization": SQS_AUTH})
    if status != 200:
        raise Failure(f"SQS {action} -> {status}: {body}")
    return body


def queue_counts(queue_url=IMAGE_QUEUE):
    attrs = sqs("GetQueueAttributes", {"QueueUrl": queue_url, "AttributeNames": ["All"]})["Attributes"]
    return {"visible": int(attrs["ApproximateNumberOfMessages"]), "in_flight": int(attrs["ApproximateNumberOfMessagesNotVisible"]),
            "delayed": int(attrs["ApproximateNumberOfMessagesDelayed"]), "visibility": int(attrs["VisibilityTimeout"])}


def queue_depth(queue_url=IMAGE_QUEUE):
    c = queue_counts(queue_url)
    return c["visible"] + c["in_flight"] + c["delayed"]


def set_visibility(seconds, queue_url=IMAGE_QUEUE, settle=True):
    """큐의 visibility timeout 을 바꾼다. LocalStack 은 수신 요청이 시작될 때의 값을 쓰므로, 이미 긴 폴링(20초)에 들어간 poller 요청이 끝날 때까지 기다린다."""
    sqs("SetQueueAttributes", {"QueueUrl": queue_url, "Attributes": {"VisibilityTimeout": str(seconds)}})
    if settle:
        time.sleep(22)


# ---- 장애 주입 -------------------------------------------------------------------------------------
def toxic_clear(proxy="postgres"):
    status, body = raw("GET", f"{TOXI}/proxies/{proxy}/toxics")
    if status == 200 and isinstance(body, list):
        for toxic in body:
            raw("DELETE", f"{TOXI}/proxies/{proxy}/toxics/{toxic['name']}")
    raw("POST", f"{TOXI}/proxies/{proxy}", {"enabled": True})


def toxic_latency(ms, proxy="postgres", jitter=0, stream="downstream"):
    status, body = raw("POST", f"{TOXI}/proxies/{proxy}/toxics", {
        "name": f"l5-latency-{stream}", "type": "latency", "stream": stream, "attributes": {"latency": ms, "jitter": jitter}})
    if status >= 300:
        raise Failure(f"toxiproxy 지연 지정 실패 {status}: {body}")


def proxy_enabled(enabled, proxy="postgres"):
    status, body = raw("POST", f"{TOXI}/proxies/{proxy}", {"enabled": bool(enabled)})
    if status >= 300:
        raise Failure(f"toxiproxy enabled={enabled} 실패 {status}: {body}")


def fault_rules(rules):
    status, body = raw("PUT", f"{FAULT}/rules", rules)
    if status != 200:
        raise Failure(f"fault-proxy 규칙 지정 실패 {status}: {body}")


def fault_stats():
    return raw("GET", f"{FAULT}/stats")[1]


def reset_faults():
    try:
        toxic_clear("postgres")
        toxic_clear("aws")
        fault_rules([])
    except Exception as error:  # noqa: BLE001
        say(f"장애 주입 정리 실패: {error}")


# ---- 관측: edge 접근 로그 ----------------------------------------------------------------------------
def percentile(values, q):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * q))]


LOADGEN_HOSTS = ("chalkak", EDGE_HTTP)  # loadgen 은 Host: chalkak 을 보낸다. 평문 8081 로 들어온 요청은 이 이름으로 접근 로그에 남는다.


def edge_entries(since, until=None):
    """edge 접근 로그(Caddy JSON). since/until 은 epoch 초. [{ts, host, method, uri, status, ms}]"""
    result = sh(["docker", "logs", "--since", f"{since:.3f}", container("edge")], timeout=300)
    out = []
    for line in (result.stdout + "\n" + result.stderr).splitlines():
        if '"handled request"' not in line:
            continue
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if until is not None and row["ts"] > until:
            continue
        if row["ts"] < since:
            continue
        request = row.get("request", {})
        out.append({"ts": row["ts"], "host": request.get("host", ""), "method": request.get("method", ""), "uri": request.get("uri", ""),
                    "status": row.get("status", 0), "ms": row.get("duration", 0) * 1000})
    return out


def edge_summary(entries, host=None, uri_prefix=None):
    hosts = None if host is None else ((host,) if isinstance(host, str) else tuple(host))
    sel = [x for x in entries if (hosts is None or x["host"] in hosts) and (uri_prefix is None or x["uri"].startswith(uri_prefix))]
    ms = [x["ms"] for x in sel]
    classes = {}
    for x in sel:
        key = f"{x['status'] // 100}xx"
        classes[key] = classes.get(key, 0) + 1
    return {"count": len(sel), "status": classes, "p50_ms": round(percentile(ms, 0.5), 2) if ms else None,
            "p95_ms": round(percentile(ms, 0.95), 2) if ms else None, "p99_ms": round(percentile(ms, 0.99), 2) if ms else None,
            "max_ms": round(max(ms), 2) if ms else None}


# ---- 관측: 서버 런타임 로그(chalkak.runtime) --------------------------------------------------------------
def parse_iso(text):
    text = re.sub(r"(\.\d{6})\d+", r"\1", text)
    return datetime.fromisoformat(text).timestamp()


def runtime_log(backend, since):
    """1분마다 남는 런타임 상태 로그. Rust: 컨테이너 로그, Java: dev 프로필의 JSON 파일 로그. [{ts, threads, hikariActive, hikariPending, rssBytes?, heapUsedBytes?}]"""
    if backend == "java":
        text = sh(["docker", "exec", container("java"), "sh", "-c", "grep -h '\"type\":\"runtime\"' /opt/chalkak/logs/application.log || true"],
                  timeout=60).stdout
    else:
        result = sh(["docker", "logs", "--since", f"{since:.3f}", container(backend)], timeout=120)
        text = result.stdout + "\n" + result.stderr
    out = []
    for line in text.splitlines():
        if '"type":"runtime"' not in line:
            continue
        try:
            row = json.loads(line)
        except ValueError:
            continue
        ts = parse_iso(row["@timestamp"])
        if ts >= since:
            out.append({**{k: v for k, v in row.items() if isinstance(v, (int, float))}, "ts": ts})
    return out


def backend_error_lines(backend, since, limit=20):
    """백엔드 로그의 ERROR 줄(수와 앞 몇 개)."""
    result = sh(["docker", "logs", "--since", f"{since:.3f}", container(backend)], timeout=120)
    lines = [ln for ln in (result.stdout + "\n" + result.stderr).splitlines() if '"level":"ERROR"' in ln or " ERROR " in ln[:60]]
    return len(lines), [ln[:300] for ln in lines[:limit]]


# ---- 표본 수집 ------------------------------------------------------------------------------------
def read_proc(pid):
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        rss = int(re.search(r"VmRSS:\s+(\d+)", status).group(1))
        threads = int(re.search(r"Threads:\s+(\d+)", status).group(1))
        fds = len(os.listdir(f"/proc/{pid}/fd"))
        return rss, threads, fds
    except (OSError, AttributeError, ValueError):
        return None


class Sampler:
    """백엔드 프로세스(/proc)와 DB(pg_stat_activity)를 주기적으로 읽어 표본으로 남긴다. 파일에도 이어 쓴다(CSV)."""

    FIELDS = ["t", "rss_mb", "threads", "fds", "pg_app", "pg_active", "pg_idle_in_tx", "load1"]

    def __init__(self, stack, period=5.0, pg_every=2, csv_path=None):
        self.stack = stack
        self.period = period
        self.pg_every = pg_every
        self.samples = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._csv = open(csv_path, "a", buffering=1) if csv_path else None
        self._toxi_ip = None
        if self._csv and self._csv.tell() == 0:
            self._csv.write(",".join(self.FIELDS) + "\n")

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=30)
        if self._csv:
            self._csv.close()
        return self.samples

    def _pg(self):
        if self._toxi_ip is None:
            self._toxi_ip = self.stack.toxiproxy_ip()
        query = ("SELECT count(*), count(*) FILTER (WHERE state = 'active'), count(*) FILTER (WHERE state LIKE 'idle in transaction%' AND now() - state_change > interval '5 seconds') "
                 f"FROM pg_stat_activity WHERE datname = 'chalkak' AND client_addr = '{self._toxi_ip}'")
        out = sh(["docker", "exec", container("postgres"), "psql", "-q", "-tA", "-F,", "-U", "chalkak", "-d", "chalkak", "-c", query], timeout=30).stdout.strip()
        try:
            return [int(x) for x in out.split(",")]
        except ValueError:
            return None

    def _run(self):
        tick = 0
        while not self._stop.is_set():
            started = time.time()
            sample = {"t": round(started, 1)}
            pid = self.stack.pid()
            proc = read_proc(pid) if pid else None
            if proc:
                sample.update(rss_mb=round(proc[0] / 1024, 1), threads=proc[1], fds=proc[2])
            if tick % self.pg_every == 0:
                pg = self._pg()
                if pg:
                    sample.update(pg_app=pg[0], pg_active=pg[1], pg_idle_in_tx=pg[2])
            try:
                sample["load1"] = float(Path("/proc/loadavg").read_text().split()[0])
            except OSError:
                pass
            self.samples.append(sample)
            if self._csv:
                self._csv.write(",".join("" if sample.get(f) is None else str(sample[f]) for f in self.FIELDS) + "\n")
            tick += 1
            self._stop.wait(max(0.1, self.period - (time.time() - started)))


def slope_per_hour(points):
    """최소제곱 기울기(값/시간). points: [(epoch초, 값)]."""
    if len(points) < 3:
        return 0.0
    n = len(points)
    mean_t = sum(p[0] for p in points) / n
    mean_v = sum(p[1] for p in points) / n
    num = sum((p[0] - mean_t) * (p[1] - mean_v) for p in points)
    den = sum((p[0] - mean_t) ** 2 for p in points)
    return (num / den) * 3600 if den else 0.0


def series(samples, key, skip=0.0):
    pts = [(s["t"], s[key]) for s in samples if s.get(key) is not None]
    return pts[int(len(pts) * skip):]


def trend_verdict(points, abs_floor, rel=0.05):
    """누수 판정(docs/perf/soak-24h.md 와 같은 기준): 기울기가 시간당 평균의 rel 또는 abs_floor 중 큰 값 이하, 마지막 구간이 앞 절반 최댓값을 넘지 않거나 그 차이가 abs_floor 이하."""
    if len(points) < 5:
        return {"ok": True, "reason": "표본 부족", "slope_per_hour": 0.0, "mean": 0.0, "first_half_max": 0.0, "last": 0.0}
    values = [p[1] for p in points]
    mean = sum(values) / len(values)
    slope = slope_per_hour(points)
    half = values[: len(values) // 2]
    tail = values[-max(3, len(values) // 10):]
    last = sum(tail) / len(tail)
    limit = max(abs(mean) * rel, abs_floor)
    ok = slope <= limit or last <= max(half) + abs_floor
    return {"ok": ok, "slope_per_hour": round(slope, 3), "mean": round(mean, 2), "limit_per_hour": round(limit, 3),
            "first_half_max": round(max(half), 2), "last": round(last, 2)}


# ---- 부하 발생기 ------------------------------------------------------------------------------------
def loadgen_run(args, log_path):
    """chalkak-loadgen 을 백그라운드로 띄운다. (Popen, 로그 파일)."""
    logfile = open(log_path, "w")
    proc = subprocess.Popen([str(LOADGEN), *args], stdout=logfile, stderr=subprocess.STDOUT, cwd=REPO)
    return proc, logfile


def load_json(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return None


def mix_args(stack, rps, duration, out, label, conns=32, idle=20, warmup="20s", interval="60", pid=None):
    args = ["run", "--base", EDGE_HTTP, "--secret", access_secret_hex(), "--rps", str(rps), "--conns", str(conns), "--idle-conns", str(idle),
            "--duration", str(duration), "--warmup", str(warmup), "--interval", str(interval), "--db", DB_URL, "--label", label, "--out", str(out),
            *SEED_ARGS]
    if pid:
        args += ["--pid", str(pid)]
    return args


def uploads_args(rate, duration, out, label, users=100, conns=32, timeout="7m", poll="500ms"):
    return ["uploads", "--base", EDGE_HTTP, "--s3", S3_DEBUG, "--secret", access_secret_hex(), "--db", DB_URL, "--rate", str(rate),
            "--duration", str(duration), "--users", str(users), "--conns", str(conns), "--timeout", str(timeout), "--poll", str(poll),
            "--label", label, "--out", str(out)]


def wait_all(procs, timeout):
    """프로세스들이 끝나길 기다린다(시간 초과면 종료). 종료 코드 목록."""
    deadline = time.monotonic() + timeout
    codes = []
    for proc in procs:
        remaining = max(1, deadline - time.monotonic())
        try:
            codes.append(proc.wait(timeout=remaining))
        except subprocess.TimeoutExpired:
            proc.kill()
            codes.append(-9)
    return codes


# ---- Lambda · poller 로그 -------------------------------------------------------------------------
def poller_rows(since):
    """esm-poller 로그(JSON 한 줄씩)를 dict 목록으로."""
    result = sh(["docker", "logs", "--since", f"{since:.3f}", container("esm-poller")], timeout=120)
    out = []
    for line in (result.stdout + "\n" + result.stderr).splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict) and row.get("event"):
            out.append(row)
    return out


def poller_events(since):
    """esm-poller 이벤트 이름별 횟수."""
    counts = {}
    for row in poller_rows(since):
        counts[row["event"]] = counts.get(row["event"], 0) + 1
    return counts


def lambda_log_events(since, names=("image_processing_failed", "image_processing_abandoned", "callback_permanently_refused")):
    """lambda-image 컨테이너(복제본 전부)의 로그에서 지정한 event 횟수."""
    counts = {n: 0 for n in names}
    ids = sh(["docker", "ps", "-q", "--filter", f"label=com.docker.compose.service=lambda-image"]).stdout.split()
    for cid in ids:
        result = sh(["docker", "logs", "--since", f"{since:.3f}", cid], timeout=120)
        text = result.stdout + result.stderr
        for name in names:
            counts[name] += text.count(f'\\"event\\": \\"{name}\\"') + text.count(f'"event": "{name}"')
    return counts


# ---- 콜백 서명(Lambda 의 callback.py 와 같은 규칙) ---------------------------------------------------
def signed_callback(path_suffix, body, environment="dev", timestamp=None, secret=CALLBACK_SECRET, base_path="/internal/v1/post-image-processing",
                    tamper=False):
    """백엔드 내부 콜백을 HMAC 서명해 보낸다. (상태, 본문). body 가 None 이면 본문 없는 POST(upload-urls)."""
    encoded = b"" if body is None else json.dumps(body, ensure_ascii=True, separators=(",", ":")).encode()
    timestamp = str(int(time.time())) if timestamp is None else str(timestamp)
    path = f"{base_path}/{path_suffix}"
    payload = f"{timestamp}\nPOST\n{path}\n{hashlib.sha256(encoded).hexdigest()}\n{environment}".encode()
    signature = "v1=" + hmac.new(secret.encode(), payload, hashlib.sha256).hexdigest()
    if tamper:
        signature = signature[:-1] + ("0" if signature[-1] != "0" else "1")
    headers = {"X-Chalkak-Callback-Timestamp": timestamp, "X-Chalkak-Callback-Signature": signature}
    if body is not None:
        headers["Content-Type"] = "application/json; charset=utf-8"
    status, data = http("POST", e.API + path, encoded if body is not None else b"", headers)
    try:
        return status, json.loads(data) if data else None
    except ValueError:
        return status, data.decode(errors="replace")


def save_result(results, scenario):
    STATE.mkdir(parents=True, exist_ok=True)
    out = STATE / f"{results.backend}-{scenario}-{time.strftime('%Y%m%d-%H%M%S')}.json"
    out.write_text(json.dumps({"backend": results.backend, "scenario": scenario, "checks": results.checks, "metrics": results.metrics,
                               "notes": results.notes}, ensure_ascii=False, indent=2))
    return out
