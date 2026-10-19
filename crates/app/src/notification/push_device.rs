//! 푸시 기기. 원본 `FcmToken`, `PushDevice`, `PushDeviceRepository`, `PushDeviceService`와 `LoginSessionService`.
//!
//! 기기 행은 로그인(리프레시 토큰 계보, `session_id`) 하나에 하나이고, 같은 FCM 토큰은 한 행에만 붙는다
//! (`ux_push_devices_session`, `ux_push_devices_fcm_token`). 토큰 원문은 잠금·조회에 쓰지 않고 SHA-256 해시를 쓴다.

use std::sync::Arc;

use chalkak_core::Clock;
use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::{Db, DbError, Pool};
use jiff::Timestamp;
use postgres::Row;
use uuid::Uuid;

use crate::auth::repository::RefreshTokenRepository;
use crate::common::{sha256_hex, transact};
use crate::user::UserRepository;
use crate::user::domain::User;
use crate::web::ApiError;
use crate::web::binding::is_java_whitespace;

const TOKEN_REQUIRED: &str = "FCM 토큰이 필요합니다.";

/// `FcmToken`: 원문을 바꾸지 않고 중복 판별용 SHA-256 해시를 함께 든다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FcmToken {
    value: String,
    hash: String,
}

impl FcmToken {
    /// 없거나 공백뿐이면 업무 오류(400 `BUSINESS_ERROR`). 공백은 `String.isBlank()`(Java 공백) 기준이다.
    pub fn new(value: &str) -> Result<FcmToken, AppError> {
        if value.chars().all(is_java_whitespace) {
            return Err(AppError::business(
                ErrorCode::BUSINESS_ERROR,
                TOKEN_REQUIRED,
            ));
        }
        Ok(FcmToken {
            value: value.to_owned(),
            hash: sha256_hex(value.as_bytes()),
        })
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// `PushDevice`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushDevice {
    /// 저장 전에는 `None`(DB 기본값 `uuidv7()`이 채운다).
    pub id: Option<Uuid>,
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub fcm_token: String,
    pub fcm_token_hash: String,
    pub registered_at: Timestamp,
    pub updated_at: Timestamp,
}

impl PushDevice {
    /// 새 기기. 최초 등록 시각과 갱신 시각이 같다.
    pub fn new(
        user_id: Uuid,
        session_id: Uuid,
        token: &FcmToken,
        registered_at: Timestamp,
    ) -> PushDevice {
        let mut device = PushDevice {
            id: None,
            user_id,
            session_id,
            fcm_token: String::new(),
            fcm_token_hash: String::new(),
            registered_at,
            updated_at: registered_at,
        };
        device.update_token(token, registered_at);
        device
    }

