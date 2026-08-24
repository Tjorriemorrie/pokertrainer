use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::analytics::{self, PendingHandResult};
use crate::error::Result;
use crate::game::{Action, GameState, Seat};
use crate::mcts::MctsConfig;
use crate::mcts::searcher::{
    CHUNK_WALL, PursuedPath, SearchCommand, Searcher, SearcherPhase, SearcherStatus,
    observable_clone,
};
use crate::range::hands::Range;
use crate::server::http::AppState;
use crate::server::protocol::{self, ClientMessage, SearchPhase, ServerMessage};
use crate::server::session::{TableEvent, TableSession, TournamentResult};
use crate::server::views;

/// Where the client is sent once it finishes the table.
pub const TOURNAMENTS_URL: &str = "/tournaments";

/// The outcome of handling one client frame: the frames to send back, how
/// many chart ticks were produced (snapshot refresh pacing), the hero action
/// that was applied (drives the solver's tree reshape), and whether the
/// player finished the table.
pub struct FrameOutcome {
    pub messages: Vec<String>,
    pub chart_ticks: usize,
    pub hero_action: Option<Action>,
    pub finish_table: bool,
}

/// Upgrades `/ws` connections; each connection owns an isolated table session
/// seeded from OS entropy and, when the database is available, an analytics
/// session that stores every hero decision.
pub async fn handler(State(app): State<Arc<AppState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

async fn handle_socket(socket: WebSocket, app: Arc<AppState>) {
    let session_id = open_session(app.pool.as_ref()).await;
    let mut session = TableSession::new(rand::random::<u64>(), app.mcts, app.survival, app.blunder);
    if let Err(error) = bootstrap(&mut session) {
        tracing::warn!(%error, "table session bootstrap failed; closing connection");
        close_session(app.pool.as_ref(), session_id).await;
        return;
    }
    tracing::info!(session_id = ?session_id, "table session started");

    let (mut sender, mut receiver) = socket.split();

    // The table — deal, blinds, opponents — renders before any solver work
    // starts; the background search is spawned only after this initial state
    // has left for the client.
    let mut initial =
        vec![state_message(&mut session).unwrap_or_else(|error| error_message(&error.to_string()))];
    if let Some(snapshot) = snapshot_frame(app.pool.as_ref()).await {
        initial.push(snapshot);
    }
    for frame in &initial {
        if sender
            .send(Message::Text(frame.clone().into()))
            .await
            .is_err()
        {
            close_session(app.pool.as_ref(), session_id).await;
            return;
        }
    }
    session.take_pump_actions();

    let (command_tx, command_rx) = mpsc::unbounded_channel::<SearchCommand>();
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SearcherStatus>();
    let solver = tokio::spawn(run_searcher(
        command_rx,
        update_tx,
        app.mcts,
        session.ranges(),
    ));
    let _ = command_tx.send(SearchCommand::Reshape {
        state: Box::new(observable_clone(session.state())),
        path: None,
        hand_no: session.hand_no(),
    });

    let mut latest_snapshot: Option<crate::mcts::SolveResult> = None;
    let mut solver_alive = true;
    let mut ticks_since_snapshot = 0usize;

    loop {
        tokio::select! {
            maybe = receiver.next() => {
                let Some(message) = maybe else { break };
                let text = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };

                let outcome = handle_client_message(&mut session, text.as_str(), latest_snapshot.as_ref());
                ticks_since_snapshot += outcome.chart_ticks;
                persist_records(app.pool.as_ref(), session_id, &mut session).await;

                for frame in &outcome.messages {
                    if sender.send(Message::Text(frame.clone().into())).await.is_err() {
                        let _ = command_tx.send(SearchCommand::Stop);
                        close_session(app.pool.as_ref(), session_id).await;
                        return;
                    }
                }

                // A tournament ends the moment only one seat is left standing:
                // persist the hand results, finalize the session with the
                // outcome, and hand the client a winner/loser modal instead of
                // dealing another hand.
                if let Some(result) = session.tournament_result() {
                    let _ = command_tx.send(SearchCommand::Stop);
                    let hand_results = session.take_hand_results();
                    let frame = finalize_tournament(
                        app.pool.as_ref(),
                        session_id,
                        &result,
                        hand_results,
                    )
                    .await;
                    if sender.send(Message::Text(frame.into())).await.is_err() {
                        return;
                    }
                    close_session(app.pool.as_ref(), session_id).await;
                    return;
                }

                if outcome.finish_table {
                    let _ = command_tx.send(SearchCommand::Stop);
                    close_session(app.pool.as_ref(), session_id).await;
                    if let Some(frame) = session_finished_message()
                        && sender.send(Message::Text(frame.into())).await.is_err()
                    {
                        return;
                    }
                    return;
                }

                // Reshape the solver's trees onto the played branch so the
                // analysis keeps accumulation between hero decisions.
                if let Some(hero_action) = outcome.hero_action {
                    if !session.state().is_hand_over() && session.state().to_act() == Seat::Hero {
                        let _ = command_tx.send(SearchCommand::Reshape {
                            state: Box::new(observable_clone(session.state())),
                            path: Some(PursuedPath {
                                hero_action,
                                opponent_actions: session.take_pump_actions(),
                            }),
                            hand_no: session.hand_no(),
                        });
                    } else {
                        session.take_pump_actions();
                    }
                }

                if session.state().is_hand_over() {
                    tokio::time::sleep(std::time::Duration::from_millis(app.result_pause_ms)).await;
                    for frame in advance_frames(&mut session) {
                        if sender.send(Message::Text(frame.into())).await.is_err() {
                            let _ = command_tx.send(SearchCommand::Stop);
                            close_session(app.pool.as_ref(), session_id).await;
                            return;
                        }
                    }
                    session.take_pump_actions();
                    let _ = command_tx.send(SearchCommand::Reshape {
                        state: Box::new(observable_clone(session.state())),
                        path: None,
                        hand_no: session.hand_no(),
                    });
                }

                if ticks_since_snapshot >= app.snapshot_interval.max(1) {
                    ticks_since_snapshot = 0;
                    if let Some(frame) = snapshot_frame(app.pool.as_ref()).await
                        && sender.send(Message::Text(frame.into())).await.is_err()
                    {
                        let _ = command_tx.send(SearchCommand::Stop);
                        close_session(app.pool.as_ref(), session_id).await;
                        return;
                    }
                }
            }
            status = update_rx.recv(), if solver_alive => {
                match status {
                    Some(status) => {
                        latest_snapshot = Some(status.result.clone());
                        let frame = search_status_message(&status);
                        if sender.send(Message::Text(frame.into())).await.is_err() {
                            let _ = command_tx.send(SearchCommand::Stop);
                            close_session(app.pool.as_ref(), session_id).await;
                            return;
                        }
                    }
                    None => {
                        // The solver task ended: drop the stale snapshot and
                        // keep playing with inline solves.
                        tracing::warn!("background solver task ended; falling back to inline solves");
                        latest_snapshot = None;
                        solver_alive = false;
                    }
                }
            }
        }
    }
    let _ = command_tx.send(SearchCommand::Stop);
    solver.abort();
    close_session(app.pool.as_ref(), session_id).await;
}

