use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::error::{Error, Result};

pub const RANGE_SIZE: usize = 169;

pub async fn connect(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| Error::Migration(e.to_string()))
}

pub async fn load_contextual_range(
    pool: &PgPool,
    profile_id: i32,
    node: &str,
) -> Result<Option<[f32; RANGE_SIZE]>> {
    let weights: Option<Vec<f32>> = sqlx::query_scalar(
        "SELECT weights FROM contextual_ranges WHERE profile_id = $1 AND node = $2",
    )
    .bind(profile_id)
    .bind(node)
    .fetch_optional(pool)
    .await?;

    weights.map(ensure_range_len).transpose()
}

pub async fn upsert_contextual_range(
    pool: &PgPool,
    profile_id: i32,
    node: &str,
    weights: &[f32; RANGE_SIZE],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO contextual_ranges (node, profile_id, weights, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (node, profile_id)
         DO UPDATE SET weights = EXCLUDED.weights, updated_at = now()",
    )
    .bind(node)
    .bind(profile_id)
    .bind(weights.as_slice())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_opponent_profile(pool: &PgPool, name: &str, player_type: &str) -> Result<i32> {
    sqlx::query_scalar(
        "INSERT INTO opponent_profiles (name, player_type)
         VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET player_type = EXCLUDED.player_type
         RETURNING id",
    )
    .bind(name)
    .bind(player_type)
    .fetch_one(pool)
    .await
    .map_err(Error::from)
}

fn ensure_range_len(weights: Vec<f32>) -> Result<[f32; RANGE_SIZE]> {
    weights.try_into().map_err(|short: Vec<f32>| {
        Error::Sqlx(sqlx::Error::Decode(
            format!(
                "contextual_ranges.weights has length {}, expected {RANGE_SIZE}",
                short.len()
            )
            .into(),
        ))
    })
}
