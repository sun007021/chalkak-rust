#!/usr/bin/env python3
"""로컬 AWS 재현 환경 종단 확인: 이미지 업로드 → S3 이벤트 → SQS → Lambda → 콜백 204 → cdn 로 결과 객체 GET 200.

기기 역할을 한다: 하네스의 가짜 ID Token(harness/keys)으로 가입하고, 서버가 준 presigned PUT URL 로 S3 에 직접 올리고,
cdn 주소로 결과를 읽는다. 표준 라이브러리만 쓴다(이미지 생성은 lambda-image 컨테이너의 Pillow, 토큰 서명은 openssl).

확인하는 것
  1. 회원가입 사인: 가짜 Google ID Token → 사인 업로드 URL → PNG PUT → Lambda 처리 → 가입 → 사인 original·thumbnail 이 cdn 에서 200
  2. 게시물 이미지: 업로드 URL → WebP PUT → 게시물 작성(VALIDATING) → Lambda 처리·콜백 → 게시물 PENDING, 사진 original·thumbnail cdn 200
     그리고 서버가 Lambda 에 준 결과 업로드 URL 이 HTTPS 여야 한다는 계약(Lambda 가 거부)이 로컬에서도 지켜진다.

환경변수: HOST_IP(기본 127.0.0.1), COMPOSE_DIR(기본 이 파일의 ../), TIMEOUT_SECONDS(기본 90)
"""

import base64
import hashlib
import json
import os
import socket
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

HOST_IP = os.environ.get("HOST_IP", "127.0.0.1")
COMPOSE_DIR = Path(os.environ.get("COMPOSE_DIR", Path(__file__).resolve().parent.parent))
REPO_ROOT = COMPOSE_DIR.parent
CA_FILE = COMPOSE_DIR / ".state/caddy/data/caddy/pki/authorities/local/root.crt"
KEY_FILE = REPO_ROOT / "harness/keys/oidc-test-private.pem"
TIMEOUT = float(os.environ.get("TIMEOUT_SECONDS", "90"))
API = f"https://api.{HOST_IP}.sslip.io"
CDN = f"https://cdn.{HOST_IP}.sslip.io"
SUFFIX = f".{HOST_IP}.sslip.io"
KST = timezone(timedelta(hours=9))

# sslip.io 해석(공개 DNS)에 기대지 않는다: 이 환경의 이름은 전부 HOST_IP 로 보낸다.
_real_getaddrinfo = socket.getaddrinfo


def _getaddrinfo(host, *args, **kwargs):
    if isinstance(host, str) and host.endswith(SUFFIX):
        host = HOST_IP
    return _real_getaddrinfo(host, *args, **kwargs)


socket.getaddrinfo = _getaddrinfo

_ctx = ssl.create_default_context(cafile=str(CA_FILE))
# 프록시 환경변수(HTTPS_PROXY)는 쓰지 않는다: 로컬 환경으로 곧바로 간다.
_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=_ctx))


class Failure(Exception):
    pass


def http(method, url, body=None, headers=None, expect=None):
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with _opener.open(request, timeout=30) as response:
            status, data = response.status, response.read()
    except urllib.error.HTTPError as error:
        status, data = error.code, error.read()
    if expect is not None and status not in expect:
        raise Failure(f"{method} {url} -> {status} (기대 {expect}): {data[:300]!r}")
    return status, data


def api(method, path, payload=None, token=None, expect=(200, 201, 204)):
    headers = {"Content-Type": "application/json"} if payload is not None else {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    body = json.dumps(payload).encode() if payload is not None else None
    status, data = http(method, API + path, body, headers, expect)
    return status, (json.loads(data) if data else None)


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def id_token(sub: str, raw_nonce: str, email: str) -> str:
    """harness/parity/src/idtoken.rs 와 같은 규칙으로 서명한 가짜 Google ID Token."""
    now = int(time.time())
    header = {"alg": "RS256", "kid": "harness-test-key", "typ": "JWT"}
    claims = {
        "iss": "https://accounts.google.com",
        "aud": "test-google-client-id",
        "sub": sub,
        "iat": now,
        "exp": now + 600,
        "nonce": hashlib.sha256(raw_nonce.encode()).hexdigest(),
        "email": email,
        "email_verified": True,
    }
    signing_input = f"{b64url(json.dumps(header).encode())}.{b64url(json.dumps(claims).encode())}"
    signature = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", str(KEY_FILE)],
        input=signing_input.encode(), capture_output=True, check=True,
    ).stdout
    return f"{signing_input}.{b64url(signature)}"


