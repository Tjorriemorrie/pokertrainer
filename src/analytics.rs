//! S9 — session persistence & EV analytics.
//!
//! Every hero decision is logged to `hero_decisions` live (decisions are
//! applied at human speed, so an awaited write per action is far below the
//! latency floor of the solve itself). The stored history then feeds two
//! renderings of the same graph:
//!
//! * the playing table's top-bar chart — the last [`CHART_WINDOW`] actions
//!   across every session (the lifetime curve, snapped on connect), and
//! * the `/tournaments` page — one decimated chart per finished session.
//!
//! Charts are decimated server-side to [`DECIMATED_POINTS`] points per the
//! S9 pipeline so the client can render them instantly.

use sqlx::{PgExecutor, PgPool};

use crate::error::{Error, Result};
use crate::game::Street;

/// Number of actions kept in a chart window.
pub const CHART_WINDOW: usize = 1000;
/// Points per decimated chart dataset.
pub const DECIMATED_POINTS: usize = 100;

/// A chart point: the x coordinate (the action's global or session ordinal)
/// plus the EV lost against the optimal action.
pub type ChartPoint = (u64, f64);

/// One hero decision awaiting a database write.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingDecision {
    pub hand_no: u64,
    pub street: Street,
    pub played: String,
    pub optimal: String,
    pub ev_loss: f64,
}

/// A finished session shown on the tournaments page.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub id: i32,
    pub started: String,
    pub ended: String,
    pub actions: i64,
    pub hands: i32,
    pub avg_ev_loss: f64,
}

/// The `hero_decisions.street` index (0: Preflop, 1: Flop, 2: Turn,
/// 3: River) for a [`Street`].
pub fn street_index(street: Street) -> i32 {
    match street {
        Street::Preflop => 0,
        Street::Flop => 1,
        Street::Turn => 2,
        Street::River => 3,
    }
}

fn hand_number(hand_no: u64) -> Result<i64> {
    i64::try_from(hand_no)
        .map_err(|_| Error::Analytics(format!("hand number {hand_no} overflows INT8")))
}

