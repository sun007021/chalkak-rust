#!/usr/bin/env bash
# 성능 비교(Java 대 Rust) 환경과 시험 실행. 하네스 lane과 별개의 스택이다(실제 시계, libfaketime 없음).
#
#   harness/scripts/perf.sh up                 # 빌드 -> 스택 기동(Java, Rust, 각자의 PostgreSQL, 가짜 서버) -> 시드
#   harness/scripts/perf.sh startup [N]        # 두 앱을 N번씩 재시작하며 기동 시간(컨테이너 시작 -> /actuator/health UP) 측정
#   harness/scripts/perf.sh sql                # 엔드포인트별 요청당 SQL 수·DB 시간 (pg_stat_statements)
#   harness/scripts/perf.sh bench [라운드]     # 번갈아 가며 혼합 부하(고정 RPS, 최대 처리량)와 엔드포인트별 최대 처리량
#   harness/scripts/perf.sh race               # 동시성 시험(같은 자원 동시 요청, 풀 고갈) 두 구현
#   harness/scripts/perf.sh soak <java|rust> <기간>   # 장시간 시험(예: 30m, 24h). 메모리·FD·DB 연결 추세를 1분마다 찍는다
#   harness/scripts/perf.sh report             # 결과 디렉터리를 비교해 게이트 판정(마크다운)
#   harness/scripts/perf.sh run <java|rust> <파일명> [loadgen run 옵션...]   # 임의 부하 한 번 (결과: $PERF_OUT/<면>/<파일명>.json)
#   harness/scripts/perf.sh restart <java|rust>
#   harness/scripts/perf.sh ps | down
#
# 설정(환경변수): PERF_OUT(결과 디렉터리, 기본 target/perf/latest), PERF_PORT_BASE(기본 21000), PERF_APP_CPUS(2), PERF_APP_MEM(1536m),
#   PERF_DB_CPUS(1), PERF_CONNS(64), PERF_RPS(300), PERF_DURATION(30), PERF_WARMUP(60), PERF_EP_DURATION(8), PERF_ROUNDS(3),
#   PERF_JAVA_OPTS(JVM 추가 옵션), ORACLE_JAR(기본 /tmp/chalkak-lanes/oracle/app.jar, harness/scripts/oracle-jar.sh로 만든다)
#
# 측정 전후 호스트 부하(uptime)를 결과 JSON(load_before/load_after)에 남긴다. 다른 작업이 같은 머신에서 빌드하면 흔들리므로
# 라운드마다 Java와 Rust의 순서를 바꾸고 여러 번 반복해서 노이즈를 본다(report가 최소·중앙값·최대를 함께 낸다).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
COMPOSE_FILE="$ROOT/harness/perf/compose.yaml"
export PERF_PROJECT="${PERF_PROJECT:-chalkak-perf-$(printf '%s' "$ROOT" | md5sum | cut -c1-6)}"
BASE="${PERF_PORT_BASE:-21000}"
export PERF_PORT_JAVA=$BASE PERF_PORT_PG_JAVA=$((BASE + 1)) PERF_PORT_RUST=$((BASE + 10)) PERF_PORT_PG_RUST=$((BASE + 11))
export PERF_FAKES_BIN="$ROOT/target/release/chalkak-fakes" PERF_RUST_BIN="$ROOT/target/release/chalkak-server"
export PERF_JAVA_JAR="${ORACLE_JAR:-/tmp/chalkak-lanes/oracle/app.jar}"
LOADGEN="$ROOT/target/release/chalkak-loadgen"
OUT="${PERF_OUT:-$ROOT/target/perf/latest}"
SECRET="$(grep '^ACCESS_TOKEN_SECRET=' "$ROOT/harness/env/oracle.env" | cut -d= -f2)"
CONNS="${PERF_CONNS:-64}" RPS="${PERF_RPS:-300}" DURATION="${PERF_DURATION:-30}" WARMUP="${PERF_WARMUP:-60}"
EP_DURATION="${PERF_EP_DURATION:-8}" ROUNDS="${PERF_ROUNDS:-3}"

die() { echo "perf: $*" >&2; exit 1; }
compose() { docker compose -f "$COMPOSE_FILE" "$@"; }
port_of() { case $1 in java) echo "$PERF_PORT_JAVA" ;; rust) echo "$PERF_PORT_RUST" ;; *) die "java 또는 rust" ;; esac; }
db_url() { case $1 in java) echo "host=127.0.0.1 port=$PERF_PORT_PG_JAVA user=chalkak password=test-password dbname=chalkak" ;;
  rust) echo "host=127.0.0.1 port=$PERF_PORT_PG_RUST user=chalkak password=test-password dbname=chalkak" ;; *) die "java 또는 rust" ;; esac; }
app_pid() { docker inspect -f '{{.State.Pid}}' "$(compose ps -q "$1")"; }
now_ms() { date +%s%3N; }

build() {
  (cd "$ROOT" && cargo build --release -p chalkak-server -p chalkak-fakes -p chalkak-loadgen)
}

wait_health() { # <side> [최대 초]
  local port; port=$(port_of "$1"); local limit=${2:-300} i
  for ((i = 0; i < limit * 10; i++)); do
    if curl -fs "http://127.0.0.1:$port/actuator/health" 2>/dev/null | grep -q '"UP"'; then return 0; fi
    sleep 0.1
  done
  die "$1 가 $limit 초 안에 UP이 되지 않았습니다"
}

