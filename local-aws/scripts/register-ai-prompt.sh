#!/usr/bin/env bash
# 관리자 API 로 AI 사진 리뷰 프롬프트 버전을 새로 만든다(만든 버전은 바꿀 수 없고, 유형별 최신 버전이 쓰인다).
#   scripts/register-ai-prompt.sh [시스템프롬프트파일] [모델] [reasoningEffort]
# 기본: prompts/photo-review-system.txt, gpt-6-luna, MEDIUM. API 주소는 API_BASE(기본 http://127.0.0.1:18080).
# 관리자 계정은 로컬 기본값(operator / operator-password). ADMIN_USERNAME, ADMIN_PASSWORD 로 바꾼다.
set -euo pipefail
cd "$(dirname "$0")/.."
file="${1:-prompts/photo-review-system.txt}"; model="${2:-gpt-6-luna}"; effort="${3:-MEDIUM}"
base="${API_BASE:-http://127.0.0.1:18080}"
[[ -f "$file" ]] || { echo "프롬프트 파일이 없다: $file" >&2; exit 1; }

token="$(curl -fsS -X POST "$base/api/v1/admin/auth/login" -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,os;print(json.dumps({"username":os.environ.get("ADMIN_USERNAME","operator"),"password":os.environ.get("ADMIN_PASSWORD","operator-password")}))')" \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["accessToken"])')"

body="$(FILE="$file" MODEL="$model" EFFORT="$effort" python3 -c '
import json,os
print(json.dumps({"type":"PHOTO_REVIEW","systemPrompt":open(os.environ["FILE"],encoding="utf-8").read().strip(),
 "userPromptTemplate":"{\"주제\": \"{{topicTitle}}\"}","model":os.environ["MODEL"],
 "parameters":{"reasoningEffort":os.environ["EFFORT"]},"memo":"로컬 등록 스크립트"}, ensure_ascii=False))')"

curl -fsS -X POST "$base/api/v1/admin/ai-prompt-versions" -H "Authorization: Bearer $token" \
  -H 'Content-Type: application/json' --data-binary "$body" | python3 -c '
import json,sys;d=json.load(sys.stdin);print("만든 버전:",d.get("version"),d.get("model"),d.get("id"))'
