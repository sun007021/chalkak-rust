# 트랜잭션 모양 비교: Java(Spring + Hibernate + pgjdbc) 대 Rust

- 실행: `harness/scripts/check.sh txshape`(Java lane 1개 + Rust lane 1개, 하네스 시나리오 48개 전체, 2026-10-15). 앱이 보낸 문장만 DB 문장 로그(`log_statement=all`)에서 가져왔다. 격리 수준은 PostgreSQL 기본 READ COMMITTED.
## 읽는 법과 해석

- 판정 규칙은 `harness/parity/src/txshape.rs` 머리 주석과 단위 테스트가 정한다. 문장 서명(종류·대상 테이블·잠금)은 `harness/parity/src/sqlsig.rs`.
- 잠금은 종류와 대상(`OF` 테이블)으로 식별한다. `OF`가 없으면 FROM의 모든 테이블을 잠그는 것으로 보고 `OF 전체`로 적는다
  (Hibernate는 `OF p1_0`을 붙이고 손 SQL은 붙이지 않는다). 같이 조인한 읽기 테이블은 잠금 식별에서 뺀다.
- 읽기 전용 단계의 `BEGIN READ ONLY` 대 자동 커밋은 SELECT가 한 문장이면 같음, 둘 이상이면 동등(읽기 묶음 다름)으로 센다.
- 격리 수준은 모든 트랜잭션이 기본(READ COMMITTED)이다. Java도 Rust도 격리 수준을 바꾸는 문장을 보내지 않았다(`격리 수준 다름` 0건).
- 주기 작업(스케줄러)은 lane에서 꺼져 있어(`CHALKAK_SCHEDULING_ENABLED=false`) 표에 들어오지 않는다.
- 쓰기가 다른 트랜잭션으로 나뉜 단계(원자성 차이)는 0건이다. 모든 "다름"은 아래 네 묶음이다. 모든 시나리오에서 응답과 DB 상태는 두 구현이 일치한다.

### 다름 해석

1. **`FOR NO KEY UPDATE`(Java) 대 `FOR UPDATE`(Rust): 사용자 행(35단계), `pending_apple_authorizations` 행(13단계).**
   Java는 `@Lock(PESSIMISTIC_WRITE)`가 Hibernate PostgreSQL 방언에서 `for no key update`가 된다. Rust는 `FOR UPDATE`다
   (`crates/app/src/user/repository.rs:59`, `admin/user/repository.rs:233`, `auth/repository.rs:352`). 실제 의미 차이가 있다.
   `FOR UPDATE`는 외래 키 검사가 잡는 `FOR KEY SHARE`와 충돌한다. `users(id)`를 참조하는 테이블이 많아(posts, post_likes,
   post_image_uploads, user_refresh_tokens, notifications, feedbacks, social_accounts) Rust에서 한 회원의 행을 잠근 트랜잭션
   (탈퇴, 사인 등록, 관리자 상태 변경)이 끝날 때까지 같은 회원의 게시물 작성·좋아요·알림 생성 INSERT가 기다린다. Java는 기다리지
   않는다. 데이터가 틀어지지는 않고 더 세게 잠가 대기와 교착 가능성만 늘어난다(락을 빨리 푸는 짧은 트랜잭션이라 위험은 낮다).
   고치려면 세 곳을 `FOR NO KEY UPDATE`로 바꾼다.
2. **사진 행 추가 잠금: Rust `OF p, ph`, Java `OF p1_0`(게시물만).** 게시물 잠금 조회 중 사진을 함께 읽는 것
   (`post/repository.rs:237,253,350`, `admin/post/repository.rs:316`; 좋아요의 `FOR SHARE OF p, ph`, 관리자 승인·거절·삭제와
   게시물 수정·삭제의 `FOR NO KEY UPDATE OF photos,posts` 등, 58+18+12+17단계)에서 Rust가 사진 행도 잠근다.
   `post/repository.rs` 머리 주석은 "JOIN FETCH 잠금 조회는 사진 행도 같이 잠근다"고 적었지만 실제 Java 로그는
   `for share of p1_0`, `for no key update of p1_0`뿐이다. 좋아요의 `FOR SHARE OF ph`는 같은 사진 행을 갱신하는 처리 콜백
   (`UPDATE photos`)과 충돌하지만 Java는 충돌하지 않는다. `FOR NO KEY UPDATE OF ph`는 같은 종류끼리만 충돌해 영향이 작다.
   사진 갱신은 항상 게시물 잠금 뒤에 하므로(`p`가 먼저) 교착 순서 문제는 보이지 않는다. 역시 더 세게 잠글 뿐 이상 현상은 아니다.
3. **쓰기 한 문장이 `BEGIN ... COMMIT` 대 자동 커밋, 한쪽만 ROLLBACK(알림 읽음 404 3단계):** Java `markRead`는 `@Transactional` 안에서
   `UPDATE`(0행)를 한 뒤 404 예외로 롤백하고, Rust는 같은 조건부 `UPDATE`를 자동 커밋으로 보낸다. 0행이라 되돌릴 것이 없고
   효과가 같다(표는 "동등"이 아니라 "다름"으로 남는다. 로그만으로는 영향 행 수를 모르기 때문이다).
4. **Rust가 더 보내는 쓰기(롤백되거나 0행):**
   - 관리자 게시물 거절·회원 상태 변경이 민감 정보 검증으로 400이 되는 단계(2단계): Rust는 `UPDATE`를 보낸 뒤 롤백한다. Java는
     엔티티만 바꾸고 flush 전에 예외로 롤백해 `UPDATE`가 없다. 둘 다 롤백이라 결과가 같다.
   - 탈퇴(2단계): Rust는 `DELETE FROM apple_authorizations`를 항상 보내고, Java는 조회해 행이 있을 때만 지운다. Apple 연결이 없는
     회원에서는 0행 삭제라 같다.
   - 탈퇴의 읽기 전용 묶음(Java `BEGIN READ ONLY`에 SELECT 둘 대 Rust 자동 커밋)과 SELECT 수 차이는 동등 사유와 같다.

### 동등 해석

- `쓰기 한 문장`: Java는 `BEGIN; 문장; COMMIT`, Rust는 자동 커밋(문장 하나라 원자성이 같다).
- `읽기 묶음 다름`: Java `BEGIN READ ONLY`로 SELECT 여러 개를 묶고 Rust가 자동 커밋으로 보낸다. READ COMMITTED에서는 문장마다 스냅샷이라 응답이
  같고, 한 트랜잭션 안에서 읽은 여러 행의 일관성(같은 시점 스냅샷)만 Java가 더 강하다. REPEATABLE READ였다면 달랐을 것이다.
- `SELECT 대상 다름`·`SELECT 개수 다름`: Hibernate 지연 로딩과 조인 구성 차이다.


## 요약

시나리오 48개, 단계(요청) 1596개: 같음 1382, 동등 85, 다름 129.

| 시나리오 | 단계 | 같음 | 동등 | 다름 | 응답·DB 일치 |
| --- | ---: | ---: | ---: | ---: | --- |
| `admin__audit-feedbacks` | 27 | 24 | 0 | 3 | 일치 |
| `admin__auth` | 32 | 32 | 0 | 0 | 일치 |
| `admin__cors` | 11 | 11 | 0 | 0 | 일치 |
| `admin__posts` | 70 | 58 | 0 | 12 | 일치 |
| `admin__topics` | 81 | 81 | 0 | 0 | 일치 |
| `admin__users` | 65 | 54 | 0 | 11 | 일치 |
| `auth__apple-login` | 10 | 1 | 0 | 9 | 일치 |
| `auth__apple-signup-withdraw` | 7 | 1 | 2 | 4 | 일치 |
| `auth__request-validation` | 44 | 44 | 0 | 0 | 일치 |
| `auth__social-login-google` | 19 | 19 | 0 | 0 | 일치 |
| `auth__social-signup-email-policy` | 13 | 13 | 0 | 0 | 일치 |
| `auth__social-signup-flow` | 10 | 10 | 0 | 0 | 일치 |
| `auth__token-refresh` | 34 | 34 | 0 | 0 | 일치 |
| `feedback__submit` | 38 | 18 | 20 | 0 | 일치 |
| `internal__callback-auth` | 22 | 22 | 0 | 0 | 일치 |
| `internal__callback-body` | 60 | 37 | 0 | 23 | 일치 |
| `internal__callback-size` | 6 | 5 | 0 | 1 | 일치 |
| `internal__callback-state` | 13 | 4 | 0 | 9 | 일치 |
| `like__like-unlike` | 28 | 9 | 1 | 18 | 일치 |
| `notification__detail-and-read` | 35 | 28 | 4 | 3 | 일치 |
| `notification__inbox-list` | 11 | 11 | 0 | 0 | 일치 |
| `notification__inbox-query` | 70 | 70 | 0 | 0 | 일치 |
| `notification__read-all` | 10 | 7 | 3 | 0 | 일치 |
| `notification__route-matrix` | 33 | 33 | 0 | 0 | 일치 |
| `platform__accept-language` | 14 | 14 | 0 | 0 | 일치 |
| `platform__accept-negotiation` | 163 | 149 | 14 | 0 | 일치 |
| `platform__allow-order` | 42 | 42 | 0 | 0 | 일치 |
| `platform__auth-matrix` | 13 | 13 | 0 | 0 | 일치 |
| `platform__cors` | 4 | 4 | 0 | 0 | 일치 |
| `platform__health` | 2 | 2 | 0 | 0 | 일치 |
| `platform__method-not-allowed` | 4 | 4 | 0 | 0 | 일치 |
| `platform__request-body-errors` | 7 | 5 | 2 | 0 | 일치 |
| `platform__springdoc` | 207 | 207 | 0 | 0 | 일치 |
| `platform__tomcat-rejections` | 130 | 124 | 6 | 0 | 일치 |
| `platform__unknown-path` | 5 | 5 | 0 | 0 | 일치 |
| `post__create-errors` | 26 | 26 | 0 | 0 | 일치 |
| `post__create-flow` | 18 | 10 | 4 | 4 | 일치 |
| `post__detail-update-delete` | 42 | 22 | 13 | 7 | 일치 |
| `post__list` | 36 | 35 | 1 | 0 | 일치 |
| `post__today-calendar` | 27 | 12 | 15 | 0 | 일치 |
| `topic__before-open` | 3 | 3 | 0 | 0 | 일치 |
| `topic__get-by-date` | 8 | 8 | 0 | 0 | 일치 |
| `topic__kst-boundary` | 3 | 3 | 0 | 0 | 일치 |
| `topic__soft-deleted` | 1 | 1 | 0 | 0 | 일치 |
| `user__callback-body-limit` | 6 | 5 | 0 | 1 | 일치 |
| `user__signature-callbacks` | 35 | 23 | 0 | 12 | 일치 |
| `user__signature` | 35 | 25 | 0 | 10 | 일치 |
| `user__withdraw` | 16 | 14 | 0 | 2 | 일치 |

## 다름

### 다름 1: 57단계

- 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts
- 시나리오: `admin__audit-feedbacks` 2, `admin__posts` 11, `internal__callback-body` 23, `internal__callback-size` 1, `internal__callback-state` 9, `post__create-flow` 4, `post__detail-update-delete` 7

예: `admin__audit-feedbacks` / 게시물 승인

Java:
```
BEGIN: SELECT posts FOR NO KEY UPDATE OF posts; INSERT admin_audit_logs; INSERT notifications; UPDATE posts → COMMIT
```
Rust:
```
BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts; INSERT admin_audit_logs; INSERT notifications → COMMIT
```

### 다름 2: 34단계

- 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users
- 시나리오: `admin__audit-feedbacks` 1, `admin__users` 10, `user__callback-body-limit` 1, `user__signature-callbacks` 12, `user__signature` 10

예: `admin__audit-feedbacks` / 회원 차단

Java:
```
BEGIN: SELECT users FOR NO KEY UPDATE OF users; INSERT admin_audit_logs; UPDATE users → COMMIT
```
Rust:
```
BEGIN: SELECT users FOR UPDATE OF users; UPDATE users; INSERT admin_audit_logs → COMMIT
```

### 다름 3: 18단계

- 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts
- 시나리오: `like__like-unlike` 18

예: `like__like-unlike` / 좋아요

Java:
```
BEGIN RO: SELECT users → COMMIT
BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT
```
Rust:
```
자동커밋: SELECT users
BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT
```

### 다름 4: 13단계

- 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations
- 시나리오: `auth__apple-login` 9, `auth__apple-signup-withdraw` 4

예: `auth__apple-login` / 신규 Apple 회원은 code를 교환하고 미가입으로 응답

Java:
```
BEGIN: SELECT social_accounts,users → COMMIT
BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT
BEGIN: INSERT pending_apple_authorizations → COMMIT
```
Rust:
```
BEGIN: SELECT social_accounts,users → COMMIT
BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT
자동커밋: INSERT pending_apple_authorizations
```

### 다름 5: 3단계

- 한쪽만 ROLLBACK(Java만)
- 시나리오: `notification__detail-and-read` 3

예: `notification__detail-and-read` / 읽음: 남의 알림은 404

Java:
```
BEGIN RO: SELECT users → COMMIT
BEGIN: UPDATE notifications → ROLLBACK
```
Rust:
```
자동커밋: SELECT users
자동커밋: UPDATE notifications
```

### 다름 6: 1단계

- 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts
- 쓰기 문장 집합 다름: Java만 [] / Rust만 [UPDATE posts]
- 시나리오: `admin__posts` 1

예: `admin__posts` / moderation reject sensitive

Java:
```
BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK
```
Rust:
```
BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts → ROLLBACK
```

### 다름 7: 1단계

- 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users
- 쓰기 문장 집합 다름: Java만 [] / Rust만 [UPDATE users]
- 시나리오: `admin__users` 1

예: `admin__users` / status sensitive reason

Java:
```
BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK
```
Rust:
```
BEGIN: SELECT users FOR UPDATE OF users; UPDATE users → ROLLBACK
```

### 다름 8: 1단계

- 쓰기 문장 집합 다름: Java만 [] / Rust만 [DELETE apple_authorizations]
- 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다
- SELECT 개수 다름: Java 9 / Rust 7 (Java만 [SELECT apple_authorizations; SELECT social_accounts], Rust만 [])
- 시나리오: `user__withdraw` 1

예: `user__withdraw` / 탈퇴

Java:
```
BEGIN RO: SELECT users → COMMIT
BEGIN RO: SELECT social_accounts; SELECT apple_authorizations → COMMIT
BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations ×2; SELECT social_accounts; SELECT user_refresh_tokens; DELETE social_accounts; SELECT advisory_xact_lock/1 ×2; UPDATE user_refresh_tokens; UPDATE users → COMMIT
```
Rust:
```
자동커밋: SELECT users
자동커밋: SELECT social_accounts
자동커밋: SELECT apple_authorizations
BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations; DELETE apple_authorizations; DELETE social_accounts; SELECT user_refresh_tokens; SELECT advisory_xact_lock/1 ×2; UPDATE user_refresh_tokens; UPDATE users → COMMIT
```

### 다름 9: 1단계

- 쓰기 문장 집합 다름: Java만 [] / Rust만 [DELETE apple_authorizations]
- 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다
- SELECT 개수 다름: Java 8 / Rust 7 (Java만 [SELECT apple_authorizations], Rust만 [])
- 시나리오: `user__withdraw` 1

예: `user__withdraw` / 정지 회원도 탈퇴할 수 있다

