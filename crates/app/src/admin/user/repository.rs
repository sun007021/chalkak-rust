//! 원본 `AdminUserQueryRepositoryImpl`·`AdminUserJpaRepository`와 상태 변경이 쓰는 `UserRepository` 조각.
//!
//! users·posts·social_accounts를 직접 읽고 쓴다(사용자 모듈의 저장소를 거치지 않는다).

use chalkak_db::DbError;
use jiff::Timestamp;
use postgres::GenericClient;
use uuid::Uuid;

use crate::admin::support::{AdminPage, SqlFilter, limit_offset};

/// 관리자 화면의 사용자 상태(`AdminUserStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminUserStatus {
    Active,
    Banned,
    Withdrawn,
}

impl AdminUserStatus {
    pub fn parse(name: &str) -> Option<AdminUserStatus> {
        match name {
            "ACTIVE" => Some(AdminUserStatus::Active),
            "BANNED" => Some(AdminUserStatus::Banned),
            "WITHDRAWN" => Some(AdminUserStatus::Withdrawn),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            AdminUserStatus::Active => "ACTIVE",
            AdminUserStatus::Banned => "BANNED",
            AdminUserStatus::Withdrawn => "WITHDRAWN",
        }
    }

    /// `AdminUserStatus.from(userStatus, deletedAt)`
    pub fn from_user(status: UserStatus, deleted_at: Option<Timestamp>) -> AdminUserStatus {
        if deleted_at.is_some() {
            return AdminUserStatus::Withdrawn;
        }
        match status {
            UserStatus::Banned => AdminUserStatus::Banned,
            UserStatus::Active => AdminUserStatus::Active,
        }
    }
}

/// 회원 상태(`UserStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserStatus {
    Active,
    Banned,
}

