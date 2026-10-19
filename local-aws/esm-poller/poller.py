"""Lambda 의 SQS 이벤트 소스 매핑을 흉내 내는 로컬 도구.

실제 이벤트 소스 매핑이 하는 일 중 이 프로젝트의 이미지 처리 Lambda 가 의존하는 것만 옮긴다.
- 큐를 긴 폴링(20초)으로 읽는다. 한 번에 BATCH_SIZE(기본 1)개까지.
- SQS 이벤트(Records[])를 만들어 Lambda RIE 로 동기 invoke 한다. 레코드에는 실제 이벤트와 같은 attributes
  (ApproximateReceiveCount, SentTimestamp, SenderId, ApproximateFirstReceiveTimestamp)를 싣는다.
  handler 가 ApproximateReceiveCount 로 재시도 상한(SQS_MAX_RECEIVE_COUNT)을 판정한다.
- 성공하면 삭제한다. invoke 가 실패(함수 오류, 타임아웃, 연결 실패)하면 삭제하지 않고 둔다: visibility timeout
  (360초)이 지나면 다시 전달되고 ApproximateReceiveCount 가 올라간다.
- REPORT_BATCH_ITEM_FAILURES=true 이면 응답의 batchItemFailures 에 든 messageId 만 두고 나머지는 삭제한다
  (Lambda 환경변수 SQS_PARTIAL_BATCH_RESPONSE=true 와 짝). false 이면 응답 내용과 무관하게 성공 = 전부 삭제.

실제와 다른 점: 동시성은 1(순차)이다. RIE 한 인스턴스는 한 번에 하나만 처리한다.
"""

import json
import os
import signal
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

import boto3
from botocore.config import Config

REGION = os.environ.get("AWS_REGION", "ap-northeast-2")
ENDPOINT = os.environ["SQS_ENDPOINT_URL"]
QUEUE_NAME = os.environ["QUEUE_NAME"]
INVOKE_URL = os.environ["LAMBDA_INVOKE_URL"]
BATCH_SIZE = max(1, min(10, int(os.environ.get("BATCH_SIZE", "1"))))
REPORT_BATCH_ITEM_FAILURES = os.environ.get("REPORT_BATCH_ITEM_FAILURES", "false").strip().lower() == "true"
INVOKE_TIMEOUT = float(os.environ.get("INVOKE_TIMEOUT_SECONDS", "60"))
WAIT_SECONDS = int(os.environ.get("WAIT_SECONDS", "20"))

_running = True


def log(event: str, **fields) -> None:
    print(json.dumps({"ts": datetime.now(timezone.utc).isoformat(), "event": event, **fields}, ensure_ascii=False), flush=True)


def _stop(*_):
    global _running
    _running = False


def queue_arn(sqs, url: str) -> str:
    return sqs.get_queue_attributes(QueueUrl=url, AttributeNames=["QueueArn"])["Attributes"]["QueueArn"]


def to_record(message: dict, arn: str) -> dict:
    """SQS ReceiveMessage 결과를 Lambda SQS 이벤트 레코드로 바꾼다."""
    attrs = message.get("Attributes", {})
    record = {
        "messageId": message["MessageId"],
        "receiptHandle": message["ReceiptHandle"],
        "body": message["Body"],
        "attributes": {
            "ApproximateReceiveCount": attrs.get("ApproximateReceiveCount", "1"),
            "SentTimestamp": attrs.get("SentTimestamp", str(int(time.time() * 1000))),
            "SenderId": attrs.get("SenderId", "AIDAIENQZJOLO23YVJ4VO"),
            "ApproximateFirstReceiveTimestamp": attrs.get(
                "ApproximateFirstReceiveTimestamp", str(int(time.time() * 1000))
            ),
        },
        "messageAttributes": message.get("MessageAttributes", {}),
        "md5OfBody": message.get("MD5OfBody", ""),
        "eventSource": "aws:sqs",
        "eventSourceARN": arn,
        "awsRegion": REGION,
    }
    return record


def invoke(records: list[dict]) -> tuple[bool, object]:
    """RIE 를 동기 invoke 한다. (성공 여부, 응답 본문 또는 오류 설명)."""
    body = json.dumps({"Records": records}).encode()
    request = urllib.request.Request(INVOKE_URL, data=body, method="POST", headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=INVOKE_TIMEOUT) as response:
            raw = response.read()
            function_error = response.headers.get("X-Amz-Function-Error")
    except urllib.error.HTTPError as error:
        return False, f"HTTP {error.code}: {error.read()[:300]!r}"
    except Exception as error:  # 연결 실패, 타임아웃
        return False, f"{type(error).__name__}: {error}"
    if function_error:
        return False, f"function error {function_error}: {raw[:300]!r}"
    try:
        return True, json.loads(raw) if raw else None
    except ValueError:
        return True, raw.decode(errors="replace")


def failed_ids(response: object) -> set[str]:
    if not REPORT_BATCH_ITEM_FAILURES or not isinstance(response, dict):
        return set()
    return {item["itemIdentifier"] for item in response.get("batchItemFailures", []) if "itemIdentifier" in item}


def handle_batch(sqs, url: str, arn: str, messages: list[dict]) -> None:
    records = [to_record(m, arn) for m in messages]
    started = time.monotonic()
    ok, result = invoke(records)
    elapsed_ms = int((time.monotonic() - started) * 1000)
    ids = [m["MessageId"] for m in messages]
    counts = [r["attributes"]["ApproximateReceiveCount"] for r in records]
    if not ok:
        # 지우지 않는다: visibility timeout 후 다시 전달된다.
        log("invoke_failed", messageIds=ids, receiveCounts=counts, elapsedMs=elapsed_ms, error=str(result))
        return
    failures = failed_ids(result)
    for message in messages:
        if message["MessageId"] in failures:
            log("message_kept", messageId=message["MessageId"], receiveCount=counts[ids.index(message["MessageId"])])
            continue
        sqs.delete_message(QueueUrl=url, ReceiptHandle=message["ReceiptHandle"])
        log("message_deleted", messageId=message["MessageId"], elapsedMs=elapsed_ms, result=result)


def main() -> None:
    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    sqs = boto3.client(
        "sqs",
        region_name=REGION,
        endpoint_url=ENDPOINT,
        config=Config(read_timeout=WAIT_SECONDS + 10, retries={"max_attempts": 3}),
    )
    url = None
    while _running and url is None:
        try:
            url = sqs.get_queue_url(QueueName=QUEUE_NAME)["QueueUrl"]
        except Exception as error:
            log("waiting_for_queue", queue=QUEUE_NAME, error=str(error))
            time.sleep(2)
    if url is None:
        return
    arn = queue_arn(sqs, url)
    log("started", queue=url, batchSize=BATCH_SIZE, reportBatchItemFailures=REPORT_BATCH_ITEM_FAILURES)
    while _running:
        try:
            response = sqs.receive_message(
                QueueUrl=url,
                MaxNumberOfMessages=BATCH_SIZE,
                WaitTimeSeconds=WAIT_SECONDS,
                AttributeNames=["All"],
                MessageAttributeNames=["All"],
            )
        except Exception as error:
            log("receive_failed", error=str(error))
            time.sleep(2)
            continue
        messages = response.get("Messages", [])
        if messages:
            handle_batch(sqs, url, arn, messages)


if __name__ == "__main__":
    main()
    sys.exit(0)
