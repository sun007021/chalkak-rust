# 성능·동시성·장시간 시험

[전환 계획 4.5](../plan/migration-plan.md#45-성능-검증)의 성능 검증을 재현하는 방법이다. 결과는 날짜별 보고서
(`report-*.md`)에 있다. 장시간(24시간) 시험 절차는 [soak-24h.md](soak-24h.md), 트래픽이 몰릴 때의 서버·풀 안정성 시험은 [stress.md](stress.md)에 있다.

## 구성

| 경로 | 내용 |
| --- | --- |
| `harness/loadgen/` | 부하 발생기(`chalkak-loadgen`). 스레드와 블로킹 소켓만 쓴다(tokio 없음). `seed`, `run`, `sql`, `race`, `stress`, `report` 명령 |
| `harness/perf/compose.yaml` | Java·Rust·각자의 PostgreSQL·가짜 서버. **libfaketime 없음**(실제 시계). 하네스 lane과 별개의 스택 |
| `harness/scripts/perf.sh` | 스택 기동, 시드, 번갈아 가며 측정, SQL 수 측정, 동시성·장시간 시험, 보고서 |

비교 조건은 두 구현이 같다.

- 같은 머신, 같은 PostgreSQL 18.4 설정(앱마다 전용 DB 컨테이너, `pg_stat_statements` 켜짐), 같은 시드 데이터.
- 같은 풀 크기: Java HikariCP 기본 10(원본이 설정하지 않음), Rust `DbConfig::DEFAULT_POOL_SIZE` 10.
- 같은 자원 한도: 앱 컨테이너 `cpus: 2`, 메모리 1.5GB(`PERF_APP_CPUS`, `PERF_APP_MEM`), DB `cpus: 1`.
- Java는 워밍업 후 측정한다(첫 라운드 워밍업 60초, 이후 20초, 그리고 엔드포인트별 실행 앞에도 3초).
- 요청 순서가 같다: 부하 발생기의 난수 시드가 같아서 두 서버가 같은 요청 열을 받는다.
- 주기 작업·런타임 로그는 두 앱 모두 같은 값이다(`PERF_SCHEDULING`, 기본 끔 = `CHALKAK_SCHEDULING_ENABLED=false`). 켜려면 `PERF_SCHEDULING=true`로 둘 다 켠다.

## 실행

```bash
# 필요: Docker, Rust 툴체인, 원본 JAR(harness/scripts/oracle-jar.sh가 /tmp/chalkak-lanes/oracle/app.jar에 만든다)
harness/scripts/perf.sh up              # 릴리스 빌드 -> 스택 기동 -> 시드(회원 300, 주제 60일, 게시물 9000, 좋아요 10.8만, 알림 600)
harness/scripts/perf.sh startup 3       # 재시작 3번씩 기동 시간
harness/scripts/perf.sh sql             # 시드를 다시 넣고 엔드포인트별 요청당 SQL 수·DB 시간
harness/scripts/perf.sh bench 5         # 5라운드. 라운드마다 Java/Rust 순서를 바꾼다
harness/scripts/perf.sh stress both     # 트래픽 급증·연결 폭주·유휴 연결·느린 클라이언트·DB 장애 시험(stress.md)
harness/scripts/perf.sh race            # 동시성 시험(같은 자원 동시 수정·좋아요·검수, 풀 고갈과 회복)
harness/scripts/perf.sh report          # 게이트 판정과 표 -> $PERF_OUT/report.md
harness/scripts/perf.sh soak rust 30m   # 장시간 시험
harness/scripts/perf.sh down
```

결과 JSON은 `$PERF_OUT`(기본 `target/perf/latest`)의 `java/`, `rust/`와 `sql-*.json`, `startup-*.json`, `race-*.json`에 쌓이고,
`report`가 한 번에 읽는다. 환경변수(`PERF_CONNS`, `PERF_RPS`, `PERF_DURATION`, `PERF_WARMUP`, `PERF_ROUNDS`,
`PERF_PORT_BASE`, `PERF_JAVA_OPTS`)는 `perf.sh` 머리말에 있다. 프로젝트 이름과 포트는 작업 트리마다 다르다
(경로 해시, 기본 포트 21000부터)라서 다른 작업과 겹치지 않는다.

부하 발생기를 직접 쓸 때:

```bash
target/release/chalkak-loadgen run --base 127.0.0.1:21010 --secret <ACCESS_TOKEN_SECRET hex> \
    --conns 64 --duration 30 --warmup 10 [--rps 300] [--only "GET /api/v1/topics"] [--no-keepalive] \
    [--idle-conns 250] [--pid <앱 PID>] [--db "<접속 문자열>"] [--interval 60] --out result.json
target/release/chalkak-loadgen list     # 엔드포인트 이름과 비율
```

- `--rps`가 없으면 최대 처리량 모드(연결마다 응답을 받자마자 다음 요청). 있으면 고정 RPS 모드이며, 지연은 **보내기로 한
  시각**부터 잰다(서버가 느려 밀린 시간도 지연에 들어간다).
- `--idle-conns`는 요청을 거의 보내지 않고 붙어만 있는 keep-alive 연결이다(워커를 붙잡는 구조인지 확인한다).
- `--pid`가 있으면 `/proc/<pid>`에서 CPU·RSS·FD·스레드를 1초마다 읽는다. `--db`가 있으면 DB 연결 수와 요청당 SQL을 함께 낸다
  (`--db`가 여는 연결 1개가 DB 연결 수에 들어가므로 `trend --pool`은 풀 크기 + 1).
- 2026-10-13 결과는 호스트가 포화라 `PERF_RPS=40`으로 잰 값이다.
- 토큰은 하네스와 같은 HS256 형식으로 직접 만든다(`ACCESS_TOKEN_SECRET`만 알면 된다). 시드는 DB에 직접 넣는다.

## 부하 구성

실제 운영 호출 수는 아직 없다(배포 전). 앱 화면 흐름에서 비율을 가정했다. `harness/loadgen/src/mix.rs`의 `WEIGHTS`에서 바꾼다.

| 구분 | 엔드포인트 | 비율 |
| --- | --- | ---: |
| 읽기 | 공개 목록(오늘 주제, 최신순) 14, 인기순 5, 랜덤순 3, 지난 주제 3 | 25 |
| | 오늘 주제 조회 15, 내 오늘 게시물 12, 게시물 상세 10 | 37 |
| | 캘린더 6, 캘린더 월 3 | 9 |
| | 알림 목록 3, 읽지 않음 5 | 8 |
| 쓰기 | 좋아요 5, 좋아요 취소 3, 제목 수정 1, 피드백 2, 업로드 URL 1, 알림 모두 읽음 1 | 13 |

쓰기는 전체의 약 14%다. 게시물 작성 완료(`POST /posts`)는 S3 객체 확인이 필요해 부하에 넣지 않았다(업로드 URL 발급까지만).
인증은 회원 300명의 토큰을 로그인 없이 직접 서명한다. 목록·주제 조회의 절반 정도는 익명이다.

## 게이트와 판정

전환 계획 4.5의 게이트: 모든 엔드포인트에서 Rust p99 ≤ Java p99, 처리량 ≥ Java, 요청당 SQL 수 ≤ Java.

- **p99**: 고정 RPS 혼합 부하에서 엔드포인트별 p99(둘 다 같은 요청률).
- **처리량**: 엔드포인트 하나만 연결 64개로 최대한 부른 실행의 req/s.
- **요청당 SQL**: 시드를 다시 넣은 상태에서 엔드포인트마다 같은 요청 40개를 보내 `pg_stat_statements`로 센 데이터 문장
  (SELECT/INSERT/UPDATE/DELETE) 수. BEGIN/COMMIT/SET 같은 제어 문장은 별도 열로 낸다.

비교는 라운드별 값의 **중앙값**으로 한다. 같은 머신을 다른 작업이 쓰므로 노이즈를 숨기지 않는다. 각 값은
`중앙값 (최소–최대)`로 쓰고 측정 전후 호스트 부하(`/proc/loadavg`)를 결과 JSON에 남긴다. 판정은 셋이다.

- 통과: Rust 중앙값이 Java 중앙값을 엄격히 만족한다.
- 통과\*: 엄격히는 어겼지만 차이가 라운드 사이 퍼짐(최대–최소)/중앙값 이내다. 이 항목은 보고서에서 따로 센다.
- 실패: 노이즈보다 크게 어겼다. 원인을 찾아 고친다.

호스트 부하가 코어 수보다 훨씬 높을 때는 절대 수치(req/s, ms)를 믿을 수 없고, 같은 조건에서 번갈아 잰 상대 비교와
요청당 CPU 시간(밀리초)이 더 믿을 만하다.

## 동시성 시험(`race`)

실행 중인 서버에 같은 자원을 향한 요청을 한꺼번에 보내고 응답 코드와 DB 불변식을 확인한다. Java와 Rust에 같은 시험을 돌린다.

| 시험 | 불변식 |
| --- | --- |
| like-same-user | 같은 회원이 같은 게시물에 32번 동시에 좋아요: 모두 200, 행은 1개 |
| like-many-users | 서로 다른 회원 64명이 동시에 좋아요: 행 64개 |
| like-unlike-same-user | 같은 회원의 좋아요/취소 40개 동시: 모두 2xx, 최종 행 0 또는 1 |
| update-same-post | 같은 게시물 제목을 16개가 동시에 수정: 모두 200, 최종 제목은 제출한 값 중 하나 |
| update-vs-delete | 수정 8개와 삭제 1개 동시: 5xx 없음, 삭제 반영 |
| moderate-approve-x8 | 같은 검수 대기 게시물을 승인 8개 동시: 2xx 1건, 감사 로그·알림 각 1건 |
| moderate-approve-vs-reject | 승인 4개와 반려 4개 동시: 2xx 1건, 최종 상태와 감사 로그·알림 종류 일치 |
| moderate-vs-owner-update | 검수와 작성자 수정 동시: 검수 성공, 감사 로그 1건 |
| feedback-burst | 피드백 100개 동시: 모두 201, 행 100개 |
| pool-exhaustion-recovers | 테이블 잠금으로 쓰기 30개가 풀(10)을 다 붙잡고 읽기 10개가 연결을 기다린 채 5초: 잠금을 풀면 모두 성공, DB 연결 수가 풀 크기 안, 헬스체크 UP |
| pool-exhaustion-timeout | 같은 상황을 35초 유지(연결 대기 한도 30초): 대기하던 읽기는 5xx로 끝나되 끊기지 않고, 풀려난 뒤 회복 |

DB 단위의 동시성(좋아요 중복, 업로드 콜백 중복, 수정·검수 잠금 순서)은 `crates/app/tests/*concurrency*`, `like_db.rs`가 이미 검증한다.
이 시험은 HTTP까지 포함해 실제 서버를 대상으로 한다.
