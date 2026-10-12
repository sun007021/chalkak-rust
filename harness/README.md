# 차등 검증 하네스

원본 Java 서버(oracle)와 Rust 서버(candidate)에 같은 시나리오를 보내고, 정규화한 응답·DB 상태를 비교한다.
설계 배경은 [전환 계획 4장](../docs/plan/migration-plan.md#4-검증-하네스-가장-먼저-만든다)에 있다.

## 구성

| 경로 | 내용 |
| --- | --- |
| `parity/` | 시나리오 러너, 정규화, 비교기 (`chalkak-parity`) |
| `scenarios/` | 시나리오(TOML). 형식은 `parity/src/scenario.rs` 상단 주석 참고 |
| `golden/` | Java oracle에서 녹화한 정규화 결과 |
| `env/` | 대상별 설정(`java-a`, `java-b`, `rust`)과 앱 공통 환경변수(`oracle.env`) |
| `scripts/up.sh`, `down.sh` | docker compose로 대상(PostgreSQL + 앱 + 시계 파일) 기동·정리. 서비스 정의는 저장소 루트 `compose.yaml` |
| `inventory/` | 원본에서 뽑은 API 목록(`endpoints.csv`), Java 테스트 이식 추적표(`java-test-inventory.csv`), 추출 스크립트 |
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

| 서비스 | 호스트 포트 | 내용 |
| --- | --- | --- |
| `java-a` / `pg-java-a` | 18080 / 15432 | 원본 Spring 서버(oracle)와 DB |
| `java-b` / `pg-java-b` | 18081 / 15433 | 결정성 확인용 두 번째 Java (`selfcheck` 프로필) |
| `rust` / `pg-rust` | 18082 / 15434 | Rust 서버(`rust` 프로필)와 DB. 개발 중에는 `pg-rust`만 띄우고 `cargo run`으로 붙어도 된다 |

모든 앱·DB 컨테이너는 libfaketime이 들어간 이미지(`docker/*.Dockerfile`)로 뜨고, `harness/out/clock-<대상>` 파일의 오프셋을 현재 시각에 더한다.

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
