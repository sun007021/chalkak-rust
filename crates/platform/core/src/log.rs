//! logstash 형식 JSON 한 줄 로그.
//!
//! 원본: `logback-spring.xml`이 dev·prod에서 `StructuredLogEncoder`(`format = logstash`)로 JSON 파일 로그를 남기고
//! CloudWatch Agent가 수집한다. 대시보드 쿼리(`deploy/monitoring/dashboard.json`)가 `type`, `route`, `userId`,
//! `waitSeconds` 같은 최상위 필드를 직접 쓰므로 필드 이름과 위치가 같아야 한다.
//!
//! Spring Boot 4.1.0 `LogstashStructuredLogFormatter`의 출력 규칙(직접 실행해 확인):
//!
//! ```text
//! {"@timestamp":..,"@version":"1","message":..,"logger_name":..,"thread_name":..,"level":..,"level_value":..,
//!  <MDC: requestId>, <key-value 쌍들, 추가한 순서>, ["tags":[..]], ["stack_trace":..]}
//! ```
//!
//! - `@timestamp`: `DateTimeFormatter.ISO_OFFSET_DATE_TIME`(JVM 기본 시간대, 소수 초는 끝의 0을 뗀 최대 9자리,
//!   오프셋 0이면 `Z`). 시간대는 [`Logger::with_time_zone`]으로 정한다(기본은 시스템 시간대).
//! - 문자열 이스케이프: `" \ \b \f \n \r \t`는 짧은 이스케이프, 그 밖의 `Character.isISOControl`(U+0000-U+001F,
//!   U+007F-U+009F)은 `\u00XX`(16진수 대문자). `/`와 U+00A0 이상(U+2028 포함)은 그대로 둔다.
//!   (오류 응답 본문을 만드는 Jackson과는 규칙이 달라 [`crate::error`]와 구현을 따로 둔다.)
//!
//! 다른 점: Rust에는 예외 객체가 없으므로 `stack_trace`에는 오류와 `source()` 체인의 메시지를 줄 단위로 싣는다.
//! 로그 파일 회전(50MB·14일·gz)은 구현하지 않는다. 배포 쪽 logrotate 같은 도구에 맡긴다.

use std::error::Error;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::clock::Clock;
use crate::request_id;

/// 원본 `LogFields`의 키 이름과 값.
pub mod fields {
    pub const REQUEST_ID: &str = "requestId";
    pub const USER_ID: &str = "userId";
    pub const TYPE: &str = "type";
    pub const METHOD: &str = "method";
    pub const ROUTE: &str = "route";
    pub const STATUS: &str = "status";
    pub const DURATION_MS: &str = "durationMs";
    pub const ERROR_CODE: &str = "errorCode";
    pub const EXCEPTION: &str = "exception";
    pub const EVENT: &str = "event";
    pub const POST_ID: &str = "postId";
    pub const TOPIC_DATE: &str = "topicDate";
    pub const SORT: &str = "sort";
    pub const PAGE: &str = "page";
    pub const PAGE_SIZE: &str = "pageSize";
    pub const WAIT_SECONDS: &str = "waitSeconds";
    pub const HEAP_USED_BYTES: &str = "heapUsedBytes";
    pub const HEAP_MAX_BYTES: &str = "heapMaxBytes";
    pub const GC_TIME_MS_TOTAL: &str = "gcTimeMsTotal";
    pub const THREADS: &str = "threads";
    pub const HIKARI_ACTIVE: &str = "hikariActive";
    pub const HIKARI_PENDING: &str = "hikariPending";

    pub const TYPE_ACCESS: &str = "access";
    pub const TYPE_ERROR: &str = "error";
    pub const TYPE_RUNTIME: &str = "runtime";
    pub const TYPE_MODERATION: &str = "moderation";
    /// 라우트가 매칭되지 않은 요청의 `route` 값.
    pub const UNMATCHED_ROUTE: &str = "UNMATCHED";
}

