# Chalkak 백엔드 Rust 전환 계획

- 원본: `woowacourse-teams/2026-Chalkak` `backend/` (기준 커밋 `0abc3f9`, 2026-10-12)
- 대상: `sun007021/chalkak-rust` (현재 빈 저장소)
- 목표: 성능 저하·기능 누락·버그 없이 동작이 같은 Rust 서버로 교체. 프레임워크 없이, `unsafe` 없이.
- 핵심 전략: **Java 서버를 정답(oracle)으로 삼는 차등 검증(differential testing)**. 같은 시나리오를 Java와 Rust에 똑같이 보내 응답·DB 상태·외부 호출·로그를 비교한다. Rust 코드를 쓰기 전에 이 하네스부터 만든다.

---

## 1. 원본 현황 요약

| 항목 | 값 | 근거 |
| --- | --- | --- |
| 스택 | Java 25, Spring Boot 4.1, Spring MVC(Tomcat), JPA/Hibernate, Flyway, Spring Security(OAuth2 Resource Server), AWS SDK v2(S3), Jackson 3 | `backend/build.gradle.kts` |
| 운영 코드 | Java 456개 파일, 약 20,600줄 | `src/main` |
| 테스트 코드 | 192개 파일, 약 36,100줄, 테스트 메서드 1,200개 | `src/test` |
| HTTP API | 51개 (+ `/actuator/health`, `/actuator/info`, dev 전용 Swagger) | [endpoints.csv](endpoints.csv) |
| 도메인 | auth, user(사인), topic, post(이미지 업로드·콜백), like, feedback, notification(알림함·Slack), admin, photo | 패키지 구조 |
| DB | PostgreSQL 18.4, Flyway 마이그레이션 28개, JPQL `@Query` 60개, 비관적 락 9곳, advisory lock 1곳 | `db/migration`, `*JpaRepository` |
| 주기 작업 | 리프레시 토큰 정리(04:30), 가입 토큰 정리(04:40), 알림 정리(04:50), Apple 임시 인증 정리(매시), 런타임 스냅샷 로그(1분) — 모두 Asia/Seoul | `@Scheduled` 5곳 |
| 외부 연동 | Google·Kakao·Apple OIDC(JWKS 조회, RS256 검증), Apple 토큰 교환·철회(ES256 client secret), Slack Incoming Webhook, S3 presigned PUT·HeadObject(EC2 인스턴스 역할 자격증명), Lambda 이미지 처리 콜백(HMAC-SHA256) | `auth/infrastructure`, `post|user/infrastructure` |
| 암호 | HS256 JWT(액세스·가입 토큰), HMAC-SHA256(소셜 식별자 지문·콜백 서명), AES-256-GCM(Apple 리프레시 토큰), BCrypt(관리자 비밀번호) | `*Cipher`, `*Encoder`, `SecurityConfig` |
| 배포 | CodeBuild → CodeDeploy → EC2 systemd(`java -jar`), 헬스체크 `GET :8080/actuator/health`, **로드밸런서 없이 공개 EC2 직접 노출** | `backend/deploy/` |
| 로그 | dev·prod는 logstash 형식 JSON 파일 → CloudWatch Agent, 대시보드가 필드명에 의존 | `logback-spring.xml`, `deploy/monitoring` |

테스트 1,200개의 구성: Spring+DB 통합 476, 단위 438, 웹 슬라이스(MockMvc) 209, JPA 슬라이스 74, 아키텍처 3. 대부분 Spring에 묶여 있어 그대로 옮길 수 없으므로, **테스트 케이스(검증 의도)를 이식하고 Spring 결합부는 블랙박스 시나리오로 대체**한다([java-test-inventory.csv](java-test-inventory.csv)).

---

## 2. 결정: "프레임워크 없이 vanilla Rust"의 범위 (2026-10-12 B안 확정)

원본이 실제로 필요로 하는 것은 TLS 1.2/1.3 클라이언트(Apple·Google·Kakao·Slack·S3·RDS), PostgreSQL 와이어 프로토콜과 SCRAM 인증, ECDSA P-256 서명·RSA 검증·AES-GCM·HMAC·BCrypt, JSON입니다. 표준 라이브러리에는 이 중 아무것도 없습니다.

