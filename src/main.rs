use pokertrainer::config::Config;
use pokertrainer::db;
use pokertrainer::error::Result;
use pokertrainer::server::{self, ServerConfig};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Where the rolling application log is written, alongside the PostgreSQL
/// data directory. Kept out of version control via `.gitignore`.
const LOG_DIR: &str = "data";
const LOG_FILE: &str = "app.log";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Log to the console and to a rolling file in `data/` so a stuck table
    // can be diagnosed after the fact — the terminal output alone is lost the
    // moment the process exits.
    let file_appender = tracing_appender::rolling::never(LOG_DIR, LOG_FILE);
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .init();

    let config = Config::from_env()?;

    tracing::info!(database = %redact_credentials(&config.database_url), "pokertrainer starting");
    tracing::info!(log_file = %format!("{LOG_DIR}/{LOG_FILE}"), "application log");
    tracing::debug!(log_level = %config.log_level, "configuration loaded");

    let pool = db::connect(&config.database_url).await?;
    db::run_migrations(&pool).await?;
    tracing::info!("database ready, migrations up to date");

    // A process that died mid-connection leaves its active table marked as
    // connected; clear stale flags so the next connection can claim it.
    pokertrainer::live::mark_disconnected(&pool).await?;

    let server_config = ServerConfig {
        bind: config.bind_addr,
        ..ServerConfig::live()
    };
    server::serve(server_config, Some(pool)).await?;

    Ok(())
}

fn redact_credentials(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let Some(at) = url.rfind('@') else {
        return url.to_string();
    };
    format!("{}://***{}", &url[..scheme_end], &url[at..])
}
