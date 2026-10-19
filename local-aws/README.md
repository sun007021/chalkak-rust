# 로컬 AWS 재현 환경 (L1·L2)

원본의 AWS 흐름 하나, **S3 → SQS → 이미지 처리 Lambda → 백엔드 HMAC 콜백**을 같은 계약(키 경로, 이벤트 모양, 재시도 규칙, HTTPS 요구)으로
docker compose 한 벌에 올린다. 백엔드는 Rust와 Java를 프로필로 바꿔 끼운다. 계획: `local-aws-app-stability-plan.md` 4~6장, 8장 L1·L2.

```text
 기기/에뮬레이터/curl ──HTTPS(로컬 CA)──▶ edge (Caddy)
                                           api.<IP>.sslip.io     ▶ backend:8080   (rust | java)
                                           *.s3.<IP>.sslip.io    ▶ aws:4566       (S3, presigned PUT / HeadObject)
                                           cdn.<IP>.sslip.io     ▶ aws:4566/techcourse-project-2026/chalkak/  (CloudFront 역할)
 backend ──IMDSv2──▶ imds-mock          S3 ObjectCreated(chalkak/staging/) ─▶ SQS chalkak-image-processing
 esm-poller ──receive──▶ SQS ──invoke──▶ lambda-image (Lambda RIE) ──presigned URL 요청·PUT·콜백(HTTPS, 같은 edge)──▶ backend
```

## 빠른 시작

```bash
cd local-aws
cp .env.example .env                 # HOST_IP 기본 127.0.0.1 (휴대폰을 붙이려면 노트북 LAN IP)
scripts/fetch-tools.sh               # 한 번: imds-mock·toxiproxy 바이너리(GitHub 릴리스) -> .state/bin
scripts/prepare-lambda.sh            # 한 번: Lambda 의존성(boto3 1.43.78, Pillow 12.3.0) -> .state/lambda-deps
( cd .. && cargo build --release -p chalkak-fakes -p chalkak-server )   # fakes 는 항상, rust 프로필이면 서버도
docker compose --profile rust up -d  # 또는 --profile java (원본 JAR: ORACLE_JAR, 기본 /tmp/chalkak-lanes/oracle/app.jar)
scripts/e2e-image.sh                 # 사인 PNG + 게시물 WebP 종단 확인
scripts/smoke.sh                     # 앱 없이 6.5 스모크: 가입 -> 로그인 -> 게시물 -> 피드 -> 승인 -> 좋아요 -> 알림 -> cdn 이미지
scripts/trace.sh <uploadId|notificationId|userId>   # DB 행·SQS·S3·서비스 로그를 한 화면에
scripts/s7.sh rust                   # S7 푸시 경로 안정성(승인 몰림, FCM 5xx·429, SQS 지연, 30분 기한, DLQ). 약 20분, docs/local-aws/report-s7.md
scripts/l5.sh rust                   # L5 안정성 시험 S1~S6·S8(compose.l5.yaml 겹침: toxiproxy DB 경로, fault-proxy S3 경로, edge 평문 8081). rust|java 를 같은 시나리오로 돌린다. docs/local-aws/report-l5.md
scripts/l5.sh compare                # 가장 최근 rust·java 결과를 놓고 A/B 기준(p99, 응답 형식) 판정
scripts/s9.sh rust --duration 8h     # S9 장시간 저부하(표본을 파일로 남기고 끝에서 판정). 1시간 검증은 --duration 1h
scripts/lambda-unit-tests.sh         # 원본 Lambda 단위 테스트(99개)를 lambda 컨테이너에서 실행
```
앱(에뮬레이터·시뮬레이터·실기기) 연결은 [`docs/local-aws/app.md`](../docs/local-aws/app.md).

백엔드 교체: `docker compose --profile rust down` 후 `--profile java up -d`. 둘 다 별칭 `backend`로 edge 뒤에 서고 같은 env 파일을 읽으므로 나머지는 그대로다.
DB는 `pgdata` 볼륨에 남는다(`docker compose down -v`로 지운다).
**Rust ↔ Java 를 바꿀 때는 `docker compose --profile rust --profile java down -v`로 DB 를 비운다.** Java(원본 HEAD)의 Flyway 가 Rust 가 아직 모르는
스키마(예: `notifications` 가 `post_id` 대신 출처 컬럼을 쓴다)로 올려서, Java 가 한 번 올린 DB 에 Rust 를 붙이면 승인 시 알림 INSERT 가 500 으로 실패한다
(L0 로 Rust 가 따라잡기 전까지의 차이). 반대로 Rust 가 만든 DB 에 Java 를 붙이는 것은 확인한 범위에서 된다(Rust → Java 순서로 smoke 통과).

