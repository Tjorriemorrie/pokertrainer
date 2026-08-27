use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::analytics::{self, PendingHandResult};
use crate::blunder::BlunderConfig;
use crate::error::{Error, Result};
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

/// Where the client is sent once it finishes the table: the dashboard.
pub const DASHBOARD_URL: &str = "/";

/// Where the client is sent once it finishes the table or is rejected
/// because the table is open elsewhere — see [`DASHBOARD_URL`].
pub const TOURNAMENTS_URL: &str = "/drill";

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

/// Upgrades `/ws` connections. Each connection claims the single active
/// tournament: with a database it resumes the stored snapshot when one
/// exists (otherwise it starts a brand-new tournament), and without one it
/// plays a fresh, in-memory table as before.
pub async fn handler(State(app): State<Arc<AppState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

/// Claims the single active table for a connection. A live connection
/// elsewhere, or any store failure, is a claim error.
async fn claim_table(
    pool: Option<&PgPool>,
) -> Result<(Option<i32>, Option<crate::snapshot::TournamentSnapshot>)> {
    let Some(pool) = pool else {
        return Ok((None, None));
    };
    match crate::live::claim_or_resume(pool).await? {
        crate::live::ClaimOutcome::Fresh(session_id) => Ok((Some(session_id), None)),
        crate::live::ClaimOutcome::Resumed {
            session_id,
            snapshot,
        } => Ok((Some(session_id), *snapshot)),
        crate::live::ClaimOutcome::Taken => Err(Error::Decision(
            "the table is already open in another window".into(),
        )),
    }
}

/// Points a rejected connection back at the dashboard and hangs up.
async fn reject_socket(mut socket: WebSocket, reason: &str) {
    tracing::warn!(reason, "connection rejected");
    let socket = &mut socket;
    if let Some(frame) = session_finished_message()
        && socket.send(Message::Text(frame.into())).await.is_err()
    {
        // The client is already gone; nothing more to do.
    }
    let _ = socket.close().await;
}

/// Loads the opponent's model for a fresh session: refreshes the historic
/// window (writing the updated range model and producing the coach panel's
/// historic read and starting-hand table), then resolves the gated per-node
/// range priors the bots' own solve uses. Falls back to an empty
/// (all-uniform, no history) model when there is no pool or any step fails —
/// bots then play exactly as before this feature existed.
async fn load_opponent_model(pool: Option<&PgPool>) -> crate::opponent_history::OpponentModel {
    let Some(pool) = pool else {
        return crate::opponent_history::OpponentModel::default();
    };
    let historic = match crate::opponent_history::refresh(pool).await {
        Ok(summary) => summary,
        Err(error) => {
            tracing::warn!(%error, "opponent history refresh failed — bots play with a uniform prior");
            return crate::opponent_history::OpponentModel::default();
        }
    };
    let hero_historic = match crate::opponent_history::refresh_hero(pool).await {
        Ok(summary) => summary,
        Err(error) => {
            tracing::warn!(%error, "hero history refresh failed — the hero starting-hand grid stays empty");
            crate::opponent_history::HistorySummary::default()
        }
    };
    let profile_id = match crate::opponent_history::pooled_profile_id(pool).await {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "opponent profile unavailable — bots play with a uniform prior");
            return crate::opponent_history::OpponentModel {
                ranges: Default::default(),
                historic,
                hero_historic,
            };
        }
    };
    let ranges = match crate::opponent_history::load_range_model(pool, profile_id).await {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(%error, "opponent range model unavailable — bots play with a uniform prior");
            Default::default()
        }
    };
    crate::opponent_history::OpponentModel {
        ranges,
        historic,
        hero_historic,
    }
}

async fn handle_socket(socket: WebSocket, app: Arc<AppState>) {
    let (session_id, snapshot) = match claim_table(app.pool.as_ref()).await {
        Ok(claim) => claim,
        Err(error) => {
            tracing::warn!(%error, "table claim failed; closing connection");
            reject_socket(socket, "the table is already open in another window").await;
            return;
        }
    };
    let opponent_model = load_opponent_model(app.pool.as_ref()).await;

    let mut session = match snapshot {
        Some(snapshot) => {
            match TableSession::from_snapshot(
                &snapshot,
                rand::random::<u64>(),
                app.mcts,
                app.blunder,
                opponent_model,
            ) {
                Ok(mut session) => {
                    hydrate_blunder(app.pool.as_ref(), session_id, app.blunder, &mut session).await;
                    tracing::info!(session_id = ?session_id, hand_no = session.hand_no(), "resumed stored tournament");
                    session
                }
                Err(error) => {
                    tracing::error!(%error, "stored snapshot cannot be restored; refusing to overwrite the tournament");
                    reject_socket(socket, "the stored table state is corrupted").await;
                    return;
                }
            }
        }
        None => {
            let template = match app.pool.as_ref() {
                Some(pool) => match crate::opponent_analysis::load_template(pool).await {
                    Ok(Some(template)) => {
                        tracing::info!(
                            skill = template.skill,
                            decisions = template.decisions,
                            "bots loaded the saved field template"
                        );
                        Some(crate::opponent::OpponentTemplate::new(template.skill))
                    }
                    Ok(None) => None,
                    Err(error) => {
                        tracing::warn!(%error, "bot template unavailable — playing the placeholder policy");
                        None
                    }
                },
                None => None,
            };
            let starting_stack = match app.pool.as_ref() {
                Some(pool) => {
                    match crate::hh::modal_starting_stack(pool, crate::hh::STARTING_STACK_WINDOW)
                        .await
                    {
                        Ok(Some(stack)) => stack,
                        Ok(None) => crate::game::STARTING_STACK,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "starting chip modal unavailable — using the engine default"
                            );
                            crate::game::STARTING_STACK
                        }
                    }
                }
                None => crate::game::STARTING_STACK,
            };
            tracing::info!(starting_stack, "new tournament starting chips");
            let mut session = TableSession::new(
                rand::random::<u64>(),
                app.mcts,
                app.blunder,
                template,
                starting_stack,
                opponent_model,
            );
            if let Err(error) = bootstrap(&mut session) {
                tracing::warn!(%error, "table session bootstrap failed; closing connection");
                reject_socket(socket, "the table could not be dealt").await;
                return;
            }
            hydrate_blunder(app.pool.as_ref(), session_id, app.blunder, &mut session).await;
            tracing::info!(session_id = ?session_id, "new tournament started");
            session
        }
    };

    let end = play_socket(socket, app.clone(), session_id, &mut session).await;

    match end {
        // A finished (or given-up) tournament already finalized its session
        // and released the active row — there is nothing left to persist.
        ConnectionEnd::Released => {}
        // A plain disconnect leaves the tournament open for the next visit:
        // save the final state, then release the claim.
        ConnectionEnd::Disconnected => {
            save_table(app.pool.as_ref(), session_id, &session).await;
            if let Some(pool) = app.pool.as_ref()
                && let Err(error) = crate::live::mark_disconnected(pool).await
            {
                tracing::warn!(%error, "table could not be released");
            }
        }
    }
}

