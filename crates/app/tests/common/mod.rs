#![allow(dead_code)]
//! auth·user 서비스 통합 테스트가 함께 쓰는 보조 도구.
//!
//! 원본은 서비스 테스트에서 외부 경계(ID Token 검증기, Apple 토큰 클라이언트, S3 저장소·업로드 발급기)만 Mockito로
//! 바꾸고 나머지는 실제 Bean과 PostgreSQL을 쓴다. 여기서도 같게, 그 경계 네 곳만 가짜(`Stub`·`Fake`)로 두고
//! 서비스·저장소·토큰 발급기는 실제 것을 쓴다.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalkak_app::auth::apple::{AppleTokenClient, AppleTokenExchangeResult};
use chalkak_app::auth::domain::{RefreshTokenPolicy, SocialProvider, VerifiedSocialIdentity};
use chalkak_app::auth::oidc::{IdTokenVerifier, SocialIdentityVerifier};
use chalkak_app::auth::refresh::UserRefreshTokenService;
use chalkak_app::auth::repository::{
    AppleAuthorizationRepository, PendingAppleAuthorizationRepository, SocialAccountRepository,
};
use chalkak_app::auth::revoker::AppleSocialConnectionRevoker;
use chalkak_app::auth::service::{
    AppleSignupAuthorizationService, ExistingSocialAccountLoginProcessor, SocialLoginService,
    SocialSignupDeps, SocialSignupService,
};
use chalkak_app::auth::tokens::{
    AccessTokenIssuer, AppleCipher, IdentityFingerprint, SignupTokens,
};
use chalkak_app::testing::TestDb;
use chalkak_app::user::UserRepository;
use chalkak_app::user::domain::{
    SignatureImagePolicy, SignatureStorageKeys, StoredImageMetadata, User,
};
use chalkak_app::user::service::{SignatureProcessingPolicy, UserService, UserWithdrawalService};
use chalkak_app::user::storage::{
    SignatureImageStorage, SignatureImageUpload, SignatureImageUploadIssuer,
    SignatureProcessingImageUpload,
};
use chalkak_app::web::{ApiError, Routes};
use chalkak_core::FixedClock;
use chalkak_core::error::{AppError, ErrorCode, ErrorKind};
use chalkak_db::Pool;
use chalkak_http::Router;
use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

pub const NOW: &str = "2026-10-12T00:00:00Z";
pub const ACCESS_KEY: [u8; 32] = [7; 32];
pub const SIGNUP_KEY: [u8; 32] = [9; 32];
pub const IDENTITY_KEY: [u8; 32] = [3; 32];
pub const CIPHER_KEY: [u8; 32] = [5; 32];
pub const ACCESS_AUDIENCE: &str = "chalkak-access";
pub const SIGNUP_AUDIENCE: &str = "chalkak-social-signup";

pub fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

