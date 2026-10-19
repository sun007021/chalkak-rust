#!/usr/bin/env bash
# 업로드·알림·회원 하나가 로컬 AWS 흐름에서 어디까지 갔는지 한 화면에 보여 준다(계획 6.5).
#   scripts/trace.sh <uploadId|notificationId|userId>
#
# 출력 순서: 대상 식별 -> DB 행(post_image_uploads, users 사인 처리 상태, posts, notifications) -> SQS 큐 깊이·속성
#           -> S3 객체 -> 같은 시간대의 backend·esm-poller·lambda-image 로그.
# 로그는 백엔드가 요청 경로를 템플릿으로 남기므로(/{uploadId}) ID 로 바로 찾을 수 없다. 대신 DB 행의 시각(created_at ~ updated_at)으로
# 시간 창을 잡아 그 안의 콜백·Lambda 줄을 보여 준다. 환경변수: TRACE_LINES(줄 수 상한, 기본 40), TRACE_PAD(창 여유 초, 기본 3).
set -uo pipefail
cd "$(dirname "$0")/.."

id="${1:-}"
if [[ ! "$id" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
  echo "사용법: scripts/trace.sh <uploadId|notificationId|userId>  (UUID)" >&2
  exit 2
fi
id="$(tr 'A-F' 'a-f' <<<"$id")"
LINES_MAX="${TRACE_LINES:-40}"
PAD="${TRACE_PAD:-3}"
REGION=ap-northeast-2
PROJECT_QUEUES_ENDPOINT="http://aws:4566"

psql_q() { docker compose exec -T postgres psql -q -U chalkak -d chalkak -v ON_ERROR_STOP=1 "$@" 2>&1; }
psql_val() { psql_q -tA -c "$1" | head -1; }
section() { printf '\n==== %s\n' "$1"; }
show() { # 제목과 SQL: 행이 없으면 "(없음)"
  local title="$1" sql="$2" out
  out="$(psql_q -c "$sql")"
  printf '%s\n' "-- $title"
  if grep -qE '^\(0 rows\)' <<<"$out"; then echo "(없음)"; else printf '%s\n' "$out"; fi
}

if ! docker compose ps --status running --format '{{.Service}}' 2>/dev/null | grep -qx postgres; then
  echo "postgres 가 떠 있지 않다. local-aws 에서 docker compose up 을 먼저 한다." >&2
  exit 1
fi

# ---- 1. 식별 --------------------------------------------------------------------------------
kind="$(psql_val "
  SELECT CASE
    WHEN EXISTS (SELECT 1 FROM post_image_uploads WHERE id = '$id') THEN 'post-upload'
    WHEN EXISTS (SELECT 1 FROM notifications WHERE id = '$id') THEN 'notification'
    WHEN EXISTS (SELECT 1 FROM users WHERE id = '$id') THEN 'user'
    WHEN EXISTS (SELECT 1 FROM users WHERE pending_signature_upload_id = '$id') THEN 'signature-upload'
    WHEN EXISTS (SELECT 1 FROM posts WHERE id = '$id') THEN 'post'
    ELSE 'unknown' END")"
user_id=""
case "$kind" in
  user) user_id="$id" ;;
  post-upload) user_id="$(psql_val "SELECT user_id FROM post_image_uploads WHERE id = '$id'")" ;;
  notification) user_id="$(psql_val "SELECT user_id FROM notifications WHERE id = '$id'")" ;;
  signature-upload) user_id="$(psql_val "SELECT id FROM users WHERE pending_signature_upload_id = '$id'")" ;;
  post) user_id="$(psql_val "SELECT user_id FROM posts WHERE id = '$id'")" ;;
esac
section "대상: $id ($kind)${user_id:+, 회원 $user_id}"
if [[ "$kind" == unknown ]]; then
  echo "DB 에 없는 ID 다. 사인 업로드는 가입 전에는 DB 행이 없을 수 있다(S3·로그만 본다)."
fi

