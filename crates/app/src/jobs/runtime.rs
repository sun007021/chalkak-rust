//! 1분마다 남기는 런타임 상태 로그. 원본 `RuntimeSnapshot`, `RuntimeSnapshotReader`, `RuntimeSnapshotLogger`.
//!
//! 로그 한 줄은 원본과 같은 모양이다: 로거 이름 `chalkak.runtime`, INFO, 메시지 `런타임 상태`, `type=runtime` 다음에
//! 측정된 값만 선언 순서대로 key-value로 담는다. 측정하지 못한 값은 0으로 채우지 않고 키를 뺀다(0이면 "풀이 비었다"와
//! "풀이 없다"를 구별할 수 없어 그래프가 거짓 값을 그린다).
//!
//! JVM 전용 지표는 Rust 프로세스에 같은 것이 없다.
//!
//! | 원본 키 | Rust |
//! | --- | --- |
//! | `heapUsedBytes`, `heapMaxBytes`, `gcTimeMsTotal` | 없다(GC 힙이 없다). 의미가 다른 RSS를 같은 키에 넣지 않고 생략한다 |
//! | `threads` | 같은 키. `/proc/self/status`의 `Threads` |
//! | `hikariActive`, `hikariPending` | 같은 키. DB 연결 풀의 빌려간 연결 수와 기다리는 스레드 수 |
//! | (새로 추가) `rssBytes` | `/proc/self/status`의 `VmRSS` |
//! | (새로 추가) `openFds` | `/proc/self/fd` 항목 수 |
//!
//! Prometheus 같은 레지스트리를 거치지 않고 프로세스와 풀에서 직접 읽는다(원본과 같은 이유: 레지스트리 필터 설정
//! 하나로 이 로그의 값이 사라지지 않게). `/proc`이 없는 플랫폼에서는 프로세스 값이 모두 빠진다.

use std::path::PathBuf;
use std::sync::Arc;

use chalkak_core::log::{Logger, fields};
use chalkak_db::{Pool, PoolStats};

/// 로그 키. 원본에 있는 것은 [`fields`]의 이름을 그대로 쓴다.
pub const RSS_BYTES: &str = "rssBytes";
pub const OPEN_FDS: &str = "openFds";

/// 로거 이름과 메시지(원본 `RuntimeSnapshotLogger`).
pub const LOGGER_NAME: &str = "chalkak.runtime";
pub const MESSAGE: &str = "런타임 상태";

/// 한 시점의 프로세스·커넥션 풀 상태. 측정하지 못한 값은 `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub rss_bytes: Option<u64>,
    pub threads: Option<u64>,
    pub open_fds: Option<u64>,
    pub pool_active: Option<u64>,
    pub pool_pending: Option<u64>,
}

impl RuntimeSnapshot {
    /// 측정된 값만 로그 키 이름으로 담아 선언 순서대로 돌려준다(`logFields`).
    pub fn log_fields(&self) -> Vec<(&'static str, u64)> {
        [
            (RSS_BYTES, self.rss_bytes),
            (fields::THREADS, self.threads),
            (OPEN_FDS, self.open_fds),
            (fields::HIKARI_ACTIVE, self.pool_active),
            (fields::HIKARI_PENDING, self.pool_pending),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .collect()
    }
}

/// 풀 지표를 돌려주는 함수. 풀이 없거나 읽지 못하면 `None`(원본: Hikari가 아니거나 아직 시작 전).
pub type PoolStatsSource = Arc<dyn Fn() -> Option<PoolStats> + Send + Sync>;

/// 프로세스와 풀에서 [`RuntimeSnapshot`]을 읽는다.
#[derive(Clone)]
pub struct RuntimeSnapshotReader {
    proc_dir: PathBuf,
    pool: PoolStatsSource,
}

impl RuntimeSnapshotReader {
    /// 실제 프로세스(`/proc/self`)와 이 풀을 읽는다.
    pub fn for_process(pool: Pool) -> Self {
        Self::new("/proc/self", Arc::new(move || Some(pool.stats())))
    }

    /// 시험에서 읽을 곳과 풀 지표를 바꿔 끼운다(원본: 관리 객체를 바꿔 넣는 두 번째 생성자).
    pub fn new(proc_dir: impl Into<PathBuf>, pool: PoolStatsSource) -> Self {
        RuntimeSnapshotReader {
            proc_dir: proc_dir.into(),
            pool,
        }
    }