| 안 | 내용 | 평가 |
| --- | --- | --- |
| A. 표준 라이브러리만 | TLS, 암호, PG 프로토콜, JSON까지 전부 직접 구현 | TLS·암호를 직접 짜는 것은 "버그 없이"와 정면 충돌하고 보안 위험이 큼. 상수 시간 연산을 `unsafe` 없이 보장하기도 어려움. **비추천** |
| **B. 프레임워크·ORM 금지, 검증된 기초 라이브러리만 허용 (추천)** | HTTP 서버·라우터·미들웨어·검증·오류 매핑·DI·스케줄러·설정·마이그레이터·SigV4는 직접 구현. TLS·암호·DB 드라이버·JSON·UUID·시간대만 허용 목록의 crate 사용 | 프레임워크 없는 구조라는 목표를 지키면서 위험한 저수준 구현을 피함 |
| C. B + 저수준 HTTP/비동기 런타임 허용 | B에 `hyper`, `tokio` 추가 | 프레임워크는 아니지만 "직접 만든 서버"라는 취지에서 멀어짐 |

**판별 기준(B):** 라이브러리가 우리 코드를 호출하는 구조(제어의 역전: 라우팅, 핸들러 등록, 엔티티 매핑)면 프레임워크로 보고 금지한다. 우리가 함수로 호출하는 도구면 허용 후보다.

B안 허용 목록 초안 (모두 `cargo-deny`로 고정·감사):

| 용도 | crate | 비고 |
| --- | --- | --- |
| TLS | `rustls`, `webpki-roots` | 외부 HTTPS, RDS TLS |
| 암호 | `ring` 또는 RustCrypto(`hmac`, `sha2`, `aes-gcm`, `p256`, `rsa`) + `bcrypt` | 하나의 계열로 통일 |
| DB 드라이버 | `postgres`(동기) | 내부적으로 tokio를 숨겨 쓰는 점은 수용 여부 확인. 대안: PG v3 프로토콜 직접 구현 |
| JSON | `serde`, `serde_json` | derive 매크로 포함 |
| 식별자·시간 | `uuid`(v7), `jiff` 또는 `time` + tzdb | Asia/Seoul 계산 |
| 종료 신호 | `signal-hook` | 표준 라이브러리로는 SIGTERM을 받을 수 없음. graceful shutdown(`TimeoutStopSec=30`)에 필요 |
| HTTP 파싱(선택) | `httparse` | 직접 구현 시 퍼징 필수. 공개 EC2 직접 노출이라 파서 견고성이 보안 문제임 |

허용하지 않는 것: axum·actix·rocket 등 웹 프레임워크, diesel·sea-orm·sqlx 매크로 ORM, AWS SDK(무거운 런타임을 끌고 옴 → SigV4·IMDSv2는 직접 구현하고 AWS 공식 테스트 벡터로 검증).

`unsafe` 정책: 우리 crate 전부 `#![forbid(unsafe_code)]`. 의존성 내부 `unsafe`는 `cargo-geiger` 보고서로 추적하고 허용 목록 심사 때 함께 본다.

> **2026-10-12 sunwook이 B안(기초 crate만 허용)으로 확정했다.** 허용 목록에 crate를 더하려면 이 표를 먼저 갱신한다.

---

## 3. 목표 아키텍처

### 3.1 런타임 모델

원본은 Tomcat 스레드 풀(요청당 스레드) + JDBC(블로킹) + HikariCP다. Rust도 같은 모델을 택한다.

- `std::net::TcpListener` + 고정 크기 워커 스레드 풀 + 블로킹 DB 커넥션 풀(직접 구현, Hikari와 같은 크기 설정).
- 같은 동시성 모델이라 락 순서·트랜잭션 경계·풀 고갈 동작이 원본과 같아져 정합성 검증이 쉽다. 비동기 런타임 의존도 없다.
- 공개 EC2 직접 노출이므로 연결 수 상한, 헤더 크기·개수 상한, 요청 읽기 타임아웃, keep-alive 유휴 타임아웃, 본문 상한을 첫 단계부터 넣는다. 유휴 keep-alive 연결이 워커를 붙잡는 문제가 부하 시험에서 보이면 accept 스레드에서 유휴 연결을 관리하는 구조로 바꾼다(성능 게이트로 판단).

### 3.2 저장소 구조

