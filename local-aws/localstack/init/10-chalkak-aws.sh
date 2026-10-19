#!/usr/bin/env bash
# LocalStack 가 Ready 가 되면 실행된다(/etc/localstack/init/ready.d).
# 원본 AWS 설정값을 한곳에 모았다. 이 값들은 #32 IaC 로 옮길 때 그대로 재사용한다.
#
# 근거:
#  - backend/lambda/image-processor/README.md "SQS와 S3 연결": chalkak-image-processing 은 Standard 큐,
#    visibility 360초, 보관 4일, long polling 20초, DLQ 없음. S3 ObjectCreated, prefix chalkak/staging/, suffix 없음.
#    staging prefix 는 lifecycle 로 1~2일 뒤 만료.
#  - backend/deploy/docs/notification-sqs-setup.md: chalkak-dev-push visibility 60초, 보관 14일(1209600초),
#    DLQ chalkak-dev-push-dlq(보관 14일), redrive maxReceiveCount=10.
set -euo pipefail
rm -f /tmp/local-aws-init.done

REGION="${AWS_DEFAULT_REGION:-ap-northeast-2}"
ACCOUNT_ID="000000000000"
BUCKET="${S3_BUCKET:-techcourse-project-2026}"
ROOT_PREFIX="${S3_PREFIX:-chalkak}"

IMAGE_QUEUE="chalkak-image-processing"
PUSH_QUEUE="chalkak-dev-push"
PUSH_DLQ="chalkak-dev-push-dlq"

aws_() { awslocal --region "$REGION" "$@"; }
arn() { echo "arn:aws:sqs:${REGION}:${ACCOUNT_ID}:$1"; }

echo "[init] bucket $BUCKET"
aws_ s3api create-bucket --bucket "$BUCKET" \
  --create-bucket-configuration "LocationConstraint=${REGION}" >/dev/null 2>&1 || true

echo "[init] queues"
aws_ sqs create-queue --queue-name "$IMAGE_QUEUE" --attributes '{
  "VisibilityTimeout": "360",
  "MessageRetentionPeriod": "345600",
  "ReceiveMessageWaitTimeSeconds": "20"
}' >/dev/null

# 푸시 큐·DLQ 는 이름만으로 만들고 속성은 아래에서 적는다(create-queue 는 속성이 다르면 이미 있는 큐에서 실패한다).
aws_ sqs create-queue --queue-name "$PUSH_DLQ" >/dev/null
aws_ sqs create-queue --queue-name "$PUSH_QUEUE" >/dev/null

# create-queue 는 이미 있는 큐의 속성을 바꾸지 못하므로 항상 속성을 다시 적는다(재시작해도 같은 값).
image_url="$(aws_ sqs get-queue-url --queue-name "$IMAGE_QUEUE" --query QueueUrl --output text)"
aws_ sqs set-queue-attributes --queue-url "$image_url" --attributes \
  VisibilityTimeout=360,MessageRetentionPeriod=345600,ReceiveMessageWaitTimeSeconds=20

# 푸시 큐도 같은 이유로 다시 적는다(notification-sqs-setup.md 표: Visibility 60초, Receive wait 20초, redrive 10회, DLQ 보관 14일).
push_url="$(aws_ sqs get-queue-url --queue-name "$PUSH_QUEUE" --query QueueUrl --output text)"
aws_ sqs set-queue-attributes --queue-url "$push_url" --attributes "{
  \"VisibilityTimeout\": \"60\",
  \"MessageRetentionPeriod\": \"1209600\",
  \"ReceiveMessageWaitTimeSeconds\": \"20\",
  \"RedrivePolicy\": \"{\\\"deadLetterTargetArn\\\":\\\"$(arn "$PUSH_DLQ")\\\",\\\"maxReceiveCount\\\":10}\"
}"
dlq_url="$(aws_ sqs get-queue-url --queue-name "$PUSH_DLQ" --query QueueUrl --output text)"
aws_ sqs set-queue-attributes --queue-url "$dlq_url" --attributes MessageRetentionPeriod=1209600

echo "[init] s3 event notification: s3:ObjectCreated:* prefix ${ROOT_PREFIX}/staging/ -> $IMAGE_QUEUE"
aws_ s3api put-bucket-notification-configuration --bucket "$BUCKET" --notification-configuration "{
  \"QueueConfigurations\": [{
    \"Id\": \"image-processing\",
    \"QueueArn\": \"$(arn "$IMAGE_QUEUE")\",
    \"Events\": [\"s3:ObjectCreated:*\"],
    \"Filter\": {\"Key\": {\"FilterRules\": [{\"Name\": \"prefix\", \"Value\": \"${ROOT_PREFIX}/staging/\"}]}}
  }]
}"

echo "[init] staging lifecycle (2 days)"
aws_ s3api put-bucket-lifecycle-configuration --bucket "$BUCKET" --lifecycle-configuration "{
  \"Rules\": [{
    \"ID\": \"expire-staging\",
    \"Status\": \"Enabled\",
    \"Filter\": {\"Prefix\": \"${ROOT_PREFIX}/staging/\"},
    \"Expiration\": {\"Days\": 2}
  }]
}"

echo "[init] sns alarm topic"
aws_ sns create-topic --name chalkak-dev-alarms >/dev/null

touch /tmp/local-aws-init.done
echo "[init] done"
