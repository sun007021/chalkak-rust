#!/usr/bin/env bash
# 백엔드 전체(Rust 서버, PostgreSQL, S3·SQS 에뮬레이터, 이미지 Lambda, PushWorker, HTTPS edge)를 로컬에서 한 번에 올린다.
# 실제 구현은 local-aws/scripts/up-all.sh. 인자(down, reset, status, --rebuild, --real-oidc, --no-smoke)는 그대로 넘긴다.
exec "$(cd "$(dirname "$0")" && pwd)/local-aws/scripts/up-all.sh" "$@"