# ---- 2. DB -----------------------------------------------------------------------------------
section "DB"
case "$kind" in
  post-upload)
    show "post_image_uploads" "SELECT id, user_id, status, rejection_reason, created_at, updated_at, claimed_at, expires_at, image_metadata IS NOT NULL AS has_exif FROM post_image_uploads WHERE id = '$id'"
    show "이 업로드를 쓰는 게시물" "SELECT id, topic_id, moderation_status, created_at, moderated_at FROM posts WHERE post_image_upload_id = '$id'"
    ;;
  user | signature-upload | notification | post)
    : # 아래 회원 공통 조회에서 보여 준다
    ;;
esac
if [[ "$kind" == notification ]]; then
  show "notifications" "SELECT id, user_id, post_id, type, read_at, rejection_reason, created_at, left(title, 30) AS title FROM notifications WHERE id = '$id'"
  show "알림을 만든 감사 로그(event_key)" "SELECT a.id, a.action, a.target_id, a.occurred_at FROM admin_audit_logs a JOIN notifications n ON n.event_key = a.id WHERE n.id = '$id'"
fi
if [[ "$kind" == post ]]; then
  show "posts" "SELECT id, user_id, topic_id, moderation_status, post_image_upload_id, created_at, moderated_at, deleted_at FROM posts WHERE id = '$id'"
fi
if [[ -n "$user_id" ]]; then
  show "users (사인 처리 상태: pending_signature_upload_id·status·started_at)" "SELECT id, status, signature_processing_status, pending_signature_upload_id, signature_processing_started_at, left(signature_original_storage_key, 60) AS signature_key, created_at FROM users WHERE id = '$user_id'"
  if [[ "$kind" != post-upload ]]; then
    show "post_image_uploads (최근 5)" "SELECT id, status, rejection_reason, created_at, updated_at, claimed_at FROM post_image_uploads WHERE user_id = '$user_id' ORDER BY created_at DESC LIMIT 5"
  fi
  if [[ "$kind" != post ]]; then
    show "posts (최근 5)" "SELECT id, moderation_status, post_image_upload_id, created_at, moderated_at FROM posts WHERE user_id = '$user_id' ORDER BY created_at DESC LIMIT 5"
  fi
  if [[ "$kind" != notification ]]; then
    show "notifications (최근 10)" "SELECT id, type, read_at IS NOT NULL AS read, post_id, created_at FROM notifications WHERE user_id = '$user_id' ORDER BY created_at DESC, id DESC LIMIT 10"
  fi
