//! 서버 조립: 설정 -> DB 풀·마이그레이션 -> 라우트·미들웨어 체인 -> HTTP 서버.
//!
//! 프레임워크 없이 직접 조립한다(의존성 주입도 생성자로). 도메인 코드는 `chalkak-app`에, 플랫폼은
//! `chalkak-platform-*`에 있고 여기서는 둘을 엮고 HTTP 계층(보안·CORS·접근 로그)을 얹는다.

pub mod logging;
pub mod settings;
pub mod web;

use std::error::Error;
use std::sync::Arc;

use chalkak_app::web::Routes;
use chalkak_app::{AppContext, register_routes};
use chalkak_core::log::Logger;
use chalkak_core::{Clock, ShutdownSignal, SystemClock};
use chalkak_db::Pool;
use chalkak_db::migrate::Migrator;
use chalkak_http::{Chain, Handler, Limits, Router, Server};

use settings::{ACCESS_TOKEN_AUDIENCE, ACCESS_TOKEN_ISSUER, Settings};
use web::access::AccessLog;
use web::actuator::{Actuator, DatabaseCheck};
use web::authorize::Authorize;
use web::bearer::BearerAuth;
use web::callback_limit::CallbackBodyLimit;
use web::cors::AdminCors;
use web::dispatch::Dispatcher;
use web::firewall::Firewall;
use web::security_headers::SecurityHeaders;
use web::tomcat::Tomcat;

/// 풀에서 연결을 빌려 `SELECT 1`이 되는지 본다(`DataSourceHealthIndicator`).
fn database_check(pool: Pool) -> DatabaseCheck {
    Arc::new(move || match pool.get() {
        Ok(mut conn) => conn.simple_query("SELECT 1").is_ok(),
        Err(_) => false,
    })
}

/// 미들웨어 체인을 만든다. 라우터와 헬스체크는 밖에서 받는다(테스트에서 바꿔 끼운다).
pub fn build_chain(
    settings: &Settings,
    router: Router,
    database: DatabaseCheck,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
) -> Chain {
    let actuator = Actuator::new(database, Arc::clone(&clock), Arc::clone(&logger));
    Chain::new(Dispatcher::new(router, actuator))
        .layer(Tomcat::new(Arc::clone(&clock)))
        .layer(AccessLog::new(Arc::clone(&logger)))
        .layer(CallbackBodyLimit::new(settings.app.callback.max_body_bytes))
        .layer(Firewall::new(Arc::clone(&clock)))
        .layer(SecurityHeaders)
        .layer(AdminCors::new(&settings.admin_cors_allowed_origins))
        .layer(BearerAuth::new(
            settings.access_token_key.clone(),
            ACCESS_TOKEN_ISSUER,
            ACCESS_TOKEN_AUDIENCE,
            Arc::clone(&clock),
            Arc::clone(&logger),
        ))
        .layer(Authorize::new(
            settings.admin_development_bypass,
            Arc::clone(&logger),
        ))
}

/// 요청 하나를 처리하는 전체 핸들러(도메인 라우트 + 미들웨어 체인)를 만든다.
pub fn build_handler(
    settings: &Settings,
    pool: Pool,
    clock: Arc<dyn Clock>,
    logger: Arc<Logger>,
) -> Arc<dyn Handler> {
    let ctx = AppContext {
        pool: pool.clone(),
        clock: Arc::clone(&clock),
        logger: Arc::clone(&logger),
        settings: Arc::new(settings.app.clone()),
    };
    let router = register_routes(Routes::new(Arc::clone(&logger)), &ctx).into_router();
    Arc::new(build_chain(
        settings,
        router,
        database_check(pool),
        clock,
        logger,
    ))
}

/// 기동해서 `shutdown`이 울릴 때까지 요청을 받는다. 종료하면 진행 중인 요청을 마치고 돌아온다.
pub fn run(settings: &Settings, shutdown: ShutdownSignal) -> Result<(), Box<dyn Error>> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let logger = logging::build(settings, Arc::clone(&clock));

    let pool = Pool::connect(&settings.db, settings.db_pool_size)?;
    migrate(settings, &pool, &logger)?;

    let handler = build_handler(settings, pool.clone(), clock, Arc::clone(&logger));
    let server = Server::bind(
        ("0.0.0.0", settings.port),
        settings.workers,
        Limits::default(),
    )?;
    logger
        .info("com.chalkak.backend.BackendApplication")
        .field("port", i64::from(settings.port))
        .log("서버를 시작했습니다");
    let result = server.serve(handler, shutdown);
    pool.close();
    logger
        .info("com.chalkak.backend.BackendApplication")
        .log("서버를 종료했습니다");
    result.map_err(Into::into)
}

fn migrate(settings: &Settings, pool: &Pool, logger: &Logger) -> Result<(), Box<dyn Error>> {
    let migrator = Migrator::from_dir(&settings.migrations_dir)?;
    let mut conn = pool.get()?;
    let report = migrator.migrate(&mut conn)?;
    logger
        .info("org.flywaydb.core.internal.command.DbMigrate")
        .field("applied", i64::try_from(report.applied.len()).unwrap_or(0))
        .log("마이그레이션을 적용했습니다");
    Ok(())
}
