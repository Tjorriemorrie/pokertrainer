//! The live-tournament store: one active tournament at a time, snapshotted
//! after every table change so a reconnect resumes the exact hand, street,
//! bet sizes, and deck order. A `connected` flag keeps a second tab from
//! claiming the same table while another connection is live.

use sqlx::PgPool;

use crate::error::{Error, Result};
use crate::snapshot::{ActiveSummary, DashboardActive, TournamentSnapshot};

/// The outcome of claiming the single active-tournament row when a table
/// connection opens.
#[derive(Clone, Debug, PartialEq)]
pub enum ClaimOutcome {
    /// A fresh row was created for a brand-new tournament.
    Fresh(i32),
    /// An open tournament was reclaimed. `snapshot` is `None` only for a
    /// table claimed by a connection that died before its first state
    /// update — treated as a fresh table on the same session.
    Resumed {
        session_id: i32,
        snapshot: Box<Option<TournamentSnapshot>>,
    },
    /// Another connection is already driving this table.
    Taken,
}

/// One active-tournament row: the session and, once the table has rendered
/// at least once, the resumable snapshot.
type ActiveRow = (i32, Option<String>);

/// Claims the single active table for a new connection:
///
/// * no row yet → creates the session and the row and returns [`ClaimOutcome::Fresh`];
/// * a row exists and nobody is connected → marks it connected and returns
///   its snapshot for a resume ([`ClaimOutcome::Resumed`]);
/// * a live connection already holds it → [`ClaimOutcome::Taken`].
pub async fn claim_or_resume(pool: &PgPool) -> Result<ClaimOutcome> {
    let mut transaction = pool.begin().await?;

    let row: Option<ActiveRow> = sqlx::query_as(
        "SELECT session_id, snapshot::text FROM active_tournament WHERE single = TRUE",
    )
    .fetch_optional(&mut *transaction)
    .await?;

    let outcome = match row {
        None => {
            let session_id: i32 =
                sqlx::query_scalar("INSERT INTO hero_sessions DEFAULT VALUES RETURNING id")
                    .fetch_one(&mut *transaction)
                    .await?;
            sqlx::query(
                "INSERT INTO active_tournament (single, session_id, connected)
                 VALUES (TRUE, $1, TRUE)",
            )
            .bind(session_id)
            .execute(&mut *transaction)
            .await?;
            ClaimOutcome::Fresh(session_id)
        }
        Some((session_id, snapshot_json)) => {
            let connected: bool =
                sqlx::query_scalar("SELECT connected FROM active_tournament WHERE single = TRUE")
                    .fetch_one(&mut *transaction)
                    .await?;
            if connected {
                ClaimOutcome::Taken
            } else {
                sqlx::query("UPDATE active_tournament SET connected = TRUE WHERE single = TRUE")
                    .execute(&mut *transaction)
                    .await?;
                let snapshot = match snapshot_json {
                    Some(json) => Some(parse_snapshot(json)?),
                    None => None,
                };
                ClaimOutcome::Resumed {
                    session_id,
                    snapshot: Box::new(snapshot),
                }
            }
        }
    };

    transaction.commit().await?;
    Ok(outcome)
}

fn parse_snapshot(json: String) -> Result<TournamentSnapshot> {
    TournamentSnapshot::from_json(&json)
}

/// Rewrites the active row with the current table snapshot. The snapshot is
/// required for a resume — until the first save the row only knows a table
/// exists.
pub async fn save_snapshot(
    pool: &PgPool,
    session_id: i32,
    snapshot: &TournamentSnapshot,
) -> Result<()> {
    let done = sqlx::query(
        "UPDATE active_tournament
         SET snapshot = CAST($2 AS jsonb), updated_at = now()
         WHERE single = TRUE AND session_id = $1",
    )
    .bind(session_id)
    .bind(snapshot.to_json()?)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(Error::Analytics(
            "active tournament missing while saving its snapshot".into(),
        ));
    }
    Ok(())
}

/// Frees the single active row: the tournament ended (or was given up).
pub async fn clear_active(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
        .execute(pool)
        .await?;
    Ok(())
}