```
chalkak-rust/
  Cargo.toml                 # workspace, lints: forbid(unsafe_code)
  crates/
    platform/                # 직접 구현하는 "프레임워크 대체물"
      http/                  # HTTP/1.1 서버, 라우터, 요청·응답, 미들웨어 체인, CORS, 보안 헤더
      db/                    # 커넥션 풀, 트랜잭션, 행 매핑, Flyway 호환 마이그레이터
      core/                  # 설정 로더, Clock, 오류 모델, JSON 로그, requestId, 스케줄러
      aws/                   # SigV4, presign, HeadObject, IMDSv2 자격증명
      jwt/                   # HS256 발급·검증, RS256 검증, ES256 서명, JWKS 캐시
    app/                     # 도메인: auth, user, topic, post, like, feedback, notification, admin, photo
      (패키지 구조는 Java와 1:1로 맞춘다: api / service / domain / repository / infrastructure)
    server/                  # main: 설정 → 의존성 조립(수동 DI) → 서버·스케줄러 기동
  migrations/                # 원본 Flyway SQL을 바이트 그대로 복사 (CI가 원본과 동일성 검사)
  harness/                   # 검증 하네스 (4장)
    parity/                  # 시나리오 러너 + 정규화 + 비교기
    fakes/                   # 가짜 OIDC·Apple·Slack·S3·IMDS 서버
    scenarios/               # 시나리오 정의
    golden/                  # Java에서 녹화한 기대값
    loadgen/                 # 부하 발생기
    inventory/               # 원본 라우트·테스트·SQL 목록 추출기
  compose.yaml               # postgres 18.4, fakes, java-oracle, rust-candidate
```

### 3.3 Spring 기능 → Rust 대응표

| Spring/JVM 기능 | Rust 구현 | 정합성 확인 방법 |
| --- | --- | --- |
| Spring MVC 라우팅·바인딩 | `platform/http` 라우터, 경로 변수·쿼리 파서 | 라우트 목록 자동 비교, 404/405 시나리오 |
| Jackson 3 직렬화 | serde + 형식 고정 헬퍼(Instant·LocalDate 포맷, null 처리, 알 수 없는 필드 무시) | 응답 바이트 비교 |
| Bean Validation + 오류 메시지 | 요청 DTO별 검증 함수, 메시지 표 | 무효 요청 생성기로 차등 비교 |
| `GlobalExceptionHandler`, `DomainErrorHttpMapper` | 오류 enum → 상태코드·`{code,message}` 매핑 | 오류 시나리오 전수 |
| Spring Security 필터 체인 | 인증 미들웨어(규칙 순서 그대로), 401/403 응답기 | 경로×토큰 종류 매트릭스 |
| JPA 엔티티·JPQL·Spring Data 페이징 | 손으로 쓴 SQL + 행 매핑 | Java 실행 중 수집한 SQL 목록 대조, 쿼리 수 비교, DB 상태 비교 |
| `@Transactional`, 비관적 락, advisory lock | 명시적 트랜잭션, `SELECT ... FOR UPDATE/FOR SHARE`, `pg_advisory_xact_lock` | 동시성 시나리오 |
| `@TransactionalEventListener(AFTER_COMMIT)` | 커밋 후 실행 훅 큐 | Slack 호출 시점·롤백 시 미발송 시나리오 |
| `@Scheduled` (Asia/Seoul cron) | `platform/core` 스케줄러 | 가짜 시계로 시각 이동 후 DB 상태 비교 |
| Flyway | 호환 마이그레이터(같은 `flyway_schema_history`, 같은 체크섬 알고리즘) | 교차 적용 시험 |
| Actuator health/info | 같은 경로·같은 본문(`{"status":"UP"}` 등), DB 장애 시 동작 포함 | 헬스 시나리오 |
| springdoc(dev 전용) | Java에서 뽑은 `openapi.json`을 정적 제공 | 파일 비교 |
| logback JSON(logstash) | 같은 필드명의 JSON 로그 | 로그 필드 비교 |
| `RuntimeSnapshotLogger`(JVM 지표) | 프로세스 지표(RSS, 스레드, DB 풀) | JVM 전용 필드는 대응 불가 → 대시보드 수정 항목 |
| AWS SDK 기본 자격증명 체인 | 환경변수 → IMDSv2 순서로 직접 구현, 만료 전 갱신 | 가짜 IMDS 시험 |

---

## 4. 검증 하네스 (가장 먼저 만든다)

### 4.1 구조

