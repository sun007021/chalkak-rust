//! 게시물·사진·좋아요 DB 통합 테스트가 함께 쓰는 보조. 시각은 `FixedClock`으로 고정하고, 행의 시각은 모두 이 시계
//! 기준의 리터럴로 넣는다(원본 테스트는 `CURRENT_TIMESTAMP`를 썼지만 여기서는 경계 비교를 결정적으로 만든다).
#![allow(dead_code)]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalkak_app::post::PostWiring;
use chalkak_app::post::listener::PostModerationPendingListener;
use chalkak_app::post::settings::{ImageSettings, PostSettings};
use chalkak_app::post::storage::fake::FakeObjectStore;
use chalkak_app::post::{PostCommandService, PostQueryService};
use chalkak_app::testing::TestDb;
use chalkak_app::{AppContext, like::service::PostLikeService};
use chalkak_core::FixedClock;
use chalkak_core::log::{Level, Logger};
use jiff::Timestamp;
use postgres::Client;
use uuid::Uuid;

pub const NOW: &str = "2026-10-12T03:00:00Z";
pub const TOPIC_DATE: &str = "2026-10-12";
pub const TOPIC_START: &str = "2026-10-11T15:00:00Z";
pub const TOPIC_END: &str = "2026-10-12T15:00:00Z";

pub fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// `NOW`에서 `seconds`초 뒤(음수면 앞)의 리터럴.
pub fn at(seconds: i64) -> String {
    ts(NOW)
        .checked_add(jiff::SignedDuration::from_secs(seconds))
        .unwrap()
        .to_string()
}

#[derive(Default)]
pub struct Events(pub Mutex<Vec<(Uuid, Timestamp)>>);

impl PostModerationPendingListener for Events {
    fn on_pending(&self, post_id: Uuid, occurred_at: Timestamp) {
        self.0.lock().unwrap().push((post_id, occurred_at));
    }
}

