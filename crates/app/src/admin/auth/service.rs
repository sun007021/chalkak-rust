//! 원본 `AdminAuthenticationService`와 `AdminRefreshTokenService`(공통 `RefreshTokenService`).

use chalkak_core::error::{AppError, ErrorCode};
use jiff::Timestamp;
use uuid::Uuid;

use super::domain::{
    DUMMY_PASSWORD_HASH, RefreshTokenPolicy, generate_refresh_token, hash_refresh_token,
    password_matches,
};
use super::repository::{AdminRefreshTokenRepository, AdminRepository, NewRefreshToken};
use crate::admin::config::AdminConfig;
use crate::admin::support::{AdminDeps, in_transaction};
use crate::web::ApiError;

const AUTHENTICATION_FAILURE_MESSAGE: &str = "아이디 또는 비밀번호가 올바르지 않습니다.";
const REAUTHENTICATION_REQUIRED_MESSAGE: &str = "다시 로그인해 주세요.";

/// 발급된 액세스 토큰(`IssuedAccessToken`).
#[derive(Clone, Debug)]
pub struct IssuedAccessToken {
    pub value: String,
    pub expires_in_secs: u64,
}

/// 발급된 리프레시 토큰(`IssuedRefreshToken`). 평문은 여기서만 나간다.
#[derive(Clone, Debug)]
pub struct IssuedRefreshToken {
    pub value: String,
    pub expires_in_secs: i64,
}

#[derive(Clone, Debug)]
pub struct AdminLoginResult {
    pub admin_id: Uuid,
    pub username: String,
    pub access_token: IssuedAccessToken,
    pub refresh_token: IssuedRefreshToken,
}

#[derive(Clone, Debug)]
pub struct TokenRefreshResult {
    pub access_token: IssuedAccessToken,
    pub refresh_token: IssuedRefreshToken,
}

#[derive(Clone, Debug)]
pub struct CurrentAdminResult {
    pub admin_id: Uuid,
    pub username: String,
}

/// `AccessTokenIssuer.issue(subject, ADMIN)`
pub fn issue_admin_access_token(
    config: &AdminConfig,
    admin_id: Uuid,
    now: Timestamp,
) -> IssuedAccessToken {
    let issued_at = now.as_second();
    let expires_at = issued_at.saturating_add_unsigned(config.access_token_expiration.as_secs());
    let subject = admin_id.hyphenated().to_string();
    let jti = Uuid::new_v4().hyphenated().to_string();
    let claims = chalkak_jwt::original::AccessTokenClaims {
        issuer: &config.access_token_issuer,
        audience: &config.access_token_audience,
        subject: &subject,
        issued_at,
        expires_at,
        jti: &jti,
        scope: "ADMIN",
        session_id: None,
    };
    let value = chalkak_jwt::hs256_encode(
        &chalkak_jwt::original::access_token_header(),
        &claims.to_value(),
        &config.access_token_key,
    );
    IssuedAccessToken {
        value,
        expires_in_secs: config.access_token_expiration.as_secs(),
    }
}

fn authentication_failure() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::UNAUTHORIZED,
        AUTHENTICATION_FAILURE_MESSAGE,
    ))
}

fn reauthentication_required() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::REAUTHENTICATION_REQUIRED,
        REAUTHENTICATION_REQUIRED_MESSAGE,
    ))
}

fn policy(config: &AdminConfig) -> Result<RefreshTokenPolicy, ApiError> {
    RefreshTokenPolicy::new(
        config.refresh_token_inactivity,
        config.refresh_token_absolute,
    )
    .map_err(ApiError::Internal)
}

fn seconds_between(from: Timestamp, to: Timestamp) -> i64 {
    to.duration_since(from).as_secs()
}

#[derive(Clone)]
pub struct AdminAuthenticationService {
    deps: AdminDeps,
}