```
           ┌──────────── 시나리오 러너 (harness/parity) ────────────┐
           │  같은 요청 시퀀스                                       │
           ▼                                                         ▼
   Java oracle (기준 커밋 빌드)                          Rust candidate
      │   │                                                 │   │
      │   └──► fakes: OIDC JWKS / Apple / Slack / S3 / IMDS ◄┘   │
      ▼                                                         ▼
  PostgreSQL A (같은 스냅샷에서 시작)                  PostgreSQL B
           │                                                         │
           └──────► 비교기: 응답 · DB 상태 · 외부 호출 기록 · 로그 ◄──┘
```

- **양쪽을 같은 초기 상태에서 같은 시각으로** 돌린다. 각 시나리오 전에 두 DB를 같은 스냅샷(템플릿 DB 복제)으로 되돌린다.
- **시계 제어:** 원본은 `Clock`을 15곳에서 주입받지만 콜백 인증은 `Instant.now()`를 직접 쓴다. 그래서 프로세스 시각 자체를 고정해야 한다. 1안은 두 프로세스 모두 `libfaketime`(`FAKETIME_TIMESTAMP_FILE` + `FAKETIME_NO_CACHE=1`)으로 띄우고 러너가 파일을 바꿔 시각을 옮기는 방식이다. JVM에서 동작하지 않는 부분이 나오면 2안으로 원본을 수정하지 않는 테스트 전용 에이전트를 붙인다. Rust 쪽은 내부적으로도 `Clock` 트레이트로 주입한다.
- **외부 연동 대체(fakes):** 한 프로세스가 다음을 흉내 내고 받은 요청을 모두 기록한다.
  - OIDC: Apple·Google·Kakao 각각의 JWKS. 러너가 테스트 키로 ID 토큰을 서명해 시나리오에 넣는다(issuer는 실제 값 유지, `jwk-set-uri`만 가짜로).
  - Apple 토큰 교환·철회(`token-uri`, `revoke-uri`): 응답과 오류를 시나리오별로 지정. 받은 client secret(ES256 JWT)의 서명·클레임도 검증.
  - Slack Webhook: 본문 기록.
  - S3: HeadObject 응답(크기·Content-Type·없음) 지정. Java는 `AWS_ENDPOINT_URL_S3` 환경변수로 연결, 정적 테스트 자격증명 사용.
  - IMDSv2: Rust 자격증명 갱신 시험용.
- **Java oracle 실행:** 기준 커밋을 빌드한 JAR을 `local`과 같은 설정 + 가짜 엔드포인트로 띄운다. 원본 코드는 수정하지 않는다.

### 4.2 시나리오와 기대값

- 시나리오는 요청 시퀀스다. 앞 응답 값을 변수로 받아 다음 요청에 쓴다(예: 로그인 → 토큰 → 게시물 생성 → 업로드 콜백 → 목록 조회).
- **녹화 모드:** Java에 돌려 정규화된 응답·DB 변경분·외부 호출을 `golden/`에 저장한다. **검증 모드:** Rust에 돌려 golden과 비교한다. **동시 모드:** 둘 다 실시간으로 돌려 바로 비교한다(golden이 낡는 문제 방지, CI 기본값).
- 정규화 규칙(최소한으로, 규칙마다 이유를 남긴다):
  - 서버가 만드는 UUIDv7 → 첫 등장 순서대로 `$id1`, `$id2`로 바꾸고 **같은 값은 같은 변수로** 묶여야 통과. 형식(v7 여부)은 따로 검사.
  - JWT → 헤더·클레임을 풀어서 비교하고, 무작위 값(`jti` 등)만 변수화. 서명은 각자 키로 검증.
  - presigned URL → 고정 시각·고정 자격증명이면 SigV4는 결정적이므로 **문자열 그대로** 같아야 한다.
  - 리프레시 토큰 등 무작위 불투명 값 → 길이·문자 집합·DB에 저장된 해시와의 관계만 비교.
  - 그 외 본문, 오류 메시지(한글), 상태코드, 헤더는 **정규화하지 않고 바이트 단위로** 비교한다.
- 비교 대상:
  1. 상태코드, 헤더(Content-Type 문자셋 포함, CORS, `Vary`, Spring Security 기본 보안 헤더, `Cache-Control`), 본문.
  2. DB 상태: 시나리오 전후 모든 테이블을 정렬된 덤프로 떠서 변경분 비교(soft delete 플래그, 타임스탬프 정밀도 포함).
  3. 외부 호출 기록: 순서, 횟수, 본문, 커밋 후 발송 여부.
  4. 로그: 요청 하나당 남는 JSON 로그의 키 집합과 `type`·`errorCode`·`status`·`requestId` 값.