#[derive(Clone, Default)]
pub struct Capture(pub Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    pub fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// 업무 오류의 상태 코드와 메시지를 확인한다.
pub fn assert_err<T: std::fmt::Debug>(
    result: Result<T, chalkak_app::web::ApiError>,
    status: u16,
    message: &str,
) {
    match result {
        Err(chalkak_app::web::ApiError::App(e)) => {
            assert_eq!((e.status(), e.message.as_str()), (status, message), "{e:?}");
        }
        other => panic!("업무 오류여야 합니다: {other:?}"),
    }
}

pub struct Fx {
    pub db: TestDb,
    pub clock: Arc<FixedClock>,
    pub store: Arc<FakeObjectStore>,
    pub events: Arc<Events>,
    pub log: Capture,
    pub ctx: AppContext,
    pub command: PostCommandService,
    pub query: PostQueryService,
    pub likes: PostLikeService,
    next: std::sync::atomic::AtomicU64,
}

pub fn settings() -> PostSettings {
    PostSettings {
        image: ImageSettings {
            bucket: "test-bucket".to_owned(),
            region: "ap-northeast-2".to_owned(),
            root_prefix: "chalkak".to_owned(),
            environment: "test".to_owned(),
            post_max_bytes: 5_242_880,
            post_cache_control: "public, max-age=86400".to_owned(),
            anonymous_access: false,
        },
        cloudfront_base_url: "https://cdn.test.chalkak".to_owned(),
        cloudfront_origin_path: "/chalkak".to_owned(),
        callback_secret: "test-callback-secret-with-enough-length".to_owned(),
        callback_max_body_bytes: 65_536,
        image_processing_timeout: Duration::from_secs(7 * 60),
        s3_endpoint_url: None,
    }
}

impl Fx {
    pub fn create() -> Option<Fx> {
        let db = TestDb::create()?;
        let clock = Arc::new(FixedClock::new(ts(NOW)));
        let store = Arc::new(FakeObjectStore::default());
        let events = Arc::new(Events::default());
        let log = Capture::default();
        let logger = Arc::new(Logger::new(
            Box::new(log.clone()),
            Level::Info,
            clock.clone(),
        ));
        let wiring = PostWiring::new(settings(), store.clone(), events.clone());
        let ctx = AppContext {
            pool: db.pool(8),
            clock: clock.clone(),
            logger,
            post: wiring.clone(),
        };
        let command = wiring.command_service(&ctx);
        let query = wiring.query_service(&ctx);
        let likes = PostLikeService::new(ctx.pool.clone());
        Some(Fx {
            db,
            clock,
            store,
            events,
            log,
            ctx,
            command,
            query,
            likes,
            next: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub fn client(&self) -> Client {
        self.db.client()
    }

    fn seq(&self) -> u64 {
        self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn exec(&self, sql: &str) {
        self.client().batch_execute(sql).unwrap();
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.client().query_one(sql, &[]).unwrap().get(0)
    }

    pub fn text(&self, sql: &str) -> Option<String> {
        self.client().query_one(sql, &[]).unwrap().get(0)
    }

    pub fn user(&self) -> Uuid {
        let id = Uuid::new_v4();
        let n = self.seq();
        self.exec(&format!(
            "INSERT INTO users (id, email, signature_original_storage_key, signature_thumbnail_storage_key) \
             VALUES ('{id}', 'u{n}@test.chalkak', 'chalkak/signatures/{id}/original.png', \
             'chalkak/signatures/{id}/thumbnail.png')"
        ));
        id
    }

    pub fn delete_user(&self, id: Uuid) {
        self.exec(&format!(
            "UPDATE users SET deleted_at = TIMESTAMPTZ '{NOW}' WHERE id = '{id}'"
        ));
    }

    /// 지금(`NOW`) 참여할 수 있는 주제.
    pub fn open_topic(&self) -> Uuid {
        self.topic(TOPIC_DATE, TOPIC_START, TOPIC_END)
    }

    pub fn topic(&self, date: &str, starts: &str, ends: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.exec(&format!(
            "INSERT INTO topics (id, title, topic_date, starts_at, ends_at, created_at, updated_at) \
             VALUES ('{id}', '주제 {date}', DATE '{date}', TIMESTAMPTZ '{starts}', TIMESTAMPTZ '{ends}', \
             TIMESTAMPTZ '{NOW}', TIMESTAMPTZ '{NOW}')"
        ));
        id
    }

    pub fn delete_topic(&self, id: Uuid) {
        self.exec(&format!(
            "UPDATE topics SET deleted_at = TIMESTAMPTZ '{NOW}' WHERE id = '{id}'"
        ));
    }

    /// 업로드 행. `status`가 READY면 메타데이터를, REJECTED면 사유를 채운다.
    pub fn upload(
        &self,
        user: Uuid,
        status: &str,
        expires_at: &str,
        claimed_at: Option<&str>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let (reason, metadata) = match status {
            "READY" => ("NULL", "'{\"width\":4032,\"height\":3024}'::jsonb"),
            "REJECTED" => ("'UNSUPPORTED_FORMAT'", "NULL"),
            _ => ("NULL", "NULL"),
        };
        let claimed = claimed_at.map_or("NULL".to_owned(), |c| format!("TIMESTAMPTZ '{c}'"));
        self.exec(&format!(
            "INSERT INTO post_image_uploads (id, user_id, status, rejection_reason, image_metadata, expires_at, \
             claimed_at, created_at, updated_at) VALUES ('{id}', '{user}', '{status}', {reason}, {metadata}, \
             TIMESTAMPTZ '{expires_at}', {claimed}, TIMESTAMPTZ '{NOW}', TIMESTAMPTZ '{NOW}')"
        ));
        id
    }

    /// 아직 쓰지 않은 유효한 업로드.
    pub fn upload_in(&self, user: Uuid, status: &str) -> Uuid {
        self.upload(user, status, &at(600), None)
    }

    /// 사진과 게시물을 직접 넣는다. 반환값은 (게시물 id, 사진 id).
    pub fn post(&self, user: Uuid, topic: Uuid, status: &str, created_at: &str) -> (Uuid, Uuid) {
        let post = Uuid::new_v4();
        let photo = Uuid::new_v4();
        self.exec(&format!(
            "INSERT INTO photos (id, original_storage_key, thumbnail_storage_key, metadata, created_at, updated_at) \
             VALUES ('{photo}', 'chalkak/posts/test/original/{photo}.webp', \
             'chalkak/posts/test/thumbnail/{photo}.webp', '{{}}'::jsonb, TIMESTAMPTZ '{created_at}', TIMESTAMPTZ '{created_at}'); \
             INSERT INTO posts (id, user_id, topic_id, photo_id, moderation_status, created_at, updated_at) \
             VALUES ('{post}', '{user}', '{topic}', '{photo}', '{status}', TIMESTAMPTZ '{created_at}', TIMESTAMPTZ '{created_at}')"
        ));
        (post, photo)
    }

    pub fn approved(&self, user: Uuid, topic: Uuid) -> Uuid {
        self.post(user, topic, "APPROVED", &at(-3600)).0
    }

    pub fn like(&self, post: Uuid, user: Uuid) {
        self.exec(&format!(
            "INSERT INTO post_likes (post_id, user_id, created_at) VALUES ('{post}', '{user}', TIMESTAMPTZ '{NOW}')"
        ));
    }

    pub fn soft_delete(&self, table: &str, id: Uuid) {
        self.exec(&format!(
            "UPDATE {table} SET deleted_at = TIMESTAMPTZ '{NOW}' WHERE id = '{id}'"
        ));
    }

    pub fn staging_key(&self, upload: Uuid) -> String {
        format!("chalkak/staging/test/posts/{upload}.webp")
    }

    /// staging 객체가 올라와 있다고 정한다.
    pub fn stage(&self, upload: Uuid) {
        self.store
            .existing
            .lock()
            .unwrap()
            .insert(self.staging_key(upload));
    }

    pub fn moderation(&self, post: Uuid) -> String {
        self.text(&format!(
            "SELECT moderation_status::text FROM posts WHERE id = '{post}'"
        ))
        .unwrap()
    }

    pub fn upload_status(&self, upload: Uuid) -> String {
        self.text(&format!(
            "SELECT status::text FROM post_image_uploads WHERE id = '{upload}'"
        ))
        .unwrap()
    }
}