fi
show "처리 중으로 남은 것 (ISSUED 업로드 / PROCESSING 사인, 전체)" "SELECT
  (SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED') AS issued_uploads,
  (SELECT count(*) FROM post_image_uploads WHERE status = 'ISSUED' AND expires_at < now()) AS expired_unclaimed,
  (SELECT count(*) FROM users WHERE signature_processing_status = 'PROCESSING') AS signature_processing,
  (SELECT count(*) FROM users WHERE signature_processing_status = 'FAILED') AS signature_failed,
  (SELECT count(*) FROM posts WHERE moderation_status = 'VALIDATING') AS posts_validating"

# ---- 3. SQS ----------------------------------------------------------------------------------
section "SQS 큐 깊이·속성 (visible / in-flight / delayed, 가시성 제한 초, 재전달 상한)"
queues="$(docker compose exec -T aws awslocal --region "$REGION" sqs list-queues --query 'QueueUrls[]' --output text 2>/dev/null | tr '\t' '\n')"
if [[ -z "$queues" ]]; then
  echo "(큐를 읽지 못했다: aws 컨테이너 확인)"
else
  printf '%-28s %8s %9s %8s %6s %6s %s\n' queue visible inflight delayed 'vis(s)' maxRcv retention
  while read -r url; do
    [[ -z "$url" ]] && continue
    attrs="$(docker compose exec -T aws awslocal --region "$REGION" sqs get-queue-attributes --queue-url "$url" --attribute-names All --output json 2>/dev/null)"
    python3 -I -c '
import json, sys
url = sys.argv[1]
a = json.load(sys.stdin).get("Attributes", {})
redrive = json.loads(a["RedrivePolicy"]) if "RedrivePolicy" in a else {}
print("%-28s %8s %9s %8s %6s %6s %ss" % (
    url.rsplit("/", 1)[-1], a.get("ApproximateNumberOfMessages", "?"), a.get("ApproximateNumberOfMessagesNotVisible", "?"),
    a.get("ApproximateNumberOfMessagesDelayed", "?"), a.get("VisibilityTimeout", "?"), redrive.get("maxReceiveCount", "-"),
    a.get("MessageRetentionPeriod", "?")))' "$url" <<<"$attrs"
  done <<<"$queues"
fi

# ---- 4. S3 -----------------------------------------------------------------------------------
section "S3 객체 (키에 $id 가 든 것)"
objs="$(docker compose exec -T aws awslocal --region "$REGION" s3 ls "s3://techcourse-project-2026/chalkak/" --recursive 2>/dev/null | grep -F "$id" || true)"
if [[ -n "$objs" ]]; then echo "$objs"; else echo "(없음: 사인·게시물 객체 키는 업로드 ID 를 쓴다. 회원/알림 ID 면 업로드 ID 로 다시 본다)"; fi

# ---- 5. 로그 ---------------------------------------------------------------------------------
# 시간 창: 대상 업로드(없으면 회원의 최근 업로드)의 created_at ~ max(updated_at, 지금) + 여유.
window_sql="SELECT (extract(epoch FROM min(created_at)) - $PAD)::bigint, (extract(epoch FROM least(max(updated_at) + interval '$PAD seconds', now())))::bigint"
case "$kind" in
  post-upload) rows_sql="$window_sql FROM post_image_uploads WHERE id = '$id'" ;;
  notification) rows_sql="$window_sql FROM (SELECT created_at, created_at AS updated_at FROM notifications WHERE id = '$id') t" ;;
  user | signature-upload) rows_sql="$window_sql FROM (SELECT created_at, updated_at FROM post_image_uploads WHERE user_id = '$user_id' UNION ALL SELECT created_at, updated_at FROM users WHERE id = '$user_id') t" ;;
  post) rows_sql="$window_sql FROM posts WHERE id = '$id'" ;;
  *) rows_sql="" ;;
esac
since="" until=""
if [[ -n "$rows_sql" ]]; then
  IFS='|' read -r since until < <(psql_q -tA -c "$rows_sql" | head -1)
fi
log_args=(--no-log-prefix)
window_note="시간 창 없음(최근 로그)"
if [[ -n "$since" && -n "$until" ]]; then
  log_args+=(--since "$since" --until "$until")
  window_note="$(date -d "@$since" '+%H:%M:%S') ~ $(date -d "@$until" '+%H:%M:%S')"
else
  log_args+=(--tail 200)
fi

logs() { # 서비스..., 거를 정규식
  local pattern="$1"; shift
  docker compose logs "${log_args[@]}" "$@" 2>/dev/null | { if [[ -n "$pattern" ]]; then grep -E "$pattern" || true; else cat; fi; } | tail -n "$LINES_MAX" | cut -c1-400
}

backend_services=()
for svc in rust java; do
  if docker compose ps --status running --format '{{.Service}}' 2>/dev/null | grep -qx "$svc"; then backend_services+=("$svc"); fi
done
section "backend 로그 [${backend_services[*]:-없음}] 창 $window_note: 이 ID·회원, 내부 콜백(/internal/v1), 검수, 오류"
if ((${#backend_services[@]})); then
  logs "${id}|${user_id:-__none__}|/internal/v1|\"type\":\"moderation\"|\"level\":\"(WARN|ERROR)\"|WARN|ERROR" "${backend_services[@]}"
else
  echo "(rust/java 백엔드가 떠 있지 않다)"
fi

section "esm-poller 로그 창 $window_note (message_deleted=성공, message_kept·invoke_failed=재시도 대기)"
logs "" esm-poller

section "lambda-image 로그 창 $window_note (복수 컨테이너면 모두; Lambda 오류·콜백 실패 줄)"
logs "ERROR|Error|Traceback|WARN|INVOKE|Exception|callback|status" lambda-image

echo
echo "[trace] 끝. 줄 수를 늘리려면 TRACE_LINES=200, 창을 넓히려면 TRACE_PAD=30 로 다시 부른다."
