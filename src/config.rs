use crate::error::{Error, Result};

#[derive(Debug)]
pub struct Config {
    pub database_url: String,
    pub log_level: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_env_with(|key| std::env::var(key).ok())
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let database_url = get("DATABASE_URL").ok_or(Error::MissingEnv("DATABASE_URL"))?;
        let log_level = get("RUST_LOG").unwrap_or_else(|| "info".to_string());
        Config::parse(database_url, log_level)
    }

    fn parse(database_url: String, log_level: String) -> Result<Self> {
        if !database_url.starts_with("postgres://") && !database_url.starts_with("postgresql://") {
            return Err(Error::InvalidConfig(
                "DATABASE_URL must be a postgres:// or postgresql:// URL".to_string(),
            ));
        }

        Ok(Self {
            database_url,
            log_level,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env_lookup(values: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        |key| values.get(key).cloned()
    }

    #[test]
    fn parses_valid_postgres_urls() {
        let env = HashMap::from([(
            "DATABASE_URL".to_string(),
            "postgres://user:pass@localhost:5433/db".to_string(),
        )]);
        let config = Config::from_env_with(env_lookup(&env)).unwrap();
        assert_eq!(
            config.database_url,
            "postgres://user:pass@localhost:5433/db"
        );
        assert_eq!(config.log_level, "info");

        let env = HashMap::from([(
            "DATABASE_URL".to_string(),
            "postgresql://localhost/db".to_string(),
        )]);
        let config = Config::from_env_with(env_lookup(&env)).unwrap();
        assert_eq!(config.database_url, "postgresql://localhost/db");
    }

    #[test]
    fn honors_explicit_log_level() {
        let env = HashMap::from([
            (
                "DATABASE_URL".to_string(),
                "postgres://localhost/db".to_string(),
            ),
            ("RUST_LOG".to_string(), "debug".to_string()),
        ]);
        let config = Config::from_env_with(env_lookup(&env)).unwrap();
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn missing_database_url_is_an_error() {
        let env = HashMap::new();
        let err = Config::from_env_with(env_lookup(&env)).unwrap_err();
        assert!(matches!(err, Error::MissingEnv("DATABASE_URL")));
    }

    #[test]
    fn rejects_non_postgres_schemes() {
        let env = HashMap::from([(
            "DATABASE_URL".to_string(),
            "http://localhost/db".to_string(),
        )]);
        let err = Config::from_env_with(env_lookup(&env)).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn from_env_delegates_to_the_process_environment() {
        let result = Config::from_env();
        assert!(result.is_ok() || matches!(result.unwrap_err(), Error::MissingEnv("DATABASE_URL")));
    }
}
