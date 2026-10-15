//! 질의 인터페이스([`Db`])와 연결별 준비된 문장(prepared statement) 캐시.
//!
//! `postgres`의 `query(&str, ..)`는 부를 때마다 이름 없는 문장을 새로 만든다(Parse → 계획 → 실행). 같은 질의를 수없이 보내는
//! 서버에서는 DB가 매번 계획을 다시 세운다. #36 동시성 시험에서 게시물 상세의 4중 조인 질의는 실행 0.14ms에 계획이 1.5ms였고,
//! DB CPU 한도(1코어)에서 처리량이 Java의 약 60%로 떨어졌다. 원본(pgjdbc)은 같은 문장을 다섯 번째부터 서버에 준비해 두고
//! 다시 쓰므로(`prepareThreshold` 5, 캐시 256개) PostgreSQL이 일반 계획(generic plan)을 재사용한다.
//!
//! 여기서는 풀의 연결마다 SQL 문자열 → [`Statement`] 캐시를 두고, [`Db`]의 질의를 그 문장으로 보낸다. 캐시는 연결과 함께
//! 풀로 돌아가고 연결이 버려지면 함께 사라진다(문장은 연결에 묶여 있다). 처음 쓸 때 준비하는 것이 pgjdbc와 다르지만,
//! 준비하지 않은 질의도 Parse를 한 번 하므로 첫 호출의 비용은 같다. 계획은 PostgreSQL이 `plan_cache_mode`(기본 auto)대로
//! 처음 다섯 번은 매개변수별 계획, 그 뒤 비용이 비슷하면 일반 계획을 쓴다(pgjdbc로 준비한 문장과 같다).
//!
//! 앱 코드는 `&mut impl Db`를 받는다. 풀 연결([`crate::Conn`])과 그 트랜잭션([`Tx`])은 캐시를 쓰고, 시험·마이그레이터가 쓰는
//! 맨 `postgres::Client`/`postgres::Transaction`은 캐시 없이 그대로 보낸다.

use std::collections::HashMap;

use postgres::types::ToSql;
use postgres::{GenericClient, Row, Statement};

/// 연결 하나에 캐시하는 문장 수 상한(pgjdbc `preparedStatementCacheQueries` 기본값). 넘으면 비우고 다시 채운다.
/// 앱의 SQL은 거의 고정 문자열이라 실제로는 닿지 않는다.
pub const STATEMENT_CACHE_CAPACITY: usize = 256;

/// 앱이 DB에 질의할 때 쓰는 인터페이스. `postgres::GenericClient`의 자주 쓰는 메서드와 같은 모양이다.
pub trait Db {
    fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error>;

    fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, postgres::Error>;

    fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, postgres::Error>;

    fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, postgres::Error>;

    /// 매개변수 없는 문장 여러 개(단순 질의 프로토콜). 캐시하지 않는다.
    fn batch_execute(&mut self, sql: &str) -> Result<(), postgres::Error>;
}

/// SQL 문자열 → 준비된 문장.
#[derive(Default)]
pub struct StatementCache {
    statements: HashMap<String, Statement>,
}