def make_image(kind: str) -> bytes:
    """lambda-image 컨테이너의 Pillow 로 시험 이미지를 만든다. 사인은 PNG, 게시물은 EXIF 가 든 WebP."""
    code = (
        "import io,sys\n"
        "from PIL import Image\n"
        "buf=io.BytesIO()\n"
        "if sys.argv[1]=='png':\n"
        "    img=Image.new('RGBA',(600,300),(255,255,255,0))\n"
        "    for x in range(40,560):\n"
        "        for y in range(120,170): img.putpixel((x,y),(20,20,20,255))\n"
        "    img.save(buf,'PNG')\n"
        "else:\n"
        "    img=Image.new('RGB',(1600,1200))\n"
        "    img.putdata([(x*255//1600,y*255//1200,128) for y in range(1200) for x in range(1600)])\n"
        "    exif=Image.Exif(); exif[0x010F]='LocalAws'; exif[0x0110]='E2E'\n"
        "    img.save(buf,'WEBP',quality=90,exif=exif.tobytes())\n"
        "sys.stdout.buffer.write(buf.getvalue())\n"
    )
    result = subprocess.run(
        ["docker", "compose", "exec", "-T", "lambda-image", "python", "-c", code, "png" if kind == "png" else "webp"],
        cwd=COMPOSE_DIR, capture_output=True,
    )
    if result.returncode != 0 or not result.stdout:
        raise Failure(f"이미지 생성 실패: {result.stderr[-300:]!r}")
    return result.stdout


def presigned_put(url: str, body: bytes, content_type: str):
    """presigned PUT. SignedHeaders 에 든 헤더를 채운다(앱이 하는 일)."""
    from urllib.parse import parse_qs, urlparse

    signed = parse_qs(urlparse(url).query).get("X-Amz-SignedHeaders", ["host"])[0].split(";")
    values = {"content-type": content_type, "cache-control": "public, max-age=86400", "if-none-match": "*"}
    headers = {name: values[name] for name in signed if name in values}
    status, data = http("PUT", url, body, headers)
    if status != 200:
        raise Failure(f"presigned PUT -> {status}: {data[:300]!r}")
    return headers


def wait_until(description, probe):
    deadline = time.monotonic() + TIMEOUT
    last = None
    while time.monotonic() < deadline:
        last = probe()
        if last:
            return last
        time.sleep(1)
    raise Failure(f"시간 초과({TIMEOUT:.0f}초): {description}")


def step(message):
    print(f"[e2e] {message}", flush=True)


def cdn_get(url):
    status, data = http("GET", url)
    if status != 200 or not data:
        raise Failure(f"cdn GET {url} -> {status}")
    return data


def purge_image_queue():
    """이전 시험의 남은 메시지를 비운다(S3 TestEvent 등)."""
    subprocess.run(
        ["docker", "compose", "exec", "-T", "aws", "awslocal", "--region", "ap-northeast-2", "sqs", "purge-queue",
         "--queue-url", "http://sqs.ap-northeast-2.localhost.localstack.cloud:4566/000000000000/chalkak-image-processing"],
        cwd=COMPOSE_DIR, capture_output=True,
    )


def psql(sql):
    return subprocess.run(
        ["docker", "compose", "exec", "-T", "postgres", "psql", "-q", "-tA", "-U", "chalkak", "-d", "chalkak",
         "-v", "ON_ERROR_STOP=1", "-c", sql], cwd=COMPOSE_DIR, check=True, capture_output=True, text=True,
    ).stdout.strip()


def todays_topic_id():
    """주제는 날짜마다 하나다: 오늘 주제가 있으면 쓰고 없으면 만든다."""
    today = datetime.now(KST).date()
    start = datetime.combine(today, datetime.min.time(), KST)
    topic_id = psql(f"SELECT id FROM topics WHERE topic_date = '{today}'")
    if not topic_id:
        topic_id = str(uuid.uuid4())
        psql("INSERT INTO topics (id, title, topic_date, starts_at, ends_at) VALUES "
             f"('{topic_id}', 'e2e', '{today}', '{start.isoformat()}', '{(start + timedelta(days=1)).isoformat()}')")
    return topic_id