async fn insert_decision<'q, E>(
    executor: E,
    session_id: i32,
    decision: &'q PendingDecision,
) -> Result<i32>
where
    E: PgExecutor<'q>,
{
    sqlx::query_scalar(
        "INSERT INTO hero_decisions
             (session_id, hand_number, street, played_action, optimal_action, ev_loss)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(session_id)
    .bind(hand_number(decision.hand_no)?)
    .bind(street_index(decision.street))
    .bind(&decision.played)
    .bind(&decision.optimal)
    .bind(decision.ev_loss)
    .fetch_one(executor)
    .await
    .map_err(Into::into)
}

/// Opens a session: one row in `hero_sessions` reserved for the table being
/// played. Sessions without any recorded decision are filtered out of the
/// tournaments page.
pub async fn start_session(pool: &PgPool) -> Result<i32> {
    sqlx::query_scalar("INSERT INTO hero_sessions DEFAULT VALUES RETURNING id")
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

/// Marks a session as finished (idempotent); returns whether the session was
/// still open.
pub async fn finish_session(pool: &PgPool, session_id: i32) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE hero_sessions SET session_end = now()
         WHERE id = $1 AND session_end IS NULL",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Writes one decision row; returns its id.
pub async fn record_decision(
    pool: &PgPool,
    session_id: i32,
    decision: &PendingDecision,
) -> Result<i32> {
    insert_decision(pool, session_id, decision).await
}

/// Writes a batch of decisions atomically; returns the persisted count.
pub async fn persist_records(
    pool: &PgPool,
    session_id: i32,
    records: &[PendingDecision],
) -> Result<usize> {
    let mut transaction = pool.begin().await?;
    for decision in records {
        insert_decision(&mut *transaction, session_id, decision).await?;
    }
    transaction.commit().await?;
    Ok(records.len())
}

/// The last [`CHART_WINDOW`] actions across every session, oldest first, with
/// x set to the global action ordinal (1-based position among all recorded
/// decisions).
pub async fn load_recent(pool: &PgPool, limit: usize) -> Result<Vec<ChartPoint>> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM hero_decisions")
        .fetch_one(pool)
        .await?;
    let limit = limit.min(CHART_WINDOW) as i64;
    let rows: Vec<(i32, f64)> =
        sqlx::query_as("SELECT id, ev_loss FROM hero_decisions ORDER BY id DESC LIMIT $1")
            .bind(limit)
            .fetch_all(pool)
            .await?;
    let skip = (total - rows.len() as i64).max(0);
    let first = (skip + 1) as u64;
    let mut points: Vec<ChartPoint> = rows
        .iter()
        .rev()
        .enumerate()
        .map(|(i, (_, ev_loss))| (first + i as u64, *ev_loss))
        .collect();
    points.truncate(CHART_WINDOW);
    Ok(points)
}

/// The last [`CHART_WINDOW`] actions of one session, oldest first, with x set
/// to the action's ordinal within the session.
pub async fn load_session(pool: &PgPool, session_id: i32, limit: usize) -> Result<Vec<ChartPoint>> {
    let limit = limit.min(CHART_WINDOW) as i64;
    let rows: Vec<(i32, f64)> = sqlx::query_as(
        "SELECT id, ev_loss FROM hero_decisions
         WHERE session_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut points: Vec<ChartPoint> = rows
        .iter()
        .rev()
        .enumerate()
        .map(|(i, (_, ev_loss))| (i as u64 + 1, *ev_loss))
        .collect();
    points.truncate(CHART_WINDOW);
    Ok(points)
}

/// Finished sessions (each with at least one recorded decision), newest
/// first, for the tournaments page.
pub async fn list_finished_sessions(pool: &PgPool, limit: i64) -> Result<Vec<SessionSummary>> {
    let rows: Vec<(i32, String, String, i64, i32, f64)> = sqlx::query_as(
        "SELECT
             s.id,
             to_char(s.session_start AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
             to_char(s.session_end   AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
             count(d.id),
             COALESCE(max(d.hand_number), 0),
             COALESCE(avg(d.ev_loss), 0.0)
         FROM hero_sessions s
         JOIN hero_decisions d ON d.session_id = s.id
         WHERE s.session_end IS NOT NULL
         GROUP BY s.id
         ORDER BY s.session_end DESC, s.id DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, started, ended, actions, hands, avg_ev_loss)| SessionSummary {
                id,
                started,
                ended,
                actions,
                hands,
                avg_ev_loss,
            },
        )
        .collect())
}

/// Server-side chart decimation: splits the point series into `target` bins
/// of equal width and keeps the last point of each non-empty bin, so the
/// latest action of the window is always part of the dataset. Series no
/// longer than `target` pass through unchanged.
pub fn decimate(points: &[ChartPoint], target: usize) -> Vec<ChartPoint> {
    if target == 0 || points.is_empty() {
        return Vec::new();
    }
    if points.len() <= target {
        return points.to_vec();
    }
    let width = points.len() as f64 / target as f64;
    (0..target)
        .map(|bucket| {
            let end = (((bucket + 1) as f64 * width).ceil() as usize).min(points.len());
            points[end - 1]
        })
        .collect()
}

/// Serializes database integration tests that share `hero_sessions` /
/// `hero_decisions` (analytics, http, ws), so window-ordinal assertions stay
/// deterministic regardless of test parallelism.
#[cfg(test)]
pub(crate) static DB_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn decision(hand_no: u64, street: Street, ev_loss: f64) -> PendingDecision {
        PendingDecision {
            hand_no,
            street,
            played: "Call".to_string(),
            optimal: "Fold".to_string(),
            ev_loss,
        }
    }

    #[test]
    fn street_indices_match_the_schema() {
        assert_eq!(street_index(Street::Preflop), 0);
        assert_eq!(street_index(Street::Flop), 1);
        assert_eq!(street_index(Street::Turn), 2);
        assert_eq!(street_index(Street::River), 3);
    }

    #[test]
    fn decimation_passes_short_series_through_untouched() {
        let empty: Vec<ChartPoint> = Vec::new();
        assert_eq!(decimate(&empty, 100), Vec::<ChartPoint>::new());
        assert_eq!(decimate(&empty, 0), Vec::<ChartPoint>::new());

        let short: Vec<ChartPoint> = (1..=37).map(|x| (x, x as f64)).collect();
        assert_eq!(decimate(&short, 100), short);
    }

    #[test]
    fn decimation_targets_exactly_100_points_and_keeps_the_last() {
        let points: Vec<ChartPoint> = (1..=1000).map(|x| (x, x as f64 * 2.0)).collect();
        let decimated = decimate(&points, 100);
        assert_eq!(decimated.len(), 100);
        assert_eq!(decimated[0], (10, 20.0), "first bin keeps its last point");
        assert_eq!(
            decimated[99],
            (1000, 2000.0),
            "the final action always survives decimation"
        );
        assert!(
            decimated.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "x stays strictly increasing"
        );
    }

    #[test]
    fn decimation_handles_windows_that_do_not_divide_evenly() {
        let points: Vec<ChartPoint> = (1..=1050).map(|x| (x, 1.0)).collect();
        let decimated = decimate(&points, 100);
        assert_eq!(decimated.len(), 100);
        assert_eq!(decimated[99], (1050, 1.0));
        assert!(
            decimated.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "x stays strictly increasing for uneven bins"
        );

        let minimal: Vec<ChartPoint> = vec![(1, 1.0), (2, 5.0)];
        assert_eq!(decimate(&minimal, 1), vec![(2, 5.0)]);
    }

    #[test]
    fn decimation_of_one_point_never_duplicates() {
        let points: Vec<ChartPoint> = (1..=101).map(|x| (x, x as f64)).collect();
        let decimated = decimate(&points, 100);
        assert_eq!(decimated.len(), 100);
        assert!(decimated.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(decimated[99], (101, 101.0));
    }

    // Database integration tests share `hero_sessions`/`hero_decisions`, so
    // they run one at a time to keep window-ordinal assertions deterministic.
    fn database_url() -> String {
        dotenvy::dotenv().ok();
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        }
    }

    async fn test_pool() -> PgPool {
        crate::db::connect(&database_url()).await.unwrap()
    }

    async fn delete_sessions(pool: &PgPool, ids: &[i32]) {
        sqlx::query("DELETE FROM hero_sessions WHERE id = ANY($1)")
            .bind(ids)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn session_lifecycle_roundtrips_decisions_and_summaries() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        let session_id = start_session(&pool).await.unwrap();
        persist_records(
            &pool,
            session_id,
            &[
                decision(1, Street::Preflop, 0.0),
                decision(1, Street::Flop, 30.0),
                decision(2, Street::Preflop, 10.0),
            ],
        )
        .await
        .unwrap();

        assert_eq!(
            load_session(&pool, session_id, CHART_WINDOW).await.unwrap(),
            vec![(1, 0.0), (2, 30.0), (3, 10.0)],
            "session points are ordinals within the session"
        );

        let recent = load_recent(&pool, CHART_WINDOW).await.unwrap();
        let tail = &recent[recent.len() - 3..];
        assert_eq!(
            tail.iter().map(|point| point.1).collect::<Vec<_>>(),
            vec![0.0, 30.0, 10.0],
            "the global window holds the newest decisions last"
        );
        assert!(tail.windows(2).all(|pair| pair[0].0 + 1 == pair[1].0));

        assert!(
            !list_finished_sessions(&pool, 10)
                .await
                .unwrap()
                .iter()
                .any(|summary| summary.id == session_id),
            "open sessions never appear on the tournaments page"
        );

        assert!(
            !finish_session(&pool, i32::MAX).await.unwrap(),
            "finishing a missing session changes nothing"
        );
        assert!(finish_session(&pool, session_id).await.unwrap());
        assert!(
            !finish_session(&pool, session_id).await.unwrap(),
            "idempotent"
        );

        let finished = list_finished_sessions(&pool, 10).await.unwrap();
        let summary = finished
            .into_iter()
            .find(|summary| summary.id == session_id)
            .expect("the finished session must be listed");
        assert_eq!(summary.actions, 3);
        assert_eq!(summary.hands, 2);
        assert!((summary.avg_ev_loss - 40.0 / 3.0).abs() < 1e-9);
        assert!(!summary.started.is_empty());
        assert!(!summary.ended.is_empty());

        delete_sessions(&pool, &[session_id]).await;
        assert_eq!(
            load_session(&pool, session_id, CHART_WINDOW).await.unwrap(),
            Vec::<ChartPoint>::new(),
            "decisions cascade with the session"
        );
    }

    #[tokio::test]
    async fn global_recent_window_spans_sessions_in_order() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        let first = start_session(&pool).await.unwrap();
        let second = start_session(&pool).await.unwrap();
        persist_records(&pool, first, &[decision(1, Street::Preflop, 1.0)])
            .await
            .unwrap();
        persist_records(&pool, second, &[decision(5, Street::River, 2.0)])
            .await
            .unwrap();

        let recent = load_recent(&pool, CHART_WINDOW).await.unwrap();
        let tail = &recent[recent.len() - 2..];
        assert_eq!(
            tail.iter().map(|point| point.1).collect::<Vec<_>>(),
            vec![1.0, 2.0],
            "the newest recorded decisions sit at the end of the global window"
        );
        assert_eq!(
            tail[0].0 + 1,
            tail[1].0,
            "global ordinals count every recorded decision in order"
        );

        delete_sessions(&pool, &[first, second]).await;
    }

    #[tokio::test]
    async fn sessions_without_decisions_never_appear() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        let empty_session = start_session(&pool).await.unwrap();
        assert!(finish_session(&pool, empty_session).await.unwrap());

        assert!(
            !list_finished_sessions(&pool, 10_000)
                .await
                .unwrap()
                .iter()
                .any(|summary| summary.id == empty_session),
            "decision-less sessions are filtered from the tournaments page"
        );

        delete_sessions(&pool, &[empty_session]).await;
    }

    #[tokio::test]
    async fn operations_against_closed_pools_fail() {
        let pool = test_pool().await;
        let mirror = pool.clone();
        pool.close().await;

        for result in [
            start_session(&mirror).await.map(|_| ()),
            finish_session(&mirror, 1).await.map(|_| ()),
            record_decision(&mirror, 1, &decision(1, Street::Preflop, 0.0))
                .await
                .map(|_| ()),
            load_recent(&mirror, 10).await.map(|_| ()),
            load_session(&mirror, 1, 10).await.map(|_| ()),
            list_finished_sessions(&mirror, 10).await.map(|_| ()),
        ] {
            assert!(matches!(result, Err(Error::Sqlx(_))));
        }
    }
}