impl StatementCache {
    pub fn len(&self) -> usize {
        self.statements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    pub(crate) fn statement(
        &mut self,
        client: &mut impl GenericClient,
        sql: &str,
    ) -> Result<Statement, postgres::Error> {
        if let Some(statement) = self.statements.get(sql) {
            return Ok(statement.clone());
        }
        let statement = client.prepare(sql)?;
        if self.statements.len() >= STATEMENT_CACHE_CAPACITY {
            self.statements.clear();
        }
        self.statements.insert(sql.to_owned(), statement.clone());
        Ok(statement)
    }
}

/// 캐시를 거쳐 질의하는 메서드를 만든다(풀 연결과 트랜잭션이 같은 코드를 쓴다). 고유 메서드로도 만들어서
/// `Db`를 가져오지 않아도 캐시를 쓰고, 풀 연결에서는 `postgres::Client`로 deref되는 같은 이름 메서드를 가린다.
macro_rules! cached_db {
    ($ty:ty, $client:ident, $cache:ident, |$s:ident| $split:expr) => {
        impl $ty {
            pub fn query(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<Vec<::postgres::Row>, ::postgres::Error> {
                let $s = self;
                let ($client, $cache) = $split;
                let statement = $cache.statement($client, sql)?;
                ::postgres::GenericClient::query($client, &statement, params)
            }

            pub fn query_one(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<::postgres::Row, ::postgres::Error> {
                let $s = self;
                let ($client, $cache) = $split;
                let statement = $cache.statement($client, sql)?;
                ::postgres::GenericClient::query_one($client, &statement, params)
            }

            pub fn query_opt(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<Option<::postgres::Row>, ::postgres::Error> {
                let $s = self;
                let ($client, $cache) = $split;
                let statement = $cache.statement($client, sql)?;
                ::postgres::GenericClient::query_opt($client, &statement, params)
            }

            pub fn execute(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<u64, ::postgres::Error> {
                let $s = self;
                let ($client, $cache) = $split;
                let statement = $cache.statement($client, sql)?;
                ::postgres::GenericClient::execute($client, &statement, params)
            }

            pub fn batch_execute(&mut self, sql: &str) -> Result<(), ::postgres::Error> {
                let $s = self;
                let ($client, _) = $split;
                ::postgres::GenericClient::batch_execute($client, sql)
            }
        }

        impl $crate::session::Db for $ty {
            fn query(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<Vec<::postgres::Row>, ::postgres::Error> {
                <$ty>::query(self, sql, params)
            }

            fn query_one(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<::postgres::Row, ::postgres::Error> {
                <$ty>::query_one(self, sql, params)
            }

            fn query_opt(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<Option<::postgres::Row>, ::postgres::Error> {
                <$ty>::query_opt(self, sql, params)
            }

            fn execute(
                &mut self,
                sql: &str,
                params: &[&(dyn ::postgres::types::ToSql + Sync)],
            ) -> Result<u64, ::postgres::Error> {
                <$ty>::execute(self, sql, params)
            }

            fn batch_execute(&mut self, sql: &str) -> Result<(), ::postgres::Error> {
                <$ty>::batch_execute(self, sql)
            }
        }
    };
}

/// 풀 연결 위의 트랜잭션. 커밋하지 않고 놓으면 롤백된다(`postgres::Transaction`과 같다).
pub struct Tx<'a> {
    tx: postgres::Transaction<'a>,
    cache: &'a mut StatementCache,
}

impl<'a> Tx<'a> {
    pub(crate) fn new(tx: postgres::Transaction<'a>, cache: &'a mut StatementCache) -> Tx<'a> {
        Tx { tx, cache }
    }

    pub fn commit(self) -> Result<(), postgres::Error> {
        self.tx.commit()
    }

    pub fn rollback(self) -> Result<(), postgres::Error> {
        self.tx.rollback()
    }
}

cached_db!(Tx<'_>, client, cache, |s| (&mut s.tx, &mut *s.cache));

/// 캐시 없이 그대로 보내는 구현(시험, 마이그레이터, 풀 밖의 연결).
macro_rules! direct_db {
    ($ty:ty) => {
        impl Db for $ty {
            fn query(
                &mut self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Vec<Row>, postgres::Error> {
                GenericClient::query(self, sql, params)
            }

            fn query_one(
                &mut self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Row, postgres::Error> {
                GenericClient::query_one(self, sql, params)
            }

            fn query_opt(
                &mut self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<Option<Row>, postgres::Error> {
                GenericClient::query_opt(self, sql, params)
            }

            fn execute(
                &mut self,
                sql: &str,
                params: &[&(dyn ToSql + Sync)],
            ) -> Result<u64, postgres::Error> {
                GenericClient::execute(self, sql, params)
            }

            fn batch_execute(&mut self, sql: &str) -> Result<(), postgres::Error> {
                GenericClient::batch_execute(self, sql)
            }
        }
    };
}

direct_db!(postgres::Client);
direct_db!(postgres::Transaction<'_>);

pub(crate) use cached_db;
