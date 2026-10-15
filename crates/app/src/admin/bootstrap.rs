//! 시작 시 관리자 계정 보장(원본 `AdminAccountBootstrap`, `DevelopmentAdminBootstrap`).
//!
//! - 개발 우회가 꺼져 있으면 설정의 계정(`ADMIN_USERNAME`, `ADMIN_PASSWORD_HASH`)이 없을 때만 만든다.
//! - 개발 우회가 켜져 있으면 로그인할 수 없는 `dev-admin` 계정을 하나 둔다(비밀번호는 아무도 모르는 난수).

use chalkak_db::Pool;
use uuid::Uuid;

use super::auth::domain::{Admin, encode_password};
use super::auth::repository::AdminRepository;
use super::config::AdminConfig;
use super::support::DEVELOPMENT_ADMIN_USERNAME;

/// 설정으로 받은 관리자 계정(`AdminAccountProperties`).
#[derive(Clone, Debug)]
pub struct AdminAccount {
    pub username: String,
    pub password_hash: String,
}

/// 계정이 없으면 만든다. 설정이 올바르지 않거나 DB 오류면 시작 실패로 알린다.
pub fn ensure_admin_account(
    pool: &Pool,
    config: &AdminConfig,
    account: Option<&AdminAccount>,
) -> Result<(), String> {
    let admin = if config.development_bypass {
        let password = Uuid::new_v4().to_string();
        let hash = encode_password(&password).map_err(|e| e.message)?;
        Admin::create(DEVELOPMENT_ADMIN_USERNAME, &hash).map_err(|e| e.message)?
    } else {
        let Some(account) = account else {
            return Ok(());
        };
        Admin::create(&account.username, &account.password_hash).map_err(|e| e.message)?
    };
    let mut conn = pool.get().map_err(|e| e.to_string())?;
    let repo = AdminRepository;
    if repo
        .find_by_username(&mut conn, &admin.username)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Ok(());
    }
    let mut admin = admin;
    repo.save(&mut conn, &mut admin).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testing::ACCESS_KEY;
    use crate::testing::TestDb;

    const HASH: &str = "$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6";

    fn account(username: &str, hash: &str) -> AdminAccount {
        AdminAccount {
            username: username.to_owned(),
            password_hash: hash.to_owned(),
        }
    }

    // AdminAccountBootstrapTest
    #[test]
    fn 설정한_계정이_없으면_만들고_있으면_그대로_둔다() {
        let Some(db) = TestDb::create() else { return };
        let pool = db.pool(2);
        let config = AdminConfig::for_tests(ACCESS_KEY.to_vec());
        ensure_admin_account(&pool, &config, Some(&account("operator", HASH))).unwrap();
        ensure_admin_account(&pool, &config, Some(&account("operator", HASH))).unwrap();
        let mut client = db.client();
        let count: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM admins WHERE username = 'operator'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(count, 1);
        let hash: String = client
            .query_one(
                "SELECT password FROM admins WHERE username = 'operator'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(hash, HASH);
    }

    // AdminLocalConfigurationTest#localConfiguration_invalidPasswordHash_rejectsStartup
    // (빈 값, 평문, 예시 문구)
    #[test]
    fn 계정_정보가_틀리면_시작을_막는다() {
        let Some(db) = TestDb::create() else { return };
        let pool = db.pool(2);
        let config = AdminConfig::for_tests(ACCESS_KEY.to_vec());
        for bad in [
            account("", HASH),
            account("local-operator", ""),
            account("local-operator", "plain-test-password"),
            account("local-operator", "REPLACE_WITH_BCRYPT_HASH"),
            account("operator", "plain"),
            account(&"a".repeat(101), HASH),
        ] {
            assert!(ensure_admin_account(&pool, &config, Some(&bad)).is_err());
        }
        assert!(ensure_admin_account(&pool, &config, None).is_ok());
    }

    // DevelopmentAdminBootstrapTest
    #[test]
    fn 개발_우회에서는_로그인할_수_없는_계정을_둔다() {
        let Some(db) = TestDb::create() else { return };
        let pool = db.pool(2);
        let mut config = AdminConfig::for_tests(ACCESS_KEY.to_vec());
        config.development_bypass = true;
        ensure_admin_account(&pool, &config, None).unwrap();
        ensure_admin_account(&pool, &config, None).unwrap();
        let mut client = db.client();
        let rows = client
            .query(
                "SELECT password FROM admins WHERE username = 'dev-admin'",
                &[],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        let hash: String = rows[0].get(0);
        assert!(hash.starts_with("$2a$10$"));
    }
}
