//! 커넥션 풀, 트랜잭션, Flyway 호환 마이그레이터.
//!
//! 원본은 HikariCP(기본값)와 Flyway를 쓴다. 같은 동시성 모델(블로킹 풀)과 같은
//! 이력 테이블을 유지해, Java와 Rust가 같은 DB를 번갈아 써도 서로의 이력을 검증할 수 있다.

mod config;
mod error;
pub mod migrate;
mod pool;
mod tls;

pub use config::DbConfig;
pub use error::DbError;
pub use pool::{Conn, Pool, PoolStats};
pub use tls::connect;
