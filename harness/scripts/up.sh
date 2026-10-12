#!/usr/bin/env bash
# 하네스 대상을 띄운다. 대상마다 PostgreSQL 컨테이너 1개 + 앱 프로세스 1개 + 시계 파일 1개를 쓴다.
#
#   harness/scripts/up.sh java-a java-b     # Java 대 Java (하네스 자체의 결정성 확인)
#   harness/scripts/up.sh java-a rust       # Java 대 Rust
#
# 필요: docker, libfaketime(/usr/lib/x86_64-linux-gnu/faketime), Java 25, 원본 JAR(ORACLE_JAR).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/harness/out"
FAKETIME_LIB="${FAKETIME_LIB:-/usr/lib/x86_64-linux-gnu/faketime/libfaketime.so.1}"
ORACLE_JAR="${ORACLE_JAR:-$ROOT/../2026-Chalkak/backend/build/libs/backend-0.0.1-SNAPSHOT.jar}"
JAVA_BIN="${JAVA_BIN:-/opt/jdk/temurin-25/bin/java}"  # Java 25 필요
RUST_BIN="${RUST_BIN:-$ROOT/target/release/chalkak-server}"
mkdir -p "$OUT"

# 대상 이름 → 포트. env/<이름>.toml의 값과 같아야 한다.
app_port() { case "$1" in java-a) echo 18080;; java-b) echo 18081;; rust) echo 18082;; esac; }
db_port()  { case "$1" in java-a) echo 15432;; java-b) echo 15433;; rust) echo 15434;; esac; }

load_env() {
  # 셸 확장 없이 읽는다(BCrypt 해시의 $ 보존).
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" || "$line" == \#* ]] && continue
    export "$line"
  done < "$1"
}

start_db() {
  local name="$1" port; port="$(db_port "$name")"
  local clock="$OUT/clock-$name"
  echo "+0" > "$clock"
  docker rm -f "chalkak-harness-pg-$name" >/dev/null 2>&1 || true
  docker run -d --name "chalkak-harness-pg-$name" --user postgres \
    -e POSTGRES_DB=chalkak -e POSTGRES_USER=chalkak -e POSTGRES_PASSWORD=test-password -e TZ=Asia/Seoul \
    -e LD_PRELOAD=/faketime/libfaketime.so.1 \
    -e FAKETIME_TIMESTAMP_FILE=/clock/clock-"$name" -e FAKETIME_NO_CACHE=1 -e FAKETIME_DONT_FAKE_MONOTONIC=1 -e FAKETIME_DISABLE_SHM=1 \
    -v "$FAKETIME_LIB":/faketime/libfaketime.so.1:ro -v "$OUT":/clock:ro \
    -p "$port":5432 postgres:18.4 >/dev/null
  for _ in $(seq 60); do
    docker exec "chalkak-harness-pg-$name" pg_isready -U chalkak -d chalkak >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "DB($name)가 뜨지 않았습니다" >&2; return 1
}

start_app() {
  local name="$1" port dbp; port="$(app_port "$name")"; dbp="$(db_port "$name")"
  local log="$OUT/app-$name.log"
  [[ -f "$OUT/app-$name.pid" ]] && kill "$(cat "$OUT/app-$name.pid")" 2>/dev/null || true
  (
    load_env "$ROOT/harness/env/oracle.env"
    export SERVER_PORT="$port"
    export SPRING_DATASOURCE_URL="jdbc:postgresql://127.0.0.1:$dbp/chalkak"
    export SPRING_DATASOURCE_USERNAME=chalkak SPRING_DATASOURCE_PASSWORD=test-password
    export DATABASE_URL="host=127.0.0.1 port=$dbp user=chalkak password=test-password dbname=chalkak"
    export LD_PRELOAD="$FAKETIME_LIB" FAKETIME_TIMESTAMP_FILE="$OUT/clock-$name" FAKETIME_NO_CACHE=1 FAKETIME_DONT_FAKE_MONOTONIC=1 FAKETIME_DISABLE_SHM=1
    unset JAVA_TOOL_OPTIONS
    if [[ "$name" == rust ]]; then
      exec "$RUST_BIN"
    else
      exec "$JAVA_BIN" -XX:MaxRAMPercentage=70 -jar "$ORACLE_JAR"
    fi
  ) >"$log" 2>&1 &
  echo $! > "$OUT/app-$name.pid"
  for _ in $(seq 400); do
    curl -fsS "http://127.0.0.1:$port/actuator/health" >/dev/null 2>&1 && { echo "$name: http://127.0.0.1:$port 준비됨"; return 0; }
    kill -0 "$(cat "$OUT/app-$name.pid")" 2>/dev/null || { echo "$name 앱이 종료됐습니다. $log 확인" >&2; tail -30 "$log" >&2; return 1; }
    sleep 1
  done
  echo "$name 앱이 준비되지 않았습니다. $log 확인" >&2; return 1
}

for name in "$@"; do start_db "$name"; done
for name in "$@"; do start_app "$name"; done
