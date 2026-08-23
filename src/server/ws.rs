use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};

use crate::error::{Error, Result};
use crate::server::http::AppState;
use crate::server::protocol::{self, ClientMessage, ServerMessage};
use crate::server::session::{TableEvent, TableSession};
use crate::server::views;

/// Upgrades `/ws` connections; each connection owns an isolated table session
/// seeded from OS entropy.
pub async fn handler(State(app): State<Arc<AppState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

async fn handle_socket(socket: WebSocket, app: Arc<AppState>) {
    let mut session = TableSession::new(rand::random::<u64>(), app.mcts, app.survival);
    if let Err(error) = bootstrap(&mut session) {
        tracing::warn!(%error, "table session bootstrap failed; closing connection");
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    let initial = state_message(&session).unwrap_or_else(|error| error_message(&error.to_string()));
    if sender.send(Message::Text(initial.into())).await.is_err() {
        return;
    }

    while let Some(message) = receiver.next().await {
        match message {
            Ok(Message::Text(text)) => {
                for outgoing in handle_client_message(&mut session, text.as_str()) {
                    if sender.send(Message::Text(outgoing.into())).await.is_err() {
                        return;
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Deals the first hand and drives opponents until the hero must act.
fn bootstrap(session: &mut TableSession) -> Result<()> {
    session.deal_next_hand()?;
    session.pump()?;
    Ok(())
}

fn state_message(session: &TableSession) -> Result<String> {
    ServerMessage::TableStateUpdate {
        fragment: views::table_fragment(session.state(), session.hand_no(), session.log()),
    }
    .to_json()
}

/// Handles one client text frame; never fails the connection — problems are
/// reported back as [`ServerMessage::Error`] frames.
fn handle_client_message(session: &mut TableSession, text: &str) -> Vec<String> {
    let client_message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return vec![error_message(&format!("malformed message: {error}"))];
        }
    };

    match client_message {
        ClientMessage::ActionSubmit { action } => {
            match protocol::resolve_action(&action, session.state()) {
                Ok(resolved) => match session.submit(resolved) {
                    Ok(events) => events_to_messages(session, events),
                    Err(error) => vec![error_message(&error.to_string())],
                },
                Err(error) => vec![error_message(&error.to_string())],
            }
        }
    }
}

fn events_to_messages(session: &TableSession, events: Vec<TableEvent>) -> Vec<String> {
    let mut messages = Vec::with_capacity(events.len());
    for event in events {
        let serialized = match event {
            TableEvent::State => ServerMessage::TableStateUpdate {
                fragment: views::table_fragment(session.state(), session.hand_no(), session.log()),
            }
            .to_json(),
            TableEvent::TacticalOverlay { decision, hand_no } => {
                ServerMessage::TriggerTacticalOverlay {
                    fragment: views::tactical_overlay_fragment(hand_no, &decision),
                }
                .to_json()
            }
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
        Err(Error::Json(_)) => r#"{"type":"ERROR","message":"serialization failure"}"#.to_string(),
        Err(other) => format!(
            r#"{{"type":"ERROR","message":"{}"}}"#,
            other.to_string().replace('\\', "\\\\").replace('"', "\\\"")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut session = TableSession::new(81, MctsConfig::test(), SurvivalConfig::default());
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
        let messages = handle_client_message(&mut session, "{not json");
        assert_eq!(messages.len(), 1);
        let json = parse(&messages[0]);
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
        let messages = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"zzz"}}"#,
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(parse(&messages[0])["type"], "ERROR");
        assert_eq!(session.hand_no(), hand_no);
        assert_eq!(session.state().to_act(), Seat::Hero);
    }

    #[test]
    fn illegal_submissions_are_rejected_over_the_same_frame() {
        let mut session = make_session();
        let messages = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"check"}}"#,
        );
        assert_eq!(messages.len(), 1);
        let json = parse(&messages[0]);
        assert_eq!(json["type"], "ERROR");
        assert!(json["message"].as_str().unwrap().contains("not available"));
    }

    #[test]
    fn valid_submissions_run_the_full_pipeline() {
        let mut session = make_session();
        let messages = handle_client_message(
            &mut session,
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#,
        );
        assert!(!messages.is_empty());
        let types: Vec<String> = messages
            .iter()
            .map(|m| parse(m)["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.iter().any(|t| t == "CHART_TICK"));
        assert!(types.iter().any(|t| t == "TABLE_STATE_UPDATE"));
        assert_eq!(
            parse(messages.last().unwrap())["type"],
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
    fn events_map_to_the_three_server_frames() {
        let session = make_session();
        let events = vec![
            TableEvent::TacticalOverlay {
                decision: sample_analysis(),
                hand_no: 1,
            },
            TableEvent::ChartTick {
                action_index: 7,
                ev_loss: 2.0,
            },
            TableEvent::State,
        ];
        let messages = events_to_messages(&session, events);
        assert_eq!(messages.len(), 3);
        let overlay = parse(&messages[0]);
        assert_eq!(overlay["type"], "TRIGGER_TACTICAL_OVERLAY");
        assert!(
            overlay["fragment"]
                .as_str()
                .unwrap()
                .contains("Decision review")
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
        let session = make_session();
        let message = state_message(&session).unwrap();
        assert!(message.contains("\"type\":\"TABLE_STATE_UPDATE\""));
        assert!(message.contains("Hand #1"));
    }

    async fn spawn_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = ServeListener::new(
            listener,
            Arc::new(AppState {
                assets: default_assets(),
                mcts: MctsConfig::test(),
                survival: SurvivalConfig::default(),
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
                    assert!(
                        frame["fragment"]
                            .as_str()
                            .unwrap()
                            .contains("Decision review")
                    );
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
}