    pub fn read(&self) -> RuntimeSnapshot {
        let status = std::fs::read_to_string(self.proc_dir.join("status")).ok();
        let stats = (self.pool)();
        RuntimeSnapshot {
            rss_bytes: status
                .as_deref()
                .and_then(|s| status_number(s, "VmRSS"))
                .map(|kib| kib.saturating_mul(1024)),
            threads: status.as_deref().and_then(|s| status_number(s, "Threads")),
            open_fds: self.open_fds(),
            pool_active: stats.map(|s| s.active as u64),
            pool_pending: stats.map(|s| s.waiting as u64),
        }
    }

    /// `fd` 디렉터리 항목 수. 읽는 동안 쓰는 디렉터리 핸들이 하나 더 보일 수 있다.
    fn open_fds(&self) -> Option<u64> {
        let entries = std::fs::read_dir(self.proc_dir.join("fd")).ok()?;
        Some(entries.filter(Result::is_ok).count() as u64)
    }
}

/// `Name:\t  1234 kB` 형식의 줄에서 숫자를 읽는다.
fn status_number(status: &str, name: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// `RuntimeSnapshotLogger.logSnapshot`: 스냅샷 하나를 `type=runtime` INFO 로그 한 줄로 남긴다.
pub fn log_snapshot(logger: &Logger, reader: &RuntimeSnapshotReader) {
    let snapshot = reader.read();
    let mut event = logger
        .info(LOGGER_NAME)
        .field(fields::TYPE, fields::TYPE_RUNTIME);
    for (key, value) in snapshot.log_fields() {
        event = event.field(key, value);
    }
    event.log(MESSAGE);
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use chalkak_core::FixedClock;
    use chalkak_core::log::Level;
    use serde_json::Value;

    use super::*;

    /// 로그 한 줄씩 모으는 sink.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

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
        fn events(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    fn logger(lines: &Lines) -> Logger {
        let clock = Arc::new(FixedClock::new("2026-10-13T00:00:00Z".parse().unwrap()));
        Logger::new(Box::new(lines.clone()), Level::Info, clock)
    }

    fn stats(active: usize, waiting: usize) -> Option<PoolStats> {
        Some(PoolStats {
            size: 10,
            total: 10,
            idle: 10 - active,
            active,
            waiting,
        })
    }

    /// 가짜 `/proc/<pid>`: `status`와 `fd/` 항목들.
    fn proc_dir(name: &str, status: Option<&str>, fds: usize) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chalkak-proc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("fd")).unwrap();
        for n in 0..fds {
            std::fs::write(dir.join("fd").join(n.to_string()), "").unwrap();
        }
        if let Some(status) = status {
            std::fs::write(dir.join("status"), status).unwrap();
        }
        dir
    }

    const STATUS: &str =
        "Name:\tchalkak-server\nVmPeak:\t  900000 kB\nVmRSS:\t   40960 kB\nThreads:\t42\n";

    // RuntimeSnapshotTest#logFields_someValuesAbsent_keepsOrderAndOmitsNulls
    #[test]
    fn log_fields_some_values_absent_keeps_order_and_omits_nones() {
        let snapshot = RuntimeSnapshot {
            rss_bytes: Some(400),
            threads: Some(40),
            open_fds: None,
            pool_active: None,
            pool_pending: Some(1),
        };
        assert_eq!(
            snapshot.log_fields(),
            vec![("rssBytes", 400), ("threads", 40), ("hikariPending", 1)]
        );
    }

    // RuntimeSnapshotLoggerTest#logSnapshot_allValuesPresent_logsOneRuntimeEvent
    #[test]
    fn log_snapshot_all_values_present_logs_one_runtime_event() {
        let dir = proc_dir("all", Some(STATUS), 5);
        let reader = RuntimeSnapshotReader::new(&dir, Arc::new(|| stats(3, 1)));
        let lines = Lines::default();

        log_snapshot(&logger(&lines), &reader);

        let events = lines.events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event["level"], "INFO");
        assert_eq!(event["logger_name"], "chalkak.runtime");
        assert_eq!(event["message"], "런타임 상태");
        assert_eq!(event["type"], "runtime");
        assert_eq!(event["rssBytes"], 40960 * 1024);
        assert_eq!(event["threads"], 42);
        assert_eq!(event["openFds"], 5);
        assert_eq!(event["hikariActive"], 3);
        assert_eq!(event["hikariPending"], 1);
        // JVM 전용 키는 남기지 않는다.
        for key in ["heapUsedBytes", "heapMaxBytes", "gcTimeMsTotal"] {
            assert!(event.get(key).is_none(), "{key}");
        }
    }

