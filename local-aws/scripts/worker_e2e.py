#!/usr/bin/env python3
"""푸시 Worker 종단 확인(rust 프로필): 푸시 기기를 등록한 회원의 게시물을 승인하면
Relay -> LocalStack 큐 -> Worker 수신 -> fakes 의 가짜 OAuth·FCM 호출 -> 큐에서 삭제까지 이어지는지 본다.

e2e_image.py 의 헬퍼를 쓴다. fakes 포트(FAKES_PORT, 기본 19000)로 받은 요청을 읽는다.
"""

import json
import os
import subprocess
import sys
import time
import urllib.request
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # python3 -I 는 스크립트 폴더를 경로에 넣지 않는다
import e2e_image as e  # noqa: E402
from e2e_image import Failure, api, step  # noqa: E402

FAKES = f"http://127.0.0.1:{os.environ.get('FAKES_PORT', '19000')}"
QUEUE = "http://localhost:4566/000000000000/chalkak-dev-push"


def fakes_requests():
    return json.load(urllib.request.urlopen(f"{FAKES}/__admin/requests"))


def queue_depth():
    out = subprocess.run(
        ["docker", "compose", "exec", "-T", "aws", "awslocal", "sqs", "get-queue-attributes", "--queue-url", QUEUE,
         "--attribute-names", "ApproximateNumberOfMessages", "ApproximateNumberOfMessagesNotVisible"],
        capture_output=True, text=True, cwd=Path(__file__).resolve().parent.parent, check=True).stdout
    attrs = json.loads(out)["Attributes"]
    return int(attrs["ApproximateNumberOfMessages"]) + int(attrs["ApproximateNumberOfMessagesNotVisible"])


def main():
    run = uuid.uuid4().hex[:10]
    user = e.signup_user(f"{run}a")
    nonce = f"login-{run}"
    _, login = api("POST", "/api/v1/auth/social-login",
                   {"provider": "GOOGLE", "idToken": e.id_token(user["sub"], nonce, user["email"]), "rawNonce": nonce})
    token = login["accessToken"]
    fcm_token = f"local-aws-token-{run}"
    api("PUT", "/api/v1/push-devices/current", {"fcmToken": fcm_token}, token=token)
    step("푸시 기기 등록")
    post_id, _, _ = e.create_post(token, e.todays_topic_id(), run)
    api("PUT", f"/api/v1/admin/posts/{post_id}/moderation", {"status": "APPROVED"}, token=e.admin_login())
    step(f"승인 {post_id}")

    sends = []
    for _ in range(60):
        records = fakes_requests()
        sends = [r for r in records if r["service"] == "fcm_send" and fcm_token in r["body"]]
        if sends:
            break
        time.sleep(1)
    if not sends:
        raise Failure("60초 안에 가짜 FCM 이 전송 요청을 받지 못했다")
    if not any(r["service"] == "oauth_token" for r in records):
        raise Failure("가짜 OAuth 토큰 교환 요청이 없다")
    message = json.loads(sends[0]["body"])["message"]
    if message["token"] != fcm_token or message["data"]["sourceId"] != post_id:
        raise Failure(f"FCM 본문이 기대와 다르다: {sends[0]['body'][:300]}")
    step("가짜 OAuth 토큰 교환 + FCM v1 전송 수신")

    for _ in range(30):
        if queue_depth() == 0:
            step("OK: 큐가 비었다(Worker 가 처리 후 메시지를 삭제함)")
            return
        time.sleep(1)
    raise Failure("30초 뒤에도 큐에 메시지가 남아 있다")


if __name__ == "__main__":
    try:
        main()
    except Failure as exc:
        print(f"[e2e] 실패: {exc}", file=sys.stderr)
        sys.exit(1)