/// Marks the active table as unclaimed after a disconnect (also used at boot
/// to clear stale flags left by a killed process). Idempotent.
pub async fn mark_disconnected(pool: &PgPool) -> Result<()> {
    sqlx::query("UPDATE active_tournament SET connected = FALSE WHERE single = TRUE")
        .execute(pool)
        .await?;
    Ok(())
}

/// The dashboard's view of the active tournament: the resume card facts and
/// the session start time. `None` when no tournament is open.
pub async fn load_dashboard(pool: &PgPool) -> Result<Option<DashboardActive>> {
    let row: Option<(i32, Option<String>, String, i64)> = sqlx::query_as(
        "SELECT a.session_id,
                a.snapshot::text,
                to_char(s.session_start AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                (SELECT count(*) FROM hero_decisions d WHERE d.session_id = a.session_id)
         FROM active_tournament a
         JOIN hero_sessions s ON s.id = a.session_id
         WHERE a.single = TRUE",
    )
    .fetch_optional(pool)
    .await?;

    let Some((session_id, snapshot_json, started, actions)) = row else {
        return Ok(None);
    };
    let Some(snapshot_json) = snapshot_json else {
        // Claimed but never rendered: no table facts to show yet — treat it
        // the same as an open tournament we know nothing about.
        return Ok(None);
    };
    let snapshot = parse_snapshot(snapshot_json)?;
    let summary =
        ActiveSummary::from_snapshot(session_id, &snapshot, started, actions.max(0) as usize);
    Ok(summary.map(|summary| DashboardActive {
        session_id,
        summary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::DB_TEST_LOCK;

    async fn test_pool() -> PgPool {
        crate::db::test_pool().await
    }

    fn sample_snapshot() -> TournamentSnapshot {
        TournamentSnapshot {
            state: crate::snapshot::StateSnapshot {
                stacks: [490, 480, 500],
                button: 0,
                blind_small: 10,
                blind_big: 20,
                street: 0,
                board: Vec::new(),
                hole_cards: vec![
                    ["As".into(), "Kd".into()],
                    ["2c".into(), "2h".into()],
                    ["7s".into(), "8s".into()],
                ],
                revealed: [true, false, false],
                street_contrib: [10, 20, 0],
                total_contrib: [10, 20, 0],
                current_bet: 20,
                min_raise: 20,
                last_full_raise: None,
                acted: [false, false, false],
                folded: [false, false, false],
                all_in: [false, false, false],
                eliminated: [false, false, false],
                to_act: 0,
                hand_over: false,
                hand_result: None,
            },
            deck: vec!["2c".into(), "3d".into()],
            hand_no: 1,
            action_no: 0,
            log: vec!["— Hand #1 — blinds 10/20".into()],
            template_skill: None,
            opponents: crate::snapshot::OpponentCountersSnapshot {
                hands: [1, 1],
                vpip: [0, 0],
                pfr: [0, 0],
                faced_bet: [0, 0],
                folded_to_bet: [0, 0],
                postflop_bets: [0, 0],
                postflop_calls: [0, 0],
                vpip_seen: [false, false],
                pfr_seen: [false, false],
            },
        }
    }

    /// Clears any pre-existing active row so the single-row store starts
    /// empty (tests share the local database).
    struct CleanSlate;

    impl CleanSlate {
        async fn take(pool: &PgPool) -> Self {
            sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
                .execute(pool)
                .await
                .unwrap();
            Self
        }
    }

    #[tokio::test]
    async fn claim_create_save_resume_and_clear_round_trip() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let _slate = CleanSlate::take(&pool).await;

        assert!(
            load_dashboard(&pool).await.unwrap().is_none(),
            "no dashboard entry without an active tournament"
        );

        let fresh = match claim_or_resume(&pool).await.unwrap() {
            ClaimOutcome::Fresh(session_id) => session_id,
            other => panic!("expected a fresh claim, got {other:?}"),
        };
        assert!(
            load_dashboard(&pool).await.unwrap().is_none(),
            "no snapshot yet"
        );

        let snapshot = sample_snapshot();
        save_snapshot(&pool, fresh, &snapshot).await.unwrap();

        let dashboard = load_dashboard(&pool)
            .await
            .unwrap()
            .expect("active row exists");
        assert_eq!(dashboard.session_id, fresh);
        assert_eq!(dashboard.summary.hand_no, 1);
        assert_eq!(dashboard.summary.hero_stack, 490);
        assert_eq!(dashboard.summary.actions, 0);

        // A second claim while connected is rejected.
        assert_eq!(claim_or_resume(&pool).await.unwrap(), ClaimOutcome::Taken);

        // Disconnect frees the row for a resume with the saved snapshot.
        mark_disconnected(&pool).await.unwrap();
        let resumed = match claim_or_resume(&pool).await.unwrap() {
            ClaimOutcome::Resumed {
                session_id,
                snapshot: stored,
            } => {
                assert_eq!(session_id, fresh);
                assert_eq!(*stored, Some(snapshot));
                session_id
            }
            other => panic!("expected a resumed claim, got {other:?}"),
        };
        assert_eq!(resumed, fresh);

        clear_active(&pool).await.unwrap();
        assert!(load_dashboard(&pool).await.unwrap().is_none());
        assert!(
            matches!(
                claim_or_resume(&pool).await.unwrap(),
                ClaimOutcome::Fresh(_)
            ),
            "a cleared tournament lets a brand-new one start"
        );
        mark_disconnected(&pool).await.unwrap();
        clear_active(&pool).await.unwrap();
    }

    /// A resumed row without a snapshot (the first connection died before its
    /// first state update) carries the session id and no snapshot.
    #[tokio::test]
    async fn resumed_rows_without_a_snapshot_carry_no_table_state() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let _slate = CleanSlate::take(&pool).await;

        let fresh = match claim_or_resume(&pool).await.unwrap() {
            ClaimOutcome::Fresh(session_id) => session_id,
            other => panic!("expected a fresh claim, got {other:?}"),
        };
        mark_disconnected(&pool).await.unwrap();
        assert_eq!(
            claim_or_resume(&pool).await.unwrap(),
            ClaimOutcome::Resumed {
                session_id: fresh,
                snapshot: Box::new(None)
            }
        );
        clear_active(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn save_against_a_missing_row_fails_loudly() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let _slate = CleanSlate::take(&pool).await;
        let err = save_snapshot(&pool, 1_000_000, &sample_snapshot())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Analytics(_)));
    }

    #[tokio::test]
    async fn dashboard_counts_actions_and_malformed_snapshots_are_rejected() {
        let _guard = DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let _slate = CleanSlate::take(&pool).await;

        let session_id = match claim_or_resume(&pool).await.unwrap() {
            ClaimOutcome::Fresh(session_id) => session_id,
            other => panic!("expected a fresh claim, got {other:?}"),
        };

        // A decision recorded before the snapshot lands on the dashboard.
        crate::analytics::persist_records(
            &pool,
            session_id,
            &[crate::analytics::PendingDecision {
                hand_no: 1,
                street: crate::game::Street::Preflop,
                played: "Call".into(),
                optimal: "Fold".into(),
                ev_loss: 1.0,
            }],
        )
        .await
        .unwrap();
        save_snapshot(&pool, session_id, &sample_snapshot())
            .await
            .unwrap();
        let dashboard = load_dashboard(&pool).await.unwrap().unwrap();
        assert_eq!(dashboard.summary.actions, 1);

        sqlx::query("UPDATE active_tournament SET snapshot = $1::jsonb WHERE single = TRUE")
            .bind(r#"{"state": {"stacks": "not-an-array"}}"#)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(load_dashboard(&pool).await, Err(Error::Json(_))));

        mark_disconnected(&pool).await.unwrap();
        assert!(matches!(claim_or_resume(&pool).await, Err(Error::Json(_))));

        mark_disconnected(&pool).await.unwrap();
        clear_active(&pool).await.unwrap();
    }
}
