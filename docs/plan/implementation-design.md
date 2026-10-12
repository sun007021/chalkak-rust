# 구현 설계: 이슈 단위와 병렬 작업

[전환 계획](migration-plan.md)의 Phase 0~3을 GitHub 이슈 단위로 나누고, 병렬로 진행할 수 있는 경계와 crate 사이의 계약을 정한다.

## 1. 진행 방식

- 큰 단위 하나 = GitHub 이슈 하나 = 브랜치 하나(`feat/<이슈번호>-<짧은-이름>`) = PR 하나. PR은 `main`으로 보낸다.
- 커밋은 의미 있는 기능 단위로 나눈다. 예: "HTTP/1.1 요청 파서", "라우터 404·405 판정". 형식: `<영역>: <무엇을>` 한국어 요약 + 본문에 이유.
- **TDD:** 실패하는 테스트를 먼저 커밋 단위 안에 쓰고 구현한다. 원본 동작이 기준인 것은 원본 Java 코드·테스트(`2026-Chalkak/backend`)에서 기대값을 가져와 테스트 이름에 근거를 남긴다.
- **완료 조건(모든 PR 공통):** `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace` 통과. 원본 동작을 옮긴 이슈는 해당 하네스 시나리오가 Java 대 Rust로 통과.
- 병렬 작업은 서로 다른 crate만 건드리게 나눈다. 공통 파일(`Cargo.toml` workspace 의존성 목록)은 각 PR이 자기 줄만 추가하고, 충돌은 머지하는 쪽이 정리한다.
- 의존성은 [전환 계획 2장](migration-plan.md#2-결정-프레임워크-없이-vanilla-rust의-범위-2026-10-12-b안-확정) 허용 목록 안에서만 추가한다. 암호는 RustCrypto 계열로 통일한다(`hmac`, `sha2`, `aes-gcm`, `p256`, `rsa`, `bcrypt`).

## 2. 이슈 목록과 의존 관계

```
배치 1 (서로 독립, 병렬)
  #1 harness: 가짜 외부 서버 + SQL·OpenAPI 수집      (harness/)
  #2 platform-core: 설정·시계·오류·JSON 로그·스케줄러   (crates/platform/core)
  #3 platform-http: HTTP/1.1 서버·라우터·미들웨어      (crates/platform/http)
  #4 platform-http-client: rustls HTTPS 클라이언트     (crates/platform/http, client 모듈)  ← #3과 같은 crate, 모듈 분리
  #5 platform-db: 커넥션 풀·트랜잭션·Flyway 호환 마이그레이터 (crates/platform/db)
  #6 platform-jwt: HS256·RS256·ES256·JWKS             (crates/platform/jwt)
  #7 platform-aws: SigV4·presign·HeadObject·IMDSv2    (crates/platform/aws)

배치 2 (배치 1 이후)
  #H server 조립 + actuator + 보안 헤더·CORS·인증 미들웨어 + topic   → 첫 Java 대 Rust 하네스 통과

배치 3 (#H 이후, 도메인 간 병렬)
  #I auth + user(회원·탈퇴)        #J 사인(user signature)      #K post + photo
  #L like     #M feedback     #N notification(+Slack)     #O admin
  (#J·#K는 #G, #I는 #F·#D에 의존. #L·#N은 #K 이후가 자연스럽다.)

배치 4
  #P 주기 작업 4종 + 런타임 지표 로그     #Q 성능·동시성·장시간 시험     #R 배포(private AWS)
```

#3과 #4는 같은 crate라 파일 충돌을 피하도록 `server/`, `client/` 하위 모듈로 나눈다.

## 3. crate 계약 (배치 1이 지켜야 할 공개 API)

배치 2가 이 시그니처로 조립한다. 구현 중 바꿔야 하면 PR 설명에 이유를 적는다.

### chalkak-core
```rust
pub trait Clock: Send + Sync { fn now(&self) -> jiff::Timestamp; }
pub struct SystemClock;                 // 실제 시각(하네스는 libfaketime으로 프로세스 시각을 옮긴다)
pub struct FixedClock(/* 테스트용, 시각 이동 가능 */);

pub fn parse_duration(s: &str) -> Result<std::time::Duration, ConfigError>; // "15m", "1h", "30d", "PT1H", "P30D", "7m"
pub struct Env { /* 환경변수 읽기, 누락·형식 오류를 ConfigError로 */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ErrorCode { BUSINESS_ERROR, SIGNATURE_REGISTRATION_REQUIRED, SIGNATURE_REUPLOAD_REQUIRED,
    SIGNATURE_PROCESSING_PENDING, RESOURCE_STATE_CHANGED, UNAUTHORIZED, REAUTHENTICATION_REQUIRED, FORBIDDEN, INTERNAL_ERROR }
pub enum ErrorKind { Business /*400*/, NotFound /*404*/, Unauthorized /*401*/, Forbidden /*403*/ }
pub struct AppError { pub kind: ErrorKind, pub code: ErrorCode, pub message: String }
// 응답 본문: {"errorCode":"...","message":"..."} (필드 순서 고정)

pub mod log;      // logstash 형식 JSON 한 줄 로그. 필드: @timestamp, level, message, logger_name, thread_name, requestId, type, errorCode, status, exception ...
pub mod request_id; // 요청별 UUID, 응답 헤더 X-Request-Id
pub mod scheduler;  // cron("0 30 4 * * *", Asia/Seoul)와 fixed-rate 작업
```

### chalkak-http (server)
```rust
pub struct Request { pub method: Method, pub path: String, pub query: Vec<(String, String)>,
    pub headers: Headers, pub body: Vec<u8>, pub remote_addr: SocketAddr }
pub struct Response { pub status: u16, pub headers: Headers, pub body: Vec<u8> }
pub trait Handler: Send + Sync { fn handle(&self, req: Request) -> Response; }

pub struct Router; // route(Method, "/api/v1/posts/{postId}", handler)
pub enum RouteMatch<'a> { Found { handler: &'a dyn Handler, params: Vec<(String, String)> }, MethodNotAllowed { allowed: Vec<Method> }, NotFound }

pub struct Limits { max_header_bytes, max_headers, max_body_bytes, read_timeout, keep_alive_timeout, max_connections }
pub struct Server; // bind(addr, workers, limits) → serve(Arc<dyn Handler>, shutdown: ShutdownSignal)
```
- HTTP/1.1 keep-alive, `Content-Length`·`chunked` 요청 본문, `Expect: 100-continue`, HEAD는 본문 생략.
- 경로 끝 슬래시·대소문자는 원본(Spring Boot 4 기본: 끝 슬래시 불일치)과 같게. 하네스 `platform/unknown-path` 결과가 기준.

### chalkak-http (client)
```rust
pub struct HttpClient; // rustls + webpki-roots, 연결·읽기 타임아웃, http:// 도 지원(하네스 fakes용)
pub fn request(&self, method, url, headers, body) -> Result<ClientResponse, ClientError>;
```

### chalkak-db
```rust
pub struct Pool;   // Pool::connect(&DbConfig, size) — postgres::Client 묶음, 빌려주고 돌려받기, 끊긴 연결 교체
pub struct Conn;   // Deref<Target = postgres::Client>
impl Conn { pub fn transaction<T>(&mut self, f: impl FnOnce(&mut postgres::Transaction) -> Result<T, DbError>) -> Result<T, DbError>; }
pub mod migrate;   // Flyway 호환: 같은 flyway_schema_history 스키마, 같은 CRC32 체크섬, validate-on-migrate
```
- Flyway 체크섬은 원본과 교차 검증한다: Java가 만든 이력 테이블을 Rust가 검증 통과, 그 반대도 통과.

### chalkak-jwt
```rust
pub fn hs256_encode(header: &Value, claims: &Value, key: &[u8]) -> String;
pub fn hs256_decode(token: &str, key: &[u8]) -> Result<Claims, JwtError>;    // 서명만 검증, 클레임 검증은 호출 측 Validator
pub fn rs256_verify(token: &str, jwks: &Jwks) -> Result<Claims, JwtError>;   // kid로 키 선택
pub fn es256_sign(header: &Value, claims: &Value, pkcs8_pem: &str) -> Result<String, JwtError>; // Apple client secret
pub struct Validator { issuer, audience, clock_skew, required_claims… } // Spring JwtTimestampValidator 등과 같은 규칙
pub struct JwksCache; // 키 세트 캐시, 모르는 kid면 다시 가져옴(원본 Nimbus 동작 확인 후 맞춤)
```

### chalkak-aws
```rust
pub struct Credentials { access_key_id, secret_access_key, session_token: Option<String> }
pub fn presign_put(cred, region, bucket, key, content_type, content_length, expires, now) -> String; // 원본 AWS SDK와 바이트 동일
pub fn sign_head_object(cred, region, bucket, key, now) -> Vec<(String, String)>;                  // 요청 헤더
pub trait CredentialsProvider { fn credentials(&self) -> Result<Credentials, AwsError>; }        // Env → IMDSv2 순서, 만료 전 갱신
```
- AWS 공식 SigV4 테스트 벡터와, 같은 시각·자격증명으로 원본 Java SDK가 만든 presigned URL을 기대값으로 쓴다.

## 4. 서브에이전트 작업 규칙

배치 1·3의 각 이슈는 Sonnet 서브에이전트 하나가 별도 git worktree에서 맡는다. 맡기는 내용:
1. 이슈 본문(범위, 계약, 완료 조건)과 이 문서.
2. 원본 코드 위치(`/home/claude/2026-Chalkak/backend`)와 해당 Java 클래스·테스트 목록.
3. 규칙: TDD, 기능 단위 커밋, 허용 의존성만, `unsafe` 금지, 계약 밖의 crate는 수정 금지, 끝나면 브랜치 push 후 결과 보고.

PR 생성·CI 확인·리뷰·머지는 조율하는 세션이 한다.
