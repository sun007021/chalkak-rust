# chalkak-rust

[Chalkak](https://github.com/woowacourse-teams/2026-Chalkak) 백엔드(Spring Boot)를 프레임워크 없이 Rust로 옮기는 학습 프로젝트.

- 전환 계획: [docs/plan/migration-plan.md](docs/plan/migration-plan.md)
- 검증 하네스: [harness/README.md](harness/README.md)
- 구현 설계·이슈 단위: [docs/plan/implementation-design.md](docs/plan/implementation-design.md)
- 로컬 환경: `harness/scripts/up.sh` (docker compose로 PostgreSQL·원본 Java 서버·Rust 서버)
- 원칙: 웹 프레임워크·ORM 없음, 우리 코드에 `unsafe` 없음(`forbid(unsafe_code)`), 원본과 응답·DB 상태가 같을 것.

| 경로 | 내용 |
| --- | --- |
| `crates/platform/*` | 프레임워크 대신 직접 만드는 기반(HTTP 서버, DB, 설정·로그, JWT, AWS 서명) |
| `crates/app` | 도메인(원본 패키지와 1:1) |
| `crates/server` | 실행 파일 |
| `migrations/` | 원본 Flyway SQL 사본(바이트 동일, CI 검사) |
| `harness/` | Java 대 Rust 차등 검증 하네스 |
| `compose.yaml`, `docker/` | 로컬 개발·검증 환경 |

## 주기 작업과 런타임 상태 로그

`crates/app/src/jobs`가 원본 `@Scheduled` 4종과 `RuntimeSnapshotLogger`를 옮긴 것이다. 실행은 `chalkak-core`의 `Scheduler`(작업마다 스레드 하나, cron은 Asia/Seoul)가 한다. 서버가 종료하면 새 실행을 시작하지 않고, 실행 중인 작업이 끝나기를 기다린 뒤(임시 Apple 정리는 남은 행을 건드리지 않고 멈춘다) DB 풀을 닫는다.

| 작업 | 주기 | 끄는 환경변수 |
| --- | --- | --- |
| 리프레시 토큰 정리(회원·관리자) | 매일 04:30 | `CHALKAK_AUTH_REFRESHTOKEN_CLEANUP_ENABLED` |
| 회원가입 토큰 소진 기록 정리 | 매일 04:40 | `CHALKAK_AUTH_CONSUMEDSIGNUPTOKEN_CLEANUP_ENABLED` |
| 알림 정리 | 매일 04:50 | `CHALKAK_NOTIFICATION_CLEANUP_ENABLED` |
| 임시 Apple 인증 정보 폐기·삭제 | 매시 정각 | `CHALKAK_AUTH_PENDINGAPPLEAUTHORIZATION_CLEANUP_ENABLED` |
| 런타임 상태 로그 | 60초(시작 즉시 한 번) | `CHALKAK_MONITORING_RUNTIMELOG_ENABLED` (local 기본 꺼짐, dev·prod 기본 켜짐) |

스케줄링 전체는 `CHALKAK_SCHEDULING_ENABLED=false`로 끈다(기본 켜짐, 작업별 키와 독립).

런타임 상태 로그는 원본과 같은 모양(로거 `chalkak.runtime`, INFO, `type=runtime`, 측정한 값만)이다. JVM 전용 지표는 Rust 프로세스에 대응 값이 없어 다음처럼 바꿨다.

| 원본 키 | Rust |
| --- | --- |
| `heapUsedBytes`, `heapMaxBytes`, `gcTimeMsTotal` | 생략. GC 힙이 없고, 의미가 다른 RSS를 같은 키에 넣으면 대시보드가 거짓 값을 그린다 |
| `threads` | 같은 키. `/proc/self/status`의 `Threads` |
| `hikariActive`, `hikariPending` | 같은 키. DB 연결 풀의 빌려간 연결 수, 연결을 기다리는 스레드 수 |
| (추가) `rssBytes`, `openFds` | `/proc/self/status`의 `VmRSS`, `/proc/self/fd` 항목 수 |

`/proc`이 없는 플랫폼에서는 프로세스 값이 로그에서 빠진다.
