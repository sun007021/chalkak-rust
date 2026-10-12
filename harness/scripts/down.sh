#!/usr/bin/env bash
# up.sh로 띄운 앱과 DB를 모두 내린다.
set -uo pipefail
OUT="$(cd "$(dirname "$0")/.." && pwd)/out"
for pid in "$OUT"/app-*.pid; do [[ -f "$pid" ]] && kill "$(cat "$pid")" 2>/dev/null; rm -f "$pid"; done
docker ps -aq --filter name=chalkak-harness-pg- | xargs -r docker rm -f >/dev/null
