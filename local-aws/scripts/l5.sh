#!/usr/bin/env bash
# L5 안정성 시험(S1~S6·S8) 실행기: 스택을 새로 올리고 시드한 뒤 시나리오를 차례로 돈다. 끝나면 down -v.
#   scripts/l5.sh rust                       # 전부: S1 S2 S3 S6 S8 S5 S4 (약 1시간 40분, Java 는 더 걸린다)
#   scripts/l5.sh java S2 S3                 # 일부만
#   S1_MINUTES=2 scripts/l5.sh rust S1       # 짧게
#   KEEP=1 scripts/l5.sh rust S3             # 끝난 뒤 스택을 내리지 않는다
#   scripts/l5.sh compare                    # 가장 최근 rust·java 결과를 나란히 놓고 A/B 기준(p99, 응답 형식)을 판정
# 사전 준비는 README 의 빠른 시작과 같다(fetch-tools, prepare-lambda, cargo build --release -p chalkak-fakes -p chalkak-server -p chalkak-loadgen).
# 결과 JSON: local-aws/.state/l5/<백엔드>-<시나리오>-<시각>.json, 시나리오별 표본·부하 발생기 로그: .state/l5/run-*/
set -uo pipefail
cd "$(dirname "$0")/.."
if [[ "${1:-}" == compare ]]; then shift; exec python3 -I scripts/l5.py compare "$@"; fi
backend="${1:?사용법: l5.sh rust|java [시나리오...] | compare}"; shift
case "$backend" in rust|java) ;; *) echo "알 수 없는 백엔드: $backend" >&2; exit 2 ;; esac
[[ $# -gt 0 ]] || set -- S1 S2 S3 S6 S8 S5 S4
export HOST_IP="${HOST_IP:-127.0.0.1}"
status=0
python3 -I scripts/l5.py "$backend" --fresh "$@" || status=$?
if [[ "${KEEP:-0}" != 1 ]]; then
  docker compose -f compose.yaml -f compose.l5.yaml --profile chaos --profile rust --profile java down -v --remove-orphans >/dev/null 2>&1
fi
exit "$status"
