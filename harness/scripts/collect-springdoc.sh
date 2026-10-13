#!/usr/bin/env bash
# Java oracle의 springdoc이 돌려주는 OpenAPI 문서와 Swagger UI 정적 파일을 crates/server/assets/springdoc/에 저장한다.
# 서버(web::springdoc)가 non-prod 프로필에서 원본과 같은 응답으로 내보내는 원본 데이터다.
# `collect-openapi.sh`가 저장하는 harness/inventory/openapi/는 키를 정렬하고 servers를 지운 비교용 사본이라
# 바이트 단위로 같은 응답을 만들 수 없어 따로 받는다.
#
#   harness/scripts/collect-springdoc.sh [기준URL]      # 기본 http://127.0.0.1:18080 (java-a)
#
# 문서 안의 서버 주소(`servers[0].url`)는 요청의 Host로 만들어지므로 기준 주소를 `{{BASE_URL}}`로 바꿔 저장한다.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE="${1:-http://127.0.0.1:18080}"
OUT="$ROOT/crates/server/assets/springdoc"
mkdir -p "$OUT/swagger-ui"

doc() {
  local url="$1" file="$2"
  curl -fsS -m 30 "$BASE$url" | sed "s|$BASE|{{BASE_URL}}|g" > "$OUT/$file"
  echo "저장: ${OUT#$ROOT/}/$file"
}
asset() {
  local name="$1"
  curl -fsS -m 60 "$BASE/swagger-ui/$name" > "$OUT/swagger-ui/$name"
  echo "저장: ${OUT#$ROOT/}/swagger-ui/$name"
}

doc /v3/api-docs api-docs.json
doc /v3/api-docs.yaml api-docs.yaml
for group in user-api admin-api internal-api; do
  doc "/v3/api-docs/$group" "$group.json"
done
doc /v3/api-docs/swagger-config swagger-config.json

for name in index.html index.css swagger-initializer.js oauth2-redirect.html swagger-ui.css \
  swagger-ui-bundle.js swagger-ui-standalone-preset.js favicon-32x32.png favicon-16x16.png; do
  asset "$name"
done
