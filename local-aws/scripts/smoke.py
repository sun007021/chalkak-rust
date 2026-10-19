#!/usr/bin/env python3
"""앱 없이 HTTP 로 계획 6.5 흐름을 도는 스모크(가짜 모드 토큰). 백엔드가 rust 든 java 든 같은 결과여야 한다.

  1. 가입(사인 업로드): 가짜 Google ID Token -> 사인 PNG presigned PUT -> Lambda -> 콜백 -> 가입, 사인 cdn GET 200
  2. 로그인: 같은 소셜 계정으로 social-login(새 nonce)
  3. 게시물 이미지 업로드·작성: WebP presigned PUT -> 작성(VALIDATING) -> 콜백 후 오늘 게시물 PENDING
  4. 피드·오늘 게시물: 승인 전에는 다른 회원의 피드에 보이지 않고, 작성자의 오늘 게시물은 PENDING
  5. 관리자 승인
  6. 피드: 승인 뒤 다른 회원(B)의 피드에 나타나고 original·thumbnail·사인 이미지가 cdn 에서 GET 200
  7. 좋아요: B 가 누르면 likeCount 1·isLiked, 작성자 화면에도 반영, 취소하면 0
  8. 알림함: 작성자 A 에게 POST_APPROVED 가 생기고 미읽음 -> 읽음 처리 -> 미읽음 없음
  (좋아요를 승인 뒤에 두는 이유: 승인 전 게시물은 다른 회원에게 보이지 않아 누를 수 없다.)

e2e_image.py 의 헬퍼(가짜 토큰, presigned PUT, 이미지 생성)를 그대로 쓴다. 환경변수: HOST_IP, TIMEOUT_SECONDS 는 같다.
"""

import json
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # python3 -I 는 스크립트 폴더를 경로에 넣지 않는다
import e2e_image as e  # noqa: E402
from e2e_image import CDN, Failure, api, cdn_get, step


def expect(condition, message):
    if not condition:
        raise Failure(message)


def check(label):
    step(f"확인: {label}")


