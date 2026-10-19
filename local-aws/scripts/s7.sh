#!/usr/bin/env bash
# S7(푸시 경로 안정성) 시험: 스택을 맞춰 띄우고 scripts/s7_push.py 를 돈다.
#   scripts/s7.sh rust                     # Rust 백엔드로 전 시나리오 (약 20분: dlq 가 실시간 약 10분)
#   scripts/s7.sh rust burst expire        # 일부만
#   scripts/s7.sh java                     # Java: Relay·DLQ 비교용(Worker 없음, 발송 시나리오는 건너뜀)
#   scripts/s7.sh java-worker dlq          # Java Worker 를 켜 FCM 도달 불가 상태의 재수신·DLQ 를 본다 (약 12분)
#   KEEP=1 scripts/s7.sh rust ...          # 끝난 뒤 스택을 내리지 않는다
# 스택: --profile chaos(toxiproxy)와 SQS_ENDPOINT_URL=toxiproxy 를 거쳐 SQS 지연을 주입한다. 끝나면 down -v 로 DB·큐를 비운다.
# 사전 준비는 README 의 빠른 시작과 같다(fetch-tools, prepare-lambda, cargo build --release -p chalkak-fakes -p chalkak-server).
set -uo pipefail
cd "$(dirname "$0")/.."
target="${1:?사용법: s7.sh rust|java|java-worker [시나리오...]}"; shift
export HOST_IP="${HOST_IP:-127.0.0.1}"
export SQS_ENDPOINT_URL="http://toxiproxy:14566"
profile="$target"
case "$target" in
  java) export JAVA_NOTIFICATION_RELAY=true; profile=java ;;
  java-worker)
    export JAVA_NOTIFICATION_RELAY=true JAVA_NOTIFICATION_WORKER=true JAVA_FCM_CREDENTIALS_PATH=/secrets/fcm-service-account.json
    profile=java ;;
  rust) ;;
  *) echo "알 수 없는 대상: $target" >&2; exit 2 ;;
esac
compose=(docker compose -f compose.yaml -f compose.s7.yaml --profile chaos --profile "$profile")
# 이전 백엔드의 DB 를 이어받지 않는다(Java 와 Rust 의 Flyway 기록이 다르다, README).
docker compose --profile chaos --profile rust --profile java down -v >/dev/null 2>&1 || true
"${compose[@]}" up -d --wait --wait-timeout 300 || exit 1
status=0
BACKEND="$profile" python3 -I scripts/s7_push.py "$@" || status=$?
[[ "${KEEP:-0}" == 1 ]] || docker compose --profile chaos --profile rust --profile java down -v >/dev/null 2>&1
exit "$status"