    // RuntimeSnapshotLoggerTest#logSnapshot_absentValues_omitsKeys
    #[test]
    fn log_snapshot_absent_values_omits_keys() {
        // 풀이 없고 fd 디렉터리도 읽을 수 없다.
        let dir = proc_dir("absent", Some(STATUS), 0);
        std::fs::remove_dir(dir.join("fd")).unwrap();
        let reader = RuntimeSnapshotReader::new(&dir, Arc::new(|| None));
        let lines = Lines::default();

        log_snapshot(&logger(&lines), &reader);

        let events = lines.events();
        assert_eq!(events.len(), 1);
        let event = events[0].as_object().unwrap();
        for key in ["rssBytes", "threads", "type"] {
            assert!(event.contains_key(key), "{key}");
        }
        for key in ["openFds", "hikariActive", "hikariPending"] {
            assert!(!event.contains_key(key), "{key}");
        }
    }

    // RuntimeSnapshotReaderTest#read_allSourcesPresent_sumsDefinedGcTimes (GC 합산은 대응 값이 없어 읽기 자체를 확인)
    #[test]
    fn read_all_sources_present_fills_every_value() {
        let dir = proc_dir("read", Some(STATUS), 3);
        let reader = RuntimeSnapshotReader::new(&dir, Arc::new(|| stats(2, 4)));

        assert_eq!(
            reader.read(),
            RuntimeSnapshot {
                rss_bytes: Some(40960 * 1024),
                threads: Some(42),
                open_fds: Some(3),
                pool_active: Some(2),
                pool_pending: Some(4),
            }
        );
    }

    // RuntimeSnapshotReaderTest#read_poolNotStarted_leavesHikariValuesAbsent / #read_nonHikariDataSource_leavesHikariValuesAbsent
    #[test]
    fn read_pool_unavailable_leaves_only_pool_values_absent() {
        let dir = proc_dir("nopool", Some(STATUS), 3);
        let reader = RuntimeSnapshotReader::new(&dir, Arc::new(|| None));

        let snapshot = reader.read();

        assert_eq!(snapshot.pool_active, None);
        assert_eq!(snapshot.pool_pending, None);
        assert_eq!(snapshot.rss_bytes, Some(40960 * 1024));
        assert_eq!(snapshot.threads, Some(42));
        assert_eq!(snapshot.open_fds, Some(3));
    }

    // RuntimeSnapshotReaderTest#read_undefinedHeapMax_leavesOnlyMaxAbsent: 줄 하나가 없으면 그 값만 비고 나머지는 채워진다.
    #[test]
    fn read_missing_status_line_leaves_only_that_value_absent() {
        let dir = proc_dir("norss", Some("Name:\tchalkak-server\nThreads:\t7\n"), 1);
        let reader = RuntimeSnapshotReader::new(&dir, Arc::new(|| stats(0, 0)));

        let snapshot = reader.read();

        assert_eq!(snapshot.rss_bytes, None);
        assert_eq!(snapshot.threads, Some(7));
        assert_eq!(snapshot.open_fds, Some(1));
        assert_eq!(snapshot.pool_active, Some(0));
        assert_eq!(snapshot.pool_pending, Some(0));
    }

    // RuntimeSnapshotReaderTest#read_noGarbageCollectors_leavesGcTimeAbsent: 원천 파일이 없으면 프로세스 값이 모두 빠진다.
    #[test]
    fn read_without_proc_leaves_process_values_absent() {
        let dir = std::env::temp_dir().join(format!("chalkak-proc-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reader = RuntimeSnapshotReader::new(&dir, Arc::new(|| stats(1, 0)));

        let snapshot = reader.read();

        assert_eq!(snapshot.rss_bytes, None);
        assert_eq!(snapshot.threads, None);
        assert_eq!(snapshot.open_fds, None);
        assert_eq!(snapshot.pool_active, Some(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn for_process_reads_this_process() {
        let snapshot = RuntimeSnapshotReader::new("/proc/self", Arc::new(|| None)).read();

        assert!(snapshot.rss_bytes.is_some_and(|v| v > 0));
        assert!(snapshot.threads.is_some_and(|v| v >= 1));
        assert!(snapshot.open_fds.is_some_and(|v| v >= 3));
    }
}
