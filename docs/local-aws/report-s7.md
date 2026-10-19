# S7 푸시 경로 안정성 보고 (2026-10-19, 이슈 #55)

계획 7.2의 S7: 승인 몰림, 가짜 FCM 5xx·429(Retry-After), SQS 지연을 주입하고 `PENDING → PUBLISHED` 전이, 30분 기한, 10회 수신 뒤 DLQ, 중복 발송 범위를
원본 문서(`backend/deploy/docs/notification-sqs-setup.md`)와 대조한다. 환경은 클라우드 세션 컨테이너(공유 4코어), 구성은 [`local-aws/README.md`](../../local-aws/README.md).

## 재현

```bash
cd local-aws
scripts/s7.sh rust                 # 전 시나리오, 약 20분 (dlq 가 실시간 약 12분)
scripts/s7.sh rust burst expire    # 일부만
scripts/s7.sh java                 # Relay·SQS 비교(Worker 없음). 발송 시나리오(fcm503·dlq)는 건너뜀
scripts/s7.sh java-worker dlq      # Java Worker 를 켜 FCM 도달 불가 상태의 재수신·DLQ (약 13분)
```

`s7.sh`는 `down -v`로 비운 뒤 `--profile chaos`(toxiproxy)와 `SQS_ENDPOINT_URL=http://toxiproxy:14566`(백엔드의 SQS 경로를 toxiproxy `aws` 프록시로)로 스택을 띄우고
`scripts/s7_push.py`를 돌린 다음 `down -v`한다(`KEEP=1`이면 남긴다). 결과는 `local-aws/.state/s7/<백엔드>-<시각>.json`(수치와 통과 기준 목록).
`compose.s7.yaml`이 주제 푸시·공지 배포(매분 도는 작업)를 꺼서 알림 수치에 끼지 않게 한다. 시나리오마다 가짜 FCM 규칙·기록, 큐, toxiproxy 지연을 비우고 시작한다.

### 주입 방법
| 주입 | 방법 |
| --- | --- |
| 승인 몰림 | SQL로 회원·로그인 세션·푸시 기기·사진·`PENDING` 게시물 N건을 만들고, 관리자 API `PUT /api/v1/admin/posts/{id}/moderation`을 16개 동시 요청으로 건다(이미지 업로드 흐름은 `e2e-image.sh`가 이미 확인) |
| FCM 5xx / 429 | fakes의 `PUT /__admin/responses`의 `fcm` 규칙. 기기 토큰(`contains`)별로 상태·헤더(`retry-after`)·본문을 지정. 429 본문은 실제 FCM처럼 `status=RESOURCE_EXHAUSTED`와 `details[].errorCode=QUOTA_EXCEEDED` |
| SQS 지연 | toxiproxy `latency` toxic(downstream = 응답 쪽)을 `aws` 프록시에. 요청은 바로 LocalStack에 닿고 응답만 늦는다 |
| 30분 기한 | **시계를 기다리지 않는다.** 기한의 기준은 `notifications.created_at + 30분`(Relay·Worker 모두 이 값을 읽는다)이라 `UPDATE notifications SET created_at = created_at - interval '31 minutes'`로 앞당긴다. 메시지 본문의 `expiresAt` 검사는 과거 시각의 메시지를 큐에 직접 넣어 확인 |
| DLQ 10회 | 가짜 FCM이 계속 실패하게 두고 실시간으로 기다린다(Worker가 60초 이상 간격으로만 다시 받는다). 큐 자체의 이동 규칙은 같은 설정의 임시 큐로 빠르게 따로 확인 |

## 결과 (Rust)

통과 기준 31개 모두 통과(`scripts/s7.sh rust` 전체 실행 한 번, 19분). 시나리오별 소요: redrive 약 2초, burst 약 30초, fcm503 약 110초, expire 약 150초, sqsdelay 약 160초, dlq 약 11.5분.

### 큐 설정과 DLQ 이동 (redrive)
- 푸시 큐: visibility 60초, 보관 14일, redrive `maxReceiveCount=10` → `chalkak-dev-push-dlq`, DLQ 보관 14일. 원본 문서 1절 표와 같다.
- **원본과 다른 점 1, 고침:** 원본 표의 `Receive message wait time 20초`가 로컬 초기화 스크립트에 빠져 있어(0초) `localstack/init/10-chalkak-aws.sh`에 더했다. 속성은 생성 뒤 항상 다시 적게 바꿨다(이미 있는 큐와 속성이 달라 `create-queue`가 실패하던 문제도 같이 해소). Worker는 요청마다 `WaitTimeSeconds=20`을 주므로 동작 차이는 없었다.
- 같은 설정의 임시 큐에서 visibility 0으로 반복 수신하면 메시지가 **정확히 10번** 돌려지고(`ApproximateReceiveCount`가 1~10), 11번째 수신 시도에서 DLQ로 옮겨진다. AWS의 "maxReceiveCount는 DLQ로 가기 전에 받을 수 있는 횟수" 의미와 같다.

