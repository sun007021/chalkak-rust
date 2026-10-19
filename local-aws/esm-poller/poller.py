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

동시성: ESM_CONCURRENCY=N 이면 워커 스레드 N개가 각자 큐를 읽고 Lambda 컨테이너 하나씩을 맡는다(실제 Lambda 의 동시 실행 N 에 해당).
RIE 한 인스턴스는 한 번에 하나만 처리하므로 컨테이너도 N개여야 한다: compose 가 같은 ESM_CONCURRENCY 로 lambda-image 를 N개
띄우고(deploy.replicas), poller 는 서비스 이름의 A 레코드를 모두 풀어 워커마다 다른 컨테이너를 배정한다. 기본 1 은 순차다.
"""

import json
import os
import signal
import socket
import sys
import threading
import time
from urllib.parse import urlsplit, urlunsplit
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
CONCURRENCY = max(1, int(os.environ.get("ESM_CONCURRENCY", "1")))

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


def invoke(invoke_url: str, records: list[dict]) -> tuple[bool, object]:
    """RIE 를 동기 invoke 한다. (성공 여부, 응답 본문 또는 오류 설명)."""
    body = json.dumps({"Records": records}).encode()
    request = urllib.request.Request(invoke_url, data=body, method="POST", headers={"Content-Type": "application/json"})
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


def handle_batch(sqs, url: str, arn: str, messages: list[dict], invoke_url: str, worker: int) -> None:
    records = [to_record(m, arn) for m in messages]
    started = time.monotonic()
    ok, result = invoke(invoke_url, records)
    elapsed_ms = int((time.monotonic() - started) * 1000)
    ids = [m["MessageId"] for m in messages]
    counts = [r["attributes"]["ApproximateReceiveCount"] for r in records]
    if not ok:
        # 지우지 않는다: visibility timeout 후 다시 전달된다.
        log("invoke_failed", worker=worker, messageIds=ids, receiveCounts=counts, elapsedMs=elapsed_ms, error=str(result))
        return
    failures = failed_ids(result)
    for message in messages:
        if message["MessageId"] in failures:
            log("message_kept", worker=worker, messageId=message["MessageId"], receiveCount=counts[ids.index(message["MessageId"])])
            continue
        sqs.delete_message(QueueUrl=url, ReceiptHandle=message["ReceiptHandle"])
        log("message_deleted", worker=worker, messageId=message["MessageId"], elapsedMs=elapsed_ms, result=result)


def make_sqs():
    return boto3.client(
        "sqs",
        region_name=REGION,
        endpoint_url=ENDPOINT,
        config=Config(read_timeout=WAIT_SECONDS + 10, retries={"max_attempts": 3}),
    )


def invoke_urls() -> list[str]:
    """워커마다 쓸 Lambda 주소. 호스트 이름이 여러 컨테이너로 풀리면 IP 별로 나눈다(동시성 N = 컨테이너 N)."""
    parts = urlsplit(INVOKE_URL)
    deadline = time.monotonic() + 30
    addrs: list[str] = []
    while True:
        try:
            infos = socket.getaddrinfo(parts.hostname, parts.port or 80, socket.AF_INET, socket.SOCK_STREAM)
            addrs = sorted({info[4][0] for info in infos})
        except OSError:
            addrs = []
        if len(addrs) >= CONCURRENCY or time.monotonic() >= deadline:
            break
        time.sleep(1)
    if not addrs:
        return [INVOKE_URL] * CONCURRENCY
    if len(addrs) < CONCURRENCY:
        log("fewer_lambda_containers", wanted=CONCURRENCY, found=len(addrs))
    urls = [urlunsplit(parts._replace(netloc=f"{addr}:{parts.port}" if parts.port else addr)) for addr in addrs]
    return [urls[i % len(urls)] for i in range(CONCURRENCY)]


def worker_loop(worker: int, url: str, arn: str, target: str) -> None:
    sqs = make_sqs()
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
            log("receive_failed", worker=worker, error=str(error))
            time.sleep(2)
            continue
        messages = response.get("Messages", [])
        if messages:
            handle_batch(sqs, url, arn, messages, target, worker)


def main() -> None:
    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    sqs = make_sqs()
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
    targets = invoke_urls()
    log("started", queue=url, batchSize=BATCH_SIZE, reportBatchItemFailures=REPORT_BATCH_ITEM_FAILURES,
        concurrency=CONCURRENCY, lambdaTargets=sorted(set(targets)))
    threads = [threading.Thread(target=worker_loop, args=(i, url, arn, targets[i]), daemon=True) for i in range(CONCURRENCY)]
    for thread in threads:
        thread.start()
    while _running and any(t.is_alive() for t in threads):
        time.sleep(0.5)
    for thread in threads:
        thread.join(timeout=WAIT_SECONDS + 15)


if __name__ == "__main__":
    main()
    sys.exit(0)
