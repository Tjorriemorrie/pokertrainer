use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::error::{Error, Result};
use crate::range::hands::{HAND_COUNT, Range};

pub use crate::range::hands::HAND_COUNT as RANGE_SIZE;

/// Row shape for `local_hero_actions`: (node, stack_bucket, hole_cards,
/// action, hand_no, position, was_preflop_aggressor, facing_cbet).
type LocalActionRow = (String, i16, String, String, i64, String, bool, bool);

/// A stored contextual range: the 169-hand weights plus the number of hands
/// that contributed to it (used for the population fallback).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredRange {
    pub weights: Range,
    pub sample_count: u32,
}

/// A stored action-category frequency mix (fold/call-check/raise/shove,
/// summing to 1) for one `contextual_action_frequencies` row, plus the
/// sample size backing it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StoredCategoryFrequency {
    pub fold_pct: f32,
    pub call_check_pct: f32,
    pub raise_pct: f32,
    pub shove_pct: f32,
    pub sample_count: u32,
}

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

/// The database URL integration tests use: the configured database name with
/// `_test` appended, so tests never touch real data.
#[cfg(test)]
pub fn test_database_url() -> String {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
        )
    });
    let (base, db) = url
        .rsplit_once('/')
        .expect("DATABASE_URL must end in /<database>");
    format!("{base}/{db}_test")
}

/// A pool to the test database with migrations applied.
#[cfg(test)]
pub async fn test_pool() -> PgPool {
    let pool = connect(&test_database_url())
        .await
        .expect("connect to the test database");
    run_migrations(&pool)
        .await
        .expect("run migrations on the test database");
    pool
}

pub async fn load_contextual_range(
    pool: &PgPool,
    profile_id: i32,
    node: &str,
    stack_bucket: i16,
) -> Result<Option<StoredRange>> {
    let row: Option<(Vec<f32>, i32)> = sqlx::query_as(
        "SELECT weights, sample_count FROM contextual_ranges
         WHERE profile_id = $1 AND node = $2 AND stack_bucket = $3",
    )
    .bind(profile_id)
    .bind(node)
    .bind(stack_bucket)
    .fetch_optional(pool)
    .await?;

    row.map(|(weights, sample_count)| {
        Ok(StoredRange {
            weights: ensure_range_len(weights)?,
            sample_count: sample_count.max(0) as u32,
        })
    })
    .transpose()
}

pub async fn upsert_contextual_range(
    pool: &PgPool,
    profile_id: i32,
    node: &str,
    stack_bucket: i16,
    range: &StoredRange,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO contextual_ranges (node, profile_id, stack_bucket, weights, sample_count, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (node, profile_id, stack_bucket)
         DO UPDATE SET weights = EXCLUDED.weights, sample_count = EXCLUDED.sample_count, updated_at = now()",
    )
    .bind(node)
    .bind(profile_id)
    .bind(stack_bucket)
    .bind(range.weights.as_slice())
    .bind(range.sample_count as i32)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_contextual_action_frequency(
    pool: &PgPool,
    profile_id: i32,
    node: &str,
    stack_bucket: i16,
    position: &str,
    aggressor_ctx: &str,
) -> Result<Option<StoredCategoryFrequency>> {
    let row: Option<(f32, f32, f32, f32, i32)> = sqlx::query_as(
        "SELECT fold_pct, call_check_pct, raise_pct, shove_pct, sample_count
         FROM contextual_action_frequencies
         WHERE profile_id = $1 AND node = $2 AND stack_bucket = $3
           AND position = $4 AND aggressor_ctx = $5",
    )
    .bind(profile_id)
    .bind(node)
    .bind(stack_bucket)
    .bind(position)
    .bind(aggressor_ctx)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(fold_pct, call_check_pct, raise_pct, shove_pct, sample_count)| StoredCategoryFrequency {
            fold_pct,
            call_check_pct,
            raise_pct,
            shove_pct,
            sample_count: sample_count.max(0) as u32,
        },
    ))
}

pub async fn upsert_contextual_action_frequency(
    pool: &PgPool,
    profile_id: i32,
    node: &str,
    stack_bucket: i16,
    position: &str,
    aggressor_ctx: &str,
    frequency: &StoredCategoryFrequency,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO contextual_action_frequencies
             (node, profile_id, stack_bucket, position, aggressor_ctx,
              fold_pct, call_check_pct, raise_pct, shove_pct, sample_count, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
         ON CONFLICT (node, profile_id, stack_bucket, position, aggressor_ctx)
         DO UPDATE SET fold_pct = EXCLUDED.fold_pct, call_check_pct = EXCLUDED.call_check_pct,
             raise_pct = EXCLUDED.raise_pct, shove_pct = EXCLUDED.shove_pct,
             sample_count = EXCLUDED.sample_count, updated_at = now()",
    )
    .bind(node)
    .bind(profile_id)
    .bind(stack_bucket)
    .bind(position)
    .bind(aggressor_ctx)
    .bind(frequency.fold_pct)
    .bind(frequency.call_check_pct)
    .bind(frequency.raise_pct)
    .bind(frequency.shove_pct)
    .bind(frequency.sample_count as i32)
    .execute(pool)
    .await?;
    Ok(())
}