### 승인 몰림 (burst, 300건)
| 항목 | Rust | Java(Relay만, Worker 없음) |
| --- | ---: | ---: |
| 승인 요청 300건 전체 소요 | 2.2초 (p50 71ms, p99 706ms) | 10.5~12.3초 (p50 435~496ms, p99 1.5~2.0초) |
| 전부 `PUBLISHED`까지 | 23초 | 27~29초 |
| Relay 결과 | `SQS_ACCEPTED` 300 (재시도·불명 0) | 큐에 정확히 300건 |
| FCM 수락 | 300기기에 각 1번, 중복 0 | (Worker 없음) |
| 큐 비움 | 23초, DLQ 0 | |

Relay는 알림 하나씩 순차로 발행한다(한 번에 100건 조회). 승인 몰림에서 병목은 Relay가 아니라 승인 요청 자체였다. Java 쪽 승인이 느린 것은 같은 JVM 안에서 Relay와 요청 처리가 경쟁하고 기동 직후라서일 수 있어 절대값은 믿지 않는다(공유 4코어, 한 번씩 측정).

### 가짜 FCM 5xx (fcm503, Rust 전용)
503을 계속 주다 회복. 기기 10대.
- 503 동안: 기기마다 SDK가 **5번**(최초 + 재시도 4번, 0.5·1·2·4초 간격) 보낸 뒤 일시 실패. Retry-After가 없으므로 로그의 `retryAfterSeconds=60`(최소 60초)이고 숨김 시간이 61초가 된다.
- Worker 스레드가 2개라서 첫 패스는 10건에 40초(건당 약 8초 ÷ 2).
- 회복 뒤 62초 안에 10대 모두 **정확히 한 번** 수락. `receiveCount`는 모두 1과 2뿐이고 DLQ는 0, 알림 행은 `PUBLISHED` 그대로(발송 결과는 DB에 반영하지 않는다는 원본 문서 서술과 같다).

### SQS 지연 (sqsdelay)
- **2초 지연(SDK 호출 제한 5초 안):** 20건이 모두 `SQS_ACCEPTED`. Relay가 알림당 약 2.2초를 쓰므로 발행에 46초(Rust)·44초(Java). 중복 없음. 즉 SQS가 느려지면 Relay 처리량이 알림당 1/지연으로 떨어지고, 100건 배치는 지연 × 100초가 걸려 30분 기한에 가까워질 수 있다(2초면 약 200초).
- **6초 지연(제한 밖):** 발행이 `SQS_UNKNOWN`(`errorCode=SQS_CLIENT_ERROR`)으로 끝나 알림은 `PENDING`, 다음 시도 시각은 +60초. **그런데 요청은 이미 LocalStack에 닿아 큐에 들어가 있다.** 지연을 풀면 약 60초 뒤 Relay가 같은 알림을 다시 발행해 `PUBLISHED`가 된다.
  - 결과: 알림 5건이 큐에 **2번씩**(Java 큐 10건, Rust는 Worker가 둘 다 받아 FCM에 기기당 **2번**). 원본 문서의 "`SQS_UNKNOWN`은 수락 여부 불명이다 … 중복 발행될 수 있다"와 같은 범위다: 불명 한 번당 최대 한 번 더. Worker는 중복을 거르지 않는다(읽음 처리 전이면 두 번 다 보낸다).
  - Worker도 같은 느린 SQS를 쓰므로 `worker_processing_failed`(`ApiCallTimeoutException`)가 났다. 받은 메시지의 삭제·숨김 변경이 5초 제한에 걸리면 그 메시지는 지워지지 않고 60초 뒤 `receiveCount=2`로 다시 온다. 이것도 중복 발송이 되는 경로다(원본 문서에는 이 조합이 명시돼 있지 않다. 코드상 같은 설계의 결과).
- 지연이 풀리면 모두 `PUBLISHED`.

### 30분 기한 (expire)
| 지점 | 방법 | 결과 |
| --- | --- | --- |
| Relay 발행 시점 | SQS 6초 지연으로 `PENDING`(불명) 알림 2건을 만들고 `created_at`을 31분 앞당김, `next_attempt_at`을 지금으로 | SQS를 부르지 않고 **`EXPIRED`**(`sqs_published_at`·`next_attempt_at` 비움). Rust·Java 같음. 원본 문서 "`SQS_UNKNOWN`이어도 기한이 지나면 `publishStatus=EXPIRED`일 수 있다"와 일치. 불명 시도로 이미 나간 한 번은 거둘 수 없다(Rust에서는 Worker가 이미 FCM에 보냄) |
| Worker, 본문 `expiresAt` | 31분 전 발생·1분 전 만료 메시지를 큐에 직접 넣음 | FCM 호출 없이 `worker_skipped reason=EXPIRED`, 메시지 삭제 |
| Worker, 재시도 대기 중 | 429(Retry-After 7)로 숨은 메시지의 알림 `created_at`을 31분 앞당김 | 다음 수신(시각 이동 뒤 63초)에서 `EXPIRED`로 건너뛰고 삭제. FCM 요청은 처음 1건뿐, DLQ 0 |

