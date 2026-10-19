# 차등 검증 하네스

원본 Java 서버(oracle)와 Rust 서버(candidate)에 같은 시나리오를 보내고, 정규화한 응답·DB 상태를 비교한다.
설계 배경은 [전환 계획 4장](../docs/plan/migration-plan.md#4-검증-하네스-가장-먼저-만든다)에 있다.

## 구성

| 경로 | 내용 |
| --- | --- |
| `parity/` | 시나리오 러너, 정규화, 비교기, ID Token 서명 (`chalkak-parity`) |
| `fakes/` | 가짜 외부 서버 (`chalkak-fakes`): OIDC JWKS, Apple 토큰 교환·철회, Slack Webhook, S3 HeadObject, SQS(SendMessage·Receive·Delete·ChangeMessageVisibility), FCM v1 전송·OAuth 토큰 교환, CloudWatch Logs Insights. 받은 요청을 기록한다 |
| `keys/` | 가짜 JWKS와 러너가 ID Token 서명에 쓰는 테스트 RSA 키 쌍 (비밀 아님) |
| `scenarios/` | 시나리오(TOML). 형식은 `parity/src/scenario.rs` 상단 주석 참고 |
| `golden/` | Java oracle에서 녹화한 정규화 결과 |
| `env/` | 대상별 설정(`java-a`, `java-b`, `rust`)과 앱 공통 환경변수(`oracle.env`) |
| `scripts/check.sh` | **기본 진입점.** 빌드 → lane 준비 → 시나리오를 lane에 나눠 병렬 실행(`verify`·`compare`·`record`) |
| `scripts/lane.sh`, `lane/compose.yaml` | lane(서로 섞이지 않는 비교 대상 하나) 띄우기·목록·정리. `TXLOG=1`이면 DB 문장 로그를, `RELAY=1`이면 SQS Relay를, `WORKER=1`이면 Relay와 푸시 Worker를 함께 켠 lane을 띄운다(`lane/compose.txlog.yaml`, `env/oracle-relay.env`, `env/oracle-worker.env`) |
| `scripts/oracle-jar.sh` | Java lane이 쓰는 원본 JAR(`BASELINE` 커밋)을 호스트에서 빌드 |
| `loadgen/`, `perf/`, `scripts/perf.sh` | 성능·동시성·장시간·스트레스(트래픽 급증·연결 폭주·DB 장애) 시험: 부하 발생기(`chalkak-loadgen`)와, libfaketime 없는 Java 대 Rust 비교 스택. 사용법은 [docs/perf/README.md](../docs/perf/README.md) |
| `scripts/up.sh`, `down.sh` | 예전 고정 환경(java-a·java-b·rust 한 벌) 기동·정리. 서비스 정의는 저장소 루트 `compose.yaml` |
| `scripts/collect-sql.sh` | Java oracle이 시나리오를 처리하는 동안 보낸 SQL을 `pg_stat_statements`로 모아 저장 |
| `scripts/collect-openapi.sh` | Java oracle의 `/v3/api-docs`(springdoc) 저장 |
| `compose.fakes-local.yaml` | `compose.yaml`에 겹쳐 fakes를 호스트에서 빌드한 바이너리로 띄우는 설정 |
| `inventory/` | 원본에서 뽑은 것: API 목록(`endpoints.csv`), OpenAPI 명세(`openapi/`), SQL 목록(`sql-statements.csv`), Java 테스트 이식 추적표(`java-test-inventory.csv`), 추출 스크립트 |
| `BASELINE` | oracle로 쓰는 원본 커밋 |

## 실행

필요한 것: Docker(Compose v2), 호스트의 libfaketime(`apt install libfaketime`), Rust 툴체인. Java lane에는 JDK 25로
빌드한 원본 JAR이 더 필요하다(`harness/scripts/oracle-jar.sh`가 만든다).

```bash
# Rust를 golden과 비교 (Java가 필요 없다). 이 작업 트리 전용 Rust lane 2개에 시나리오를 나눠 돌린다
harness/scripts/check.sh
harness/scripts/check.sh --filter post --jobs 3

# Java oracle과 바로 비교 / 시나리오를 바꿨을 때 golden 녹화 (Java lane은 모든 작업 트리가 함께 쓴다)
harness/scripts/check.sh compare --filter post
harness/scripts/check.sh record --filter post

# lane 상태와 정리
harness/scripts/lane.sh ls
harness/scripts/lane.sh down rust          # 이 작업 트리의 Rust lane
harness/scripts/lane.sh down all
```

같은 시나리오 31개(단계 584개) 기준 측정(2026-10-13, 4코어 클라우드 컨테이너):

| | 예전 고정 환경 | lane |
| --- | --- | --- |
| Rust 전체 verify | 약 20분(단계당 약 2초, 추정) | lane 1개 5초, 3개 2초 |
| Java 전체 실행 | 약 22분(추정) | lane 1개 10초, 2개 8초 |
| Java 기동 | 4~6분(다른 작업과 겹칠 때) | 14초 |
| 쉬는 Java 하나의 CPU | 1.1~1.7코어 | 0.04코어 |

### lane

lane 하나는 앱 1개(rust 또는 java) + 전용 PostgreSQL + 전용 가짜 외부 서버 + 전용 시계 파일이다. 아무것도 공유하지
않으므로 여러 lane에 시나리오를 나눠 동시에 돌릴 수 있고, 여러 작업(작업 트리, 서브에이전트)이 서로 기다리지 않는다.

- **Rust lane은 작업 트리 전용이다.** 그 작업 트리의 `target/release/chalkak-server`를 마운트하므로 다른 작업 트리가 쓰지 않는다.
  `check.sh`는 매번 빌드하고 앱 컨테이너만 다시 만든다(약 1초). 기동이 수 초라 필요한 만큼 띄우고 내리면 된다.
- **Java lane은 모두가 함께 쓴다.** 원본 JAR은 기준 커밋이 같으면 누구에게나 같다. 러너는 실행하는 동안 lane 잠금
  (`lock`)을 잡고, 다른 작업이 잡고 있는 lane은 건너뛴다(모두 잡혀 있으면 기다린다). 동시에 녹화할 일이 많으면
  `lane.sh up java N`으로 늘린다. Java 하나가 쉬는 동안 메모리 약 600MB를 쓴다.
- 이미지는 공개 기본 이미지(`postgres:18.4`, `eclipse-temurin:25-jre`, `debian:trixie-slim`)를 그대로 쓰고,
  libfaketime·앱 바이너리·JAR·fakes는 호스트 파일을 마운트한다. 컨테이너 안에서 apt·git·cargo 네트워크가 막힌
  환경(클라우드 세션)에서도 이미지 빌드 없이 뜬다. 같은 이유로 lane은 리눅스 호스트용이다.
- 상태는 `$CHALKAK_LANES_DIR`(기본 `/tmp/chalkak-lanes`)`/<lane>/`에 있다(`clock`, `target.toml`, `lock`). 포트는
  lane 번호 N마다 `20000+N*10`(앱), `+1`(DB), `+2`(가짜 서버). 앱이 가짜 서버를 lane 네트워크의 `fakes`,
  `test-bucket.fakes`로 찾으므로 호스트 `/etc/hosts`나 포트 전달이 필요 없다.

러너를 직접 부를 때는 대상 설정을 쉼표로 여러 개 준다. 대상마다 작업 스레드 하나가 남은 시나리오를 가져간다.

```bash
target/release/chalkak-parity verify --target "$(harness/scripts/lane.sh targets rust)"
target/release/chalkak-parity compare --left "$(harness/scripts/lane.sh targets java)" --right "$(harness/scripts/lane.sh targets rust)"
```

### 트랜잭션 모양 비교(`txshape`)

Java(Spring `@Transactional` + Hibernate + pgjdbc)와 Rust가 같은 요청에 같은 트랜잭션 경계·잠금으로 DB를 쓰는지 본다.
시나리오 단계(요청)마다 앱이 보낸 SQL을 트랜잭션 목록(시작 방식, 끝, 문장 서명)으로 잘라 두 쪽을 비교하고 표를 낸다.

```bash
harness/scripts/check.sh txshape [--filter 태그] [--jobs N] [--out 파일.md] [--note-file 머리말.md]
```

- lane DB의 문장 로그는 `TXLOG=1 harness/scripts/lane.sh up rust|java [개수]`로 켠다(기본 꺼짐, 기존 lane은 그대로).
  `log_statement=all`과 줄 머리 `@@세션|application_name|사용자|xid|vxid|시각|`를 `<lane 디렉터리>/pglog/postgres.log`에 쓰고
  `target.toml`에 `pg_log_file`이 들어간다. `check.sh txshape`는 `TXLOG` lane(Rust `--jobs`개, 기본 1, Java 같은 수)을 따로
  띄워 쓰고 끝나도 내리지 않는다. `lane.sh targets rust|java --txlog`가 그런 lane만 낸다.
- 앱 문장과 하네스 문장의 구분은 `application_name`이다. 러너의 DB 연결(준비 SQL, 덤프, 표식)은 `chalkak-harness`를 보내고,
  pgjdbc는 `PostgreSQL JDBC Driver`, Rust 앱은 보내지 않는다(로그에 `[unknown]`). 앱 이름이 `chalkak-harness`가 아닌 세션이 앱이다.
- 단계 경계는 러너가 요청 앞뒤로 하네스 연결에 보내는 표식 SQL(`SELECT 'txshape:<실행 id>:b<단계>'`)로 나눈다. 같은 DB의 로그 순서가
  실제 실행 순서다. 단계 경계를 넘는 트랜잭션은 시작한 단계에 속한다.
- 문장 서명(`parity/src/sqlsig.rs`): 종류(SELECT/INSERT/UPDATE/DELETE/UPSERT), 대상 테이블, 잠금(`FOR UPDATE`·`FOR NO KEY UPDATE`·
  `FOR SHARE`·`FOR KEY SHARE` + `OF` 대상, `pg_advisory_xact_lock`). 풀 검증, `SET`/`SHOW`, pgjdbc 타입 조회는 뺀다.
- 판정(`parity/src/txshape.rs`, 규칙은 머리 주석과 단위 테스트): 같음, 동등(읽기 묶음·SELECT 수 차이 등 설명이 필요한 것), 다름
  (쓰기 분할, 쓰기·잠금 집합, 잠금 종류·대상·순서, 한쪽만 잠금, 격리 수준, 한쪽만 ROLLBACK). 다름이 있으면 종료 코드 1.
- 결과: `--out`(기본 `harness/out/txshape.md`)의 표와 시나리오별 원문 `harness/out/<시나리오>.txshape.txt`. 2026-10-15 결과는
  [docs/perf/results/2026-10-15/txshape.md](../docs/perf/results/2026-10-15/txshape.md).
- 스케줄러 작업은 lane에서 꺼져 있어(`CHALKAK_SCHEDULING_ENABLED=false`) 표에 들어오지 않는다.
- 로그가 쌓이므로 다 쓰면 `lane.sh down rust`, `lane.sh down java-<N>`으로 내린다(Java lane은 다른 작업과 공유한다).

### 예전 고정 환경

`compose.yaml`의 java-a·java-b·rust 한 벌도 그대로 쓸 수 있다(대상 설정 `harness/env/*.toml`). 대상 하나씩이라
여러 작업이 함께 쓰면 차례를 기다린다. 원본 Java 이미지는 처음 한 번 `BASELINE` 커밋을 받아 컨테이너 안에서 빌드한다.

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
| `POST /` + `X-Amz-Target: AmazonSQS.<작업>` | SQS (JSON 프로토콜) | `SendMessage`는 메시지를 저장하고 정상 응답(본문 MD5·`MessageId`). `ReceiveMessage`·`DeleteMessage`·`ChangeMessageVisibility`는 저장한 메시지에 대해 동작한다. 빈 `ReceiveMessage`는 `WaitTimeSeconds`만큼(최대 1초) 기다렸다 비어서 돌아가고 기록하지 않는다(Worker의 유휴 폴링이 기록을 흐리지 않게). 오류·지연은 규칙 목록(`[[fakes.sqs]]`)으로 지정. `POST /__admin/sqs/messages`(`{"messages":[...]}`)로 Relay를 거치지 않는 본문을 큐에 넣는다 |
| `POST /oauth2/token` | Google OAuth 토큰 교환(FCM 서비스 계정) | 정상이면 `{"access_token":"fake-access-token","expires_in":3600}`. 규칙 `[[fakes.oauth]]`로 응답 지정. 서비스 `oauth_token` |
| `POST /v1/projects/<id>/messages:send` | FCM HTTP v1 | 정상이면 `{"name":"projects/<id>/messages/fake-N"}`. 규칙 `[[fakes.fcm]]`. 서비스 `fcm_send` |
| `POST /` + `X-Amz-Target: Logs_20140328.*` | CloudWatch Logs Insights(`StartQuery`·`GetQueryResults`·`StopQuery`) | 쿼리는 즉시 `Complete`. 쿼리의 `as <지표>` 이름으로 시나리오가 지정한 결과 행을 돌려주고, 지정하지 않은 지표는 행이 없다. 쿼리 ID는 `fake-query-N` |
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
| `AWS_ENDPOINT_URL_SQS` | `http://fakes:9000` | AWS SDK 공통 환경변수. Java SDK(2.54.1)가 존중함을 확인했다(요청이 fakes에 도착한다). 큐 URL은 요청 본문에만 들어간다 |
| `AWS_ENDPOINT_URL_CLOUDWATCH_LOGS` | `http://fakes:9000` | 서비스별 AWS SDK 환경변수. 사용자 지표 집계가 읽는 Logs Insights를 가짜 서버로 돌린다 |
| `CHALKAK_ANALYTICS_LOGGROUP`, `CHALKAK_ANALYTICS_EXCLUDEDUSERIDS` | `/chalkak/test/application`, `01990000-0000-7000-8000-0000000000e1` | `chalkak.analytics.*`. 쿼리 본문의 로그 그룹과 제외 회원 조건이 비교에 나온다 |
| `CHALKAK_IMAGE_ANONYMOUSACCESS` | `false` | `chalkak.image.anonymous-access`. local 프로필의 익명 자격증명으로는 presigned URL을 만들 수 없다("Only SigV4 signers are supported") |

**Slack은 가짜 서버로 돌릴 수 없다.** 원본의 `SlackNotificationProperties`가 `https://hooks.slack.com/services/…`
(포트 없음)만 받는다. 그래서 oracle은 `ADMIN_NOTIFICATION_DELIVERY_ENABLED=false`를 유지하고, fakes의 Slack 경로는
URL 검증이 느슨한 Rust 서버용이다. 원본의 Slack 본문 형식은 Java 단위 테스트 이식으로 확인한다.

### relay lane (SQS Relay)

원본 `oracle.env`는 Spring 스케줄링 전체를 끈다(시계를 옮기는 시나리오에서 정리 작업이 DB를 바꾸면 안 된다). SQS Relay처럼 앱이
스스로 도는 작업을 확인하려면 그 작업만 켠 lane이 따로 필요하다.

- `RELAY=1 harness/scripts/lane.sh up rust|java [개수]`가 `env/oracle-relay.env`를 더한 lane을 띄운다(스케줄링 켬, 정리 작업과
  런타임 로그 끔, `NOTIFICATION_RELAY_ENABLED=true`, 큐 URL은 표준 형식의 가짜 값). `target.toml`에 `flavor = "relay"`가 들어가고
  `lane.sh targets rust|java --relay`가 그런 lane만 낸다. 기본 `targets`에는 섞이지 않는다.
- 시나리오는 최상위에 `lane = "relay"`를 쓴다. `parity`는 `--lane`(기본 `default`)과 같은 시나리오만 고르고, 시나리오 `lane`과 대상
  `flavor`가 다르면 하네스 오류다. `check.sh verify|compare|record`는 기본 lane을 돌린 뒤 relay 시나리오가 있으면 relay lane
  하나(없으면 띄운다)에서 이어서 돌린다. `parity count --lane relay`가 개수를 센다.
- 단계의 `wait_until_sql = "SELECT ..."`는 요청 전에 쿼리의 첫 열(bool)이 참이 될 때까지(최대 30초) 지켜본다. 앱이 스스로 도는
  작업의 결과를 기다릴 때 쓴다.
- `[[fakes.sqs]]` 규칙(`op`, `contains`, `status`, `content_type`, `body`, `headers`, `delay_ms`)으로 SQS 응답을 지정한다. `contains`는 요청
  본문에 든 문자열(알림 ID)이고, 규칙이 없으면 정상 응답이다. `delay_ms`로 5초 제한 시나리오를 만든다.
- 비교에서 SQS 요청은 작업 이름·큐 URL·`MessageBody`(JSON으로 풀어 정규화)만 남기고 서명·SDK 헤더는 버린다. JSON 키 순서(Jackson
  레코드 선언 순서)는 정규화로 정렬되어 보이지 않으므로 cargo 시험(`sqs_push_tests`)이 바이트 단위로 확인한다.
- **주제 푸시(cron)와 발행 대기 모니터는 relay lane에서 켜지 않는다.** Java cron은 기동 때 계산한 지연을 단조 시계로 기다려
  faketime을 따르지 않는다. 이 둘은 시계를 고정한 DB 시험(`topic_push_tests`, `publication_monitor_tests`)으로 확인한다.

### worker lane (푸시 Worker)

`WORKER=1 harness/scripts/lane.sh up rust|java [개수]`는 relay lane에 `env/oracle-worker.env`를 더해 푸시 Worker를 켠다
(`NOTIFICATION_WORKER_ENABLED=true`, 프로젝트 `chalkak-harness`, 서비스 계정 `keys/fcm-service-account.json`을 `/secrets/`에 마운트,
`token_uri`는 `http://fakes:9000/oauth2/token`). `flavor = "worker"`, 시나리오는 `lane = "worker"`, 목록은 `lane.sh targets ... --worker`.
단계의 `sqs_seed = ['''본문''']`은 요청 전에 가짜 큐에 메시지를 넣고, `await_fakes = { service = "sqs", count = N }`은 Worker가 받아 지울 때까지 기다린다.

- **Java로는 FCM 전송을 녹화할 수 없다.** Firebase Admin SDK가 FCM 호스트를 `https://fcm.googleapis.com`으로 고정해 바꿀 수 없다.
  (OAuth 주소만 서비스 계정 JSON의 `token_uri`로 돌릴 수 있다.) 그래서 worker lane 시나리오는 보내지 않는 경로(잘못된 본문, 만료,
  기기 없음, 읽음, 탈퇴 등)와 큐 수신·삭제 흐름만 담고, 전송·재시도·오류 분류는 Rust 단위 시험과 DB 시험
  (`crates/app/tests/push_worker_db.rs`, `fcm/tests.rs`, 원본 JAR로 기록한 표 `fcm_java_parity.rs`)이 맡는다.
- Rust에는 원본에 없는 `NOTIFICATION_FCM_ENDPOINT_URL`이 있어 같은 가짜 FCM 서버(`fcm_send`)로 전송 경로를 직접 확인할 수 있다
  (env는 `oracle-worker.env`가 Rust lane에도 주지만 Java는 무시한다).
- 비교에서 `fcm_send`는 본문을 JSON으로 풀어 정규화하고 `oauth_token`은 폼 본문을 푼다.

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

[fakes.apple_token]                           # 시작 시 응답 지정. apple_revoke, slack, s3."<키>", [[fakes.sqs]]도 같은 방식
status = 200
body = '''{"access_token":"a","token_type":"Bearer","expires_in":3600,"refresh_token":"r","id_token":"${apple}"}'''

[[step]]
name = "..."
# ...
[step.fakes.s3."chalkak/signatures/local/original/${uploadId}.png"]   # 이 단계 전에 덧씌움(이후에도 유지)
size = 2048
content_type = "image/png"
```

- 이미지 처리 콜백(`/internal/v1/**`) 단계는 `callback = {}`로 HMAC 헤더(`X-Chalkak-Callback-Timestamp`, `-Signature`)를 붙인다. 서명 대상은 `타임스탬프\nPOST\n경로\nsha256hex(빈 본문)\nlocal`이고 비밀키는 `oracle.env`의 `IMAGE_PROCESSOR_CALLBACK_SECRET`이다. `timestamp_offset`, `sign_path`, `secret`, `audience`, `signature`, `omit_timestamp`, `omit_signature`로 잘못된 요청을 만든다.
- 응답을 보낸 뒤 비동기로 외부 호출을 이어 가는 단계(사용자 지표 집계)는 `await_fakes = { service = "cloudwatch_logs", count = N }`로 가짜 서버 기록이 누적 N개가 될 때까지 기다린다. 앱마다 걸리는 시간이 달라(JVM은 첫 호출이 느리다) 다음 단계의 "진행 중" 거절과 겹치지 않게 맞춘다. `[fakes.logs]`는 `status`(기본 `Complete`)와 `rows.<지표>.<필드> = "값"`을 지정한다.
- 응답 지정과 s3 키의 `${변수}`는 단계마다 현재 변수 값으로 채운다. 그래서 앞 단계에서 `capture`한 `uploadId`로 키를 지정할 수 있다.
- 비교에서 **빼는 것**: JWKS 조회(앱의 공개키 캐시 상태로 횟수가 달라진다), 요청 헤더(Content-Type만 남긴다), S3의 호스트·경로 방식(`버킷/키`만 남긴다).
- Apple 요청 본문은 폼 필드로 풀어 이름순으로 비교하고, `client_secret`(ES256 JWT)은 헤더·클레임으로 풀어 비교한다. **서명 검증은 하지 않는다**(허용 목록에 `p256`이 없다).
- presigned URL은 `X-Amz-Date`를 시나리오 시계 기준 60초 칸의 상대값으로, `X-Amz-Signature`를 길이로 바꾼다(서명 시각이 초 단위로 흔들려서). 나머지는 그대로 비교한다.
- 외부 연동을 선언하지 않았고 받은 요청도 없는 시나리오는 결과 형식이 이전과 같아 기존 golden이 그대로 유효하다.
- **내부 콜백 서명:** `/internal/v1/post-image-processing/...` 단계는 `callback = {}`를 적으면 러너가 경로(쿼리 제외)와 본문으로
  `X-Chalkak-Callback-Timestamp`·`X-Chalkak-Callback-Signature`(`v1=<HMAC-SHA256 hex>`)를 만든다. 시각은 앱 시계(시나리오 시계 + 앱 오프셋)이고
  비밀키 `test-callback-secret-with-enough-length`, 대상 `local`이 기본이다. 틀린 서명·시각·헤더 누락은 같은 표에서 바꾼다:
  `timestamp_offset`(초), `secret`, `audience`, `sign_body`(서명에 쓸 다른 본문), `signature`(서명을 통째로 지정), `omit = ["timestamp"|"signature"]`.

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

- **시계:** 앱과 DB 컨테이너를 libfaketime으로 띄우고, 러너가 대상의 시계 파일에 오프셋(소수 초)을 써서 단계마다
  앱 시각이 `시나리오 시각 + 1.1초` 근처에서 출발하게 한다. 원본이 `Clock`을 쓰지 않고 `Instant.now()`를 부르는
  곳(주제 단계 판단, 콜백 인증 등)까지 같은 시각을 본다. 시계 파일은 목표 시각이 바뀔 때(시나리오 시작,
  `advance_seconds`)나 마지막으로 맞춘 뒤 1초가 지났을 때만 다시 쓴다(`runner.rs`의 `ClockSync`). 예전처럼 단계마다
  쓰려면 `PARITY_CLOCK=strict`. 시계를 쓴 뒤 기다리는 시간은 대상 설정 `clock_settle_ms`(lane: Java 200, Rust 50,
  예전 환경 기본 1100).
- **libfaketime 설정:** `FAKETIME_FORCE_MONOTONIC_FIX=0`이 꼭 필요하다. 기본값(보정 켜짐)에서는 `CLOCK_MONOTONIC`
  시간 제한 대기가 바로 끝나서 JVM 내부 스레드(Cleaner, G1 Refine, Hikari 등)가 쉬지 않고 돌며 시계 파일을 읽는다.
  쉬는 Java 하나가 1~2코어를 쓰고, 기동이 수 분 걸리고, 그 부하로 시계 미반영 오류가 났다(`Object.wait(6초)`가 0.8초에 끝남).
- **DB:** 시나리오마다 `flyway_schema_history`를 뺀 모든 테이블을 비우고 `setup.sql`을 실행한다.
- **일시적 하네스 오류는 시나리오를 다시 실행:** 앱 `Date`가 시나리오 시각과 다르거나(libfaketime이 시계 파일을 놓침) 응답 없이
  연결이 닫히면 시나리오를 DB 초기화부터 최대 3번 다시 돌리고 경고를 낸다. 세 번 모두 같으면 그 시나리오를 오류로 남기므로 구현이 실제로 응답 없이 닫는 경우는 가려지지 않는다.
  하네스 오류가 하나라도 있으면 종료 코드는 2다. 끝나면 시계 파일을 `+0`(실제 시각)으로 되돌린다.
- **정규화:** UUID는 등장 순서 기호(`$uuid1`), 시각은 시나리오 시계 기준 5초 칸의 상대값과 표기 규칙
  (`<t+0s iso Z>`), JWT는 클레임을 풀어서 비교한다. 그 밖의 본문·헤더·한글 메시지는 바이트 그대로 비교한다.
  DB 덤프는 행 순서를 정렬하고, 응답에서 보지 못한 UUID는 `$uuid?`로 둔다.
  구현이 정하지 않는 순서는 결과와 golden 모두에서 한 형식으로 맞춘다: DB 행 JSON의 키 순서(serde_json
  `preserve_order`가 빌드 묶음에 따라 켜지고 꺼진다), `Allow` 헤더의 메서드 순서(Spring이 JVM 실행마다 다르게 낸다).
  golden 파일도 키를 정렬해 쓴다.

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

### 관리자 API 시나리오로 확인한 원본 동작(`scenarios/admin/`, 태그 `admin-api`)

- **쿼리 바인딩:** 공백만 있는 값은 없는 값이다. 정수는 Spring 규칙(앞뒤 공백 허용 안 함, `+`·0 패딩 허용, 16진수 거절), UUID는 대소문자·앞뒤 공백을 느슨하게 받는다.
  바인딩 오류는 서비스 검사(페이지 범위·기간 순서)보다 먼저 나고, 파라미터가 여럿 틀리면 선언 순서의 첫 오류가 나간다.
- **본문 바인딩(Jackson):** enum은 이름(대소문자 구분)이나 서수로 읽고, 숫자·불리언을 문자열 필드에 넣으면 문자열로 바꾼다. 시각은 ISO(오프셋 필수) 또는 epoch 초.
- **검증 메시지:** 기본 영어 문구(`must not be blank`, `size must be between 0 and 500`)이고 길이는 UTF-16 단위다. 위반이 여러 개인 요청은 값에 따라
  메시지 순서가 달라지므로(같은 값도 실행마다 달랐다) 시나리오에 넣지 않았다. 한국어 `Accept-Language` 메시지는 Rust에서 아직 이식하지 않았다.
- **회원 목록:** `status`를 생략하면 `ACTIVE`. 이메일은 trim·소문자로 정규화한 뒤 320자를 넘으면 400(`이메일 검색어가 올바르지 않습니다.`)이지만 페이지 검사가 먼저다.
- **게시물:** 목록·상세는 삭제된 게시물을 포함하고 `VALIDATING`을 뺀다. 검수는 `PENDING`만 처리하고 아니면 `RESOURCE_STATE_CHANGED`. 삭제는 이미 삭제된 게시물에 204로 멱등.
- **리프레시 토큰:** 회전한 토큰을 다시 내면 계보 전체를 폐기하고 401 `REAUTHENTICATION_REQUIRED`. 폐기는 401 응답 전에 커밋된다.
- **감사 로그:** 같은 시각이면 id(UUIDv7)로 정렬하므로, 시나리오는 변경 단계마다 시계를 올려 순서를 고정했다.
- **토큰 재발급(`auth/token-refresh`):** 비활동 30일·절대 90일. 재발급마다 토큰이 회전하고 만료는 절대 만료를 넘지 않는다. 이미 회전된 토큰을 다시 내면 그 계보 전체를 폐기한다(다른 기기 계보는 유지).
  거절은 모두 401 `REAUTHENTICATION_REQUIRED`(`다시 로그인해 주세요.`). 로그아웃은 멱등이라 모르는 토큰도 204.
  절대 만료로 줄어든 `refreshTokenExpiresIn`은 요청이 시각을 읽는 순간에 따라 1초 흔들려 비교에서 뺀다(`opaque`).
- **요청 본문이 흔들리는 곳:** 위반이 둘 이상인 요청은 원본이 어느 메시지를 고를지 정해져 있지 않다(Bean Validation 위반 집합 순서). 시나리오는 한 번에 하나만 어기게 한다.
  `provider`가 빈 문자열이면 null이 아니라 본문 파싱 오류(`JSON 형식이 올바르지 않거나 요청 본문이 비어 있습니다.`)다.
- **사인(`user/signature`, `user/signature-callbacks`):** 처리 대기 중 사인 교체는 같은 업로드 ID를 되풀이해도 현재 사인 URL을 돌려주고, 처리 시간(7분)이 지나거나 실패하면
  400 `SIGNATURE_REGISTRATION_REQUIRED`/`SIGNATURE_REUPLOAD_REQUIRED`. 콜백은 `X-Chalkak-Callback-Timestamp`·`X-Chalkak-Callback-Signature`(`v1=<hex>`)로만 인증하고
  서명 대상은 `타임스탬프\nPOST\n경로\nsha256(빈 본문)\n환경`, 시각 창은 과거 300초·미래 30초다. 경로의 업로드 ID는 소문자 표준형이어야 하며 아니면 인증보다 먼저 400이다.
- **콜백 본문 제한(`user/callback-body-limit`):** `/internal/v1/*`에서 `Content-Length`가 65536바이트를 넘으면 인증 전에 413이다. 청크 본문은 길이를 모르므로 거르지 않는다.
- **탈퇴(`user/withdraw`):** 정지 회원도 탈퇴할 수 있고, 이미 탈퇴한 회원은 401이다. 모든 리프레시 토큰이 폐기되고 소셜 계정이 지워진다(정지 회원은 계정 유지).