/// How a table connection ended: by giving up / finishing the tournament
/// (the row is already released and finalized) or by simply disconnecting
/// (the tournament stays open for a resume).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionEnd {
    Released,
    Disconnected,
}

/// Rebuilds the blunder tracker from the hero's stored decisions across
/// every session, so a brand-new game starts already calibrated instead of
/// cold and a resumed table's intervention threshold picks up exactly where
/// it stopped.
async fn hydrate_blunder(
    pool: Option<&PgPool>,
    session_id: Option<i32>,
    blunder: BlunderConfig,
    session: &mut TableSession,
) {
    let (Some(pool), Some(session_id)) = (pool, session_id) else {
        return;
    };
    match analytics::load_recent_losses(pool, blunder.history_actions).await {
        Ok(history) => {
            session.hydrate_blunder(&history);
            tracing::info!(
                session_id,
                actions = history.len(),
                "blunder history restored"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "blunder history unavailable — resuming with a fresh tracker")
        }
    }
}

/// Sends a batch of text frames; `false` once the socket is gone.
async fn send_all<S>(sender: &mut S, frames: Vec<String>) -> bool
where
    S: futures_util::SinkExt<Message> + Unpin,
{
    for frame in frames {
        if sender.send(Message::Text(frame.into())).await.is_err() {
            return false;
        }
    }
    true
}

/// Persists the current table snapshot for a later resume.
async fn save_table(pool: Option<&PgPool>, session_id: Option<i32>, session: &TableSession) {
    let (Some(pool), Some(session_id)) = (pool, session_id) else {
        return;
    };
    if let Err(error) = crate::live::save_snapshot(pool, session_id, &session.to_snapshot()).await {
        tracing::warn!(%error, session_id, "table snapshot could not be persisted");
    }
}

