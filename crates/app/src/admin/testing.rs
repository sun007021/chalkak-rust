//! 관리자 모듈의 DB 통합 테스트 보조(테스트에서만 쓴다).

use std::sync::Arc;

use chalkak_core::FixedClock;
use chalkak_core::log::{Level, Logger};
use postgres::Client;
use uuid::Uuid;

use super::config::AdminConfig;
use super::support::AdminDeps;
use crate::testing::TestDb;

pub const NOW: &str = "2026-10-12T01:00:00Z";
pub const ACCESS_KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

pub fn deps(db: &TestDb) -> AdminDeps {
    deps_at(db, NOW)
}

pub fn deps_at(db: &TestDb, now: &str) -> AdminDeps {
    let clock: Arc<FixedClock> = Arc::new(FixedClock::new(now.parse().unwrap()));
    AdminDeps {
        pool: db.pool(4),
        logger: Arc::new(Logger::new(
            Box::new(std::io::sink()),
            Level::Info,
            clock.clone(),
        )),
        clock,
        config: Arc::new(AdminConfig::for_tests(ACCESS_KEY.to_vec())),
    }
}

pub fn insert_admin(client: &mut Client, username: &str) -> Uuid {
    insert_admin_with_hash(
        client,
        username,
        "$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6",
    )
}

pub fn insert_admin_with_hash(client: &mut Client, username: &str, hash: &str) -> Uuid {
    client
        .query_one(
            "INSERT INTO admins (username, password) VALUES ($1, $2) RETURNING id",
            &[&username, &hash],
        )
        .unwrap()
        .get(0)
}

pub fn insert_user(client: &mut Client, email: &str, status: &str) -> Uuid {
    let key = format!("chalkak/signatures/{}/original.png", Uuid::new_v4());
    client
        .query_one(
            "INSERT INTO users (email, status, signature_original_storage_key, \
             signature_thumbnail_storage_key) VALUES ($1, $2::text::user_status, $3, $4) RETURNING id",
            &[&email, &status, &key, &key.replace("original", "thumbnail")],
        )
        .unwrap()
        .get(0)
}

pub fn insert_topic(
    client: &mut Client,
    title: &str,
    date: &str,
    starts: &str,
    ends: &str,
) -> Uuid {
    client
        .query_one(
            "INSERT INTO topics (title, topic_date, starts_at, ends_at) \
             VALUES ($1, $2::text::date, $3::text::timestamptz, $4::text::timestamptz) RETURNING id",
            &[&title, &date, &starts, &ends],
        )
        .unwrap()
        .get(0)
}

/// 게시물과 사진을 함께 넣는다. `status`는 `moderation_status` 이름.
pub fn insert_post(client: &mut Client, user: Uuid, topic: Uuid, status: &str) -> Uuid {
    let key = format!("chalkak/posts/{}/original.webp", Uuid::new_v4());
    let photo: Uuid = client
        .query_one(
            "INSERT INTO photos (original_storage_key, thumbnail_storage_key, metadata) \
             VALUES ($1, $2, '{\"width\": 10, \"height\": 20, \"byteSize\": 30}'::jsonb) RETURNING id",
            &[&key, &key.replace("original", "thumbnail")],
        )
        .unwrap()
        .get(0);
    client
        .query_one(
            "INSERT INTO posts (user_id, topic_id, photo_id, title, moderation_status) \
             VALUES ($1, $2, $3, '제목', $4::text::moderation_status) RETURNING id",
            &[&user, &topic, &photo, &status],
        )
        .unwrap()
        .get(0)
}