### 동시성 (ESM_CONCURRENCY)
실제 Lambda 는 SQS 메시지가 몰리면 동시 실행이 늘지만 RIE 한 컨테이너는 한 번에 하나만 처리한다. 업로드 몰림(S2) 시험은 둘을 같이 늘린다.
```bash
ESM_CONCURRENCY=4 docker compose --profile rust up -d      # lambda-image 4개 + esm-poller 워커 4개 (기본 1 = 순차)
docker compose logs esm-poller | head -1                    # "concurrency": 4, lambdaTargets 4개
```
`lambda-image` 는 `deploy.replicas: ${ESM_CONCURRENCY:-1}` 로 N 개가 뜨고(컨테이너마다 `mem_limit: 1024m`), poller 는 서비스 이름의 A 레코드를 모두 풀어
워커 스레드마다 다른 컨테이너를 배정한다. 컨테이너가 모자라면(`fewer_lambda_containers` 로그) 있는 만큼만 나눠 쓴다. 이 머신(4코어)에서는 Lambda 처리가 CPU 를 쓰므로
N 을 4 이상으로 올려도 선형으로 늘지 않는다(아래 부하 발생기 측정 참고).

### 부하 발생기 업로드 흐름 (S2)
`harness/loadgen` 의 `uploads` 명령이 presign → S3 PUT → 콜백 대기를 건다(가짜 모드 회원 토큰, 앱 불필요). loadgen 에 TLS 가 없어서 백엔드 디버그 포트(`--base`)와
LocalStack 디버그 포트(`--s3`)로 직접 간다(서버가 서명한 `https://<버킷>.s3.<IP>.sslip.io/…` 를 path-style 로 바꿔 PUT). Lambda 의 콜백은 평소처럼 edge 를 거친다.
```bash
cargo build --release -p chalkak-loadgen
target/release/chalkak-loadgen uploads --base 127.0.0.1:18080 --s3 127.0.0.1:4566 \
  --secret fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210 \
  --db "postgres://chalkak:local-aws-password@127.0.0.1:15440/chalkak" \
  --rate 3 --duration 1m [--users 50 --conns 16 --timeout 7m --image 파일.webp --label rust-c4 --out up.json]
```
`--rate` 건/초를 `--duration` 동안 열린 루프로 시작하고, 끝난 뒤 `--timeout`(기본 7분 = `POST_IMAGE_PROCESSING_TIMEOUT`)까지 남은 업로드가 닫히길 기다린다.
출력: presign·PUT·**콜백 대기**(PUT 완료 → `post_image_uploads` 가 `ISSUED` 를 벗어남)·전체 지연의 p50/p95/p99, 시작 지연(lag, 발생기 포화 지표), `stuck`(닫히지 않음).
종료 코드 0 = 오류 없고 stuck 0 (S2 통과 기준 "모두 완료 또는 실패로 닫힘, 고아 0"의 완료 쪽). 회원은 `--users` 명을 시드와 같은 UUID 규칙으로 만들고 기존 데이터는 지우지 않는다.

이 머신(공유 4코어)에서 1분·3건/초(180건)로 돌려 본 값(Java, 한 번씩):

| ESM_CONCURRENCY | 완료 | 오류 | 콜백 대기 p50 / p99 | 끝난 시각 |
| ---: | ---: | ---: | ---: | ---: |
| 2 | 180/180 | 0 | 13.8초 / 29.9초 | 90초 |
| 4 | 180/180 | 0 | 11.9초 / 16.1초 | 76초 |

처리 용량(약 2건/초 안팎)이 도착률(3건/초)보다 낮아 큐가 쌓였다가 빠진다. 호스트가 다른 작업과 공유돼 절대값은 믿지 않는다(S2 본 시험은 전용 호스트에서).

## 서비스

| 서비스 | 이미지/구현 | 역할 (원본 대응) |
| --- | --- | --- |
| `edge` | `caddy:2.11`, `tls internal` | 공개 EC2의 HTTPS + CloudFront. 로컬 CA 루트: `.state/caddy/data/caddy/pki/authorities/local/root.crt`. 와일드카드 인증서 `*.s3.<IP>.sslip.io` 포함. HTTP/3은 끔 |
| `aws` | `localstack/localstack:4.9.2` (S3·SQS·SNS) | S3 버킷 `techcourse-project-2026`, 이벤트 알림, SQS 3개, SNS 알람 토픽. 초기화: `localstack/init/10-chalkak-aws.sh` |
| `postgres` | `postgres:18.4` | RDS / dev의 Docker PostgreSQL (시계 고정 없음) |
| `imds-mock` | `amazon-ec2-metadata-mock v1.13.0` `--imdsv2` | EC2 인스턴스 역할 자격증명 경로. 백엔드에는 `AWS_ACCESS_KEY_ID`를 주지 않는다 |
| `fakes` | `harness/fakes` 바이너리 | 가짜 OIDC JWKS(하네스가 서명한 ID Token을 받아들인다), Apple 토큰, Slack |
| `lambda-image` | `amazon/aws-lambda-python:3.13` + 원본 코드 마운트 | `chalkak-image-processor`. 메모리 1024MB는 `mem_limit`, 60초는 poller의 invoke 제한 |
| `esm-poller` | `esm-poller/poller.py` (Lambda 이미지의 python+boto3) | SQS 이벤트 소스 매핑 흉내 |
| `rust` / `java` | `debian:trixie-slim`+바이너리 마운트 / `eclipse-temurin:25-jre`+JAR 마운트 | 백엔드 (프로필) |
| `toxiproxy` | 바이너리 마운트, `--profile chaos` | 장애 주입(L5용). 기본 경로에는 끼워 넣지 않는다 |

