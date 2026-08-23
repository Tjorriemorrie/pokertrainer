use serde::{Deserialize, Serialize};

use crate::analytics::ChartPoint;
use crate::error::{Error, Result};
use crate::game::{Action, GameState};
use crate::range::BetSize;

/// A player-submitted action: the played action type plus either an exact
/// amount or a bet-size bucket name (resolved server-side against the legal
/// action set).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SubmittedAction {
    /// `fold`, `check`, `call`, `bet`, `raise`, or `all_in`.
    pub kind: String,
    /// Exact "raise to" amount from the bet slider; ignored for non-betting
    /// kinds and mutually exclusive with `bucket`.
    #[serde(default)]
    pub amount: Option<u32>,
    /// A [`BetSize`] bucket name (e.g. `"1/2pot"`); resolved via
    /// [`BetSize::to_raise_to`] and the current legal-action bounds.
    #[serde(default)]
    pub bucket: Option<String>,
}

/// Messages sent from the client to the server over the WebSocket.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientMessage {
    ActionSubmit {
        action: SubmittedAction,
    },
    /// S8: the player has reviewed an intercepted blunder and wants the held
    /// back action applied.
    ReviewDone,
    /// S9: the player is done with the table — the session is finalized and
    /// the server replies with [`ServerMessage::SessionFinished`].
    FinishTable,
}

/// Messages sent from the server to the client over the WebSocket.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServerMessage {
    /// A raw table-state HTML fragment to swap into the DOM.
    TableStateUpdate { fragment: String },
    /// A full tactical-breakdown fragment to overlay the table.
    TriggerTacticalOverlay {
        fragment: String,
        /// S8: whether the state transition was halted — the client must
        /// send `REVIEW_DONE` before the action is applied.
        intercepted: bool,
    },
    /// One evaluated action: global action index and the EV lost against the
    /// optimal action.
    ChartTick { action_index: u64, ev_loss: f64 },
    /// S9: a decimated dataset (100 points mapping the chart window) sent on
    /// connect and periodically, so the client renders the stored history
    /// instantly instead of replaying every tick.
    ChartSnapshot { points: Vec<ChartPoint> },
    /// S9: the table was finished (`FINISH_TABLE`); the client navigates to
    /// the given page.
    SessionFinished { url: String },
    /// A rejected submission; the connection stays open.
    Error { message: String },
}