/// One locally-generated hero decision, with the engine's true dealt hole
/// cards — the fallback/fill source for the hero's own starting-hand window
/// whenever the imported `gg_hands` alone don't reach it (see
/// `opponent_history`).
#[derive(Clone, Debug, PartialEq)]
pub struct LocalHeroAction {
    pub node: String,
    pub stack_bucket: i16,
    /// e.g. `"As Kh"`.
    pub hole_cards: String,
    /// `"Fold"` / `"CallCheck"` / `"BetRaise"` / `"Shove"`.
    pub action: String,
    /// The session-local hand number this decision belongs to — lets the
    /// window report how many distinct *hands* it covers, not just how many
    /// decisions. Not globally unique across sessions (rows carry no session
    /// id), so this undercounts hands when two sessions' numbers collide;
    /// acceptable for a coarse "how much history is this built from" count.
    pub hand_no: i64,
    /// `"BUTTON"` / `"BIG_BLIND"` / `"THIRD"`.
    pub position: String,
    /// Was this actor the last seat to bet/raise/all-in preflop this hand.
    pub was_preflop_aggressor: bool,
    /// Is the flop bet this actor is facing from that same preflop
    /// aggressor.
    pub facing_cbet: bool,
}

/// Persists a batch of local hero decisions atomically.
pub async fn insert_local_hero_actions(pool: &PgPool, actions: &[LocalHeroAction]) -> Result<()> {
    if actions.is_empty() {
        return Ok(());
    }
    let mut transaction = pool.begin().await?;
    for action in actions {
        sqlx::query(
            "INSERT INTO local_hero_actions
                 (node, stack_bucket, hole_cards, action, hand_no, position,
                  was_preflop_aggressor, facing_cbet)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&action.node)
        .bind(action.stack_bucket)
        .bind(&action.hole_cards)
        .bind(&action.action)
        .bind(action.hand_no)
        .bind(&action.position)
        .bind(action.was_preflop_aggressor)
        .bind(action.facing_cbet)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Loads the most recent local hero decisions (newest first), capped at
/// `limit`.
pub async fn load_recent_local_hero_actions(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<LocalHeroAction>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let rows: Vec<LocalActionRow> = sqlx::query_as(
        "SELECT node, stack_bucket, hole_cards, action, hand_no, position,
                was_preflop_aggressor, facing_cbet
         FROM local_hero_actions
         ORDER BY created_at DESC, id DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(node, stack_bucket, hole_cards, action, hand_no, position, was_preflop_aggressor, facing_cbet)| {
                LocalHeroAction {
                    node,
                    stack_bucket,
                    hole_cards,
                    action,
                    hand_no,
                    position,
                    was_preflop_aggressor,
                    facing_cbet,
                }
            },
        )
        .collect())
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

fn ensure_range_len(weights: Vec<f32>) -> Result<Range> {
    weights.try_into().map_err(|short: Vec<f32>| {
        Error::Sqlx(sqlx::Error::Decode(
            format!(
                "contextual_ranges.weights has length {}, expected {HAND_COUNT}",
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

    fn stored(weights: Range, sample_count: u32) -> StoredRange {
        StoredRange {
            weights,
            sample_count,
        }
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
            load_contextual_range(&pool, profile_id, &node, 25)
                .await
                .unwrap(),
            None
        );

        let first = [0.001f32; RANGE_SIZE];
        upsert_contextual_range(&pool, profile_id, &node, 25, &stored(first, 0))
            .await
            .unwrap();
        assert_eq!(
            load_contextual_range(&pool, profile_id, &node, 25)
                .await
                .unwrap(),
            Some(stored(first, 0))
        );

        let second = [0.005f32; RANGE_SIZE];
        upsert_contextual_range(&pool, profile_id, &node, 25, &stored(second, 42))
            .await
            .unwrap();
        assert_eq!(
            load_contextual_range(&pool, profile_id, &node, 25)
                .await
                .unwrap(),
            Some(stored(second, 42))
        );

        // A different stack bucket is a distinct range.
        assert_eq!(
            load_contextual_range(&pool, profile_id, &node, 10)
                .await
                .unwrap(),
            None
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
        let err = load_contextual_range(&pool, profile_id, &malformed_node, 25)
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

        let err = load_contextual_range(&mirror, 1, "NODE", 25)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));

        let err = upsert_contextual_range(&mirror, 1, "NODE", 25, &stored([0.0; RANGE_SIZE], 0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));

        let err = upsert_opponent_profile(&mirror, "name", "TAG")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));
    }
}