/// 원본 `chalkak.logging.file`의 기본값.
pub const DEFAULT_LOG_FILE: &str = "/opt/chalkak/logs/application.log";

/// 로그 수준. 원본 루트 로거는 INFO다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// Logback `Level.toInt()`.
    pub const fn value(self) -> u32 {
        match self {
            Self::Trace => 5_000,
            Self::Debug => 10_000,
            Self::Info => 20_000,
            Self::Warn => 30_000,
            Self::Error => 40_000,
        }
    }
}

/// 로그 이벤트 한 건. [`format_event`]가 JSON 한 줄로 바꾼다.
#[derive(Clone, Debug)]
pub struct LogEvent {
    pub timestamp: Timestamp,
    pub level: Level,
    pub logger: String,
    pub thread: String,
    pub message: String,
    /// MDC `requestId`.
    pub request_id: Option<String>,
    /// key-value 쌍(추가한 순서).
    pub fields: Vec<(String, Value)>,
    pub stack_trace: Option<String>,
}

/// 예약된 최상위 키. key-value가 같은 이름을 쓰면 JSON 키가 겹치므로 뒤늦은 쪽을 버린다.
const RESERVED: [&str; 10] = [
    "@timestamp",
    "@version",
    "message",
    "logger_name",
    "thread_name",
    "level",
    "level_value",
    fields::REQUEST_ID,
    "tags",
    "stack_trace",
];

/// 이벤트를 logstash 형식 JSON 한 줄(개행 없음)로 만든다.
pub fn format_event(event: &LogEvent, tz: &TimeZone) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("{\"@timestamp\":");
    write_string(&mut out, &format_timestamp(event.timestamp, tz));
    out.push_str(",\"@version\":\"1\",\"message\":");
    write_string(&mut out, &event.message);
    out.push_str(",\"logger_name\":");
    write_string(&mut out, &event.logger);
    out.push_str(",\"thread_name\":");
    write_string(&mut out, &event.thread);
    out.push_str(",\"level\":\"");
    out.push_str(event.level.as_str());
    let _ = write!(out, "\",\"level_value\":{}", event.level.value());
    if let Some(id) = &event.request_id {
        out.push_str(",\"requestId\":");
        write_string(&mut out, id);
    }
    let mut seen: Vec<&str> = Vec::new();
    for (key, value) in &event.fields {
        if RESERVED.contains(&key.as_str()) || seen.contains(&key.as_str()) {
            continue;
        }
        seen.push(key);
        out.push(',');
        write_string(&mut out, key);
        out.push(':');
        write_value(&mut out, value);
    }
    if let Some(trace) = &event.stack_trace {
        out.push_str(",\"stack_trace\":");
        write_string(&mut out, trace);
    }
    out.push('}');
    out
}

/// `ISO_OFFSET_DATE_TIME`: `2026-10-13T01:39:46.088706915+09:00`, 오프셋 0이면 `Z`.
pub fn format_timestamp(ts: Timestamp, tz: &TimeZone) -> String {
    let dt = tz.to_datetime(ts);
    let mut out = String::with_capacity(35);
    let _ = write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    let nanos = dt.subsec_nanosecond();
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    let offset = tz.to_offset(ts).seconds();
    if offset == 0 {
        out.push('Z');
    } else {
        let abs = offset.unsigned_abs();
        let _ = write!(
            out,
            "{}{:02}:{:02}",
            if offset < 0 { '-' } else { '+' },
            abs / 3600,
            abs % 3600 / 60
        );
        if !abs.is_multiple_of(60) {
            let _ = write!(out, ":{:02}", abs % 60);
        }
    }
    out
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                // char::is_control == Character.isISOControl (Cc)
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, v);
            }
            out.push('}');
        }
    }
}

