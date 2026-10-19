#!/usr/bin/env bash
# 하네스 lane 관리. lane 하나 = 앱 1개(rust 또는 java) + 전용 PostgreSQL + 전용 가짜 외부 서버 + 전용 시계 파일.
# lane끼리는 아무것도 공유하지 않으므로 여러 lane에 시나리오를 나눠 동시에 돌릴 수 있다(parity --target a,b,c).
#
#   harness/scripts/lane.sh up rust [개수]    # 이 작업 트리의 target/release 바이너리로 Rust lane을 띄운다(기본 1개)
#   harness/scripts/lane.sh up java [개수]    # 원본 JAR로 Java oracle lane을 띄운다. 작업 트리와 무관하게 모두가 함께 쓴다
#   harness/scripts/lane.sh restart rust      # 이 작업 트리의 Rust lane 앱만 다시 띄운다(cargo build 후)
#   harness/scripts/lane.sh targets rust|java [--txlog|--relay|--worker]
#                                             # 쓸 수 있는 lane의 대상 설정 경로를 쉼표로 잇는다(parity --target 값).
#                                             # --txlog이면 TXLOG=1로 띄운 lane만(parity txshape용),
#                                             # --relay이면 RELAY=1, --worker이면 WORKER=1로 띄운 lane만. 아무것도 없으면 기본 lane만
#   harness/scripts/lane.sh ls
#   harness/scripts/lane.sh down rust|java|<lane 이름>|all
#
# lane 상태는 $CHALKAK_LANES_DIR(기본 /tmp/chalkak-lanes)/<이름>/ 에 있다: 시계 파일 clock, 대상 설정 target.toml,
# 사용 중 잠금 lock(parity가 실행 동안 잡는다), Rust lane이면 주인 작업 트리 owner.
# Rust lane은 그 lane을 띄운 작업 트리의 바이너리를 마운트하므로 그 작업 트리만 쓴다(targets rust가 거른다).
# Java lane은 기준 커밋이 같으면 누구나 쓴다. parity가 lane 잠금을 잡으므로 여러 작업이 같은 Java lane을 동시에 쓰지 않는다.
#
# TXLOG=1 harness/scripts/lane.sh up rust|java [개수]: DB 문장 로그를 켠 lane(`log_statement=all`, 세션·application_name 등을
#   줄 머리에 붙여 <lane 디렉터리>/pglog/postgres.log에 쓴다). 로그가 늘어나므로 평소 lane은 끄고 `parity txshape`가 쓸
#   때만 켠다. target.toml에 pg_log_file이 들어가고 lane 디렉터리에 txlog 표시 파일이 생긴다.
#
# RELAY=1 harness/scripts/lane.sh up rust|java [개수]: SQS Relay를 켠 lane(harness/env/oracle-relay.env를 더한다). 앱이 스스로
#   도는 작업을 확인하는 시나리오(`lane = "relay"`)만 이 lane에서 돈다. target.toml에 flavor = "relay"가 들어가고 lane
#   디렉터리에 relay 표시 파일이 생긴다. 기본 lane에는 영향이 없다(targets가 서로 섞지 않는다).
#
# WORKER=1 harness/scripts/lane.sh up rust|java [개수]: SQS Relay와 푸시 Worker를 함께 켠 lane(harness/env/oracle-worker.env를 더한다,
#   서비스 계정 파일은 harness/keys/fcm-service-account.json). `lane = "worker"` 시나리오만 이 lane에서 돈다. target.toml에
#   flavor = "worker"가 들어가고 lane 디렉터리에 worker 표시 파일이 생긴다. 원본의 FCM 주소(fcm.googleapis.com)는 바꿀 수 없어
#   Java는 FCM을 부르지 않는 경로만 시나리오로 확인한다(harness/README.md의 worker lane).
#
# 포트: lane 번호 N마다 20000+N*10 (앱), +1 (DB), +2 (가짜 서버). 번호는 잠금 아래에서 비어 있는 가장 작은 수로 정한다.
#
# 필요한 것: Docker(Compose v2), 호스트의 libfaketime(apt install libfaketime, 경로는 FAKETIME_LIB로 바꿀 수 있다),
#   Rust lane: cargo build --release -p chalkak-server -p chalkak-fakes
#   Java lane: 원본 JAR(ORACLE_JAR, 기본 $CHALKAK_LANES_DIR/oracle/app.jar). 만드는 법은 harness/README.md
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LANES_DIR="${CHALKAK_LANES_DIR:-/tmp/chalkak-lanes}"
COMPOSE_FILE="$ROOT/harness/lane/compose.yaml"
FAKETIME_LIB="${FAKETIME_LIB:-$(ls /usr/lib/*/faketime/libfaketime.so.1 2>/dev/null | head -n1 || true)}"
ORACLE_JAR="${ORACLE_JAR:-$LANES_DIR/oracle/app.jar}"
TOKEN_SECRET="fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
mkdir -p "$LANES_DIR"

