#!/usr/bin/env bash
# Java lane이 쓰는 원본 JAR을 만든다. harness/BASELINE 커밋을 받아 호스트에서 bootJar 하고
# $CHALKAK_LANES_DIR/oracle/app.jar(기본 /tmp/chalkak-lanes/oracle/app.jar)에 둔다. 이미 같은 커밋 JAR이 있으면 건너뛴다.
#
#   harness/scripts/oracle-jar.sh [원본 저장소 경로]   # 경로를 주면 그 클론에서 worktree를 만든다. 없으면 GitHub에서 받는다
#
# JDK 25가 필요하다. JAVA25_HOME → /opt/jdk/temurin-25 순으로 찾고, 없으면 eclipse-temurin:25-jdk 이미지에서 꺼낸다
# (Gradle 툴체인 자동 다운로드가 막힌 환경 대비). Maven Central이 429를 주면 간격을 늘려 다시 시도한다.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LANES_DIR="${CHALKAK_LANES_DIR:-/tmp/chalkak-lanes}"
OUT="$LANES_DIR/oracle"
BASELINE="$(cat "$ROOT/harness/BASELINE")"
mkdir -p "$OUT"

if [[ -f "$OUT/app.jar" && "$(cat "$OUT/BASELINE" 2>/dev/null)" == "$BASELINE" ]]; then
  echo "$OUT/app.jar (기준 커밋 ${BASELINE:0:7}, 이미 있음)"
  exit 0
fi

exec 9>"$OUT/.build.lock"
flock 9

JDK="${JAVA25_HOME:-/opt/jdk/temurin-25}"
if [[ ! -x "$JDK/bin/java" ]]; then
  echo "JDK 25를 eclipse-temurin:25-jdk 이미지에서 꺼냅니다 → $JDK" >&2
  mkdir -p "$(dirname "$JDK")"
  id=$(docker create eclipse-temurin:25-jdk)
  docker cp "$id:/opt/java/openjdk" "$JDK"
  docker rm "$id" >/dev/null
fi

SRC="$OUT/src"
if [[ ! -d "$SRC/.git" && ! -f "$SRC/.git" ]]; then
  if [[ -n "${1:-}" ]]; then
    git -C "$1" fetch -q origin "$BASELINE" 2>/dev/null || true
    git -C "$1" worktree add -q --detach "$SRC" "$BASELINE"
  else
    git init -q "$SRC"
    git -C "$SRC" remote add origin https://github.com/woowacourse-teams/2026-Chalkak.git
  fi
fi
if [[ "$(git -C "$SRC" rev-parse HEAD 2>/dev/null)" != "$BASELINE" ]]; then
  git -C "$SRC" fetch -q --depth 1 origin "$BASELINE" 2>/dev/null || true
  git -C "$SRC" checkout -q --detach "$BASELINE"
fi

cd "$SRC/backend"
for i in 1 2 3 4 5 6; do
  if ./gradlew bootJar -x test --no-daemon -q --max-workers=1 \
    -Porg.gradle.java.installations.paths="$JDK" >"$OUT/build.log" 2>&1; then
    cp build/libs/backend-0.0.1-SNAPSHOT.jar "$OUT/app.jar"
    echo "$BASELINE" > "$OUT/BASELINE"
    echo "$OUT/app.jar"
    exit 0
  fi
  echo "빌드 실패($i/6), 다시 시도합니다. 로그: $OUT/build.log" >&2
  sleep $((i * 20))
done
tail -n 30 "$OUT/build.log" >&2
exit 1
