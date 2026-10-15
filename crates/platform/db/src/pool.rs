//! 크기가 고정된 블로킹 커넥션 풀(HikariCP와 같은 모델).

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use postgres::Client;

use crate::config::DbConfig;
use crate::error::DbError;
use crate::session::{StatementCache, Tx, cached_db};

struct Idle {
    client: Client,
    /// 이 연결에 준비해 둔 문장. 연결과 함께 풀을 오간다.
    cache: StatementCache,
    created: Instant,
    returned: Instant,
}

struct State {
    idle: VecDeque<Idle>,
    /// 풀이 소유한 연결 수(대여 중 + 유휴 + 맺는 중). 항상 `size` 이하다.
    total: usize,
    waiting: usize,
    closed: bool,
}

struct Inner {
    config: DbConfig,
    size: usize,
    state: Mutex<State>,
    available: Condvar,
    /// 채우기 스레드가 돌고 있는지. 동시에 하나만 돈다.
    filling: AtomicBool,
    /// 채우기 스레드가 풀을 붙잡아 두지 않도록 약한 참조로 넘기기 위한 자기 참조.
    me: Weak<Inner>,
}

/// 채우기 재시도 간격(연속 실패마다 두 배, 상한까지).
const FILL_RETRY_MIN: Duration = Duration::from_millis(250);
const FILL_RETRY_MAX: Duration = Duration::from_secs(5);

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // 잠금 안에서는 패닉할 코드가 없지만, 오염되어도 카운터는 일관되므로 그대로 쓴다.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn open(&self) -> Result<Client, postgres::Error> {
        let mut pg = self.config.pg.clone();
        pg.connect_timeout(self.config.connection_timeout);
        crate::tls::connect(&pg)
    }

    /// 연결 자리를 반납한다(`total` 감소). 풀이 열려 있으면 빈자리를 채우도록 요청한다.
    fn give_up_slot(&self) {
        let closed = {
            let mut state = self.lock();
            state.total -= 1;
            state.closed
        };
        self.available.notify_one();
        if !closed {
            self.request_fill();
        }
    }

    // 주기 점검(Hikari housekeeper) 스레드는 두지 않는다. Hikari의 `fillPool`도 연결을
    // 버리는 순간(`closeConnection`)에 호출되고, housekeeper는 유휴 연결 정리·시계 점프
    // 대응만 한다(`minimumIdle == maximumPoolSize`이면 유휴 정리는 꺼진다). 여기서는 연결을
    // 버리는 모든 경로(release 불가, get의 usable 실패, open 실패의 자리 반납)가
    // `request_fill`로 이어지므로 이벤트만으로 덮인다. 조용히 끊긴 유휴 연결은 Hikari처럼
    // 다음 `get`이 찾아내 한꺼번에 버리고(그 경로가 채우기를 요청한다) 주기 검사로 찾지 않는다.
    // 주기 스레드는 오히려 유휴 풀에 쓸데없는 검사 쿼리를 보내는 상시 비용이 된다.

    /// 비어 있는 자리를 백그라운드에서 채우도록 요청한다. 이미 돌고 있으면 아무것도 안 한다.
    fn request_fill(&self) {
        if self.filling.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = self.me.clone();
        let spawned = std::thread::Builder::new()
            .name("chalkak-pool-fill".to_string())
            .spawn(move || fill_loop(weak));
        if spawned.is_err() {
            // 스레드를 못 만들면 다음 버림 때 다시 시도한다.
            self.filling.store(false, Ordering::Release);
        }
    }

    /// 채울 자리가 있으면 하나 예약한다(`total += 1`). 채울 것이 없거나 풀이 닫혔으면
    /// 같은 잠금 안에서 `filling`을 내려 `None`을 돌려준다. 잠금 안에서 내리는 이유는,
    /// 그 뒤에 `total`을 줄이고 `request_fill`을 부르는 쪽이 반드시 새 스레드를 띄우게 하기 위해서다.
    fn reserve_fill_slot(&self) -> bool {
        let mut state = self.lock();
        if state.closed || state.total >= self.size {
            self.filling.store(false, Ordering::Release);
            return false;
        }
        state.total += 1;
        true
    }

    fn release(&self, client: Client, cache: StatementCache, created: Instant) {
        let reusable = !client.is_closed() && created.elapsed() < self.config.max_lifetime;
        let mut state = self.lock();
        if reusable && !state.closed {
            state.idle.push_back(Idle {
                client,
                cache,
                created,
                returned: Instant::now(),
            });
            drop(state);
            self.available.notify_one();
        } else {
            let closed = state.closed;
            state.total -= 1;
            drop(state);
            drop(client);
            self.available.notify_one();
            if !closed {
                self.request_fill();
            }
        }
    }
}

