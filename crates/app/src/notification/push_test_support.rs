//! 푸시 발행 DB 테스트가 함께 쓰는 보조(테스트에서만 쓴다): 로그 수집, 가짜 발행기, 자주 쓰는 INSERT.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use chalkak_core::FixedClock;
use chalkak_core::log::{Level, Logger};
use jiff::{SignedDuration, Timestamp};
use postgres::Client;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::sqs_push::{PublisherFailure, PushMessage, PushMessagePublisher, PushPublicationResult};

pub fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

pub fn plus(at: Timestamp, seconds: i64) -> Timestamp {
    at.checked_add(SignedDuration::from_secs(seconds)).unwrap()
}

/// 로그를 한 줄씩 모으는 sink.
#[derive(Clone, Default)]
pub struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Lines {
    /// 기록된 로그 줄 그대로(키 순서를 보려고 쓴다).
    pub fn raw(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn events(&self) -> Vec<Map<String, Value>> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|value| value.as_object().unwrap().clone())
            .collect()
    }

    /// `stage`가 같은 이벤트들.
    pub fn at_stage(&self, stage: &str) -> Vec<Map<String, Value>> {
        self.events()
            .into_iter()
            .filter(|event| event.get("stage").and_then(Value::as_str) == Some(stage))
            .collect()
    }

    /// 메시지가 같은 이벤트들의 수준.
    pub fn levels_of(&self, message: &str) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|event| event["message"] == message)
            .map(|event| event["level"].as_str().unwrap().to_owned())
            .collect()
    }
}

pub fn logger(lines: &Lines, clock: &Arc<FixedClock>) -> Arc<Logger> {
    Arc::new(Logger::new(
        Box::new(lines.clone()),
        Level::Info,
        clock.clone(),
    ))
}

type Hook = dyn Fn(&PushMessage) + Send + Sync;

/// 정해 둔 결과를 돌려주고 받은 메시지를 기억하는 발행기. 순서 목록을 다 쓰면 `fallback`을 돌려준다.
pub struct FakePublisher {
    queue: Mutex<VecDeque<Result<PushPublicationResult, PublisherFailure>>>,
    fallback: Mutex<Result<PushPublicationResult, PublisherFailure>>,
    calls: Mutex<Vec<PushMessage>>,
    hook: Mutex<Option<Arc<Hook>>>,
}

impl FakePublisher {
    pub fn accepting() -> Arc<Self> {
        Arc::new(FakePublisher {
            queue: Mutex::new(VecDeque::new()),
            fallback: Mutex::new(Ok(PushPublicationResult::accepted("sqs-message-id"))),
            calls: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
        })
    }

    pub fn set_fallback(&self, result: PushPublicationResult) {
        *self.fallback.lock().unwrap() = Ok(result);
    }

    pub fn queue(&self, results: Vec<Result<PushPublicationResult, PublisherFailure>>) {
        *self.queue.lock().unwrap() = results.into();
    }

    /// 발행기가 호출될 때마다(결과를 정하기 전에) 실행할 동작.
    pub fn on_publish(&self, hook: impl Fn(&PushMessage) + Send + Sync + 'static) {
        *self.hook.lock().unwrap() = Some(Arc::new(hook));
    }

    pub fn calls(&self) -> Vec<PushMessage> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}

impl PushMessagePublisher for FakePublisher {
    fn publish(&self, message: &PushMessage) -> Result<PushPublicationResult, PublisherFailure> {
        self.calls.lock().unwrap().push(message.clone());
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(message);
        }
        match self.queue.lock().unwrap().pop_front() {
            Some(result) => result,
            None => self.fallback.lock().unwrap().clone(),
        }
    }
}

pub fn failure(exception: &str) -> PublisherFailure {
    PublisherFailure {
        exception: exception.to_owned(),
    }
}

/// ACTIVE 회원 한 명.
pub fn insert_user(client: &mut Client, id: Uuid) {
    client
        .execute(
            "INSERT INTO users (id, email, status, signature_original_storage_key) \
             VALUES ($1, $2, 'ACTIVE', $3)",
            &[
                &id,
                &format!("{id}@push-test.example"),
                &format!("sig/{id}"),
            ],
        )
        .unwrap();
}
