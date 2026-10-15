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