pub fn plus(t: Timestamp, seconds: i64) -> Timestamp {
    t.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

pub fn minutes_later(t: Timestamp, minutes: i64) -> Timestamp {
    plus(t, minutes * 60)
}

// ---------------------------------------------------------------------------------------------
// 오류 확인
// ---------------------------------------------------------------------------------------------

pub fn app_error(error: ApiError) -> AppError {
    match error {
        ApiError::App(e) => e,
        other => panic!("도메인 오류가 아니다: {other:?}"),
    }
}

/// 오류가 `(종류, 코드, 메시지)`인지 확인한다.
#[track_caller]
pub fn assert_app_error<T: std::fmt::Debug>(
    result: Result<T, ApiError>,
    kind: ErrorKind,
    code: ErrorCode,
    message: &str,
) {
    let error = app_error(result.expect_err("오류가 나야 한다"));
    assert_eq!(
        (error.kind, error.code, error.message.as_str()),
        (kind, code, message)
    );
}

#[track_caller]
pub fn assert_app_error_message<T: std::fmt::Debug>(
    result: Result<T, ApiError>,
    kind: ErrorKind,
    message: &str,
) {
    let error = app_error(result.expect_err("오류가 나야 한다"));
    assert_eq!((error.kind, error.message.as_str()), (kind, message));
}

// ---------------------------------------------------------------------------------------------
// 외부 경계 가짜
// ---------------------------------------------------------------------------------------------

/// `IdTokenVerifier`를 Mockito `willReturn(...).verify(token, nonce)`처럼 쓰는 가짜.
pub struct StubVerifier {
    provider: SocialProvider,
    answers: Mutex<HashMap<(String, String), VerifiedSocialIdentity>>,
}

impl StubVerifier {
    pub fn new(provider: SocialProvider) -> Arc<StubVerifier> {
        Arc::new(StubVerifier {
            provider,
            answers: Mutex::new(HashMap::new()),
        })
    }

    pub fn will_return(&self, id_token: &str, raw_nonce: &str, identity: VerifiedSocialIdentity) {
        self.answers
            .lock()
            .unwrap()
            .insert((id_token.to_owned(), raw_nonce.to_owned()), identity);
    }
}

impl IdTokenVerifier for StubVerifier {
    fn provider(&self) -> SocialProvider {
        self.provider
    }

    fn verify(&self, id_token: &str, raw_nonce: &str) -> Result<VerifiedSocialIdentity, ApiError> {
        self.answers
            .lock()
            .unwrap()
            .get(&(id_token.to_owned(), raw_nonce.to_owned()))
            .cloned()
            .ok_or_else(|| {
                ApiError::App(AppError::unauthorized(
                    ErrorCode::UNAUTHORIZED,
                    "유효하지 않은 ID Token입니다.",
                ))
            })
    }
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// `AppleTokenClient` 가짜. 교환 결과와 폐기 실패를 정하고 호출을 기록한다.
#[derive(Default)]
pub struct StubAppleClient {
    exchanges: Mutex<HashMap<String, Result<AppleTokenExchangeResult, String>>>,
    revoke_failure: Mutex<Option<String>>,
    on_revoke: Mutex<Option<Hook>>,
    pub exchange_calls: Mutex<Vec<String>>,
    pub revoke_calls: Mutex<Vec<String>>,
}

impl StubAppleClient {
    pub fn new() -> Arc<StubAppleClient> {
        Arc::new(StubAppleClient::default())
    }

    pub fn exchange_returns(&self, code: &str, result: AppleTokenExchangeResult) {
        self.exchanges
            .lock()
            .unwrap()
            .insert(code.to_owned(), Ok(result));
    }

    /// 원본 `IllegalStateException(message)`에 해당하는 실패.
    pub fn exchange_fails(&self, code: &str, message: &str) {
        self.exchanges
            .lock()
            .unwrap()
            .insert(code.to_owned(), Err(message.to_owned()));
    }

    pub fn revoke_fails(&self, message: &str) {
        *self.revoke_failure.lock().unwrap() = Some(message.to_owned());
    }

    /// 폐기 요청이 나가는 동안 실행할 일(동시에 끼어드는 다른 요청).
    pub fn on_revoke(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_revoke.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn exchanged(&self) -> Vec<String> {
        self.exchange_calls.lock().unwrap().clone()
    }

    pub fn revoked(&self) -> Vec<String> {
        self.revoke_calls.lock().unwrap().clone()
    }
}

impl AppleTokenClient for StubAppleClient {
    fn exchange_authorization_code(
        &self,
        authorization_code: &str,
    ) -> Result<AppleTokenExchangeResult, ApiError> {
        self.exchange_calls
            .lock()
            .unwrap()
            .push(authorization_code.to_owned());
        match self.exchanges.lock().unwrap().get(authorization_code) {
            Some(Ok(result)) => Ok(result.clone()),
            Some(Err(message)) => Err(ApiError::Internal(message.clone())),
            None => Ok(AppleTokenExchangeResult {
                id_token: String::new(),
                refresh_token: String::new(),
            }),
        }
    }

    fn revoke_refresh_token(&self, refresh_token: &str) -> Result<(), ApiError> {
        self.revoke_calls
            .lock()
            .unwrap()
            .push(refresh_token.to_owned());
        if let Some(hook) = self.on_revoke.lock().unwrap().as_ref() {
            hook();
        }
        match self.revoke_failure.lock().unwrap().as_ref() {
            Some(message) => Err(ApiError::Internal(message.clone())),
            None => Ok(()),
        }
    }
}

/// `SignatureImageStorage` 가짜. 올라온 이미지와 처리 완료 여부를 정한다.
#[derive(Default)]
pub struct FakeStorage {
    images: Mutex<HashMap<Uuid, StoredImageMetadata>>,
    completions: Mutex<HashMap<Uuid, VecDeque<bool>>>,
    key_overrides: Mutex<HashMap<Uuid, SignatureStorageKeys>>,
    calls: AtomicUsize,
}

pub fn png() -> StoredImageMetadata {
    StoredImageMetadata {
        content_type: Some("image/png".to_owned()),
        byte_size: 1024,
    }
}

pub fn image(content_type: &str, byte_size: i64) -> StoredImageMetadata {
    StoredImageMetadata {
        content_type: Some(content_type.to_owned()),
        byte_size,
    }
}

impl FakeStorage {
    pub fn new() -> Arc<FakeStorage> {
        Arc::new(FakeStorage::default())
    }

    pub fn uploaded(&self, upload_id: Uuid, image: StoredImageMetadata) {
        self.images.lock().unwrap().insert(upload_id, image);
    }

    /// 호출할 때마다 차례로 돌려주고 마지막 값을 유지한다(`willReturn(false, true)`).
    pub fn processing_completed(&self, upload_id: Uuid, answers: &[bool]) {
        self.completions
            .lock()
            .unwrap()
            .insert(upload_id, answers.iter().copied().collect());
    }

    /// `toStorageKeys(uploadId)`가 정해진 키를 돌려주게 한다.
    pub fn storage_keys_for(&self, upload_id: Uuid, keys: SignatureStorageKeys) {
        self.key_overrides.lock().unwrap().insert(upload_id, keys);
    }

    /// 저장소를 건드린 횟수(`verifyNoInteractions` 확인용).
    pub fn interactions(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

pub fn keys_for(upload_id: Uuid) -> SignatureStorageKeys {
    SignatureStorageKeys {
        original: format!("chalkak/signatures/dev/original/{upload_id}.png"),
        thumbnail: format!("chalkak/signatures/dev/thumbnail/{upload_id}.png"),
    }
}

impl SignatureImageStorage for FakeStorage {
    fn find_uploaded_image(
        &self,
        upload_id: Uuid,
    ) -> Result<Option<StoredImageMetadata>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.images.lock().unwrap().get(&upload_id).cloned())
    }

    fn to_staging_storage_key(&self, upload_id: Uuid) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        format!("chalkak/staging/dev/signatures/{upload_id}.png")
    }

    fn to_storage_keys(&self, upload_id: Uuid) -> SignatureStorageKeys {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.key_overrides.lock().unwrap().get(&upload_id) {
            Some(keys) => keys.clone(),
            None => keys_for(upload_id),
        }
    }

    fn is_processing_completed(&self, upload_id: Uuid) -> Result<bool, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut completions = self.completions.lock().unwrap();
        Ok(match completions.get_mut(&upload_id) {
            Some(queue) if queue.len() > 1 => queue.pop_front().unwrap_or(false),
            Some(queue) => queue.front().copied().unwrap_or(false),
            None => false,
        })
    }

    fn to_image_url(&self, storage_key: &str) -> Result<String, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("https://cdn.test.chalkak/{storage_key}"))
    }
}