def signup_user(run):
    """회원가입 사인 흐름. 가짜 ID Token -> 사인 업로드 URL -> PNG PUT -> Lambda 처리 -> 가입 -> 사인 cdn GET 200.

    돌려주는 값: sub(가짜 소셜 계정 식별자), userId, accessToken, refreshToken, signature(응답), uploadId.
    """
    sub = f"e2e-sub-{run}"
    email = f"e2e-{run}@example.com"
    nonce = f"nonce-{run}"
    token = id_token(sub, nonce, email)
    step("사인 업로드 URL 발급 (가짜 ID Token)")
    _, upload = api("POST", "/api/v1/auth/social-signup/signature/uploads",
                    {"provider": "GOOGLE", "idToken": token, "rawNonce": nonce})
    signup_token = upload["signupToken"]
    if not upload["uploadUrl"].startswith("https://") or f".s3.{HOST_IP}.sslip.io" not in upload["uploadUrl"]:
        raise Failure(f"사인 uploadUrl 이 예상한 S3 이름이 아니다: {upload['uploadUrl'][:120]}")
    step(f"사인 PNG presigned PUT -> {upload['uploadUrl'].split('?')[0]}")
    presigned_put(upload["uploadUrl"], make_image("png"), "image/png")

    step("Lambda 가 처리해 콜백(204)으로 닫힐 때까지 가입을 시도한다")

    def try_signup():
        status, data = http("POST", API + "/api/v1/auth/social-signup",
                            json.dumps({"signupToken": signup_token}).encode(), {"Content-Type": "application/json"})
        return json.loads(data) if status in (200, 201) else None

    signup = wait_until("사인 처리 후 가입", try_signup)
    access_token = signup.get("accessToken") or signup.get("tokens", {}).get("accessToken")
    if not access_token:
        raise Failure(f"가입 응답에 accessToken 이 없다: {list(signup)}")
    _, signature = api("GET", "/api/v1/users/me/signature", token=access_token)
    step(f"사인 응답: {json.dumps(signature)[:200]}")
    urls = [v for v in signature.values() if isinstance(v, str) and v.startswith(CDN)]
    if len(urls) < 2:
        raise Failure(f"사인 original·thumbnail cdn 주소가 없다: {signature}")
    for url in urls:
        size = len(cdn_get(url))
        step(f"cdn GET 200 {url} ({size} bytes)")
    return {"sub": sub, "email": email, "userId": signup.get("userId"), "accessToken": access_token,
            "refreshToken": signup.get("refreshToken"), "signature": signature, "uploadId": upload["uploadId"]}


def create_post(access_token, topic_id, run):
    """게시물 업로드 URL -> WebP PUT -> 게시물 작성 -> 콜백으로 PENDING 이 될 때까지. (postId, uploadId, 오늘 게시물) 반환."""
    step("게시물 업로드 URL 발급")
    _, post_upload = api("POST", "/api/v1/posts/uploads", token=access_token)
    post_upload_id = post_upload["uploadId"]
    step(f"게시물 WebP presigned PUT -> {post_upload['uploadUrl'].split('?')[0]}")
    presigned_put(post_upload["uploadUrl"], make_image("webp"), post_upload.get("contentType", "image/webp"))
    _, post = api("POST", "/api/v1/posts",
                  {"topicId": topic_id, "photoUploadId": post_upload_id, "title": f"e2e{run[:5]}"}, token=access_token)
    post_id = post["postId"] if "postId" in post else post["id"]
    step(f"게시물 작성 {post_id} 상태={post.get('moderationStatus') or post.get('status')}")

    def processed():
        # 처리 전(VALIDATING)에는 작성자에게도 상세가 보이지 않고, 처리 뒤(PENDING)에도 승인 전에는 상세가 404 다.
        # 오늘 게시물 상태로 콜백이 끝났는지 본다.
        _, today_post = api("GET", "/api/v1/posts/today", token=access_token)
        return today_post if today_post.get("moderationStatus") not in ("VALIDATING", None) else None

    today_post = wait_until("게시물 이미지 처리 콜백", processed)
    step(f"게시물 상태 {today_post['moderationStatus']}")
    if today_post["moderationStatus"] != "PENDING":
        raise Failure(f"게시물이 PENDING 이 아니다: {today_post}")
    return post_id, post_upload_id, today_post


def admin_login():
    step("관리자 로그인 (operator / operator-password)")
    _, admin = api("POST", "/api/v1/admin/auth/login", {"username": "operator", "password": "operator-password"})
    return admin["accessToken"]


def main():
    if not CA_FILE.exists():
        raise Failure(f"로컬 CA 루트가 없다: {CA_FILE} (docker compose up 으로 edge 를 먼저 띄운다)")
    purge_image_queue()
    run = uuid.uuid4().hex[:10]

    # ---- 1. 회원가입 사인 ----------------------------------------------------------------------
    access_token = signup_user(run)["accessToken"]

    # ---- 2. 게시물 이미지 ------------------------------------------------------------------------
    post_id, _, _ = create_post(access_token, todays_topic_id(), run)

    step("관리자가 게시물을 승인")
    admin_token = admin_login()
    api("PUT", f"/api/v1/admin/posts/{post_id}/moderation", {"status": "APPROVED"}, token=admin_token)
    _, detail = api("GET", f"/api/v1/posts/{post_id}", token=access_token)
    photo_urls = [detail.get("originalImageUrl"), detail.get("thumbnailImageUrl")]
    if not all(isinstance(u, str) and u.startswith(CDN) for u in photo_urls):
        raise Failure(f"게시물 original·thumbnail cdn 주소가 없다: {json.dumps(detail)[:400]}")
    for url in photo_urls:
        size = len(cdn_get(url))
        step(f"cdn GET 200 {url} ({size} bytes)")

    step("OK: 사인(PNG)·게시물(WebP) 모두 presigned PUT -> Lambda -> 콜백 204 -> cdn GET 200")


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"[e2e] 실패: {failure}", file=sys.stderr)
        sys.exit(1)
