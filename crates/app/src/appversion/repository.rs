//! 원본 `AppVersionRepositoryImpl`·`AppVersionJpaRepository`: 플랫폼별 설정 조회·잠금·저장.

use chalkak_core::error::{AppError, ErrorCode};
use chalkak_db::{Db, DbError};
use jiff::Timestamp;
use postgres::Row;

use super::domain::{AppPlatform, AppVersion, MinimumSupportedVersion, StoreUrl};

pub const STATE_CHANGED_MESSAGE: &str = "설정이 변경되었습니다. 다시 조회한 뒤 저장해 주세요.";

const COLUMNS: &str = "id, platform, minimum_supported_version, store_url, revision, \
                       last_changed_by, change_reason, created_at, updated_at";

/// 저장 결과. 낙관적 잠금 충돌은 업무 오류로 바꾸고 그 밖의 DB 오류는 그대로 전파한다.
#[derive(Debug)]
pub enum SaveError {
    Business(AppError),
    Db(DbError),
}

impl From<DbError> for SaveError {
    fn from(e: DbError) -> Self {
        SaveError::Db(e)
    }
}

impl From<postgres::Error> for SaveError {
    fn from(e: postgres::Error) -> Self {
        SaveError::Db(DbError::from(e))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AppVersionRepository;

fn from_row(row: &Row) -> Result<AppVersion, DbError> {
    let platform: String = row.try_get("platform")?;
    let version: String = row.try_get("minimum_supported_version")?;
    let url: String = row.try_get("store_url")?;
    let corrupt =
        |what: &str| DbError::Config(format!("저장된 앱 버전 설정의 {what}이 올바르지 않습니다."));
    Ok(AppVersion {
        id: Some(row.try_get("id")?),
        platform: AppPlatform::parse(&platform).ok_or_else(|| corrupt("플랫폼"))?,
        minimum_supported_version: MinimumSupportedVersion::new(&version)
            .map_err(|_| corrupt("최소 지원 버전"))?,
        store_url: StoreUrl::new(&url).map_err(|_| corrupt("스토어 주소"))?,
        revision: row.try_get("revision")?,
        last_changed_by: row.try_get("last_changed_by")?,
        change_reason: row.try_get("change_reason")?,
        created_at: Some(row.try_get("created_at")?),
        updated_at: Some(row.try_get("updated_at")?),
    })
}

impl AppVersionRepository {
    /// `findByPlatform`
    pub fn find_by_platform(
        &self,
        db: &mut impl Db,
        platform: AppPlatform,
    ) -> Result<Option<AppVersion>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM app_versions WHERE platform = $1");
        db.query_opt(&sql, &[&platform.as_str()])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `lockPlatform`: 행이 없는 최초 저장도 플랫폼 단위로 직렬화한다. 네임스페이스 3은 앱 버전 설정에 쓴다.
    /// 트랜잭션 안에서 불러야 하고 트랜잭션이 끝나면 풀린다.
    pub fn lock_platform(&self, db: &mut impl Db, platform: AppPlatform) -> Result<(), DbError> {
        db.query_one(
            "SELECT pg_advisory_xact_lock(3, hashtext($1))",
            &[&platform.as_str()],
        )?;
        Ok(())
    }

    /// `saveAndFlush`: 새 설정이면 INSERT, 이미 저장된 설정이면 revision을 조건으로 UPDATE한다.
    /// 조건에 맞는 행이 없으면(다른 요청이 먼저 바꿨다) `RESOURCE_STATE_CHANGED`다.
    /// `created_at`·`updated_at`은 JPA의 `@CreationTimestamp`·`@UpdateTimestamp`처럼 앱 시각(`now`)으로 채운다.
    pub fn save_and_flush(
        &self,
        db: &mut impl Db,
        setting: &mut AppVersion,
        now: Timestamp,
    ) -> Result<(), SaveError> {
        match setting.id {
            None => {
                let row = db.query_one(
                    "INSERT INTO app_versions \
                     (platform, minimum_supported_version, store_url, revision, last_changed_by, \
                      change_reason, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
                     RETURNING id, created_at, updated_at",
                    &[
                        &setting.platform.as_str(),
                        &setting.minimum_supported_version.value(),
                        &setting.store_url.value(),
                        &setting.revision,
                        &setting.last_changed_by,
                        &setting.change_reason,
                        &now,
                    ],
                )?;
                setting.id = Some(row.try_get("id")?);
                setting.created_at = Some(row.try_get("created_at")?);
                setting.updated_at = Some(row.try_get("updated_at")?);
            }
            Some(id) => {
                let row = db.query_opt(
                    "UPDATE app_versions SET minimum_supported_version = $3, store_url = $4, \
                     revision = revision + 1, last_changed_by = $5, change_reason = $6, updated_at = $7 \
                     WHERE id = $1 AND revision = $2 \
                     RETURNING revision, updated_at",
                    &[
                        &id,
                        &setting.revision,
                        &setting.minimum_supported_version.value(),
                        &setting.store_url.value(),
                        &setting.last_changed_by,
                        &setting.change_reason,
                        &now,
                    ],
                )?;
                let Some(row) = row else {
                    return Err(SaveError::Business(AppError::business(
                        ErrorCode::RESOURCE_STATE_CHANGED,
                        STATE_CHANGED_MESSAGE,
                    )));
                };
                setting.revision = row.try_get("revision")?;
                setting.updated_at = Some(row.try_get("updated_at")?);
            }
        }
        Ok(())
    }
}