def main():
    if not e.CA_FILE.exists():
        raise Failure(f"로컬 CA 루트가 없다: {e.CA_FILE} (docker compose up 으로 edge 를 먼저 띄운다)")
    e.purge_image_queue()
    run = uuid.uuid4().hex[:10]
    started = time.monotonic()

    # ---- 1. 가입(사인 업로드): 작성자 A ----------------------------------------------------------
    step("== 1. 가입(사인 업로드) A")
    a = e.signup_user(f"{run}a")

    # ---- 2. 로그인 --------------------------------------------------------------------------------
    step("== 2. 로그인 A (같은 소셜 계정, 새 nonce)")
    nonce = f"login-{run}"
    _, login = api("POST", "/api/v1/auth/social-login",
                   {"provider": "GOOGLE", "idToken": e.id_token(a["sub"], nonce, a["email"]), "rawNonce": nonce})
    expect(login.get("status") == "LOGIN_SUCCESS" and login.get("accessToken"), f"로그인 응답: {json.dumps(login)[:300]}")
    expect(login.get("userId") == a["userId"], "로그인 회원이 가입 회원과 다르다")
    a_token = login["accessToken"]
    if login.get("refreshToken"):
        _, refreshed = api("POST", "/api/v1/auth/refresh", {"refreshToken": login["refreshToken"]})
        expect(refreshed.get("accessToken"), f"토큰 갱신 응답: {list(refreshed)}")
        check("refresh 로 새 액세스 토큰")

    # ---- 3. 게시물 이미지 업로드·작성 ---------------------------------------------------------------
    step("== 3. 게시물 업로드·작성 A")
    topic_id = e.todays_topic_id()
    post_id, upload_id, today = e.create_post(a_token, topic_id, run)
    expect(today.get("postId") == post_id or today.get("isPosted"), f"오늘 게시물: {today}")

    # ---- 4. 피드·오늘 게시물 (승인 전) -------------------------------------------------------------
    step("== 4. 피드·오늘 게시물 (승인 전)")
    b = e.signup_user(f"{run}b")
    b_token = b["accessToken"]
    topic_date = today["topicDate"]
    _, feed = api("GET", f"/api/v1/posts?topicDate={topic_date}&pageSize=100", token=b_token)
    expect(post_id not in {p["id"] for p in feed["posts"]}, "승인 전 게시물이 다른 회원 피드에 보인다")
    check("승인 전 게시물은 다른 회원 피드에 없고, 작성자 오늘 게시물은 PENDING")

    # ---- 5. 관리자 승인 ---------------------------------------------------------------------------
    step("== 5. 관리자 승인")
    admin_token = e.admin_login()
    api("PUT", f"/api/v1/admin/posts/{post_id}/moderation", {"status": "APPROVED"}, token=admin_token)

    # ---- 6. 피드 + 이미지 cdn GET -----------------------------------------------------------------
    step("== 6. 피드 (승인 후, B 의 화면) + 이미지")
    _, feed = api("GET", f"/api/v1/posts?topicDate={topic_date}&pageSize=100", token=b_token)
    mine = next((p for p in feed["posts"] if p["id"] == post_id), None)
    expect(mine is not None, f"승인 뒤 피드에 게시물이 없다: {[p['id'] for p in feed['posts']][:5]}")
    expect(mine["likeCount"] == 0 and mine["isLiked"] is False and mine["isMine"] is False, f"피드 항목: {mine}")
    for key in ("originalImageUrl", "thumbnailImageUrl", "signatureOriginalImageUrl"):
        url = mine.get(key)
        expect(isinstance(url, str) and url.startswith(CDN + "/"), f"피드 {key} 가 cdn 주소가 아니다: {url}")
        step(f"cdn GET 200 {url.split('/', 3)[-1]} ({len(cdn_get(url))} bytes)")

    # ---- 7. 좋아요 --------------------------------------------------------------------------------
    step("== 7. 좋아요")
    _, liked = api("PUT", f"/api/v1/posts/{post_id}/likes", token=b_token)
    expect(liked["likeCount"] == 1 and liked["isLiked"] is True, f"좋아요 응답: {liked}")
    _, detail = api("GET", f"/api/v1/posts/{post_id}", token=a_token)
    expect(detail["likeCount"] == 1 and detail["isLiked"] is False, f"작성자 상세: {detail}")
    _, again = api("PUT", f"/api/v1/posts/{post_id}/likes", token=b_token)
    expect(again["likeCount"] == 1, f"좋아요 멱등: {again}")
    _, unliked = api("DELETE", f"/api/v1/posts/{post_id}/likes", token=b_token)
    expect(unliked["likeCount"] == 0 and unliked["isLiked"] is False, f"좋아요 취소: {unliked}")
    api("PUT", f"/api/v1/posts/{post_id}/likes", token=b_token)
    check("좋아요 등록·멱등·취소·재등록")

    # ---- 8. 알림함 --------------------------------------------------------------------------------
    step("== 8. 알림함 (작성자 A)")
    _, unread = api("GET", "/api/v1/notifications/unread-status", token=a_token)
    expect(unread["hasUnread"] is True, f"미읽음 표시: {unread}")
    _, inbox = api("GET", "/api/v1/notifications", token=a_token)
    expect(len(inbox["notifications"]) == 1, f"알림 수: {json.dumps(inbox)[:300]}")
    note = inbox["notifications"][0]
    expect(note["type"] == "POST_APPROVED" and note["readAt"] is None, f"알림: {note}")
    thumb = note.get("thumbnailImageUrl")
    if thumb:
        step(f"알림 썸네일 cdn GET 200 ({len(cdn_get(thumb))} bytes)")
    api("PATCH", f"/api/v1/notifications/{note['id']}/read", token=a_token)
    _, unread = api("GET", "/api/v1/notifications/unread-status", token=a_token)
    expect(unread["hasUnread"] is False, f"읽음 뒤 미읽음 표시: {unread}")
    check("POST_APPROVED 알림 1건, 읽음 처리 후 미읽음 없음")

    step(f"OK: 가입 -> 로그인 -> 게시물 -> 피드 -> 승인 -> 좋아요 -> 알림 -> cdn 이미지 ({time.monotonic() - started:.0f}초)")
    step(f"추적: scripts/trace.sh {upload_id}   (게시물 업로드)  /  scripts/trace.sh {note['id']}   (알림)  /  scripts/trace.sh {a['userId']}   (회원)")


if __name__ == "__main__":
    try:
        main()
    except Failure as failure:
        print(f"[e2e] 실패: {failure}", file=sys.stderr)
        sys.exit(1)