/// 한 줄 로그를 쓰는 로거. 줄 단위로 잠가서 여러 스레드의 출력이 섞이지 않는다.
pub struct Logger {
    sink: Mutex<Box<dyn Write + Send>>,
    min_level: Level,
    clock: Arc<dyn Clock>,
    tz: TimeZone,
}

impl Logger {
    /// 임의의 출력 대상으로 만든다.
    pub fn new(sink: Box<dyn Write + Send>, min_level: Level, clock: Arc<dyn Clock>) -> Self {
        Self {
            sink: Mutex::new(sink),
            min_level,
            clock,
            tz: TimeZone::system(),
        }
    }

    /// 표준 출력으로 남긴다.
    pub fn stdout(min_level: Level, clock: Arc<dyn Clock>) -> Self {
        Self::new(Box::new(std::io::stdout()), min_level, clock)
    }

    /// 파일에 이어 쓴다(없으면 만든다, 상위 디렉터리 포함). `RollingFileAppender`의 기본 append와 같다.
    pub fn to_file(
        path: impl AsRef<Path>,
        min_level: Level,
        clock: Arc<dyn Clock>,
    ) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self::new(Box::new(file), min_level, clock))
    }

    /// `@timestamp`에 쓸 시간대. 기본은 시스템 시간대다(원본 JVM 기본 시간대).
    #[must_use]
    pub fn with_time_zone(mut self, tz: TimeZone) -> Self {
        self.tz = tz;
        self
    }

    pub fn trace(&self, logger: &str) -> EventBuilder<'_> {
        self.event(Level::Trace, logger)
    }
    pub fn debug(&self, logger: &str) -> EventBuilder<'_> {
        self.event(Level::Debug, logger)
    }
    pub fn info(&self, logger: &str) -> EventBuilder<'_> {
        self.event(Level::Info, logger)
    }
    pub fn warn(&self, logger: &str) -> EventBuilder<'_> {
        self.event(Level::Warn, logger)
    }
    pub fn error(&self, logger: &str) -> EventBuilder<'_> {
        self.event(Level::Error, logger)
    }

    /// `logger`는 원본 로거 이름(`chalkak.access`, `chalkak.runtime` 등)이다.
    pub fn event(&self, level: Level, logger: &str) -> EventBuilder<'_> {
        EventBuilder {
            logger: self,
            level,
            name: logger.to_owned(),
            fields: Vec::new(),
            stack_trace: None,
        }
    }

    fn emit(&self, event: &LogEvent) {
        let mut line = format_event(event, &self.tz);
        line.push('\n');
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        // 로그 쓰기 실패를 로그로 남길 수는 없다. 요청 처리를 막지 않도록 버린다.
        let _ = sink.write_all(line.as_bytes());
        let _ = sink.flush();
    }
}

/// `LoggingEventBuilder`에 해당. `.log("메시지")`로 마무리한다.
#[must_use = "log()를 호출해야 기록된다"]
pub struct EventBuilder<'a> {
    logger: &'a Logger,
    level: Level,
    name: String,
    fields: Vec<(String, Value)>,
    stack_trace: Option<String>,
}

