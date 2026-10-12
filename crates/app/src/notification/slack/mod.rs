//! 관리자 Slack 알림(검수 대기). 원본의 `NotificationMessageFactory`, `NotificationSender`,
//! `SlackIncomingWebhookNotificationSender`, `PostModerationPendingNotificationListener`.
//!
//! 게시물이 검수 대기(PENDING)가 되어 커밋된 뒤 [`PostModerationPendingNotifier::on_pending`]을 부르면 관리자 웹의 게시물 상세
//! 링크가 든 메시지를 Slack Incoming Webhook으로 보낸다. 같은 스레드에서 동기로 보내고, 실패해도 예외를 밖으로 내지 않는다
//! (게시물 처리 흐름을 막지 않는다). 전달을 끄면(`ADMIN_NOTIFICATION_DELIVERY_ENABLED=false`) 이 모듈의 구현은 만들지 않는다.
//!
//! 검수 대기 이벤트를 내는 쪽(게시물 도메인)과의 연결은 서버 조립에서 한다. 이 모듈은 게시물 도메인에 의존하지 않는다.

pub mod settings;

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use chalkak_core::log::Logger;
use chalkak_http::client::HttpClient;
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use crate::web::json::{format_instant, to_json};
use settings::SlackNotificationSettings;

const LISTENER_LOGGER: &str =
    "com.chalkak.backend.notification.service.PostModerationPendingNotificationListener";
const DELIVERY_FAILED: &str = "NOTIFICATION_DELIVERY_FAILED";
const DELIVERY_ERROR: &str = "NOTIFICATION_DELIVERY_ERROR";

/// 공급자 중립 알림 메시지(`NotificationMessage`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationMessage {
    pub title: String,
    pub body: String,
    pub action_label: String,
    pub action_uri: String,
    pub occurred_at: Timestamp,
}

/// 외부 전달 기술을 바꾸거나 더할 때 구현하는 포트(`NotificationSender`). 보냈으면 `true`.
pub trait NotificationSender: Send + Sync {
    fn send(&self, message: &NotificationMessage) -> bool;
}

/// `NotificationMessageFactory`
#[derive(Clone, Debug)]
pub struct NotificationMessageFactory {
    admin_web_base_url: String,
}

impl NotificationMessageFactory {
    /// 기준 주소의 마지막 슬래시는 뗀다.
    pub fn new(admin_web_base_url: &str) -> Self {
        NotificationMessageFactory {
            admin_web_base_url: admin_web_base_url
                .strip_suffix('/')
                .unwrap_or(admin_web_base_url)
                .to_owned(),
        }
    }