die() { echo "lane: $*" >&2; exit 1; }

lane_env() { # <이름> <번호> <종류>
  local name=$1 n=$2 kind=$3 base=$((20000 + $2 * 10))
  export LANE_PROJECT="chalkak-lane-$name" LANE_DIR="$LANES_DIR/$name"
  export LANE_APP_PORT=$base LANE_DB_PORT=$((base + 1)) LANE_FAKES_PORT=$((base + 2))
  export FAKETIME_LIB
  export LANE_FAKES_BIN="${LANE_FAKES_BIN:-$ROOT/target/release/chalkak-fakes}"
  export LANE_RUST_BIN="$ROOT/target/release/chalkak-server"
  export LANE_JAVA_JAR="$ORACLE_JAR"
  if is_worker "$name"; then
    export LANE_ENV_EXTRA="$ROOT/harness/env/oracle-worker.env"
  elif is_relay "$name"; then
    export LANE_ENV_EXTRA="$ROOT/harness/env/oracle-relay.env"
  else
    unset LANE_ENV_EXTRA
  fi
}

# RELAY=1로 띄운 lane인가(lane 디렉터리의 표시 파일).
is_relay() { [[ -f "$LANES_DIR/$1/relay" ]]; }

# WORKER=1로 띄운 lane인가(Relay도 함께 켜져 있다).
is_worker() { [[ -f "$LANES_DIR/$1/worker" ]]; }

# TXLOG=1로 띄운 lane인가(lane 디렉터리의 표시 파일).
is_txlog() { [[ -f "$LANES_DIR/$1/txlog" ]]; }

# 문장 로그를 켜는 변수(TXLOG lane). 줄 머리: 세션 id|application_name|사용자|트랜잭션 id|가상 트랜잭션 id|시각|
# (harness/parity/src/txlog.rs가 읽는다)
txlog_env() {
  export LANE_PG_LOG_STATEMENT=all LANE_PG_LOGGING_COLLECTOR=on LANE_PG_LOG_DIR=/pglog
  export LANE_PG_LOG_PREFIX='@@%c|%a|%u|%x|%v|%m|'
}

compose() { # <종류> 인자...
  local kind=$1; shift
  local files=(-f "$COMPOSE_FILE")
  [[ -n "${LANE_PG_LOG_STATEMENT:-}" ]] && files+=(-f "$ROOT/harness/lane/compose.txlog.yaml")
  docker compose "${files[@]}" --profile "$kind" "$@"
}

used_numbers() {
  for d in "$LANES_DIR"/*/; do [[ -f "$d/number" ]] && cat "$d/number"; done | sort -n
}

# lane 이름 목록(종류별). Rust는 이 작업 트리 것만.
# 둘째 인자: --txlog(문장 로그 lane만) | --relay(Relay lane만) | --worker(Worker lane만) | --all(종류의 모든 lane) | 없음(기본 lane만).
lanes_of() { # <종류> [--txlog|--relay|--worker|--all]
  local kind=$1 only=${2:-}
  for d in "$LANES_DIR"/"$kind"-*/; do
    [[ -f "$d/target.toml" ]] || continue
    [[ $only == --txlog && ! -f "$d/txlog" ]] && continue
    [[ $only == --relay && ! -f "$d/relay" ]] && continue
    [[ $only == --worker && ! -f "$d/worker" ]] && continue
    # Worker lane은 Relay도 켜져 있지만 relay 표시 파일은 따로 만들지 않는다(종류가 하나씩이다).
    [[ $only != --relay && $only != --worker && $only != --all && ( -f "$d/relay" || -f "$d/worker" ) ]] && continue
    if [[ $kind == rust && "$(cat "$d/owner" 2>/dev/null)" != "$ROOT" ]]; then continue; fi
    basename "$d"
  done
}

