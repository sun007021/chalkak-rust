#!/usr/bin/env bash
# 이미지 업로드 종단 확인(사인 PNG, 게시물 WebP). 백엔드(rust 또는 java)와 나머지 서비스가 떠 있어야 한다.
#   scripts/e2e-image.sh                # 확인만
#   HOST_IP=192.168.0.10 scripts/e2e-image.sh
# 실패하면 서비스 로그(edge·lambda-image·esm-poller·backend) 끝을 보여 준다.
set -uo pipefail
cd "$(dirname "$0")/.."
export HOST_IP="${HOST_IP:-127.0.0.1}"
if python3 -I scripts/e2e_image.py; then exit 0; fi
status=$?
echo "---- esm-poller" >&2; docker compose logs --tail 15 esm-poller >&2 || true
echo "---- lambda-image" >&2; docker compose logs --tail 25 lambda-image >&2 || true
echo "---- backend" >&2; docker compose logs --tail 15 rust java 2>/dev/null >&2 || true
exit "$status"