/// 채우기 스레드 본체. 풀이 사라지거나 닫히거나 자리가 다 차면 끝난다.
/// 반복마다 `Weak`를 올려 쓰고 쉬는 동안에는 놓으므로 풀의 수명을 늘리지 않는다.
fn fill_loop(weak: Weak<Inner>) {
    let mut delay = FILL_RETRY_MIN;
    loop {
        let Some(inner) = weak.upgrade() else { return };
        if !inner.reserve_fill_slot() {
            return;
        }
        match inner.open() {
            Ok(client) => {
                delay = FILL_RETRY_MIN;
                let now = Instant::now();
                let mut state = inner.lock();
                if state.closed {
                    state.total -= 1;
                    drop(state);
                    drop(client);
                } else {
                    state.idle.push_back(Idle {
                        client,
                        cache: StatementCache::default(),
                        created: now,
                        returned: now,
                    });
                    drop(state);
                }
                inner.available.notify_one();
            }
            Err(_) => {
                // 자리만 돌려주고(기다리는 get이 직접 맺어 볼 수 있다) 쉬었다 다시 시도한다.
                {
                    inner.lock().total -= 1;
                }
                inner.available.notify_one();
                drop(inner);
                if !fill_sleep(&weak, delay) {
                    return;
                }
                delay = (delay * 2).min(FILL_RETRY_MAX);
            }
        }
    }
}

