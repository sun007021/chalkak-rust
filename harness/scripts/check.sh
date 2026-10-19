#!/usr/bin/env bash
# 작업 트리 하나에서 하네스 검증을 한 번에 돌린다: 빌드 → lane 준비 → 병렬 실행.
#
#   harness/scripts/check.sh [verify] [--filter 태그] [--jobs N]   # Rust를 golden과 비교(기본). Java가 필요 없다
#   harness/scripts/check.sh compare [--filter 태그] [--jobs N]     # Java oracle과 Rust를 바로 비교
#   harness/scripts/check.sh record --filter 태그 [--jobs N]        # Java oracle에서 golden 녹화(시나리오를 바꿨을 때)
#   harness/scripts/check.sh txshape [--filter 태그] [--jobs N] [--out 파일.md]
#                                                                  # Java와 Rust가 단계마다 연 DB 트랜잭션의 모양을 비교해 표로 낸다
#
# txshape는 DB 문장 로그를 켠 lane(TXLOG=1)이 필요해서 Java·Rust 모두 그런 lane을 따로 띄워 쓰고(없으면 --jobs개, 기본 1),
# Java lane은 평소 쓰던 것과 별개다. 끝나도 내리지 않는다(`lane.sh down rust`, `lane.sh down java-N`).
#
# --jobs는 Rust lane 수(기본 2)다. 이 작업 트리의 Rust lane이 모자라면 더 띄우고, 매번 방금 빌드한 바이너리로 앱을
# 다시 띄운다. Java lane은 모든 작업 트리가 함께 쓴다. 하나도 없으면 하나 띄우고, 있으면 쉬고 있는 것만 골라 쓴다
# (parity가 lane 잠금으로 나눠 가진다). Java lane을 늘리려면 harness/scripts/lane.sh up java N.
#
# 시나리오에 `lane = "relay"`가 있으면 기본 lane 실행 뒤에 SQS Relay를 켠 lane(RELAY=1, 하나)에서 이어서 돌린다.
#
# 끝나도 lane은 내리지 않는다(다음 실행이 바로 시작한다). 다 쓰면 harness/scripts/lane.sh down rust.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LANE="$ROOT/harness/scripts/lane.sh"
cd "$ROOT"

mode=verify
case "${1:-}" in verify | compare | record | txshape) mode=$1; shift ;; esac
jobs=2
[[ $mode == txshape ]] && jobs=1
args=()
while (($#)); do
  case $1 in
    --jobs) jobs=$2; shift 2 ;;
    *) args+=("$1"); shift ;;
  esac
done
[[ $mode == record && " ${args[*]} " != *" --filter "* ]] && { echo "record에는 --filter가 필요합니다(golden 전체를 다시 쓰지 않게)" >&2; exit 2; }

count_lines() { grep -c . || true; }

need_rust=1
[[ $mode == record ]] && need_rust=0
pkgs=(-p chalkak-parity -p chalkak-fakes)
((need_rust)) && pkgs+=(-p chalkak-server)
# 워크스페이스 기능 통합이 매번 같도록 패키지 묶음을 고정한다.
cargo build --release -q "${pkgs[@]}"

txlog_flag=()
[[ $mode == txshape ]] && txlog_flag=(--txlog)
if ((need_rust)); then
  have=$("$LANE" targets rust "${txlog_flag[@]}" | tr , '\n' | count_lines)
  if ((have < jobs)); then
    if [[ $mode == txshape ]]; then export TXLOG=1; fi
    "$LANE" up rust $((jobs - have)) >/dev/null
    unset TXLOG
  fi
  "$LANE" restart rust
fi
if [[ $mode == txshape ]]; then
  have=$("$LANE" targets java --txlog | tr , '\n' | count_lines)
  if ((have < jobs)); then
    [[ -f "${ORACLE_JAR:-/tmp/chalkak-lanes/oracle/app.jar}" ]] || "$ROOT/harness/scripts/oracle-jar.sh" >/dev/null
    TXLOG=1 "$LANE" up java $((jobs - have)) >/dev/null
  fi
elif [[ $mode != verify ]]; then
  if [[ -z "$("$LANE" targets java)" ]]; then
    "$ROOT/harness/scripts/oracle-jar.sh" >/dev/null
    "$LANE" up java 1 >/dev/null
  fi
fi

parity="$ROOT/target/release/chalkak-parity"

# txshape는 기본 lane만 쓴다. 나머지는 기본 lane 시나리오를 돌린 뒤, SQS Relay처럼 앱이 스스로 도는 작업을 켠 relay lane에서
# 돌 시나리오(`lane = "relay"`)가 있으면 이어서 그 lane에서도 돌린다(lane 종류가 다르면 환경변수가 달라 기본 lane에서 못 돈다).
if [[ $mode == txshape ]]; then
  exec "$parity" txshape --java "$("$LANE" targets java --txlog)" --rust "$("$LANE" targets rust --txlog)" "${args[@]}"
fi

run_pass() { # [relay]  -> 종료 코드
  local lane_args=() rust_flag=() java_flag=()
  if [[ ${1:-} == relay ]]; then
    lane_args=(--lane relay)
    rust_flag=(--relay)
    java_flag=(--relay)
  fi
  case $mode in
    verify) "$parity" verify "${lane_args[@]}" --target "$("$LANE" targets rust "${rust_flag[@]}")" "${args[@]}" ;;
    compare) "$parity" compare "${lane_args[@]}" --left "$("$LANE" targets java "${java_flag[@]}")" --right "$("$LANE" targets rust "${rust_flag[@]}")" "${args[@]}" ;;
    record) "$parity" record "${lane_args[@]}" --target "$("$LANE" targets java "${java_flag[@]}")" "${args[@]}" ;;
  esac
}

rc=0
run_pass || rc=$?

relay_scenarios=$("$parity" count --lane relay "${args[@]}")
if ((relay_scenarios > 0)); then
  echo "relay lane 시나리오 ${relay_scenarios}개"
  if ((need_rust)) && [[ -z "$("$LANE" targets rust --relay)" ]]; then
    RELAY=1 "$LANE" up rust 1 >/dev/null
  fi
  if [[ $mode != verify && -z "$("$LANE" targets java --relay)" ]]; then
    [[ -f "${ORACLE_JAR:-/tmp/chalkak-lanes/oracle/app.jar}" ]] || "$ROOT/harness/scripts/oracle-jar.sh" >/dev/null
    RELAY=1 "$LANE" up java 1 >/dev/null
  fi
  run_pass relay || rc=$?
fi
exit "$rc"
