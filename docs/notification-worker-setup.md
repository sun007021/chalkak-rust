# 푸시 Worker·FCM 설정

원본 `backend/deploy/docs/notification-sqs-setup.md`의 Worker·FCM 부분에 해당한다. 큐·DLQ·IAM 준비와 Relay 설정은 원본 문서와 같고,
여기에는 Rust 서버에서 Worker를 켜는 데 필요한 값과 원본과 다른 점만 둔다.

## 흐름

승인·반려·주제·공지 알림 → Relay(`NOTIFICATION_RELAY_ENABLED`)가 SQS 발행 → Worker(`NOTIFICATION_WORKER_ENABLED`)가 수신 →
최신 DB 조건 확인 → 기기마다 FCM HTTP v1 전송 → 처리 끝난 메시지 삭제. 일시 실패는 삭제하지 않아 다음 수신에서 다시 처리한다.

## 환경변수

| 키 | 설명 |
| --- | --- |
| `NOTIFICATION_WORKER_ENABLED` | `true`면 Worker 시작. 켜면 아래 FCM 값과 큐 URL이 모두 있어야 하고, 없으면 기동을 거절한다 |
| `NOTIFICATION_FCM_PROJECT_ID` | 기기 토큰을 발급한 Firebase 프로젝트 ID |
| `NOTIFICATION_FCM_CREDENTIALS_PATH` | 서비스 계정 JSON 절대 경로. 실행 계정만 읽게 하고 Git에 넣지 않는다 |
| `NOTIFICATION_FCM_ENDPOINT_URL` | **원본에 없는 설정.** 비우면 `https://fcm.googleapis.com`. 로컬 스택·하네스가 가짜 FCM 서버를 쓰려고 만들었다. 운영에서는 비운다 |
| `NOTIFICATION_SQS_QUEUE_URL`, `AWS_REGION`, `AWS_ENDPOINT_URL_SQS` | Relay와 같은 값. 큐는 하나다 |

OAuth 토큰 주소는 원본처럼 서비스 계정 JSON의 `token_uri`를 따른다(가짜 서버를 쓰려면 이 값을 바꾼다).

## 수신 설정

한 번에 1건, 긴 폴링 20초, 처리 중 숨김 시간 60초, `ApproximateReceiveCount` 요청. 처리하는 동안 숨김 시간을 연장하는 하트비트가 돈다.
FCM 연결·읽기 제한은 각각 10초다.

## 원본과 다른 점

- 종료 신호를 받으면 진행 중인 긴 폴링(최대 20초)이 끝날 때까지 기다린다. 원본은 스레드를 인터럽트한다.
- 하트비트와 FCM 재시도 대기는 인터럽트로 끊지 않는다.
- `Instant`가 ±9999년을 넘는 메시지 시각은 `InvalidFormatException`으로 거절한다(원본은 ±10억 년). 운영 메시지에서는 생기지 않는다.
- SQS 오류 이름 매핑은 근사치이며, 로그의 예외 이름이 원본과 다를 수 있다.
- FCM 전송은 Admin SDK 대신 HTTP v1 REST를 직접 호출한다. 서비스 계정 JWT(RS256)로 토큰을 받는다.

## 로그 확인

`stage=fcm_result`(기기별 결과), Worker 처리 결과 로그 필드는 원본과 같다. Worker가 소비 중인 큐를 콘솔에서 반복 Poll하면 수신 횟수가
늘므로 처리 추적은 로그를 우선한다.

## 로컬 확인

`local-aws`의 Rust 프로필은 Relay와 Worker를 켜고, FCM은 `chalkak-fakes`(가짜 FCM·OAuth)로 돌린다. 절차와 결과는
[`local-aws/verification.md`](local-aws/verification.md)의 푸시 Worker 절을 본다.
