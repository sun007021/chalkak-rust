//! 검수 대기 Slack 알림이 게시물 흐름에 붙었을 때의 동작. 원본은 `@TransactionalEventListener(AFTER_COMMIT)`라
//! 게시물이 커밋된 뒤에만 발송하고, 발송 실패·오류는 요청을 망치지 않으며 로그에도 원문을 남기지 않는다.

mod support;

use std::sync::{Arc, Mutex};

use chalkak_app::notification::slack::{
    NotificationMessage, NotificationMessageFactory, NotificationSender,
    PostModerationPendingNotifier,
};
use chalkak_app::post::PostWiring;
use chalkak_core::log::{Level, Logger};
use postgres::Client;
use support::{Capture, Fx, at, settings};
use uuid::Uuid;

/// 보낼 때 다른 연결로 게시물 상태를 읽어 기록한다. 커밋 전이면 행이 보이지 않는다.
struct Recording {
    reader: Mutex<Client>,
    sent: Mutex<Vec<(NotificationMessage, Option<String>)>>,
    outcome: Outcome,
}

#[derive(Clone, Copy)]
enum Outcome {
    Delivered,
    Failed,
    Panics,
}

impl NotificationSender for Recording {
    fn send(&self, message: &NotificationMessage) -> bool {
        let post_id: Uuid = message
            .action_uri
            .rsplit('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let status = self
            .reader
            .lock()
            .unwrap()
            .query_opt(
                "SELECT moderation_status::text FROM posts WHERE id = $1",
                &[&post_id],
            )
            .unwrap()
            .map(|r| r.get::<_, String>(0));
        self.sent.lock().unwrap().push((message.clone(), status));
        match self.outcome {
            Outcome::Delivered => true,
            Outcome::Failed => false,
            Outcome::Panics => panic!("https://hooks.slack.com/services/T000/B000/SECRET"),
        }
    }
}

struct Setup {
    fx: Fx,
    sender: Arc<Recording>,
    log: Capture,
    command: chalkak_app::post::PostCommandService,
}

fn setup(outcome: Outcome) -> Option<Setup> {
    let fx = Fx::create()?;
    let sender = Arc::new(Recording {
        reader: Mutex::new(fx.client()),
        sent: Mutex::default(),
        outcome,
    });
    let log = Capture::default();
    let logger = Arc::new(Logger::new(
        Box::new(log.clone()),
        Level::Info,
        fx.clock.clone(),
    ));
    let notifier = PostModerationPendingNotifier::new(
        NotificationMessageFactory::new("https://admin.example.com"),
        sender.clone(),
        logger,
    );
    let command =
        PostWiring::new(settings(), fx.store.clone(), Arc::new(notifier)).command_service(&fx.ctx);
    Some(Setup {
        fx,
        sender,
        log,
        command,
    })
}

// PostModerationNotificationTransactionTest#createPost_readyUpload_sendsAfterCommit: 구독자가 불린 때 게시물은 이미 커밋돼 있다
#[test]
fn 게시물이_커밋된_뒤에_관리자_알림을_보낸다() {
    let Some(s) = setup(Outcome::Delivered) else {
        return;
    };
    let user = s.fx.user();
    let topic = s.fx.open_topic();
    let upload = s.fx.upload_in(user, "READY");

    let result = s.command.create_post(user, topic, upload, None).unwrap();

    let sent = s.sender.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let (message, status) = &sent[0];
    assert_eq!(status.as_deref(), Some("PENDING"), "커밋 전에 보냈다");
    assert_eq!(
        message.action_uri,
        format!("https://admin.example.com/posts/{}", result.post_id)
    );
    assert!(s.log.lines().is_empty());
}

#[test]
fn 처리가_실패해_롤백되면_알림을_보내지_않는다() {
    let Some(s) = setup(Outcome::Delivered) else {
        return;
    };
    let user = s.fx.user();
    let topic = s.fx.open_topic();
    // 이미 만료된 업로드라 게시물을 만들지 못한다.
    let expired = s.fx.upload(user, "READY", &at(-60), None);

    assert!(s.command.create_post(user, topic, expired, None).is_err());

    assert!(s.sender.sent.lock().unwrap().is_empty());
    assert_eq!(s.fx.count("SELECT count(*) FROM posts"), 0);
}

// PostModerationPendingNotificationListenerTest: 실패·오류는 요청을 막지 않고 원문 없이 코드만 남긴다
#[test]
fn 발송이_실패해도_게시물은_만들어지고_실패_코드만_남긴다() {
    let Some(s) = setup(Outcome::Failed) else {
        return;
    };
    let user = s.fx.user();
    let topic = s.fx.open_topic();
    let upload = s.fx.upload_in(user, "READY");

    let result = s.command.create_post(user, topic, upload, None).unwrap();

    assert_eq!(s.fx.count("SELECT count(*) FROM posts"), 1);
    let lines = s.log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(&format!(
        "관리자 알림 발송 실패: postId={}, failureCode=NOTIFICATION_DELIVERY_FAILED",
        result.post_id
    )));
    assert!(lines[0].contains("\"level\":\"WARN\"") || lines[0].contains("WARN"));
}

#[test]
fn 발송기가_오류를_내도_게시물은_만들어지고_오류_원문은_남기지_않는다() {
    let Some(s) = setup(Outcome::Panics) else {
        return;
    };
    let user = s.fx.user();
    let topic = s.fx.open_topic();
    let upload = s.fx.upload_in(user, "READY");

    let result = s.command.create_post(user, topic, upload, None).unwrap();

    assert_eq!(s.fx.count("SELECT count(*) FROM posts"), 1);
    let lines = s.log.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(&format!(
        "관리자 알림 발송 오류: postId={}, failureCode=NOTIFICATION_DELIVERY_ERROR",
        result.post_id
    )));
    assert!(!lines[0].contains("SECRET") && !lines[0].contains("hooks.slack.com"));
}
