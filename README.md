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
