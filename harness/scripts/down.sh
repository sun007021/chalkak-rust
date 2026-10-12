#!/usr/bin/env bash
# up.sh로 띄운 서비스를 모두 내린다. DB 데이터는 컨테이너와 함께 사라진다(볼륨 없음).
set -euo pipefail
cd "$(dirname "$0")/../.."
docker compose --profile selfcheck --profile rust down -v --remove-orphans