impl EventBuilder<'_> {
    /// `addKeyValue`. 값은 문자열·정수·실수·불리언·`serde_json::Value`를 받는다.
    pub fn field(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.push((key.to_owned(), value.into()));
        self
    }

    /// `setCause`. 오류와 `source()` 체인을 `stack_trace`에 싣는다.
    pub fn error(mut self, err: &(dyn Error + 'static)) -> Self {
        let mut trace = String::new();
        let mut current: Option<&(dyn Error + 'static)> = Some(err);
        let mut first = true;
        while let Some(e) = current {
            let _ = writeln!(trace, "{}{e}", if first { "" } else { "Caused by: " });
            first = false;
            current = e.source();
        }
        self.stack_trace = Some(trace);
        self
    }

    pub fn log(self, message: &str) {
        if self.level < self.logger.min_level {
            return;
        }
        let thread = std::thread::current();
        let event = LogEvent {
            timestamp: self.logger.clock.now(),
            level: self.level,
            logger: self.name,
            thread: thread
                .name()
                .map_or_else(|| format!("{:?}", thread.id()), str::to_owned),
            message: message.to_owned(),
            request_id: request_id::current(),
            fields: self.fields,
            stack_trace: self.stack_trace,
        };
        self.logger.emit(&event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use serde_json::json;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn event(message: &str) -> LogEvent {
        LogEvent {
            timestamp: ts("2026-10-12T16:38:58.198201888Z"),
            level: Level::Info,
            logger: "chalkak.access".into(),
            thread: "main".into(),
            message: message.into(),
            request_id: None,
            fields: vec![],
            stack_trace: None,
        }
    }

    // 아래 기대 문자열은 Spring Boot 4.1.0 StructuredLogEncoder(logstash)를 직접 실행해 얻은 출력이다.
    // (@timestamp·thread_name은 고정값으로 바꿨다.)

    // RequestIdFilter.logAccess
    #[test]
    fn 접근_로그_한_줄은_spring_logstash_출력과_같다() {
        let mut e = event("access");
        e.request_id = Some("0b9d6f0e-2a4b-4a5e-9f3a-1c2d3e4f5a6b".into());
        e.fields = vec![
            ("type".into(), json!("access")),
            ("method".into(), json!("GET")),
            ("route".into(), json!("/api/v1/posts/{postId}")),
            ("status".into(), json!(200)),
            ("durationMs".into(), json!(12)),
            ("userId".into(), json!("u-1")),
        ];
        assert_eq!(
            format_event(&e, &TimeZone::UTC),
            r#"{"@timestamp":"2026-10-12T16:38:58.198201888Z","@version":"1","message":"access","logger_name":"chalkak.access","thread_name":"main","level":"INFO","level_value":20000,"requestId":"0b9d6f0e-2a4b-4a5e-9f3a-1c2d3e4f5a6b","type":"access","method":"GET","route":"/api/v1/posts/{postId}","status":200,"durationMs":12,"userId":"u-1"}"#
        );
    }

    // RuntimeSnapshotLogger: 요청 밖이라 requestId가 없고 한글 메시지
    #[test]
    fn 런타임_로그는_requestid_없이_한글_메시지() {
        let mut e = event("런타임 상태");
        e.logger = "chalkak.runtime".into();
        e.fields = vec![
            ("type".into(), json!("runtime")),
            ("heapUsedBytes".into(), json!(123_456_789_u64)),
            ("threads".into(), json!(31)),
        ];
        assert_eq!(
            format_event(&e, &TimeZone::UTC),
            r#"{"@timestamp":"2026-10-12T16:38:58.198201888Z","@version":"1","message":"런타임 상태","logger_name":"chalkak.runtime","thread_name":"main","level":"INFO","level_value":20000,"type":"runtime","heapUsedBytes":123456789,"threads":31}"#
        );
    }

    // GlobalExceptionHandler.handleUnexpectedException: ERROR + 불리언·실수·배열 값 + stack_trace는 맨 끝
    #[test]
    fn 오류_로그는_stack_trace가_맨_끝이다() {
        let mut e = event("처리되지 않은 예외");
        e.level = Level::Error;
        e.logger = "chalkak.x".into();
        e.request_id = Some("r-\"1\"".into());
        e.fields = vec![
            ("type".into(), json!("error")),
            ("flag".into(), json!(true)),
            ("ratio".into(), json!(1.5)),
            ("obj".into(), json!([1, 2])),
        ];
        e.stack_trace = Some("boom\n".into());
        assert_eq!(
            format_event(&e, &TimeZone::UTC),
            r#"{"@timestamp":"2026-10-12T16:38:58.198201888Z","@version":"1","message":"처리되지 않은 예외","logger_name":"chalkak.x","thread_name":"main","level":"ERROR","level_value":40000,"requestId":"r-\"1\"","type":"error","flag":true,"ratio":1.5,"obj":[1,2],"stack_trace":"boom\n"}"#
        );
    }

    // JsonWriter: isISOControl -> \u%04X, / 와 U+00A0 이상은 그대로
    #[test]
    fn 문자열_이스케이프는_spring_jsonwriter와_같다() {
        let e = event(
            "A\u{1f}\u{1b}\u{8}\u{c}\r\u{0} \u{7f}\u{80}\u{9f}\u{a0}\u{2028}\u{2029} \"q\" \\ / é \n\t",
        );
        let line = format_event(&e, &TimeZone::UTC);
        let expected_message = concat!(
            r#""message":"A\u001F\u001B\b\f\r\u0000 \u007F\u0080\u009F"#,
            "\u{a0}\u{2028}\u{2029}",
            r#" \"q\" \\ / é \n\t""#
        );
        assert!(line.contains(expected_message), "{line}");
    }

    // ISO_OFFSET_DATE_TIME
    #[test]
    fn 타임스탬프_형식() {
        let utc = TimeZone::UTC;
        assert_eq!(
            format_timestamp(ts("2026-10-12T00:00:00Z"), &utc),
            "2026-10-12T00:00:00Z"
        );
        assert_eq!(
            format_timestamp(ts("2026-10-12T00:00:00.5Z"), &utc),
            "2026-10-12T00:00:00.5Z"
        );
        assert_eq!(
            format_timestamp(ts("2026-10-12T00:00:00.120Z"), &utc),
            "2026-10-12T00:00:00.12Z"
        );
        assert_eq!(
            format_timestamp(ts("2026-10-12T00:00:00.000000001Z"), &utc),
            "2026-10-12T00:00:00.000000001Z"
        );
        let seoul = TimeZone::get("Asia/Seoul").unwrap();
        assert_eq!(
            format_timestamp(ts("2026-10-12T16:39:46.088706915Z"), &seoul),
            "2026-10-13T01:39:46.088706915+09:00"
        );
        let minus = TimeZone::get("America/New_York").unwrap();
        assert_eq!(
            format_timestamp(ts("2026-01-12T05:00:00Z"), &minus),
            "2026-01-12T00:00:00-05:00"
        );
    }

    #[test]
    fn 예약된_키와_중복_키는_버린다() {
        let mut e = event("m");
        e.fields = vec![
            ("message".into(), json!("dup")),
            ("level".into(), json!("dup")),
            ("requestId".into(), json!("dup")),
            ("a".into(), json!(1)),
            ("a".into(), json!(2)),
        ];
        let line = format_event(&e, &TimeZone::UTC);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["message"], "m");
        assert_eq!(parsed["level"], "INFO");
        assert_eq!(parsed["a"], 1);
        assert!(parsed.get("requestId").is_none());
        assert_eq!(line.matches("\"a\"").count(), 1);
    }

    #[test]
    fn level_값은_logback과_같다() {
        let values: Vec<u32> = [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ]
        .iter()
        .map(|l| l.value())
        .collect();
        assert_eq!(values, [5_000, 10_000, 20_000, 30_000, 40_000]);
        assert!(Level::Warn > Level::Info);
    }

    // --- Logger ---

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

    impl Capture {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn logger(min: Level) -> (Logger, Capture, Arc<FixedClock>) {
        let capture = Capture::default();
        let clock = Arc::new(FixedClock::new(ts("2026-10-12T16:38:58Z")));
        let logger = Logger::new(Box::new(capture.clone()), min, clock.clone())
            .with_time_zone(TimeZone::UTC);
        (logger, capture, clock)
    }

    // JsonFileLogTest: MDC requestId와 key-value가 최상위 필드로 한 번씩 담긴 JSON 한 줄
    #[test]
    fn logger_는_requestid와_필드가_최상위인_json_한_줄을_남긴다() {
        let (logger, capture, _) = logger(Level::Info);
        let _guard = request_id::enter("request-id-1");
        logger
            .info("chalkak.access")
            .field(fields::TYPE, fields::TYPE_ACCESS)
            .field(fields::ROUTE, "/api/v1/posts/{postId}")
            .field(fields::STATUS, 200)
            .log("access");
        let lines = capture.lines();
        assert_eq!(lines.len(), 1);
        let json: Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(json.get("@timestamp").is_some());
        assert_eq!(json["level"], "INFO");
        assert_eq!(json["message"], "access");
        assert_eq!(json["logger_name"], "chalkak.access");
        assert_eq!(json["requestId"], "request-id-1");
        assert_eq!(json["type"], "access");
        assert_eq!(json["route"], "/api/v1/posts/{postId}");
        assert_eq!(json["status"], 200);
        assert_eq!(json["@timestamp"], "2026-10-12T16:38:58Z");
    }

    #[test]
    fn 요청_밖에서는_requestid_키가_없다() {
        let (logger, capture, _) = logger(Level::Info);
        logger.info("chalkak.runtime").log("런타임 상태");
        let json: Value = serde_json::from_str(&capture.lines()[0]).unwrap();
        assert!(json.get("requestId").is_none());
    }

    #[test]
    fn 최소_수준_미만은_남기지_않는다() {
        let (logger, capture, _) = logger(Level::Info);
        logger.debug("x").log("hidden");
        logger.trace("x").log("hidden");
        logger.warn("x").log("shown");
        let lines = capture.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"level\":\"WARN\""));
    }

    #[test]
    fn 시계를_옮기면_타임스탬프가_따라간다() {
        let (logger, capture, clock) = logger(Level::Info);
        logger.info("x").log("a");
        clock.advance(std::time::Duration::from_millis(1_500));
        logger.info("x").log("b");
        let lines = capture.lines();
        assert!(lines[0].contains("\"@timestamp\":\"2026-10-12T16:38:58Z\""));
        assert!(lines[1].contains("\"@timestamp\":\"2026-10-12T16:38:59.5Z\""));
    }

    #[test]
    fn error_는_source_체인을_stack_trace로_싣는다() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("저장 실패")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }
        let (logger, capture, _) = logger(Level::Info);
        let err = Outer(std::io::Error::other("disk full"));
        logger
            .error("chalkak.x")
            .error(&err)
            .log("처리되지 않은 예외");
        let json: Value = serde_json::from_str(&capture.lines()[0]).unwrap();
        assert_eq!(json["stack_trace"], "저장 실패\nCaused by: disk full\n");
        assert_eq!(json["level"], "ERROR");
    }

    #[test]
    fn 여러_스레드의_출력이_줄_단위로_섞이지_않는다() {
        let (logger, capture, _) = logger(Level::Info);
        let logger = Arc::new(logger);
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let logger = Arc::clone(&logger);
                std::thread::spawn(move || {
                    for i in 0..50 {
                        logger.info("x").field("t", t).field("i", i).log("m");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let lines = capture.lines();
        assert_eq!(lines.len(), 400);
        for line in lines {
            serde_json::from_str::<Value>(&line).expect("각 줄은 완결된 JSON");
        }
    }

    #[test]
    fn 파일_로그는_상위_디렉터리를_만들고_이어_쓴다() {
        let dir = std::env::temp_dir().join(format!("chalkak-core-log-{}", std::process::id()));
        let file = dir.join("logs").join("application.log");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(ts("2026-10-12T00:00:00Z")));
        {
            let logger = Logger::to_file(&file, Level::Info, Arc::clone(&clock)).unwrap();
            logger.info("x").log("first");
        }
        {
            let logger = Logger::to_file(&file, Level::Info, clock).unwrap();
            logger.info("x").log("second");
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
