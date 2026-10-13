#!/usr/bin/env bash
# 작업 트리 하나에서 하네스 검증을 한 번에 돌린다: 빌드 → lane 준비 → 병렬 실행.
#
#   harness/scripts/check.sh [verify] [--filter 태그] [--jobs N]   # Rust를 golden과 비교(기본). Java가 필요 없다
#   harness/scripts/check.sh compare [--filter 태그] [--jobs N]     # Java oracle과 Rust를 바로 비교
#   harness/scripts/check.sh record --filter 태그 [--jobs N]        # Java oracle에서 golden 녹화(시나리오를 바꿨을 때)
#
# --jobs는 Rust lane 수(기본 2)다. 이 작업 트리의 Rust lane이 모자라면 더 띄우고, 매번 방금 빌드한 바이너리로 앱을
# 다시 띄운다. Java lane은 모든 작업 트리가 함께 쓴다. 하나도 없으면 하나 띄우고, 있으면 쉬고 있는 것만 골라 쓴다
# (parity가 lane 잠금으로 나눠 가진다). Java lane을 늘리려면 harness/scripts/lane.sh up java N.
#
# 끝나도 lane은 내리지 않는다(다음 실행이 바로 시작한다). 다 쓰면 harness/scripts/lane.sh down rust.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LANE="$ROOT/harness/scripts/lane.sh"
cd "$ROOT"

mode=verify
case "${1:-}" in verify | compare | record) mode=$1; shift ;; esac
jobs=2
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

if ((need_rust)); then
  have=$("$LANE" targets rust | tr , '\n' | count_lines)
  if ((have < jobs)); then "$LANE" up rust $((jobs - have)) >/dev/null; fi
  "$LANE" restart rust
fi
if [[ $mode != verify ]]; then
  if [[ -z "$("$LANE" targets java)" ]]; then
    "$ROOT/harness/scripts/oracle-jar.sh" >/dev/null
    "$LANE" up java 1 >/dev/null
  fi
fi

parity="$ROOT/target/release/chalkak-parity"
case $mode in
  verify) exec "$parity" verify --target "$("$LANE" targets rust)" "${args[@]}" ;;
  compare) exec "$parity" compare --left "$("$LANE" targets java)" --right "$("$LANE" targets rust)" "${args[@]}" ;;
  record) exec "$parity" record --target "$("$LANE" targets java)" "${args[@]}" ;;
esac