    /// 토큰과 갱신 시각을 바꾼다. 최초 등록 시각은 유지한다.
    pub fn update_token(&mut self, token: &FcmToken, updated_at: Timestamp) {
        self.fcm_token = token.value().to_owned();
        self.fcm_token_hash = token.hash().to_owned();
        self.updated_at = updated_at;
    }
}

const COLUMNS: &str =
    "id, user_id, session_id, fcm_token, fcm_token_hash, registered_at, updated_at";

fn device_from_row(row: &Row) -> Result<PushDevice, DbError> {
    Ok(PushDevice {
        id: Some(row.try_get("id")?),
        user_id: row.try_get("user_id")?,
        session_id: row.try_get("session_id")?,
        fcm_token: row.try_get("fcm_token")?,
        fcm_token_hash: row.try_get("fcm_token_hash")?,
        registered_at: row.try_get("registered_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// `PushDeviceRepository`(`PushDeviceJpaRepository`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PushDeviceRepository;

impl PushDeviceRepository {
    /// `lockToken`: 같은 토큰의 동시 등록을 줄 세운다(트랜잭션이 끝날 때까지 유지). 회차 잠금(단일 키)과 다른
    /// 네임스페이스(1)를 쓰고, 토큰 원문은 잠금에 쓰지 않는다.
    pub fn lock_token(&self, db: &mut impl Db, token_hash: &str) -> Result<(), DbError> {
        db.query_one(
            "SELECT pg_advisory_xact_lock(1, hashtext($1))",
            &[&token_hash],
        )?;
        Ok(())
    }

    pub fn find_session_id_by_token_hash(
        &self,
        db: &mut impl Db,
        token_hash: &str,
    ) -> Result<Option<Uuid>, DbError> {
        db.query_opt(
            "SELECT session_id FROM push_devices WHERE fcm_token_hash = $1",
            &[&token_hash],
        )?
        .map(|row| row.try_get(0).map_err(DbError::from))
        .transpose()
    }

    pub fn find_by_session_id(
        &self,
        db: &mut impl Db,
        session_id: Uuid,
    ) -> Result<Option<PushDevice>, DbError> {
        db.query_opt(
            &format!("SELECT {COLUMNS} FROM push_devices WHERE session_id = $1"),
            &[&session_id],
        )?
        .map(|row| device_from_row(&row))
        .transpose()
    }

    /// `deleteBySessionId`: 로그인 하나에 연결된 기기를 지운다.
    pub fn delete_by_session_id(&self, db: &mut impl Db, session_id: Uuid) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM push_devices WHERE session_id = $1",
            &[&session_id],
        )?)
    }

    /// `deleteByUserId`: 한 회원의 기기를 모두 지운다.
    pub fn delete_by_user_id(&self, db: &mut impl Db, user_id: Uuid) -> Result<u64, DbError> {
        Ok(db.execute("DELETE FROM push_devices WHERE user_id = $1", &[&user_id])?)
    }

    /// `deleteOtherSessionByTokenHash`: 같은 토큰을 든 다른 로그인의 기기를 지운다.
    pub fn delete_other_session_by_token_hash(
        &self,
        db: &mut impl Db,
        token_hash: &str,
        session_id: Uuid,
    ) -> Result<u64, DbError> {
        Ok(db.execute(
            "DELETE FROM push_devices WHERE fcm_token_hash = $1 AND session_id <> $2",
            &[&token_hash, &session_id],
        )?)
    }

    /// `save`(새 기기). 식별자는 DB 기본값이 만든다.
    pub fn insert(&self, db: &mut impl Db, device: &PushDevice) -> Result<Uuid, DbError> {
        let row = db.query_one(
            "INSERT INTO push_devices (user_id, session_id, fcm_token, fcm_token_hash, \
             registered_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
            &[
                &device.user_id,
                &device.session_id,
                &device.fcm_token,
                &device.fcm_token_hash,
                &device.registered_at,
                &device.updated_at,
            ],
        )?;
        Ok(row.try_get(0)?)
    }

    /// 읽은 시점(`before`)과 달라졌을 때만 `UPDATE`한다(Hibernate의 더티 체킹). 갱신했으면 `true`.
    pub fn update_if_changed(
        &self,
        db: &mut impl Db,
        before: &PushDevice,
        after: &PushDevice,
    ) -> Result<bool, DbError> {
        if before == after {
            return Ok(false);
        }
        db.execute(
            "UPDATE push_devices SET fcm_token = $2, fcm_token_hash = $3, updated_at = $4 \
             WHERE id = $1",
            &[
                &after.id,
                &after.fcm_token,
                &after.fcm_token_hash,
                &after.updated_at,
            ],
        )?;
        Ok(true)
    }
}

fn reauthentication_required() -> ApiError {
    ApiError::App(AppError::unauthorized(
        ErrorCode::REAUTHENTICATION_REQUIRED,
        "다시 로그인해 주세요.",
    ))
}

/// `LoginSessionService.lockSessions`: 전체 폐기와 같은 UUID 문자열 순서로 잠그고 트랜잭션이 끝날 때까지 쥔다.
pub fn lock_sessions(db: &mut impl Db, session_ids: &[Uuid]) -> Result<(), DbError> {
    let mut sorted: Vec<String> = session_ids.iter().map(Uuid::to_string).collect();
    sorted.sort();
    sorted.dedup();
    for id in sorted {
        let id: Uuid = id.parse().expect("방금 만든 UUID 문자열");
        RefreshTokenRepository.lock_session(db, id)?;
    }
    Ok(())
}

/// `LoginSessionService.getUsableUser`: 회원이 있고 탈퇴하지 않았으며 로그인에 쓸 수 있는 토큰이 있어야 한다.
/// 회원이 없거나 로그인이 쓸 수 없으면 재로그인(401 `REAUTHENTICATION_REQUIRED`), 탈퇴했으면 401 `UNAUTHORIZED`.
pub fn get_usable_user(
    db: &mut impl Db,
    user_id: Uuid,
    session_id: Uuid,
    now: Timestamp,
) -> Result<User, ApiError> {
    let user = UserRepository
        .find_by_id(db, user_id)?
        .ok_or_else(reauthentication_required)?;
    user.validate_not_withdrawn()?;
    if !RefreshTokenRepository
        .exists_usable_by_session_id_and_user_id(db, session_id, user_id, now)?
    {
        return Err(reauthentication_required());
    }
    Ok(user)
}

/// `PushDeviceService`.
#[derive(Clone)]
pub struct PushDeviceService {
    pool: Pool,
    clock: Arc<dyn Clock>,
    repository: PushDeviceRepository,
}

impl PushDeviceService {
    pub fn new(pool: Pool, clock: Arc<dyn Clock>) -> PushDeviceService {
        PushDeviceService {
            pool,
            clock,
            repository: PushDeviceRepository,
        }
    }

    /// `register`: 로그인 하나에 기기 하나를 등록하거나 토큰을 갱신한다. 트랜잭션 하나로 처리한다.
    pub fn register(
        &self,
        user_id: Option<Uuid>,
        session_id: Option<Uuid>,
        fcm_token: &str,
    ) -> Result<(), ApiError> {
        transact(&self.pool, |tx| {
            self.register_in(tx, user_id, session_id, fcm_token)
        })
    }

    /// [`register`](Self::register)의 본문. 호출자의 트랜잭션에 참여한다.
    ///
    /// 잠금 순서: 토큰 -> 로그인(UUID 문자열 순서). 서로의 토큰으로 바꾸는 두 요청도 같은 순서로 잠가 교착이 없다.
    pub fn register_in(
        &self,
        db: &mut impl Db,
        user_id: Option<Uuid>,
        session_id: Option<Uuid>,
        fcm_token: &str,
    ) -> Result<(), ApiError> {
        let (Some(user_id), Some(session_id)) = (user_id, session_id) else {
            return Err(reauthentication_required());
        };
        let token = FcmToken::new(fcm_token)?;
        self.repository.lock_token(db, token.hash())?;
        let mut session_ids = vec![session_id];
        if let Some(holder) = self
            .repository
            .find_session_id_by_token_hash(db, token.hash())?
        {
            session_ids.push(holder);
        }
        lock_sessions(db, &session_ids)?;

        let now = self.clock.now();
        let user = get_usable_user(db, user_id, session_id, now)?;

        self.repository
            .delete_other_session_by_token_hash(db, token.hash(), session_id)?;
        match self.repository.find_by_session_id(db, session_id)? {
            Some(before) => {
                let mut after = before.clone();
                after.update_token(&token, now);
                self.repository.update_if_changed(db, &before, &after)?;
            }
            None => {
                let device = PushDevice::new(user.id, session_id, &token, now);
                self.repository.insert(db, &device)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // FcmTokenTest#constructor_missingToken_rejectsRegistration
    #[test]
    fn 빈_토큰이나_공백뿐인_토큰은_등록할_수_없다() {
        for blank in ["", " ", "   ", "\t\n", "\u{2003}"] {
            let error = FcmToken::new(blank).unwrap_err();
            assert_eq!(error.code, ErrorCode::BUSINESS_ERROR, "{blank:?}");
            assert_eq!(error.message, "FCM 토큰이 필요합니다.");
        }
    }

    // FcmTokenTest#constructor_validToken_preservesOpaqueValueAndCreatesHash
    #[test]
    fn 유효한_토큰은_원문을_바꾸지_않고_해시를_만든다() {
        let raw = "  token:with/special+chars==  ";
        let token = FcmToken::new(raw).unwrap();
        assert_eq!(token.value(), raw);
        assert_eq!(token.hash(), sha256_hex(raw.as_bytes()));
        assert_eq!(token.hash().len(), 64);
    }

    // PushDeviceTest#updateToken_newToken_preservesFirstRegistrationTime
    #[test]
    fn 토큰을_갱신해도_최초_등록_시각은_유지한다() {
        let registered: Timestamp = "2026-09-30T00:00:00Z".parse().unwrap();
        let updated: Timestamp = "2026-09-30T00:01:00Z".parse().unwrap();
        let first = FcmToken::new("first").unwrap();
        let second = FcmToken::new("second").unwrap();
        let mut device = PushDevice::new(Uuid::new_v4(), Uuid::new_v4(), &first, registered);

        device.update_token(&second, updated);

        assert_eq!(device.registered_at, registered);
        assert_eq!(device.updated_at, updated);
        assert_eq!(device.fcm_token, "second");
        assert_eq!(device.fcm_token_hash, second.hash());
    }
}