write_target() { # <이름> <종류> <번호>
  local name=$1 kind=$2 base=$((20000 + $3 * 10)) settle
  # Java는 큰 시각 이동 직후 응답 없이 연결을 끊는 경우가 있어 시계를 바꾼 뒤 기다린다(target.rs settle 참고).
  if [[ $kind == java ]]; then settle=200; else settle=50; fi
  cat > "$LANES_DIR/$name/target.toml" <<EOF
name = "$name"
base_url = "http://127.0.0.1:$base"
db_url = "host=127.0.0.1 port=$((base + 1)) user=chalkak password=test-password dbname=chalkak"
clock_file = "$LANES_DIR/$name/clock"
access_token_secret = "$TOKEN_SECRET"
fakes_url = "http://127.0.0.1:$((base + 2))"
lock_file = "$LANES_DIR/$name/lock"
clock_settle_ms = $settle
EOF
  if is_worker "$name"; then
    echo 'flavor = "worker"' >> "$LANES_DIR/$name/target.toml"
  elif is_relay "$name"; then
    echo 'flavor = "relay"' >> "$LANES_DIR/$name/target.toml"
  fi
  if is_txlog "$name"; then
    echo "pg_log_file = \"$LANES_DIR/$name/pglog/postgres.log\"" >> "$LANES_DIR/$name/target.toml"
  fi
}

check_inputs() { # <종류>
  [[ -n "$FAKETIME_LIB" && -f "$FAKETIME_LIB" ]] || die "libfaketime이 없습니다(apt install libfaketime 또는 FAKETIME_LIB=경로)"
  [[ -x "$ROOT/target/release/chalkak-fakes" ]] || die "cargo build --release -p chalkak-fakes 가 필요합니다"
  case $1 in
    rust) [[ -x "$ROOT/target/release/chalkak-server" ]] || die "cargo build --release -p chalkak-server 가 필요합니다" ;;
    java)
      [[ -f "$ORACLE_JAR" ]] || die "원본 JAR이 없습니다: $ORACLE_JAR (harness/scripts/oracle-jar.sh로 만든다)"
      local stamp
      stamp="$(cat "$(dirname "$ORACLE_JAR")/BASELINE" 2>/dev/null || true)"
      [[ -z "$stamp" || "$stamp" == "$(cat "$ROOT/harness/BASELINE")" ]] \
        || die "원본 JAR의 기준 커밋($stamp)이 harness/BASELINE과 다릅니다. harness/scripts/oracle-jar.sh로 다시 만드세요"
      ;;
    *) die "종류는 rust 또는 java" ;;
  esac
}

