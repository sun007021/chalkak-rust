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
scripts/lambda-unit-tests.sh         # 원본 Lambda 단위 테스트(99개)를 lambda 컨테이너에서 실행
```

백엔드 교체: `docker compose --profile rust down` 후 `--profile java up -d`. 둘 다 별칭 `backend`로 edge 뒤에 서고 같은 env 파일을 읽으므로 나머지는 그대로다.
DB는 `pgdata` 볼륨에 남는다(`docker compose down -v`로 지운다).

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
`SSL_CERT_FILE`, `CHALKAK_AUTH_*`)만 빼면 #32의 실제 AWS에서 같은 파일을 쓸 수 있다. 푸시 관련 키(`NOTIFICATION_*`)는 이름만 두고 꺼 둔다(L4).
`NOTIFICATION_SQS_QUEUE_URL`은 `https://sqs.ap-northeast-2.amazonaws.com/000000000000/chalkak-dev-push` 형식을 유지하고, 전송 주소만 `AWS_ENDPOINT_URL_SQS=http://aws:4566`로 돌린다.

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