impl AdminAuthenticationService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminAuthenticationService { deps }
    }

    /// `login`: 계정이 없어도 더미 해시로 BCrypt 비교를 한 번 수행해 응답 시간 차이를 줄인다.
    pub fn login(&self, username: &str, password: &str) -> Result<AdminLoginResult, ApiError> {
        let mut conn = self.deps.pool.get()?;
        let now = self.deps.now();
        in_transaction(&mut conn, |tx| {
            let found = AdminRepository.find_by_username(tx, username)?;
            let hash = found
                .as_ref()
                .map_or(DUMMY_PASSWORD_HASH, |a| a.password_hash.as_str());
            let matches = password_matches(password, hash);
            let Some(admin) = found.filter(|_| matches) else {
                return Err(authentication_failure());
            };
            let admin_id = admin.id.ok_or_else(authentication_failure)?;
            let access_token = issue_admin_access_token(&self.deps.config, admin_id, now);
            let refresh_token = self.issue_refresh_token(tx, admin_id, now)?;
            Ok(AdminLoginResult {
                admin_id,
                username: admin.username,
                access_token,
                refresh_token,
            })
        })
    }

    /// `issue`: 새 회전 계보를 시작한다.
    fn issue_refresh_token(
        &self,
        tx: &mut chalkak_db::Tx<'_>,
        admin_id: Uuid,
        now: Timestamp,
    ) -> Result<IssuedRefreshToken, ApiError> {
        let policy = policy(&self.deps.config)?;
        let absolute_expires_at = policy.absolute_expires_at(now);
        let expires_at = policy.next_expires_at(now, absolute_expires_at);
        let generated = generate_refresh_token();
        AdminRefreshTokenRepository.save(
            tx,
            &NewRefreshToken {
                admin_id,
                session_id: Uuid::new_v4(),
                token_hash: generated.token_hash,
                expires_at,
                absolute_expires_at,
                created_at: now,
            },
        )?;
        Ok(IssuedRefreshToken {
            value: generated.value,
            expires_in_secs: seconds_between(now, expires_at),
        })
    }

    pub fn get_current_admin(&self, admin_id: Uuid) -> Result<CurrentAdminResult, ApiError> {
        let mut conn = self.deps.pool.get()?;
        let admin = AdminRepository
            .find_by_id(&mut conn, admin_id)?
            .ok_or_else(authentication_failure)?;
        Ok(CurrentAdminResult {
            admin_id,
            username: admin.username,
        })
    }
}

/// 계보를 잠근 뒤 판단한 결과. 재사용·만료로 계보를 폐기한 경우도 커밋은 해야 하므로 오류가 아니라 값으로 돌려준다.
enum RefreshOutcome {
    Rotated(TokenRefreshResult),
    ReauthenticationRequired,
}

#[derive(Clone)]
pub struct AdminRefreshTokenService {
    deps: AdminDeps,
}

impl AdminRefreshTokenService {
    pub fn new(deps: AdminDeps) -> Self {
        AdminRefreshTokenService { deps }
    }

    /// `refresh`(`noRollbackFor = UnauthorizedException`): 폐기까지 커밋한 뒤에 401을 돌려준다.
    pub fn refresh(&self, presented: &str) -> Result<TokenRefreshResult, ApiError> {
        let policy = policy(&self.deps.config)?;
        let mut conn = self.deps.pool.get()?;
        let token_hash = hash_refresh_token(presented);
        let outcome = in_transaction(&mut conn, |tx| {
            let repo = AdminRefreshTokenRepository;
            let Some(consumed) = lock_lineage_and_read(tx, &token_hash)? else {
                return Ok(RefreshOutcome::ReauthenticationRequired);
            };
            let now = self.deps.now();
            if consumed.is_revoked() {
                return Ok(RefreshOutcome::ReauthenticationRequired);
            }
            if consumed.is_reused() || consumed.is_expired(now) {
                repo.revoke_session(tx, consumed.session_id, now)?;
                return Ok(RefreshOutcome::ReauthenticationRequired);
            }
            // rotate
            let generated = generate_refresh_token();
            let expires_at = policy.next_expires_at(now, consumed.absolute_expires_at);
            repo.mark_rotated(tx, consumed.id, now)?;
            repo.save(
                tx,
                &NewRefreshToken {
                    admin_id: consumed.admin_id,
                    session_id: consumed.session_id,
                    token_hash: generated.token_hash,
                    expires_at,
                    absolute_expires_at: consumed.absolute_expires_at,
                    created_at: now,
                },
            )?;
            Ok(RefreshOutcome::Rotated(TokenRefreshResult {
                access_token: issue_admin_access_token(&self.deps.config, consumed.admin_id, now),
                refresh_token: IssuedRefreshToken {
                    value: generated.value,
                    expires_in_secs: seconds_between(now, expires_at),
                },
            }))
        })?;
        match outcome {
            RefreshOutcome::Rotated(result) => Ok(result),
            RefreshOutcome::ReauthenticationRequired => Err(reauthentication_required()),
        }
    }