cmd_up() { # <종류> [개수]
  local kind=${1:-} count=${2:-1} names=() name n
  check_inputs "$kind"
  # 번호 고르기와 디렉터리 만들기는 잠금 아래에서 한다(여러 작업 트리가 동시에 up 할 수 있다).
  exec 9>"$LANES_DIR/.alloc.lock"
  flock 9
  for ((i = 0; i < count; i++)); do
    n=1
    while used_numbers | grep -qx "$n"; do n=$((n + 1)); done
    name="$kind-$n"
    mkdir -p "$LANES_DIR/$name"
    chmod a+rx "$LANES_DIR/$name"
    echo "$n" > "$LANES_DIR/$name/number"
    echo "+0" > "$LANES_DIR/$name/clock"
    chmod a+r "$LANES_DIR/$name/clock"
    [[ $kind == rust ]] && echo "$ROOT" > "$LANES_DIR/$name/owner"
    if [[ "${TXLOG:-}" == 1 ]]; then
      # postgres 사용자(uid 999)가 쓸 수 있어야 한다.
      mkdir -p "$LANES_DIR/$name/pglog"
      chmod 777 "$LANES_DIR/$name/pglog"
      touch "$LANES_DIR/$name/txlog"
    fi
    [[ "${RELAY:-}" == 1 ]] && touch "$LANES_DIR/$name/relay"
    [[ "${WORKER:-}" == 1 ]] && touch "$LANES_DIR/$name/worker"
    names+=("$name")
  done
  flock -u 9
  # 동시에 띄운다. Java는 기동이 오래 걸려 한꺼번에 띄우는 편이 빠르다.
  local pids=()
  for name in "${names[@]}"; do
    (
      lane_env "$name" "$(cat "$LANES_DIR/$name/number")" "$kind"
      if is_txlog "$name"; then txlog_env; fi
      compose "$kind" up -d --wait --quiet-pull >"$LANES_DIR/$name/up.log" 2>&1 \
        || { cat "$LANES_DIR/$name/up.log" >&2; exit 1; }
      write_target "$name" "$kind" "$(cat "$LANES_DIR/$name/number")"
    ) &
    pids+=($!)
  done
  local failed=0
  for p in "${pids[@]}"; do wait "$p" || failed=1; done
  ((failed == 0)) || die "일부 lane을 띄우지 못했습니다(위 로그)"
  for name in "${names[@]}"; do echo "$LANES_DIR/$name/target.toml"; done
}

cmd_restart() { # rust
  local kind=${1:-rust} name
  for name in $(lanes_of "$kind" --all); do
    (lane_env "$name" "$(cat "$LANES_DIR/$name/number")" "$kind"
     if is_txlog "$name"; then txlog_env; fi
     compose "$kind" up -d --wait --force-recreate --no-deps "$kind" >/dev/null 2>&1) &
  done
  wait
}

cmd_targets() { # <종류>
  local out=()
  for name in $(lanes_of "${1:?종류}" "${2:-}"); do out+=("$LANES_DIR/$name/target.toml"); done
  local IFS=,
  echo "${out[*]}"
}

cmd_ls() {
  printf "%-10s %-6s %-40s %s\n" LANE PORT OWNER STATE
  for d in "$LANES_DIR"/*/; do
    [[ -f "$d/number" ]] || continue
    local name n state owner
    name=$(basename "$d"); n=$(cat "$d/number")
    owner=$(cat "$d/owner" 2>/dev/null || echo "(공용)")
    if [[ -f "$d/target.toml" ]]; then state=ready; else state=starting; fi
    [[ -f "$d/txlog" ]] && state="$state+txlog"
    [[ -f "$d/relay" ]] && state="$state+relay"
    [[ -f "$d/worker" ]] && state="$state+worker"
    if [[ -f "$d/lock" ]] && ! flock -n "$d/lock" true 2>/dev/null; then state=busy; fi
    printf "%-10s %-6s %-40s %s\n" "$name" "$((20000 + n * 10))" "$owner" "$state"
  done
}

cmd_down() { # rust|java|<이름>|all
  local what=${1:?무엇을 내릴지: rust, java, lane 이름, all} names=() name
  case $what in
    all) for d in "$LANES_DIR"/*/; do [[ -f "$d/number" ]] && names+=("$(basename "$d")"); done ;;
    rust | java) names=($(lanes_of "$what" --all)) ;;
    *) names=("$what") ;;
  esac
  for name in "${names[@]}"; do
    [[ -f "$LANES_DIR/$name/number" ]] || continue
    (
      lane_env "$name" "$(cat "$LANES_DIR/$name/number")" "${name%%-*}"
      docker compose -p "$LANE_PROJECT" -f "$COMPOSE_FILE" --profile rust --profile java down -v --remove-orphans >/dev/null 2>&1 || true
      rm -rf "${LANES_DIR:?}/$name"
    ) &
  done
  wait
}

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  restart) shift; cmd_restart "$@" ;;
  targets) shift; cmd_targets "$@" ;;
  ls) cmd_ls ;;
  down) shift; cmd_down "$@" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