/// The background MCTS worker: owns the persistent solver, consumes reshape
/// commands, and publishes a progress status after every bounded chunk of
/// blocking work.
async fn run_searcher(
    mut commands: mpsc::UnboundedReceiver<SearchCommand>,
    updates: mpsc::UnboundedSender<SearcherStatus>,
    config: MctsConfig,
    ranges: [Range; 2],
) {
    let mut search: Option<Searcher> = None;
    let mut pending: Option<(Box<GameState>, Option<PursuedPath>, u64)> = None;

    loop {
        let mut stop = false;
        while let Ok(command) = commands.try_recv() {
            match command {
                SearchCommand::Stop => stop = true,
                SearchCommand::Reshape {
                    state,
                    path,
                    hand_no,
                } => {
                    pending = Some((state, path, hand_no));
                }
            }
        }
        if stop {
            return;
        }

        if search.is_none() {
            let Some((state, _, hand_no)) = pending.take() else {
                match commands.recv().await {
                    Some(SearchCommand::Stop) | None => return,
                    Some(SearchCommand::Reshape {
                        state,
                        path,
                        hand_no,
                    }) => {
                        pending = Some((state, path, hand_no));
                    }
                }
                continue;
            };
            let built = tokio::task::spawn_blocking(move || {
                Searcher::build(
                    &state,
                    ranges,
                    config,
                    hand_no,
                    &mut crate::rng::seeded_rng(rand::random::<u64>()),
                )
            })
            .await;
            match built {
                Ok(Ok(built)) => {
                    match built.status() {
                        Ok(status) => {
                            let _ = updates.send(status);
                        }
                        Err(error) => tracing::warn!(%error, "solver startup status unavailable"),
                    }
                    search = Some(built);
                }
                Ok(Err(error)) => {
                    tracing::error!(%error, "background solver build failed");
                    return;
                }
                Err(join) => {
                    tracing::error!(?join, "background solver worker panicked");
                    return;
                }
            }
            continue;
        }

        if pending.is_none() && !search.as_ref().is_some_and(Searcher::needs_work) {
            match commands.recv().await {
                Some(SearchCommand::Stop) | None => return,
                Some(SearchCommand::Reshape {
                    state,
                    path,
                    hand_no,
                }) => {
                    pending = Some((state, path, hand_no));
                }
            }
        }

        let active = search.take().expect("built above");
        let reshape = pending.take();
        let updates = updates.clone();
        let outcome = tokio::task::spawn_blocking(move || -> Result<Searcher> {
            let mut active = active;
            if let Some((state, path, hand_no)) = reshape {
                active.reshape(&state, path.as_ref(), hand_no)?;
            }
            if active.needs_work() {
                let _ = active.run_chunk(CHUNK_WALL)?;
            }
            Ok(active)
        })
        .await;
        match outcome {
            Ok(Ok(active)) => {
                match active.status() {
                    Ok(status) => {
                        let _ = updates.send(status);
                    }
                    Err(error) => tracing::warn!(%error, "solver status unavailable"),
                }
                search = Some(active);
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "background solver failed; falling back to inline solves");
                return;
            }
            Err(join) => {
                tracing::error!(?join, "background solver worker panicked");
                return;
            }
        }
    }
}

/// Serializes the solver's progress into a client-ready frame.
fn search_status_message(status: &SearcherStatus) -> String {
    let phase = match status.phase {
        SearcherPhase::Searching => SearchPhase::Searching,
        SearcherPhase::DepthReached => SearchPhase::DepthReached,
        SearcherPhase::Ready => SearchPhase::Ready,
    };
    match (ServerMessage::SearchStatus {
        iterations_done: status.iterations_done,
        target_iterations: status.target_iterations,
        tree_depth: status.result.max_tree_depth,
        max_depth: status.result.max_depth,
        nodes: status.result.nodes as u64,
        phase,
    })
    .to_json()
    {
        Ok(json) => json,
        Err(error) => error_message(&error.to_string()),
    }
}

/// Opens the analytics session backing this connection; persistence is
/// best-effort — the table keeps playing without it.
async fn open_session(pool: Option<&PgPool>) -> Option<i32> {
    let pool = pool?;
    match analytics::start_session(pool).await {
        Ok(session_id) => Some(session_id),
        Err(error) => {
            tracing::warn!(%error, "analytics session could not be opened — playing without persistence");
            None
        }
    }
}

/// Finalizes the analytics session when the table ends (disconnect or an
/// explicit finish).
async fn close_session(pool: Option<&PgPool>, session_id: Option<i32>) {
    let (Some(pool), Some(session_id)) = (pool, session_id) else {
        return;
    };
    if let Err(error) = analytics::finish_session(pool, session_id).await {
        tracing::warn!(%error, session_id, "analytics session could not be finalized");
    }
}

