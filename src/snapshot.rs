//! Live-tournament snapshots: a serializable picture of one table session,
//! persisted so a tournament resumes exactly where it stopped — the street,
//! the bet amounts, the board, the deck order, and every seat's action state.
//!
//! The core game types (`GameState`, `Deck`, the opponent HUD counters) stay
//! free of serde derives; this module owns the JSON DTOs and the conversions
//! between them and the engine types (implemented on [`super::game::GameState`]
//! where its private fields are reachable).

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::game::{GameState, HandEndReason, PotAward, Seat, Street};

/// Seat indices on the wire (0: Hero, 1: Opponent 1, 2: Opponent 2).
fn seat_from_index(index: u8) -> Option<Seat> {
    match index {
        0 => Some(Seat::Hero),
        1 => Some(Seat::Opponent1),
        2 => Some(Seat::Opponent2),
        _ => None,
    }
}

fn street_from_index(index: u8) -> Option<Street> {
    match index {
        0 => Some(Street::Preflop),
        1 => Some(Street::Flop),
        2 => Some(Street::Turn),
        3 => Some(Street::River),
        _ => None,
    }
}

/// A denormalized view of one finished hand's result (winner and awards),
/// persisted purely so a table paused on the win ribbon resumes on the same
/// hand instead of dealing a fresh one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HandResultSnapshot {
    /// `"fold"` (with the winning seat index) or `"showdown"`.
    pub reason: String,
    /// `(seat_index, chips)` pairs, one per seat that won contested pot chips.
    pub awards: Vec<(u8, u32)>,
    /// `(seat_index, chips)` pairs of uncalled bet portions handed back —
    /// not wins. `#[serde(default)]` keeps snapshots from before this field
    /// existed loadable.
    #[serde(default)]
    pub returns: Vec<(u8, u32)>,
}

/// The wire form of a full [`GameState`]: every private field the engine
/// needs to reconstruct bets, ordering, and eligibility exactly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub stacks: [u32; 3],
    pub button: u8,
    pub blind_small: u32,
    pub blind_big: u32,
    pub street: u8,
    /// Board cards as codes, in dealt order.
    pub board: Vec<String>,
    /// Three seats × two card codes (opponents' hidden cards included — they
    /// are part of the live state and must survive a resume).
    pub hole_cards: Vec<[String; 2]>,
    pub revealed: [bool; 3],
    pub street_contrib: [u32; 3],
    pub total_contrib: [u32; 3],
    pub current_bet: u32,
    pub min_raise: u32,
    pub last_full_raise: Option<u8>,
    pub acted: [bool; 3],
    pub folded: [bool; 3],
    pub all_in: [bool; 3],
    pub eliminated: [bool; 3],
    pub to_act: u8,
    pub hand_over: bool,
    /// Present only when `hand_over`: enough to re-render the win ribbon.
    pub hand_result: Option<HandResultSnapshot>,
}

/// The live HUD counters backing the opponents panel — persist them so the
/// coach feedback does not reset on every reconnect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpponentCountersSnapshot {
    pub hands: [usize; 2],
    pub vpip: [usize; 2],
    pub pfr: [usize; 2],
    pub faced_bet: [usize; 2],
    pub folded_to_bet: [usize; 2],
    pub postflop_bets: [usize; 2],
    pub postflop_calls: [usize; 2],
    pub vpip_seen: [bool; 2],
    pub pfr_seen: [bool; 2],
}

/// Everything a resuming table needs: the game state, the remainder of the
/// deck in dealing order, the hand/action counters, the action log, the
/// opponent HUD counters, and the bot skill template the opponents play with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TournamentSnapshot {
    pub state: StateSnapshot,
    pub deck: Vec<String>,
    pub hand_no: u64,
    pub action_no: u64,
    pub log: Vec<String>,
    pub opponents: OpponentCountersSnapshot,
    /// The field skill template both bots use; `None` (old snapshots) keeps
    /// the plain placeholder policy.
    #[serde(default)]
    pub template_skill: Option<f64>,
}

impl TournamentSnapshot {
    /// Serializes to the JSON column value of `active_tournament.snapshot`.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Deserializes an `active_tournament.snapshot` column value.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }
}

/// The dashboard summary derived from a stored snapshot: what the resume
/// card shows before the player makes any decision.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSummary {
    pub session_id: i32,
    pub hand_no: u64,
    pub street: Street,
    pub blind_small: u32,
    pub blind_big: u32,
    pub hero_stack: u32,
    pub active_opponents: usize,
    pub actions: usize,
    /// Raw `hero_sessions.session_start` string.
    pub started: String,
}

/// The dashboard's complete view of the open tournament: the summary facts
/// plus the session id (the resume page does not need the id itself).
#[derive(Clone, Debug, PartialEq)]
pub struct DashboardActive {
    pub session_id: i32,
    pub summary: ActiveSummary,
}

