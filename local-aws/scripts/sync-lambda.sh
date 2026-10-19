#!/usr/bin/env bash
# 원본 저장소의 이미지 처리 Lambda 코드를 local-aws/lambda-image/function 에 그대로 복사한다.
#   scripts/sync-lambda.sh /home/claude/2026-Chalkak
set -euo pipefail
SRC="${1:?원본 저장소 경로(2026-Chalkak)를 주세요}/backend/lambda/image-processor"
DST="$(cd "$(dirname "$0")/.." && pwd)/lambda-image/function"
[[ -f "$SRC/handler.py" ]] || { echo "없음: $SRC/handler.py" >&2; exit 1; }
rm -rf "$DST"; mkdir -p "$DST"
cp -r "$SRC/handler.py" "$SRC/image_processor" "$SRC/tests" "$SRC/requirements.txt" "$DST/"
find "$DST" -name __pycache__ -prune -exec rm -rf {} +
echo "synced $(git -C "$1" rev-parse --short HEAD 2>/dev/null || echo '?') -> $DST (lambda-image/SOURCE 의 기준 커밋을 갱신하세요)"
