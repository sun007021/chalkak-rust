# 차등 검증 하네스

원본 Java 서버(oracle)와 Rust 서버(candidate)에 같은 시나리오를 보내고, 정규화한 응답·DB 상태를 비교한다.
설계 배경은 [전환 계획 4장](../docs/plan/migration-plan.md#4-검증-하네스-가장-먼저-만든다)에 있다.

## 구성

| 경로 | 내용 |
| --- | --- |
| `parity/` | 시나리오 러너, 정규화, 비교기, ID Token 서명 (`chalkak-parity`) |
| `fakes/` | 가짜 외부 서버 (`chalkak-fakes`): OIDC JWKS, Apple 토큰 교환·철회, Slack Webhook, S3 HeadObject. 받은 요청을 기록한다 |
| `keys/` | 가짜 JWKS와 러너가 ID Token 서명에 쓰는 테스트 RSA 키 쌍 (비밀 아님) |
| `scenarios/` | 시나리오(TOML). 형식은 `parity/src/scenario.rs` 상단 주석 참고 |
| `golden/` | Java oracle에서 녹화한 정규화 결과 |
| `env/` | 대상별 설정(`java-a`, `java-b`, `rust`)과 앱 공통 환경변수(`oracle.env`) |
| `scripts/up.sh`, `down.sh` | docker compose로 대상(PostgreSQL + 앱 + 시계 파일 + fakes) 기동·정리. 서비스 정의는 저장소 루트 `compose.yaml` |
| `scripts/collect-sql.sh` | Java oracle이 시나리오를 처리하는 동안 보낸 SQL을 `pg_stat_statements`로 모아 저장 |
| `scripts/collect-openapi.sh` | Java oracle의 `/v3/api-docs`(springdoc) 저장 |
| `compose.fakes-local.yaml` | `compose.yaml`에 겹쳐 fakes를 호스트에서 빌드한 바이너리로 띄우는 설정 |
| `inventory/` | 원본에서 뽑은 것: API 목록(`endpoints.csv`), OpenAPI 명세(`openapi/`), SQL 목록(`sql-statements.csv`), Java 테스트 이식 추적표(`java-test-inventory.csv`), 추출 스크립트 |
| `BASELINE` | oracle로 쓰는 원본 커밋 |

## 실행

필요한 것: Docker(Compose v2). 원본 Java 이미지는 처음 한 번 `BASELINE` 커밋을 받아 컨테이너 안에서 빌드한다.

```bash
# Java oracle(java-a) + Rust 개발용 DB(pg-rust)
harness/scripts/up.sh
# Rust 서버 컨테이너까지
harness/scripts/up.sh rust
# Java 대 Java: 하네스 자체의 결정성 확인 (Phase 0 완료 조건)
harness/scripts/up.sh selfcheck
cargo run -p chalkak-parity -- compare --left harness/env/java-a.toml --right harness/env/java-b.toml

# Java 대 Rust
cargo run -p chalkak-parity -- compare --left harness/env/java-a.toml --right harness/env/rust.toml

# golden 녹화와 검증
cargo run -p chalkak-parity -- record --target harness/env/java-a.toml --out harness/golden
cargo run -p chalkak-parity -- verify --target harness/env/rust.toml --golden harness/golden

harness/scripts/down.sh
```

컨테이너 안에서 cargo·apt 네트워크를 쓸 수 없는 환경에서는 fakes 이미지를 빌드하지 않고, 호스트에서 만든 바이너리를
이미 있는 `debian:trixie-slim`에 마운트한다.

```bash
cargo build --release -p chalkak-fakes
FAKES_LOCAL=1 COMPOSE_UP_ARGS="--no-build" harness/scripts/up.sh selfcheck
# 대상 서비스만 다시 만들 때: docker compose -f compose.yaml -f harness/compose.fakes-local.yaml --profile selfcheck up -d --no-build --no-deps java-a java-b fakes
```

| 서비스 | 호스트 포트 | 내용 |
| --- | --- | --- |
| `fakes` | 19000 | 가짜 외부 서버. 세 대상이 하나를 공유하고 시나리오마다 초기화한다 |
| `java-a` / `pg-java-a` | 18080 / 15432 | 원본 Spring 서버(oracle)와 DB |
| `java-b` / `pg-java-b` | 18081 / 15433 | 결정성 확인용 두 번째 Java (`selfcheck` 프로필) |
| `rust` / `pg-rust` | 18082 / 15434 | Rust 서버(`rust` 프로필)와 DB. 개발 중에는 `pg-rust`만 띄우고 `cargo run`으로 붙어도 된다 |

모든 앱·DB 컨테이너는 libfaketime이 들어간 이미지(`docker/*.Dockerfile`)로 뜨고, `harness/out/clock-<대상>` 파일의 오프셋을 현재 시각에 더한다.

## 가짜 외부 서버(fakes)

한 프로세스·한 포트(컨테이너 9000, 호스트 19000)가 원본이 부르는 외부 서비스를 흉내 내고 받은 요청을 모두 기록한다.
러너가 시나리오 시작 때 초기화하고 응답을 지정하며, 끝나면 기록을 가져와 정규화해 결과의 `fakes`에 넣어 비교한다.

| 경로 | 흉내 내는 것 | 응답 |
| --- | --- | --- |
| `GET /oidc/google/certs`, `/oidc/kakao/jwks.json`, `/oidc/apple/keys` | 제공자 JWKS | 테스트 RSA 공개키(`keys/jwks.json`). 제공자마다 경로가 달라 기록으로 구분 |
| `POST /apple/auth/token`, `/apple/auth/revoke` | Apple 토큰 교환·철회 | 시나리오가 지정. 지정하지 않으면 500 |
| `POST /slack/...` | Slack Incoming Webhook | 본문 기록. 지정하지 않으면 200 `ok` |
| `HEAD /<키>` (가상 호스트 `<버킷>.fakes`) 또는 `HEAD /<버킷>/<키>` | S3 HeadObject | 키별 크기·Content-Type·상태를 시나리오가 지정. 지정하지 않은 키는 404 |
| `PUT /__admin/responses`, `GET /__admin/requests`, `POST /__admin/reset`, `GET /__admin/health` | 관리 | 응답 지정(통째로 교체), 기록 조회, 초기화 |

ID Token은 러너가 `keys/oidc-test-private.pem`으로 RS256 서명해 만든다. **발급자(issuer)는 실제 값을 그대로** 두고
공개키 주소만 바꾸므로 원본의 발급자 검증도 시나리오에 걸린다.

### 원본 설정에서 바꾼 것(`env/oracle.env`)

Spring은 `chalkak.*` 속성을 환경변수로 덮어쓸 수 있다. Rust 서버도 같은 환경변수를 읽어 가짜 서버를 본다.

| 환경변수 | 값 | 바꾸는 속성 |
| --- | --- | --- |
| `CHALKAK_AUTH_OIDC_{GOOGLE,KAKAO,APPLE}_JWKSETURI` | `http://fakes:9000/oidc/...` | `chalkak.auth.oidc.*.jwk-set-uri` |
| `CHALKAK_AUTH_APPLE_TOKEN_TOKENURI`, `..._REVOKEURI` | `http://fakes:9000/apple/auth/{token,revoke}` | `chalkak.auth.apple.token.token-uri`, `revoke-uri` |
| `AWS_ENDPOINT_URL_S3` | `http://fakes:9000` | AWS SDK 공통 환경변수. HeadObject뿐 아니라 **presigned URL의 호스트도** `test-bucket.fakes:9000`으로 바뀐다 |
| `CHALKAK_IMAGE_ANONYMOUSACCESS` | `false` | `chalkak.image.anonymous-access`. local 프로필의 익명 자격증명으로는 presigned URL을 만들 수 없다("Only SigV4 signers are supported") |

**Slack은 가짜 서버로 돌릴 수 없다.** 원본의 `SlackNotificationProperties`가 `https://hooks.slack.com/services/…`
(포트 없음)만 받는다. 그래서 oracle은 `ADMIN_NOTIFICATION_DELIVERY_ENABLED=false`를 유지하고, fakes의 Slack 경로는
URL 검증이 느슨한 Rust 서버용이다. 원본의 Slack 본문 형식은 Java 단위 테스트 이식으로 확인한다.

## 시나리오 형식 추가분

기본 형식은 `parity/src/scenario.rs` 상단 주석에 있다. 외부 연동을 쓰려면 대상 설정(`env/*.toml`)에 `fakes_url`이 있어야 한다.

```toml
db_opaque = ["encrypted_refresh_token"]      # DB 덤프에서 길이만 비교할 컬럼(무작위 IV 암호문, 해시 등)

[[id_token]]                                  # 시나리오 시작 시 서명해 ${googleNew} 변수로 쓴다
name = "googleNew"
provider = "google"                           # google | kakao | apple → iss·aud 기본값
sub = "google-sub-1"
nonce = "raw-nonce-1"                         # 원본 nonce. 토큰에는 SHA-256 hex가 들어간다
email = "a@example.com"
email_verified = true
# issued_in, expires_in(시계 기준 초), aud, iss, kid, claims, omit, tamper_signature

[fakes.apple_token]                           # 시작 시 응답 지정. apple_revoke, slack, s3."<키>"도 같은 방식
status = 200
body = '''{"access_token":"a","token_type":"Bearer","expires_in":3600,"refresh_token":"r","id_token":"${apple}"}'''

[[step]]
name = "..."
# ...
[step.fakes.s3."chalkak/signatures/local/original/${uploadId}.png"]   # 이 단계 전에 덧씌움(이후에도 유지)
size = 2048
content_type = "image/png"
```

- 응답 지정과 s3 키의 `${변수}`는 단계마다 현재 변수 값으로 채운다. 그래서 앞 단계에서 `capture`한 `uploadId`로 키를 지정할 수 있다.
- 비교에서 **빼는 것**: JWKS 조회(앱의 공개키 캐시 상태로 횟수가 달라진다), 요청 헤더(Content-Type만 남긴다), S3의 호스트·경로 방식(`버킷/키`만 남긴다).
- Apple 요청 본문은 폼 필드로 풀어 이름순으로 비교하고, `client_secret`(ES256 JWT)은 헤더·클레임으로 풀어 비교한다. **서명 검증은 하지 않는다**(허용 목록에 `p256`이 없다).
- presigned URL은 `X-Amz-Date`를 시나리오 시계 기준 60초 칸의 상대값으로, `X-Amz-Signature`를 길이로 바꾼다(서명 시각이 초 단위로 흔들려서). 나머지는 그대로 비교한다.
- 외부 연동을 선언하지 않았고 받은 요청도 없는 시나리오는 결과 형식이 이전과 같아 기존 golden이 그대로 유효하다.

## 원본 정보 수집

```bash
# SQL 목록: Java가 시나리오를 처리하는 동안 Hibernate가 보낸 문장(정규화됨, 호출 수·행 수 포함)
harness/scripts/collect-sql.sh run            # java-b + pg-java-b에서 초기화 → 녹화 → harness/inventory/sql-statements.csv

# OpenAPI: springdoc /v3/api-docs (local 프로필에서만 켜짐). 전체와 그룹별(user-api, admin-api, internal-api)
harness/scripts/collect-openapi.sh            # 기본 java-a(18080). 인자로 기준 URL
```

`pg_stat_statements`는 DB 컨테이너를 **새로 만들 때** 적재된다(`compose.yaml`의 postgres `command`). 이미 떠 있던
컨테이너는 `up -d --force-recreate <pg 서비스>` 후 앱을 다시 띄워야 한다. 하네스 자신의 문장(대문자 SQL)은
Hibernate(소문자)와 구분해 걸러낸다.

## 결정성을 만드는 장치

- **시계:** 앱과 DB 컨테이너를 libfaketime으로 띄우고, 러너가 단계마다 `harness/out/clock-<대상>` 파일에
  오프셋을 써서 각 단계가 시나리오 시각에서 출발하게 한다. 원본이 `Clock`을 쓰지 않고 `Instant.now()`를 부르는
  곳(주제 단계 판단, 콜백 인증 등)까지 같은 시각을 본다.
- **DB:** 시나리오마다 `flyway_schema_history`를 뺀 모든 테이블을 비우고 `setup.sql`을 실행한다.
- **정규화:** UUID는 등장 순서 기호(`$uuid1`), 시각은 시나리오 시계 기준 5초 칸의 상대값과 표기 규칙
  (`<t+0s iso Z>`), JWT는 클레임을 풀어서 비교한다. 그 밖의 본문·헤더·한글 메시지는 바이트 그대로 비교한다.
  DB 덤프는 행 순서를 정렬하고, 응답에서 보지 못한 UUID는 `$uuid?`로 둔다.

## 지금까지 녹화로 확인한 원본 동작

Rust 구현이 그대로 따라야 하는 것들이다. 시나리오로 고정돼 있다.

- 모든 응답에 Spring Security 기본 헤더가 붙는다: `Cache-Control: no-cache, no-store, max-age=0, must-revalidate`,
  `Expires: 0`, `Pragma: no-cache`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `X-XSS-Protection: 0`,
  그리고 요청마다 새 `X-Request-Id`.
- 헬스체크 본문은 `{"groups":["liveness","readiness"],"status":"UP"}`.
- JSON 문자열 필드에 숫자를 보내면 거절하지 않고 문자열로 바꿔 저장한다(`{"content": 123}` → `"123"`, 201).
- 알 수 없는 JSON 필드는 무시한다. `text/plain` 본문은 415와 `지원하지 않는 요청 방식이거나 형식입니다.`
- `Authorization: Basic ...`은 401이 아니라 익명으로 처리된다. 형식 오류·서명 위조·만료 Bearer 토큰은 401과 `WWW-Authenticate: Bearer`.
- 공개 GET에 관리자 토큰은 403 `FORBIDDEN`, 회원 전용 경로에 관리자 토큰도 403.
- 쿼리 파라미터 오류 메시지는 `date: 필수 요청 값이 없습니다.`(누락·빈 값), `date: 요청 값의 형식이 올바르지 않습니다.`(형식 오류·없는 날짜).
- 관리자 CORS preflight는 허용 Origin에 200과 `Access-Control-Max-Age: 3600`, 허용되지 않은 Origin에 403. `Vary` 헤더 3개가 따로 나간다.
- 주제 단계(`BEFORE_OPEN`/`OPEN`/`CLOSED`)와 "미래 날짜" 판단은 한국 시각 기준이다.

### 외부 연동 시나리오로 확인한 원본 동작(`scenarios/auth/`)

- **ID Token 검증:** 서명(RS256, 공개키는 JWKS)·발급자·aud는 `JwtDecoder`가 보고, 실패는 모두 401 `유효하지 않은 <제공자> ID Token입니다.`
  aud는 **허용 값 하나만** 든 경우만 통과(허용 값 + 다른 값은 거절). `exp`와 `iat`가 없으면 거절하고 `iat`가 현재보다 60초를 넘는 미래여도 거절한다.
  nonce는 원본 `rawNonce`의 SHA-256 소문자 hex와 같아야 하며 nonce 클레임이 없어도 401 `… nonce가 일치하지 않습니다.`
  `sub`가 없으면 `…에 사용자 식별 정보가 없습니다.`, 255자를 넘으면 `…의 사용자 식별 정보가 너무 깁니다.`(둘 다 401).
- **요청 형식:** `authorizationCode`는 APPLE만 필수이고 다른 제공자가 보내면 400 `Authorization Code는 APPLE 로그인에만 필요합니다.`
  알 수 없는 제공자 값은 필드 검증이 아니라 본문 파싱 오류(`JSON 형식이 올바르지 않거나 요청 본문이 비어 있습니다.`)로 400이다.
- **이메일 채택:** 카카오는 `email`을 그대로, 구글·Apple은 `email_verified`가 참(`true`/`"true"`, 대소문자 무시)일 때만 쓰고 아니면 `users.email`이 NULL이다.
- **Apple 로그인:** 신규 회원만 `authorizationCode`를 `/auth/token`으로 교환한다(폼: `client_id`, `client_secret`, `code`, `grant_type`).
  `client_secret`은 ES256 JWT(`kid`=키 ID, `iss`=팀 ID, `sub`=클라이언트 ID, `aud`=`https://appleid.apple.com`, 발급 시각부터 300초).
  교환한 ID Token은 같은 nonce와 같은 `sub`여야 한다. 만료 전 임시 보관분이 있으면 교환하지 않고 만료만 15분 뒤로 민다.
  교환 오류는 400 `invalid_grant`만 401(`만료되었거나 이미 사용된 Apple authorizationCode입니다.`)이고 그 밖의 400, 5xx, 필수 필드가
  빠진 응답, `token_type`이 bearer가 아닌 응답은 모두 500이다.
- **S3 HeadObject:** 사인 이미지 조회는 staging → original 순이고, 가입 때는 이어서 thumbnail까지 본다. 403은 "없음"으로 취급하고(404와 같은 응답),
  5xx는 AWS SDK가 같은 요청을 4번 시도한 뒤 500이다. 이미지는 `image/png`, 1048576바이트 이하만 통과(경계 포함).
- **업로드 URL(presigned PUT):** 호스트가 `<버킷>.<엔드포인트 호스트>`이고 `X-Amz-Expires=300`, 서명 헤더는 `content-type;host`.
- **가입:** 이미지가 없으면 404, 썸네일 처리 전이면 400 `SIGNATURE_PROCESSING_PENDING`. 가입을 마친 `signupToken`을 다시 보내면 새 토큰과 함께 200으로 응답한다.
  이미 가입한 계정으로 업로드 URL을 요청하면 400 `이미 가입된 소셜 계정입니다.`
- **탈퇴:** Apple 회원은 보관한 Refresh Token을 복호화한 평문으로 `/auth/revoke`를 부른다(폼: `client_id`, `client_secret`, `token`, `token_type_hint=refresh_token`).
  폐기가 실패하면(400) 탈퇴는 500으로 끝나고 회원은 그대로다. 성공하면 204, 이메일은 `withdrawn+<id>@chalkak.invalid`, 같은 Apple 계정의 재로그인은 미가입으로 돌아간다.
