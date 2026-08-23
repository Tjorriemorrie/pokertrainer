use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing required environment variable: {0}")]
    MissingEnv(&'static str),
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("migration error: {0}")]
    Migration(String),
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}