impl UserStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            UserStatus::Active => "ACTIVE",
            UserStatus::Banned => "BANNED",
        }
    }

    pub fn parse(name: &str) -> Option<UserStatus> {
        match name {
            "ACTIVE" => Some(UserStatus::Active),
            "BANNED" => Some(UserStatus::Banned),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminUserSort {
    CreatedAtDesc,
    CreatedAtAsc,
}

impl AdminUserSort {
    pub fn from_query(value: &str) -> Option<AdminUserSort> {
        match value {
            "createdAtDesc" => Some(AdminUserSort::CreatedAtDesc),
            "createdAtAsc" => Some(AdminUserSort::CreatedAtAsc),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UserQueryCriteria {
    pub status: Option<AdminUserStatus>,
    pub email: Option<String>,
    pub sort: AdminUserSort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostCounts {
    pub pending: i64,
    pub approved: i64,
    pub rejected: i64,
}

#[derive(Clone, Debug)]
pub struct UserSummary {
    pub user_id: Uuid,
    pub email: Option<String>,
    pub user_status: UserStatus,
    pub app_version: Option<String>,
    pub social_provider: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
    pub post_counts: PostCounts,
}

#[derive(Clone, Debug)]
pub struct UserDetailRow {
    pub summary: UserSummary,
    pub signature_original_storage_key: String,
    pub signature_thumbnail_storage_key: Option<String>,
}

/// 회원 행(상태 변경용).
#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: Uuid,
    pub status: UserStatus,
    pub deleted_at: Option<Timestamp>,
}

const SUMMARY_COLUMNS: &str = "u.id, u.email, u.status::text AS status, u.app_version, \
    sa.provider::text AS provider, u.created_at, u.updated_at, u.deleted_at, \
    u.signature_original_storage_key, u.signature_thumbnail_storage_key, \
    (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id AND p.moderation_status = 'PENDING') AS pending_count, \
    (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id AND p.moderation_status = 'APPROVED') AS approved_count, \
    (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id AND p.moderation_status = 'REJECTED') AS rejected_count";

fn detail_from_row(row: &postgres::Row) -> Result<UserDetailRow, DbError> {
    let status: String = row.try_get("status")?;
    let status = UserStatus::parse(&status)
        .ok_or_else(|| DbError::Config(format!("알 수 없는 회원 상태: {status}")))?;
    Ok(UserDetailRow {
        summary: UserSummary {
            user_id: row.try_get("id")?,
            email: row.try_get("email")?,
            user_status: status,
            app_version: row.try_get("app_version")?,
            social_provider: row.try_get("provider")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            deleted_at: row.try_get("deleted_at")?,
            post_counts: PostCounts {
                pending: row.try_get("pending_count")?,
                approved: row.try_get("approved_count")?,
                rejected: row.try_get("rejected_count")?,
            },
        },
        signature_original_storage_key: row.try_get("signature_original_storage_key")?,
        signature_thumbnail_storage_key: row.try_get("signature_thumbnail_storage_key")?,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AdminUserRepository;

impl AdminUserRepository {
    pub fn find_users(
        &self,
        db: &mut impl GenericClient,
        criteria: &UserQueryCriteria,
        page: i32,
        page_size: i32,
    ) -> Result<AdminPage<UserSummary>, DbError> {
        let mut filter = SqlFilter::new();
        match criteria.status {
            None => {}
            Some(AdminUserStatus::Withdrawn) => filter.add_fixed("u.deleted_at IS NOT NULL"),
            Some(AdminUserStatus::Active) => {
                filter.add_fixed("u.deleted_at IS NULL AND u.status = 'ACTIVE'");
            }
            Some(AdminUserStatus::Banned) => {
                filter.add_fixed("u.deleted_at IS NULL AND u.status = 'BANNED'");
            }
        }
        // LOCATE(LOWER(:email), LOWER(user.email)) > 0
        filter.add_opt(
            "position(lower(?::text) in lower(u.email)) > 0",
            criteria.email.clone(),
        );
        let direction = match criteria.sort {
            AdminUserSort::CreatedAtDesc => "DESC",
            AdminUserSort::CreatedAtAsc => "ASC",
        };
        let (limit, offset) = limit_offset(page, page_size);
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS} FROM users u \
             LEFT JOIN social_accounts sa ON sa.user_id = u.id{} \
             ORDER BY u.created_at {direction}, u.id {direction} LIMIT ${} OFFSET ${}",
            filter.where_clause(),
            filter.next_placeholder(),
            filter.next_placeholder() + 1,
        );
        let params = filter.params_with(&[&limit, &offset]);
        let rows = db.query(&sql, &params)?;
        let items = rows
            .iter()
            .map(|row| detail_from_row(row).map(|d| d.summary))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AdminPage::from_overfetched(items, page, page_size))
    }

    pub fn find_user_by_id(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
    ) -> Result<Option<UserDetailRow>, DbError> {
        let sql = format!(
            "SELECT {SUMMARY_COLUMNS} FROM users u \
             LEFT JOIN social_accounts sa ON sa.user_id = u.id WHERE u.id = $1 LIMIT 1"
        );
        let row = db.query_opt(&sql, &[&user_id])?;
        row.map(|row| detail_from_row(&row)).transpose()
    }

    /// `UserRepository.findByIdForUpdate`
    pub fn find_by_id_for_update(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
    ) -> Result<Option<UserRow>, DbError> {
        let row = db.query_opt(
            "SELECT id, status::text AS status, deleted_at FROM users WHERE id = $1 FOR UPDATE",
            &[&user_id],
        )?;
        row.map(|row| {
            let status: String = row.try_get("status")?;
            Ok(UserRow {
                id: row.try_get("id")?,
                status: UserStatus::parse(&status)
                    .ok_or_else(|| DbError::Config(format!("알 수 없는 회원 상태: {status}")))?,
                deleted_at: row.try_get("deleted_at")?,
            })
        })
        .transpose()
    }

    /// 상태를 바꾸고 `updated_at`을 갱신한다(`@UpdateTimestamp`).
    pub fn update_status(
        &self,
        db: &mut impl GenericClient,
        user_id: Uuid,
        status: UserStatus,
        now: Timestamp,
    ) -> Result<(), DbError> {
        db.execute(
            "UPDATE users SET status = $2::text::user_status, updated_at = $3 WHERE id = $1",
            &[&user_id, &status.as_str(), &now],
        )?;
        Ok(())
    }
}