Java:
```
BEGIN RO: SELECT users → COMMIT
BEGIN RO: SELECT social_accounts; SELECT apple_authorizations → COMMIT
BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations ×2; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; UPDATE users → COMMIT
```
Rust:
```
자동커밋: SELECT users
자동커밋: SELECT social_accounts
자동커밋: SELECT apple_authorizations
BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations; DELETE apple_authorizations; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; UPDATE users → COMMIT
```

## 동등 사유 집계

| 사유 | 단계 수 |
| --- | ---: |
| SELECT 개수 다름 | 2 |
| SELECT 대상 다름 | 9 |
| 쓰기 한 문장 | 48 |
| 읽기 묶음 다름 | 36 |

## 전체 단계

<details><summary><code>admin__audit-feedbacks</code> 관리자 감사 로그와 피드백 조회 (같음 24, 동등 0, 다름 3)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 회원 차단 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users; INSERT admin_audit_logs; UPDATE users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users; INSERT admin_audit_logs → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 게시물 승인 | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts; INSERT admin_audit_logs; INSERT notifications; UPDATE posts → COMMIT | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts; INSERT admin_audit_logs; INSERT notifications → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 게시물 거절 | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 주제 등록 | 같음 | BEGIN: SELECT topics ×2; INSERT topics; INSERT admin_audit_logs; SELECT posts,topics → COMMIT | BEGIN: SELECT topics ×2; INSERT topics; INSERT admin_audit_logs; SELECT posts,topics → COMMIT |  |
| 감사 로그 기본 | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| 감사 로그 작업 필터 | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| 감사 로그 대상 필터 | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| 감사 로그 작업자 필터 | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| 감사 로그 기간 필터 | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| 감사 로그 대상 id 필터 | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| 피드백 기본 | 같음 | BEGIN RO: SELECT feedbacks,users → COMMIT | 자동커밋: SELECT feedbacks,users |  |
| 피드백 오래된 순 | 같음 | BEGIN RO: SELECT feedbacks,users → COMMIT | 자동커밋: SELECT feedbacks,users |  |
| 피드백 둘째 페이지 | 같음 | BEGIN RO: SELECT feedbacks,users → COMMIT | 자동커밋: SELECT feedbacks,users |  |
| 피드백 경계 크기 | 같음 | BEGIN RO: SELECT feedbacks,users → COMMIT | 자동커밋: SELECT feedbacks,users |  |
| audit action bogus | 같음 |  |  |  |
| audit targetType bogus | 같음 |  |  |  |
| audit adminId bad | 같음 |  |  |  |
| audit range inverted | 같음 |  |  |  |
| audit sort bad | 같음 |  |  |  |
| feedback sort bad | 같음 |  |  |  |
| feedback page 0 | 같음 |  |  |  |
| audit list | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| feedbacks list | 같음 | BEGIN RO: SELECT feedbacks,users → COMMIT | 자동커밋: SELECT feedbacks,users |  |
| audit action space | 같음 | BEGIN RO: SELECT admin_audit_logs,admins → COMMIT | 자동커밋: SELECT admin_audit_logs,admins |  |
| audit sort lead space | 같음 |  |  |  |
| audit targetId bad | 같음 |  |  |  |
| audit occurredFrom bad | 같음 |  |  |  |

</details>