/// `SignatureImageUploadIssuer` 가짜.
#[derive(Default)]
pub struct FakeUploadIssuer {
    calls: AtomicUsize,
}

pub const PRESIGNED_URL: &str = "https://s3.example.com/presigned";

impl FakeUploadIssuer {
    pub fn new() -> Arc<FakeUploadIssuer> {
        Arc::new(FakeUploadIssuer::default())
    }

    pub fn interactions(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl SignatureImageUploadIssuer for FakeUploadIssuer {
    fn issue(&self, upload_id: Uuid) -> Result<SignatureImageUpload, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(SignatureImageUpload {
            upload_id,
            upload_url: PRESIGNED_URL.to_owned(),
            expires_in_seconds: 300,
        })
    }

    fn issue_processing_upload(
        &self,
        upload_id: Uuid,
    ) -> Result<SignatureProcessingImageUpload, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(SignatureProcessingImageUpload {
            original_upload_url: format!("{PRESIGNED_URL}/original/{upload_id}"),
            thumbnail_upload_url: format!("{PRESIGNED_URL}/thumbnail/{upload_id}"),
            content_type: "image/png".to_owned(),
            cache_control: "public, max-age=86400".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 세계
// ---------------------------------------------------------------------------------------------

pub struct World {
    pub db: TestDb,
    pub pool: Pool,
    pub clock: Arc<FixedClock>,
    pub fingerprint: IdentityFingerprint,
    pub cipher: AppleCipher,
    pub access_tokens: AccessTokenIssuer,
    pub signup_tokens: SignupTokens,
    pub refresh: UserRefreshTokenService,
    pub google: Arc<StubVerifier>,
    pub kakao: Arc<StubVerifier>,
    pub apple_verifier: Arc<StubVerifier>,
    pub apple_client: Arc<StubAppleClient>,
    pub storage: Arc<FakeStorage>,
    pub issuer: Arc<FakeUploadIssuer>,
    pub apple_signup: AppleSignupAuthorizationService,
    pub existing: ExistingSocialAccountLoginProcessor,
    pub login: SocialLoginService,
    pub signup: SocialSignupService,
    pub users: UserService,
    pub withdrawal: UserWithdrawalService,
    pub revoker: AppleSocialConnectionRevoker,
}

pub fn image_policy() -> SignatureImagePolicy {
    SignatureImagePolicy {
        max_bytes: 5 * 1024 * 1024,
        allowed_content_types: vec!["image/png".to_owned()],
    }
}

impl World {
    /// `CHALKAK_TEST_DB_URL`이 없으면 `None`.
    pub fn create() -> Option<World> {
        let db = TestDb::create()?;
        let pool = db.pool(4);
        let clock = Arc::new(FixedClock::new(ts(NOW)));
        let dyn_clock: Arc<dyn chalkak_core::Clock> = clock.clone();
        let access_tokens = AccessTokenIssuer::new(
            ACCESS_KEY.to_vec(),
            ACCESS_AUDIENCE,
            Duration::from_secs(900),
            Arc::clone(&dyn_clock),
        );
        let signup_tokens = SignupTokens::new(
            SIGNUP_KEY.to_vec(),
            SIGNUP_AUDIENCE,
            Duration::from_secs(300),
            Arc::clone(&dyn_clock),
        );
        let refresh = UserRefreshTokenService::new(
            pool.clone(),
            Arc::clone(&dyn_clock),
            RefreshTokenPolicy::new(
                Duration::from_secs(30 * 86400),
                Duration::from_secs(90 * 86400),
            )
            .unwrap(),
            access_tokens.clone(),
        );
        let fingerprint = IdentityFingerprint::new(IDENTITY_KEY.to_vec());
        let cipher = AppleCipher::new(&CIPHER_KEY);

        let google = StubVerifier::new(SocialProvider::Google);
        let kakao = StubVerifier::new(SocialProvider::Kakao);
        let apple_verifier = StubVerifier::new(SocialProvider::Apple);
        let verifier = SocialIdentityVerifier::new(vec![
            google.clone() as Arc<dyn IdTokenVerifier>,
            kakao.clone() as Arc<dyn IdTokenVerifier>,
            apple_verifier.clone() as Arc<dyn IdTokenVerifier>,
        ])
        .unwrap();
        let apple_client = StubAppleClient::new();
        let storage = FakeStorage::new();
        let issuer = FakeUploadIssuer::new();

        let apple_signup = AppleSignupAuthorizationService::new(
            pool.clone(),
            Arc::clone(&dyn_clock),
            fingerprint.clone(),
        );
        let existing = ExistingSocialAccountLoginProcessor::new(
            pool.clone(),
            fingerprint.clone(),
            access_tokens.clone(),
            refresh.clone(),
        );
        let login = SocialLoginService::new(
            verifier.clone(),
            apple_client.clone(),
            cipher.clone(),
            apple_signup.clone(),
            existing.clone(),
        );
        let signup = SocialSignupService::new(SocialSignupDeps {
            pool: pool.clone(),
            clock: Arc::clone(&dyn_clock),
            verifier,
            fingerprint: fingerprint.clone(),
            signup_tokens: signup_tokens.clone(),
            access_tokens: access_tokens.clone(),
            refresh_tokens: refresh.clone(),
            apple_signup: apple_signup.clone(),
            upload_issuer: issuer.clone(),
            storage: storage.clone(),
            image_policy: image_policy(),
        });
        let users = UserService::new(
            pool.clone(),
            Arc::clone(&dyn_clock),
            refresh.clone(),
            storage.clone(),
            issuer.clone(),
            image_policy(),
            SignatureProcessingPolicy::new(Duration::from_secs(7 * 60)).unwrap(),
        );
        let revoker =
            AppleSocialConnectionRevoker::new(pool.clone(), cipher.clone(), apple_client.clone());
        let withdrawal = UserWithdrawalService::new(Arc::new(revoker.clone()), users.clone());

        Some(World {
            db,
            pool,
            clock,
            fingerprint,
            cipher,
            access_tokens,
            signup_tokens,
            refresh,
            google,
            kakao,
            apple_verifier,
            apple_client,
            storage,
            issuer,
            apple_signup,
            existing,
            login,
            signup,
            users,
            withdrawal,
            revoker,
        })
    }

    pub fn now(&self) -> Timestamp {
        use chalkak_core::Clock;
        self.clock.now()
    }

    // ----- 저장소 보조(`UserFixture`, `Repository.save`) -----

    /// 컨트롤러 계층 테스트용 라우터. 인증 필터 없이 핸들러만 실제 서비스와 DB에 붙인다(`@WebMvcTest` +
    /// `addFilters = false`에 해당). 주체는 `security::enter`로 올린다.
    pub fn router(&self) -> Router {
        use chalkak_app::auth::UsableUserPolicy;
        use chalkak_app::wiring::Components;

        let (ctx, logger) = self.app_context();
        let components = Components {
            policy: UsableUserPolicy::new(self.pool.clone()),
            login: self.login.clone(),
            signup: self.signup.clone(),
            refresh: self.refresh.clone(),
            users: self.users.clone(),
            withdrawal: self.withdrawal.clone(),
            callback_auth: self.callback_auth(),
        };
        let routes = Routes::new(logger);
        let routes = chalkak_app::auth::api::register(routes, &ctx, &components);
        chalkak_app::user::api::register(routes, &ctx, &components).into_router()
    }

    /// 알림 수신 설정·푸시 기기 컨트롤러만 붙인 라우터(인증 필터 없음).
    pub fn push_router(&self) -> Router {
        let (ctx, logger) = self.app_context();
        chalkak_app::notification::push_api::register(Routes::new(logger), &ctx).into_router()
    }

    fn app_context(&self) -> (chalkak_app::AppContext, Arc<chalkak_core::log::Logger>) {
        use chalkak_app::AppContext;
        use chalkak_app::config::{AppSettings, TEST_ENV};
        use chalkak_core::Env;
        use chalkak_core::log::{Level, Logger};

        let clock: Arc<dyn chalkak_core::Clock> = self.clock.clone();
        let mut pairs = TEST_ENV.to_vec();
        pairs.push(("S3_PREFIX", "chalkak"));
        let env = Env::from_pairs(pairs);
        let post_settings = chalkak_app::post::PostSettings::from_env(&env, "dev").unwrap();
        let settings = AppSettings::from_env(
            &env,
            &post_settings,
            ACCESS_KEY.to_vec(),
            Duration::from_secs(900),
        )
        .unwrap();
        let logger = Arc::new(Logger::new(
            Box::new(std::io::sink()),
            Level::Error,
            Arc::clone(&clock),
        ));
        let ctx = AppContext {
            pool: self.pool.clone(),
            clock: Arc::clone(&clock),
            logger: Arc::clone(&logger),
            settings: Arc::new(settings),
            admin: chalkak_app::admin::AdminConfig::for_tests(
                b"0123456789abcdef0123456789abcdef".to_vec(),
            ),
            post: chalkak_app::post::PostWiring::new(
                post_settings,
                Arc::new(NoObjects),
                Arc::new(chalkak_app::post::NoopPostModerationPendingListener),
            ),
        };
        (ctx, logger)
    }

    pub fn callback_auth(&self) -> chalkak_app::post::ProcessingCallbackAuthenticator {
        let clock: Arc<dyn chalkak_core::Clock> = self.clock.clone();
        chalkak_app::post::ProcessingCallbackAuthenticator::new(
            "test-callback-secret-with-enough-length",
            "dev",
            clock,
        )
        .unwrap()
    }

    /// `UserFixture.create()`.
    pub fn save_user(&self) -> User {
        let unique = Uuid::new_v4();
        let user = User::create(
            Some(format!("user-{unique}@chalkak.test")),
            &SignatureStorageKeys {
                original: format!("signatures/{unique}"),
                thumbnail: format!("thumbnails/{unique}"),
            },
        )
        .unwrap();
        UserRepository
            .insert(&mut self.db.client(), &user, self.now())
            .unwrap()
    }

    /// `UserFixture.createBanned(null)`.
    pub fn save_banned_user(&self) -> User {
        let unique = Uuid::new_v4();
        let mut user = User::create(
            Some(format!("user-{unique}@chalkak.test")),
            &SignatureStorageKeys {
                original: format!("signatures/{unique}"),
                thumbnail: format!("thumbnails/{unique}"),
            },
        )
        .unwrap();
        user.ban().unwrap();
        UserRepository
            .insert(&mut self.db.client(), &user, self.now())
            .unwrap()
    }

    /// 이미 저장된 회원에게 정지(`BANNED`)를 반영한다(`user.ban()` 뒤 flush).
    pub fn ban(&self, user_id: Uuid) {
        let before = self.find_user(user_id);
        let mut after = before.clone();
        after.ban().unwrap();
        UserRepository
            .update_if_changed(&mut self.db.client(), &before, &after, self.now())
            .unwrap();
    }

    /// 이미 저장된 회원을 서비스를 거치지 않고 탈퇴 처리한다(`user.withdraw()` 뒤 flush).
    pub fn withdraw_directly(&self, user_id: Uuid) {
        let before = self.find_user(user_id);
        let mut after = before.clone();
        after.withdraw(self.now()).unwrap();
        UserRepository
            .update_if_changed(&mut self.db.client(), &before, &after, self.now())
            .unwrap();
    }

    /// 저장된 회원을 고친다(`user.startSignatureProcessing(...)` 뒤 flush).
    pub fn modify_user(&self, user_id: Uuid, change: impl FnOnce(&mut User)) {
        let before = self.find_user(user_id);
        let mut after = before.clone();
        change(&mut after);
        UserRepository
            .update_if_changed(&mut self.db.client(), &before, &after, self.now())
            .unwrap();
    }

    pub fn find_user(&self, user_id: Uuid) -> User {
        UserRepository
            .find_by_id(&mut self.db.client(), user_id)
            .unwrap()
            .expect("회원이 있다")
    }

    pub fn find_active_user(&self, user_id: Uuid) -> Option<User> {
        UserRepository
            .find_active_by_id(&mut self.db.client(), user_id)
            .unwrap()
    }

    pub fn subject_hmac(&self, provider: SocialProvider, subject: &str) -> String {
        self.fingerprint.encode(provider, subject)
    }

    /// `SocialAccount.create(user, provider, hmac)` 저장. 소셜 계정 식별자를 돌려준다.
    pub fn save_social_account(
        &self,
        user_id: Uuid,
        provider: SocialProvider,
        subject: &str,
    ) -> Uuid {
        SocialAccountRepository
            .insert(
                &mut self.db.client(),
                user_id,
                provider,
                &self.subject_hmac(provider, subject),
                self.now(),
            )
            .unwrap()
    }

    pub fn find_social_account(
        &self,
        provider: SocialProvider,
        subject: &str,
    ) -> Option<(chalkak_app::auth::repository::SocialAccount, User)> {
        SocialAccountRepository
            .find_by_provider_and_subject_hmac(
                &mut self.db.client(),
                provider,
                &self.subject_hmac(provider, subject),
            )
            .unwrap()
    }

    /// `AppleAuthorization.create(socialAccount, enc)` 저장.
    pub fn save_apple_authorization(&self, social_account_id: Uuid, encrypted: &str) {
        AppleAuthorizationRepository
            .insert(
                &mut self.db.client(),
                social_account_id,
                encrypted,
                self.now(),
            )
            .unwrap();
    }

    pub fn apple_authorizations(
        &self,
        social_account_id: Uuid,
    ) -> Vec<chalkak_app::auth::repository::AppleAuthorization> {
        AppleAuthorizationRepository
            .find_all_by_social_account_id(&mut self.db.client(), social_account_id)
            .unwrap()
    }

    pub fn save_pending(&self, subject: &str, encrypted: &str, expires_at: Timestamp) {
        PendingAppleAuthorizationRepository
            .insert(
                &mut self.db.client(),
                &self.subject_hmac(SocialProvider::Apple, subject),
                encrypted,
                expires_at,
                self.now(),
            )
            .unwrap();
    }

    /// `findLatestUnexpiredBySubjectHmacForUpdate`.
    pub fn latest_pending(
        &self,
        subject: &str,
    ) -> Option<chalkak_app::auth::repository::PendingAppleAuthorization> {
        let mut client = self.db.client();
        let mut tx = client.transaction().unwrap();
        let found = PendingAppleAuthorizationRepository
            .find_latest_unexpired_for_update(
                &mut tx,
                &self.subject_hmac(SocialProvider::Apple, subject),
                self.now(),
            )
            .unwrap();
        tx.commit().unwrap();
        found
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.db.client().query_one(sql, &[]).unwrap().get(0)
    }

    pub fn pending_count(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM pending_apple_authorizations")
    }

    pub fn stored_refresh_hashes(&self, user_id: Uuid) -> Vec<String> {
        self.db
            .client()
            .query(
                "SELECT token_hash FROM user_refresh_tokens WHERE user_id = $1",
                &[&user_id],
            )
            .unwrap()
            .into_iter()
            .map(|row| row.get(0))
            .collect()
    }

    pub fn live_refresh_token_count(&self, user_id: Uuid) -> i64 {
        self.db
            .client()
            .query_one(
                "SELECT COUNT(*) FROM user_refresh_tokens WHERE user_id = $1 AND revoked_at IS NULL",
                &[&user_id],
            )
            .unwrap()
            .get(0)
    }

    pub fn refresh_token_revoked_at(&self, token: &str) -> Option<Timestamp> {
        self.db
            .client()
            .query_one(
                "SELECT revoked_at FROM user_refresh_tokens WHERE token_hash = $1",
                &[&chalkak_app::auth::tokens::hash_refresh_token(token)],
            )
            .unwrap()
            .get(0)
    }

    pub fn consumed_token_count(&self, jti: &str) -> i64 {
        self.db
            .client()
            .query_one(
                "SELECT COUNT(*) FROM consumed_signup_tokens WHERE jti = $1",
                &[&jti],
            )
            .unwrap()
            .get(0)
    }

    // ----- 신원 -----

    pub fn google_identity(subject: &str, email: Option<&str>) -> VerifiedSocialIdentity {
        VerifiedSocialIdentity {
            provider: SocialProvider::Google,
            subject: subject.to_owned(),
            email: email.map(str::to_owned),
        }
    }

    pub fn apple_identity(subject: &str) -> VerifiedSocialIdentity {
        VerifiedSocialIdentity {
            provider: SocialProvider::Apple,
            subject: subject.to_owned(),
            email: Some("user@privaterelay.appleid.com".to_owned()),
        }
    }

    /// 실제 회원가입 토큰(`SignupTokens`)을 발급한다.
    pub fn signup_token(&self, identity: &VerifiedSocialIdentity, upload_id: Uuid) -> String {
        self.signup_tokens.issue(identity, upload_id).value
    }
}

/// 액세스 토큰의 `sub`(`jwtDecoder().decode(...).getSubject()`).
pub fn access_token_subject(token: &str) -> String {
    let claims = chalkak_jwt::hs256_decode(token, &ACCESS_KEY).expect("액세스 토큰 서명이 맞는다");
    claims.subject().expect("sub가 있다").to_owned()
}

pub fn assert_lower_hex_64(value: &str) {
    assert!(
        value.len() == 64
            && value
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "{value}"
    );
}

/// 컨트롤러 계층 시험에서 게시물 흐름이 S3를 부르지 않는다는 표시.
struct NoObjects;

impl chalkak_app::post::ObjectStore for NoObjects {
    fn object_exists(
        &self,
        _bucket: &str,
        _key: &str,
    ) -> Result<bool, chalkak_app::post::storage::StorageError> {
        Ok(false)
    }

    fn presign_put(
        &self,
        _request: &chalkak_app::post::storage::PresignPut,
    ) -> Result<String, chalkak_app::post::storage::StorageError> {
        Err(chalkak_app::post::storage::StorageError(
            "시험에서는 S3를 부르지 않습니다.".to_owned(),
        ))
    }
}