### 10회 수신 뒤 DLQ (dlq, Rust, 실시간 693초)
FCM이 기기별로 다르게 실패하게 하고 기다린 결과.

| 기기 | FCM 응답 | 수신 횟수 | 수신 간격 | FCM 요청 | 결과 |
| --- | --- | ---: | ---: | ---: | --- |
| A | 429 + Retry-After 7 (QUOTA_EXCEEDED) | 10 | 62.0초 | 10 | 10번째 뒤 DLQ |
| B | 429 + Retry-After 120 | 6 (693초 시점) | 122.0초 | 6 | 아직 큐에 있음(간격이 길어 약 18분 뒤) |
| C | 503, Retry-After 없음 | 10 | 69.0초 | 50(수신마다 SDK 5번) | 10번째 뒤 DLQ |
| D | 정상 | 1 | | 1 | 수락 후 삭제 |
| F | 429이지만 `details` 없음 | 1 | | 1 | 영구 실패로 한 번 보내고 삭제 |

원본 문서와 대조:
- "`maxReceiveCount`는 최초 수신을 포함한 수신 횟수"와 "60초마다 정확히 10회 시도한다는 뜻은 아니다": 최소 60초 + 1초(`Retry-After` 120이면 120 + 1)이고 간격이 실제로 62초·122초라 10회에 약 10~11분, 길면 훨씬 더 걸린다.
- "Retry-After가 있으면 최소 60초 또는 해당 값만큼": Retry-After 7이어도 60초로 올려 쓴다(로그 `retryAfterSeconds=60`). 일치.
- 429는 SDK가 재시도하지 않는다(요청 10 = 수신 10). 503은 SDK가 수신마다 5번 보낸다.
- DLQ로 간 알림 행은 `PUBLISHED` 그대로. DLQ 이동 뒤 자동 Redrive는 하지 않았다(원본 문서 1절과 같은 정책).
- **주의(원본 코드와 같은 동작):** FCM의 429가 `details.errorCode=QUOTA_EXCEEDED` 없이 오면(기기 F) 재시도 대상이 아니다. 실제 FCM의 쿼터 429는 이 코드를 달고 오므로 문제는 아니지만, 게이트웨이가 만든 bare 429는 영구 실패로 처리된다.

### Java 비교
- **Relay:** 같은 주입에서 같은 전이를 보인다: 불명 → `PENDING`(+60초) → 지연 해소 뒤 재발행, 중복은 알림당 최대 2건(Java 큐 10건/알림 5건), 기한 초과 시 `EXPIRED`, 2초 지연에서 알림당 약 2.2초, 300건 모두 `PUBLISHED`·큐 300건. Java의 콘솔 로그(`dev` 프로필)에는 구조화 필드(`result`, `receiveCount`)가 없어 Rust처럼 로그로 세지 못하고 DB 상태와 큐 깊이로 비교했다.
- **FCM:** Java는 FCM 주소를 바꿀 수 없어(Admin SDK가 호스트 고정) 발송 단계(fcm503, 429, 기한 내 FCM 호출)는 비교하지 않았다.
- **DLQ(`java-worker dlq`):** Worker를 켜면 FCM(인터넷 차단) 도달 불가가 되어 모든 발송이 일시 실패다. 724초 시점에 5건 중 **2건이 DLQ**, 3건은 큐에 남았다. 이동 규칙(10회 수신 후 DLQ)은 같은 큐 설정이 하며, 수신 횟수를 로그로 센 정밀 비교는 하지 못했다(위 로그 형식 이유). Rust는 693초에 A·C 2건이 이동해 비슷한 시간대다.

## 원본과 다른 점 / 한계
- 로컬 푸시 큐 속성 하나(Receive wait 20초) 누락을 고쳤다(위). 그 외 큐 설정은 원본 문서와 같다.
- LocalStack은 visibility 만료 뒤 재수신과 DLQ 이동을 구현하지만 AWS와 시각 정밀도가 같다는 보장은 없다. 이동 시점은 "11번째 수신 시도"로 관찰했다.
- 가짜 FCM은 토큰별로 고정된 응답만 준다(횟수 제한 없음). 회복은 규칙을 지워서 만든다.
- 시계는 `created_at` 이동으로만 앞당겼다. 큐 쪽 시간(visibility, 보관 기간)은 앞당길 수 없어 실시간으로 기다렸다.
- toxiproxy 지연은 응답 방향만 건다. Relay와 Worker가 같은 SQS 경로를 쓰므로 둘이 함께 영향을 받는다(실제 SQS 지연도 그렇다).
- 호스트가 공유 4코어라 절대 시간은 참고용이다. 이 문서의 수치는 한 번씩 측정했다.
- 앱 단말 수신(실기기 푸시)은 범위 밖이다(계획 2절: 앱에 Messaging이 아직 없다).