cmd_up() {
  [[ -f "$PERF_JAVA_JAR" ]] || die "원본 JAR이 없습니다: $PERF_JAVA_JAR (harness/scripts/oracle-jar.sh)"
  build
  compose up -d --wait --quiet-pull
  cmd_seed
  echo "perf 스택 준비됨: java=:$PERF_PORT_JAVA rust=:$PERF_PORT_RUST (프로젝트 $PERF_PROJECT)"
}

cmd_seed() {
  for side in java rust; do
    wait_health "$side"
    "$LOADGEN" seed --db "$(db_url "$side")" ${PERF_SEED_ARGS:-}
  done
}

cmd_restart() {
  local side=${1:?java|rust}
  compose restart "$side" >/dev/null
  wait_health "$side"
}

cmd_startup() {
  local n=${1:-3} side i t0
  mkdir -p "$OUT"
  for side in java rust; do
    local times=()
    for ((i = 0; i < n; i++)); do
      compose stop -t 30 "$side" >/dev/null
      t0=$(now_ms)
      compose start "$side" >/dev/null
      wait_health "$side"
      times+=($(($(now_ms) - t0)))
    done
    echo "기동 시간(ms) $side: ${times[*]}"
    printf '{"kind":"startup","label":"%s","millis":[%s]}\n' "$side" "$(IFS=,; echo "${times[*]}")" > "$OUT/startup-$side.json"
  done
}

cmd_sql() {
  mkdir -p "$OUT"
  for side in java rust; do
    wait_health "$side"
    # 같은 시작 상태에서 같은 요청 열을 보내야 문장 수를 정확히 비교할 수 있다(좋아요 같은 경로는 상태에 따라 갈린다).
    "$LOADGEN" seed --db "$(db_url "$side")" ${PERF_SEED_ARGS:-}
    echo "== $side 요청당 SQL =="
    "$LOADGEN" sql --base "127.0.0.1:$(port_of "$side")" --secret "$SECRET" --db "$(db_url "$side")" \
      --label "$side" --out "$OUT/sql-$side.json"
  done
}

slug() { printf '%s' "$1" | tr -c 'A-Za-z0-9\n' '_' | cut -c1-60; }

run_one() { # <side> <파일명> 추가 인자...
  local side=$1 file=$2; shift 2
  mkdir -p "$OUT/$side"
  "$LOADGEN" run --base "127.0.0.1:$(port_of "$side")" --secret "$SECRET" --label "$side" \
    --db "$(db_url "$side")" --pid "$(app_pid "$side")" --out "$OUT/$side/$file.json" "$@" || true
}

cmd_bench() {
  local rounds=${1:-$ROUNDS} r side order
  mapfile -t eps < <("$LOADGEN" list)
  for ((r = 1; r <= rounds; r++)); do
    if ((r % 2 == 1)); then order="java rust"; else order="rust java"; fi
    for side in $order; do
      echo "=== 라운드 $r/$rounds: $side ==="
      # 고정 RPS에서는 지연을, 최대 처리량에서는 처리량과 CPU를 본다. 첫 라운드는 JIT 워밍업을 길게 준다.
      local warm=$WARMUP
      ((r > 1)) && warm=$((WARMUP / 3))
      run_one "$side" "mix-rps-r$r" --rps "$RPS" --conns "$CONNS" --duration "$DURATION" --warmup "$warm"
      run_one "$side" "mix-max-r$r" --conns "$CONNS" --duration "$DURATION" --warmup "$((warm / 3))"
      local ep
      for ep in "${eps[@]}"; do
        run_one "$side" "ep-$(slug "$ep")-r$r" --only "$ep" --conns "$CONNS" --duration "$EP_DURATION" --warmup 3
      done
    done
  done
}

cmd_race() {
  mkdir -p "$OUT"
  for side in java rust; do
    wait_health "$side"
    echo "== $side 동시성 =="
    "$LOADGEN" race --base "127.0.0.1:$(port_of "$side")" --secret "$SECRET" --db "$(db_url "$side")" \
      --label "$side" --out "$OUT/race-$side.json" || true
  done
}

cmd_soak() {
  local side=${1:?java|rust} dur=${2:?기간(예: 30m, 24h)}
  mkdir -p "$OUT/$side"
  wait_health "$side"
  "$LOADGEN" run --base "127.0.0.1:$(port_of "$side")" --secret "$SECRET" --label "$side" \
    --db "$(db_url "$side")" --pid "$(app_pid "$side")" --rps "${PERF_SOAK_RPS:-$RPS}" --conns "$CONNS" \
    --duration "$dur" --warmup 30 --interval 60 --idle-conns "${PERF_SOAK_IDLE:-50}" \
    --out "$OUT/$side/soak.json" | tee "$OUT/$side/soak.log"
}

cmd_report() {
  "$LOADGEN" report --dir "$OUT" --out "$OUT/report.md"
}

case "${1:-}" in
  up) cmd_up ;;
  seed) cmd_seed ;;
  startup) shift; cmd_startup "$@" ;;
  sql) cmd_sql ;;
  bench) shift; cmd_bench "$@" ;;
  race) cmd_race ;;
  soak) shift; cmd_soak "$@" ;;
  report) cmd_report ;;
  run) shift; side=${1:?java|rust}; file=${2:?파일명}; shift 2; run_one "$side" "$file" "$@" ;;
  restart) shift; cmd_restart "$@" ;;
  ps) compose ps ;;
  down) compose down -v --remove-orphans ;;
  *) sed -n '2,19p' "$0"; exit 2 ;;
esac
