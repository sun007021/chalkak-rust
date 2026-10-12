#!/usr/bin/env bash
# docker compose로 하네스 대상을 띄운다. 시계 파일을 먼저 만들고, 모든 서비스가 healthy가 될 때까지 기다린다.
#
#   harness/scripts/up.sh             # java-a(oracle) + pg-rust  (Rust 서버는 호스트에서 cargo run 하거나 아래 rust 사용)
#   harness/scripts/up.sh rust        # + rust 컨테이너
#   harness/scripts/up.sh selfcheck   # + java-b (Java 대 Java)
#
# 원본 Java 이미지는 처음 한 번 기준 커밋을 받아 빌드한다(수 분). 이후에는 캐시를 쓴다.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$ROOT/harness/out"
chmod a+rx "$ROOT/harness/out"
for t in java-a java-b rust; do
  [[ -f "$ROOT/harness/out/clock-$t" ]] || echo "+0" > "$ROOT/harness/out/clock-$t"
  chmod a+r "$ROOT/harness/out/clock-$t"
done

profiles=()
for p in "$@"; do profiles+=(--profile "$p"); done

export BASELINE="$(cat "$ROOT/harness/BASELINE")"
cd "$ROOT"
docker compose "${profiles[@]}" up -d --wait ${COMPOSE_UP_ARGS:-}
docker compose "${profiles[@]}" ps --format 'table {{.Service}}\t{{.Status}}\t{{.Ports}}'