impl ServerMessage {
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

/// Resolves a submitted wire action into a concrete [`Action`] against the
/// current game state. Bucket strings are mapped to chip amounts with
/// [`BetSize::to_raise_to`], then every candidate is re-checked against the
/// legal-action set before it is accepted.
pub fn resolve_action(submitted: &SubmittedAction, state: &GameState) -> Result<Action> {
    let legal = state.legal_actions();
    let kind = submitted.kind.trim().to_lowercase();

    let action = match kind.as_str() {
        "fold" => require(
            Action::Fold,
            legal.allows(Action::Fold),
            "fold is not available",
        )?,
        "check" => require(
            Action::Check,
            legal.allows(Action::Check),
            "check is not available",
        )?,
        "call" => require(
            Action::Call,
            legal.allows(Action::Call),
            "call is not available",
        )?,
        "all_in" => require(
            Action::AllIn,
            legal.allows(Action::AllIn),
            "all-in is not available",
        )?,
        "bet" | "raise" => resolve_sized(submitted, state, &kind)?,
        other => return Err(Error::Decision(format!("unknown action kind {other:?}"))),
    };
    Ok(action)
}

fn require(action: Action, allowed: bool, why: &str) -> Result<Action> {
    if allowed {
        Ok(action)
    } else {
        Err(Error::Decision(why.to_string()))
    }
}

fn resolve_sized(submitted: &SubmittedAction, state: &GameState, kind: &str) -> Result<Action> {
    let legal = state.legal_actions();
    let betting = kind == "bet";
    let to_call = if betting { 0 } else { legal.call_amount };
    let (min, max) = if betting {
        (legal.min_bet, legal.max_bet)
    } else {
        (legal.min_raise_to, legal.max_raise_to)
    };
    let stack = state.stack(state.to_act());

    let amount = match submitted.amount {
        Some(amount) => amount,
        None => {
            let bucket = submitted
                .bucket
                .as_deref()
                .ok_or_else(|| Error::Decision(format!("{kind} requires an amount or a bucket")))?;
            parse_bucket(bucket)?.to_raise_to(
                state.total_pot(),
                to_call,
                state.blind_level().big_blind,
                min,
                stack,
            )
        }
    };

    if amount < min || amount > max {
        return Err(Error::Decision(format!(
            "{kind} to {amount} is outside the legal range {min}..={max}"
        )));
    }
    if amount >= stack {
        return require(
            Action::AllIn,
            legal.allows(Action::AllIn),
            "all-in is not available",
        );
    }
    match (betting, legal.can_bet, legal.can_raise) {
        (true, true, _) => Ok(Action::Bet(amount)),
        (false, _, true) => Ok(Action::Raise(amount)),
        _ => Err(Error::Decision(format!("{kind} is not available"))),
    }
}

/// Parses a bet-size bucket name (case- and separator-insensitive).
pub fn parse_bucket(name: &str) -> Result<BetSize> {
    match name.trim().to_lowercase().as_str() {
        "min" => Ok(BetSize::Min),
        "3bb" => Ok(BetSize::ThreeBb),
        "4bb" => Ok(BetSize::FourBb),
        "1/3pot" | "third" => Ok(BetSize::ThirdPot),
        "1/2pot" | "half" => Ok(BetSize::HalfPot),
        "3/4pot" | "threequarter" => Ok(BetSize::ThreeQuarterPot),
        "pot" => Ok(BetSize::Pot),
        "overbet" => Ok(BetSize::Overbet),
        "all_in" | "allin" => Ok(BetSize::AllIn),
        other => Err(Error::Decision(format!(
            "unknown bet-size bucket {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::game::Seat;
    use crate::game::blinds::BlindLevel;
    use crate::rng::seeded_rng;
    use serde_json::{Value, json};

    /// A preflop state where the hero (button) faces the big blind: Opponent 2
    /// has called, so the pot is 50, the call is 20, and the min raise-to is 40.
    fn hero_facing_bet() -> GameState {
        let mut state = GameState::new(Seat::Hero, BlindLevel::new(10, 20));
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(21)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    fn submitted(kind: &str) -> SubmittedAction {
        SubmittedAction {
            kind: kind.to_string(),
            amount: None,
            bucket: None,
        }
    }

    #[test]
    fn parses_plain_kinds() {
        let state = hero_facing_bet();
        assert_eq!(
            resolve_action(&submitted("fold"), &state).unwrap(),
            Action::Fold
        );
        assert_eq!(
            resolve_action(&submitted("call"), &state).unwrap(),
            Action::Call
        );
        assert_eq!(
            resolve_action(&submitted("all_in"), &state).unwrap(),
            Action::AllIn
        );
        assert_eq!(
            resolve_action(
                &SubmittedAction {
                    kind: "  CALL ".to_string(),
                    ..submitted("call")
                },
                &state
            )
            .unwrap(),
            Action::Call
        );
    }

    #[test]
    fn rejects_unavailable_actions() {
        let state = hero_facing_bet();
        assert!(matches!(
            resolve_action(&submitted("check"), &state),
            Err(Error::Decision(_))
        ));
        assert!(matches!(
            resolve_action(&submitted("zzz"), &state),
            Err(Error::Decision(_))
        ));
    }

    #[test]
    fn resolves_buckets_to_concrete_amounts() {
        let state = hero_facing_bet();
        let three_bb = SubmittedAction {
            kind: "raise".into(),
            bucket: Some("3bb".into()),
            amount: None,
        };
        assert_eq!(
            resolve_action(&three_bb, &state).unwrap(),
            Action::Raise(60)
        );

        let pot = SubmittedAction {
            kind: "raise".into(),
            bucket: Some("pot".into()),
            amount: None,
        };
        assert_eq!(resolve_action(&pot, &state).unwrap(), Action::Raise(70));
    }

    #[test]
    fn resolves_exact_slider_amounts_and_bounds() {
        let state = hero_facing_bet();
        let raise_100 = SubmittedAction {
            kind: "raise".into(),
            amount: Some(100),
            bucket: None,
        };
        assert_eq!(
            resolve_action(&raise_100, &state).unwrap(),
            Action::Raise(100)
        );

        for bad in [39, 501] {
            let amount = SubmittedAction {
                kind: "raise".into(),
                amount: Some(bad),
                bucket: None,
            };
            assert!(matches!(
                resolve_action(&amount, &state),
                Err(Error::Decision(_))
            ));
        }

        let missing = SubmittedAction {
            kind: "raise".into(),
            amount: None,
            bucket: None,
        };
        assert!(matches!(
            resolve_action(&missing, &state),
            Err(Error::Decision(_))
        ));
    }

    #[test]
    fn amounts_covering_the_stack_become_all_in() {
        let state = hero_facing_bet();
        let shove = SubmittedAction {
            kind: "raise".into(),
            amount: Some(490),
            bucket: None,
        };
        assert_eq!(resolve_action(&shove, &state).unwrap(), Action::AllIn);
    }

    #[test]
    fn parse_bucket_accepts_all_documented_names() {
        assert_eq!(parse_bucket("min").unwrap(), BetSize::Min);
        assert_eq!(parse_bucket("3bb").unwrap(), BetSize::ThreeBb);
        assert_eq!(parse_bucket("4BB").unwrap(), BetSize::FourBb);
        assert_eq!(parse_bucket("1/3pot").unwrap(), BetSize::ThirdPot);
        assert_eq!(parse_bucket("1/2pot").unwrap(), BetSize::HalfPot);
        assert_eq!(parse_bucket("3/4pot").unwrap(), BetSize::ThreeQuarterPot);
        assert_eq!(parse_bucket("pot").unwrap(), BetSize::Pot);
        assert_eq!(parse_bucket("overbet").unwrap(), BetSize::Overbet);
        assert_eq!(parse_bucket("all_in").unwrap(), BetSize::AllIn);
        assert!(matches!(parse_bucket("quantum"), Err(Error::Decision(_))));
    }

    #[test]
    fn client_messages_deserialize_from_the_wire_shape() {
        let msg: ClientMessage =
            serde_json::from_str(r#"{"type":"ACTION_SUBMIT","action":{"kind":"call"}}"#).unwrap();
        assert_eq!(
            msg,
            ClientMessage::ActionSubmit {
                action: submitted("call")
            }
        );

        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"bet","bucket":"1/2pot"}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ClientMessage::ActionSubmit {
                action: SubmittedAction {
                    kind: "bet".into(),
                    bucket: Some("1/2pot".into()),
                    amount: None,
                }
            }
        );

        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"ACTION_SUBMIT","action":{"kind":"raise","amount":150}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ClientMessage::ActionSubmit {
                action: SubmittedAction {
                    kind: "raise".into(),
                    bucket: None,
                    amount: Some(150),
                }
            }
        );

        assert!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"UNKNOWN_TAG"}"#).is_err(),
            "unknown message types must be rejected"
        );
        assert!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"ACTION_SUBMIT","action":{"kind":1}}"#
            )
            .is_err(),
            "wrong field types must be rejected"
        );
        let msg: ClientMessage = serde_json::from_str(r#"{"type":"REVIEW_DONE"}"#).unwrap();
        assert_eq!(msg, ClientMessage::ReviewDone);
        let msg: ClientMessage = serde_json::from_str(r#"{"type":"FINISH_TABLE"}"#).unwrap();
        assert_eq!(msg, ClientMessage::FinishTable);
    }

    #[test]
    fn server_messages_serialize_to_the_wire_shape() {
        assert_eq!(
            ServerMessage::TableStateUpdate {
                fragment: "<div>table</div>".into()
            }
            .to_json()
            .unwrap(),
            r#"{"type":"TABLE_STATE_UPDATE","fragment":"<div>table</div>"}"#
        );
        assert_eq!(
            ServerMessage::TriggerTacticalOverlay {
                fragment: "<div>overlay</div>".into(),
                intercepted: true
            }
            .to_json()
            .unwrap(),
            r#"{"type":"TRIGGER_TACTICAL_OVERLAY","fragment":"<div>overlay</div>","intercepted":true}"#
        );
        assert_eq!(
            ServerMessage::ChartTick {
                action_index: 12,
                ev_loss: 3.5
            }
            .to_json()
            .unwrap(),
            r#"{"type":"CHART_TICK","action_index":12,"ev_loss":3.5}"#
        );
        assert_eq!(
            ServerMessage::ChartSnapshot {
                points: vec![(1, 0.0), (10, 2.5)]
            }
            .to_json()
            .unwrap(),
            r#"{"type":"CHART_SNAPSHOT","points":[[1,0.0],[10,2.5]]}"#
        );
        assert_eq!(
            ServerMessage::SessionFinished {
                url: "/tournaments".into()
            }
            .to_json()
            .unwrap(),
            r#"{"type":"SESSION_FINISHED","url":"/tournaments"}"#
        );
        assert_eq!(
            ServerMessage::Error {
                message: "illegal action".into()
            }
            .to_json()
            .unwrap(),
            r#"{"type":"ERROR","message":"illegal action"}"#
        );
    }

    #[test]
    fn serialized_messages_carry_the_expected_type_tags() {
        let json: Value = serde_json::from_str(
            &ServerMessage::Error {
                message: "x".into(),
            }
            .to_json()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(json, json!({"type": "ERROR", "message": "x"}));
    }
}
