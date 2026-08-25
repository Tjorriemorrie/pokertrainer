//! Session persistence & EV analytics.
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
//! Charts are decimated server-side to [`DECIMATED_POINTS`] points so the
//! client can render them instantly.

use sqlx::{PgExecutor, PgPool};

use crate::error::{Error, Result};
use crate::game::Street;

/// Number of actions kept in a chart window.
pub const CHART_WINDOW: usize = 1000;
/// Points per decimated chart dataset.
pub const DECIMATED_POINTS: usize = 100;

/// A chart point: the x coordinate (the action's global or session ordinal)
/// plus the EV lost against the optimal action, in big blinds.
pub type ChartPoint = (u64, f64);

/// A `hero_sessions` row joined with its decision aggregates, as read by the
/// summary and detail queries.
type SummaryRow = (
    i32,
    String,
    String,
    i64,
    i32,
    f64,
    f64,
    Option<String>,
    Option<i32>,
    i64,
    i64,
    i64,
);

/// One hero decision awaiting a database write.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingDecision {
    pub hand_no: u64,
    pub street: Street,
    pub played: String,
    pub optimal: String,
    /// EV given up against the optimal action, in big blinds — the
    /// human-readable figure shown in the coach overlay and the progress
    /// chart.
    pub ev_loss: f64,
    /// The same EV given up, normalized instead to the pot at the decision
    /// point — what the blunder tracker's rolling calibration is built
    /// from, so a river mistake in a big pot doesn't outrank an equally bad
    /// preflop mistake just because more chips were on the table.
    pub ev_loss_pot: f64,
}

/// One completed hand awaiting a database write: who won it and how the hero
/// fared, so the tournament detail page can aggregate wins, losses, and
/// all-in frequency.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingHandResult {
    pub hand_no: u64,
    pub hero_won: bool,
    pub hero_all_in: bool,
    pub hero_busted: bool,
    /// The winning seat's index (0: Hero, 1: Opponent 1, 2: Opponent 2).
    pub winner_seat: i32,
}

/// A finished session shown on the tournaments page.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub id: i32,
    pub started: String,
    pub ended: String,
    pub actions: i64,
    pub hands: i32,
    /// Hands the hero won within this session.
    pub hands_won: i64,
    /// Mean EV loss across the session's actions, in big blinds.
    pub avg_ev_loss: f64,
    /// Total EV lost across the whole session's actions, in big blinds — the
    /// headline number for tracking improvement drill over drill.
    pub total_ev_loss: f64,
    /// The tournament outcome (`WIN`/`LOSS`), or `None` for sessions finished
    /// manually before a winner was decided.
    pub result: Option<String>,
    /// The hero's stack when the tournament ended, or `None` when unknown.
    pub final_stack: Option<i32>,
    /// Wins among all decided (`WIN`/`LOSS`) sessions up to and including
    /// this one, in chronological order — the numerator of the running
    /// win rate shown on the drill listing.
    pub running_wins: i64,
    /// Decided sessions up to and including this one, in chronological
    /// order — the denominator of the running win rate.
    pub running_decided: i64,
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
             (session_id, hand_number, street, played_action, optimal_action, ev_loss, ev_loss_pot)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id",
    )
    .bind(session_id)
    .bind(hand_number(decision.hand_no)?)
    .bind(street_index(decision.street))
    .bind(&decision.played)
    .bind(&decision.optimal)
    .bind(decision.ev_loss)
    .bind(decision.ev_loss_pot)
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

/// Finalizes a session that ended naturally: records the outcome and the
/// hero's final stack alongside the end timestamp (idempotent).
pub async fn finalize_session(
    pool: &PgPool,
    session_id: i32,
    result: &str,
    final_stack: i32,
) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE hero_sessions
         SET session_end = now(), result = $2, final_stack = $3
         WHERE id = $1 AND session_end IS NULL",
    )
    .bind(session_id)
    .bind(result)
    .bind(final_stack)
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

