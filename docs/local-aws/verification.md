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
