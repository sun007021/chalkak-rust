//! 분석 모듈 단위 시험이 함께 쓰는 로그 수집기.

use std::sync::{Arc, Mutex};

use chalkak_core::FixedClock;
use chalkak_core::log::{Level, Logger};
use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// 로그 한 줄씩 모으는 sink.
#[derive(Clone, Default)]
pub struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 최상위 키를 JSON에 쓰인 순서대로 모은다(`serde_json::Map`은 빌드 설정에 따라 순서를 보존하지 않는다).
struct Keys(Vec<String>);

impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Keys;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(Keys(keys))
            }
        }
        deserializer.deserialize_map(V)
    }
}

impl Lines {
    fn raw(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn events(&self) -> Vec<Value> {
        self.raw()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// `index`번째 줄에서 로거가 붙이는 공통 키(`level_value`까지)를 뺀 키를 JSON 순서대로 돌려준다.
    pub fn keys(&self, index: usize) -> Vec<String> {
        let keys: Keys = serde_json::from_str(&self.raw()[index]).unwrap();
        let start = keys
            .0
            .iter()
            .position(|k| k == "level_value")
            .expect("level_value는 항상 있다")
            + 1;
        keys.0.into_iter().skip(start).collect()
    }
}

pub fn logger(lines: &Lines) -> Arc<Logger> {
    let clock = Arc::new(FixedClock::new("2026-10-19T00:00:00Z".parse().unwrap()));
    Arc::new(Logger::new(Box::new(lines.clone()), Level::Info, clock))
}

/// 접근 로그 지표가 없는 리더.
pub struct NoAccessLogs;

impl super::service::AccessLogMetricsReader for NoAccessLogs {
    fn read(
        &self,
        _: jiff::civil::Date,
    ) -> Result<Option<super::service::AccessLogMetrics>, crate::web::ApiError> {
        Ok(None)
    }
}

/// 바로 실행하지 않고 쌓아 두어 "진행 중" 상태를 만드는 실행기.
#[derive(Default)]
pub struct QueueExecutor {
    pub tasks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    pub reject: bool,
}

impl super::runner::TaskExecutor for QueueExecutor {
    fn execute(&self, task: Box<dyn FnOnce() + Send>) -> Result<(), crate::web::ApiError> {
        if self.reject {
            return Err(crate::web::ApiError::Internal("queue full".into()));
        }
        self.tasks.lock().unwrap().push(task);
        Ok(())
    }
}

impl QueueExecutor {
    pub fn len(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    pub fn run_first(&self) {
        let task = self.tasks.lock().unwrap().remove(0);
        task();
    }
}