/// `delay`만큼 쉰다. 쉬는 도중 풀이 사라지거나 닫히면 `false`(스레드 종료 신호).
/// 종료 직전 `filling`을 내려 둔다.
fn fill_sleep(weak: &Weak<Inner>, delay: Duration) -> bool {
    let end = Instant::now() + delay;
    loop {
        let Some(inner) = weak.upgrade() else {
            return false;
        };
        if inner.lock().closed {
            inner.filling.store(false, Ordering::Release);
            return false;
        }
        drop(inner);
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// 연결 풀. 복제하면 같은 풀을 가리킨다.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

/// 풀 지표. 원본의 `hikariActive`·`hikariPending` 로그 필드에 대응한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    pub size: usize,
    pub total: usize,
    pub idle: usize,
    /// 빌려가 있는 연결 수(Hikari `activeConnections`).
    pub active: usize,
    /// 연결을 기다리는 스레드 수(Hikari `threadsAwaitingConnection`).
    pub waiting: usize,
}

impl Pool {
    /// `size`개의 연결을 미리 맺는다(고정 크기). 하나라도 실패하면 오류(Hikari
    /// `initializationFailTimeout` 기본값처럼 시작할 때 바로 드러낸다).
    pub fn connect(config: &DbConfig, size: usize) -> Result<Pool, DbError> {
        if size == 0 {
            return Err(DbError::Config("pool size must be at least 1".to_string()));
        }
        let inner = Arc::new_cyclic(|me| Inner {
            config: config.clone(),
            size,
            state: Mutex::new(State {
                idle: VecDeque::new(),
                total: 0,
                waiting: 0,
                closed: false,
            }),
            available: Condvar::new(),
            filling: AtomicBool::new(false),
            me: me.clone(),
        });
        let mut idle = VecDeque::with_capacity(size);
        for _ in 0..size {
            let client = inner.open()?;
            let now = Instant::now();
            idle.push_back(Idle {
                client,
                cache: StatementCache::default(),
                created: now,
                returned: now,
            });
        }
        {
            let mut state = inner.lock();
            state.total = size;
            state.idle = idle;
        }
        Ok(Pool { inner })
    }

    /// 연결을 빌린다. 없으면 `connection_timeout`까지 기다리고 넘으면 `PoolTimeout`.
    /// 유휴 연결은 닫혔거나 수명이 지났거나(`max_lifetime`), 오래 쉬었는데 검사에 실패하면
    /// 버리고 새로 맺는다(빈자리는 `total`이 줄어드는 것으로 표현된다).
    pub fn get(&self) -> Result<Conn, DbError> {
        let inner = &self.inner;
        let started = Instant::now();
        let deadline = started + inner.config.connection_timeout;
        let mut last_connect_error: Option<postgres::Error> = None;
        let mut state = inner.lock();
        loop {
            if state.closed {
                return Err(DbError::PoolClosed);
            }
            if let Some(mut entry) = state.idle.pop_back() {
                drop(state);
                if self.usable(&mut entry) {
                    return Ok(self.lend(entry.client, entry.cache, entry.created));
                }
                drop(entry);
                inner.give_up_slot();
                state = inner.lock();
                continue;
            }
            if state.total < inner.size {
                state.total += 1;
                drop(state);
                match inner.open() {
                    Ok(client) => {
                        return Ok(self.lend(client, StatementCache::default(), Instant::now()));
                    }
                    Err(e) => {
                        inner.give_up_slot();
                        last_connect_error = Some(e);
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        std::thread::sleep(remaining.min(Duration::from_millis(250)));
                        state = inner.lock();
                        continue;
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state.waiting += 1;
            let (guard, _) = inner
                .available
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
            state.waiting -= 1;
        }
        match last_connect_error {
            Some(e) => Err(DbError::Postgres(e)),
            None => Err(DbError::PoolTimeout {
                waited: started.elapsed(),
            }),
        }
    }

    fn usable(&self, entry: &mut Idle) -> bool {
        let config = &self.inner.config;
        if entry.client.is_closed() || entry.created.elapsed() >= config.max_lifetime {
            return false;
        }
        if entry.returned.elapsed() > config.alive_bypass_window {
            return entry.client.is_valid(config.validation_timeout).is_ok();
        }
        true
    }

    fn lend(&self, client: Client, cache: StatementCache, created: Instant) -> Conn {
        Conn {
            client: Some(client),
            cache,
            created,
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn stats(&self) -> PoolStats {
        let state = self.inner.lock();
        PoolStats {
            size: self.inner.size,
            total: state.total,
            idle: state.idle.len(),
            active: state.total - state.idle.len(),
            waiting: state.waiting,
        }
    }

    /// 유휴 연결을 닫고 이후 `get`을 막는다. 빌려간 연결은 반환될 때 닫힌다.
    pub fn close(&self) {
        let drained: Vec<Idle> = {
            let mut state = self.inner.lock();
            state.closed = true;
            let drained: Vec<Idle> = state.idle.drain(..).collect();
            state.total -= drained.len();
            drained
        };
        drop(drained);
        self.inner.available.notify_all();
    }
}

/// 빌린 연결. 놓으면 풀로 돌아간다. 질의는 [`Db`]로 보내면 연결별 문장 캐시를 쓴다
/// (`postgres::Client`로 deref해서 보내면 캐시를 거치지 않는다).
pub struct Conn {
    client: Option<Client>,
    cache: StatementCache,
    created: Instant,
    inner: Arc<Inner>,
}

impl Conn {
    /// 트랜잭션을 연다(`BEGIN`). 커밋하지 않고 놓으면 롤백된다.
    pub fn begin(&mut self) -> Result<Tx<'_>, postgres::Error> {
        let client = self.client.as_mut().expect("connection already returned");
        Ok(Tx::new(client.transaction()?, &mut self.cache))
    }

    /// 이 연결에 캐시된 문장 수.
    pub fn cached_statements(&self) -> usize {
        self.cache.len()
    }

    /// 트랜잭션 안에서 `f`를 실행한다. `Ok`면 커밋, `Err`이거나 패닉하면 롤백한다.
    pub fn transaction<T>(
        &mut self,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut tx = self.begin()?;
        match f(&mut tx) {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(e) => {
                // 롤백 실패는 원래 오류를 가리지 않는다. 연결이 끊겼다면 반환 시 버려진다.
                let _ = tx.rollback();
                Err(e)
            }
        }
    }
}

cached_db!(Conn, client, cache, |s| (
    s.client.as_mut().expect("connection already returned"),
    &mut s.cache
));

impl Deref for Conn {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client.as_ref().expect("connection already returned")
    }
}

impl DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("connection already returned")
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let cache = std::mem::take(&mut self.cache);
            self.inner.release(client, cache, self.created);
        }
    }
}
