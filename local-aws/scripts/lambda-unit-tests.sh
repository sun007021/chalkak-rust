#!/usr/bin/env bash
# 원본 Lambda 단위 테스트를 lambda 컨테이너(Lambda 기본 이미지 + 설치한 의존성)에서 실행한다.
# 원본 buildspec.yml 의 `python -m unittest discover --start-directory tests --verbose` 와 같다.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${LAMBDA_IMAGE:-amazon/aws-lambda-python:3.13}"
[[ -d "$ROOT/.state/lambda-deps" ]] || "$ROOT/scripts/prepare-lambda.sh"
exec docker run --rm --entrypoint python \
  -v "$ROOT/lambda-image/function:/var/task:ro" -v "$ROOT/.state/lambda-deps:/opt/deps:ro" \
  -e PYTHONPATH=/var/task:/opt/deps -e PYTHONDONTWRITEBYTECODE=1 -w /var/task \
  "$IMAGE" -m unittest discover --start-directory tests --verbose