### 이름과 TLS
- `HOST_IP`(기본 `127.0.0.1`)로 `api.<IP>.sslip.io`, `s3.<IP>.sslip.io`, `techcourse-project-2026.s3.<IP>.sslip.io`, `cdn.<IP>.sslip.io`가 정해진다.
  sslip.io는 이름 안의 IP로 응답하는 공개 DNS다. 인터넷이 없으면 dnsmasq 등으로 같은 이름을 같은 IP로 풀어 준다.
- 컨테이너끼리도 같은 이름으로 edge에 닿는다: edge에 네트워크 별칭을 달았다(sslip.io 해석 없이도 동작). 와일드카드 별칭은 없어서 버킷 이름을 적었다(버킷은 하나).
- HTTPS 포트는 443 고정이다. presigned URL의 호스트(SigV4 서명 대상)에 포트가 들어가면 컨테이너 안과 밖의 URL이 달라지기 때문이다. 호스트 443을 쓰는 프로세스가 있으면 먼저 비운다.
- 클라이언트가 로컬 CA를 믿어야 한다: 위 `root.crt`를 기기/시뮬레이터에 설치한다(계획 6장). Lambda·Rust·Java는 compose가 `SSL_CERT_FILE`, `AWS_CA_BUNDLE`, JVM truststore로 처리한다.

### 환경변수 계약
`env/local-aws.env`는 원본 `application.dev.env.example`의 키 이름 그대로다(`SPRING_PROFILES_ACTIVE=dev`: Lambda가 처리하는 staging 환경은 `dev`·`prod` 뿐이고
원본 `local` 프로필은 `environment: local`이라 Lambda가 반려한다). 값은 시험용 더미다. `[로컬 전용]` 블록(`AWS_ENDPOINT_URL_*`, `AWS_EC2_METADATA_SERVICE_ENDPOINT`,
`SSL_CERT_FILE`, `CHALKAK_AUTH_*`)만 빼면 #32의 실제 AWS에서 같은 파일을 쓸 수 있다. 푸시 관련 키(`NOTIFICATION_*`)는 이름만 두고 꺼 둔다(L4). 예외로 Rust 프로필은 SQS Relay(`NOTIFICATION_RELAY_ENABLED`)와 푸시 Worker(`NOTIFICATION_WORKER_ENABLED`)를 compose에서 켜서 승인·반려 알림이 `chalkak-dev-push` 큐를 거쳐 fakes의 가짜 FCM까지 가게 한다(`scripts/worker_e2e.py`, [설정 문서](../docs/notification-worker-setup.md)).
`NOTIFICATION_SQS_QUEUE_URL`은 `https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-dev-push` 형식을 유지하고, 전송 주소만 `AWS_ENDPOINT_URL_SQS=http://aws:4566`로 돌린다.

### 실계정 모드 (앱 시험)
앱의 실제 Google·Kakao·Apple 로그인으로 시험하려면 백엔드가 진짜 JWKS 와 앱과 같은 클라이언트 값을 쓰게 한다. `compose.real-oidc.yaml` 오버레이가 백엔드 `env_file` 뒤에 `env/real-oidc.env`(예시: `env/real-oidc.env.example`)를 덧붙인다.
```bash
cp env/real-oidc.env.example env/real-oidc.env       # 앱 값으로 채운다. git 에 올라가지 않는다
docker compose -f compose.yaml -f compose.real-oidc.yaml --profile rust up -d
```
가짜 모드(하네스·`smoke.sh`)로 돌아오려면 `-f compose.real-oidc.yaml` 없이 같은 명령. 값의 출처와 절차는 [`docs/local-aws/app.md`](../docs/local-aws/app.md) 5절.

## 결정 기록