### 4.3 시나리오를 채우는 다섯 가지 출처

| 출처 | 내용 | 기대 규모 |
| --- | --- | --- |
| ① Java 테스트 이식 | 1,200개 테스트의 의도를 옮긴다. 웹·통합 테스트(685개)는 블랙박스 시나리오로, 도메인·단위 테스트(438개)는 Rust 단위 테스트로, JPA 슬라이스(74개)는 리포지토리 통합 테스트로. Spring 내부 구현만 검사하는 것은 근거를 남기고 제외 | [java-test-inventory.csv](java-test-inventory.csv)의 모든 행이 `rust_target`과 `status`를 갖는다 |
| ② 비즈니스 규칙 문서 | `docs/business-rules/rules/*.md` 8개 도메인의 규칙 ID마다 최소 1개 시나리오 | 규칙 ID → 시나리오 추적표 |
| ③ 엔드포인트 매트릭스 | 51개 API × (토큰 없음 / 회원 / 관리자 / 만료 / 위조 / 다른 용도 토큰) × (정상 / 각 필드 누락·빈 값·형식 오류·경계값) | 자동 생성 |
| ④ 생성형 차등 퍼징 | OpenAPI 스키마로 무작위 요청을 만들어 Java와 Rust에 동시에 보내 차이를 찾는다. 차이가 나면 최소 재현 케이스로 줄여 시나리오로 승격 | CI 야간 실행 |
| ⑤ 실제 클라이언트 흐름 | Android·iOS·admin-web이 실제로 호출하는 순서(가입, 사인 등록, 오늘의 주제 게시, 캘린더, 좋아요, 알림함, 관리자 검수) | 흐름당 1개 이상 |

### 4.4 특수 검증

- **동시성:** 같은 요청을 N개 스레드로 동시에 보낸 뒤 DB 불변식을 검사한다. 대상: 좋아요 중복, 리프레시 토큰 회전 재사용 감지, 같은 주제 재게시, 업로드 콜백 중복 도착, 관리자 세션 advisory lock, 탈퇴와 게시 경합. 두 구현 모두 불변식을 지키고 최종 상태가 같은지 본다.
- **마이그레이션 호환:** (a) Flyway로 적용한 DB를 Rust 마이그레이터가 검증 통과, (b) Rust로 적용한 DB를 Flyway `validate`가 통과, (c) 두 경우 `pg_dump --schema-only` 결과 동일. 운영 전환 후에도 Java로 즉시 되돌릴 수 있게 하는 조건이다.
- **Lambda 콜백 계약:** `backend/lambda/image-processor`의 실제 서명 코드(`callback.py`)로 Rust 콜백 엔드포인트를 호출하는 계약 시험. 본문 상한(`CALLBACK_MAX_BODY_BYTES`), 시각 허용 범위(과거 300초, 미래 30초), 환경 바인딩 포함.
- **HTTP 견고성:** 파서 퍼징(`cargo-fuzz`), 느린 클라이언트, 과대 헤더, chunked 본문, `Expect: 100-continue`, 잘못된 Content-Length, 요청 스머글링 패턴. 여기서는 Java와 같은 응답보다 **안전한 거절**을 기준으로 삼는다.
- **설정 계약:** 원본의 `check_env_contract.sh`가 검사하는 환경변수 계약을 Rust도 그대로 따른다. 기간 형식(`15m`, `PT1H`, `30d`), 누락·잘못된 값에서 기동 실패하는 조건(예: 리프레시 절대 만료 < 비활동 만료, 콜백 비밀 32자 미만, dev·prod에서 관리자 우회 켜짐)도 시나리오로 검사.

### 4.5 성능 검증