/// Runs the table until the connection ends: the lobby-to-solver lifecycle,
/// with the tournament snapshot persisted after every applied change. Returns
/// how the connection ended — giving up or finishing releases the active row
/// inside the loop; a plain disconnect leaves the tournament open for a
/// resume.
async fn play_socket(
    socket: WebSocket,
    app: Arc<AppState>,
    session_id: Option<i32>,
    session: &mut TableSession,
) -> ConnectionEnd {
    let (mut sender, mut receiver) = socket.split();

    // The table — deal, blinds, opponents — renders before any solver work
    // starts; the background search is spawned only after this initial state
    // has left for the client.
    let state_frame =
        state_message(session).unwrap_or_else(|error| error_message(&error.to_string()));
    let range_tables_frame =
        range_tables_message(session).unwrap_or_else(|error| error_message(&error.to_string()));
    let mut initial = vec![state_frame, range_tables_frame];
    if let Some(snapshot) = snapshot_frame(app.pool.as_ref()).await {
        initial.push(snapshot);
    }
    if !send_all(&mut sender, initial).await {
        return ConnectionEnd::Disconnected;
    }
    session.take_pump_actions();

    // A resumed table can park on the winner ribbon: honor the result pause
    // and deal on before the solver starts — unless the tournament already
    // ended there (a busted hero gets the loser modal immediately, even
    // mid-hand).
    let resume =
        post_resume_frames(session, app.pool.as_ref(), session_id, app.result_pause_ms).await;
    let tournament_over = session.tournament_result().is_some();
    if !send_all(&mut sender, resume).await {
        return ConnectionEnd::Disconnected;
    }
    if tournament_over {
        let _ = sender.close().await;
        return ConnectionEnd::Released;
    }
    session.take_pump_actions();
    save_table(app.pool.as_ref(), session_id, session).await;

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
        decision: session.decision_token().unwrap_or_default(),
    });

    let mut latest_snapshot: Option<crate::mcts::SolveResult> = None;
    let mut solver_alive = true;
    let mut ticks_since_snapshot = 0usize;

    let end = 'socket: loop {
        tokio::select! {
            maybe = receiver.next() => {
                let Some(message) = maybe else { break ConnectionEnd::Disconnected };
                let text = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Close(_)) | Err(_) => break ConnectionEnd::Disconnected,
                    Ok(_) => continue,
                };

                let outcome = handle_client_message(&mut *session, text.as_str(), latest_snapshot.as_ref());
                ticks_since_snapshot += outcome.chart_ticks;
                persist_records(app.pool.as_ref(), session_id, session).await;
                persist_local_actions(app.pool.as_ref(), session).await;
                persist_local_hero_actions(app.pool.as_ref(), session).await;
                // Save before the frames leave, so a disconnect triggered by
                // the rendered state resumes exactly that state.
                save_table(app.pool.as_ref(), session_id, session).await;

                if !send_all(&mut sender, outcome.messages).await {
                    let _ = command_tx.send(SearchCommand::Stop);
                    break ConnectionEnd::Disconnected;
                }

                // A tournament ends the moment the hero busts out (the
                // opponents never play on without the hero) or only one seat
                // is left standing: persist the hand results, finalize the
                // session with the outcome, and hand the client a winner/loser
                // modal instead of dealing another hand.
                if let Some(frame) =
                    tournament_over_frame(session, app.pool.as_ref(), session_id).await
                {
                    let _ = command_tx.send(SearchCommand::Stop);
                    if sender.send(Message::Text(frame.into())).await.is_err() {
                        break ConnectionEnd::Disconnected;
                    }
                    let _ = sender.close().await;
                    break ConnectionEnd::Released;
                }

                if outcome.finish_table {
                    let _ = command_tx.send(SearchCommand::Stop);
                    finish_table(app.pool.as_ref(), session_id, session).await;
                    if let Some(frame) = session_finished_message()
                        && sender.send(Message::Text(frame.into())).await.is_err()
                    {
                        break ConnectionEnd::Disconnected;
                    }
                    let _ = sender.close().await;
                    break ConnectionEnd::Released;
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
                            decision: session.decision_token().unwrap_or_default(),
                        });
                    } else {
                        session.take_pump_actions();
                    }
                }

                while session.state().is_hand_over() {
                    tokio::time::sleep(std::time::Duration::from_millis(app.result_pause_ms)).await;
                    if !send_all(&mut sender, advance_frames(&mut *session)).await {
                        let _ = command_tx.send(SearchCommand::Stop);
                        break 'socket ConnectionEnd::Disconnected;
                    }
                    session.take_pump_actions();
                    save_table(app.pool.as_ref(), session_id, session).await;
                    // Reshape only when the pump left a live hero decision;
                    // a hand-over state has no decision and the searcher
                    // refuses to reshape onto a finished hand.
                    if let Some(decision) = session.decision_token() {
                        let _ = command_tx.send(SearchCommand::Reshape {
                            state: Box::new(observable_clone(session.state())),
                            path: None,
                            hand_no: session.hand_no(),
                            decision,
                        });
                    }
                    // The freshly dealt hand can still end the tournament (the
                    // hero posts an all-in blind and the opponents play it
                    // out to a showdown), so the tournament may end here too
                    // — stop and show the modal instead of dealing on. A
                    // freshly dealt hand can also end uncontested again right
                    // away (e.g. the hero is the only seat left to act
                    // against), so this stays a `while` above: keep pausing
                    // and dealing until a hand actually leaves the hero a
                    // decision.
                    if let Some(frame) =
                        tournament_over_frame(session, app.pool.as_ref(), session_id).await
                    {
                        let _ = command_tx.send(SearchCommand::Stop);
                        if sender.send(Message::Text(frame.into())).await.is_err() {
                            break 'socket ConnectionEnd::Disconnected;
                        }
                        let _ = sender.close().await;
                        break 'socket ConnectionEnd::Released;
                    }
                }

                if ticks_since_snapshot >= app.snapshot_interval.max(1) {
                    ticks_since_snapshot = 0;
                    if let Some(frame) = snapshot_frame(app.pool.as_ref()).await
                        && sender.send(Message::Text(frame.into())).await.is_err()
                    {
                        let _ = command_tx.send(SearchCommand::Stop);
                        break ConnectionEnd::Disconnected;
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
                            break ConnectionEnd::Disconnected;
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
    };
    let _ = command_tx.send(SearchCommand::Stop);
    solver.abort();
    end
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
    let mut pending: Option<(Box<GameState>, Option<PursuedPath>, u64, String)> = None;

    loop {
        let mut stop = false;
        while let Ok(command) = commands.try_recv() {
            match command {
                SearchCommand::Stop => stop = true,
                SearchCommand::Reshape {
                    state,
                    path,
                    hand_no,
                    decision,
                } => {
                    pending = Some((state, path, hand_no, decision));
                }
            }
        }
        if stop {
            return;
        }

        if search.is_none() {
            let Some((state, _, hand_no, decision)) = pending.take() else {
                match commands.recv().await {
                    Some(SearchCommand::Stop) | None => return,
                    Some(SearchCommand::Reshape {
                        state,
                        path,
                        hand_no,
                        decision,
                    }) => {
                        pending = Some((state, path, hand_no, decision));
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
                    &decision,
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
                    decision,
                }) => {
                    pending = Some((state, path, hand_no, decision));
                }
            }
        }

        let active = search.take().expect("built above");
        let reshape = pending.take();
        let updates = updates.clone();
        let outcome = tokio::task::spawn_blocking(move || -> Result<Searcher> {
            let mut active = active;
            if let Some((state, path, hand_no, decision)) = reshape {
                active.reshape(&state, path.as_ref(), hand_no, &decision)?;
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
        decision: status.decision.clone(),
    })
    .to_json()
    {
        Ok(json) => json,
        Err(error) => error_message(&error.to_string()),
    }
}

/// Finalizes the analytics session when the tournament ends (a natural
/// winner or an explicit give-up). Disconnects leave the session open so the
/// table can be resumed.
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

/// Persists every local bot decision queued by the table since the last
/// frame, into `local_opponent_actions` (no `session_id` — the window pools
/// across every session). Failures are logged and dropped — the game never
/// blocks on the database.
async fn persist_local_actions(pool: Option<&PgPool>, session: &mut TableSession) {
    let actions = session.take_local_actions();
    if actions.is_empty() {
        return;
    }
    let Some(pool) = pool else {
        return;
    };
    if let Err(error) = crate::db::insert_local_opponent_actions(pool, &actions).await {
        tracing::warn!(
            %error,
            dropped = actions.len(),
            "local opponent actions could not be persisted — the table keeps playing"
        );
    }
}

/// Persists every local hero decision queued by the table since the last
/// frame, into `local_hero_actions`. Mirrors [`persist_local_actions`].
async fn persist_local_hero_actions(pool: Option<&PgPool>, session: &mut TableSession) {
    let actions = session.take_local_hero_actions();
    if actions.is_empty() {
        return;
    }
    let Some(pool) = pool else {
        return;
    };
    if let Err(error) = crate::db::insert_local_hero_actions(pool, &actions).await {
        tracing::warn!(
            %error,
            dropped = actions.len(),
            "local hero actions could not be persisted — the table keeps playing"
        );
    }
}

/// The last 1,000 stored actions, one point per action; an empty dataset
/// means there is no stored history.
async fn snapshot_frame(pool: Option<&PgPool>) -> Option<String> {
    let pool = pool?;
    match analytics::load_recent(pool, analytics::CHART_WINDOW).await {
        Ok(points) => Some(
            match (ServerMessage::ChartSnapshot { points }).to_json() {
                Ok(json) => json,
                Err(error) => error_message(&error.to_string()),
            },
        ),
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
        url: DASHBOARD_URL.to_string(),
    })
    .to_json()
    {
        Ok(json) => Some(json),
        Err(error) => Some(error_message(&error.to_string())),
    }
}

/// Finish table = give up: the tournament is recorded as a LOSS with the
/// hero's current stack and the active row is released, so the dashboard can
/// offer a brand-new tournament.
async fn finish_table(pool: Option<&PgPool>, session_id: Option<i32>, session: &TableSession) {
    let (Some(pool), Some(session_id)) = (pool, session_id) else {
        return;
    };
    let final_stack = session.state().stack(Seat::Hero) as i32;
    if let Err(error) = analytics::finalize_session(pool, session_id, "LOSS", final_stack).await {
        tracing::warn!(%error, session_id, "gave-up tournament could not be finalized as a loss");
    }
    if let Err(error) = crate::live::clear_active(pool).await {
        tracing::warn!(%error, session_id, "finished table could not release the active row");
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
        Some(id) => format!("{TOURNAMENTS_URL}?highlight={id}"),
        None => TOURNAMENTS_URL.to_string(),
    };
    tournament_finished_message(result.won, &url)
        .unwrap_or_else(|| error_message("serialization failure"))
}

/// When the tournament just ended, finalizes the session (persisting the
/// queued hand results and outcome), releases the active-tournament row, and
/// returns the winner/loser modal frame; `None` while it is still running.
async fn tournament_over_frame(
    session: &mut TableSession,
    pool: Option<&PgPool>,
    session_id: Option<i32>,
) -> Option<String> {
    let result = session.tournament_result()?;
    let hand_results = session.take_hand_results();
    let frame = finalize_tournament(pool, session_id, &result, hand_results).await;
    close_session(pool, session_id).await;
    if let Some(pool) = pool
        && let Err(error) = crate::live::clear_active(pool).await
    {
        tracing::warn!(%error, session_id, "finished tournament could not release the active row");
    }
    Some(frame)
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
        fragment: views::table_fragment(
            session.state(),
            session.hand_no(),
            session.action_no(),
            session.log(),
            &sounds,
        )?,
    }
    .to_json()
}

