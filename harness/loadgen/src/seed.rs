//! 시드 데이터. 두 구현(Java, Rust)의 DB에 같은 데이터를 넣는다. 앱이 한 번 떠서 Flyway가 스키마를 만든 뒤에 실행한다.
//!
//! 모든 값은 결정적이다(UUID, 시각 오프셋). 날짜는 실행 시각의 한국 날짜 기준이라 "오늘" 주제와 게시물이 항상 있다.
//! 회원 N명, 주제 D일치(오늘 포함), 주제마다 게시물 P개(APPROVED), 게시물마다 좋아요 L개, 앞 100명에게 알림 몇 개.

use postgres::Client;

#[derive(Clone, Copy, Debug)]
pub struct SeedSize {
    pub users: u64,
    pub days: u64,
    pub posts_per_topic: u64,
    pub likes_per_post: u64,
    pub notifications_per_user: u64,
}

impl Default for SeedSize {
    fn default() -> Self {
        SeedSize {
            users: 300,
            days: 60,
            posts_per_topic: 150,
            likes_per_post: 12,
            notifications_per_user: 6,
        }
    }
}

/// 게시물 UUID. 주제 `d`일 전의 `u`번째 회원 게시물.
pub fn post_uuid(day: u64, user: u64) -> String {
    format!("01990000-0000-7000-8001-{:012x}", day * 100_000 + user)
}

