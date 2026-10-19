#!/usr/bin/env bash
# Lambda 의존성(원본 requirements.txt: boto3, Pillow)을 .state/lambda-deps 에 설치한다.
# Lambda 기본 이미지에서 pip 을 돌려 그 런타임(Python 버전, 라이브러리)에 맞는 휠을 받는다.
# 원본 CodeBuild 와 같이 --only-binary 로 소스 빌드를 막는다(WebP 코덱이 빠진 Pillow 방지).
#
# 프록시가 TLS 를 다시 맺는 환경이면 PIP_CERT_FILE=/경로/ca-bundle.crt 로 CA 를 준다.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${LAMBDA_IMAGE:-amazon/aws-lambda-python:3.13}"
DEPS="$ROOT/.state/lambda-deps"
mkdir -p "$DEPS"

args=(--rm --entrypoint python -v "$ROOT/lambda-image/function/requirements.txt:/requirements.txt:ro" -v "$DEPS:/deps")
if [[ -n "${PIP_CERT_FILE:-}" ]]; then
  args+=(-v "$PIP_CERT_FILE:/pip-ca.crt:ro" -e PIP_CERT=/pip-ca.crt)
elif [[ -f /root/.ccr/ca-bundle.crt ]]; then
  args+=(-v /root/.ccr/ca-bundle.crt:/pip-ca.crt:ro -e PIP_CERT=/pip-ca.crt)
fi

docker run "${args[@]}" "$IMAGE" -m pip install --requirement /requirements.txt --target /deps \
  --no-cache-dir --only-binary=:all: --upgrade
docker run --rm --entrypoint python -v "$DEPS:/deps:ro" -e PYTHONPATH=/deps "$IMAGE" -c \
  "import boto3; from PIL import features; assert features.check('webp'), 'Pillow에 WebP 지원이 없다'; print('boto3', boto3.__version__, 'Pillow', __import__('PIL').__version__)"
