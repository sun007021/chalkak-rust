//! AI 프롬프트 버전 저장소(원본 `AiPromptVersionRepositoryImpl`, `AiPromptVersionJpaRepository`).
//! 연결(또는 트랜잭션)은 호출 측이 넘긴다.

use chalkak_db::{Db, DbError};
use jiff::Timestamp;
use postgres::Row;
use uuid::Uuid;

use super::domain::{AiPromptParameters, AiPromptType, AiPromptVersion};

/// 어드바이저리 잠금 classid 2는 AI 프롬프트 버전 번호 채번 전용이다(1은 푸시 기기).
const LOCK_KEY_PREFIX: &str = "ai_prompt_versions:";

const COLUMNS: &str = "id, type, version, system_prompt, user_prompt_template, model, \
     parameters::text AS parameters, output_schema_version, memo, created_by_admin_id, created_at";

/// `AiPromptVersionPage`
#[derive(Debug)]
pub struct PromptVersionPage {
    pub items: Vec<AiPromptVersion>,
    pub has_next: bool,
}

/// DB가 마이크로초까지 저장하므로 응답과 저장값이 같도록 맞춘다.
fn truncate_to_micros(at: Timestamp) -> Timestamp {
    Timestamp::from_microsecond(at.as_microsecond()).unwrap_or(at)
}

fn from_row(row: &Row) -> Result<AiPromptVersion, DbError> {
    let type_name: String = row.try_get("type")?;
    let prompt_type = AiPromptType::parse(&type_name)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 프롬프트 유형: {type_name}")))?;
    let parameters_text: String = row.try_get("parameters")?;
    let parameters = AiPromptParameters::from_json(&parameters_text)
        .map_err(|e| DbError::Config(format!("저장된 프롬프트 옵션이 올바르지 않습니다: {e}")))?;
    Ok(AiPromptVersion {
        id: Some(row.try_get("id")?),
        prompt_type,
        version: row.try_get("version")?,
        system_prompt: row.try_get("system_prompt")?,
        user_prompt_template: row.try_get("user_prompt_template")?,
        model: row.try_get("model")?,
        parameters,
        output_schema_version: row.try_get("output_schema_version")?,
        memo: row.try_get("memo")?,
        created_by_admin_id: row.try_get("created_by_admin_id")?,
        created_at: row.try_get("created_at")?,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PromptVersionRepository;

impl PromptVersionRepository {
    /// `save`: 새 버전을 넣는다. 아이디는 DB가 만든다.
    pub fn save(
        &self,
        db: &mut impl Db,
        version: &mut AiPromptVersion,
        now: Timestamp,
    ) -> Result<(), DbError> {
        let created_at = truncate_to_micros(now);
        let parameters = version.parameters.to_json();
        let row = db.query_one(
            "INSERT INTO ai_prompt_versions (type, version, system_prompt, user_prompt_template, \
             model, parameters, output_schema_version, memo, created_by_admin_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6::text::jsonb, $7, $8, $9, $10) RETURNING id",
            &[
                &version.prompt_type.as_str(),
                &version.version,
                &version.system_prompt,
                &version.user_prompt_template,
                &version.model,
                &parameters,
                &version.output_schema_version,
                &version.memo,
                &version.created_by_admin_id,
                &created_at,
            ],
        )?;
        version.id = Some(row.try_get("id")?);
        version.created_at = Some(created_at);
        Ok(())
    }

    /// `lockType`: 용도 하나의 버전 번호 부여를 트랜잭션이 끝날 때까지 직렬화한다. 아직 없는 다음 버전 행까지 덮도록 행이
    /// 아니라 용도 키를 어드바이저리 잠금으로 건다. 해제 호출은 없고 커밋이나 롤백이 푼다.
    pub fn lock_type(&self, db: &mut impl Db, prompt_type: AiPromptType) -> Result<(), DbError> {
        let key = format!("{LOCK_KEY_PREFIX}{}", prompt_type.as_str());
        db.query(
            "SELECT pg_advisory_xact_lock(2, hashtext(CAST($1 AS text)))",
            &[&key],
        )?;
        Ok(())
    }

    /// `findMaxVersion`: 용도의 가장 큰 버전 번호. 아직 버전이 없으면 0이다.
    pub fn find_max_version(
        &self,
        db: &mut impl Db,
        prompt_type: AiPromptType,
    ) -> Result<i32, DbError> {
        let row = db.query_one(
            "SELECT COALESCE(MAX(version), 0) AS max_version FROM ai_prompt_versions WHERE type = $1",
            &[&prompt_type.as_str()],
        )?;
        Ok(row.try_get("max_version")?)
    }

    /// `findLatestByType`: 용도의 가장 큰 버전. 출력 스키마나 모델은 거르지 않는다.
    pub fn find_latest_by_type(
        &self,
        db: &mut impl Db,
        prompt_type: AiPromptType,
    ) -> Result<Option<AiPromptVersion>, DbError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM ai_prompt_versions WHERE type = $1 ORDER BY version DESC LIMIT 1"
        );
        db.query_opt(&sql, &[&prompt_type.as_str()])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    pub fn find_by_id(
        &self,
        db: &mut impl Db,
        id: Uuid,
    ) -> Result<Option<AiPromptVersion>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM ai_prompt_versions WHERE id = $1");
        db.query_opt(&sql, &[&id])?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// `findByType`: 큰 번호부터 페이지 단위로. `page`는 1부터 시작한다.
    pub fn find_by_type(
        &self,
        db: &mut impl Db,
        prompt_type: AiPromptType,
        page: i32,
        page_size: i32,
    ) -> Result<PromptVersionPage, DbError> {
        let (limit, offset) = crate::admin::support::limit_offset(page, page_size);
        let sql = format!(
            "SELECT {COLUMNS} FROM ai_prompt_versions WHERE type = $1 \
             ORDER BY version DESC LIMIT $2 OFFSET $3"
        );
        let rows = db.query(&sql, &[&prompt_type.as_str(), &limit, &offset])?;
        let mut items = rows.iter().map(from_row).collect::<Result<Vec<_>, _>>()?;
        let size = usize::try_from(page_size).unwrap_or(0);
        let has_next = items.len() > size;
        items.truncate(size);
        Ok(PromptVersionPage { items, has_next })
    }
}