- **부하 모델:** 실제 클라이언트 호출 비율을 반영한 혼합 부하(공개 목록·오늘 게시물·캘린더 조회 위주 + 쓰기 일부). 같은 머신, 같은 DB, 같은 풀 크기에서 Java(워밍업 후)와 Rust를 비교.
- **지표:** 처리량, p50/p95/p99/최대 지연, 오류율, CPU, RSS, 기동 시간, 요청당 SQL 수와 DB 시간(`pg_stat_statements`).
- **게이트:** 모든 엔드포인트에서 Rust p99 ≤ Java p99, 처리량 ≥ Java, 요청당 SQL 수 ≤ Java. 하나라도 어기면 다음 단계로 못 간다.
- **SQL 기준선:** 녹화 실행 중 Hibernate가 보낸 SQL을 `pg_stat_statements`로 모아 "SQL 목록"을 만든다. Rust 쿼리 작성의 출발점이자 N+1·인덱스 누락 비교 기준이다. 주요 쿼리는 `EXPLAIN` 계획도 비교한다.
- **장시간 시험:** 24시간 부하로 메모리 증가·커넥션 누수·파일 디스크립터 누수 확인.

### 4.6 CI 게이트

PR마다: `cargo fmt`/`clippy -D warnings`, `forbid(unsafe_code)` 확인, `cargo-deny`(허용 목록·라이선스·취약점), 단위·리포지토리 테스트, 차등 시나리오 전체(Java·Rust 동시 모드), 마이그레이션 SQL 바이트 동일성, 라우트 목록 동일성, 테스트 추적표 진척.
야간: 차등 퍼징, 성능 비교, HTTP 퍼징.

---

## 5. 놓치기 쉬운 원본 동작 (시나리오로 반드시 고정할 것)

하네스가 결국 잡아내겠지만, 처음부터 시나리오로 박아 두어야 할 항목이다.

1. **검증 메시지 로케일:** 메시지를 지정하지 않은 제약이 40개 이상이다(`@NotBlank` 30, `@NotNull` 10 등). Spring은 요청의 `Accept-Language`로 기본 메시지를 고를 수 있어 응답 문구가 헤더에 따라 달라질 수 있다. `Accept-Language` 없음/ko/en/기타 조합으로 녹화해서 확인한다.
2. **오류 메시지 형식:** 기본 문구일 때만 `필드명: 메시지`로 붙이고, 지정 문구는 그대로 둔다. 여러 필드가 틀리면 "첫 번째" 오류만 나간다. 그 순서가 Spring 내부 순서에 의존하므로 다중 오류 케이스를 녹화한다.
3. **Jackson 동작:** 알 수 없는 필드 무시, `null`과 필드 누락의 구분(`@JsonProperty(required = true)` 1곳), `@JsonInclude(NON_NULL)` 2곳, 숫자 범위 초과·문자열 숫자 처리, 빈 본문, 시간 직렬화의 소수점 자릿수(PostgreSQL은 마이크로초 저장).
4. **보안 규칙 순서:** 관리자 공개 경로가 `/api/v1/admin/**`보다 먼저, 공개 GET 2개는 익명 또는 회원만(관리자 토큰은 거절), 그 외 기본값은 회원 토큰. dev·prod에서 관리자 우회가 켜지면 기동 실패.
5. **Spring Security 기본 응답 헤더:** `X-Content-Type-Options`, `X-Frame-Options`, `Cache-Control`/`Pragma`/`Expires`, `X-XSS-Protection` 등. 클라이언트 캐시 동작이 바뀔 수 있으므로 값까지 맞춘다.
6. **CORS:** 관리자 웹 origin 목록, preflight 응답, 허용되지 않은 origin의 처리.
7. **경로 처리:** 끝 슬래시, 대소문자, 인코딩된 경로, 알 수 없는 경로 404 본문(`요청한 API를 찾을 수 없습니다.`), 지원하지 않는 메서드 405, HEAD·OPTIONS 자동 처리.
8. **콜백 본문 상한:** HMAC 검증보다 먼저 크기로 끊는다. 서명은 **수신한 원문 바이트** 기준.
9. **시간대:** "오늘", 캘린더 월, 주제 기간 겹침 판단, cron 모두 Asia/Seoul. 자정 직전·직후 경계 시나리오.
10. **타임아웃 기반 상태 전이:** 이미지 처리 7분 초과 게시물은 "다음 게시물 생성 시점"에 거절 처리, 사인 처리 7분 초과 후 도착한 성공 콜백은 무시.
11. **커밋 후 알림:** 검수 대기 Slack 알림은 트랜잭션 커밋 뒤에만 나가고 롤백되면 나가지 않는다. Slack 타임아웃(연결 2초, 읽기 3초)과 실패가 요청 결과에 주는 영향.
12. **헬스체크:** DB가 내려갔을 때의 상태코드와 본문. 배포 스크립트가 이 값에 의존한다.
13. **종료 동작:** SIGTERM에서 진행 중 요청을 마치고 종료(systemd `SuccessExitStatus=143`, `TimeoutStopSec=30`).