    /// 검수 대기 이벤트(`PostModerationPendingEvent(postId, occurredAt)`)의 메시지.
    pub fn create(&self, post_id: Uuid, occurred_at: Timestamp) -> NotificationMessage {
        NotificationMessage {
            title: "새 게시물 검수 요청".to_owned(),
            body: "새 게시물이 승인 대기 상태가 되었습니다.".to_owned(),
            action_label: "관리자 웹에서 검수하기".to_owned(),
            action_uri: format!("{}/posts/{post_id}", self.admin_web_base_url),
            occurred_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Slack Incoming Webhook
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct SlackPayload {
    text: String,
    blocks: Vec<SlackBlock>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum SlackBlock {
    Text(SlackTextBlock),
    Context(SlackContextBlock),
}

#[derive(Serialize)]
struct SlackTextBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: SlackText,
}

#[derive(Serialize)]
struct SlackContextBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    elements: Vec<SlackText>,
}

#[derive(Serialize)]
struct SlackText {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

fn text_block(kind: &'static str, text_kind: &'static str, text: &str) -> SlackBlock {
    SlackBlock::Text(SlackTextBlock {
        kind,
        text: SlackText {
            kind: text_kind,
            text: text.to_owned(),
        },
    })
}

/// 메시지를 Slack Block Kit 본문(JSON)으로 바꾼다.
pub fn slack_payload(message: &NotificationMessage) -> String {
    let payload = SlackPayload {
        text: message.title.clone(),
        blocks: vec![
            text_block("header", "plain_text", &message.title),
            text_block("section", "plain_text", &message.body),
            SlackBlock::Context(SlackContextBlock {
                kind: "context",
                elements: vec![SlackText {
                    kind: "plain_text",
                    text: format!("발생 시각: {}", format_instant(message.occurred_at)),
                }],
            }),
            text_block(
                "section",
                "mrkdwn",
                &format!("<{}|{}>", message.action_uri, message.action_label),
            ),
        ],
    };
    to_json(&payload)
}

/// `SlackIncomingWebhookNotificationSender`. 리다이렉트는 따르지 않고(HTTP 클라이언트의 기본), 2xx만 성공이다.
#[derive(Clone)]
pub struct SlackIncomingWebhookNotificationSender {
    client: HttpClient,
    webhook_url: String,
}

impl SlackIncomingWebhookNotificationSender {
    /// `webhook_url`은 이미 검증된 주소여야 한다(검증은 [`SlackNotificationSettings`]가 한다).
    pub fn new(client: HttpClient, webhook_url: impl Into<String>) -> Self {
        SlackIncomingWebhookNotificationSender {
            client,
            webhook_url: webhook_url.into(),
        }
    }

    /// 설정의 제한 시간으로 클라이언트를 만든다.
    pub fn from_settings(settings: &SlackNotificationSettings) -> Result<Self, String> {
        let client = HttpClient::builder()
            .connect_timeout(settings.connect_timeout)
            .read_timeout(settings.read_timeout)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self::new(client, settings.webhook_url.clone()))
    }
}

impl NotificationSender for SlackIncomingWebhookNotificationSender {
    fn send(&self, message: &NotificationMessage) -> bool {
        let body = slack_payload(message);
        // 통신 오류와 시간 초과는 모두 발송 실패다. 오류 원문에 주소가 들어갈 수 있어 남기지 않는다.
        match self.client.post_json(&self.webhook_url, &[], &body) {
            Ok(response) => response.is_success(),
            Err(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 검수 대기 알림 발송
// ---------------------------------------------------------------------------------------------

/// `PostModerationPendingNotificationListener`: 게시물 커밋이 끝난 뒤 관리자 알림을 외부 공급자에게 전달한다.
pub struct PostModerationPendingNotifier {
    factory: NotificationMessageFactory,
    sender: Arc<dyn NotificationSender>,
    logger: Arc<Logger>,
}

impl PostModerationPendingNotifier {
    pub fn new(
        factory: NotificationMessageFactory,
        sender: Arc<dyn NotificationSender>,
        logger: Arc<Logger>,
    ) -> Self {
        PostModerationPendingNotifier {
            factory,
            sender,
            logger,
        }
    }

    /// `sendPostModerationPending`. 어떤 실패도 호출한 쪽으로 전파하지 않는다.
    pub fn on_pending(&self, post_id: Uuid, occurred_at: Timestamp) {
        let sent = panic::catch_unwind(AssertUnwindSafe(|| {
            let message = self.factory.create(post_id, occurred_at);
            self.sender.send(&message)
        }));
        match sent {
            Ok(true) => {}
            Ok(false) => self.warn(post_id, "관리자 알림 발송 실패", DELIVERY_FAILED),
            // Webhook 주소나 공급자 응답이 오류에 포함될 수 있어 원문은 기록하지 않는다.
            Err(_) => self.warn(post_id, "관리자 알림 발송 오류", DELIVERY_ERROR),
        }
    }

    fn warn(&self, post_id: Uuid, what: &str, failure_code: &str) {
        self.logger.warn(LISTENER_LOGGER).log(&format!(
            "{what}: postId={post_id}, failureCode={failure_code}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::time::Duration;

    use chalkak_core::FixedClock;
    use chalkak_core::log::Level;

    use super::*;

    const POST_ID: Uuid = Uuid::from_u128(0x0199a000_0000_7000_8000_000000000001);
    const POST_ID_TEXT: &str = "0199a000-0000-7000-8000-000000000001";

    fn occurred_at() -> Timestamp {
        "2026-09-02T00:00:00Z".parse().unwrap()
    }

    fn message() -> NotificationMessage {
        NotificationMessageFactory::new("https://admin.example.com").create(POST_ID, occurred_at())
    }

    // NotificationMessageFactoryTest#create_postModerationPending_createsAdminPostDetailLink
    #[test]
    fn 검수_알림은_관리자_웹의_게시물_상세_주소를_만든다() {
        let m = message();
        assert_eq!(m.title, "새 게시물 검수 요청");
        assert!(m.body.contains("승인 대기"));
        assert_eq!(m.action_label, "관리자 웹에서 검수하기");
        assert_eq!(
            m.action_uri,
            format!("https://admin.example.com/posts/{POST_ID_TEXT}")
        );
        assert_eq!(m.occurred_at, occurred_at());
    }

    // NotificationMessageFactoryTest#create_baseUrlWithTrailingSlash_doesNotDuplicateSlash
    #[test]
    fn 기준_주소의_마지막_슬래시는_떼고_붙인다() {
        let m = NotificationMessageFactory::new("https://admin.example.com/")
            .create(POST_ID, occurred_at());
        assert_eq!(
            m.action_uri,
            format!("https://admin.example.com/posts/{POST_ID_TEXT}")
        );
    }

    // SlackIncomingWebhookNotificationSenderTest#send_successfulResponse_postsBlockKitAndReturnsSent
    #[test]
    fn block_kit_본문은_검수_내용과_정확한_링크를_담는다() {
        let json: serde_json::Value = serde_json::from_str(&slack_payload(&message())).unwrap();
        assert_eq!(json["text"], "새 게시물 검수 요청");
        assert_eq!(json["blocks"][0]["type"], "header");
        assert_eq!(json["blocks"][0]["text"]["type"], "plain_text");
        assert_eq!(json["blocks"][0]["text"]["text"], "새 게시물 검수 요청");
        assert_eq!(json["blocks"][1]["type"], "section");
        assert_eq!(
            json["blocks"][1]["text"]["text"],
            "새 게시물이 승인 대기 상태가 되었습니다."
        );
        assert_eq!(json["blocks"][2]["type"], "context");
        assert_eq!(
            json["blocks"][2]["elements"][0]["text"],
            "발생 시각: 2026-09-02T00:00:00Z"
        );
        assert_eq!(json["blocks"][3]["text"]["type"], "mrkdwn");
        assert_eq!(
            json["blocks"][3]["text"]["text"],
            format!("<https://admin.example.com/posts/{POST_ID_TEXT}|관리자 웹에서 검수하기>")
        );
        assert_eq!(json["blocks"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn 본문의_필드_순서는_원본_레코드와_같다() {
        let body = slack_payload(&message());
        assert!(body.starts_with(
            r#"{"text":"새 게시물 검수 요청","blocks":[{"type":"header","text":{"type":"plain_text","text":"새 게시물 검수 요청"}},"#
        ));
        assert!(body.contains(r#"{"type":"context","elements":[{"type":"plain_text","text":"발생 시각: 2026-09-02T00:00:00Z"}]}"#));
    }

    // ---- 가짜 Webhook 서버 ----

    /// 한 번의 요청을 받아 `status`로 답하고 요청 원문을 돌려주는 서버.
    fn fake_webhook(status: u16) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/services/T/B/SECRET_WEBHOOK_PATH",
            listener.local_addr().unwrap()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply =
                format!("HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });
        (url, handle)
    }

    fn client() -> HttpClient {
        HttpClient::builder()
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(3))
            .build()
            .unwrap()
    }

    #[test]
    fn 성공_응답은_json으로_post하고_true를_돌려준다() {
        let (url, server) = fake_webhook(200);
        let sender = SlackIncomingWebhookNotificationSender::new(client(), url);
        assert!(sender.send(&message()));
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /services/T/B/SECRET_WEBHOOK_PATH HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json\r\n")
        );
        assert!(request.ends_with(&slack_payload(&message())));
    }

    // SlackIncomingWebhookNotificationSenderTest#send_unsuccessfulResponse_returnsFalse
    #[test]
    fn 이백번대가_아닌_응답은_발송_실패다() {
        for status in [301, 400, 429, 500] {
            let (url, server) = fake_webhook(status);
            let sender = SlackIncomingWebhookNotificationSender::new(client(), url);
            assert!(!sender.send(&message()), "{status}");
            server.join().unwrap();
        }
    }

    // SlackIncomingWebhookNotificationSenderTest#send_timeout_returnsFalse
    #[test]
    fn 응답_시간_초과는_발송_실패다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/services/T/B/X", listener.local_addr().unwrap());
        let _hold = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(600));
        });
        let client = HttpClient::builder()
            .connect_timeout(Duration::from_millis(200))
            .read_timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        assert!(!SlackIncomingWebhookNotificationSender::new(client, url).send(&message()));
    }

    // SlackIncomingWebhookNotificationSenderTest#send_networkError_returnsFalse
    #[test]
    fn 연결_실패도_발송_실패다() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/services/T/B/X", listener.local_addr().unwrap());
        drop(listener);
        assert!(!SlackIncomingWebhookNotificationSender::new(client(), url).send(&message()));
    }

    // ---- 발송 흐름 ----

    struct FakeSender {
        result: Result<bool, &'static str>,
        sent: Mutex<Vec<NotificationMessage>>,
    }

    impl NotificationSender for FakeSender {
        fn send(&self, message: &NotificationMessage) -> bool {
            self.sent.lock().unwrap().push(message.clone());
            match self.result {
                Ok(sent) => sent,
                Err(secret) => panic!("{secret}"),
            }
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn notifier(
        result: Result<bool, &'static str>,
    ) -> (PostModerationPendingNotifier, Arc<FakeSender>, Capture) {
        let capture = Capture::default();
        let logger = Arc::new(Logger::new(
            Box::new(capture.clone()),
            Level::Info,
            Arc::new(FixedClock::new("2026-09-02T00:00:00Z".parse().unwrap())),
        ));
        let sender = Arc::new(FakeSender {
            result,
            sent: Mutex::new(Vec::new()),
        });
        let notifier = PostModerationPendingNotifier::new(
            NotificationMessageFactory::new("https://admin.example.com"),
            sender.clone(),
            logger,
        );
        (notifier, sender, capture)
    }

    fn logged(capture: &Capture) -> String {
        String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
    }

    // PostModerationPendingNotificationListenerTest#sendPostModerationPending_afterCommit_sendsMessage
    #[test]
    fn 이벤트를_받으면_만든_메시지를_전송한다() {
        let (notifier, sender, capture) = notifier(Ok(true));
        notifier.on_pending(POST_ID, occurred_at());
        assert_eq!(*sender.sent.lock().unwrap(), vec![message()]);
        assert_eq!(logged(&capture), "", "성공하면 로그를 남기지 않는다");
    }

    // PostModerationPendingNotificationListenerTest#sendPostModerationPending_senderReturnsFalse_doesNotThrow
    #[test]
    fn 전송이_실패를_돌려주면_경고만_남기고_전파하지_않는다() {
        let (notifier, _sender, capture) = notifier(Ok(false));
        notifier.on_pending(POST_ID, occurred_at());
        let log = logged(&capture);
        assert!(log.contains(&format!(
            "관리자 알림 발송 실패: postId={POST_ID_TEXT}, failureCode=NOTIFICATION_DELIVERY_FAILED"
        )), "{log}");
        assert!(log.contains(r#""level":"WARN""#), "{log}");
        assert!(log.contains(LISTENER_LOGGER), "{log}");
    }

    // PostModerationPendingNotificationListenerTest#sendPostModerationPending_senderThrows_doesNotExposeSecret
    #[test]
    fn 전송이_패닉해도_전파하지_않고_원문을_남기지_않는다() {
        let secret = "https://hooks.slack.com/services/SECRET/SECRET/SECRET";
        let (notifier, _sender, capture) = notifier(Err(secret));
        notifier.on_pending(POST_ID, occurred_at());
        let log = logged(&capture);
        assert!(log.contains("NOTIFICATION_DELIVERY_ERROR"), "{log}");
        assert!(!log.contains("hooks.slack.com"), "{log}");
        assert!(!log.contains("SECRET"), "{log}");
    }
}
