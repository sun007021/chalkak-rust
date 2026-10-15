//! 탈퇴 시 Apple 연결 폐기. 원본 `AppleSocialConnectionRevoker`, `AppleAuthorizationService`.
//!
//! DB 조회는 짧게 끊고 Apple HTTP 호출은 연결을 놓은 채로 한다. Apple은 이미 폐기된 리프레시 토큰의 재폐기도 성공으로
//! 처리하므로 중간에 실패해도 다시 시도할 수 있다.

use std::sync::Arc;

use chalkak_db::Pool;
use uuid::Uuid;

use super::apple::AppleTokenClient;
use super::repository::{AppleAuthorizationRepository, SocialAccountRepository};
use super::tokens::{AppleCipher, apple_authorization_fingerprint};
use crate::user::service::{SocialConnectionRevocationSnapshot, SocialConnectionRevoker};
use crate::web::ApiError;

#[derive(Clone)]
pub struct AppleSocialConnectionRevoker {
    pool: Pool,
    social_accounts: SocialAccountRepository,
    apple: AppleAuthorizationRepository,
    cipher: AppleCipher,
    client: Arc<dyn AppleTokenClient>,
}

impl AppleSocialConnectionRevoker {
    pub fn new(
        pool: Pool,
        cipher: AppleCipher,
        client: Arc<dyn AppleTokenClient>,
    ) -> AppleSocialConnectionRevoker {
        AppleSocialConnectionRevoker {
            pool,
            social_accounts: SocialAccountRepository,
            apple: AppleAuthorizationRepository,
            cipher,
            client,
        }
    }
}

impl SocialConnectionRevoker for AppleSocialConnectionRevoker {
    fn revoke_all(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<SocialConnectionRevocationSnapshot>, ApiError> {
        let authorizations = {
            let mut conn = self.pool.get()?;
            match self
                .social_accounts
                .find_id_by_user_id(&mut conn, user_id)?
            {
                Some(social_account_id) => self
                    .apple
                    .find_all_by_social_account_id(&mut conn, social_account_id)?,
                None => Vec::new(),
            }
        };
        for authorization in &authorizations {
            let refresh_token = self
                .cipher
                .decrypt(&authorization.encrypted_refresh_token)
                .map_err(ApiError::Internal)?;
            self.client.revoke_refresh_token(&refresh_token)?;
        }
        Ok(authorizations
            .into_iter()
            .map(|a| SocialConnectionRevocationSnapshot {
                authorization_id: a.id,
                credential_fingerprint: apple_authorization_fingerprint(&a.encrypted_refresh_token),
            })
            .collect())
    }
}
