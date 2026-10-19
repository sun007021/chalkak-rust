#!/usr/bin/env bash
# 앱 없이 HTTP 로 계획 6.5 스모크(가입 -> 로그인 -> 게시물 -> 피드 -> 승인 -> 좋아요 -> 알림 -> cdn 이미지)를 돈다. 가짜 모드 토큰.
#   scripts/smoke.sh                       # 떠 있는 백엔드(rust 또는 java)에 대해
#   HOST_IP=192.168.0.10 scripts/smoke.sh
# 실패하면 trace.sh 로 마지막 업로드와 서비스 로그 끝을 보여 준다.
set -uo pipefail
cd "$(dirname "$0")/.."
export HOST_IP="${HOST_IP:-127.0.0.1}"
if python3 -I scripts/smoke.py; then exit 0; fi
status=$?
echo "---- 마지막 게시물 업로드 추적" >&2
last="$(docker compose exec -T postgres psql -q -tA -U chalkak -d chalkak -c 'SELECT id FROM post_image_uploads ORDER BY created_at DESC LIMIT 1' 2>/dev/null | head -1)"
if [[ -n "$last" ]]; then scripts/trace.sh "$last" >&2 || true; fi
exit "$status"