/// Persists every decision queued by the table since the last frame.
/// Failures are logged and dropped — the game never blocks on the database.
async fn persist_records(
    pool: Option<&PgPool>,
    session_id: Option<i32>,
    session: &mut TableSession,
) {
    let records = session.take_records();
    if records.is_empty() {
        return;
    }
    let (Some(pool), Some(session_id)) = (pool, session_id) else {
        return;
    };
    if let Err(error) = analytics::persist_records(pool, session_id, &records).await {
        tracing::warn!(
            %error,
            session_id,
            dropped = records.len(),
            "decisions could not be persisted — the table keeps playing"
        );
    }
}

/// The decimated chart dataset mapping the last 1,000 stored actions;
/// an empty dataset means there is no stored history.
async fn snapshot_frame(pool: Option<&PgPool>) -> Option<String> {
    let pool = pool?;
    match analytics::load_recent(pool, analytics::CHART_WINDOW).await {
        Ok(points) => {
            let decimated = analytics::decimate(&points, analytics::DECIMATED_POINTS);
            Some(
                match (ServerMessage::ChartSnapshot { points: decimated }).to_json() {
                    Ok(json) => json,
                    Err(error) => error_message(&error.to_string()),
                },
            )
        }
        Err(error) => {
            tracing::warn!(%error, "stored chart history unavailable — sending an empty snapshot");
            Some(
                ServerMessage::ChartSnapshot { points: Vec::new() }
                    .to_json()
                    .unwrap_or_else(|error| error_message(&error.to_string())),
            )
        }
    }
}

fn session_finished_message() -> Option<String> {
    match (ServerMessage::SessionFinished {
        url: TOURNAMENTS_URL.to_string(),
    })
    .to_json()
    {
        Ok(json) => Some(json),
        Err(error) => Some(error_message(&error.to_string())),
    }
}

/// The winner/loser modal frame sent when a tournament ends naturally.
fn tournament_finished_message(won: bool, url: &str) -> Option<String> {
    match (ServerMessage::TournamentFinished {
        won,
        url: url.to_string(),
    })
    .to_json()
    {
        Ok(json) => Some(json),
        Err(error) => Some(error_message(&error.to_string())),
    }
}

/// Persists the final hand results, finalizes the session with the outcome,
/// and returns the winner/loser modal frame. Persistence is best-effort — the
/// modal is sent even when the database is unavailable.
async fn finalize_tournament(
    pool: Option<&PgPool>,
    session_id: Option<i32>,
    result: &TournamentResult,
    hand_results: Vec<PendingHandResult>,
) -> String {
    if let (Some(pool), Some(session_id)) = (pool, session_id) {
        if let Err(error) = analytics::persist_hand_results(pool, session_id, &hand_results).await {
            tracing::warn!(%error, session_id, "hand results could not be persisted");
        }
        let outcome = if result.won { "WIN" } else { "LOSS" };
        let final_stack = result.final_stacks[Seat::Hero.index()] as i32;
        if let Err(error) =
            analytics::finalize_session(pool, session_id, outcome, final_stack).await
        {
            tracing::warn!(%error, session_id, "tournament result could not be finalized");
        }
    }
    let url = match session_id {
        Some(id) => format!("/tournaments/{id}"),
        None => TOURNAMENTS_URL.to_string(),
    };
    tournament_finished_message(result.won, &url)
        .unwrap_or_else(|| error_message("serialization failure"))
}

/// Deals the first hand and drives opponents until the hero must act.
fn bootstrap(session: &mut TableSession) -> Result<()> {
    session.deal_next_hand()?;
    session.pump()?;
    Ok(())
}

fn state_message(session: &mut TableSession) -> Result<String> {
    let sounds = session.take_sounds();
    ServerMessage::TableStateUpdate {
        fragment: views::table_fragment(session.state(), session.hand_no(), session.log(), &sounds),
    }
    .to_json()
}

/// Deals the next hand once the winner has been shown for the configured
/// pause and serializes the resulting state update.
fn advance_frames(session: &mut TableSession) -> Vec<String> {
    match session
        .advance_after_result()
        .and_then(|()| state_message(session))
    {
        Ok(frame) => vec![frame],
        Err(error) => vec![error_message(&error.to_string())],
    }
}

/// Handles one client text frame; never fails the connection — problems are
/// reported back as [`ServerMessage::Error`] frames. `snapshot` is the
/// background solver's latest result: submissions score against it instantly
/// and fall back to an inline solve when it is unavailable or misses the
/// played action.
fn handle_client_message(
    session: &mut TableSession,
    text: &str,
    snapshot: Option<&crate::mcts::SolveResult>,
) -> FrameOutcome {
    let client_message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return FrameOutcome {
                messages: vec![error_message(&format!("malformed message: {error}"))],
                chart_ticks: 0,
                hero_action: None,
                finish_table: false,
            };
        }
    };
    tracing::debug!(message = ?client_message, "client frame received");

    match client_message {
        ClientMessage::ActionSubmit { action } => {
            let hand_no = session.hand_no();
            let street = session.state().street();
            let to_act = session.state().to_act();
            match protocol::resolve_action(&action, session.state()) {
                Ok(resolved) => {
                    tracing::info!(
                        hand_no,
                        street = %street,
                        to_act = %to_act,
                        submitted = ?action,
                        resolved = ?resolved,
                        "action submission resolved"
                    );
                    let submitted = match snapshot {
                        Some(snapshot) => session.submit_with_snapshot(resolved, snapshot),
                        None => session.submit(resolved),
                    };
                    match submitted {
                        Ok(events) => outcome(session, events, Some(resolved)),
                        Err(error) => {
                            tracing::warn!(
                                hand_no,
                                street = %street,
                                to_act = %to_act,
                                submitted = ?action,
                                %error,
                                "action submission failed"
                            );
                            error_outcome(&error.to_string())
                        }
                    }
                }
                Err(error) => {
                    let legal = session.state().legal_actions();
                    tracing::warn!(
                        hand_no,
                        street = %street,
                        to_act = %to_act,
                        legal = ?legal,
                        submitted = ?action,
                        %error,
                        "action submission rejected"
                    );
                    error_outcome(&error.to_string())
                }
            }
        }
        ClientMessage::ReviewDone => {
            let hero_action = session.resolving_action();
            match session.confirm_review() {
                Ok(events) => outcome(session, events, hero_action),
                Err(error) => {
                    let hand_no = session.hand_no();
                    let street = session.state().street();
                    let to_act = session.state().to_act();
                    tracing::warn!(
                        hand_no,
                        street = %street,
                        to_act = %to_act,
                        %error,
                        "review confirmation failed"
                    );
                    error_outcome(&error.to_string())
                }
            }
        }
        ClientMessage::FinishTable => FrameOutcome {
            messages: Vec::new(),
            chart_ticks: 0,
            hero_action: None,
            finish_table: true,
        },
    }
}

