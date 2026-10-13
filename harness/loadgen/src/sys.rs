//! 대상 프로세스의 CPU·메모리·FD·스레드를 `/proc`에서 읽는다(리눅스 호스트에서 컨테이너 프로세스도 보인다).
//! 호스트 부하(`/proc/loadavg`)도 함께 기록해 측정이 다른 작업에 얼마나 흔들렸는지 남긴다.

use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// 커널 `CLK_TCK`. 리눅스는 사실상 항상 100이다.
const CLK_TCK: f64 = 100.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// utime + stime (clock tick)
    pub cpu_ticks: u64,
    pub rss_kb: u64,
    pub threads: u64,
    pub fds: u64,
}

/// `/proc/<pid>/stat` 한 줄에서 (utime+stime, num_threads).
pub fn parse_stat(stat: &str) -> Option<(u64, u64)> {
    // comm은 괄호 안에 공백이 올 수 있으므로 마지막 ')' 뒤부터 센다. 이후 첫 필드가 state(3번째).
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let threads: u64 = f.get(17)?.parse().ok()?;
    Some((utime + stime, threads))
}

pub fn parse_rss_kb(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

pub fn sample(pid: u32) -> io::Result<Sample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (cpu_ticks, threads) =
        parse_stat(&stat).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stat 형식"))?;
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let rss_kb = parse_rss_kb(&status).unwrap_or(0);
    let fds = fs::read_dir(format!("/proc/{pid}/fd"))?.count() as u64;
    Ok(Sample {
        cpu_ticks,
        rss_kb,
        threads,
        fds,
    })
}

/// 호스트 부하 평균 (1분, 5분, 15분).
pub fn loadavg() -> [f64; 3] {
    let text = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut it = text.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    [
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
    ]
}

/// 측정 구간 요약.
#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    /// 구간 동안 쓴 CPU 시간(초). 코어 수로 나누지 않은 값이다.
    pub cpu_secs: f64,
    pub rss_kb_max: u64,
    pub rss_kb_last: u64,
    pub fds_max: u64,
    pub fds_last: u64,
    pub threads_max: u64,
    pub threads_last: u64,
    pub samples: u64,
}

/// 1초마다 표본을 뜨는 스레드. `finish`가 요약을 돌려준다.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    state: Arc<Mutex<(Option<Sample>, Summary)>>,
}

impl Watcher {
    pub fn start(pid: u32) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let state: Arc<Mutex<(Option<Sample>, Summary)>> = Arc::default();
        let (s, st) = (Arc::clone(&stop), Arc::clone(&state));
        let handle = thread::spawn(move || {
            let mut first: Option<Sample> = None;
            while !s.load(Ordering::Relaxed) {
                if let Ok(cur) = sample(pid) {
                    let base = *first.get_or_insert(cur);
                    let mut g = st.lock().unwrap();
                    g.1 = Summary {
                        cpu_secs: cur.cpu_ticks.saturating_sub(base.cpu_ticks) as f64 / CLK_TCK,
                        rss_kb_max: g.1.rss_kb_max.max(cur.rss_kb),
                        rss_kb_last: cur.rss_kb,
                        fds_max: g.1.fds_max.max(cur.fds),
                        fds_last: cur.fds,
                        threads_max: g.1.threads_max.max(cur.threads),
                        threads_last: cur.threads,
                        samples: g.1.samples + 1,
                    };
                    g.0 = Some(cur);
                }
                for _ in 0..10 {
                    if s.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Watcher {
            stop,
            handle: Some(handle),
            state,
        }
    }

    /// 지금까지의 요약(멈추지 않는다).
    pub fn peek(&self) -> Summary {
        self.state.lock().unwrap().1
    }

    pub fn finish(mut self) -> Summary {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peek()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat에서_cpu와_스레드를_읽는다() {
        // 필드: pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt utime stime cutime cstime priority nice num_threads ...
        let line = "123 (java (x)) S 1 1 1 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 42 0 1000 0 0";
        assert_eq!(parse_stat(line), Some((300, 42)));
    }

    #[test]
    fn 형식이_틀리면_none() {
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (a) S 1"), None);
    }

    #[test]
    fn status에서_rss를_읽는다() {
        let s = "Name:\tjava\nVmPeak:\t 1 kB\nVmRSS:\t  123456 kB\nThreads:\t3\n";
        assert_eq!(parse_rss_kb(s), Some(123_456));
        assert_eq!(parse_rss_kb("Name: x"), None);
    }

    #[test]
    fn 자기_프로세스를_읽을_수_있다() {
        let s = sample(std::process::id()).unwrap();
        assert!(s.rss_kb > 0 && s.threads >= 1 && s.fds >= 3);
    }

    #[test]
    fn 감시자가_요약을_돌려준다() {
        let w = Watcher::start(std::process::id());
        thread::sleep(Duration::from_millis(1300));
        let sum = w.finish();
        assert!(sum.samples >= 1);
        assert!(sum.rss_kb_max >= sum.rss_kb_last);
    }
}