---

## 6. 단계별 전환 계획

각 단계는 완료 조건을 만족해야 다음으로 넘어간다. 시간 대신 완료 조건으로 관리한다.

### Phase 0. 하네스 구축 (Rust 서버 코드 없이)
- `compose.yaml`: PostgreSQL 18.4 ×2, fakes, Java oracle(기준 커밋 JAR).
- `harness/inventory`: 원본에서 라우트 목록, 테스트 목록, 환경변수 계약, 마이그레이션 목록을 자동 추출(이 문서의 CSV를 만든 방식을 스크립트로 고정).
- 시나리오 러너·정규화·비교기, libfaketime 시계 제어, DB 스냅샷 복원.
- Java oracle에서 SQL 목록과 OpenAPI 문서 수집.
- 시나리오 1차분: 플랫폼 공통(404/405/잘못된 JSON/인증 매트릭스/CORS/헬스) + topic.
- **완료 조건:** Java를 Java와 비교했을 때 모든 시나리오가 통과한다(정규화 규칙의 거짓 양성 0). 같은 시나리오를 두 번 돌려 결과가 같다(결정성 확인).

### Phase 1. 플랫폼 계층
- HTTP 서버·라우터·미들웨어, JSON, 오류 모델, 설정 로더, JSON 로그·requestId, DB 풀·트랜잭션, Flyway 호환 마이그레이터, Clock, 스케줄러, 보안 헤더, CORS, 인증 미들웨어, 본문 상한, graceful shutdown, 헬스·info.
- **완료 조건:** 플랫폼 공통 시나리오 전부 통과, 마이그레이션 교차 적용 시험 통과, HTTP 퍼징 무중단, 빈 라우트 기준 기본 처리량이 Java 이상.

### Phase 2. 도메인 이식 (의존 순서)

| 순서 | 도메인 | 범위 | 주의점 |
| --- | --- | --- | --- |
| 2-1 | topic | 공개 목록, 관리자 주제 CRUD는 admin에서 | 기간 겹침 규칙, 시간대 |
| 2-2 | auth + user | 소셜 로그인(OIDC 3사), 가입 토큰, 가입, 리프레시 회전·비활동/절대 만료, 로그아웃, 탈퇴, Apple 토큰 교환·철회·임시 인증, 차단 계정 HMAC 지문 | 가장 큰 영역(테스트 343+122). JWKS 캐시·키 교체, ES256 서명 |
| 2-3 | 사인(user) | 업로드 URL 발급, 조회·변경, 처리 콜백 3종 | SigV4 presign, HeadObject, 7분 타임아웃 |
| 2-4 | post + photo | 업로드, 생성·수정·삭제, 목록·오늘·상세·캘린더·월 목록, 이미지 처리 콜백 3종 | 비관적 락 6곳, 재게시 규칙, 타임아웃 거절 |
| 2-5 | like | 좋아요·취소 | 멱등성, 동시성 |
| 2-6 | feedback | 피드백 등록 | |
| 2-7 | notification | 알림함 목록·상세·읽음·전체 읽음·미읽음 여부, Slack 검수 알림 | 커밋 후 발송 |
| 2-8 | admin | 로그인(BCrypt)·me·refresh·logout, 게시물 검수·삭제, 회원 상태, 주제 CRUD, 피드백, 감사 로그 | 동적 검색 조건·정렬·페이지, advisory lock, 개발용 우회 |
| 2-9 | 주기 작업 | 정리 작업 4종 + 런타임 지표 로그 | 시각 이동 시나리오 |

- 도메인 하나의 **완료 조건:** 해당 엔드포인트의 차등 시나리오 전부 통과, 추적표의 해당 행 전부 `이식` 또는 근거 있는 `제외`, 해당 규칙 ID 시나리오 통과, 요청당 SQL 수 ≤ Java.

### Phase 3. 비기능 검증
- 성능 게이트(4.5), 24시간 장시간 시험, 동시성 시험 전체, 차등 퍼징 무차이 연속 유지, 보안 점검(`cargo-deny`, `cargo-audit`, 의존성 `unsafe` 보고서, 비밀값 로그 노출 검사).
- **완료 조건:** 모든 게이트 통과 기록을 문서로 남김.

