#!/usr/bin/env bash
# Java oracle의 springdoc이 만든 OpenAPI 명세를 harness/inventory/openapi/에 저장한다.
# 생성형 차등 퍼징(전환 계획 4.3 ④)의 입력이고, 엔드포인트·스키마 목록의 기준이다.
#
#   harness/scripts/collect-openapi.sh [기준URL]      # 기본 http://127.0.0.1:18080 (java-a)
#
# springdoc은 local 프로필에서만 켜진다(prod는 api-docs.enabled=false). 그룹별 문서(user-api, admin-api,
# internal-api)와 전체 문서를 저장한다. 서버 주소(servers)는 실행 포트에 따라 달라지므로 지우고, 키 순서는 정렬해
# 같은 코드에서 같은 파일이 나오게 한다.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE="${1:-http://127.0.0.1:18080}"
OUT="$ROOT/harness/inventory/openapi"
mkdir -p "$OUT"

fetch() {
  local url="$1" file="$2"
  curl -fsS -m 30 "$BASE$url" | python3 -I -c '
import json, sys
doc = json.load(sys.stdin)
doc.pop("servers", None)
json.dump(doc, sys.stdout, ensure_ascii=False, indent=2, sort_keys=True)
sys.stdout.write("\n")
' > "$OUT/$file"
  echo "저장: harness/inventory/openapi/$file"
}

fetch /v3/api-docs openapi.json
for group in user-api admin-api internal-api; do
  fetch "/v3/api-docs/$group" "openapi-$group.json"
done