impl ActiveSummary {
    /// Derives the dashboard summary from a parsed snapshot plus the session
    /// metadata the store layer already loaded.
    pub fn from_snapshot(
        session_id: i32,
        snapshot: &TournamentSnapshot,
        started: String,
        actions: usize,
    ) -> Option<Self> {
        let street = street_from_index(snapshot.state.street)?;
        let hero_index = Seat::Hero.index();
        let active_opponents = [Seat::Opponent1, Seat::Opponent2]
            .into_iter()
            .filter(|seat| !snapshot.state.eliminated[seat.index()])
            .count();
        Some(Self {
            session_id,
            hand_no: snapshot.hand_no,
            street,
            blind_small: snapshot.state.blind_small,
            blind_big: snapshot.state.blind_big,
            hero_stack: snapshot.state.stacks[hero_index],
            active_opponents,
            actions,
            started,
        })
    }
}

/// Rebuilds the hand result stored in a snapshot (fold winner or showdown
/// awards) so the resumed table renders the same win ribbon the player saw.
pub fn reconstruct_hand_result(
    state: &GameState,
    snapshot: &HandResultSnapshot,
) -> Result<crate::game::HandResult> {
    let reason = match snapshot.reason.as_str() {
        "fold" => {
            let Some(seat) = snapshot.reason_seat() else {
                return Err(Error::Game("fold hand result is missing its winner".into()));
            };
            HandEndReason::Fold(seat)
        }
        "showdown" => HandEndReason::Showdown,
        other => return Err(Error::Game(format!("unknown hand result reason {other:?}"))),
    };
    let awards = snapshot
        .awards
        .iter()
        .map(|(index, amount)| {
            seat_from_index(*index)
                .map(|seat| PotAward {
                    seat,
                    amount: *amount,
                })
                .ok_or_else(|| Error::Game(format!("invalid award seat index {index}")))
        })
        .collect::<Result<Vec<_>>>()?;
    let returns = snapshot
        .returns
        .iter()
        .map(|(index, amount)| {
            seat_from_index(*index)
                .map(|seat| PotAward {
                    seat,
                    amount: *amount,
                })
                .ok_or_else(|| Error::Game(format!("invalid return seat index {index}")))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(crate::game::HandResult {
        reason,
        awards,
        returns,
        pots: state.pots(),
        revealed: Vec::new(),
    })
}

impl HandResultSnapshot {
    /// The winning seat of a fold result: the first award's seat.
    fn reason_seat(&self) -> Option<Seat> {
        seat_from_index(self.awards.first()?.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_and_street_indices_map_both_ways() {
        for (index, seat) in [
            (0u8, Seat::Hero),
            (1, Seat::Opponent1),
            (2, Seat::Opponent2),
        ] {
            assert_eq!(seat_from_index(index), Some(seat));
        }
        assert_eq!(seat_from_index(3), None);
        assert_eq!(seat_from_index(u8::MAX), None);
    }

    #[test]
    fn snapshot_json_round_trips() {
        let snapshot = TournamentSnapshot {
            state: StateSnapshot {
                stacks: [500, 480, 500],
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
            hand_no: 4,
            action_no: 7,
            log: vec!["You raise to 60".into()],
            template_skill: None,
            opponents: OpponentCountersSnapshot {
                hands: [4, 4],
                vpip: [1, 2],
                pfr: [0, 1],
                faced_bet: [2, 1],
                folded_to_bet: [1, 0],
                postflop_bets: [1, 0],
                postflop_calls: [0, 2],
                vpip_seen: [false, false],
                pfr_seen: [false, false],
            },
        };
        let json = snapshot.to_json().unwrap();
        assert_eq!(TournamentSnapshot::from_json(&json).unwrap(), snapshot);
    }

    #[test]
    fn hand_result_snapshots_reconstruct_fold_wins() {
        let snapshot = HandResultSnapshot {
            reason: "fold".into(),
            awards: vec![(0, 30)],
            returns: Vec::new(),
        };
        assert_eq!(snapshot.reason_seat(), Some(Seat::Hero));
        let state = GameState::new(Seat::Hero, crate::game::blinds::BlindLevel::new(10, 20));
        let result = reconstruct_hand_result(&state, &snapshot).unwrap();
        assert_eq!(result.reason, HandEndReason::Fold(Seat::Hero));
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 30
            }]
        );
        assert!(result.returns.is_empty());
    }

    #[test]
    fn fold_snapshots_without_a_winner_are_rejected() {
        let bad = HandResultSnapshot {
            reason: "fold".into(),
            awards: Vec::new(),
            returns: Vec::new(),
        };
        assert_eq!(bad.reason_seat(), None);
        let state = GameState::new(Seat::Hero, crate::game::blinds::BlindLevel::new(10, 20));
        assert!(matches!(
            reconstruct_hand_result(&state, &bad),
            Err(Error::Game(_))
        ));
    }

    #[test]
    fn unknown_reasons_are_rejected() {
        let bad = HandResultSnapshot {
            reason: "photo finish".into(),
            awards: vec![(1, 10)],
            returns: Vec::new(),
        };
        let state = GameState::new(Seat::Hero, crate::game::blinds::BlindLevel::new(10, 20));
        assert!(matches!(
            reconstruct_hand_result(&state, &bad),
            Err(Error::Game(_))
        ));
    }

    #[test]
    fn showdown_snapshots_keep_uncalled_returns_apart_from_awards() {
        let snapshot = HandResultSnapshot {
            reason: "showdown".into(),
            awards: vec![(0, 410)],
            returns: vec![(2, 5)],
        };
        let state = GameState::new(Seat::Hero, crate::game::blinds::BlindLevel::new(10, 20));
        let result = reconstruct_hand_result(&state, &snapshot).unwrap();
        assert_eq!(result.reason, HandEndReason::Showdown);
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 410
            }]
        );
        assert_eq!(
            result.returns,
            vec![PotAward {
                seat: Seat::Opponent2,
                amount: 5
            }]
        );
    }

    #[test]
    fn old_hand_result_snapshots_without_returns_still_deserialize() {
        let json = r#"{"reason":"showdown","awards":[[0,300]]}"#;
        let snapshot: HandResultSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.returns, Vec::<(u8, u32)>::new());
        serde_json::to_string(&snapshot).unwrap();
    }

    #[test]
    fn old_snapshots_without_a_template_skill_still_deserialize() {
        let json = r#"{"state":{"stacks":[500,480,500],"button":0,"blind_small":10,"blind_big":20,"street":0,"board":[],"hole_cards":[["As","Kd"],["2c","2h"],["7s","8s"]],"revealed":[true,false,false],"street_contrib":[10,20,0],"total_contrib":[10,20,0],"current_bet":20,"min_raise":20,"last_full_raise":null,"acted":[false,false,false],"folded":[false,false,false],"all_in":[false,false,false],"eliminated":[false,false,false],"to_act":0,"hand_over":false,"hand_result":null},"deck":[],"hand_no":1,"action_no":0,"log":[],"opponents":{"hands":[1,1],"vpip":[0,0],"pfr":[0,0],"faced_bet":[0,0],"folded_to_bet":[0,0],"postflop_bets":[0,0],"postflop_calls":[0,0],"vpip_seen":[false,false],"pfr_seen":[false,false]}}"#;
        let snapshot: TournamentSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.template_skill, None);
        assert_eq!(snapshot.hand_no, 1);

        snapshot.to_json().unwrap();
    }

    #[test]
    fn template_skill_survives_the_json_round_trip() {
        let snapshot = TournamentSnapshot {
            state: StateSnapshot {
                stacks: [500, 480, 500],
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
            deck: Vec::new(),
            hand_no: 3,
            action_no: 0,
            log: Vec::new(),
            opponents: OpponentCountersSnapshot {
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
            template_skill: Some(0.62),
        };
        let json = snapshot.to_json().unwrap();
        let back = TournamentSnapshot::from_json(&json).unwrap();
        assert_eq!(back.template_skill, Some(0.62));

        // A corrupt `all_in` marker must not deserialize silently.
        let bad = json.replace(
            r#""all_in":[false,false,false]"#,
            r#""all_in":[false,false,"x"]"#,
        );
        assert!(TournamentSnapshot::from_json(&bad).is_err());
    }

    #[test]
    fn summaries_surface_the_dashboard_facts() {
        let snapshot = TournamentSnapshot {
            state: StateSnapshot {
                stacks: [490, 480, 500],
                button: 0,
                blind_small: 10,
                blind_big: 20,
                street: 1,
                board: vec!["2c".into(), "7h".into(), "Kd".into()],
                hole_cards: vec![
                    ["As".into(), "Kd".into()],
                    ["2c".into(), "2h".into()],
                    ["7s".into(), "8s".into()],
                ],
                revealed: [true, false, false],
                street_contrib: [0, 0, 0],
                total_contrib: [10, 10, 10],
                current_bet: 0,
                min_raise: 20,
                last_full_raise: None,
                acted: [false, false, false],
                folded: [false, false, false],
                all_in: [false, false, false],
                eliminated: [false, true, false],
                to_act: 0,
                hand_over: false,
                hand_result: None,
            },
            deck: Vec::new(),
            hand_no: 12,
            action_no: 9,
            log: Vec::new(),
            template_skill: None,
            opponents: OpponentCountersSnapshot {
                hands: [12, 12],
                vpip: [1, 2],
                pfr: [0, 1],
                faced_bet: [2, 1],
                folded_to_bet: [1, 0],
                postflop_bets: [1, 0],
                postflop_calls: [0, 2],
                vpip_seen: [false, false],
                pfr_seen: [false, false],
            },
        };
        let summary =
            ActiveSummary::from_snapshot(9, &snapshot, "2026-08-24T10:00:00Z".into(), 41).unwrap();
        assert_eq!(summary.session_id, 9);
        assert_eq!(summary.hand_no, 12);
        assert_eq!(summary.street, Street::Flop);
        assert_eq!((summary.blind_small, summary.blind_big), (10, 20));
        assert_eq!(summary.hero_stack, 490);
        assert_eq!(summary.active_opponents, 1);
        assert_eq!(summary.actions, 41);
        assert_eq!(summary.started, "2026-08-24T10:00:00Z");

        let mut bad = snapshot;
        bad.state.street = 9;
        assert!(ActiveSummary::from_snapshot(9, &bad, String::new(), 0).is_none());
    }
}