### Phase 4. 배포 준비
- 빌드: CodeBuild에서 `cargo build --release`(정적 링크 바이너리 검토), 같은 산출물을 dev·prod에 사용.
- systemd unit 교체(`ExecStart`만 바꾸고 보안 옵션 유지), CodeDeploy 스크립트의 헬스체크 경로 유지.
- 로그·대시보드: JSON 필드 호환 확인, JVM 지표 위젯을 프로세스 지표로 교체.
- 환경변수 계약 동일 유지(`check_env_contract.sh` 그대로 통과).

### Phase 5. 전환과 롤백
1. **dev 전환:** dev EC2에서 Rust로 교체하고 앱으로 실제 사용. 이 기간에 dev 트래픽의 읽기 요청을 Java로 재생해 응답을 비교(쓰기 요청은 재생하지 않음).
2. **prod 전환:** 같은 DB, 같은 `flyway_schema_history`를 쓰므로 바이너리만 바꾼다. 롤백은 이전 JAR로 CodeDeploy 재배포.
3. **롤백 조건:** 5xx 비율, p99 지연, 헬스체크 실패, 클라이언트 오류 보고 중 하나라도 기준을 넘으면 즉시 롤백.
4. 전환 후 일정 기간 Java 산출물과 하네스를 유지한다.

---

## 7. 원본 변경 추적

원본 저장소는 계속 바뀐다(최근 커밋에 모니터링·알림함이 추가됨). 이동 표적 문제를 이렇게 다룬다.

- 기준 커밋을 `harness/BASELINE`에 고정한다. 하네스의 Java oracle은 항상 이 커밋으로 빌드한다.
- 주기적으로 기준 커밋과 원본 최신 `backend/` 차이를 뽑아 "따라잡기 작업 목록"을 만든다: 새 마이그레이션, 바뀐 컨트롤러·DTO·서비스, 새 테스트, 바뀐 규칙 문서.
- 따라잡기는 ① 기준 커밋 올리기 → ② golden 재녹화 → ③ 실패한 시나리오가 곧 Rust 작업 목록, 순서로 진행한다.
- 운영 전환 직전에는 원본 백엔드 변경을 잠시 멈추는(코드 프리즈) 합의가 필요하다.

---

## 8. 리스크와 대응

| 리스크 | 영향 | 대응 |
| --- | --- | --- |
| 공개 EC2 직접 노출 상태에서 직접 만든 HTTP 서버의 결함 | 서비스 거부, 요청 스머글링 | 상한·타임아웃 기본값, 퍼징, 필요하면 앞단 프록시 도입을 별도 결정으로 |
| Spring·Jackson·Hibernate의 암묵 동작 누락 | 클라이언트 오동작 | 차등 비교를 바이트 단위로, 생성형 퍼징 |
| 시계 제어가 JVM에서 불완전 | 시간 의존 시나리오 불안정 | libfaketime 실패 시 테스트 전용 에이전트로 대체 |
| Flyway 체크섬 비호환 | 롤백 불가 | 교차 적용 시험을 Phase 1 완료 조건으로 |
| 원본이 계속 바뀜 | 끝나지 않는 추격 | 기준 커밋 고정, 따라잡기 절차, 전환 전 프리즈 |
| 암호·JWT 세부 차이(키 형식, 클레임, 시계 오차 허용) | 로그인 장애 | Java 발급 토큰을 Rust가 검증, 그 반대도 검증하는 교차 시험 |
| 의존성 정책이 더 엄격하게 정해짐(A안) | 범위가 크게 늘어남 | 2장 결정을 Phase 0 전에 확정 |

---

## 9. 바로 다음 작업

1. ~~2장 의존성 정책 확정~~ (B안 확정).
2. Phase 0 시작: `chalkak-rust`에 workspace·compose·하네스 골격을 만들고, Java oracle을 띄워 topic·플랫폼 공통 시나리오를 녹화.
3. 테스트 추적표(`java-test-inventory.csv`)를 저장소의 `harness/inventory/`로 옮겨 진척을 CI에서 집계.

## 부록

- [endpoints.csv](endpoints.csv): 51개 API의 도메인, 메서드, 경로, 핸들러, 인증 수준 (원본 소스에서 자동 추출)
- [java-test-inventory.csv](java-test-inventory.csv): Java 테스트 1,200개의 파일, 메서드, `@DisplayName`, 계층, 이식 대상·상태 (원본 소스에서 자동 추출)