/// The always-visible starting-hands panel (hero + bot grids), built once
/// from the session's history loaded at connect — see
/// [`load_opponent_model`]. Sent once, right after the initial table state.
fn range_tables_message(session: &TableSession) -> Result<String> {
    ServerMessage::RangeTablesUpdate {
        fragment: views::starting_hands_fragment(session.opponent_history(), session.hero_history())?,
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

/// The frames to send right after the connect-time state frame: none while a
/// decision is live, the result-pause next deal when the resumed table was
/// parked on a finished hand, or the winner/loser modal when the tournament
/// ended (on that exact hand, or anywhere the hero is already out).
async fn post_resume_frames(
    session: &mut TableSession,
    pool: Option<&PgPool>,
    session_id: Option<i32>,
    pause_ms: u64,
) -> Vec<String> {
    if let Some(frame) = tournament_over_frame(session, pool, session_id).await {
        return vec![frame];
    }
    let mut frames = Vec::new();
    // A freshly dealt hand can end uncontested again right away (e.g. the
    // hero is the only seat left to act against), so this keeps pausing and
    // dealing until a hand actually leaves the hero a decision, or the
    // tournament ends.
    while session.state().is_hand_over() {
        tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
        frames.extend(advance_frames(session));
        if let Some(frame) = tournament_over_frame(session, pool, session_id).await {
            frames.push(frame);
            return frames;
        }
    }
    frames
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
            if action.kind.trim().eq_ignore_ascii_case("check_fold") {
                let submitted = match snapshot {
                    Some(snapshot) => session.submit_check_fold_with_snapshot(snapshot),
                    None => session.submit_check_fold(),
                };
                return match submitted {
                    Ok(events) => outcome(session, events, Some(Action::Check)),
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
                };
            }
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
                views::table_fragment(
                    session.state(),
                    session.hand_no(),
                    session.action_no(),
                    session.log(),
                    &sounds,
                )
                .and_then(|fragment| ServerMessage::TableStateUpdate { fragment }.to_json())
            }
            TableEvent::TacticalOverlay {
                decision,
                hand_no,
                intercepted,
            } => views::tactical_overlay_fragment(
                hand_no,
                &decision,
                intercepted,
                &session.merged_opponent_snapshot(),
                session.opponent_history(),
                session.state().legal_actions().call_amount,
                session.state().stack(Seat::Hero),
                session.state().street(),
            )
            .and_then(|fragment| {
                ServerMessage::TriggerTacticalOverlay {
                    fragment,
                    intercepted,
                }
                .to_json()
            }),
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
    use crate::decision::{Analysis, AnalyzedDecision, PlayedEvaluation, SearchReport};
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
            BlunderConfig::default(),
            None,
            crate::game::STARTING_STACK,
            crate::opponent_history::OpponentModel::default(),
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
                decision: "h1-a0-preflop".into(),
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
                decision: "h1-a0-preflop".into(),
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
                decision: "h2-a0-preflop".into(),
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
                decision: "h1-a0-preflop".into(),
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
                decision: "h1-a0-preflop".into(),
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
                decision: "h1-a0-preflop".into(),
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
                decision: "h2-a0-preflop".into(),
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
            claim_table(Some(&pool)).await.is_err(),
            "an unreachable database rejects the claim"
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

    #[tokio::test]
    async fn post_resume_frames_leave_live_decisions_untouched() {
        let mut session = make_session();
        assert_eq!(session.state().to_act(), Seat::Hero);
        let frames = post_resume_frames(&mut session, None, Some(7), 0).await;
        assert!(
            frames.is_empty(),
            "a live decision needs no extra connect frames"
        );
    }

    /// Regression test for a table that froze after auto-playing one hand: a
    /// heads-up table (the third seat already busted) parked on a finished
    /// hand can deal straight into another hand that folds around
    /// uncontested before the hero ever gets a decision — the short-stacked
    /// small blind folds preflop to the hero's big blind with nobody left to
    /// act. `post_resume_frames` must keep pausing and dealing through every
    /// such uncontested hand rather than stopping after the first one and
    /// leaving the table stuck with no further frames to send.
    #[tokio::test]
    async fn post_resume_frames_deals_through_consecutive_uncontested_hands() {
        use crate::card::Deck;
        use crate::game::blinds::BlindLevel;
        use crate::server::session::apply_settled;

        let mut state = GameState::new(Seat::Hero, BlindLevel::new(10, 20));
        state.set_stack(Seat::Hero, 2000);
        state.set_stack(Seat::Opponent1, 200);
        state.set_stack(Seat::Opponent2, 0);
        state.set_eliminated(Seat::Opponent2, true);
        // Seed 3 is a fixed point found for this stack/blind setup: the
        // hero's own preflop fold ends hand 1, and dealing hand 2 (button
        // rotates to Opponent 1) has Opponent 1 fold preflop before the hero
        // ever acts — reproducing the reported freeze.
        let seed = 3;
        let mut deck = Deck::shuffled(&mut crate::rng::seeded_rng(seed));
        state.start_hand(&mut deck).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        apply_settled(&mut state, &mut deck, Action::Fold).unwrap();
        assert!(state.is_hand_over(), "the hero's fold ends hand 1");

        let mut session = TableSession::resume(
            state,
            deck,
            1,
            seed,
            MctsConfig::test(),
            BlunderConfig::default(),
            None,
        );

        let frames = post_resume_frames(&mut session, None, None, 0).await;

        assert!(
            !session.state().is_hand_over(),
            "the table must not be left parked on a finished hand with nobody to advance it"
        );
        assert_eq!(
            session.state().to_act(),
            Seat::Hero,
            "the table must stop only once the hero has a live decision"
        );
        assert!(
            frames.len() >= 2,
            "hand 2 folded around uncontested too, so a second pause/deal was required: {frames:?}"
        );
    }

    /// A table parked on the winner ribbon resumes with the result pause's
    /// next deal, delivered as the frame right after the state frame.
    #[tokio::test]
    async fn post_resume_frames_deal_the_next_hand_after_a_parked_result() {
        let mut session = make_session();
        session.stage_pending_interception(Action::Fold, sample_analysis());
        let _ = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#, None);
        assert!(session.state().is_hand_over(), "the fold ended the hand");
        assert_eq!(session.hand_no(), 1);

        let frames = post_resume_frames(&mut session, None, Some(7), 0).await;
        assert_eq!(frames.len(), 1, "the paused result deals the next hand");
        let next = parse(&frames[0]);
        assert_eq!(next["type"], "TABLE_STATE_UPDATE");
        assert!(
            next["fragment"].as_str().unwrap().contains("Hand #2"),
            "the resumed table moves on to the next hand"
        );
    }

    /// When the tournament ended on the hand the table was parked on, the
    /// connect frames hand the client the winner/loser modal instead of
    /// dealing another hand.
    #[tokio::test]
    async fn post_resume_frames_modal_for_a_tournament_ended_on_the_parked_hand() {
        let mut state = hero_decision_state();
        state.set_stack(Seat::Opponent1, 0);
        state.set_stack(Seat::Opponent2, 0);
        state.set_eliminated(Seat::Opponent1, true);
        state.set_eliminated(Seat::Opponent2, true);
        let mut session = TableSession::resume(
            state,
            crate::card::Deck::default(),
            9,
            99,
            MctsConfig::test(),
            BlunderConfig::default(),
            None,
        );
        session.stage_pending_interception(Action::Fold, sample_analysis());
        let _ = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#, None);
        assert!(session.state().is_hand_over());

        let frames = post_resume_frames(&mut session, None, Some(9), 0).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(parse(&frames[0])["type"], "TOURNAMENT_FINISHED");
    }

    /// A resumed table whose hero is already busted gets the loser modal
    /// immediately — even mid-hand, the opponents never play on.
    #[tokio::test]
    async fn post_resume_frames_modal_when_the_hero_is_busted() {
        let mut state = hero_decision_state();
        state.set_stack(Seat::Hero, 0);
        state.set_stack(Seat::Opponent1, 420);
        state.set_stack(Seat::Opponent2, 1080);
        state.set_eliminated(Seat::Hero, true);
        let mut session = TableSession::resume(
            state,
            crate::card::Deck::default(),
            23,
            96,
            MctsConfig::test(),
            BlunderConfig::default(),
            None,
        );
        assert!(!session.state().is_hand_over(), "the hero busted mid-hand");

        let frames = post_resume_frames(&mut session, None, Some(7), 0).await;
        assert_eq!(frames.len(), 1);
        assert_eq!(
            parse(&frames[0]),
            json!({
                "type": "TOURNAMENT_FINISHED",
                "won": false,
                "url": "/drill?highlight=7"
            })
        );
    }

    #[tokio::test]
    async fn claim_table_without_a_database_is_a_fresh_memory_table() {
        assert_eq!(
            claim_table(None).await.unwrap(),
            (None, None),
            "no persistence means no session id and no snapshot"
        );
    }

    fn sample_analysis() -> AnalyzedDecision {
        let fold = Analysis {
            action: Action::Fold,
            bucket: None,
            ev: 0.0,
            variance: 0.0,
            bust_prob: 0.0,
            visits: 120,
        };
        AnalyzedDecision {
            ranking: vec![fold],
            optimal: fold,
            played: Some(PlayedEvaluation {
                analysis: fold,
                ev_loss_bb: 0.0,
                ev_loss_pot: 0.0,
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
            json!({"type": "SESSION_FINISHED", "url": DASHBOARD_URL})
        );
    }

    #[test]
    fn tournament_finished_message_carries_the_outcome_and_detail_url() {
        let message = tournament_finished_message(true, "/drill/7").unwrap();
        assert_eq!(
            parse(&message),
            json!({"type": "TOURNAMENT_FINISHED", "won": true, "url": "/drill/7"})
        );
        let loss = tournament_finished_message(false, "/drill/9").unwrap();
        assert_eq!(
            parse(&loss),
            json!({"type": "TOURNAMENT_FINISHED", "won": false, "url": "/drill/9"})
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
            json!({"type": "TOURNAMENT_FINISHED", "won": false, "url": "/drill?highlight=7"})
        );
    }

    #[tokio::test]
    async fn finalize_tournament_persists_results_and_finalizes_the_session() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
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
                ev_loss_pot: 1.0,
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
            json!({"type": "TOURNAMENT_FINISHED", "won": true, "url": format!("/drill?highlight={session_id}")})
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

    #[tokio::test]
    async fn tournament_over_frame_matches_the_session_outcome() {
        let mut state = hero_decision_state();
        state.set_stack(Seat::Opponent1, 0);
        state.set_stack(Seat::Opponent2, 0);
        state.set_eliminated(Seat::Opponent1, true);
        state.set_eliminated(Seat::Opponent2, true);
        let mut session = TableSession::resume(
            state,
            crate::card::Deck::default(),
            9,
            99,
            MctsConfig::test(),
            BlunderConfig::default(),
            None,
        );
        assert!(
            session.tournament_result().unwrap().won,
            "the hero is the only seat left"
        );

        let frame = tournament_over_frame(&mut session, None, Some(7)).await;
        assert_eq!(
            parse(&frame.unwrap()),
            json!({"type": "TOURNAMENT_FINISHED", "won": true, "url": "/drill?highlight=7"})
        );
        assert!(
            session.take_hand_results().is_empty(),
            "finalization drains the queued hand results"
        );
    }

    #[tokio::test]
    async fn tournament_over_frame_is_none_while_the_tournament_runs() {
        let mut session = make_session();
        let frame = tournament_over_frame(&mut session, None, Some(9)).await;
        assert_eq!(frame, None, "a live table has no final modal yet");
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
                decision: Box::new(sample_analysis()),
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
                decision: "h1-a0-preflop".into(),
            };
            let json = parse(&search_status_message(&status));
            assert_eq!(json["phase"], tag);
            assert_eq!(json["iterations_done"], 1);
            assert_eq!(json["tree_depth"], 1);
            assert_eq!(json["max_depth"], 1);
            assert_eq!(json["decision"], "h1-a0-preflop");
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
                blunder: BlunderConfig::default(),
                pool,
                snapshot_interval,
                result_pause_ms,
                history_dir: crate::hh::default_history_dir(),
                analysis: Arc::new(std::sync::Mutex::new(
                    crate::opponent_analysis::JobState::Idle,
                )),
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

    /// Like [`next_text`], but returns `None` instead of panicking when the
    /// connection errors or closes. Tests whose tournament can legitimately
    /// end mid-exchange (a knockout at the engine's 15bb starting stacks,
    /// before the hero sees the frame sequence they're driving toward) use
    /// this to detect that outcome and retry with a fresh table, rather than
    /// treating a real but out-of-scope conclusion as a failure. The server
    /// closes the socket once it finalizes a naturally-concluded tournament,
    /// so this can surface as a clean close or an aborted-connection error
    /// depending on exactly when the client was reading.
    async fn next_text_or_closed(
        stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match stream.next().await {
                    Some(Ok(TMessage::Text(text))) => return Some(text.to_string()),
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return None,
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
                "SEARCH_STATUS" | "RANGE_TABLES_UPDATE" => {}
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
                    "TABLE_STATE_UPDATE" | "SEARCH_STATUS" | "RANGE_TABLES_UPDATE" => continue,
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
                "TABLE_STATE_UPDATE" | "SEARCH_STATUS" | "RANGE_TABLES_UPDATE" => continue,
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
                "SEARCH_STATUS" | "RANGE_TABLES_UPDATE" => continue,
                other => panic!("unexpected frame type {other} while finishing"),
            }
        };
        assert_eq!(
            frame,
            json!({"type": "SESSION_FINISHED", "url": DASHBOARD_URL})
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
                "SEARCH_STATUS" | "RANGE_TABLES_UPDATE" => {}
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
        let pool = crate::db::test_pool().await;
        let sessions_before: Option<i32> = sqlx::query_scalar("SELECT max(id) FROM hero_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();

        let (address, _server) = spawn_server_with(Some(pool.clone()), 1, 0).await;

        // Every fresh tournament is dealt from a genuinely random seed, and
        // at the engine's 15bb starting stacks the played call can
        // occasionally end the tournament outright (a knockout) before this
        // exchange completes — the server then finalizes and closes the
        // socket. That's a real, legitimate outcome, not a bug, just not the
        // scenario this test is about, so retry with a fresh table instead
        // of treating it as a failure.
        let mut retries = 0;
        let mut stream = loop {
            // Free any pre-existing active row so the claim starts fresh.
            sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
                .execute(&pool)
                .await
                .unwrap();

            let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();

            let initial = parse(&next_text(&mut stream).await);
            assert_eq!(initial["type"], "TABLE_STATE_UPDATE");
            let ranges = parse(&next_text(&mut stream).await);
            assert_eq!(ranges["type"], "RANGE_TABLES_UPDATE");
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
            let concluded_early = loop {
                let Some(text) = next_text_or_closed(&mut stream).await else {
                    break true;
                };
                let frame = parse(&text);
                match frame["type"].as_str().unwrap() {
                    "CHART_SNAPSHOT" => {
                        refreshed = true;
                        assert!(
                            !frame["points"].as_array().unwrap().is_empty(),
                            "the snapshot covers at least the played action"
                        );
                        if state_seen {
                            break false;
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
                            break false;
                        }
                    }
                    "TOURNAMENT_FINISHED" => break true,
                    _ => {}
                }
            };
            if concluded_early {
                retries += 1;
                assert!(
                    retries < 20,
                    "20 straight tournaments ended before this exchange completed"
                );
                continue;
            }
            assert!(
                refreshed,
                "the snapshot refreshes once the interval elapses"
            );
            assert!(state_seen, "the table state followed the played action");
            break stream;
        };

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
                "TABLE_STATE_UPDATE" | "CHART_SNAPSHOT" | "SEARCH_STATUS" | "RANGE_TABLES_UPDATE" => {
                    continue;
                }
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

        let (finalized, result, active_rows): (Option<String>, Option<String>, i64) =
            sqlx::query_as(
                "SELECT (SELECT session_end::text FROM hero_sessions WHERE id = $1 AND session_end IS NOT NULL),
                        (SELECT s.result FROM hero_sessions s WHERE id = $1),
                        (SELECT count(*) FROM active_tournament)",
            )
            .bind(recorded_session)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            finalized.is_some(),
            "finishing the table finalizes the analytics session"
        );
        assert_eq!(
            result.as_deref(),
            Some("LOSS"),
            "finishing the table gives up: the tournament is recorded as a loss"
        );
        assert_eq!(
            active_rows, 0,
            "finishing releases the active row so a new tournament can start"
        );

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(recorded_session)
            .execute(&pool)
            .await
            .unwrap();
    }

    /// Closing the tab leaves the tournament open; reconnecting resumes the
    /// very same hand/decision it paused on.
    #[tokio::test]
    async fn reconnect_resumes_the_tournament_where_it_left_off() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;

        let (address, _server) = spawn_server_with(Some(pool.clone()), 100, 0).await;

        // Every fresh tournament is dealt from a genuinely random seed
        // (`rand::random`, not a fixed test seed — see the new-tournament
        // branch in this file), and at the engine's 15bb starting stacks a
        // hand can occasionally knock someone out (hero included) before the
        // hero ever sees a second decision. That's a real, legitimate
        // outcome, not a bug — just not the scenario this test is about —
        // so retry with a fresh table rather than asserting it can't happen.
        let mut retries = 0;
        let (mut stream, decision, hand_marker) = loop {
            sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
                .execute(&pool)
                .await
                .unwrap();

            // First visit: a fresh table, hand #1, hero to act.
            let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
            let initial = parse(&next_text(&mut stream).await);
            assert_eq!(initial["type"], "TABLE_STATE_UPDATE");
            assert!(initial["fragment"].as_str().unwrap().contains("Hand #1"));

            // Play one action and wait until the hero must decide again.
            stream
                .send(TMessage::Text(
                    r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#.into(),
                ))
                .await
                .unwrap();
            let decision = loop {
                let frame = parse(&next_text(&mut stream).await);
                match frame["type"].as_str().unwrap() {
                    "TRIGGER_TACTICAL_OVERLAY" => {
                        stream
                            .send(TMessage::Text(r#"{"type":"REVIEW_DONE"}"#.into()))
                            .await
                            .unwrap();
                    }
                    "TABLE_STATE_UPDATE" => {
                        let fragment = frame["fragment"].as_str().unwrap();
                        // Hand #1 can walk to showdown on checks alone after
                        // the hero's one call, with no further hero decision
                        // — the wait then spills into hand #2 (or beyond)
                        // before a decision block appears again. Any
                        // non-decision frame just means it still isn't the
                        // hero's turn; the surrounding `next_text` timeout is
                        // the backstop against a genuine hang.
                        let Some(datadecision) = fragment
                            .split(r#"class="pt-action-block" data-decision=""#)
                            .nth(1)
                            .and_then(|rest| rest.split('"').next())
                        else {
                            continue;
                        };
                        break Some(datadecision.to_string());
                    }
                    "TOURNAMENT_FINISHED" => break None,
                    "CHART_TICK" | "SEARCH_STATUS" | "CHART_SNAPSHOT" | "RANGE_TABLES_UPDATE" => {}
                    other => panic!("unexpected frame type {other}"),
                }
            };
            let Some(decision) = decision else {
                retries += 1;
                assert!(
                    retries < 20,
                    "20 straight tournaments ended before a second hero decision"
                );
                continue;
            };
            // The hand the captured decision belongs to — usually still hand
            // #1, but a walked hand (every street checks through after the
            // hero's call) can leave zero further hero decisions in hand #1,
            // so the first decision found here is already hand #2's. Either
            // way the resume below must land on this same hand and token.
            let hand_marker = format!(
                "Hand #{}",
                decision
                    .strip_prefix('h')
                    .and_then(|rest| rest.split('-').next())
                    .expect("decision token starts with the hand number")
            );
            break (stream, decision, hand_marker);
        };
        stream.close(None).await.unwrap();

        // Wait for the server to release the table, then reconnect.
        let mut attempts = 0;
        loop {
            let connected: bool = sqlx::query_scalar("SELECT connected FROM active_tournament")
                .fetch_one(&pool)
                .await
                .unwrap();
            if !connected {
                break;
            }
            attempts += 1;
            assert!(attempts < 100, "the table was never released");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();

        let resumed_frame = parse(&next_text(&mut stream).await);
        assert_eq!(resumed_frame["type"], "TABLE_STATE_UPDATE");
        let fragment = resumed_frame["fragment"].as_str().unwrap();
        assert!(
            fragment.contains(&hand_marker),
            "the resume lands on the saved hand, not a fresh deal: {fragment}"
        );
        let resumed_decision = fragment
            .split(r#"class="pt-action-block" data-decision=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the resumed hero decision renders the action dock");
        assert_eq!(
            resumed_decision, decision,
            "the pause resumes the same decision token"
        );

        // Give up so the row and session are cleaned up.
        stream
            .send(TMessage::Text(r#"{"type":"FINISH_TABLE"}"#.into()))
            .await
            .unwrap();
        let finished = loop {
            let frame = parse(&next_text(&mut stream).await);
            match frame["type"].as_str().unwrap() {
                "SESSION_FINISHED" => break frame,
                "TABLE_STATE_UPDATE" | "SEARCH_STATUS" | "CHART_SNAPSHOT" | "RANGE_TABLES_UPDATE" => {
                    continue;
                }
                other => panic!("unexpected frame type {other} while finishing"),
            }
        };
        assert_eq!(finished["type"], "SESSION_FINISHED");
        drop(stream);

        let session_id: Option<i32> =
            sqlx::query_scalar("SELECT max(id) FROM hero_sessions WHERE result = 'LOSS'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let Some(session_id) = session_id else {
            panic!("the gave-up tournament was not recorded");
        };
        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
            .execute(&pool)
            .await
            .unwrap();
    }

    /// Two tabs cannot drive the same table: the second connection is
    /// pointed back at the dashboard while the first keeps playing.
    #[tokio::test]
    async fn a_second_connection_is_sent_back_to_the_dashboard() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
            .execute(&pool)
            .await
            .unwrap();
        let sessions_before: Option<i32> = sqlx::query_scalar("SELECT max(id) FROM hero_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();

        let (address, _server) = spawn_server_with(Some(pool.clone()), 100, 0).await;

        let (mut first, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let initial = parse(&next_text(&mut first).await);
        assert_eq!(initial["type"], "TABLE_STATE_UPDATE");

        // The table is claimed: a second window is rejected.
        let (mut second, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let frame = parse(&next_text(&mut second).await);
        assert_eq!(
            frame,
            json!({"type": "SESSION_FINISHED", "url": DASHBOARD_URL})
        );
        drop(second);

        // The first connection keeps working: finish to clean up.
        first
            .send(TMessage::Text(r#"{"type":"FINISH_TABLE"}"#.into()))
            .await
            .unwrap();
        let finished = loop {
            let frame = parse(&next_text(&mut first).await);
            match frame["type"].as_str().unwrap() {
                "SESSION_FINISHED" => break frame,
                "TABLE_STATE_UPDATE" | "SEARCH_STATUS" | "CHART_SNAPSHOT" | "RANGE_TABLES_UPDATE" => {
                    continue;
                }
                other => panic!("unexpected frame type {other} while finishing"),
            }
        };
        assert_eq!(finished["type"], "SESSION_FINISHED");
        drop(first);

        sqlx::query("DELETE FROM hero_sessions WHERE id > $1")
            .bind(sessions_before.unwrap_or(0))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
            .execute(&pool)
            .await
            .unwrap();
    }

    /// A stored snapshot that no longer parses must never be overwritten by
    /// a fresh table: the connection is rejected and the data stays put.
    #[tokio::test]
    async fn corrupted_snapshots_reject_the_connection() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = crate::db::test_pool().await;
        sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
            .execute(&pool)
            .await
            .unwrap();

        let session_id = match crate::live::claim_or_resume(&pool).await.unwrap() {
            crate::live::ClaimOutcome::Fresh(id) => id,
            other => panic!("expected a fresh claim, got {other:?}"),
        };
        sqlx::query("UPDATE active_tournament SET snapshot = $1::jsonb WHERE single = TRUE")
            .bind(r#"{"state": {"stacks": "corrupt"}}"#)
            .execute(&pool)
            .await
            .unwrap();
        crate::live::mark_disconnected(&pool).await.unwrap();

        let (address, _server) = spawn_server_with(Some(pool.clone()), 100, 0).await;
        let (mut stream, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let frame = parse(&next_text(&mut stream).await);
        assert_eq!(
            frame,
            json!({"type": "SESSION_FINISHED", "url": DASHBOARD_URL})
        );
        drop(stream);

        // The stored snapshot is untouched — a resume stays possible once
        // it is repaired.
        let stored: Option<String> =
            sqlx::query_scalar("SELECT snapshot::text FROM active_tournament")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(r#"{"state": {"stacks": "corrupt"}}"#),
            "the corrupted snapshot is left for inspection"
        );

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM active_tournament WHERE single = TRUE")
            .execute(&pool)
            .await
            .unwrap();
    }
}
