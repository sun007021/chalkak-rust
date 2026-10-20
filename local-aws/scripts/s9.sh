#!/usr/bin/env bash
# S9 장시간 저부하 시험 실행기: 스택을 새로 올리고 시드한 뒤 저부하+분 단위 작업을 주입하며 표본을 파일로 남기고, 끝에서 판정한다. 끝나면 down -v.
#   scripts/s9.sh rust                       # 8시간(기본). 8시간 본시험은 이 명령 그대로
#   scripts/s9.sh rust --duration 1h         # 1시간(스크립트 검증용)
#   scripts/s9.sh rust --duration 10m --rps 5 --out /경로   # 짧게, 출력 위치 지정
#   scripts/s9.sh judge local-aws/.state/s9/<디렉터리>      # 이미 있는 출력 파일만으로 다시 판정
# 옵션: --duration 8h|60m|3600s, --rps 부하(기본 10), --out 출력 디렉터리, --keep(끝나도 스택 유지),
#       --static-credentials(IMDS 대신 정적 자격증명: imds-mock 의 연결 거부 잡음을 뺀다. 이때 SQS 연결 오류·주제 푸시 미수락은 0 이어야 통과)
# 출력: local-aws/.state/s9/<백엔드>-<시각>/ (meta.json samples.csv jobs.csv runtime.jsonl jobs_log.jsonl edge.jsonl errors.log progress.log verdict.json verdict.txt ...)
# 종료 코드 0 이면 판정 통과. 사전 준비는 l5.sh 와 같다.
set -uo pipefail
cd "$(dirname "$0")/.."
export HOST_IP="${HOST_IP:-127.0.0.1}"
status=0
args=()
for a in "$@"; do
  if [[ "$a" == --static-credentials ]]; then export S9_STATIC_CREDENTIALS=1; else args+=("$a"); fi
done
set -- "${args[@]}"
if [[ "${1:-}" == judge ]]; then python3 -I scripts/s9.py "$@"; exit $?; fi
python3 -I scripts/s9.py run "$@" || status=$?
if [[ " $* " != *" --keep "* ]]; then
  docker compose -f compose.yaml -f compose.l5.yaml --profile chaos --profile rust --profile java down -v --remove-orphans >/dev/null 2>&1
fi
exit "$status"