### 1. 에뮬레이터: LocalStack (MinIO 대체안은 쓰지 않았다)
`localstack/localstack:4.9.2`는 인증 토큰 없이 뜬다(2026-10-19 확인: 기동 로그에 라이선스 요구 없음, `Ready.`). `latest`·2026.x 계열은 토큰을 요구하는 것으로 알려져 있어
**버전을 4.9.2로 고정**했다(tag를 올리기 전에 토큰 없이 뜨는지 먼저 확인할 것). 확인한 동작:
S3 presigned PUT·HeadObject(가상 호스트와 path-style 모두), `If-None-Match: *` 조건부 PUT, S3 `ObjectCreated` → SQS 이벤트(S3 TestEvent 포함),
SQS visibility·redrive 속성, 큐 URL이 amazonaws 형식(`https://sqs.ap-northeast-2.amazonaws.com/000000000000/...`)이어도 `endpoint_url=http://aws:4566`로 보내면 큐를 찾는다(계획 5장 #4, edge 경로 이동 불필요).
MinIO + ElasticMQ + webhook 브리지는 LocalStack이 안 뜰 때의 대체안이었으나 필요하지 않았다.

### 2. Lambda 이미지: `amazon/aws-lambda-python:3.13` (Docker Hub)
계획의 `public.ecr.aws/lambda/python:3.14`는 이 클라우드 세션에서 받을 수 없다(manifest는 응답하지만 레이어를 싣는 CloudFront가 403, 정책 차단). Docker Hub의
`amazon/aws-lambda-python`은 공식 AWS 이미지이며 3.12·3.13·latest(=3.13.0)가 있고 3.14는 없다. **가장 가까운 3.13**을 쓴다. 원본 코드·의존성(`boto3==1.43.78`, `Pillow==12.3.0`)은 그대로이고
원본 단위 테스트 99개가 3.13에서 통과한다. 3.14에서만 드러나는 차이는 이 환경에서 못 잡는다. ECR을 받을 수 있는 환경이면
`LAMBDA_IMAGE=public.ecr.aws/lambda/python:3.14 scripts/prepare-lambda.sh && LAMBDA_IMAGE=... docker compose ... up -d`로 바꾼다(의존성 휠은 그 이미지의 파이썬에 맞춰 다시 설치된다).
빌드(`docker build`)는 쓰지 않는다: 코드는 `lambda-image/function`을 마운트하고 의존성은 `.state/lambda-deps`를 마운트한다(apt·pip·ECR이 막혀도 뜨게).

### 3. Lambda 코드는 원본 사본
`lambda-image/function/`은 원본 `backend/lambda/image-processor`(기준 `ae1a2c1`) 그대로의 사본이다. 수정하지 않는다. 갱신: `scripts/sync-lambda.sh <원본 저장소>`.

### 4. Rust 서버에 더한 것 (이 PR의 Rust 변경)
- `crates/platform/aws`: `AWS_EC2_METADATA_SERVICE_ENDPOINT`를 `ImdsCredentialsProvider`가 읽는다(`with_endpoint_from_env`, 기본 체인에 연결). 시험 포함.
- `crates/platform/http`: `SSL_CERT_FILE`이 가리키는 PEM의 인증서를 `HttpClient`가 webpki-roots 외에 루트로 더한다(`env_root_certificates`). 로컬 CA의 `https://*.s3.<IP>.sslip.io`로 HeadObject를 하려면 필요하다. 시험 포함.

## 원본과 다른 점 / 한계
- 동시성: esm-poller는 순차 1건이다(RIE 한 인스턴스는 한 번에 하나). 실제 Lambda는 동시 실행으로 늘어난다. 업로드 몰림(S2) 시험 때 poller/lambda를 늘리는 방법을 정한다.
- LocalStack은 S3 서명 검증을 하지 않는다(`S3_SKIP_SIGNATURE_VALIDATION` 기본). 서명 오류·만료 URL 같은 시험은 서버 쪽 단위 시험이나 별도 도구로 한다. IAM 누락은 S3 403 주입(toxiproxy/aws 설정)으로 대신한다.
- S3 lifecycle(staging 2일 만료)은 설정만 기록되고 LocalStack이 실행하지는 않는다.
- imds-mock의 자격증명은 목 고정값(role `baskinc-role`)이라 만료 갱신 경로는 `Expiration` 값에 따라 동작이 달라진다.
- `Max Memory Used: 1024 MB`로 찍히는 RIE 로그는 에뮬레이터의 표기이며 실측치가 아니다.
- 실패 후 visibility timeout(360초)이 지나 재전달되고 `ApproximateReceiveCount`가 오르는 경로는 큐 설정(`VisibilityTimeout=360`)을 확인했고, poller가 실패한 메시지를 지우지 않는 것까지(`message_kept`)만 실행해 확인했다. 재전달 이후의 상한(`SQS_MAX_RECEIVE_COUNT`) 동작은 원본 단위 테스트(`test_handler`)가 다룬다.

## 검증 결과
`docs/local-aws/verification.md` 참고.