fn outcome(
    session: &mut TableSession,
    events: Vec<TableEvent>,
    hero_action: Option<Action>,
) -> FrameOutcome {
    let chart_ticks = events
        .iter()
        .filter(|event| matches!(event, TableEvent::ChartTick { .. }))
        .count();
    FrameOutcome {
        messages: events_to_messages(session, events),
        chart_ticks,
        hero_action,
        finish_table: false,
    }
}

fn error_outcome(message: &str) -> FrameOutcome {
    FrameOutcome {
        messages: vec![error_message(message)],
        chart_ticks: 0,
        hero_action: None,
        finish_table: false,
    }
}

fn events_to_messages(session: &mut TableSession, events: Vec<TableEvent>) -> Vec<String> {
    let mut messages = Vec::with_capacity(events.len());
    for event in events {
        let serialized = match event {
            TableEvent::State => {
                let sounds = session.take_sounds();
                ServerMessage::TableStateUpdate {
                    fragment: views::table_fragment(
                        session.state(),
                        session.hand_no(),
                        session.log(),
                        &sounds,
                    ),
                }
                .to_json()
            }
            TableEvent::TacticalOverlay {
                decision,
                hand_no,
                intercepted,
            } => ServerMessage::TriggerTacticalOverlay {
                fragment: views::tactical_overlay_fragment(
                    hand_no,
                    &decision,
                    intercepted,
                    &session.opponent_snapshots(),
                    session.state().blind_level().big_blind,
                    session.state().legal_actions().call_amount,
                    session.state().stack(Seat::Hero),
                ),
                intercepted,
            }
            .to_json(),
            TableEvent::ChartTick {
                action_index,
                ev_loss,
            } => ServerMessage::ChartTick {
                action_index,
                ev_loss,
            }
            .to_json(),
        };
        match serialized {
            Ok(json) => messages.push(json),
            Err(error) => messages.push(error_message(&error.to_string())),
        }
    }
    messages
}

