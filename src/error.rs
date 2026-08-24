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
    #[error("range store failure: {0}")]
    Store(String),
    #[error("game state error: {0}")]
    Game(String),
    #[error("solver error: {0}")]
    Solver(String),
    #[error("decision layer error: {0}")]
    Decision(String),
    #[error("analytics error: {0}")]
    Analytics(String),
    #[error("hand history error: {0}")]
    Hh(String),
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