pub fn seed_sql(size: &SeedSize) -> String {
    let SeedSize {
        users,
        days,
        posts_per_topic: ppt,
        likes_per_post: lpp,
        notifications_per_user: npu,
    } = *size;
    assert!(ppt <= users, "주제마다 회원당 게시물은 하나다");
    assert!(lpp < users);
    // 회원 u(1..=users)의 d일 전 게시물 id = 01990000-0000-7000-8001-(d*100000+u)
    format!(
        r#"
TRUNCATE users, topics, photos, admin_audit_logs, feedbacks RESTART IDENTITY CASCADE;

INSERT INTO users (id, email, status, signature_original_storage_key, signature_thumbnail_storage_key)
SELECT ('01990000-0000-7000-8000-' || lpad(to_hex(g), 12, '0'))::uuid,
       'load' || g || '@test.chalkak', 'ACTIVE',
       'chalkak/signatures/load' || g || '/original.png',
       'chalkak/signatures/load' || g || '/thumbnail.png'
FROM generate_series(1, {users}) g;

-- 주제: 오늘(한국 날짜)부터 {days}일치. 하루 전체가 열려 있다.
INSERT INTO topics (id, title, topic_date, starts_at, ends_at)
SELECT ('01990000-0000-7000-8002-' || lpad(to_hex(d), 12, '0'))::uuid,
       '주제 ' || d,
       t.day,
       t.day::timestamp AT TIME ZONE 'Asia/Seoul',
       (t.day + 1)::timestamp AT TIME ZONE 'Asia/Seoul'
FROM generate_series(0, {days} - 1) d,
     LATERAL (SELECT ((now() AT TIME ZONE 'Asia/Seoul')::date - d) AS day) t;

INSERT INTO photos (id, original_storage_key, thumbnail_storage_key, metadata)
SELECT ('01990000-0000-7000-8003-' || lpad(to_hex(d * 100000 + u), 12, '0'))::uuid,
       'chalkak/posts/load/original/' || d || '-' || u || '.webp',
       'chalkak/posts/load/thumbnail/' || d || '-' || u || '.webp',
       '{{"width":4032,"height":3024,"byteSize":183423,"location":{{"latitude":37.5665,"longitude":126.978}},"capturedAt":"2026-10-12T08:00:00+09:00","metaAttributes":{{"make":"Apple","iso":100}}}}'::jsonb
FROM generate_series(0, {days} - 1) d, generate_series(1, {ppt}) u;

INSERT INTO posts (id, user_id, topic_id, photo_id, title, moderation_status, moderated_at, created_at, updated_at)
SELECT ('01990000-0000-7000-8001-' || lpad(to_hex(d * 100000 + u), 12, '0'))::uuid,
       ('01990000-0000-7000-8000-' || lpad(to_hex(u), 12, '0'))::uuid,
       ('01990000-0000-7000-8002-' || lpad(to_hex(d), 12, '0'))::uuid,
       ('01990000-0000-7000-8003-' || lpad(to_hex(d * 100000 + u), 12, '0'))::uuid,
       '글' || u, 'APPROVED',
       tp.starts_at + (u || ' minutes')::interval,
       tp.starts_at + (u || ' minutes')::interval,
       tp.starts_at + (u || ' minutes')::interval
FROM generate_series(0, {days} - 1) d, generate_series(1, {ppt}) u,
     LATERAL (SELECT starts_at FROM topics
              WHERE id = ('01990000-0000-7000-8002-' || lpad(to_hex(d), 12, '0'))::uuid) tp;

-- 게시물마다 좋아요 {lpp}개: 작성자 다음 번호의 회원들이 누른다.
INSERT INTO post_likes (post_id, user_id, created_at)
SELECT ('01990000-0000-7000-8001-' || lpad(to_hex(d * 100000 + u), 12, '0'))::uuid,
       ('01990000-0000-7000-8000-' || lpad(to_hex((u + k - 1) % {users} + 1), 12, '0'))::uuid,
       now()
FROM generate_series(0, {days} - 1) d, generate_series(1, {ppt}) u, generate_series(1, {lpp}) k;

-- 알림: 앞 100명에게 {npu}개씩. (회원, 사건)이 유일해야 해서 사건(감사 로그)은 알림 순번마다 하나다.
INSERT INTO admin_audit_logs (id, actor_admin_id, action, target_type, target_id, occurred_at, request_id, before_state, after_state)
SELECT ('01990000-0000-7000-8004-' || lpad(to_hex(n), 12, '0'))::uuid, (SELECT id FROM admins ORDER BY created_at LIMIT 1),
       'POST_APPROVED', 'POST', '01990000-0000-7000-8001-000000000001'::uuid, now(), gen_random_uuid(),
       '{{"moderationStatus":"PENDING"}}'::jsonb, '{{"moderationStatus":"APPROVED"}}'::jsonb
FROM generate_series(0, {npu} - 1) n
WHERE EXISTS (SELECT 1 FROM admins);

INSERT INTO notifications (user_id, post_id, event_key, type, title, body, created_at)
SELECT ('01990000-0000-7000-8000-' || lpad(to_hex(u), 12, '0'))::uuid,
       ('01990000-0000-7000-8001-' || lpad(to_hex(n * 100000 + u), 12, '0'))::uuid,
       ('01990000-0000-7000-8004-' || lpad(to_hex(n), 12, '0'))::uuid,
       'POST_APPROVED', '게시물이 승인되었어요', '게시물이 공개되었습니다.',
       now() - (n || ' hours')::interval
FROM generate_series(1, LEAST(100, {ppt})) u, generate_series(0, {npu} - 1) n
WHERE EXISTS (SELECT 1 FROM admins);

CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
ANALYZE;
"#
    )
}

pub fn run(client: &mut Client, size: &SeedSize) -> Result<(), postgres::Error> {
    client.batch_execute(&seed_sql(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 게시물_uuid가_시드_sql과_같은_규칙이다() {
        assert_eq!(post_uuid(0, 1), "01990000-0000-7000-8001-000000000001");
        assert_eq!(post_uuid(2, 5), "01990000-0000-7000-8001-000000030d45");
    }

    #[test]
    fn 시드_sql에_크기가_들어간다() {
        let sql = seed_sql(&SeedSize {
            users: 7,
            days: 3,
            posts_per_topic: 5,
            likes_per_post: 2,
            notifications_per_user: 1,
        });
        assert!(sql.contains("generate_series(1, 7) g"));
        assert!(sql.contains("generate_series(0, 3 - 1) d"));
        assert!(sql.contains("generate_series(1, 5) u"));
    }

    #[test]
    #[should_panic(expected = "주제마다")]
    fn 회원보다_많은_게시물은_거절한다() {
        seed_sql(&SeedSize {
            users: 3,
            days: 1,
            posts_per_topic: 4,
            likes_per_post: 1,
            notifications_per_user: 1,
        });
    }
}
