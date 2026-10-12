//! 수동 검증용: `cargo run -p chalkak-db --example migrate -- <접속 URL> [마이그레이션 디렉터리]`

use std::path::PathBuf;

use chalkak_db::{DbConfig, migrate::Migrator};

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("usage: migrate <url> [dir]");
    let dir = args
        .next()
        .map_or_else(|| PathBuf::from("migrations"), PathBuf::from);
    let config = DbConfig::from_url(&url).expect("url");
    let mut client = config.pg.connect(postgres::NoTls).expect("connect");
    let report = Migrator::from_dir(&dir)
        .expect("load")
        .migrate(&mut client)
        .expect("migrate");
    for a in &report.applied {
        println!(
            "applied {} {} ({}ms)",
            a.version, a.script, a.execution_time_ms
        );
    }
    println!("{} applied", report.applied.len());
}