/// The hero's last `limit` recorded decisions across every session, oldest
/// first: session id, hand number, and the pot-normalized EV lost. Fed into
/// the blunder tracker at the start of every table — a brand-new game
/// inherits the hero's established calibration instead of starting cold,
/// and a resumed table continues exactly where it stopped. The session id
/// travels alongside the hand number because every session restarts hand
/// numbering at 1.
pub async fn load_recent_losses(pool: &PgPool, limit: usize) -> Result<Vec<(i32, i64, f64)>> {
    let limit = limit as i64;
    let rows: Vec<(i32, i64, f64)> = sqlx::query_as(
        "SELECT session_id, hand_number::bigint, ev_loss_pot FROM hero_decisions
         ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().rev().collect())
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

/// Writes a batch of per-hand results atomically; returns the persisted count.
pub async fn persist_hand_results(
    pool: &PgPool,
    session_id: i32,
    results: &[PendingHandResult],
) -> Result<usize> {
    let mut transaction = pool.begin().await?;
    for result in results {
        sqlx::query(
            "INSERT INTO hero_hand_results
                 (session_id, hand_number, hero_won, hero_all_in, hero_busted, winner_seat)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(session_id)
        .bind(hand_number(result.hand_no)?)
        .bind(result.hero_won)
        .bind(result.hero_all_in)
        .bind(result.hero_busted)
        .bind(result.winner_seat)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(results.len())
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

/// Lifetime drill aggregates, shown at the top of the drill page the same
/// way `hh::OverallStats` tops the hand-history page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrillOverallStats {
    pub drills: i64,
    pub drills_won: i64,
    pub hands: i64,
    pub hands_won: i64,
    pub avg_ev_loss: f64,
    pub total_ev_loss: f64,
}

/// Aggregates every finished drill (session with at least one recorded
/// decision): how many were played and won, hands played and won across all
/// of them, and the EV-loss figures that track improvement over time.
pub async fn overall_drill_stats(pool: &PgPool) -> Result<DrillOverallStats> {
    let row: (i64, i64, i64, i64, f64, f64) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM hero_sessions s WHERE s.session_end IS NOT NULL
                 AND EXISTS (SELECT 1 FROM hero_decisions d WHERE d.session_id = s.id)),
             (SELECT count(*) FROM hero_sessions s WHERE s.session_end IS NOT NULL
                 AND s.result = 'WIN'
                 AND EXISTS (SELECT 1 FROM hero_decisions d WHERE d.session_id = s.id)),
             (SELECT count(*) FROM hero_hand_results r
                 JOIN hero_sessions s ON s.id = r.session_id WHERE s.session_end IS NOT NULL),
             COALESCE((SELECT sum(CASE WHEN r.hero_won THEN 1 ELSE 0 END) FROM hero_hand_results r
                 JOIN hero_sessions s ON s.id = r.session_id WHERE s.session_end IS NOT NULL), 0),
             COALESCE((SELECT avg(d.ev_loss) FROM hero_decisions d
                 JOIN hero_sessions s ON s.id = d.session_id WHERE s.session_end IS NOT NULL), 0.0),
             COALESCE((SELECT sum(d.ev_loss) FROM hero_decisions d
                 JOIN hero_sessions s ON s.id = d.session_id WHERE s.session_end IS NOT NULL), 0.0)",
    )
    .fetch_one(pool)
    .await?;
    Ok(DrillOverallStats {
        drills: row.0,
        drills_won: row.1,
        hands: row.2,
        hands_won: row.3,
        avg_ev_loss: row.4,
        total_ev_loss: row.5,
    })
}

/// The number of finished sessions shown on the tournaments page (every
/// session with at least one recorded decision).
pub async fn count_finished_sessions(pool: &PgPool) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM hero_sessions s
         WHERE s.session_end IS NOT NULL
           AND EXISTS (SELECT 1 FROM hero_decisions d WHERE d.session_id = s.id)",
    )
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Finished sessions (each with at least one recorded decision), newest
/// first, for one page of the tournaments page. `offset` skips the first
/// pages worth of sessions.
pub async fn list_finished_sessions(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<Vec<SessionSummary>> {
    let rows: Vec<SummaryRow> = sqlx::query_as(
        "SELECT
                 s.id,
                 to_char(s.session_start AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                 to_char(s.session_end   AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                 count(d.id),
                 COALESCE(max(d.hand_number), 0),
                 COALESCE(avg(d.ev_loss), 0.0),
                 COALESCE(sum(d.ev_loss), 0.0),
                 s.result,
                 s.final_stack,
                 count(*) FILTER (WHERE s.result = 'WIN')
                     OVER (ORDER BY s.session_end ASC, s.id ASC ROWS UNBOUNDED PRECEDING),
                 count(*) FILTER (WHERE s.result IN ('WIN', 'LOSS'))
                     OVER (ORDER BY s.session_end ASC, s.id ASC ROWS UNBOUNDED PRECEDING),
                 COALESCE((
                     SELECT sum(CASE WHEN r.hero_won THEN 1 ELSE 0 END)
                     FROM hero_hand_results r WHERE r.session_id = s.id
                 ), 0)
             FROM hero_sessions s
             JOIN hero_decisions d ON d.session_id = s.id
             WHERE s.session_end IS NOT NULL
             GROUP BY s.id
             ORDER BY s.session_end DESC, s.id DESC
             LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                started,
                ended,
                actions,
                hands,
                avg_ev_loss,
                total_ev_loss,
                result,
                final_stack,
                running_wins,
                running_decided,
                hands_won,
            )| {
                SessionSummary {
                    id,
                    started,
                    ended,
                    actions,
                    hands,
                    hands_won,
                    avg_ev_loss,
                    total_ev_loss,
                    result,
                    final_stack,
                    running_wins,
                    running_decided,
                }
            },
        )
        .collect())
}

/// The full detail of one finished tournament: the session summary plus the
/// hand-level aggregates (wins, losses, all-in frequency) and EV stats that
/// the detail page renders.
#[derive(Clone, Debug, PartialEq)]
pub struct TournamentDetail {
    pub summary: SessionSummary,
    pub hands: i64,
    pub hands_won: i64,
    pub hands_lost: i64,
    pub all_ins: i64,
    pub all_in_pct: f64,
    pub total_ev_loss: f64,
    pub max_ev_loss: f64,
    pub points: Vec<ChartPoint>,
}

/// Loads one tournament's detail, or `None` when the session does not exist
/// or has no recorded decisions.
pub async fn load_tournament_detail(
    pool: &PgPool,
    session_id: i32,
) -> Result<Option<TournamentDetail>> {
    let summary_row: Option<SummaryRow> = sqlx::query_as(
        "SELECT
                 s.id,
                 to_char(s.session_start AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                 to_char(s.session_end   AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                 count(d.id),
                 COALESCE(max(d.hand_number), 0),
                 COALESCE(avg(d.ev_loss), 0.0),
                 COALESCE(sum(d.ev_loss), 0.0),
                 s.result,
                 s.final_stack,
                 0::bigint,
                 0::bigint,
                 0::bigint
             FROM hero_sessions s
             LEFT JOIN hero_decisions d ON d.session_id = s.id
             WHERE s.id = $1
             GROUP BY s.id",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    let Some((
        id,
        started,
        ended,
        actions,
        hands,
        avg_ev_loss,
        total_ev_loss,
        result,
        final_stack,
        _running_wins,
        _running_decided,
        _hands_won,
    )) = summary_row
    else {
        return Ok(None);
    };
    if actions == 0 {
        return Ok(None);
    }

    let hand_stats: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),
                COALESCE(sum(CASE WHEN hero_won THEN 1 ELSE 0 END), 0),
                COALESCE(sum(CASE WHEN hero_all_in THEN 1 ELSE 0 END), 0)
         FROM hero_hand_results WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    let (hand_count, hands_won, all_ins) = hand_stats;
    let total_hands = if hand_count > 0 {
        hand_count
    } else {
        hands as i64
    };
    let hands_lost = total_hands - hands_won;
    let all_in_pct = if total_hands > 0 {
        all_ins as f64 * 100.0 / total_hands as f64
    } else {
        0.0
    };

    let max_ev_loss: f64 = sqlx::query_scalar(
        "SELECT COALESCE(max(ev_loss), 0.0) FROM hero_decisions WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;

    let points = decimate(
        &load_session(pool, session_id, CHART_WINDOW).await?,
        DECIMATED_POINTS,
    );

    Ok(Some(TournamentDetail {
        summary: SessionSummary {
            id,
            started,
            ended,
            actions,
            hands,
            hands_won,
            avg_ev_loss,
            total_ev_loss,
            result,
            final_stack,
            running_wins: 0,
            running_decided: 0,
        },
        hands: total_hands,
        hands_won,
        hands_lost,
        all_ins,
        all_in_pct,
        total_ev_loss,
        max_ev_loss,
        points,
    }))
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
            ev_loss_pot: ev_loss,
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
    async fn test_pool() -> PgPool {
        crate::db::test_pool().await
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

        let recent_losses = load_recent_losses(&pool, CHART_WINDOW).await.unwrap();
        let loss_tail = &recent_losses[recent_losses.len() - 3..];
        assert_eq!(
            loss_tail,
            &[
                (session_id, 1, 0.0),
                (session_id, 1, 30.0),
                (session_id, 2, 10.0)
            ],
            "hand losses replay in play order across every session, newest last, for blunder hydration"
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
            !list_finished_sessions(&pool, 10, 0)
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

        let finished = list_finished_sessions(&pool, 10, 0).await.unwrap();
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
    async fn running_win_rate_accumulates_chronologically_across_decided_sessions() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        // Real (or earlier test) sessions may already be decided, so the
        // running counts are read relative to whatever the most recent
        // session already shows rather than assuming an empty history.
        let baseline = list_finished_sessions(&pool, 1, 0)
            .await
            .unwrap()
            .first()
            .map(|summary| (summary.running_wins, summary.running_decided))
            .unwrap_or((0, 0));

        let mut ids = Vec::new();
        for result in ["WIN", "LOSS", "WIN"] {
            let session_id = start_session(&pool).await.unwrap();
            persist_records(&pool, session_id, &[decision(1, Street::Preflop, 1.0)])
                .await
                .unwrap();
            finalize_session(&pool, session_id, result, 0)
                .await
                .unwrap();
            ids.push((session_id, result));
        }

        let all = list_finished_sessions(&pool, 1_000_000, 0).await.unwrap();
        let by_id = |id: i32| all.iter().find(|summary| summary.id == id).unwrap();

        let (base_wins, base_decided) = baseline;
        assert_eq!(
            (
                by_id(ids[0].0).running_wins,
                by_id(ids[0].0).running_decided
            ),
            (base_wins + 1, base_decided + 1),
            "first WIN"
        );
        assert_eq!(
            (
                by_id(ids[1].0).running_wins,
                by_id(ids[1].0).running_decided
            ),
            (base_wins + 1, base_decided + 2),
            "then a LOSS: wins stay flat, decided climbs"
        );
        assert_eq!(
            (
                by_id(ids[2].0).running_wins,
                by_id(ids[2].0).running_decided
            ),
            (base_wins + 2, base_decided + 3),
            "then a WIN: both climb"
        );

        delete_sessions(&pool, &ids.iter().map(|(id, _)| *id).collect::<Vec<_>>()).await;
    }

    #[tokio::test]
    async fn hand_results_and_finalize_feed_the_tournament_detail() {
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
        persist_hand_results(
            &pool,
            session_id,
            &[
                PendingHandResult {
                    hand_no: 1,
                    hero_won: true,
                    hero_all_in: false,
                    hero_busted: false,
                    winner_seat: 0,
                },
                PendingHandResult {
                    hand_no: 2,
                    hero_won: false,
                    hero_all_in: true,
                    hero_busted: true,
                    winner_seat: 1,
                },
            ],
        )
        .await
        .unwrap();

        assert!(
            finalize_session(&pool, session_id, "LOSS", 0)
                .await
                .unwrap()
        );
        assert!(
            !finalize_session(&pool, session_id, "LOSS", 0)
                .await
                .unwrap(),
            "finalizing is idempotent"
        );

        let detail = load_tournament_detail(&pool, session_id)
            .await
            .unwrap()
            .expect("a finished session with decisions has a detail");
        assert_eq!(detail.summary.id, session_id);
        assert_eq!(detail.summary.result.as_deref(), Some("LOSS"));
        assert_eq!(detail.summary.final_stack, Some(0));
        assert_eq!(detail.hands, 2);
        assert_eq!(detail.hands_won, 1);
        assert_eq!(detail.hands_lost, 1);
        assert_eq!(detail.all_ins, 1);
        assert!((detail.all_in_pct - 50.0).abs() < 1e-9);
        assert!((detail.total_ev_loss - 40.0).abs() < 1e-9);
        assert!((detail.max_ev_loss - 30.0).abs() < 1e-9);
        assert_eq!(detail.points.len(), 3);

        let listing = list_finished_sessions(&pool, 1_000_000, 0).await.unwrap();
        let row = listing
            .iter()
            .find(|summary| summary.id == session_id)
            .expect("the finalized session is listed");
        assert_eq!(
            row.hands_won, 1,
            "the listing's hands_won matches the detail page's"
        );

        delete_sessions(&pool, &[session_id]).await;
        assert_eq!(
            load_tournament_detail(&pool, session_id).await.unwrap(),
            None,
            "a deleted session has no detail"
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
            !list_finished_sessions(&pool, 10_000, 0)
                .await
                .unwrap()
                .iter()
                .any(|summary| summary.id == empty_session),
            "decision-less sessions are filtered from the tournaments page"
        );

        delete_sessions(&pool, &[empty_session]).await;
    }

    #[tokio::test]
    async fn finished_session_count_and_paging_track_the_listing() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        let mut ids = Vec::new();
        for _ in 0..7 {
            let id = start_session(&pool).await.unwrap();
            persist_records(&pool, id, &[decision(1, Street::Preflop, 1.0)])
                .await
                .unwrap();
            finish_session(&pool, id).await.unwrap();
            ids.push(id);
        }

        let all = list_finished_sessions(&pool, 1_000_000, 0).await.unwrap();
        assert_eq!(
            all.len() as i64,
            count_finished_sessions(&pool).await.unwrap(),
            "the count matches the full listing"
        );
        assert!(
            all.iter().any(|summary| summary.id == ids[6]),
            "the freshly finished sessions are listed"
        );
        let newest: Vec<i32> = all.iter().take(7).map(|summary| summary.id).collect();
        assert_eq!(
            newest,
            ids.iter().rev().copied().collect::<Vec<_>>(),
            "sessions created now lead the listing, newest (highest id) first"
        );

        let page1 = list_finished_sessions(&pool, 3, 0).await.unwrap();
        let page2 = list_finished_sessions(&pool, 3, 3).await.unwrap();
        let page3 = list_finished_sessions(&pool, 3, 6).await.unwrap();
        assert_eq!(
            page1.iter().map(|summary| summary.id).collect::<Vec<_>>(),
            vec![ids[6], ids[5], ids[4]],
            "page one carries the newest sessions"
        );
        assert_eq!(
            page2.iter().map(|summary| summary.id).collect::<Vec<_>>(),
            vec![ids[3], ids[2], ids[1]],
            "the offset advances the page window"
        );
        assert_eq!(
            page3.first().map(|summary| summary.id),
            Some(ids[0]),
            "older, pre-existing sessions trail the freshly finished ones"
        );

        delete_sessions(&pool, &ids).await;
    }

    #[tokio::test]
    async fn operations_against_closed_pools_fail() {
        let pool = test_pool().await;
        let mirror = pool.clone();
        pool.close().await;

        for result in [
            start_session(&mirror).await.map(|_| ()),
            finish_session(&mirror, 1).await.map(|_| ()),
            finalize_session(&mirror, 1, "WIN", 1500).await.map(|_| ()),
            record_decision(&mirror, 1, &decision(1, Street::Preflop, 0.0))
                .await
                .map(|_| ()),
            persist_hand_results(
                &mirror,
                1,
                &[PendingHandResult {
                    hand_no: 1,
                    hero_won: true,
                    hero_all_in: false,
                    hero_busted: false,
                    winner_seat: 0,
                }],
            )
            .await
            .map(|_| ()),
            load_recent(&mirror, 10).await.map(|_| ()),
            load_session(&mirror, 1, 10).await.map(|_| ()),
            count_finished_sessions(&mirror).await.map(|_| ()),
            list_finished_sessions(&mirror, 10, 0).await.map(|_| ()),
            load_tournament_detail(&mirror, 1).await.map(|_| ()),
        ] {
            assert!(matches!(result, Err(Error::Sqlx(_))));
        }
    }
}
