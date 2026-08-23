use crate::error::{Error, Result};

pub struct Config {
    pub database_url: String,
    pub log_level: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| Error::MissingEnv("DATABASE_URL"))?;
        if !database_url.starts_with("postgres://") && !database_url.starts_with("postgresql://") {
            return Err(Error::InvalidConfig(
                "DATABASE_URL must be a postgres:// or postgresql:// URL".to_string(),
            ));
        }

        let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

        Ok(Self {
            database_url,
            log_level,
        })
    }
}
