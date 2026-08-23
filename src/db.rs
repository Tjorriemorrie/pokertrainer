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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn database_url() -> String {
        dotenvy::dotenv().ok();
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        }
    }

    fn unique(prefix: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            "{prefix}_{nanos}_{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    async fn test_pool() -> PgPool {
        connect(&database_url()).await.unwrap()
    }

    #[tokio::test]
    async fn connect_and_run_migrations() {
        let pool = test_pool().await;
        run_migrations(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn unreachable_host_yields_sqlx_error() {
        let err = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(300))
            .connect("postgres://user:pass@127.0.0.1:1/nope")
            .await
            .unwrap_err();
        assert!(matches!(Error::from(err), Error::Sqlx(_)));
    }

    #[tokio::test]
    async fn migrations_report_errors_on_closed_pools() {
        let pool = test_pool().await;
        let mirror = pool.clone();
        pool.close().await;
        let err = run_migrations(&mirror).await.unwrap_err();
        assert!(matches!(err, Error::Migration(_)));
    }

    #[tokio::test]
    async fn contextual_range_roundtrip_and_validation() {
        let pool = test_pool().await;
        let name = unique("test_range_profile");
        let profile_id = upsert_opponent_profile(&pool, &name, "TAG").await.unwrap();
        let node = unique("test_range_node");

        assert_eq!(
            load_contextual_range(&pool, profile_id, &node)
                .await
                .unwrap(),
            None
        );

        let first = [0.001f32; RANGE_SIZE];
        upsert_contextual_range(&pool, profile_id, &node, &first)
            .await
            .unwrap();
        assert_eq!(
            load_contextual_range(&pool, profile_id, &node)
                .await
                .unwrap(),
            Some(first)
        );

        let second = [0.005f32; RANGE_SIZE];
        upsert_contextual_range(&pool, profile_id, &node, &second)
            .await
            .unwrap();
        assert_eq!(
            load_contextual_range(&pool, profile_id, &node)
                .await
                .unwrap(),
            Some(second)
        );

        let malformed_node = unique("test_range_malformed");
        sqlx::query(
            "INSERT INTO contextual_ranges (node, profile_id, weights)
             VALUES ($1, $2, ARRAY[1.0::real, 2.0::real, 3.0::real])",
        )
        .bind(&malformed_node)
        .bind(profile_id)
        .execute(&pool)
        .await
        .unwrap();
        let err = load_contextual_range(&pool, profile_id, &malformed_node)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sqlx(sqlx::Error::Decode(_))));

        let name_again = unique("test_range_profile");
        upsert_opponent_profile(&pool, &name_again, "LAG")
            .await
            .unwrap();
        sqlx::query("DELETE FROM opponent_profiles WHERE name = ANY($1)")
            .bind(vec![&name, &name_again])
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opponent_profile_upsert_returns_stable_id_and_updates_type() {
        let pool = test_pool().await;
        let name = unique("test_opp_profile");

        let id = upsert_opponent_profile(&pool, &name, "NIT").await.unwrap();
        let updated = upsert_opponent_profile(&pool, &name, "MANIAC")
            .await
            .unwrap();
        assert_eq!(id, updated);

        let player_type: String =
            sqlx::query_scalar("SELECT player_type FROM opponent_profiles WHERE name = $1")
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(player_type, "MANIAC");

        sqlx::query("DELETE FROM opponent_profiles WHERE name = $1")
            .bind(&name)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn operations_against_closed_pools_fail() {
        let pool = test_pool().await;
        let mirror = pool.clone();
        pool.close().await;

        let err = load_contextual_range(&mirror, 1, "NODE").await.unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));

        let err = upsert_contextual_range(&mirror, 1, "NODE", &[0.0; RANGE_SIZE])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));

        let err = upsert_opponent_profile(&mirror, "name", "TAG")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));
    }
}
