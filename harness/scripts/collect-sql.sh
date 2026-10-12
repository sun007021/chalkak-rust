#!/usr/bin/env bash
# Java oracle이 시나리오를 처리하는 동안 Hibernate가 보낸 SQL을 pg_stat_statements로 모아 CSV로 저장한다.
# Rust 쿼리 작성의 출발점이고, 요청당 SQL 수와 N+1·인덱스 비교의 기준선이다(전환 계획 4.5).
#
#   harness/scripts/collect-sql.sh run  [대상] [DB서비스]   # 초기화 → 시나리오 녹화 → 저장 (기본: java-b pg-java-b)
#   harness/scripts/collect-sql.sh reset [DB서비스]         # 통계만 비운다
#   harness/scripts/collect-sql.sh dump  [DB서비스]         # 지금까지 모은 SQL을 표준 출력으로 낸다
#
# 결과: harness/inventory/sql-statements.csv (query, calls, rows). 쿼리는 pg_stat_statements가 상수를 $1…로 바꾼 정규화 문장이다.
#
# 앱과 같은 사용자가 하네스(초기화·준비 SQL·덤프)도 보내므로 구분이 필요하다. 걸러내는 것:
#   - 하네스: TRUNCATE, 상태 덤프(to_jsonb), 테이블 목록(pg_tables), 준비 SQL의 `INSERT INTO <표> (id, ...)`.
#     준비 SQL은 id를 직접 넣는 형태만 걸러내므로, 시나리오가 다른 형태의 대문자 준비 SQL을 쓰면 여기에 패턴을 더한다.
#   - DB 내부: FK 검사(`FROM ONLY`, `OPERATOR(pg_catalog.…)`), JDBC 드라이버의 타입 조회(`pg_catalog.pg_type`).
#   - 트랜잭션 제어(begin/commit)·세션 설정(set/show)과 통계 확장 자신의 조회.
# Hibernate가 만드는 소문자 SQL과 앱이 직접 쓰는 네이티브 쿼리(`INSERT INTO consumed_signup_tokens … ON CONFLICT`,
# `pg_advisory_xact_lock`)는 남는다.
#
# pg_stat_statements는 DB 컨테이너를 새로 만들 때 적재된다(compose.yaml의 postgres command). 이미 떠 있던 컨테이너는
# 다시 만들어야 한다. 시나리오는 대상 DB의 모든 테이블을 비우므로 비교용 DB가 아닌 곳에서 돌릴 때 주의한다.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

cmd="${1:-}"
[[ -n "$cmd" ]] || { sed -n '2,15p' "$0"; exit 2; }

# 컨테이너 이름은 compose 프로젝트(chalkak)의 <서비스>-1.
psql_in() { docker exec -i "chalkak-$1-1" psql -U chalkak -d chalkak -v ON_ERROR_STOP=1 -q "${@:2}"; }

prepare() {
  local db="$1" libs
  libs="$(psql_in "$db" -Atc 'SHOW shared_preload_libraries')"
  if [[ "$libs" != *pg_stat_statements* ]]; then
    echo "$db에 pg_stat_statements가 적재돼 있지 않습니다. compose.yaml의 postgres command를 반영해 컨테이너를 다시 만드세요." >&2
    echo "(예: docker compose --profile selfcheck up -d --no-deps --force-recreate $db 후 앱 재기동)" >&2
    exit 1
  fi
  psql_in "$db" -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements'
}

reset_stats() { psql_in "$1" -Atc 'SELECT pg_stat_statements_reset()' >/dev/null; }

dump_stats() {
  psql_in "$1" -c "COPY (
    SELECT regexp_replace(query, '\s+', ' ', 'g') AS query, calls, rows
    FROM pg_stat_statements
    WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
      AND query !~ '^(TRUNCATE|CREATE|COPY)'
      AND query !~ 'to_jsonb\(x\)|FROM pg_tables|^INSERT INTO [a-z_]+ \(id,'
      AND query !~ 'FROM ONLY|OPERATOR\(pg_catalog|pg_catalog\.pg_type|current_schemas'
      AND query !~* '^(begin|commit|rollback|start transaction|set |show |deallocate|discard)'
      AND query !~ 'pg_stat_statements'
    ORDER BY 1
  ) TO STDOUT WITH CSV HEADER"
}

case "$cmd" in
  run)
    target="${2:-java-b}"
    db="${3:-pg-$target}"
    prepare "$db"
    reset_stats "$db"
    out="$ROOT/harness/out/golden-sql-run"
    cargo run -q -p chalkak-parity -- record --target "harness/env/$target.toml" --out "$out"
    dump_stats "$db" > "$ROOT/harness/inventory/sql-statements.csv"
    echo "저장: harness/inventory/sql-statements.csv ($(($(wc -l < "$ROOT/harness/inventory/sql-statements.csv") - 1))개 문장)"
    ;;
  reset) prepare "${2:-pg-java-b}"; reset_stats "${2:-pg-java-b}" ;;
  dump) dump_stats "${2:-pg-java-b}" ;;
  *) echo "사용법: collect-sql.sh <run|reset|dump> ..." >&2; exit 2 ;;
esac
