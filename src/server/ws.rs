use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;

use crate::analytics;
use crate::error::Result;
use crate::server::http::AppState;
use crate::server::protocol::{self, ClientMessage, ServerMessage};
use crate::server::session::{TableEvent, TableSession};
use crate::server::views;

/// Where the client is sent once it finishes the table (S9).
pub const TOURNAMENTS_URL: &str = "/tournaments";

/// The outcome of handling one client frame: the frames to send back, how
/// many chart ticks were produced (snapshot refresh pacing), and whether the
/// player finished the table.
pub struct FrameOutcome {
    pub messages: Vec<String>,
    pub chart_ticks: usize,
    pub finish_table: bool,
}

/// Upgrades `/ws` connections; each connection owns an isolated table session
/// seeded from OS entropy and, when the database is available, an analytics
/// session that stores every hero decision (S9).
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

    let (mut sender, mut receiver) = socket.split();

    let mut initial =
        vec![state_message(&mut session).unwrap_or_else(|error| error_message(&error.to_string()))];
    if let Some(snapshot) = snapshot_frame(app.pool.as_ref()).await {
        initial.push(snapshot);
    }
    for frame in initial {
        if sender.send(Message::Text(frame.into())).await.is_err() {
            close_session(app.pool.as_ref(), session_id).await;
            return;
        }
    }

    let mut ticks_since_snapshot = 0usize;
    loop {
        let message = match receiver.next().await {
            Some(message) => message,
            None => break,
        };
        let text = match message {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };

        let outcome = handle_client_message(&mut session, text.as_str());
        ticks_since_snapshot += outcome.chart_ticks;
        persist_records(app.pool.as_ref(), session_id, &mut session).await;

        for frame in outcome.messages {
            if sender.send(Message::Text(frame.into())).await.is_err() {
                close_session(app.pool.as_ref(), session_id).await;
                return;
            }
        }

        if outcome.finish_table {
            close_session(app.pool.as_ref(), session_id).await;
            if let Some(frame) = session_finished_message()
                && sender.send(Message::Text(frame.into())).await.is_err()
            {
                return;
            }
            return;
        }

        if ticks_since_snapshot >= app.snapshot_interval.max(1) {
            ticks_since_snapshot = 0;
            if let Some(frame) = snapshot_frame(app.pool.as_ref()).await
                && sender.send(Message::Text(frame.into())).await.is_err()
            {
                close_session(app.pool.as_ref(), session_id).await;
                return;
            }
        }
    }
    close_session(app.pool.as_ref(), session_id).await;
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

/// The decimated chart dataset mapping the last 1,000 stored actions (S9);
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

/// Handles one client text frame; never fails the connection — problems are
/// reported back as [`ServerMessage::Error`] frames.
fn handle_client_message(session: &mut TableSession, text: &str) -> FrameOutcome {
    let client_message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return FrameOutcome {
                messages: vec![error_message(&format!("malformed message: {error}"))],
                chart_ticks: 0,
                finish_table: false,
            };
        }
    };

    match client_message {
        ClientMessage::ActionSubmit { action } => {
            match protocol::resolve_action(&action, session.state()) {
                Ok(resolved) => match session.submit(resolved) {
                    Ok(events) => outcome(session, events),
                    Err(error) => error_outcome(&error.to_string()),
                },
                Err(error) => error_outcome(&error.to_string()),
            }
        }
        ClientMessage::ReviewDone => match session.confirm_review() {
            Ok(events) => outcome(session, events),
            Err(error) => error_outcome(&error.to_string()),
        },
        ClientMessage::FinishTable => FrameOutcome {
            messages: Vec::new(),
            chart_ticks: 0,
            finish_table: true,
        },
    }
}

fn outcome(session: &mut TableSession, events: Vec<TableEvent>) -> FrameOutcome {
    let chart_ticks = events
        .iter()
        .filter(|event| matches!(event, TableEvent::ChartTick { .. }))
        .count();
    FrameOutcome {
        messages: events_to_messages(session, events),
        chart_ticks,
        finish_table: false,
    }
}

fn error_outcome(message: &str) -> FrameOutcome {
    FrameOutcome {
        messages: vec![error_message(message)],
        chart_ticks: 0,
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
                fragment: views::tactical_overlay_fragment(hand_no, &decision, intercepted),
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
    use crate::decision::{Analysis, AnalyzedDecision, PlayedEvaluation, SurvivalConfig};
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
        let outcome = handle_client_message(&mut session, "{not json");
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
                ev_loss: 0.0,
                is_optimal: true,
            }),
        }
    }

    #[test]
    fn finish_table_yields_the_session_finished_frame() {
        let mut session = make_session();
        let outcome = handle_client_message(&mut session, r#"{"type":"FINISH_TABLE"}"#);
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
    fn review_done_without_a_pending_interception_is_an_error() {
        let mut session = make_session();
        let outcome = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#);
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
    fn review_done_applies_a_held_back_interception() {
        let mut session = make_session();
        let legal = session.state().legal_actions();
        let action = if legal.can_check {
            Action::Check
        } else {
            Action::Fold
        };
        session.stage_pending_interception(action, sample_analysis());

        let outcome = handle_client_message(&mut session, r#"{"type":"REVIEW_DONE"}"#);
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

    #[tokio::test]
    async fn snapshot_frame_without_a_pool_is_none() {
        assert_eq!(snapshot_frame(None).await, None);
    }

    async fn spawn_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        spawn_server_with(None, 100).await
    }

    async fn spawn_server_with(
        pool: Option<PgPool>,
        snapshot_interval: usize,
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
                        "S8 overlays are always intercepted"
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
                other => panic!("unexpected frame type {other}"),
            }
        }
        assert!(chart_tick, "every submitted action is charted");

        for payload in [
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"zzz"}}"#,
            "not json at all",
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"raise"}}"#,
        ] {
            stream.send(TMessage::Text(payload.into())).await.unwrap();
            let frame = parse(&next_text(&mut stream).await);
            assert_eq!(
                frame["type"], "ERROR",
                "payload {payload} should be rejected"
            );
        }

        stream
            .send(TMessage::Binary(vec![1, 2, 3].into()))
            .await
            .unwrap();
        // Any response after the binary frame proves the server kept playing.
        stream
            .send(TMessage::Text(
                r#"{"type":"ACTION_SUBMIT","action":{"kind":"zzz"}}"#.into(),
            ))
            .await
            .unwrap();
        let frame = parse(&next_text(&mut stream).await);
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
        let frame = parse(&next_text(&mut stream).await);
        assert_eq!(
            frame,
            json!({"type": "SESSION_FINISHED", "url": TOURNAMENTS_URL})
        );
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

        let (address, _server) = spawn_server_with(Some(pool.clone()), 1).await;
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
        let frame = parse(&next_text(&mut stream).await);
        assert_eq!(frame["type"], "SESSION_FINISHED");
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
