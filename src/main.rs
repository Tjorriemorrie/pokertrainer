use pokertrainer::config::Config;
use pokertrainer::db;
use pokertrainer::error::Result;
use pokertrainer::server::{self, ServerConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = Config::from_env()?;

    tracing::info!(database = %redact_credentials(&config.database_url), "pokertrainer starting");
    tracing::debug!(log_level = %config.log_level, "configuration loaded");

    let pool = db::connect(&config.database_url).await?;
    db::run_migrations(&pool).await?;
    tracing::info!("database ready, migrations up to date");

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