    /// `logout`: 모르는 토큰이나 이미 폐기된 토큰도 성공으로 다룬다.
    pub fn logout(&self, presented: &str) -> Result<(), ApiError> {
        let mut conn = self.deps.pool.get()?;
        let token_hash = hash_refresh_token(presented);
        in_transaction(&mut conn, |tx| {
            if let Some(token) = lock_lineage_and_read(tx, &token_hash)?
                && !token.is_revoked()
            {
                AdminRefreshTokenRepository.revoke_session(
                    tx,
                    token.session_id,
                    self.deps.now(),
                )?;
            }
            Ok(())
        })
    }
}

/// 계보 식별자를 읽고(잠금 없이) 잠근 뒤, 잠금 아래에서 다시 읽는다.
fn lock_lineage_and_read(
    tx: &mut chalkak_db::Tx<'_>,
    token_hash: &str,
) -> Result<Option<super::domain::RefreshTokenRecord>, ApiError> {
    let repo = AdminRefreshTokenRepository;
    let Some(session_id) = repo.find_session_id_by_token_hash(tx, token_hash)? else {
        return Ok(None);
    };
    repo.lock_session(tx, session_id)?;
    Ok(repo.find_by_token_hash(tx, token_hash)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testing::{ACCESS_KEY, deps, deps_at, insert_admin};
    use crate::testing::TestDb;

    const PASSWORD: &str = "operator-password";

    fn login_failure(e: ApiError) {
        assert_eq!(e.status(), 401);
        assert_eq!(e.message(), AUTHENTICATION_FAILURE_MESSAGE);
        assert_eq!(e.code().as_str(), "UNAUTHORIZED");
    }

    // AdminAuthenticationServiceTest
    #[test]
    fn 로그인하면_관리자_토큰과_리프레시_토큰을_준다() {
        let Some(db) = TestDb::create() else { return };
        let admin = insert_admin(&mut db.client(), "operator");
        let service = AdminAuthenticationService::new(deps(&db));
        let result = service.login("operator", PASSWORD).unwrap();
        assert_eq!(result.admin_id, admin);
        assert_eq!(result.username, "operator");
        assert_eq!(result.access_token.expires_in_secs, 900);
        assert_eq!(result.refresh_token.expires_in_secs, 30 * 86_400);

        let claims = chalkak_jwt::hs256_decode(&result.access_token.value, ACCESS_KEY).unwrap();
        assert_eq!(claims.get_str("sub"), Some(admin.to_string().as_str()));
        assert_eq!(claims.get_str("scope"), Some("ADMIN"));
        assert_eq!(claims.get_str("purpose"), Some("ACCESS"));
        assert_eq!(claims.get_str("iss"), Some("chalkak-backend"));
        assert_eq!(claims.get_str("aud"), Some("chalkak-access"));

        let stored: String = db
            .client()
            .query_one("SELECT token_hash FROM admin_refresh_tokens", &[])
            .unwrap()
            .get(0);
        assert_eq!(stored, hash_refresh_token(&result.refresh_token.value));
    }

    #[test]
    fn 비밀번호가_틀리거나_계정이_없으면_같은_401이다() {
        let Some(db) = TestDb::create() else { return };
        insert_admin(&mut db.client(), "operator");
        let service = AdminAuthenticationService::new(deps(&db));
        login_failure(service.login("operator", "wrong").unwrap_err());
        login_failure(service.login("nobody", PASSWORD).unwrap_err());
        login_failure(service.login("operator", &"p".repeat(73)).unwrap_err());
        let rows: i64 = db
            .client()
            .query_one("SELECT COUNT(*) FROM admin_refresh_tokens", &[])
            .unwrap()
            .get(0);
        assert_eq!(rows, 0);
    }

    #[test]
    fn 현재_관리자는_식별자로_찾고_없으면_401이다() {
        let Some(db) = TestDb::create() else { return };
        let admin = insert_admin(&mut db.client(), "operator");
        let service = AdminAuthenticationService::new(deps(&db));
        let me = service.get_current_admin(admin).unwrap();
        assert_eq!((me.admin_id, me.username.as_str()), (admin, "operator"));
        login_failure(service.get_current_admin(Uuid::new_v4()).unwrap_err());
    }

    fn reauth(e: ApiError) {
        assert_eq!(e.status(), 401);
        assert_eq!(e.message(), REAUTHENTICATION_REQUIRED_MESSAGE);
        assert_eq!(e.code().as_str(), "REAUTHENTICATION_REQUIRED");
    }

    // AdminRefreshTokenServiceTest: 회전
    #[test]
    fn 재발급하면_토큰이_회전하고_이전_토큰은_쓸_수_없다() {
        let Some(db) = TestDb::create() else { return };
        insert_admin(&mut db.client(), "operator");
        let login = AdminAuthenticationService::new(deps(&db))
            .login("operator", PASSWORD)
            .unwrap();
        let later = deps_at(&db, "2026-10-12T02:00:00Z");
        let refresh = AdminRefreshTokenService::new(later);
        let next = refresh.refresh(&login.refresh_token.value).unwrap();
        assert_ne!(next.refresh_token.value, login.refresh_token.value);
        assert_eq!(next.refresh_token.expires_in_secs, 30 * 86_400);
        let mut client = db.client();
        let rows = client
            .query(
                "SELECT rotated_at IS NOT NULL, revoked_at IS NOT NULL, session_id FROM admin_refresh_tokens ORDER BY created_at, id",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].get::<_, bool>(0));
        assert!(!rows[1].get::<_, bool>(0));
        assert_eq!(rows[0].get::<_, Uuid>(2), rows[1].get::<_, Uuid>(2));

        // 회전된 토큰을 다시 내면 재사용이다: 계보 전체가 폐기되고 401. 폐기는 커밋된다.
        reauth(refresh.refresh(&login.refresh_token.value).unwrap_err());
        let live: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM admin_refresh_tokens WHERE revoked_at IS NULL",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(live, 0, "재사용 탐지로 폐기한 결과가 롤백되면 안 된다");
        reauth(refresh.refresh(&next.refresh_token.value).unwrap_err());
    }

    #[test]
    fn 모르는_토큰이거나_만료된_토큰은_401이고_만료는_계보를_폐기한다() {
        let Some(db) = TestDb::create() else { return };
        insert_admin(&mut db.client(), "operator");
        let login = AdminAuthenticationService::new(deps(&db))
            .login("operator", PASSWORD)
            .unwrap();
        let service_now = AdminRefreshTokenService::new(deps(&db));
        reauth(service_now.refresh("unknown-token").unwrap_err());

        let expired = AdminRefreshTokenService::new(deps_at(&db, "2026-12-01T00:00:00Z"));
        reauth(expired.refresh(&login.refresh_token.value).unwrap_err());
        let live: i64 = db
            .client()
            .query_one(
                "SELECT COUNT(*) FROM admin_refresh_tokens WHERE revoked_at IS NULL",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(live, 0);
        reauth(service_now.refresh(&login.refresh_token.value).unwrap_err());
    }

    // 로그아웃
    #[test]
    fn 로그아웃은_계보를_폐기하고_모르는_토큰에도_성공한다() {
        let Some(db) = TestDb::create() else { return };
        insert_admin(&mut db.client(), "operator");
        let login = AdminAuthenticationService::new(deps(&db))
            .login("operator", PASSWORD)
            .unwrap();
        let service = AdminRefreshTokenService::new(deps(&db));
        service.logout(&login.refresh_token.value).unwrap();
        service.logout(&login.refresh_token.value).unwrap();
        service.logout("unknown-token").unwrap();
        reauth(service.refresh(&login.refresh_token.value).unwrap_err());
        let revoked: i64 = db
            .client()
            .query_one(
                "SELECT COUNT(*) FROM admin_refresh_tokens WHERE revoked_at IS NOT NULL",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(revoked, 1);
    }

    #[test]
    fn 기기마다_계보가_나뉜다() {
        let Some(db) = TestDb::create() else { return };
        insert_admin(&mut db.client(), "operator");
        let auth = AdminAuthenticationService::new(deps(&db));
        let first = auth.login("operator", PASSWORD).unwrap();
        let second = auth.login("operator", PASSWORD).unwrap();
        let service = AdminRefreshTokenService::new(deps(&db));
        service.logout(&first.refresh_token.value).unwrap();
        assert!(service.refresh(&second.refresh_token.value).is_ok());
    }
}
