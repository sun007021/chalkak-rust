//! 관리자 게시물 삭제가 회원 조회 모델(상세·캘린더·피드)에서 즉시 사라지게 하는지 확인한다.
//! 원본 `AdminPostCommandDeletionTest#deletePost_approvedPost_excludesEveryUserReadModel`.
//! `CHALKAK_TEST_DB_URL`이 없으면 건너뛴다.

mod support;

use std::sync::Arc;

use chalkak_app::admin::AdminConfig;
use chalkak_app::admin::post::service::AdminPostCommandService;
use chalkak_app::admin::support::AdminDeps;
use chalkak_app::post::query::PostSort;
use chalkak_app::web::binding::LocalDate;
use support::Fx;

// AdminPostCommandDeletionTest#deletePost_approvedPost_excludesEveryUserReadModel
#[test]
fn delete_post_approved_post_excludes_every_user_read_model() {
    let Some(fx) = Fx::create() else { return };
    let author = fx.user();
    let topic = fx.open_topic();
    let post = fx.approved(author, topic);
    let admin: uuid::Uuid = fx
        .client()
        .query_one(
            "INSERT INTO admins (username, password) VALUES ('operator', $1) RETURNING id",
            &[&"$2y$12$GbP5jGCW/tL2TyqY9POOG.gHRFksjWnUfK/OtpzXE.U5.tyR1.MX6"],
        )
        .unwrap()
        .get(0);

    let today = || LocalDate::Valid(jiff::civil::date(2026, 10, 12));
    assert!(fx.query.get_post(post, author).is_ok());
    assert_eq!(
        fx.query
            .get_posts(today(), PostSort::Recent, None, 1, 20, None)
            .unwrap()
            .posts
            .len(),
        1
    );

    let deps = AdminDeps {
        pool: fx.ctx.pool.clone(),
        clock: fx.clock.clone(),
        logger: fx.ctx.logger.clone(),
        config: Arc::new(AdminConfig::for_tests(
            b"0123456789abcdef0123456789abcdef".to_vec(),
        )),
    };
    AdminPostCommandService::new(deps)
        .delete_post(post, admin, "운영 정책 위반")
        .unwrap();

    assert!(fx.query.get_post(post, author).is_err());
    assert!(
        fx.query
            .get_my_post_calendar(author, 2026, 10)
            .unwrap()
            .posts
            .is_empty()
    );
    assert!(
        fx.query
            .get_posts(today(), PostSort::Recent, None, 1, 20, None)
            .unwrap()
            .posts
            .is_empty()
    );
}
