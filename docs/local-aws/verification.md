# L1·L2 검증 결과 (2026-10-19)

환경: 클라우드 세션 컨테이너(Docker 29.8.2, x86_64). 모든 이미지는 공개 이미지 + 호스트 파일 마운트.
Rust: `feat/44-local-aws`의 `target/release/chalkak-server`(origin/main 기준 51개 API). Java: `/tmp/chalkak-lanes/oracle/app.jar`(harness BASELINE `0abc3f9`).

| 확인 | 명령 | 결과 |
| --- | --- | --- |
| 종단(Rust) | `local-aws/scripts/e2e-image.sh` (`--profile rust`, 자격증명은 IMDS) | 통과 |
| 종단(Java) | 같은 스크립트 (`--profile java`) | 통과 |
| 종단(Java, `HOST_IP=172.17.0.1`) | 같은 스크립트 | 통과 (이름이 `*.172.17.0.1.sslip.io`로 바뀌어도 동작) |
| 원본 Lambda 단위 테스트 | `local-aws/scripts/lambda-unit-tests.sh` (Lambda 컨테이너, Python 3.13) | 99개 통과 |
| Rust 단위 | `cargo test -p chalkak-aws -p chalkak-http` | aws 12(자격증명)+8+15, http 97+14+4+5+23 통과 (새 시험: IMDS 엔드포인트 환경변수 2, `SSL_CERT_FILE` 3) |
| 서식·린트 | `cargo fmt --all --check`, `cargo clippy -p chalkak-aws -p chalkak-http --all-targets -- -D warnings` | 통과 |
| SQS URL(계획 5장 #4) | amazonaws 형식 `QueueUrl` + `endpoint_url=http://aws:4566`로 send/receive/delete | 동작, 속성(visibility 60, 보관 14일, redrive 10) 확인 |
| 이벤트 소스 매핑 | 백엔드를 끈 채 업로드 → Lambda 예외 → `message_kept` | 메시지를 지우지 않고 둠 |

종단 흐름이 확인하는 것(Rust·Java 공통):

1. 가짜 Google ID Token(`harness/keys`로 서명)으로 사인 업로드 URL 발급 → URL이 `https://techcourse-project-2026.s3.<IP>.sslip.io/chalkak/staging/dev/signatures/{id}.png`
2. presigned PUT(SignedHeaders대로 헤더 채움) → S3 이벤트 → SQS → esm-poller → Lambda → `upload-urls` 200 → 결과 PUT → `complete` 204 → 가입 성공
3. `signatureOriginalImageUrl`·`signatureThumbnailImageUrl`(cdn)을 GET 200
4. 게시물: 업로드 URL → WebP(EXIF 포함) PUT → 게시물 작성(`VALIDATING`) → 콜백 후 `/posts/today`가 `PENDING` → 관리자(`operator`) 승인 → 상세의 `originalImageUrl`·`thumbnailImageUrl`(cdn) GET 200

관찰: 이미지 한 건 처리(Lambda invoke)는 100~700ms, 스크립트 전체는 약 5초.

## L3 검증 결과 (2026-10-19)

같은 환경(클라우드 세션 컨테이너, 공유 4코어). 에뮬레이터·시뮬레이터·실기기는 이 세션에 없어서 **앱 쪽(`docs/local-aws/app.md`의 기기 명령)은 실행하지 못했다**. 서버 쪽은 아래처럼 확인했다.

| 확인 | 명령 | 결과 |
| --- | --- | --- |
| 6.5 스모크(Rust) | `local-aws/scripts/smoke.sh` (`--profile rust`, 빈 DB) | 통과 (9초): 가입 → 로그인·refresh → 게시물 → 오늘 PENDING·승인 전 피드 비노출 → 승인 → 피드 cdn 이미지 3종 → 좋아요 등록·멱등·취소 → 알림 POST_APPROVED·읽음 |
| 6.5 스모크(Java) | 같은 스크립트 (`--profile java`, 빈 DB) | 통과 (14초) |
| 추적 스크립트 | `scripts/trace.sh <업로드ID>`·`<알림ID>`·`<회원ID>`·존재하지 않는 UUID·잘못된 인자 | DB 행, SQS, S3, 시간 창 로그 출력(Rust·Java). 잘못된 인자는 종료 코드 2 |
| 실계정 오버레이 | `docker compose -f compose.yaml -f compose.real-oidc.yaml --profile rust up -d` | 백엔드 env 가 앱 값·진짜 JWKS 로 바뀌고 기동(healthy), 가짜 ID Token 은 401. 진짜 구글·카카오·애플 서버에서 키를 받는 것은 외부 접속 제한으로 못 해 봤다 |
| poller 동시성 | `ESM_CONCURRENCY=3` 후 `e2e-image.sh`, `=2`·`=4` 후 부하 | lambda-image N개 + 워커 N개로 기동(`concurrency`·`lambdaTargets` 로그), 종단 통과 |
| 업로드 부하 | `chalkak-loadgen uploads --rate 3 --duration 1m` (Java) | 180건 모두 READY·오류 0·stuck 0. 콜백 대기 p50 13.8초(동시성 2) → 11.9초(동시성 4). 처리 용량 약 2건/초 < 도착 3건/초라 큐가 쌓였다 빠짐 |
| loadgen 단위·린트 | `cargo test -p chalkak-loadgen uploads`, `cargo clippy -p chalkak-loadgen --all-targets -- -D warnings`, `cargo fmt --all --check` | 5 통과, 경고 0 |

발견: **Java 가 올린 DB 에 Rust 를 붙이면** `notifications.post_id` 컬럼이 없어 관리자 승인이 500(`postgres error: db error`)이 된다. Java 원본 HEAD 의 스키마가 현재 Rust(main)보다 앞서 있기 때문이다(L0 범위). 백엔드를 바꿀 때 `down -v` 로 DB 를 비운다.