<details><summary><code>admin__auth</code> 관리자 인증: 로그인·재발급·로그아웃·내 정보 (같음 32, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| login no username | 같음 |  |  |  |
| login no password | 같음 |  |  |  |
| login blank pw | 같음 |  |  |  |
| login long | 같음 |  |  |  |
| login long pw | 같음 |  |  |  |
| login ok pw 100 | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login unknown | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login ok | 같음 | BEGIN: SELECT admins; INSERT admin_refresh_tokens → COMMIT | BEGIN: SELECT admins; INSERT admin_refresh_tokens → COMMIT |  |
| me | 같음 | BEGIN RO: SELECT admins → COMMIT | 자동커밋: SELECT admins |  |
| me user token | 같음 |  |  |  |
| me none | 같음 |  |  |  |
| me unknown admin | 같음 | BEGIN RO: SELECT admins → ROLLBACK | 자동커밋: SELECT admins |  |
| refresh empty | 같음 |  |  |  |
| refresh bogus | 같음 | BEGIN: SELECT admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens → COMMIT |  |
| refresh long | 같음 | BEGIN: SELECT admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens → COMMIT |  |
| logout empty | 같음 |  |  |  |
| logout bogus | 같음 | BEGIN: SELECT admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens → COMMIT |  |
| logout long | 같음 | BEGIN: SELECT admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens → COMMIT |  |
| login cookie | 같음 | BEGIN: SELECT admins; INSERT admin_refresh_tokens → COMMIT | BEGIN: SELECT admins; INSERT admin_refresh_tokens → COMMIT |  |
| refresh ok | 같음 | BEGIN: SELECT admin_refresh_tokens; SELECT advisory_xact_lock/1; SELECT admin_refresh_tokens; INSERT admin_refresh_tokens; UPDATE admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens; SELECT advisory_xact_lock/1; SELECT admin_refresh_tokens; UPDATE admin_refresh_tokens; INSERT admin_refresh_tokens → COMMIT |  |
| refresh reuse | 같음 | BEGIN: SELECT admin_refresh_tokens; SELECT advisory_xact_lock/1; SELECT admin_refresh_tokens; UPDATE admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens; SELECT advisory_xact_lock/1; SELECT admin_refresh_tokens; UPDATE admin_refresh_tokens → COMMIT |  |
| logout after | 같음 | BEGIN: SELECT admin_refresh_tokens; SELECT advisory_xact_lock/1; SELECT admin_refresh_tokens → COMMIT | BEGIN: SELECT admin_refresh_tokens; SELECT advisory_xact_lock/1; SELECT admin_refresh_tokens → COMMIT |  |
| login wrong pw | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login case | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login trailing space | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login pw multibyte | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login get | 같음 |  |  |  |
| login form | 같음 |  |  |  |
| login username number | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |
| login username array | 같음 |  |  |  |
| login username blank pw ok | 같음 |  |  |  |
| login long pw 72 bytes | 같음 | BEGIN: SELECT admins → ROLLBACK | BEGIN: SELECT admins → ROLLBACK |  |

</details>

<details><summary><code>admin__cors</code> 관리자 CORS: 실제 관리자 엔드포인트 (같음 11, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 허용 Origin 로그인 | 같음 | BEGIN: SELECT admins; INSERT admin_refresh_tokens → COMMIT | BEGIN: SELECT admins; INSERT admin_refresh_tokens → COMMIT |  |
| 로그인 preflight | 같음 |  |  |  |
| 사용자 상태 변경 preflight | 같음 |  |  |  |
| 검수 preflight | 같음 |  |  |  |
| 게시물 삭제 preflight | 같음 |  |  |  |
| 주제 삭제 preflight | 같음 |  |  |  |
| 허용하지 않는 Origin 검수 preflight | 같음 |  |  |  |
| 허용 밖 메서드 preflight | 같음 |  |  |  |
| 허용 Origin 실제 요청 | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| 허용하지 않는 Origin 실제 요청 | 같음 |  |  |  |
| 허용하지 않는 Origin 토큰 없음 | 같음 |  |  |  |

</details>

<details><summary><code>admin__posts</code> 관리자 게시물: 목록 조건, 상세, 검수, 삭제 (같음 58, 동등 0, 다름 12)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| posts status validating | 같음 |  |  |  |
| posts status bogus | 같음 |  |  |  |
| posts topicId bad | 같음 |  |  |  |
| posts topicDate bad | 같음 |  |  |  |
| posts createdAtFrom date only | 같음 |  |  |  |
| posts createdAtFrom ok | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom offset | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom lower | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom nosec | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts range inverted | 같음 |  |  |  |
| posts userId upper | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts userId short | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts empty uuid | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts topicDate offset | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| post id bad | 같음 |  |  |  |
| post validating detail | 같음 | BEGIN RO: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users → ROLLBACK | 자동커밋: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users |  |
| moderation empty | 같음 |  |  |  |
| moderation validating | 같음 |  |  |  |
| moderation pending | 같음 |  |  |  |
| moderation approve with reason | 같음 |  |  |  |
| moderation reject no reason | 같음 |  |  |  |
| moderation reject blank | 같음 |  |  |  |
| moderation reject long | 같음 |  |  |  |
| moderation already approved | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation deleted post | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation missing | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation reject sensitive | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts / 쓰기 문장 집합 다름: Java만 [] / Rust만 [UPDATE posts] |
| delete empty | 같음 |  |  |  |
| delete blank | 같음 |  |  |  |
| delete long | 같음 |  |  |  |
| delete validating | 다름 | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| delete missing | 다름 | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| posts list | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| post detail | 같음 | BEGIN RO: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users |  |
| post detail approved | 같음 | BEGIN RO: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users |  |
| post detail deleted | 같음 | BEGIN RO: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users |  |
| post detail withdrawn | 같음 | BEGIN RO: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users |  |
| post detail noupload | 같음 | BEGIN RO: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT admin_audit_logs,photos,post_image_uploads,post_likes,posts,topics,users |  |
| posts userId space | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts topicDate space | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom space | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom nozone | 같음 |  |  |  |
| posts createdAtFrom 12 digits | 같음 |  |  |  |
| posts createdAtFrom sec 60 | 같음 |  |  |  |
| posts createdAtFrom hour 24 | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom leap | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts createdAtFrom lead space | 같음 |  |  |  |
| posts topicDate datetime | 같음 |  |  |  |
| posts topicDate compact | 같음 |  |  |  |
| posts topicId upper | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts userId nonhex | 같음 |  |  |  |
| posts userId 36plus | 같음 |  |  |  |
| posts userId plus | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts status lowercase | 같음 |  |  |  |
| posts status space | 같음 | BEGIN RO: SELECT photos,post_likes,posts,topics,users → COMMIT | 자동커밋: SELECT photos,post_likes,posts,topics,users |  |
| posts two binding errors | 같음 |  |  |  |
| posts binding then range | 같음 |  |  |  |
| moderation ordinal | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation string ordinal | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation lead space | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation lowercase | 같음 |  |  |  |
| moderation empty string | 같음 |  |  |  |
| moderation float | 같음 |  |  |  |
| moderation bool | 같음 |  |  |  |
| moderation reason num | 같음 |  |  |  |
| moderation reason empty | 같음 |  |  |  |
| moderation null reason | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation reject trimmed 500 | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation reject nbsp | 다름 | BEGIN: SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| moderation reject 0x1f | 같음 |  |  |  |

</details>

<details><summary><code>admin__topics</code> 관리자 주제: 목록, 등록, 수정, 삭제 (같음 81, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| topic create null title | 같음 |  |  |  |
| topic create no title | 같음 |  |  |  |
| topic create no topicDate | 같음 |  |  |  |
| topic create no startsAt | 같음 |  |  |  |
| topic create no endsAt | 같음 |  |  |  |
| topics phase bogus | 같음 |  |  |  |
| topics dateFrom bad | 같음 |  |  |  |
| topics date inverted | 같음 |  |  |  |
| topics sort bad | 같음 |  |  |  |
| topics page overflow | 같음 |  |  |  |
| topic create title long | 같음 |  |  |  |
| topic create bad date | 같음 |  |  |  |
| topic create date mismatch | 같음 |  |  |  |
| topic create epoch | 같음 |  |  |  |
| topic create offset | 같음 | BEGIN: SELECT topics ×2; INSERT topics; INSERT admin_audit_logs; SELECT posts,topics → COMMIT | BEGIN: SELECT topics ×2; INSERT topics; INSERT admin_audit_logs; SELECT posts,topics → COMMIT |  |
| topic create blank title | 같음 |  |  |  |
| topic create no body | 같음 |  |  |  |
| topic create ok | 같음 | BEGIN: SELECT topics → ROLLBACK | BEGIN: SELECT topics → ROLLBACK |  |
| topic create dup | 같음 | BEGIN: SELECT topics → ROLLBACK | BEGIN: SELECT topics → ROLLBACK |  |
| topic create overlap | 같음 |  |  |  |
| topic delete no body | 같음 |  |  |  |
| topic delete empty | 같음 |  |  |  |
| topic delete blank | 같음 |  |  |  |
| topic delete long | 같음 |  |  |  |
| topic delete closed | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic put open | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic put deleted | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic get deleted | 같음 | BEGIN RO: SELECT posts,topics → ROLLBACK | 자동커밋: SELECT posts,topics |  |
| topic put future | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics; SELECT topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics; SELECT topics → ROLLBACK |  |
| topic delete future | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics; INSERT admin_audit_logs; UPDATE topics → COMMIT | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics; UPDATE topics; INSERT admin_audit_logs → COMMIT |  |
| topics list | 같음 | BEGIN RO: SELECT posts,topics → COMMIT | 자동커밋: SELECT posts,topics |  |
| topics list phase open | 같음 | BEGIN RO: SELECT posts,topics → COMMIT | 자동커밋: SELECT posts,topics |  |
| topics list dates | 같음 | BEGIN RO: SELECT posts,topics → COMMIT | 자동커밋: SELECT posts,topics |  |
| topic starts int | 같음 |  |  |  |
| topic starts decimal | 같음 |  |  |  |
| topic starts string int | 같음 |  |  |  |
| topic starts string decimal | 같음 |  |  |  |
| topic starts epoch string neg | 같음 |  |  |  |
| topic starts iso zone | 같음 |  |  |  |
| topic starts iso nozone | 같음 |  |  |  |
| topic starts iso nosec | 같음 |  |  |  |
| topic starts date only | 같음 |  |  |  |
| topic starts lowercase | 같음 |  |  |  |
| topic starts space | 같음 |  |  |  |
| topic starts bool | 같음 |  |  |  |
| topic starts array | 같음 |  |  |  |
| topic starts object | 같음 |  |  |  |
| topic starts empty string | 같음 |  |  |  |
| topic starts null | 같음 |  |  |  |
| topic starts huge | 같음 |  |  |  |
| topic starts exp | 같음 |  |  |  |
| topic starts long digits | 같음 |  |  |  |
| topic date datetime | 같음 |  |  |  |
| topic date datetime z | 같음 |  |  |  |
| topic date lead space | 같음 |  |  |  |
| topic date compact | 같음 |  |  |  |
| topic date int | 같음 |  |  |  |
| topic date array | 같음 |  |  |  |
| topic date offset | 같음 |  |  |  |
| topic date empty | 같음 |  |  |  |
| topic date short month | 같음 |  |  |  |
| topic date feb30 | 같음 |  |  |  |
| topic date year5 | 같음 |  |  |  |
| topic title number | 같음 |  |  |  |
| topic title bool | 같음 |  |  |  |
| topic title array | 같음 |  |  |  |
| topic title empty | 같음 |  |  |  |
| topic title 255 | 같음 |  |  |  |
| topic title 256 padded | 같음 |  |  |  |
| topic title surrogate 255 | 같음 |  |  |  |
| topic title surrogate 128 | 같음 |  |  |  |
| topic put bad id | 같음 |  |  |  |
| topic put missing | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic put unchanged | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic get bad id | 같음 |  |  |  |
| topic get missing | 같음 | BEGIN RO: SELECT posts,topics → ROLLBACK | 자동커밋: SELECT posts,topics |  |
| topic delete bad id | 같음 |  |  |  |
| topic delete missing | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic delete reason num | 같음 | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK | BEGIN: SELECT topics FOR NO KEY UPDATE OF topics → ROLLBACK |  |
| topic delete reason 500 spaces | 같음 |  |  |  |
| topic delete reason 501 | 같음 |  |  |  |

</details>

<details><summary><code>admin__users</code> 관리자 회원: 목록 조건, 상세, 상태 변경 (같음 54, 동등 0, 다름 11)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| users status bogus | 같음 |  |  |  |
| users status empty | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users sort bogus | 같음 |  |  |  |
| users sort empty | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page abc | 같음 |  |  |  |
| users page 0 | 같음 |  |  |  |
| users pageSize 101 | 같음 |  |  |  |
| users page0 status bogus | 같음 |  |  |  |
| users page0 pageSize0 | 같음 |  |  |  |
| users email long | 같음 |  |  |  |
| users page overflow | 같음 |  |  |  |
| users page huge offset | 같음 |  |  |  |
| users page float | 같음 |  |  |  |
| users page plus | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page space | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page repeated | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users email blank | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users email filter | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users status lowercase | 같음 |  |  |  |
| users status banned page2 | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| user id bad | 같음 |  |  |  |
| user id noncanonical | 같음 |  |  |  |
| user id upper | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| user missing | 같음 | BEGIN RO: SELECT posts,social_accounts,users → ROLLBACK | 자동커밋: SELECT posts,social_accounts,users |  |
| status body no reason | 같음 |  |  |  |
| status body no status | 같음 |  |  |  |
| status body bad enum | 같음 |  |  |  |
| status body enum ordinal | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users; INSERT admin_audit_logs; UPDATE users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users; INSERT admin_audit_logs → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| status body reason blank | 같음 |  |  |  |
| status body reason long | 같음 |  |  |  |
| status unknown user | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| status withdrawn | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| status already banned | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| status already active | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users; INSERT admin_audit_logs; UPDATE users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users; INSERT admin_audit_logs → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| status sensitive reason | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users / 쓰기 문장 집합 다름: Java만 [] / Rust만 [UPDATE users] |
| users sort space | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users sort lead space | 같음 |  |  |  |
| users status lead space | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users status trail space | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users status space | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page space only | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page hex | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page hex big | 같음 |  |  |  |
| users page inner space | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page neg hex | 같음 |  |  |  |
| users page leading zeros | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users page hash | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users pageSize plus | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users email upper multi | 같음 | BEGIN RO: SELECT posts,social_accounts,users → COMMIT | 자동커밋: SELECT posts,social_accounts,users |  |
| users two binding errors | 같음 |  |  |  |
| users two binding errors rev | 같음 |  |  |  |
| user status ordinal0 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| user status string ordinal | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| user status lowercase | 같음 |  |  |  |
| user status lead space | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| user status float | 같음 |  |  |  |
| user status negative | 같음 |  |  |  |
| user status big | 같음 |  |  |  |
| user status empty string | 같음 |  |  |  |
| user status bool | 같음 |  |  |  |
| user status reason number | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| user status reason array | 같음 |  |  |  |
| user status reason 500 plus | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| user status reason surrogate | 같음 |  |  |  |
| user status reason padded 501 | 같음 |  |  |  |

</details>

<details><summary><code>auth__apple-login</code> 인증: Apple 로그인(ID Token 검증 + authorizationCode 교환)과 교환 서버 오류 (같음 1, 동등 0, 다름 9)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| authorizationCode 없이 Apple 로그인 | 같음 |  |  |  |
| 신규 Apple 회원은 code를 교환하고 미가입으로 응답 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT<br>BEGIN: INSERT pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT<br>자동커밋: INSERT pending_apple_authorizations | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 임시 보관분이 있으면 다시 교환하지 않는다 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations; UPDATE pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations; UPDATE pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 교환한 ID Token의 사용자가 다르면 거절 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 교환한 ID Token의 nonce가 다르면 거절 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 만료되었거나 사용된 code (invalid_grant) | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 그 밖의 400 (invalid_client) | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| Apple 서버 오류 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 교환 응답에 refresh_token이 없음 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| token_type이 bearer가 아님 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |

</details>

<details><summary><code>auth__apple-signup-withdraw</code> 인증: Apple 가입 후 탈퇴 시 Refresh Token 폐기 호출과 폐기 실패 (같음 1, 동등 2, 다름 4)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| Apple 로그인(교환) | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT<br>BEGIN: INSERT pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT<br>자동커밋: INSERT pending_apple_authorizations | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 업로드 URL 발급 | 다름 | 자동커밋: SELECT social_accounts,users<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT | 자동커밋: SELECT social_accounts,users<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 가입 완료 | 다름 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations; INSERT apple_authorizations; INSERT user_refresh_tokens; DELETE pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations; INSERT apple_authorizations; DELETE pending_apple_authorizations; INSERT user_refresh_tokens → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |
| 폐기가 400이면 탈퇴 실패 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT social_accounts; SELECT apple_authorizations → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT social_accounts<br>자동커밋: SELECT apple_authorizations | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 폐기가 성공하면 탈퇴 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT social_accounts; SELECT apple_authorizations → COMMIT<br>BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations ×2; SELECT social_accounts; SELECT user_refresh_tokens; DELETE apple_authorizations; DELETE social_accounts; SELECT advisory_xact_lock/1; UPDATE user_refresh_tokens; UPDATE users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT social_accounts<br>자동커밋: SELECT apple_authorizations<br>BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations; DELETE apple_authorizations; DELETE social_accounts; SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; UPDATE user_refresh_tokens; UPDATE users → COMMIT | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 개수 다름: Java 9 / Rust 7 (Java만 [SELECT apple_authorizations; SELECT social_accounts], Rust만 []) |
| 탈퇴한 회원의 토큰으로 재탈퇴 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 탈퇴 후 같은 Apple 계정 로그인 | 다름 | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR NO KEY UPDATE OF pending_apple_authorizations → COMMIT<br>BEGIN: INSERT pending_apple_authorizations → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT<br>BEGIN: SELECT pending_apple_authorizations FOR UPDATE OF pending_apple_authorizations → COMMIT<br>자동커밋: INSERT pending_apple_authorizations | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF pending_apple_authorizations / Rust FOR UPDATE OF pending_apple_authorizations |

</details>

<details><summary><code>auth__request-validation</code> 인증: 소셜 로그인·업로드 URL·가입 요청의 형식 검증(제공자 열거형, 필수 값, authorizationCode) (같음 44, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 본문 없음 | 같음 |  |  |  |
| 본문이 JSON이 아님 | 같음 |  |  |  |
| 본문이 배열 | 같음 |  |  |  |
| Content-Type이 JSON이 아님 | 같음 |  |  |  |
| rawNonce가 null | 같음 |  |  |  |
| 제공자 없음 | 같음 |  |  |  |
| 제공자가 null | 같음 |  |  |  |
| 제공자가 빈 문자열 | 같음 |  |  |  |
| 제공자 이름이 소문자 | 같음 |  |  |  |
| 모르는 제공자 | 같음 |  |  |  |
| 제공자 이름 앞뒤 공백은 잘라서 읽는다 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 제공자를 서수(0)로 보냄 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 제공자를 서수 문자열로 보냄 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 범위 밖 서수 | 같음 |  |  |  |
| 음수 서수 | 같음 |  |  |  |
| 제공자가 실수 | 같음 |  |  |  |
| 제공자가 불리언 | 같음 |  |  |  |
| 제공자가 배열 | 같음 |  |  |  |
| 제공자가 객체 | 같음 |  |  |  |
| idToken 없음 | 같음 |  |  |  |
| idToken 공백 | 같음 |  |  |  |
| rawNonce 없음 | 같음 |  |  |  |
| rawNonce 공백 | 같음 |  |  |  |
| idToken이 숫자(문자열로 바뀌어 형식 오류 토큰) | 같음 |  |  |  |
| idToken이 객체 | 같음 |  |  |  |
| 구글에 authorizationCode를 보내면 거절 | 같음 |  |  |  |
| 구글에 빈 authorizationCode도 거절 | 같음 |  |  |  |
| 구글에 null authorizationCode는 통과 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| Apple은 authorizationCode가 필요 | 같음 |  |  |  |
| Apple의 공백 authorizationCode도 거절 | 같음 |  |  |  |
| 알 수 없는 필드는 무시하고 정상 로그인 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 업로드 URL: 제공자 없음 | 같음 |  |  |  |
| 업로드 URL: idToken 공백 | 같음 |  |  |  |
| 업로드 URL: rawNonce 없음 | 같음 |  |  |  |
| 업로드 URL: authorizationCode는 받지 않으므로 무시 | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 업로드 URL: 제공자가 잘못된 서수 | 같음 |  |  |  |
| 가입: 본문 없음 | 같음 |  |  |  |
| 가입: 토큰 없음 | 같음 |  |  |  |
| 가입: 토큰 공백 | 같음 |  |  |  |
| 가입: 형식이 틀린 토큰은 401 | 같음 |  |  |  |
| 가입: 토큰이 객체 | 같음 |  |  |  |
| 가입: 액세스 토큰은 회원가입 토큰이 아니다 | 같음 |  |  |  |
| GET은 허용되지 않음 | 같음 |  |  |  |
| 로그아웃: GET은 허용되지 않음 | 같음 |  |  |  |

</details>

<details><summary><code>auth__social-login-google</code> 인증: 구글 ID Token 검증(서명·발급자·aud·시각·nonce)과 요청 형식 (같음 19, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 정상 토큰은 미가입 회원으로 응답 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 다른 rawNonce | 같음 |  |  |  |
| nonce 클레임이 없는 토큰 | 같음 |  |  |  |
| 만료된 토큰 | 같음 |  |  |  |
| 다른 aud | 같음 |  |  |  |
| aud가 둘 | 같음 |  |  |  |
| 다른 발급자 | 같음 |  |  |  |
| 발급 시각이 미래 | 같음 |  |  |  |
| exp가 없는 토큰 | 같음 |  |  |  |
| sub가 없는 토큰 | 같음 |  |  |  |
| sub가 255자 초과 | 같음 |  |  |  |
| 서명이 깨진 토큰 | 같음 |  |  |  |
| 구글 토큰을 카카오로 제출 | 같음 |  |  |  |
| JWT 형식이 아닌 토큰 | 같음 |  |  |  |
| 구글에 authorizationCode | 같음 |  |  |  |
| 빈 rawNonce | 같음 |  |  |  |
| 빈 idToken | 같음 |  |  |  |
| 알 수 없는 제공자 | 같음 |  |  |  |
| 제공자 누락 | 같음 |  |  |  |

</details>

<details><summary><code>auth__social-signup-email-policy</code> 인증: 제공자별 이메일 채택 규칙(카카오 그대로, 구글·Apple은 email_verified일 때만)과 사인 이미지 정책 (같음 13, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 카카오 업로드 URL | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 카카오 가입 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 구글(이메일 미확인) 업로드 URL | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 구글(이메일 미확인) 가입 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 구글(email_verified 문자열) 업로드 URL | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 구글(email_verified 문자열) 가입 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 큰 이미지 업로드 URL | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 1 MiB를 넘는 사인 이미지 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK |  |
| 정확히 1 MiB인 사인 이미지 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| JPEG 업로드 URL | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 허용되지 않는 Content-Type | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK |  |
| S3가 403이면 없는 것으로 본다 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK |  |
| S3가 500이면 서버 오류 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK |  |

</details>

<details><summary><code>auth__social-signup-flow</code> 인증: 구글 가입 흐름(로그인 → 업로드 URL → 가입 → 재로그인 → 토큰 재발급) 과 S3 HeadObject (같음 10, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 미가입 회원 로그인 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 사인 업로드 URL 발급 | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 이미지가 없으면 가입 불가 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK |  |
| 원본만 있고 썸네일 처리 전 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users → ROLLBACK |  |
| 썸네일까지 있으면 가입 완료 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 같은 가입 토큰 재전송 | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 가입한 회원 로그인 | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 이미 가입한 계정의 업로드 URL 발급 | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 리프레시 토큰으로 재발급 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 회전 전 리프레시 토큰 재사용 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT |  |

</details>

<details><summary><code>auth__token-refresh</code> 인증: 리프레시 토큰 회전·재사용 탐지·로그아웃·비활동·절대 만료 (같음 34, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 업로드 URL 발급 | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 가입(첫 기기, 계보 1) | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 두 번째 기기 로그인(계보 2) | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 세 번째 기기 로그인(계보 3) | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 네 번째 기기 로그인(계보 4) | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 다섯 번째 기기 로그인(계보 5) | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 재발급: 본문 없음 | 같음 |  |  |  |
| 재발급: 토큰 필드 없음 | 같음 |  |  |  |
| 재발급: 토큰이 null | 같음 |  |  |  |
| 재발급: 토큰이 공백 | 같음 |  |  |  |
| 재발급: 토큰이 배열 | 같음 |  |  |  |
| 재발급: 토큰이 숫자(문자열로 바뀌어 모르는 토큰) | 같음 | BEGIN: SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens → COMMIT |  |
| 재발급: 모르는 토큰 | 같음 | BEGIN: SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens → COMMIT |  |
| 재발급: 앞뒤 공백이 붙은 토큰은 다른 토큰 | 같음 | BEGIN: SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens → COMMIT |  |
| 재발급: 알 수 없는 필드는 무시 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 로그아웃: 본문 없음 | 같음 |  |  |  |
| 로그아웃: 토큰 공백 | 같음 |  |  |  |
| 정상 재발급(계보 1: r1 → r1b) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 회전된 r1을 재사용하면 계보 전체 폐기 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT |  |
| 폐기된 계보의 최신 토큰도 쓸 수 없다 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 다른 계보(2)는 영향받지 않는다 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 로그아웃(계보 3) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT |  |
| 로그아웃한 토큰으로 재발급 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 이미 폐기된 토큰의 로그아웃도 204 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 모르는 토큰의 로그아웃도 204 | 같음 | BEGIN: SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens → COMMIT |  |
| 회전된 옛 토큰으로 로그아웃하면 계보가 폐기된다(계보 2) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT |  |
| 계보 2의 최신 토큰도 폐기됨 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 29일을 보낸다 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 29일째 재발급(계보 5: r5 → r5b, 만료는 59일째) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 30일이 지나면 만료(계보 4) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT |  |
| 만료로 폐기된 계보는 이후에도 쓸 수 없다 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 58일째 재발급(계보 5: r5b → r5c, 만료는 88일째) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 87일째 재발급(만료는 절대 만료인 90일째로 줄어든다) | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; INSERT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; INSERT user_refresh_tokens → COMMIT |  |
| 91일째는 절대 만료 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens; UPDATE user_refresh_tokens → COMMIT |  |

</details>

<details><summary><code>feedback__submit</code> 피드백: 제출 검증과 저장 (같음 18, 동등 20, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 앞뒤 공백을 떼어 저장 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 정확히 1000자 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 1001자는 400 | 같음 |  |  |  |
| 공백 포함 1000자 + 앞뒤 공백은 201 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 공백 포함 1001자 + 앞뒤 공백은 400 | 같음 |  |  |  |
| 보조 평면 문자 1000자 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 보조 평면 문자 1001자는 400 | 같음 |  |  |  |
| 공백 사이는 센다 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 공백 사이 포함 1001자는 400 | 같음 |  |  |  |
| 개행·탭만이면 필요합니다 | 같음 |  |  |  |
| 전각 공백만이면 필요합니다 | 같음 |  |  |  |
| 줄바꿈 문자(U+2028)만 | 같음 |  |  |  |
| NBSP만(자바 공백이 아님) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| NBSP로 감싼 내용 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| FS 제어문자로 감싼 내용(자바 공백) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 내부 줄바꿈 유지 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| null 내용 | 같음 |  |  |  |
| 숫자 내용 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 소수 내용 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 지수 내용 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 불리언 내용 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 배열 내용 | 같음 |  |  |  |
| 객체 내용 | 같음 |  |  |  |
| NUL 문자는 저장 오류 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users (오류) |  |
| 외톨이 서로게이트 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 이스케이프된 유니코드 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 필드 이름 대소문자 구분 | 같음 |  |  |  |
| 반복 필드는 마지막 값 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 정지 회원도 제출할 수 있다 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 탈퇴 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 탈퇴 회원은 본문 오류가 먼저(400) | 같음 |  |  |  |
| 없는 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원은 본문 오류가 먼저(400) | 같음 |  |  |  |
| 토큰 없이 401 | 같음 |  |  |  |
| 관리자 토큰은 403 | 같음 |  |  |  |
| charset이 붙은 JSON 타입 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| application/problem+json 타입 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| BOM이 붙은 본문 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |

</details>

<details><summary><code>internal__callback-auth</code> 이미지 처리 콜백: 서명·시각·경로 검증 (같음 22, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| upload-urls: 올바른 서명 | 같음 |  |  |  |
| upload-urls: 서명 없음 | 같음 |  |  |  |
| upload-urls: 서명 헤더만 없음 | 같음 |  |  |  |
| upload-urls: 타임스탬프 헤더만 없음 | 같음 |  |  |  |
| upload-urls: 서명이 틀림 | 같음 |  |  |  |
| upload-urls: 접두사 없는 서명 | 같음 |  |  |  |
| upload-urls: 비밀키가 다름 | 같음 |  |  |  |
| upload-urls: audience가 다름 | 같음 |  |  |  |
| upload-urls: 300초 전은 허용 | 같음 |  |  |  |
| upload-urls: 301초 전은 거절 | 같음 |  |  |  |
| upload-urls: 30초 뒤는 허용 | 같음 |  |  |  |
| upload-urls: 31초 뒤는 거절 | 같음 |  |  |  |
| upload-urls: 업로드 ID 형식 오류(서명보다 먼저) | 같음 |  |  |  |
| upload-urls: 대문자 업로드 ID | 같음 |  |  |  |
| upload-urls: 없는 업로드도 URL을 발급한다 | 같음 |  |  |  |
| upload-urls: 본문이 있어도 서명은 본문 없이 계산 | 같음 |  |  |  |
| upload-urls: 본문이 있는데 본문 없는 서명 | 같음 |  |  |  |
| complete: 본문 변조 | 같음 |  |  |  |
| complete: 다른 경로의 서명 | 같음 |  |  |  |
| GET은 허용되지 않는다 | 같음 |  |  |  |
| PUT은 허용되지 않는다 | 같음 |  |  |  |
| 알 수 없는 결과 경로 | 같음 |  |  |  |

</details>

<details><summary><code>internal__callback-body</code> 이미지 처리 콜백: 본문 읽기와 검증 (같음 37, 동등 0, 다름 23)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| complete: 본문 없음 | 같음 |  |  |  |
| complete: 빈 본문 | 같음 |  |  |  |
| complete: 깨진 JSON | 같음 |  |  |  |
| complete: null | 같음 |  |  |  |
| complete: 배열 | 같음 |  |  |  |
| complete: 문자열 | 같음 |  |  |  |
| complete: 숫자 | 같음 |  |  |  |
| complete: 빈 객체 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 알 수 없는 필드는 무시 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 뒤에 남은 값은 무시 | 같음 |  |  |  |
| complete: 뒤에 쓰레기 | 같음 |  |  |  |
| complete: BOM 접두 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: width 0 | 같음 |  |  |  |
| complete: width 음수 | 같음 |  |  |  |
| complete: height 0 | 같음 |  |  |  |
| complete: width 문자열 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: width 소수 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: width 지수 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: width 공백 문자열 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: width 빈 문자열 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: width 문자열 아님 | 같음 |  |  |  |
| complete: width 불리언 | 같음 |  |  |  |
| complete: width 객체 | 같음 |  |  |  |
| complete: width int 초과 | 같음 |  |  |  |
| complete: byteSize 음수 | 같음 |  |  |  |
| complete: byteSize 0 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: byteSize 큰 값 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: byteSize long 초과 | 같음 |  |  |  |
| complete: 위도 91 | 같음 |  |  |  |
| complete: 경도 -181 | 같음 |  |  |  |
| complete: 위도만 | 같음 |  |  |  |
| complete: 경도만 | 같음 |  |  |  |
| complete: 빈 위치 | 같음 |  |  |  |
| complete: 위치 null | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 위치 배열 | 같음 |  |  |  |
| complete: 위도 문자열 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 위도 NaN 문자열 | 같음 |  |  |  |
| complete: 위도 경계 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 정밀한 좌표 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: capturedAt 64자 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: capturedAt 65자 | 같음 |  |  |  |
| complete: capturedAt 숫자 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: capturedAt 배열 | 같음 |  |  |  |
| complete: metaAttributes 중첩 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: metaAttributes 200개 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: metaAttributes 201개 | 같음 |  |  |  |
| complete: metaAttributes 배열 | 같음 |  |  |  |
| complete: metaAttributes 부동소수 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: metaAttributes 유니코드 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 본문 없음 | 같음 |  |  |  |
| failed: 빈 객체 | 같음 |  |  |  |
| failed: null | 같음 |  |  |  |
| failed: reason null | 같음 |  |  |  |
| failed: 공백 | 같음 |  |  |  |
| failed: 빈 문자열 | 같음 |  |  |  |
| failed: 50자 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 51자 | 같음 |  |  |  |
| failed: 숫자 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 배열 | 같음 |  |  |  |
| failed: 한글 50자 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |

</details>

<details><summary><code>internal__callback-size</code> 이미지 처리 콜백: 본문 크기 제한 (같음 5, 동등 0, 다름 1)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 서명 없는 큰 본문은 인증 전에 막는다 | 같음 |  |  |  |
| 서명이 있어도 큰 본문은 막는다 | 같음 |  |  |  |
| 상한과 같은 크기는 통과한다 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 상한을 1바이트 넘기면 막는다 | 같음 |  |  |  |
| 큰 본문을 읽지 않는 경로도 선언 길이로 막는다 | 같음 |  |  |  |
| 내부 경로가 아니면 상한이 없다 | 같음 |  |  |  |

</details>

<details><summary><code>internal__callback-state</code> 이미지 처리 콜백: 상태 전이 (같음 4, 동등 0, 다름 9)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| complete: 검증 중 게시물을 검수 대기로 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads; UPDATE photos; UPDATE posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE photos; UPDATE posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 다시 보내도 같다 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 다른 값으로 다시 보내도 처음 값 유지 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 게시물 없는 업로드 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| complete: 이미 반려된 업로드 | 같음 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT |  |
| complete: 없는 업로드는 조용히 끝 | 같음 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT |  |
| complete: 이미 승인 대기인 게시물 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 검증 중 게시물을 반려 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads; UPDATE posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 다시 보내도 같다 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 이미 처리 완료된 업로드 | 같음 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT |  |
| failed: 게시물 없는 업로드 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 이미 반려된 업로드 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| failed: 없는 업로드 | 같음 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → COMMIT |  |

</details>

<details><summary><code>like__like-unlike</code> 좋아요: 등록·취소와 오류 (같음 9, 동등 1, 다름 18)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 좋아요: 인증 없음 | 같음 |  |  |  |
| 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 좋아요 반복(멱등) | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 다른 회원도 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 내 게시물에 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 좋아요 반영 확인 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users; SELECT post_likes ×2 → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users<br>자동커밋: SELECT post_likes<br>자동커밋: SELECT post_likes | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 4개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 좋아요 취소 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; DELETE post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; DELETE post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 좋아요 취소 반복(멱등) | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; DELETE post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; DELETE post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 안 눌렀던 게시물 취소 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; DELETE post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; DELETE post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 취소: 인증 없음 | 같음 |  |  |  |
| 없는 게시물 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 없는 게시물 취소 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 검수 대기 게시물 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 작성자도 검수 대기 게시물은 좋아요 못 한다 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 반려 게시물 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 삭제된 게시물 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 삭제된 게시물 취소 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 검수 대기 게시물 취소 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 잘못된 게시물 ID 좋아요 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 잘못된 게시물 ID 취소 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 차단된 회원 좋아요 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 차단된 회원 취소 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; DELETE post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; DELETE post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 없는 회원 좋아요 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원 취소 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 차단된 작성자의 게시물에 좋아요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| 본문이 있어도 된다 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF posts; UPSERT post_likes; SELECT post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts,topics,users FOR SHARE OF photos,posts; UPSERT post_likes; SELECT post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR SHARE OF posts / Rust FOR SHARE OF photos,posts |
| POST는 허용되지 않는다 | 같음 |  |  |  |
| 인기순 목록에 반영 | 같음 |  |  |  |

</details>

<details><summary><code>notification__detail-and-read</code> 알림함: 상세·미읽음 여부·읽음 처리 (같음 28, 동등 4, 다름 3)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 미읽음 여부(있음) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 승인 알림 상세(읽은 알림) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 반려 알림 상세(사유 포함, 마이크로초 시각) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 썸네일 없는 사진의 알림 상세 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 대문자 식별자도 받는다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 삭제된 게시물의 알림 상세는 404 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 남의 알림 상세는 404 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 없는 알림 상세는 404 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 상세 조회로 읽음 처리되지 않는다(미읽음 여부) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 형식이 잘못된 식별자 abc | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 1-2-3-4-5 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 0199000000007000800000000000102 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 01990000-0000-7000-8000-00000000010g | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 %2001990000-0000-7000-8000-000000000102 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 01990000-0000-7000-8000-000000000102%20 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 %7B01990000-0000-7000-8000-000000000102%7D | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 01990000-0000-7000-8000-0000000001022 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 %2B1-2-3-4-5 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 1-2-3-4-5-6 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 형식이 잘못된 식별자 0198f6c1-62ba-7d30-8b12-0f733b6572f2a | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 식별자 형식 오류보다 회원 확인이 먼저(없는 회원) | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 식별자 형식 오류보다 회원 확인이 먼저(탈퇴 회원) | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 읽음: 이미 읽은 알림은 시각을 유지 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 읽음: 안 읽은 알림 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 읽음: 반복해도 처음 시각을 유지 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 읽음: 남의 알림은 404 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 한쪽만 ROLLBACK(Java만) |
| 읽음: 삭제된 게시물의 알림은 404 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 한쪽만 ROLLBACK(Java만) |
| 읽음: 없는 알림은 404 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 한쪽만 ROLLBACK(Java만) |
| 읽음: 형식 오류 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 읽음: 대문자 식별자 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 읽음 뒤 목록 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 읽음 뒤 미읽음 여부(없음) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 다른 회원의 미읽음 여부 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 읽음: 탈퇴 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 읽음: 토큰 없이 401 | 같음 |  |  |  |

</details>

<details><summary><code>notification__inbox-list</code> 알림함: 목록 조회와 페이지 나누기 (같음 11, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 기본 목록(최신순, 삭제된 게시물·남의 알림 제외) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 페이지 크기 1, 1쪽 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 페이지 크기 1, 2쪽 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 페이지 크기 1, 3쪽 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 페이지 크기 1, 4쪽 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 페이지 크기 2, 2쪽 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 다른 회원의 목록 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 탈퇴한 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 토큰 없이 401 | 같음 |  |  |  |
| 관리자 토큰은 403 | 같음 |  |  |  |

</details>

<details><summary><code>notification__inbox-query</code> 알림함: 목록 조회 조건 검증 (같음 70, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 쿼리 page=0 | 같음 |  |  |  |
| 쿼리 page=-1 | 같음 |  |  |  |
| 쿼리 pageSize=0 | 같음 |  |  |  |
| 쿼리 pageSize=-5 | 같음 |  |  |  |
| 쿼리 pageSize=101 | 같음 |  |  |  |
| 쿼리 pageSize=100 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 pageSize=1 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=abc | 같음 |  |  |  |
| 쿼리 pageSize=abc | 같음 |  |  |  |
| 쿼리 page=1.5 | 같음 |  |  |  |
| 쿼리 pageSize=1.5 | 같음 |  |  |  |
| 쿼리 page= | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 pageSize= | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=&pageSize= | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%20 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%201%202 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=1&page=2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=2&page=x | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=x&page=2 | 같음 |  |  |  |
| 쿼리 page=&page=2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=0x1 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=0X1 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%230 | 같음 |  |  |  |
| 쿼리 page=%2312 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=0x10&pageSize=0x5 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%2B2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%2B | 같음 |  |  |  |
| 쿼리 page=-0 | 같음 |  |  |  |
| 쿼리 page=007 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=1e1 | 같음 |  |  |  |
| 쿼리 page=2147483647 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 쿼리 page=2147483648 | 같음 |  |  |  |
| 쿼리 page=-2147483648 | 같음 |  |  |  |
| 쿼리 page=2147483647&pageSize=1 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=1073741825&pageSize=2 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 쿼리 page=107374183&pageSize=20 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=0&pageSize=0 | 같음 |  |  |  |
| 쿼리 page=abc&pageSize=0 | 같음 |  |  |  |
| 쿼리 page=0&pageSize=abc | 같음 |  |  |  |
| 쿼리 page=abc&pageSize=abc | 같음 |  |  |  |
| 쿼리 page=1&pageSize=101&x=1 | 같음 |  |  |  |
| 쿼리 Page=2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 PAGE=2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 pageSize=%EF%BC%92 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%D9%A2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%C2%A02 | 같음 |  |  |  |
| 쿼리 page=%E3%80%802 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=1,2 | 같음 |  |  |  |
| 쿼리 page=true | 같음 |  |  |  |
| 쿼리 page=NaN | 같음 |  |  |  |
| 쿼리 page=0x | 같음 |  |  |  |
| 쿼리 page=0xFFFFFFFF | 같음 |  |  |  |
| 쿼리 page=-0x1 | 같음 |  |  |  |
| 쿼리 page=%C2%A0 | 같음 |  |  |  |
| 쿼리 page=%E3%80%80 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%09 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 pageSize=%0A | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%202 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=2%20 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=+3 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%2B%202 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%E2%80%A82 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%0B2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%1C2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%EF%BC%B0 | 같음 |  |  |  |
| 쿼리 page=0x%EF%BC%A6 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 쿼리 page=%F0%9D%9F%A4 | 같음 |  |  |  |
| 검증 오류가 회원 확인보다 먼저(없는 회원, pageSize=0) | 같음 |  |  |  |
| 형식 오류가 회원 확인보다 먼저(없는 회원, page=abc) | 같음 |  |  |  |
| 탈퇴 회원, pageSize=101 | 같음 |  |  |  |

</details>

<details><summary><code>notification__read-all</code> 알림함: 모두 읽기 (같음 7, 동등 3, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 모두 읽기 전 미읽음 여부 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 모두 읽기(남의 알림·삭제된 게시물의 알림은 그대로) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 모두 읽기 뒤 미읽음 여부 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 모두 읽기 뒤 목록(읽은 시각) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 다시 모두 읽기는 시각을 바꾸지 않는다 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 목록 재확인 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 다른 회원의 모두 읽기 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: UPDATE notifications → COMMIT | 자동커밋: SELECT users<br>자동커밋: UPDATE notifications | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 탈퇴 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 토큰 없이 401 | 같음 |  |  |  |

</details>

<details><summary><code>notification__route-matrix</code> 알림함: 경로와 메서드 매트릭스 (같음 33, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| POST /api/v1/notifications | 같음 |  |  |  |
| PUT /api/v1/notifications | 같음 |  |  |  |
| DELETE /api/v1/notifications | 같음 |  |  |  |
| PATCH /api/v1/notifications | 같음 |  |  |  |
| POST /api/v1/notifications/unread-status | 같음 |  |  |  |
| PATCH /api/v1/notifications/unread-status | 같음 |  |  |  |
| DELETE /api/v1/notifications/unread-status | 같음 |  |  |  |
| POST /api/v1/notifications/01990000-0000-7000-8000-000000000102 | 같음 |  |  |  |
| PATCH /api/v1/notifications/01990000-0000-7000-8000-000000000102 | 같음 |  |  |  |
| DELETE /api/v1/notifications/01990000-0000-7000-8000-000000000102 | 같음 |  |  |  |
| GET /api/v1/notifications/01990000-0000-7000-8000-000000000102/read | 같음 |  |  |  |
| POST /api/v1/notifications/01990000-0000-7000-8000-000000000102/read | 같음 |  |  |  |
| PUT /api/v1/notifications/01990000-0000-7000-8000-000000000102/read | 같음 |  |  |  |
| DELETE /api/v1/notifications/01990000-0000-7000-8000-000000000102/read | 같음 |  |  |  |
| GET /api/v1/notifications/read-all | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| POST /api/v1/notifications/read-all | 같음 |  |  |  |
| PUT /api/v1/notifications/read-all | 같음 |  |  |  |
| GET /api/v1/notifications/ | 같음 |  |  |  |
| PATCH /api/v1/notifications/read-all/ | 같음 |  |  |  |
| GET /api/v1/notifications/unread-status/ | 같음 |  |  |  |
| GET /api/v1/notifications/01990000-0000-7000-8000-000000000102/ | 같음 |  |  |  |
| GET /api/v1/notifications/unread-status/x | 같음 |  |  |  |
| GET /api/v1/Notifications | 같음 |  |  |  |
| GET /api/v1/notifications/READ-ALL | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| PATCH /api/v1/notifications/READ-ALL | 같음 |  |  |  |
| OPTIONS /api/v1/notifications | 같음 |  |  |  |
| OPTIONS /api/v1/notifications/read-all | 같음 |  |  |  |
| OPTIONS /api/v1/notifications/01990000-0000-7000-8000-000000000102/read | 같음 |  |  |  |
| HEAD /api/v1/notifications/unread-status | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| PATCH /api/v1/notifications/read | 같음 |  |  |  |
| PATCH /api/v1/notifications/01990000-0000-7000-8000-000000000102/read/x | 같음 |  |  |  |
| 메서드 오류는 인가보다 뒤: 토큰 없이 POST | 같음 |  |  |  |
| 메서드 오류, 관리자 토큰 PATCH | 같음 |  |  |  |

</details>

<details><summary><code>platform__accept-language</code> Whitelabel 오류 페이지의 Content-Language (같음 14, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| Accept-Language ko | 같음 |  |  |  |
| Accept-Language ko-KR | 같음 |  |  |  |
| Accept-Language en-GB | 같음 |  |  |  |
| Accept-Language fr, en;q=0.5 | 같음 |  |  |  |
| Accept-Language * | 같음 |  |  |  |
| Accept-Language xx-YY | 같음 |  |  |  |
| Accept-Language bad lang!! | 같음 |  |  |  |
| Accept-Language ko;q=0 | 같음 |  |  |  |
| Accept-Language zh-Hant-TW | 같음 |  |  |  |
| Accept-Language en_US | 같음 |  |  |  |
| Accept-Language en;q=0.3, ko;q=0.9 | 같음 |  |  |  |
| Accept-Language EN-us | 같음 |  |  |  |
| Accept-Language ko-kr | 같음 |  |  |  |
| Accept-Language de-DE-1996 | 같음 |  |  |  |

</details>

<details><summary><code>platform__accept-negotiation</code> Accept 협상: 응답 종류와 Accept 조합 (같음 149, 동등 14, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 성공 200 / Accept text/html | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept text/* | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/xml | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept bad | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept text/html, application/json;q=0.5 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/json;charset=UTF-8 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/vnd.x+json | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/*+json;charset=UTF-8 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept */*;q=0 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/json;q=2 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept text/html;q=0, application/xml | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/problem+json, text/html | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/yaml | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 성공 200 / Accept application/yaml, application/json;q=0.5 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| Spring 예외 400 / Accept text/html | 같음 |  |  |  |
| Spring 예외 400 / Accept text/* | 같음 |  |  |  |
| Spring 예외 400 / Accept application/xml | 같음 |  |  |  |
| Spring 예외 400 / Accept bad | 같음 |  |  |  |
| Spring 예외 400 / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| Spring 예외 400 / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| Spring 예외 400 / Accept application/vnd.x+json | 같음 |  |  |  |
| Spring 예외 400 / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| Spring 예외 400 / Accept */*;q=0 | 같음 |  |  |  |
| Spring 예외 400 / Accept application/json;q=2 | 같음 |  |  |  |
| Spring 예외 400 / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| Spring 예외 400 / Accept application/problem+json, text/html | 같음 |  |  |  |
| Spring 예외 400 / Accept application/yaml | 같음 |  |  |  |
| Spring 예외 400 / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| 도메인 예외 404 / Accept text/html | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept text/* | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/xml | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept bad | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept text/html, application/json;q=0.5 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/json;charset=UTF-8 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/vnd.x+json | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/*+json;charset=UTF-8 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept */*;q=0 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/json;q=2 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept text/html;q=0, application/xml | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/problem+json, text/html | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/yaml | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 도메인 예외 404 / Accept application/yaml, application/json;q=0.5 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 경로 없음 404 / Accept text/html | 같음 |  |  |  |
| 경로 없음 404 / Accept text/* | 같음 |  |  |  |
| 경로 없음 404 / Accept application/xml | 같음 |  |  |  |
| 경로 없음 404 / Accept bad | 같음 |  |  |  |
| 경로 없음 404 / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 경로 없음 404 / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| 경로 없음 404 / Accept application/vnd.x+json | 같음 |  |  |  |
| 경로 없음 404 / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| 경로 없음 404 / Accept */*;q=0 | 같음 |  |  |  |
| 경로 없음 404 / Accept application/json;q=2 | 같음 |  |  |  |
| 경로 없음 404 / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| 경로 없음 404 / Accept application/problem+json, text/html | 같음 |  |  |  |
| 경로 없음 404 / Accept application/yaml | 같음 |  |  |  |
| 경로 없음 404 / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| 메서드 없음 405 / Accept text/html | 같음 |  |  |  |
| 메서드 없음 405 / Accept text/* | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/xml | 같음 |  |  |  |
| 메서드 없음 405 / Accept bad | 같음 |  |  |  |
| 메서드 없음 405 / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/vnd.x+json | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| 메서드 없음 405 / Accept */*;q=0 | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/json;q=2 | 같음 |  |  |  |
| 메서드 없음 405 / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/problem+json, text/html | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/yaml | 같음 |  |  |  |
| 메서드 없음 405 / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| 방화벽 400 / Accept text/html | 같음 |  |  |  |
| 방화벽 400 / Accept text/* | 같음 |  |  |  |
| 방화벽 400 / Accept application/xml | 같음 |  |  |  |
| 방화벽 400 / Accept bad | 같음 |  |  |  |
| 방화벽 400 / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 방화벽 400 / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| 방화벽 400 / Accept application/vnd.x+json | 같음 |  |  |  |
| 방화벽 400 / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| 방화벽 400 / Accept */*;q=0 | 같음 |  |  |  |
| 방화벽 400 / Accept application/json;q=2 | 같음 |  |  |  |
| 방화벽 400 / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| 방화벽 400 / Accept application/problem+json, text/html | 같음 |  |  |  |
| 방화벽 400 / Accept application/yaml | 같음 |  |  |  |
| 방화벽 400 / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| TRACE / Accept text/html | 같음 |  |  |  |
| TRACE / Accept text/* | 같음 |  |  |  |
| TRACE / Accept application/xml | 같음 |  |  |  |
| TRACE / Accept bad | 같음 |  |  |  |
| TRACE / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| TRACE / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| TRACE / Accept application/vnd.x+json | 같음 |  |  |  |
| TRACE / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| TRACE / Accept */*;q=0 | 같음 |  |  |  |
| TRACE / Accept application/json;q=2 | 같음 |  |  |  |
| TRACE / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| TRACE / Accept application/problem+json, text/html | 같음 |  |  |  |
| TRACE / Accept application/yaml | 같음 |  |  |  |
| TRACE / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| 깨진 JSON 400 / Accept text/html | 같음 |  |  |  |
| 깨진 JSON 400 / Accept text/* | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/xml | 같음 |  |  |  |
| 깨진 JSON 400 / Accept bad | 같음 |  |  |  |
| 깨진 JSON 400 / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/vnd.x+json | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| 깨진 JSON 400 / Accept */*;q=0 | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/json;q=2 | 같음 |  |  |  |
| 깨진 JSON 400 / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/problem+json, text/html | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/yaml | 같음 |  |  |  |
| 깨진 JSON 400 / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| 검증 400 / Accept text/html | 같음 |  |  |  |
| 검증 400 / Accept text/* | 같음 |  |  |  |
| 검증 400 / Accept application/xml | 같음 |  |  |  |
| 검증 400 / Accept bad | 같음 |  |  |  |
| 검증 400 / Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 검증 400 / Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| 검증 400 / Accept application/vnd.x+json | 같음 |  |  |  |
| 검증 400 / Accept application/*+json;charset=UTF-8 | 같음 |  |  |  |
| 검증 400 / Accept */*;q=0 | 같음 |  |  |  |
| 검증 400 / Accept application/json;q=2 | 같음 |  |  |  |
| 검증 400 / Accept text/html;q=0, application/xml | 같음 |  |  |  |
| 검증 400 / Accept application/problem+json, text/html | 같음 |  |  |  |
| 검증 400 / Accept application/yaml | 같음 |  |  |  |
| 검증 400 / Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| 생성 201 / Accept text/html | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept text/* | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/xml | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept bad | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept text/html, application/json;q=0.5 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/json;charset=UTF-8 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/vnd.x+json | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/*+json;charset=UTF-8 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept */*;q=0 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/json;q=2 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept text/html;q=0, application/xml | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/problem+json, text/html | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/yaml | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 생성 201 / Accept application/yaml, application/json;q=0.5 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| Accept 없음 / 성공 200 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| Accept 빈 값 / 성공 200 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,photos,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,photos,posts |  |
| 인증 없음 401 / Accept text/html | 같음 |  |  |  |
| 토큰 없음 401 / Accept application/xml | 같음 |  |  |  |
| 관리자 경로 회원 토큰 403 / Accept text/html | 같음 |  |  |  |
| Content-Type text/plain 415 / Accept text/html | 같음 |  |  |  |
| Content-Type text/plain 415 / Accept application/xml | 같음 |  |  |  |
| Content-Type text/plain 415 / Accept 없음 | 같음 |  |  |  |
| actuator health / Accept 없음 | 같음 |  |  |  |
| actuator health / Accept application/json | 같음 |  |  |  |
| actuator health / Accept application/* | 같음 |  |  |  |
| actuator health / Accept application/vnd.spring-boot.actuator.v2+json | 같음 |  |  |  |
| actuator health / Accept application/*+json | 같음 |  |  |  |
| actuator health / Accept text/html | 같음 |  |  |  |
| actuator health / Accept application/xml | 같음 |  |  |  |
| actuator health / Accept bad | 같음 |  |  |  |
| actuator health / Accept application/vnd.x+json | 같음 |  |  |  |
| actuator health / Accept text/html,application/json;q=0.5 | 같음 |  |  |  |
| /error / Accept 없음 | 같음 |  |  |  |
| /error / Accept application/json | 같음 |  |  |  |
| /error / Accept text/html | 같음 |  |  |  |
| /error / Accept application/xml | 같음 |  |  |  |
| /error / Accept bad | 같음 |  |  |  |

</details>

<details><summary><code>platform__allow-order</code> 405 Allow 헤더의 메서드 순서(Accept: text/html) (같음 42, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| PATCH /api/v1/admin/audit-logs (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/admin/auth/login (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/admin/auth/logout (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/admin/auth/me (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/admin/auth/refresh (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/admin/feedbacks (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/admin/posts (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/admin/posts/{postId} (등록: DELETE,GET) | 같음 |  |  |  |
| PATCH /api/v1/admin/posts/{postId}/moderation (등록: PUT) | 같음 |  |  |  |
| PATCH /api/v1/admin/topics (등록: GET,POST) | 같음 |  |  |  |
| PATCH /api/v1/admin/topics/{topicId} (등록: DELETE,GET,PUT) | 같음 |  |  |  |
| PATCH /api/v1/admin/users (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/admin/users/{userId} (등록: GET) | 같음 |  |  |  |
| PUT /api/v1/admin/users/{userId}/status (등록: PATCH) | 같음 |  |  |  |
| PATCH /api/v1/auth/logout (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/auth/refresh (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/auth/social-login (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/auth/social-signup (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/auth/social-signup/signature/uploads (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/feedbacks (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/notifications (등록: GET) | 같음 |  |  |  |
| PUT /api/v1/notifications/read-all (등록: PATCH) | 같음 |  |  |  |
| PATCH /api/v1/notifications/unread-status (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/notifications/{notificationId} (등록: GET) | 같음 |  |  |  |
| PUT /api/v1/notifications/{notificationId}/read (등록: PATCH) | 같음 |  |  |  |
| PATCH /api/v1/posts (등록: GET,POST) | 같음 |  |  |  |
| PATCH /api/v1/posts/calendar (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/posts/calendar/months (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/posts/today (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/posts/uploads (등록: POST) | 같음 |  |  |  |
| PATCH /api/v1/posts/{postId} (등록: DELETE,GET,PUT) | 같음 |  |  |  |
| PATCH /api/v1/posts/{postId}/likes (등록: DELETE,PUT) | 같음 |  |  |  |
| PATCH /api/v1/topics (등록: GET) | 같음 |  |  |  |
| PATCH /api/v1/users/me (등록: DELETE) | 같음 |  |  |  |
| PATCH /api/v1/users/me/signature (등록: GET,PUT) | 같음 |  |  |  |
| PATCH /api/v1/users/me/signature/uploads (등록: POST) | 같음 |  |  |  |
| PATCH /internal/v1/post-image-processing/{uploadId}/complete (등록: POST) | 같음 |  |  |  |
| PATCH /internal/v1/post-image-processing/{uploadId}/failed (등록: POST) | 같음 |  |  |  |
| PATCH /internal/v1/post-image-processing/{uploadId}/upload-urls (등록: POST) | 같음 |  |  |  |
| PATCH /internal/v1/signature-processing/{uploadId}/complete (등록: POST) | 같음 |  |  |  |
| PATCH /internal/v1/signature-processing/{uploadId}/failed (등록: POST) | 같음 |  |  |  |
| PATCH /internal/v1/signature-processing/{uploadId}/upload-urls (등록: POST) | 같음 |  |  |  |

</details>

<details><summary><code>platform__auth-matrix</code> 인증 매트릭스: 공개 GET, 회원 전용, 관리자 전용 (같음 13, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 공개 GET 익명 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 공개 GET 회원 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 공개 GET 관리자 | 같음 |  |  |  |
| 공개 GET 만료 토큰 | 같음 |  |  |  |
| 공개 GET 만료 10초 지남(원본 허용 오차 30초, iat보다 exp가 앞섬) | 같음 |  |  |  |
| 공개 GET 형식 오류 토큰 | 같음 |  |  |  |
| 공개 GET Bearer 아닌 스킴 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 공개 GET 서명 위조 | 같음 |  |  |  |
| 회원 전용 익명 | 같음 |  |  |  |
| 회원 전용 관리자 토큰 | 같음 |  |  |  |
| 관리자 전용 익명 | 같음 |  |  |  |
| 관리자 전용 회원 토큰 | 같음 |  |  |  |
| 관리자 공개 경로(로그아웃) 익명 | 같음 |  |  |  |

</details>

<details><summary><code>platform__cors</code> 관리자 웹 CORS (같음 4, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 허용 Origin preflight | 같음 |  |  |  |
| 허용하지 않는 Origin preflight | 같음 |  |  |  |
| 허용 Origin 실제 요청(인증 실패) | 같음 |  |  |  |
| 회원 API에 Origin | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |

</details>

<details><summary><code>platform__health</code> 헬스체크와 info (같음 2, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| health | 같음 |  |  |  |
| info | 같음 |  |  |  |

</details>

<details><summary><code>platform__method-not-allowed</code> 지원하지 않는 메서드 (같음 4, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 토큰 없이 DELETE 공개 경로 | 같음 |  |  |  |
| 회원 토큰으로 DELETE 공개 경로 | 같음 |  |  |  |
| HEAD 공개 경로 | 같음 |  |  |  |
| OPTIONS 공개 경로(Origin 없음) | 같음 |  |  |  |

</details>

<details><summary><code>platform__request-body-errors</code> 요청 본문 오류: 잘못된 JSON, 빈 본문, 검증 실패, 잘못된 Content-Type (같음 5, 동등 2, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 깨진 JSON | 같음 |  |  |  |
| 빈 본문 | 같음 |  |  |  |
| 필수 필드 누락 | 같음 |  |  |  |
| 공백 내용, 영어 Accept-Language | 같음 |  |  |  |
| 타입이 다른 필드 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 알 수 없는 필드는 무시 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| text/plain Content-Type | 같음 |  |  |  |

</details>

<details><summary><code>platform__springdoc</code> springdoc: OpenAPI 문서와 Swagger UI(non-prod) (같음 207, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| GET /v3/api-docs | 같음 |  |  |  |
| GET /v3/api-docs 잘못된 토큰 | 같음 |  |  |  |
| GET /v3/api-docs.yaml | 같음 |  |  |  |
| GET /v3/api-docs.yaml 잘못된 토큰 | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config 잘못된 토큰 | 같음 |  |  |  |
| GET /v3/api-docs/user-api | 같음 |  |  |  |
| GET /v3/api-docs/user-api 잘못된 토큰 | 같음 |  |  |  |
| GET /v3/api-docs/admin-api | 같음 |  |  |  |
| GET /v3/api-docs/admin-api 잘못된 토큰 | 같음 |  |  |  |
| GET /v3/api-docs/internal-api | 같음 |  |  |  |
| GET /v3/api-docs/internal-api 잘못된 토큰 | 같음 |  |  |  |
| GET /v3/api-docs Accept text/html | 같음 |  |  |  |
| GET /v3/api-docs Accept application/xml | 같음 |  |  |  |
| GET /v3/api-docs Accept application/json | 같음 |  |  |  |
| GET /v3/api-docs Accept application/yaml | 같음 |  |  |  |
| GET /v3/api-docs Accept bad | 같음 |  |  |  |
| GET /v3/api-docs Accept */* | 같음 |  |  |  |
| GET /v3/api-docs Accept application/vnd.x+json | 같음 |  |  |  |
| GET /v3/api-docs Accept application/*+json | 같음 |  |  |  |
| GET /v3/api-docs Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| GET /v3/api-docs Accept application/* | 같음 |  |  |  |
| GET /v3/api-docs Accept text/* | 같음 |  |  |  |
| GET /v3/api-docs Accept application/json, text/html | 같음 |  |  |  |
| GET /v3/api-docs Accept application/xml, application/json;q=0.1 | 같음 |  |  |  |
| GET /v3/api-docs Accept application/vnd.oai.openapi | 같음 |  |  |  |
| GET /v3/api-docs Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| HEAD /v3/api-docs | 같음 |  |  |  |
| OPTIONS /v3/api-docs | 같음 |  |  |  |
| POST /v3/api-docs | 같음 |  |  |  |
| DELETE /v3/api-docs | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept text/html | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/xml | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/json | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/yaml | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept bad | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept */* | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/vnd.x+json | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/*+json | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/* | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept text/* | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/json, text/html | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/xml, application/json;q=0.1 | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/vnd.oai.openapi | 같음 |  |  |  |
| GET /v3/api-docs.yaml Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| HEAD /v3/api-docs.yaml | 같음 |  |  |  |
| OPTIONS /v3/api-docs.yaml | 같음 |  |  |  |
| POST /v3/api-docs.yaml | 같음 |  |  |  |
| DELETE /v3/api-docs.yaml | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept text/html | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/xml | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/json | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/yaml | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept bad | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept */* | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/vnd.x+json | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/*+json | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/* | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept text/* | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/json, text/html | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/xml, application/json;q=0.1 | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/vnd.oai.openapi | 같음 |  |  |  |
| GET /v3/api-docs/user-api Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| HEAD /v3/api-docs/user-api | 같음 |  |  |  |
| OPTIONS /v3/api-docs/user-api | 같음 |  |  |  |
| POST /v3/api-docs/user-api | 같음 |  |  |  |
| DELETE /v3/api-docs/user-api | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept text/html | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/xml | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/json | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/yaml | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept bad | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept */* | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/vnd.x+json | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/*+json | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/json;charset=UTF-8 | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/* | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept text/* | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/json, text/html | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/xml, application/json;q=0.1 | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/vnd.oai.openapi | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config Accept application/yaml, application/json;q=0.5 | 같음 |  |  |  |
| HEAD /v3/api-docs/swagger-config | 같음 |  |  |  |
| OPTIONS /v3/api-docs/swagger-config | 같음 |  |  |  |
| POST /v3/api-docs/swagger-config | 같음 |  |  |  |
| DELETE /v3/api-docs/swagger-config | 같음 |  |  |  |
| GET /v3/api-docs/nope | 같음 |  |  |  |
| GET /v3/api-docs/nope 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/nope | 같음 |  |  |  |
| GET /v3/api-docs/ | 같음 |  |  |  |
| GET /v3/api-docs/ 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/ | 같음 |  |  |  |
| GET /v3/api-docs/a/b | 같음 |  |  |  |
| GET /v3/api-docs/a/b 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/a/b | 같음 |  |  |  |
| GET /v3/api-docs.yaml/ | 같음 |  |  |  |
| GET /v3/api-docs.yaml/ 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs.yaml/ | 같음 |  |  |  |
| GET /v3/api-docs.yaml/user-api | 같음 |  |  |  |
| POST /v3/api-docs.yaml/user-api | 같음 |  |  |  |
| GET /v3/api-docs/user-api/ | 같음 |  |  |  |
| GET /v3/api-docs/user-api/ 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/user-api/ | 같음 |  |  |  |
| GET /V3/api-docs | 같음 |  |  |  |
| GET /V3/api-docs 인증됨 | 같음 |  |  |  |
| POST /V3/api-docs | 같음 |  |  |  |
| GET /v3/api-docs?x=1 | 같음 |  |  |  |
| GET /v3/api-docs?x=1 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs?x=1 | 같음 |  |  |  |
| GET /v3/api-docs/user-api?x=1 | 같음 |  |  |  |
| GET /v3/api-docs/user-api?x=1 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/user-api?x=1 | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config/ | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config/ 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/swagger-config/ | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config.json | 같음 |  |  |  |
| GET /v3/api-docs/swagger-config.json 인증됨 | 같음 |  |  |  |
| POST /v3/api-docs/swagger-config.json | 같음 |  |  |  |
| GET /swagger-ui.html | 같음 |  |  |  |
| GET /swagger-ui.html?x=1 | 같음 |  |  |  |
| HEAD /swagger-ui.html | 같음 |  |  |  |
| POST /swagger-ui.html | 같음 |  |  |  |
| OPTIONS /swagger-ui.html | 같음 |  |  |  |
| GET /swagger-ui.html/ | 같음 |  |  |  |
| GET /swagger-ui/index.html | 같음 |  |  |  |
| HEAD /swagger-ui/index.html | 같음 |  |  |  |
| GET /swagger-ui/index.css | 같음 |  |  |  |
| HEAD /swagger-ui/index.css | 같음 |  |  |  |
| GET /swagger-ui/swagger-initializer.js | 같음 |  |  |  |
| HEAD /swagger-ui/swagger-initializer.js | 같음 |  |  |  |
| GET /swagger-ui/oauth2-redirect.html | 같음 |  |  |  |
| HEAD /swagger-ui/oauth2-redirect.html | 같음 |  |  |  |
| GET /swagger-ui/swagger-ui.css | 같음 |  |  |  |
| HEAD /swagger-ui/swagger-ui.css | 같음 |  |  |  |
| GET /swagger-ui/swagger-ui-bundle.js | 같음 |  |  |  |
| HEAD /swagger-ui/swagger-ui-bundle.js | 같음 |  |  |  |
| GET /swagger-ui/swagger-ui-standalone-preset.js | 같음 |  |  |  |
| HEAD /swagger-ui/swagger-ui-standalone-preset.js | 같음 |  |  |  |
| GET /swagger-ui/favicon-32x32.png | 같음 |  |  |  |
| HEAD /swagger-ui/favicon-32x32.png | 같음 |  |  |  |
| GET /swagger-ui/favicon-16x16.png | 같음 |  |  |  |
| HEAD /swagger-ui/favicon-16x16.png | 같음 |  |  |  |
| GET /swagger-ui/ | 같음 |  |  |  |
| GET /swagger-ui/ Accept html | 같음 |  |  |  |
| POST /swagger-ui/ | 같음 |  |  |  |
| GET /swagger-ui | 같음 |  |  |  |
| GET /swagger-ui Accept html | 같음 |  |  |  |
| POST /swagger-ui | 같음 |  |  |  |
| GET /swagger-ui/nope.js | 같음 |  |  |  |
| GET /swagger-ui/nope.js Accept html | 같음 |  |  |  |
| POST /swagger-ui/nope.js | 같음 |  |  |  |
| GET /swagger-ui/nope | 같음 |  |  |  |
| GET /swagger-ui/nope Accept html | 같음 |  |  |  |
| POST /swagger-ui/nope | 같음 |  |  |  |
| GET /swagger-ui/index.html/ | 같음 |  |  |  |
| GET /swagger-ui/index.html/ Accept html | 같음 |  |  |  |
| POST /swagger-ui/index.html/ | 같음 |  |  |  |
| GET /swagger-ui/sub/index.html | 같음 |  |  |  |
| GET /swagger-ui/sub/index.html Accept html | 같음 |  |  |  |
| POST /swagger-ui/sub/index.html | 같음 |  |  |  |
| GET /swagger-ui/%69ndex.html | 같음 |  |  |  |
| GET /swagger-ui/%69ndex.html Accept html | 같음 |  |  |  |
| POST /swagger-ui/%69ndex.html | 같음 |  |  |  |
| GET /swagger-ui/INDEX.HTML | 같음 |  |  |  |
| GET /swagger-ui/INDEX.HTML Accept html | 같음 |  |  |  |
| POST /swagger-ui/INDEX.HTML | 같음 |  |  |  |
| GET /swagger-ui/index.html?x=1 | 같음 |  |  |  |
| GET /swagger-ui/index.html?x=1 Accept html | 같음 |  |  |  |
| POST /swagger-ui/index.html?x=1 | 같음 |  |  |  |
| GET /swagger-ui/../v3/api-docs | 같음 |  |  |  |
| GET /swagger-ui/../v3/api-docs Accept html | 같음 |  |  |  |
| POST /swagger-ui/../v3/api-docs | 같음 |  |  |  |
| GET /swagger-ui//index.html | 같음 |  |  |  |
| GET /swagger-ui//index.html Accept html | 같음 |  |  |  |
| POST /swagger-ui//index.html | 같음 |  |  |  |
| swagger-ui Accept text/html | 같음 |  |  |  |
| swagger-ui Accept json | 같음 |  |  |  |
| swagger-ui.css Accept json | 같음 |  |  |  |
| swagger-ui.css Origin | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'js' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'html' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'css' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'txt' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'json' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'xml' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'png' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'PNG' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'jpg' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'webp' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'svg' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'mp3' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'mp4' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'pdf' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'zip' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'yaml' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'yml' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'csv' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 'x' | 같음 |  |  |  |
| 인증된 없는 경로 확장자 '' | 같음 |  |  |  |
| 인증된 폴더에 점 | 같음 |  |  |  |
| 인증된 메서드 없음 확장자 | 같음 |  |  |  |
| 공개 경로 확장자 | 같음 |  |  |  |
| 공개 경로 확장자 js | 같음 |  |  |  |
| 방화벽 경로 확장자 | 같음 |  |  |  |
| actuator 확장자 | 같음 |  |  |  |

</details>

<details><summary><code>platform__tomcat-rejections</code> Tomcat 단계 요청 거절: 허용되지 않는 문자와 잘못된 퍼센트 인코딩 (같음 124, 동등 6, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 쿼리에 '"' | 같음 |  |  |  |
| 경로에 '"' | 같음 |  |  |  |
| 쿼리에 '<' | 같음 |  |  |  |
| 경로에 '<' | 같음 |  |  |  |
| 쿼리에 '>' | 같음 |  |  |  |
| 경로에 '>' | 같음 |  |  |  |
| 쿼리에 '[' | 같음 |  |  |  |
| 경로에 '[' | 같음 |  |  |  |
| 쿼리에 ']' | 같음 |  |  |  |
| 경로에 ']' | 같음 |  |  |  |
| 쿼리에 '\\' | 같음 |  |  |  |
| 경로에 '\\' | 같음 |  |  |  |
| 쿼리에 '^' | 같음 |  |  |  |
| 경로에 '^' | 같음 |  |  |  |
| 쿼리에 '`' | 같음 |  |  |  |
| 경로에 '`' | 같음 |  |  |  |
| 쿼리에 '{' | 같음 |  |  |  |
| 경로에 '{' | 같음 |  |  |  |
| 쿼리에 '\|' | 같음 |  |  |  |
| 경로에 '\|' | 같음 |  |  |  |
| 쿼리에 '}' | 같음 |  |  |  |
| 경로에 '}' | 같음 |  |  |  |
| 쿼리에 'ß' | 같음 |  |  |  |
| 경로에 'ß' | 같음 |  |  |  |
| 쿼리에 '가' | 같음 |  |  |  |
| 경로에 '가' | 같음 |  |  |  |
| 쿼리 %zz 공개 | 같음 |  |  |  |
| 쿼리 %zz 인증 후 회원 경로 | 같음 |  |  |  |
| 쿼리 %zz 인증 없음 보호 경로 | 같음 |  |  |  |
| 쿼리 %zz 없는 경로 인증됨 | 같음 |  |  |  |
| 쿼리 %zz 405 | 같음 |  |  |  |
| 쿼리 %zz actuator | 같음 |  |  |  |
| 쿼리 %4 공개 | 같음 |  |  |  |
| 쿼리 %4 인증 후 회원 경로 | 같음 |  |  |  |
| 쿼리 %4 인증 없음 보호 경로 | 같음 |  |  |  |
| 쿼리 %4 없는 경로 인증됨 | 같음 |  |  |  |
| 쿼리 %4 405 | 같음 |  |  |  |
| 쿼리 %4 actuator | 같음 |  |  |  |
| 쿼리 % 공개 | 같음 |  |  |  |
| 쿼리 % 인증 후 회원 경로 | 같음 |  |  |  |
| 쿼리 % 인증 없음 보호 경로 | 같음 |  |  |  |
| 쿼리 % 없는 경로 인증됨 | 같음 |  |  |  |
| 쿼리 % 405 | 같음 |  |  |  |
| 쿼리 % actuator | 같음 |  |  |  |
| 쿼리 %2 공개 | 같음 |  |  |  |
| 쿼리 %2 인증 후 회원 경로 | 같음 |  |  |  |
| 쿼리 %2 인증 없음 보호 경로 | 같음 |  |  |  |
| 쿼리 %2 없는 경로 인증됨 | 같음 |  |  |  |
| 쿼리 %2 405 | 같음 |  |  |  |
| 쿼리 %2 actuator | 같음 |  |  |  |
| 쿼리 %G0 공개 | 같음 |  |  |  |
| 쿼리 %G0 인증 후 회원 경로 | 같음 |  |  |  |
| 쿼리 %G0 인증 없음 보호 경로 | 같음 |  |  |  |
| 쿼리 %G0 없는 경로 인증됨 | 같음 |  |  |  |
| 쿼리 %G0 405 | 같음 |  |  |  |
| 쿼리 %G0 actuator | 같음 |  |  |  |
| 경로 /api/v1/topics%2Fx | 같음 |  |  |  |
| 경로 /api/v1/topics%2Fx Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics%2fx | 같음 |  |  |  |
| 경로 /api/v1/topics%2fx Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics%5Cx | 같음 |  |  |  |
| 경로 /api/v1/topics%5Cx Accept html | 같음 |  |  |  |
| 경로 /api/v1/to%00pics | 같음 |  |  |  |
| 경로 /api/v1/to%00pics Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics/% | 같음 |  |  |  |
| 경로 /api/v1/topics/% Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics/%zz | 같음 |  |  |  |
| 경로 /api/v1/topics/%zz Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics/%FF | 같음 |  |  |  |
| 경로 /api/v1/topics/%FF Accept html | 같음 |  |  |  |
| 경로 /%2e%2e/api/v1/topics | 같음 |  |  |  |
| 경로 /%2e%2e/api/v1/topics Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics/%2E%2E/x | 같음 |  |  |  |
| 경로 /api/v1/topics/%2E%2E/x Accept html | 같음 |  |  |  |
| 경로 /api/v1/topics//x | 같음 |  |  |  |
| 경로 /api/v1/topics//x Accept html | 같음 |  |  |  |
| 경로 /api/v1//topics | 같음 |  |  |  |
| 경로 /api/v1//topics Accept html | 같음 |  |  |  |
| 쿼리 %FF topics | 같음 |  |  |  |
| 쿼리 %C3 topics | 같음 |  |  |  |
| 쿼리 %zz=1 topics | 같음 |  |  |  |
| 쿼리 =%zz topics | 같음 |  |  |  |
| 쿼리 a=1&&b=%zz topics | 같음 |  |  |  |
| 쿼리 %e4%b8 topics | 같음 |  |  |  |
| 쿼리 a=%u0041 topics | 같음 |  |  |  |
| 쿼리 %zz /api/v1/posts | 같음 |  |  |  |
| 쿼리 %zz /api/v1/posts/calendar | 같음 |  |  |  |
| 쿼리 %zz /api/v1/posts/today | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 쿼리 %zz /api/v1/notifications/unread-status | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT notifications,posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT notifications,posts |  |
| 쿼리 %zz /api/v1/admin/audit-logs | 같음 |  |  |  |
| 쿼리 %zz /api/v1/admin/feedbacks | 같음 |  |  |  |
| 쿼리 %zz /api/v1/admin/posts | 같음 |  |  |  |
| 쿼리 %zz /api/v1/admin/topics | 같음 |  |  |  |
| 쿼리 %zz /api/v1/admin/users | 같음 |  |  |  |
| 쿼리 %zz /api/v1/admin/auth/me | 같음 | BEGIN RO: SELECT admins → ROLLBACK | 자동커밋: SELECT admins |  |
| 쿼리 %zz HEAD topics | 같음 |  |  |  |
| 쿼리 %zz /actuator | 같음 |  |  |  |
| 쿼리 %zz /actuator/info | 같음 |  |  |  |
| 쿼리 %zz /actuator/health/liveness | 같음 |  |  |  |
| 쿼리 %zz /actuator/health/nothing | 같음 |  |  |  |
| 쿼리 %zz /actuator/env | 같음 |  |  |  |
| 쿼리 %zz actuator Accept html | 같음 |  |  |  |
| 쿼리 %zz topics Accept html | 같음 |  |  |  |
| OPTIONS * | 같음 |  |  |  |
| OPTIONS * Accept html | 같음 |  |  |  |
| 루트 위로 올라가는 경로 | 같음 |  |  |  |
| 루트 안에서 끝나는 .. 경로 | 같음 |  |  |  |
| 쿼리 %zz topics Accept text/html | 같음 |  |  |  |
| 쿼리 %zz actuator Accept text/html | 같음 |  |  |  |
| 쿼리 %zz topics Accept text/* | 같음 |  |  |  |
| 쿼리 %zz actuator Accept text/* | 같음 |  |  |  |
| 쿼리 %zz topics Accept application/xml | 같음 |  |  |  |
| 쿼리 %zz actuator Accept application/xml | 같음 |  |  |  |
| 쿼리 %zz topics Accept bad | 같음 |  |  |  |
| 쿼리 %zz actuator Accept bad | 같음 |  |  |  |
| 쿼리 %zz topics Accept */* | 같음 |  |  |  |
| 쿼리 %zz actuator Accept */* | 같음 |  |  |  |
| 쿼리 %zz topics Accept application/json | 같음 |  |  |  |
| 쿼리 %zz actuator Accept application/json | 같음 |  |  |  |
| 쿼리 %zz topics Accept application/vnd.spring-boot.actuator.v3+json | 같음 |  |  |  |
| 쿼리 %zz actuator Accept application/vnd.spring-boot.actuator.v3+json | 같음 |  |  |  |
| 쿼리 %zz topics Accept application/problem+json, text/html | 같음 |  |  |  |
| 쿼리 %zz actuator Accept application/problem+json, text/html | 같음 |  |  |  |
| 쿼리 %zz topics Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 쿼리 %zz actuator Accept text/html, application/json;q=0.5 | 같음 |  |  |  |
| 쿼리 %zz POST | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 쿼리 %4 POST | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 쿼리 % POST | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 쿼리 %2 POST | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |
| 쿼리 %G0 POST | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: INSERT feedbacks → COMMIT | 자동커밋: SELECT users<br>자동커밋: INSERT feedbacks | 쓰기 한 문장: Java BEGIN / Rust 자동 커밋 (효과 같음) |

</details>

<details><summary><code>platform__unknown-path</code> 없는 경로: 인증 전후 응답과 끝 슬래시 (같음 5, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 토큰 없이 없는 경로 | 같음 |  |  |  |
| 회원 토큰으로 없는 경로 | 같음 |  |  |  |
| 공개 목록 경로 끝 슬래시 | 같음 |  |  |  |
| 대소문자가 다른 경로 | 같음 |  |  |  |
| actuator 비공개 경로 | 같음 |  |  |  |

</details>

<details><summary><code>post__create-errors</code> 게시물: 작성 거절과 요청 본문 오류 (같음 26, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 인증 없음 | 같음 |  |  |  |
| 차단된 회원 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 주제 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics → ROLLBACK |  |
| 마감된 주제 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics → ROLLBACK |  |
| 시작 전 주제 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics → ROLLBACK |  |
| 없는 업로드 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| 다른 회원의 업로드 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| 이미 사용한 업로드 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| 반려된 업로드 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| claim 기한이 지난 업로드 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| claim 기한이 지난 미처리 업로드 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| S3가 403이면 사진이 없는 것으로 본다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK |  |
| S3 확인이 5xx면 서버 오류 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads → ROLLBACK |  |
| 본문 없음 | 같음 |  |  |  |
| 깨진 JSON | 같음 |  |  |  |
| 주제 누락 | 같음 |  |  |  |
| 업로드 누락 | 같음 |  |  |  |
| 제목이 10자를 넘는다 | 같음 |  |  |  |
| 잘못된 UUID 형식 | 같음 |  |  |  |
| UUID가 아닌 값 타입 | 같음 |  |  |  |
| JSON 배열 본문 | 같음 |  |  |  |
| 제목이 공백이면 비어 있는 제목으로 작성된다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts; SELECT photos; INSERT photos; INSERT posts; UPDATE post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT photos; UPDATE post_image_uploads; INSERT photos; INSERT posts → COMMIT |  |
| 업로드 URL 발급은 인증이 필요하다 | 같음 |  |  |  |
| 차단된 회원은 업로드를 못 받는다 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 업로드 URL 발급에 본문이 있어도 된다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT |  |

</details>

<details><summary><code>post__create-flow</code> 게시물: 업로드 발급 - 작성 - 처리 콜백 - 반려 후 재작성 (같음 10, 동등 4, 다름 4)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 업로드 URL 발급 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT |  |
| 사진을 올리기 전에는 작성할 수 없다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK |  |
| staging 사진이 있으면 VALIDATING으로 작성된다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts; SELECT photos; INSERT photos; INSERT posts; UPDATE post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT photos; UPDATE post_image_uploads; INSERT photos; INSERT posts → COMMIT |  |
| 내 오늘 게시물 상태 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 같은 주제에 다시 작성할 수 없다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads → ROLLBACK |  |
| 처리 완료 콜백이 게시물을 검수 대기로 올린다 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads; UPDATE photos; UPDATE posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE photos; UPDATE posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 같은 콜백을 다시 보내도 같다 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 작성자는 검수 대기 게시물을 본다 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 다른 회원은 검수 대기 게시물을 못 본다 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 다른 회원의 업로드 발급 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT |  |
| 게시물보다 먼저 처리 완료 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 처리가 끝난 업로드로 작성하면 바로 검수 대기다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts; SELECT photos; INSERT photos; INSERT posts; UPDATE post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT photos; UPDATE post_image_uploads; INSERT photos; INSERT posts → COMMIT |  |
| 세 번째 회원 업로드 발급 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT |  |
| 세 번째 회원 작성 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts; SELECT photos; INSERT photos; INSERT posts; UPDATE post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT photos; UPDATE post_image_uploads; INSERT photos; INSERT posts → COMMIT |  |
| 처리 실패 콜백이 게시물을 반려한다 | 다름 | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; UPDATE post_image_uploads; UPDATE posts → COMMIT | BEGIN: SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; UPDATE post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 반려된 게시물 상태 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 반려 뒤에는 다시 업로드해 작성할 수 있다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; INSERT post_image_uploads → COMMIT |  |
| 재작성 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT posts FOR NO KEY UPDATE OF posts; SELECT photos; INSERT photos; INSERT posts; UPDATE post_image_uploads → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT topics; SELECT post_image_uploads; SELECT post_image_uploads FOR NO KEY UPDATE OF post_image_uploads; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT photos; UPDATE post_image_uploads; INSERT photos; INSERT posts → COMMIT |  |

</details>

<details><summary><code>post__detail-update-delete</code> 게시물: 상세·수정·삭제 (같음 22, 동등 13, 다름 7)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 승인된 게시물(익명) | 같음 |  |  |  |
| 승인된 게시물(좋아요 한 회원) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users; SELECT post_likes ×2 → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users<br>자동커밋: SELECT post_likes<br>자동커밋: SELECT post_likes | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 4개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 제목 없는 게시물 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users; SELECT post_likes ×2 → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users<br>자동커밋: SELECT post_likes<br>자동커밋: SELECT post_likes | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 4개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 차단된 작성자의 게시물 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users; SELECT post_likes ×2 → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users<br>자동커밋: SELECT post_likes<br>자동커밋: SELECT post_likes | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 4개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 검수 대기(작성자) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 검수 대기(타인) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 검수 대기(익명) | 같음 |  |  |  |
| 반려(작성자) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 검증 중(작성자) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 삭제된 게시물 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 없는 게시물 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 잘못된 ID | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 대문자 ID는 형식 오류 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users; SELECT post_likes ×2 → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users<br>자동커밋: SELECT post_likes<br>자동커밋: SELECT post_likes | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 4개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 중괄호 ID | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 수정: 인증 없음 | 같음 |  |  |  |
| 수정: 제목 변경 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT |  |
| 수정: 같은 제목 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics → COMMIT |  |
| 수정: 제목 지우기(null) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT |  |
| 수정: 제목 공백 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics → COMMIT |  |
| 수정: 제목 키 없음 | 같음 |  |  |  |
| 수정: 제목 11자 | 같음 |  |  |  |
| 수정: 제목 10자 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT |  |
| 수정: 깨진 본문 | 같음 |  |  |  |
| 수정: 본문 없음 | 같음 |  |  |  |
| 수정: 남의 게시물 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics → ROLLBACK | SELECT 개수 다름: Java 2 / Rust 3 (Java만 [], Rust만 [SELECT topics]) |
| 수정: 없는 게시물 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK |  |
| 수정: 삭제된 게시물 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK |  |
| 수정: 잘못된 ID | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 수정: 차단된 회원 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 수정: 마감된 주제의 게시물 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics → ROLLBACK |  |
| 수정: 검수 대기 게시물 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users; SELECT posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users; SELECT photos,posts FOR NO KEY UPDATE OF posts; SELECT topics; UPDATE posts → COMMIT |  |
| 삭제: 인증 없음 | 같음 |  |  |  |
| 삭제: 남의 게시물 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제: 없는 게시물 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제: 잘못된 ID | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 삭제: 내 게시물(좋아요도 지워진다) | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts; DELETE post_likes; UPDATE photos; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts; UPDATE photos; DELETE post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제: 이미 삭제됨(멱등) | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts; DELETE post_likes → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; DELETE post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제 뒤 조회 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics,users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 삭제: 검수 대기 게시물 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts; DELETE post_likes; UPDATE photos; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts; UPDATE photos; DELETE post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제: 반려 게시물 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제: 차단된 회원 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF posts; DELETE post_likes; UPDATE photos; UPDATE posts → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT photos,posts FOR NO KEY UPDATE OF photos,posts; UPDATE posts; UPDATE photos; DELETE post_likes → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF posts / Rust FOR NO KEY UPDATE OF photos,posts |
| 삭제 뒤 다시 작성 가능한지(오늘 상태) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |

</details>

<details><summary><code>post__list</code> 게시물: 목록 조회(정렬·페이지·시드·바인딩 오류) (같음 35, 동등 1, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 익명 기본(최신순) | 같음 |  |  |  |
| 익명 최신순 명시 | 같음 |  |  |  |
| 익명 인기순 | 같음 |  |  |  |
| 익명 다른 날짜 | 동등 | BEGIN RO: SELECT topics; SELECT photos,posts,topics,users; SELECT post_likes → COMMIT | 자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts,topics,users<br>자동커밋: SELECT post_likes | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 익명 페이지 크기 2, 첫 페이지 | 같음 |  |  |  |
| 익명 페이지 크기 2, 둘째 페이지 | 같음 |  |  |  |
| 익명 범위를 넘는 페이지 | 같음 |  |  |  |
| 익명 인기순 페이지 크기 1 | 같음 |  |  |  |
| 익명 무작위 시드 고정 | 같음 |  |  |  |
| 익명 무작위 둘째 페이지 | 같음 |  |  |  |
| 익명 시드 길이 부족 | 같음 |  |  |  |
| 익명 시드 허용되지 않는 문자 | 같음 |  |  |  |
| 익명 시드는 무작위순에서만 | 같음 |  |  |  |
| 익명 페이지 크기 0 | 같음 |  |  |  |
| 익명 페이지 크기 101 | 같음 |  |  |  |
| 익명 페이지 크기 100 | 같음 |  |  |  |
| 익명 음수 페이지 | 같음 |  |  |  |
| 익명 정수가 아닌 페이지 | 같음 |  |  |  |
| 익명 정수 앞뒤 공백 | 같음 |  |  |  |
| 익명 16진수 페이지 | 같음 |  |  |  |
| 익명 정수 범위를 넘는 페이지 | 같음 |  |  |  |
| 익명 빈 페이지 | 같음 |  |  |  |
| 익명 잘못된 정렬 | 같음 |  |  |  |
| 익명 대문자 정렬 | 같음 |  |  |  |
| 익명 빈 정렬 | 같음 |  |  |  |
| 익명 잘못된 날짜 | 같음 |  |  |  |
| 익명 미래 날짜 | 같음 |  |  |  |
| 익명 주제가 없는 날 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 익명 여러 오류는 첫 필드 | 같음 |  |  |  |
| 익명 바인딩 오류가 검증보다 먼저 | 같음 |  |  |  |
| 로그인(좋아요 여부 포함) 최신 | 같음 |  |  |  |
| 로그인 인기순 | 같음 |  |  |  |
| 차단된 회원 | 같음 |  |  |  |
| 없는 회원 | 같음 |  |  |  |
| 로그인 무작위 | 같음 |  |  |  |
| 토큰 형식 오류 | 같음 |  |  |  |

</details>

<details><summary><code>post__today-calendar</code> 게시물: 오늘 상태와 캘린더 (같음 12, 동등 15, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 오늘 상태: 인증 없음 | 같음 |  |  |  |
| 오늘 상태: 승인 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 오늘 상태: 검수 대기 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 오늘 상태: 반려 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 오늘 상태: 검증 중 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 오늘 상태: 삭제 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 오늘 상태: 승인(다른) | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT topics; SELECT posts → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT topics<br>자동커밋: SELECT photos,posts | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 3개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 대상 다름: Java만 [SELECT posts] / Rust만 [SELECT photos,posts] |
| 오늘 상태: 차단된 회원 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 오늘 상태: 없는 회원 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 캘린더: 인증 없음 | 같음 |  |  |  |
| 캘린더: 10월 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더: 9월 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더: 게시물 없는 달 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더: 다른 회원 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더: 월 0 | 같음 |  |  |  |
| 캘린더: 월 13 | 같음 |  |  |  |
| 캘린더: 연도 0 | 같음 |  |  |  |
| 캘린더: 연도 누락 | 같음 |  |  |  |
| 캘린더: 월 누락 | 같음 |  |  |  |
| 캘린더: 숫자 아님 | 같음 |  |  |  |
| 캘린더: 큰 연도 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더: 범위 밖 연도 | 같음 |  |  |  |
| 캘린더: 차단된 회원 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topics → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topics | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더 월 목록: 인증 없음 | 같음 |  |  |  |
| 캘린더 월 목록 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topic_date,topics,users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topic_date,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더 월 목록: 게시물 없는 회원 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topic_date,topics,users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topic_date,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |
| 캘린더 월 목록: 차단된 회원 | 동등 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users; SELECT photos,posts,topic_date,topics,users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users<br>자동커밋: SELECT photos,posts,topic_date,topics,users | 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 |

</details>

<details><summary><code>topic__before-open</code> 주제: 열리기 전 단계와 시각 이동 (같음 3, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 열리기 전 | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |
| 20초 뒤 열림 | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |
| 12시간 뒤 마감 | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |

</details>

<details><summary><code>topic__get-by-date</code> 주제: 날짜로 조회와 진행 단계 (같음 8, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 오늘(진행 중) | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |
| 어제(마감) | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |
| 주제 없는 날 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |
| 미래 날짜 | 같음 |  |  |  |
| 날짜 누락 | 같음 |  |  |  |
| 잘못된 날짜 형식 | 같음 |  |  |  |
| 없는 날짜 | 같음 |  |  |  |
| 빈 날짜 | 같음 |  |  |  |

</details>

<details><summary><code>topic__kst-boundary</code> 주제: 한국 시각 자정 경계 (같음 3, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 13일은 이미 오늘이다 | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |
| 12일은 마감 | 같음 | BEGIN RO: SELECT topics → COMMIT | 자동커밋: SELECT topics |  |
| 14일은 미래 | 같음 |  |  |  |

</details>

<details><summary><code>topic__soft-deleted</code> 주제: 삭제된 주제는 조회되지 않는다 (같음 1, 동등 0, 다름 0)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 삭제된 주제의 날짜 | 같음 | BEGIN RO: SELECT topics → ROLLBACK | 자동커밋: SELECT topics |  |

</details>

<details><summary><code>user__callback-body-limit</code> 회원: 이미지 처리 콜백 본문 크기 제한(CallbackBodySizeLimitFilter) (같음 5, 동등 0, 다름 1)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 상한(65536바이트) 본문은 통과 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 상한을 1바이트 넘으면 413 | 같음 |  |  |  |
| 서명이 없어도 크기가 먼저 거절된다 | 같음 |  |  |  |
| 상한 이하 본문이어도 서명이 없으면 401 | 같음 |  |  |  |
| 경로의 업로드 ID가 틀려도 크기가 먼저 거절된다 | 같음 |  |  |  |
| 콜백 밖의 공개 경로에는 제한이 걸리지 않는다 | 같음 |  |  |  |

</details>

<details><summary><code>user__signature-callbacks</code> 회원: 사인 이미지 처리 콜백(HMAC 인증, 업로드 URL, 완료, 실패) (같음 23, 동등 0, 다름 12)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 헤더가 모두 없으면 401 | 같음 |  |  |  |
| 타임스탬프만 없음 | 같음 |  |  |  |
| 서명만 없음 | 같음 |  |  |  |
| 타임스탬프가 숫자가 아님 | 같음 |  |  |  |
| 다른 비밀키로 서명 | 같음 |  |  |  |
| 다른 환경(audience)으로 서명 | 같음 |  |  |  |
| 다른 콜백 경로의 서명을 재사용 | 같음 |  |  |  |
| 서명 접두어(v1=)가 없음 | 같음 |  |  |  |
| 서명이 16진수가 아님 | 같음 |  |  |  |
| 서명이 틀림 | 같음 |  |  |  |
| 300초 전 타임스탬프는 통과(없는 업로드라 204) | 같음 |  |  |  |
| 301초 전은 거절 | 같음 |  |  |  |
| 30초 뒤 타임스탬프는 통과 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 31초 뒤는 거절 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 관리자 토큰이 있어도 콜백 인증은 서명으로만 한다 | 같음 |  |  |  |
| 업로드 ID 형식이 틀리면 인증보다 먼저 400 | 같음 |  |  |  |
| 하이픈 없는 업로드 ID는 표준형이 아니라 400 | 같음 |  |  |  |
| 자리수가 짧은 UUID(UUID.fromString은 받지만 표준형이 아님)도 400 | 같음 |  |  |  |
| 대문자 업로드 ID는 표준형으로 읽고 소문자 경로로 서명한다 | 같음 |  |  |  |
| 대문자 경로 그대로 서명하면 거절 | 같음 |  |  |  |
| 처리용 업로드 URL 발급 | 같음 |  |  |  |
| 회원이 없는 업로드 ID도 URL은 발급된다 | 같음 |  |  |  |
| GET은 허용되지 않음 | 같음 |  |  |  |
| 처리 중인 회원의 완료 콜백 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users; UPDATE users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 완료 콜백을 다시 받아도 204 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 완료 뒤 사인 조회는 새 이미지 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 처리 시간이 지난 회원의 완료 콜백은 무시 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 실패한 회원의 완료 콜백은 무시 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 없는 업로드 ID의 완료 콜백도 204 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 처리 중인 회원의 실패 콜백 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users; UPDATE users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 실패 뒤 사인 조회는 다시 등록 필요 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → ROLLBACK | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 실패 뒤 같은 업로드 ID로 변경하면 재업로드 필요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 실패 콜백을 다시 받아도 204 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 시간이 지난 처리의 실패 콜백은 FAILED로 바꾼다 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users; UPDATE users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users; UPDATE users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 없는 업로드 ID의 실패 콜백도 204 | 다름 | BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |

</details>

<details><summary><code>user__signature</code> 회원: 내 사인 조회·업로드 URL 발급·변경(UserController, UserService)과 처리 시간 초과 (같음 25, 동등 0, 다름 10)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 정상 회원의 사인 조회 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 정지 회원도 사인은 조회할 수 있다(validateExisting) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 탈퇴 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 토큰 없이 조회하면 401 | 같음 |  |  |  |
| 사인 변경용 업로드 URL 발급 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 정지 회원은 업로드 URL을 받을 수 없다(validateUsable) | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 탈퇴 회원의 업로드 URL 발급은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 없는 회원의 업로드 URL 발급은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 본문 없이 변경 | 같음 |  |  |  |
| 업로드 ID가 없음 | 같음 |  |  |  |
| 업로드 ID가 null | 같음 |  |  |  |
| 업로드 ID가 빈 문자열 | 같음 |  |  |  |
| 업로드 ID 형식 오류 | 같음 |  |  |  |
| 업로드 ID가 숫자 | 같음 |  |  |  |
| 업로드 ID가 하이픈 없는 32자 | 같음 |  |  |  |
| 정지 회원의 변경은 형식이 맞아도 403 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 정지 회원의 변경도 형식이 틀리면 400이 먼저 | 같음 |  |  |  |
| 탈퇴 회원의 변경은 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 업로드한 이미지가 없음 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; SELECT users → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; SELECT users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| PNG가 아닌 이미지 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; SELECT users → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; SELECT users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 1MiB를 넘는 이미지 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; SELECT users → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; SELECT users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 정확히 1MiB인 PNG는 처리 시작 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; SELECT users; UPDATE users → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; SELECT users; UPDATE users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 처리 중에 같은 업로드 ID로 다시 변경하면 같은 응답 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 처리 중에도 이전 사인은 조회된다 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 처리 시작 6분 50초 뒤는 아직 유효 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 7분 20초가 지나면 다시 등록해야 한다(조회) | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 시간이 지난 업로드 ID로 변경하면 재업로드 필요 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 현재 사인과 같은 업로드 ID는 처리 상태만 정리하고 성공 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; UPDATE users → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; UPDATE users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 정리 뒤 조회는 다시 가능 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 다른 회원이 쓰는 사인 이미지의 업로드 ID는 찾을 수 없음 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; SELECT users → ROLLBACK | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; SELECT users → ROLLBACK | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 새 업로드 ID 발급 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| original과 thumbnail이 모두 있으면 바로 완료 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users; SELECT users; UPDATE users → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users; SELECT users; UPDATE users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |
| 완료 뒤 조회는 새 사인 | 같음 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT users |  |
| 대문자·공백이 섞인 업로드 ID도 같은 UUID로 읽힌다 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN: SELECT users FOR NO KEY UPDATE OF users → COMMIT | 자동커밋: SELECT users<br>BEGIN: SELECT users FOR UPDATE OF users → COMMIT | 잠금 종류·대상 다름: Java FOR NO KEY UPDATE OF users / Rust FOR UPDATE OF users |

</details>

<details><summary><code>user__withdraw</code> 회원: 탈퇴(DELETE /users/me)와 탈퇴 뒤 로그인·재발급, 정지 회원의 탈퇴 (같음 14, 동등 0, 다름 2)</summary>

| 단계 | 판정 | Java | Rust | 비고 |
| --- | --- | --- | --- | --- |
| 사인 업로드 URL 발급 | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 가입 완료 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 다른 기기로 한 번 더 로그인 | 같음 | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; INSERT user_refresh_tokens → COMMIT |  |
| 토큰 없이 탈퇴하면 401 | 같음 |  |  |  |
| 관리자 토큰으로 탈퇴하면 403 | 같음 |  |  |  |
| 탈퇴 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT social_accounts; SELECT apple_authorizations → COMMIT<br>BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations ×2; SELECT social_accounts; SELECT user_refresh_tokens; DELETE social_accounts; SELECT advisory_xact_lock/1 ×2; UPDATE user_refresh_tokens; UPDATE users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT social_accounts<br>자동커밋: SELECT apple_authorizations<br>BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations; DELETE apple_authorizations; DELETE social_accounts; SELECT user_refresh_tokens; SELECT advisory_xact_lock/1 ×2; UPDATE user_refresh_tokens; UPDATE users → COMMIT | 쓰기 문장 집합 다름: Java만 [] / Rust만 [DELETE apple_authorizations] / 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 개수 다름: Java 9 / Rust 7 (Java만 [SELECT apple_authorizations; SELECT social_accounts], Rust만 []) |
| 탈퇴 뒤 사인 조회는 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 이미 탈퇴한 회원의 탈퇴는 401 | 같음 | BEGIN RO: SELECT users → COMMIT | 자동커밋: SELECT users |  |
| 탈퇴 뒤 첫 기기의 리프레시 토큰은 폐기됨 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 탈퇴 뒤 다른 기기의 리프레시 토큰도 폐기됨 | 같음 | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT | BEGIN: SELECT user_refresh_tokens; SELECT advisory_xact_lock/1; SELECT user_refresh_tokens → COMMIT |  |
| 탈퇴 뒤 같은 계정으로 로그인하면 가입이 필요하다 | 같음 | BEGIN: SELECT social_accounts,users → COMMIT | BEGIN: SELECT social_accounts,users → COMMIT |  |
| 탈퇴한 계정으로 다시 가입할 업로드 URL | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |
| 재가입 | 같음 | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT | BEGIN: SELECT social_accounts,users; UPSERT consumed_signup_tokens; SELECT users; INSERT users; INSERT social_accounts; INSERT user_refresh_tokens → COMMIT |  |
| 정지 회원도 탈퇴할 수 있다 | 다름 | BEGIN RO: SELECT users → COMMIT<br>BEGIN RO: SELECT social_accounts; SELECT apple_authorizations → COMMIT<br>BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations ×2; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; UPDATE users → COMMIT | 자동커밋: SELECT users<br>자동커밋: SELECT social_accounts<br>자동커밋: SELECT apple_authorizations<br>BEGIN: SELECT users; SELECT social_accounts; SELECT apple_authorizations; DELETE apple_authorizations; SELECT user_refresh_tokens; UPDATE user_refresh_tokens; UPDATE users → COMMIT | 쓰기 문장 집합 다름: Java만 [] / Rust만 [DELETE apple_authorizations] / 읽기 묶음 다름: Java BEGIN READ ONLY에 SELECT 2개 / Rust 묶음 없음(자동 커밋). READ COMMITTED에서는 문장마다 스냅샷이라 결과는 같고, 한 트랜잭션 안의 읽기 일관성만 다르다 / SELECT 개수 다름: Java 8 / Rust 7 (Java만 [SELECT apple_authorizations], Rust만 []) |
| 정지 회원이 탈퇴해도 소셜 계정 연결은 남아 로그인할 수 없다 | 같음 | BEGIN: SELECT social_accounts,users → ROLLBACK | BEGIN: SELECT social_accounts,users → ROLLBACK |  |
| 정지 회원의 소셜 계정으로 업로드 URL을 받을 수 없다 | 같음 | 자동커밋: SELECT social_accounts,users | 자동커밋: SELECT social_accounts,users |  |

</details>

