//! 크기가 고정된 블로킹 커넥션 풀(HikariCP와 같은 모델).

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use postgres::Client;

use crate::config::DbConfig;
use crate::error::DbError;

struct Idle {
    client: Client,
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
}

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

    fn give_up_slot(&self) {
        self.lock().total -= 1;
        self.available.notify_one();
    }

    fn release(&self, client: Client, created: Instant) {
        let reusable = !client.is_closed() && created.elapsed() < self.config.max_lifetime;
        let mut state = self.lock();
        if reusable && !state.closed {
            state.idle.push_back(Idle {
                client,
                created,
                returned: Instant::now(),
            });
            drop(state);
        } else {
            state.total -= 1;
            drop(state);
            drop(client);
        }
        self.available.notify_one();
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
        let inner = Inner {
            config: config.clone(),
            size,
            state: Mutex::new(State {
                idle: VecDeque::new(),
                total: 0,
                waiting: 0,
                closed: false,
            }),
            available: Condvar::new(),
        };
        let mut idle = VecDeque::with_capacity(size);
        for _ in 0..size {
            let client = inner.open()?;
            let now = Instant::now();
            idle.push_back(Idle {
                client,
                created: now,
                returned: now,
            });
        }
        {
            let mut state = inner.lock();
            state.total = size;
            state.idle = idle;
        }
        Ok(Pool {
            inner: Arc::new(inner),
        })
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
                    return Ok(self.lend(entry.client, entry.created));
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
                    Ok(client) => return Ok(self.lend(client, Instant::now())),
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

    fn lend(&self, client: Client, created: Instant) -> Conn {
        Conn {
            client: Some(client),
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

/// 빌린 연결. 놓으면 풀로 돌아간다. `postgres::Client`처럼 쓸 수 있다.
pub struct Conn {
    client: Option<Client>,
    created: Instant,
    inner: Arc<Inner>,
}

impl Conn {
    /// 트랜잭션 안에서 `f`를 실행한다. `Ok`면 커밋, `Err`이거나 패닉하면 롤백한다.
    pub fn transaction<T>(
        &mut self,
        f: impl FnOnce(&mut postgres::Transaction) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut tx = self.deref_mut().transaction()?;
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
            self.inner.release(client, self.created);
        }
    }
}