fn error_message(message: &str) -> String {
    match (ServerMessage::Error {
        message: message.to_string(),
    })
    .to_json()
    {
        Ok(json) => json,
        Err(crate::error::Error::Json(_)) => {
            r#"{"type":"ERROR","message":"serialization failure"}"#.to_string()
        }
        Err(other) => format!(
            r#"{{"type":"ERROR","message":"{}"}}"#,
            other.to_string().replace('\\', "\\\\").replace('"', "\\\"")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blunder::BlunderConfig;
    use crate::decision::{
        Analysis, AnalyzedDecision, PlayedEvaluation, SearchReport, SurvivalConfig,
    };
    use crate::game::{Action, Seat};
    use crate::mcts::MctsConfig;
    use crate::server::http::{AppState, ServeListener, default_assets};
    use serde_json::{Value, json};
    use tokio::net::TcpStream;
    use tokio::time::Duration;
    use tokio_tungstenite::MaybeTlsStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as TMessage;

    fn make_session() -> TableSession {
        let mut session = TableSession::new(
            81,
            MctsConfig::test(),
            SurvivalConfig::default(),
            BlunderConfig::default(),
        );
        bootstrap(&mut session).unwrap();
        session
    }

    fn hero_decision_state() -> GameState {
        use crate::card::Deck;
        use crate::game::blinds::BlindLevel;
        use crate::rng::seeded_rng;
        let mut state = GameState::new(Seat::Opponent1, BlindLevel::new(10, 20));
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(90)))
            .unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    fn uniform_ranges() -> [Range; 2] {
        use crate::range::hands::HAND_COUNT;
        [[1.0 / HAND_COUNT as f32; HAND_COUNT]; 2]
    }

    #[tokio::test]
    async fn searcher_task_stops_immediately_without_commands() {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        command_tx.send(SearchCommand::Stop).unwrap();
        run_searcher(command_rx, update_tx, MctsConfig::test(), uniform_ranges()).await;
        assert!(
            update_rx.recv().await.is_none(),
            "no status is published for a stopped worker"
        );
    }

    #[tokio::test]
    async fn searcher_task_waits_for_its_first_command() {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_searcher(
            command_rx,
            update_tx,
            MctsConfig::test(),
            uniform_ranges(),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        command_tx.send(SearchCommand::Stop).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn searcher_task_builds_reports_and_stops() {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let state = hero_decision_state();
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 1,
            })
            .unwrap();
        let task = tokio::spawn(run_searcher(
            command_rx,
            update_tx,
            MctsConfig::test(),
            uniform_ranges(),
        ));
        let mut status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("the solver publishes a status")
            .expect("the solver stays alive");
        assert!(!status.result.actions.is_empty());
        while status.phase != SearcherPhase::Ready {
            status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
                .await
                .expect("the solver keeps publishing")
                .expect("the solver stays alive");
        }
        command_tx.send(SearchCommand::Stop).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn searcher_task_reshapes_on_command() {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let state = hero_decision_state();
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 1,
            })
            .unwrap();
        let task = tokio::spawn(run_searcher(
            command_rx,
            update_tx,
            MctsConfig::test(),
            uniform_ranges(),
        ));
        let _ = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("first status arrives");
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 2,
            })
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("the reshaped solver publishes again")
            .expect("the solver stays alive");
        assert!(!status.result.actions.is_empty());
        command_tx.send(SearchCommand::Stop).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn searcher_task_survives_a_build_failure() {
        use crate::range::hands::HAND_COUNT;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, _update_rx) = mpsc::unbounded_channel();
        let state = hero_decision_state();
        let dead_ranges = [[0.0f32; HAND_COUNT]; 2];
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 1,
            })
            .unwrap();
        run_searcher(command_rx, update_tx, MctsConfig::test(), dead_ranges).await;
    }

    #[tokio::test]
    async fn searcher_task_picks_up_a_command_arriving_while_it_waits() {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let state = hero_decision_state();
        let task = tokio::spawn(run_searcher(
            command_rx,
            update_tx,
            MctsConfig::test(),
            uniform_ranges(),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 1,
            })
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("a command sent while the worker is parked still starts the search")
            .expect("the solver stays alive");
        assert!(!status.result.actions.is_empty());
        command_tx.send(SearchCommand::Stop).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn searcher_task_reshapes_while_idle_between_budgets() {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut update_rx) = mpsc::unbounded_channel();
        let state = hero_decision_state();
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 1,
            })
            .unwrap();
        let task = tokio::spawn(run_searcher(
            command_rx,
            update_tx,
            MctsConfig::test(),
            uniform_ranges(),
        ));
        let mut status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("the solver publishes a status")
            .expect("the solver stays alive");
        while status.phase != SearcherPhase::Ready {
            status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
                .await
                .expect("the solver keeps publishing")
                .expect("the solver stays alive");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        command_tx
            .send(SearchCommand::Reshape {
                state: Box::new(observable_clone(&state)),
                path: None,
                hand_no: 2,
            })
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("an idle solver wakes up and publishes on the reshaped tree")
            .expect("the solver stays alive");
        assert!(!status.result.actions.is_empty());
        command_tx.send(SearchCommand::Stop).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn analytics_failures_log_but_never_block_the_table() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(300))
            .connect_lazy("postgres://user:pass@127.0.0.1:1/nope")
            .expect("a lazy pool builds without touching the network");
        assert!(
            open_session(Some(&pool)).await.is_none(),
            "an unreachable database yields no analytics session"
        );
        close_session(Some(&pool), Some(17)).await;
        let mut session = make_session();
        let _ = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#,
            None,
        );
        persist_records(Some(&pool), Some(17), &mut session).await;
        assert!(
            session.take_records().is_empty(),
            "records are drained even when persistence fails"
        );
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn bootstrap_deals_a_hand_and_reaches_the_hero() {
        let session = make_session();
        assert_eq!(session.hand_no(), 1);
        assert_eq!(session.state().to_act(), Seat::Hero);
    }

    #[test]
    fn malformed_json_yields_an_error_frame() {
        let mut session = make_session();
        let outcome = handle_client_message(&mut session, "{not json", None);
        assert_eq!(outcome.messages.len(), 1);
        assert!(!outcome.finish_table);
        assert_eq!(outcome.chart_ticks, 0);
        let json = parse(&outcome.messages[0]);
        assert_eq!(json["type"], "ERROR");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("malformed message")
        );
    }

    #[test]
    fn unknown_kinds_yield_errors_without_touching_the_session() {
        let mut session = make_session();
        let hand_no = session.hand_no();
        let outcome = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"zzz"}}"#,
            None,
        );
        assert_eq!(outcome.messages.len(), 1);
        assert_eq!(parse(&outcome.messages[0])["type"], "ERROR");
        assert_eq!(session.hand_no(), hand_no);
        assert_eq!(session.state().to_act(), Seat::Hero);
    }

    #[test]
    fn illegal_submissions_are_rejected_over_the_same_frame() {
        let mut session = make_session();
        let outcome = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"check"}}"#,
            None,
        );
        assert_eq!(outcome.messages.len(), 1);
        let json = parse(&outcome.messages[0]);
        assert_eq!(json["type"], "ERROR");
        assert!(json["message"].as_str().unwrap().contains("not available"));
    }

    #[test]
    fn valid_submissions_run_the_full_pipeline() {
        let mut session = make_session();
        let outcome = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#,
            None,
        );
        assert_eq!(outcome.chart_ticks, 1, "each applied action charts a tick");
        assert!(!outcome.messages.is_empty());
        let types: Vec<String> = outcome
            .messages
            .iter()
            .map(|m| parse(m)["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.iter().any(|t| t == "CHART_TICK"));
        assert!(types.iter().any(|t| t == "TABLE_STATE_UPDATE"));
        assert_eq!(
            parse(outcome.messages.last().unwrap())["type"],
            "TABLE_STATE_UPDATE"
        );
    }

    #[test]
    fn a_finished_hand_shows_the_win_badge_then_deals_the_next_hand() {
        let mut session = make_session();
        session.stage_pending_interception(Action::Fold, sample_analysis());

        let outcome = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#, None);
        assert!(
            session.state().is_hand_over(),
            "with the hero folded, opponents play the hand to its end"
        );
        let json = parse(outcome.messages.last().unwrap());
        assert_eq!(json["type"], "TABLE_STATE_UPDATE");
        let fragment = json["fragment"].as_str().unwrap();
        assert!(
            fragment.contains(r#"class="pt-win"><b>WIN</b>"#),
            "the winner is marked at their seat: {fragment}"
        );
        assert!(
            fragment.contains(r#"class="pt-seat pt-winner""#),
            "the winning seat carries the winner class: {fragment}"
        );
        assert!(
            !fragment.contains(r#"id="action-panel""#),
            "the dock stays hidden while the hand is over: {fragment}"
        );
        assert_eq!(session.hand_no(), 1, "the deal waits for the result pause");

        let frames = advance_frames(&mut session);
        assert_eq!(frames.len(), 1, "one state frame for the fresh deal");
        let next = parse(&frames[0]);
        assert_eq!(next["type"], "TABLE_STATE_UPDATE");
        let next_fragment = next["fragment"].as_str().unwrap();
        assert!(
            next_fragment.contains("Hand #2"),
            "the pause advances to the next hand: {next_fragment}"
        );
        assert!(
            !next_fragment.contains("pt-win"),
            "the win badge does not leak into the fresh hand: {next_fragment}"
        );
        assert_eq!(session.hand_no(), 2);
        assert!(!session.state().is_hand_over());
    }

    fn sample_analysis() -> AnalyzedDecision {
        let fold = Analysis {
            action: Action::Fold,
            bucket: None,
            ev: 0.0,
            variance: 0.0,
            bust_prob: 0.0,
            score: 0.0,
            visits: 120,
        };
        AnalyzedDecision {
            ranking: vec![fold],
            optimal: fold,
            played: Some(PlayedEvaluation {
                analysis: fold,
                ev_loss_bb: 0.0,
                is_optimal: true,
            }),
            search: SearchReport {
                worlds: 1,
                iterations: 1,
                max_depth: 1,
                max_tree_depth: 1,
                nodes: 2,
                rollout_actions: 5,
            },
        }
    }

    #[test]
    fn finish_table_yields_the_session_finished_frame() {
        let mut session = make_session();
        let outcome = handle_client_message(&mut session, r#"{"type":"FINISH_TABLE"}"#, None);
        assert!(
            outcome.finish_table,
            "FINISH_TABLE ends the connection loop"
        );
        assert!(outcome.messages.is_empty());

        let message = session_finished_message().unwrap();
        assert_eq!(
            parse(&message),
            json!({"type": "SESSION_FINISHED", "url": TOURNAMENTS_URL})
        );
    }

    #[test]
    fn tournament_finished_message_carries_the_outcome_and_detail_url() {
        let message = tournament_finished_message(true, "/tournaments/7").unwrap();
        assert_eq!(
            parse(&message),
            json!({"type": "TOURNAMENT_FINISHED", "won": true, "url": "/tournaments/7"})
        );
        let loss = tournament_finished_message(false, "/tournaments/9").unwrap();
        assert_eq!(
            parse(&loss),
            json!({"type": "TOURNAMENT_FINISHED", "won": false, "url": "/tournaments/9"})
        );
    }

    fn sample_tournament_result(won: bool) -> TournamentResult {
        TournamentResult {
            won,
            winner: if won { Seat::Hero } else { Seat::Opponent1 },
            final_stacks: if won { [1500, 0, 0] } else { [0, 1500, 0] },
            hands: 3,
            hands_won: if won { 3 } else { 0 },
            all_ins: 1,
        }
    }

    #[tokio::test]
    async fn finalize_tournament_without_a_pool_still_sends_the_modal() {
        let frame =
            finalize_tournament(None, None, &sample_tournament_result(true), Vec::new()).await;
        assert_eq!(
            parse(&frame),
            json!({"type": "TOURNAMENT_FINISHED", "won": true, "url": TOURNAMENTS_URL})
        );

        let frame =
            finalize_tournament(None, Some(7), &sample_tournament_result(false), Vec::new()).await;
        assert_eq!(
            parse(&frame),
            json!({"type": "TOURNAMENT_FINISHED", "won": false, "url": "/tournaments/7"})
        );
    }

    #[tokio::test]
    async fn finalize_tournament_persists_results_and_finalizes_the_session() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        dotenvy::dotenv().ok();
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        };
        let pool = crate::db::connect(&database_url).await.unwrap();
        let session_id = analytics::start_session(&pool).await.unwrap();
        analytics::persist_records(
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

        let hand_results = vec![PendingHandResult {
            hand_no: 1,
            hero_won: true,
            hero_all_in: false,
            hero_busted: false,
            winner_seat: 0,
        }];
        let frame = finalize_tournament(
            Some(&pool),
            Some(session_id),
            &sample_tournament_result(true),
            hand_results,
        )
        .await;
        assert_eq!(
            parse(&frame),
            json!({"type": "TOURNAMENT_FINISHED", "won": true, "url": format!("/tournaments/{session_id}")})
        );

        let detail = analytics::load_tournament_detail(&pool, session_id)
            .await
            .unwrap()
            .expect("the finalized session has a detail");
        assert_eq!(detail.summary.result.as_deref(), Some("WIN"));
        assert_eq!(detail.summary.final_stack, Some(1500));
        assert_eq!(detail.hands_won, 1);

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn review_done_without_a_pending_interception_is_an_error() {
        let mut session = make_session();
        let outcome = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#, None);
        assert_eq!(outcome.messages.len(), 1);
        let json = parse(&outcome.messages[0]);
        assert_eq!(json["type"], "ERROR");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("no blunder interception")
        );
    }

    #[test]
    fn submissions_are_rejected_while_a_review_is_pending() {
        let mut session = make_session();
        session.stage_pending_interception(Action::Fold, sample_analysis());

        let outcome = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#,
            None,
        );
        assert_eq!(outcome.messages.len(), 1);
        let json = parse(&outcome.messages[0]);
        assert_eq!(json["type"], "ERROR");
        assert!(json["message"].as_str().unwrap().contains("pending review"));
        assert!(
            session.has_pending_interception(),
            "the interception is still awaiting review"
        );
    }

    #[test]
    fn review_done_applies_a_held_back_interception() {
        let mut session = make_session();
        let legal = session.state().legal_actions();
        let action = if legal.can_check {
            Action::Check
        } else {
            Action::Fold
        };
        session.stage_pending_interception(action, sample_analysis());

        let outcome = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#, None);
        let types: Vec<String> = outcome
            .messages
            .iter()
            .map(|m| parse(m)["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.iter().any(|t| t == "CHART_TICK"), "{types:?}");
        assert_eq!(types.last().unwrap(), "TABLE_STATE_UPDATE");
        assert!(!session.has_pending_interception());
    }

    #[test]
    fn events_map_to_the_three_server_frames() {
        let mut session = make_session();
        let events = vec![
            TableEvent::TacticalOverlay {
                decision: sample_analysis(),
                hand_no: 1,
                intercepted: true,
            },
            TableEvent::ChartTick {
                action_index: 7,
                ev_loss: 2.0,
            },
            TableEvent::State,
        ];
        let messages = events_to_messages(&mut session, events);
        assert_eq!(messages.len(), 3);
        let overlay = parse(&messages[0]);
        assert_eq!(overlay["type"], "TRIGGER_TACTICAL_OVERLAY");
        assert_eq!(overlay["intercepted"], true);
        assert!(
            overlay["fragment"]
                .as_str()
                .unwrap()
                .contains("Blunder intercepted")
        );
        assert_eq!(
            parse(&messages[1]),
            json!({"type": "CHART_TICK", "action_index": 7, "ev_loss": 2.0})
        );
        assert_eq!(parse(&messages[2])["type"], "TABLE_STATE_UPDATE");
    }

    #[test]
    fn error_frames_round_trip() {
        let json = parse(&error_message("bad move"));
        assert_eq!(json, json!({"type": "ERROR", "message": "bad move"}));
    }

    #[test]
    fn state_message_serializes_a_fragment() {
        let mut session = make_session();
        let message = state_message(&mut session).unwrap();
        assert!(message.contains("\"type\":\"TABLE_STATE_UPDATE\""));
        assert!(message.contains("Hand #1"));
    }

    #[test]
    fn search_status_message_serializes_every_phase() {
        let result = crate::mcts::SolveResult {
            actions: vec![],
            worlds: 1,
            iterations: 1,
            max_depth: 1,
            nodes: 2,
            max_tree_depth: 1,
            rollout_actions: 5,
        };
        for (phase, tag) in [
            (SearcherPhase::Searching, "SEARCHING"),
            (SearcherPhase::DepthReached, "DEPTH_REACHED"),
            (SearcherPhase::Ready, "READY"),
        ] {
            let status = SearcherStatus {
                result: result.clone(),
                iterations_done: 1,
                target_iterations: 1,
                phase,
            };
            let json = parse(&search_status_message(&status));
            assert_eq!(json["phase"], tag);
            assert_eq!(json["iterations_done"], 1);
            assert_eq!(json["tree_depth"], 1);
            assert_eq!(json["max_depth"], 1);
        }
    }

    #[tokio::test]
    async fn snapshot_frame_without_a_pool_is_none() {
        assert_eq!(snapshot_frame(None).await, None);
    }

    async fn spawn_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        spawn_server_with(None, 100, 0).await
    }

    async fn spawn_server_with(
        pool: Option<PgPool>,
        snapshot_interval: usize,
        result_pause_ms: u64,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = ServeListener::new(
            listener,
            Arc::new(AppState {
                assets: default_assets(),
                mcts: MctsConfig::test(),
                survival: SurvivalConfig::default(),
                blunder: BlunderConfig::default(),
                pool,
                snapshot_interval,
                result_pause_ms,
            }),
        );
        let handle = tokio::spawn(async move {
            server.await_forever().await.unwrap();
        });
        (address, handle)
    }

    async fn next_text(stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match stream.next().await {
                    Some(Ok(TMessage::Text(text))) => return text.to_string(),
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => panic!("websocket error: {error}"),
                    None => panic!("websocket closed unexpectedly"),
                }
            }
        })
        .await
        .expect("timed out waiting for a server message")
    }

    #[tokio::test]
    async fn websocket_end_to_end_flow() {
        let (address, _server) = spawn_server().await;
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();

        let initial = parse(&next_text(&mut stream).await);
        assert_eq!(initial["type"], "TABLE_STATE_UPDATE");
        assert!(initial["fragment"].as_str().unwrap().contains("Hand #1"));

        stream
            .send(TMessage::Text(
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#.into(),
            ))
            .await
            .unwrap();
        let mut chart_tick = false;
        loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "CHART_TICK" => {
                    chart_tick = true;
                    assert_eq!(frame["action_index"], 1);
                }
                "TRIGGER_TACTICAL_OVERLAY" => {
                    assert_eq!(
                        frame["intercepted"], true,
                        "overlays are always intercepted"
                    );
                    assert!(
                        frame["fragment"]
                            .as_str()
                            .unwrap()
                            .contains("Blunder intercepted")
                    );
                    stream
                        .send(TMessage::Text(r#"{"type":"REVIEW_DONE"}"#.into()))
                        .await
                        .unwrap();
                }
                "TABLE_STATE_UPDATE" => break,
                "SEARCH_STATUS" => {}
                other => panic!("unexpected frame type {other}"),
            }
        }
        assert!(chart_tick, "every submitted action is charted");

        for (payload, expected) in [
            (
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"zzz"}}"#,
                "unknown action kind",
            ),
            ("not json at all", "malformed message"),
            (
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"raise"}}"#,
                "requires an amount",
            ),
        ] {
            stream.send(TMessage::Text(payload.into())).await.unwrap();
            let error = loop {
                let frame = parse(&next_text(&mut stream).await);
                match frame["type"].as_str().unwrap() {
                    "ERROR" => break frame,
                    // A hand that ended earlier may still ship its delayed
                    // next-deal state before the error frame arrives.
                    "TABLE_STATE_UPDATE" | "SEARCH_STATUS" => continue,
                    other => panic!("unexpected frame type {other} for payload {payload}"),
                }
            };
            assert!(
                error["message"].as_str().unwrap().contains(expected),
                "payload {payload} should be rejected: {error}"
            );
        }

        stream
            .send(TMessage::Binary(vec![1, 2, 3].into()))
            .await
            .unwrap();
        stream
            .send(TMessage::Text(
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"zzz"}}"#.into(),
            ))
            .await
            .unwrap();
        let frame = loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "ERROR" => break frame,
                "TABLE_STATE_UPDATE" | "SEARCH_STATUS" => continue,
                other => panic!("unexpected frame type {other}"),
            }
        };
        assert_eq!(frame["type"], "ERROR", "server must survive binary frames");

        stream.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn finishing_the_table_replies_with_the_navigation_frame() {
        let (address, _server) = spawn_server().await;
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let initial = parse(&next_text(&mut stream).await);
        assert_eq!(initial["type"], "TABLE_STATE_UPDATE");

        stream
            .send(TMessage::Text(r#"{"type":"FINISH_TABLE"}"#.into()))
            .await
            .unwrap();
        let frame = loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "SESSION_FINISHED" => break frame,
                "SEARCH_STATUS" => continue,
                other => panic!("unexpected frame type {other} while finishing"),
            }
        };
        assert_eq!(
            frame,
            json!({"type": "SESSION_FINISHED", "url": TOURNAMENTS_URL})
        );
        drop(stream);
    }

    #[tokio::test]
    async fn the_socket_shows_the_winner_then_deals_after_the_pause() {
        let (address, _server) = spawn_server().await;
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let initial = parse(&next_text(&mut stream).await);
        assert_eq!(initial["type"], "TABLE_STATE_UPDATE");

        // Folding is always legal on the hero's first decision (the hero
        // posted the small blind), and it always ends the hand — either
        // immediately or once the fold applies after a review.
        stream
            .send(TMessage::Text(
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"fold"}}"#.into(),
            ))
            .await
            .unwrap();

        let mut win_seen = false;
        loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "CHART_TICK" => {}
                "TRIGGER_TACTICAL_OVERLAY" => {
                    stream
                        .send(TMessage::Text(r#"{"type":"REVIEW_DONE"}"#.into()))
                        .await
                        .unwrap();
                }
                "TABLE_STATE_UPDATE" => {
                    let fragment = frame["fragment"].as_str().unwrap();
                    if fragment.contains("pt-win") {
                        win_seen = true;
                        assert!(
                            !fragment.contains(r#"id="action-panel""#),
                            "the dock is hidden while the winner shows: {fragment}"
                        );
                    } else if win_seen {
                        assert!(
                            fragment.contains("Hand #2"),
                            "the pause deals the next hand: {fragment}"
                        );
                        break;
                    }
                }
                "SEARCH_STATUS" => {}
                other => panic!("unexpected frame type {other}"),
            }
        }
        assert!(win_seen, "the win badge was shown at the winner's seat");
        drop(stream);
    }

    #[tokio::test]
    async fn server_survives_client_disconnects() {
        let (address, _server) = spawn_server().await;
        let (stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        drop(stream);

        tokio::time::sleep(Duration::from_millis(100)).await;
        // A fresh connection must still be served afterwards.
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let initial = parse(&next_text(&mut stream).await);
        assert_eq!(initial["type"], "TABLE_STATE_UPDATE");
        drop(stream);
    }

    #[tokio::test]
    async fn pooled_connection_persists_decisions_and_snapshots_history() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        dotenvy::dotenv().ok();
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => panic!(
                "DATABASE_URL is required for database integration tests; start PostgreSQL via pg.ps1"
            ),
        };
        let pool = crate::db::connect(&database_url).await.unwrap();
        let sessions_before: Option<i32> = sqlx::query_scalar("SELECT max(id) FROM hero_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();

        let (address, _server) = spawn_server_with(Some(pool.clone()), 1, 0).await;
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();

        let initial = parse(&next_text(&mut stream).await);
        assert_eq!(initial["type"], "TABLE_STATE_UPDATE");
        let snapshot = parse(&next_text(&mut stream).await);
        assert_eq!(
            snapshot["type"], "CHART_SNAPSHOT",
            "a pooled connection replays the stored history first"
        );

        stream
            .send(TMessage::Text(
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#.into(),
            ))
            .await
            .unwrap();
        let mut refreshed = false;
        let mut state_seen = false;
        loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "CHART_SNAPSHOT" => {
                    refreshed = true;
                    assert!(
                        !frame["points"].as_array().unwrap().is_empty(),
                        "the snapshot covers at least the played action"
                    );
                    if state_seen {
                        break;
                    }
                }
                "TRIGGER_TACTICAL_OVERLAY" => {
                    stream
                        .send(TMessage::Text(r#"{"type":"REVIEW_DONE"}"#.into()))
                        .await
                        .unwrap();
                }
                "TABLE_STATE_UPDATE" => {
                    state_seen = true;
                    if refreshed {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(
            refreshed,
            "the snapshot refreshes once the interval elapses"
        );
        assert!(state_seen, "the table state followed the played action");

        stream
            .send(TMessage::Text(r#"{"type":"FINISH_TABLE"}"#.into()))
            .await
            .unwrap();
        let finished = loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "SESSION_FINISHED" => break frame,
                // Delayed post-result deals or snapshot refreshes may arrive
                // before the navigation frame.
                "TABLE_STATE_UPDATE" | "CHART_SNAPSHOT" | "SEARCH_STATUS" => continue,
                other => panic!("unexpected frame type {other} while finishing"),
            }
        };
        assert_eq!(finished["type"], "SESSION_FINISHED");
        drop(stream);

        let recordings: Vec<(i32, i32, i32, f64)> = sqlx::query_as(
            "SELECT d.session_id, d.hand_number, d.street, d.ev_loss
             FROM hero_decisions d
             JOIN hero_sessions s ON s.id = d.session_id
             WHERE s.id > $1",
        )
        .bind(sessions_before.unwrap_or(0))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!recordings.is_empty(), "the hero decision was persisted");
        let (recorded_session, hand_no, street, ev_loss) = recordings[0];
        assert!(hand_no >= 1);
        assert!((0..=3).contains(&street), "street index 0..=3");
        assert!(ev_loss >= 0.0);

        let finalized: Option<String> = sqlx::query_scalar(
            "SELECT session_end::text FROM hero_sessions WHERE id = $1 AND session_end IS NOT NULL",
        )
        .bind(recorded_session)
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(
            finalized.is_some(),
            "finishing the table finalizes the analytics session"
        );

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(recorded_session)
            .execute(&pool)
            .await
            .unwrap();
    }
}
