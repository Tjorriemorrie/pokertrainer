//! Opponent-skill analysis: grades every opponent decision found in the
//! imported GGPoker hands against the MCTS solver, turns the pooled average
//! EV loss (in big blinds) into one field skill level, and persists the
//! per-hand results so re-runs only grade hands that were never analyzed.
//!
//! The pipeline has three parts:
//!
//! 1. **Walking** ([`walk_hand`]) rebuilds each hand's action timeline inside
//!    the game engine. Blind posts, streets, and boards are reproduced from
//!    the parsed episode, so every opponent decision point can be frozen as a
//!    real [`GameState`] with the acting seat rotated into the hero role.
//! 2. **Grading** ([`grade_points`]) solves each decision point from the
//!    opponent's perspective with [`crate::mcts::solve_for_seat`] and measures
//!    how many big blinds the played action gives up against the best one.
//! 3. **Aggregation & persistence** ([`run_job`], [`DrillTemplate`]) pools
//!    both opponents' decisions into one average and caches per-hand results
//!    in `gg_hand_analysis`, so analyzing twice costs nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rand::Rng;
use sqlx::PgPool;

use crate::card::{Card, Deck, Rank, Suit};
use crate::error::Result;
use crate::game::{Action, ActionOutcome, GameState, NUM_PLAYERS, Seat, Street};
use crate::hh::{Episode, EpisodeVerb};
use crate::mcts::{self, MctsConfig};
use crate::range::hands::Hand;
use crate::rng::seeded_rng;
use crate::snapshot::StateSnapshot;

/// How many of the most recent imported hands get walked and graded (and
/// cached) on each analysis run — a computational cap, not the reporting
/// window (see [`EV_ROLLING_WINDOW`]).
pub const ANALYSIS_WINDOW: i64 = 1000;

/// How many of the most recent graded *decisions* (not hands) feed the
/// pooled EV figures reported for the hero and for the opponent field —
/// walked newest-hand-first and stopped once the threshold is reached, so
/// the cutoff lands on a hand boundary rather than mid-hand. Kept separate
/// from [`ANALYSIS_WINDOW`], which bounds how many hands get solved at all.
pub const EV_ROLLING_WINDOW: i64 = 1000;

/// The average EV loss (in big blinds) that maps to skill zero. Graded from
/// the opponent's seat with a uniform prior (their exact cards are unknown),
/// the analyzed decisions measure looser than hero decisions, so the anchor
/// sits above the hero scale deliberately.
pub const SKILL_ZERO_LOSS_BB: f64 = 5.0;

/// Maps an average EV loss per decision (in big blinds) onto a 0..1 skill.
/// Losing zero big blinds is solver-perfect (skill 1); losing
/// [`SKILL_ZERO_LOSS_BB`] per decision is skill zero. Without any graded
/// decision the skill is 0.0 ("no data").
pub fn skill_from(ev_loss_sum: f64, decisions: u32) -> f64 {
    if decisions == 0 {
        return 0.0;
    }
    let avg = ev_loss_sum / f64::from(decisions);
    (1.0 - avg / SKILL_ZERO_LOSS_BB).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------- walking

/// The two decision streams one hand walk produces: every opponent decision
/// (graded against a uniform prior, since their cards are usually unknown)
/// and every decision the real hero made in that same hand (graded with the
/// hero's true known cards, since PokerCraft always records them).
#[derive(Default)]
pub struct WalkedHand {
    pub opponent: Vec<DecisionPoint>,
    pub hero: Vec<DecisionPoint>,
}

/// One frozen decision: the game state exactly as the hand stood before the
/// played action, re-labeled so the acting seat occupies the hero slot, plus
/// the pins the grader knows (the real hero's hole cards, when grading an
/// opponent's decision).
pub struct DecisionPoint {
    pub state: GameState,
    pub pins: [Option<[Card; 2]>; NUM_PLAYERS],
    pub played: Action,
    pub actor_name: String,
    /// The actor's real hand class, when it was revealed at showdown (`"...:
    /// shows [...]"`). Most decisions carry `None` — folds never reveal —
    /// which is expected and handled downstream by a minimum-sample
    /// fallback, not treated as an error.
    pub opponent_hand: Option<Hand>,
}

/// The hand class an actor showed at showdown, if their cards parse and they
/// showed at all — most decisions have no such reveal.
fn shown_hand(episode: &Episode, seat_no: u8) -> Option<Hand> {
    let (_, codes) = episode.shown.iter().find(|(no, _)| *no == seat_no)?;
    let a = Card::from_code(&codes[0])?;
    let b = Card::from_code(&codes[1])?;
    Some(Hand::from_cards(a, b))
}

/// The outcome of grading one imported hand.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HandScore {
    pub decisions: u32,
    pub ev_loss_bb_sum: f64,
    /// Per-actor aggregates over the graded decisions of this hand.
    pub players: Vec<(String, u32, f64)>,
    /// Non-fatal problems (unwalked/ungradable decisions) collected during
    /// the pass.
    pub problems: Vec<String>,
}

/// Walks a parsed episode through the engine state machine and freezes every
/// opponent decision point.
pub fn walk_hand(
    episode: &Episode,
    sb: i32,
    bb: i32,
    hero_cards: Option<[Card; 2]>,
) -> std::result::Result<WalkedHand, String> {
    if sb <= 0 || bb <= 0 {
        return Err(format!("nonsensical blinds {sb}/{bb}"));
    }

    // Seat numbers map to successively numbered slots in ascending order;
    // ascending seat numbers form the real acting cycle, so the engine's
    // clockwise rotation matches the table.
    let mut seat_nos: Vec<u8> = episode.seats.iter().map(|seat| seat.no).collect();
    seat_nos.sort_unstable();
    seat_nos.dedup();
    if seat_nos.len() > NUM_PLAYERS {
        return Err(format!(
            "hand lists {} seats, expected at most 3",
            seat_nos.len()
        ));
    }
    let slot_of = |no: u8| {
        seat_nos
            .iter()
            .position(|&candidate| candidate == no)
            .ok_or_else(|| format!("action by undeclared seat {no}"))
    };
    let hero_slot = slot_of(
        episode
            .seats
            .iter()
            .find(|seat| seat.name == "Hero")
            .ok_or_else(|| "hand has no hero seat".to_string())?
            .no,
    )?;

    // Initial stacks from the seat declarations.
    let mut stacks = [0u32; NUM_PLAYERS];
    let mut eliminated = [true; NUM_PLAYERS];
    for seat in &episode.seats {
        let slot = slot_of(seat.no)?;
        let stack = seat
            .stack
            .ok_or_else(|| format!("seat {} declares no stack", seat.no))?;
        if stack < 0 {
            return Err(format!("seat {} has a negative stack", seat.no));
        }
        stacks[slot] = stack as u32;
        eliminated[slot] = false;
    }

    let button_seat = match episode.button {
        Some(no) => no,
        None => {
            let first_post = episode
                .actions
                .iter()
                .find(|action| action.verb == EpisodeVerb::Post)
                .ok_or_else(|| "hand has no blind post to derive the button".to_string())?;
            first_post.seat_no
        }
    };
    let button_slot = slot_of(button_seat)?;

    // Blind/ante posts open the hand: fold their commitments into the initial
    // snapshot.
    let mut contribs = [0u32; NUM_PLAYERS];
    let mut current_bet = 0u32;
    let mut all_in = [false; NUM_PLAYERS];
    let mut first_actor: Option<usize> = None;
    for action in &episode.actions {
        if action.verb != EpisodeVerb::Post {
            first_actor.get_or_insert(slot_of(action.seat_no)?);
            continue;
        }
        let amount = action
            .amount
            .ok_or_else(|| "blind post without an amount".to_string())?;
        let slot = slot_of(action.seat_no)?;
        let posted = (amount as u32).min(stacks[slot]);
        stacks[slot] -= posted;
        contribs[slot] += posted;
        current_bet = current_bet.max(contribs[slot]);
        if action.all_in {
            all_in[slot] = true;
        }
    }
    let Some(first_actor) = first_actor else {
        return Ok(WalkedHand::default());
    };

    // Real board cards in engine deal order (no burns in the engine).
    let board_cards = real_board_cards(episode)?;
    let mut deck_cards = board_cards.clone();
    for suit in Suit::ALL {
        for rank in Rank::ALL {
            let card = Card::new(rank, suit);
            if !deck_cards.contains(&card) {
                deck_cards.push(card);
            }
        }
    }
    let mut deck = Deck::try_from_remaining(deck_cards)
        .ok_or_else(|| "reconstructed deck exceeds 52 cards".to_string())?;

    let hero_codes: [String; 2] = hero_cards.map_or_else(
        || ["2c".to_string(), "2d".to_string()],
        |cards| [cards[0].to_code(), cards[1].to_code()],
    );
    let hole_codes: Vec<[String; 2]> = (0..NUM_PLAYERS)
        .map(|slot| {
            if slot == hero_slot {
                hero_codes.clone()
            } else if slot == 0 {
                ["3c".to_string(), "3d".to_string()]
            } else {
                ["3h".to_string(), "3s".to_string()]
            }
        })
        .collect();
    // The grader replaces every seat's cards with sampled holdings; these
    // codes only need to parse.

    let snapshot = StateSnapshot {
        stacks,
        button: button_slot as u8,
        blind_small: sb as u32,
        blind_big: bb as u32,
        street: 0,
        board: Vec::new(),
        hole_cards: hole_codes,
        revealed: [hero_slot == 0, hero_slot == 1, hero_slot == 2],
        street_contrib: contribs,
        total_contrib: contribs,
        current_bet,
        min_raise: bb as u32,
        last_full_raise: None,
        acted: [false; NUM_PLAYERS],
        folded: [false; NUM_PLAYERS],
        all_in,
        eliminated,
        to_act: first_actor as u8,
        hand_over: false,
        hand_result: None,
    };
    let mut state = GameState::from_snapshot(&snapshot)
        .map_err(|error| format!("initial state does not rebuild: {error}"))?;

    let mut walked = WalkedHand::default();
    for (index, action) in episode.actions.iter().enumerate() {
        if action.verb == EpisodeVerb::Post {
            continue;
        }
        let slot = slot_of(action.seat_no)?;
        if state.is_hand_over() {
            return Err("hand over but actions remain".to_string());
        }
        if state.to_act().index() != slot {
            return Err(format!(
                "engine expects {} to act but seat {} acted",
                state.to_act(),
                action.seat_no
            ));
        }

        let stack = state.stack(Seat::ALL[slot]);
        let to_call = state.legal_actions().call_amount;
        let played = convert_action(
            state.street_contribution(Seat::ALL[slot]),
            stack,
            to_call,
            action,
        )?;
        if !state.legal_actions().allows(played) {
            return Err(format!(
                "played action {played:?} is illegal in the reconstructed state"
            ));
        }
        if matches!(played, Action::Call) {
            let expected_call = state.legal_actions().call_amount;
            let real_call = action.amount.unwrap_or(expected_call as i32) as u32;
            if real_call != expected_call {
                return Err(format!(
                    "engine call amount {expected_call} differs from the real {real_call}"
                ));
            }
        }

        if slot != hero_slot {
            // The hero's cards are known: pin them into the decision point,
            // following the rotation the same way `GameState::rotated` does.
            let mut pins = [None; NUM_PLAYERS];
            if let Some(cards) = hero_cards {
                let hero_rotated = (hero_slot + NUM_PLAYERS - slot) % NUM_PLAYERS;
                pins[hero_rotated] = Some(cards);
            }
            walked.opponent.push(DecisionPoint {
                state: state.rotated(Seat::ALL[slot]),
                pins,
                played,
                actor_name: episode
                    .seats
                    .iter()
                    .find(|seat| seat.no == action.seat_no)
                    .map(|seat| seat.name.clone())
                    .unwrap_or_default(),
                opponent_hand: shown_hand(episode, action.seat_no),
            });
        } else {
            // The hero's own decision: rotating by hero_slot moves their
            // already-revealed real cards into the seat-0 hero role, so no
            // pins are needed here — the state itself carries the truth.
            walked.hero.push(DecisionPoint {
                state: state.rotated(Seat::ALL[slot]),
                pins: [None; NUM_PLAYERS],
                played,
                actor_name: "Hero".to_string(),
                opponent_hand: None,
            });
        }

        let before_street = state.street();
        apply_settled(&mut state, &mut deck, played)?;
        if state.street() != before_street && !state.is_hand_over() {
            verify_board(&state, &board_cards)?;
            // The engine's positional street-entry order diverges from real
            // tables once players fold (e.g. a button fold leaves the big
            // blind first to act postflop). The real next action names the
            // true first actor, so force the rotation to match it.
            if let Some(next) = episode.actions[index + 1..]
                .iter()
                .find(|next| next.verb != EpisodeVerb::Post)
            {
                let next_slot = slot_of(next.seat_no)?;
                if state.to_act().index() != next_slot {
                    let mut snapshot = state.to_snapshot();
                    snapshot.to_act = next_slot as u8;
                    state = GameState::from_snapshot(&snapshot)
                        .map_err(|error| format!("actor fixup failed: {error}"))?;
                }
            }
        }
    }
    Ok(walked)
}

/// Converts a parsed action into the engine action space (`raises ... to` is
/// a raise-to unless nothing had to be called — the big blind's option reads
/// as a bet in the engine; all-in calls/bets/raises collapse into
/// [`Action::AllIn`]).
fn convert_action(
    contrib: u32,
    stack: u32,
    to_call: u32,
    action: &crate::hh::EpisodeAction,
) -> std::result::Result<Action, String> {
    let all_in_push = |amount: i32| -> bool { action.all_in && amount as u32 >= stack };
    match action.verb {
        EpisodeVerb::Fold => Ok(Action::Fold),
        EpisodeVerb::Check => {
            if to_call > 0 {
                return Err(format!(
                    "check faced a {to_call} call — real hand inconsistent"
                ));
            }
            Ok(Action::Check)
        }
        EpisodeVerb::Call => {
            let amount = action
                .amount
                .ok_or_else(|| "call without an amount".to_string())?;
            if all_in_push(amount) {
                Ok(Action::AllIn)
            } else {
                Ok(Action::Call)
            }
        }
        EpisodeVerb::Bet => {
            let amount = action
                .amount
                .ok_or_else(|| "bet without an amount".to_string())?;
            if all_in_push(amount) {
                Ok(Action::AllIn)
            } else {
                Ok(Action::Bet(amount as u32))
            }
        }
        EpisodeVerb::Raise => {
            let to = action
                .to
                .ok_or_else(|| "raise without a to amount".to_string())?;
            let commits = (to as u32).saturating_sub(contrib);
            if action.all_in && commits >= stack {
                Ok(Action::AllIn)
            } else if to_call == 0 {
                // The big blind's option: the engine models it as a bet.
                Ok(Action::Bet(to as u32))
            } else {
                Ok(Action::Raise(to as u32))
            }
        }
        EpisodeVerb::Post => Err("blind posts are folded into the initial state".to_string()),
    }
}

/// Applies an action and resolves street transitions exactly like the live
/// table does.
fn apply_settled(
    state: &mut GameState,
    deck: &mut Deck,
    action: Action,
) -> std::result::Result<(), String> {
    let outcome = state
        .apply_action(action)
        .map_err(|error| format!("replayed action {action:?} rejected: {error}"))?;
    if outcome == ActionOutcome::StreetEnded {
        if state.can_continue_betting() && !matches!(state.street(), Street::River) {
            state
                .advance_street(deck)
                .map_err(|error| format!("street advance failed: {error}"))?;
        } else if !state.is_hand_over() {
            state
                .showdown(deck)
                .map_err(|error| format!("showdown failed: {error}"))?;
        }
    }
    Ok(())
}

/// The real board in engine consumption order (three flop cards, then the
/// turn, then the river — the engine does not burn).
fn real_board_cards(episode: &Episode) -> std::result::Result<Vec<Card>, String> {
    let mut codes: Vec<String> = episode
        .boards
        .iter()
        .filter(|(_, cards)| !cards.is_empty())
        .max_by_key(|(street, _)| street)
        .map(|(_, cards)| cards.clone())
        .unwrap_or_default();
    if let Some(summary) = &episode.summary_board
        && summary.len() > codes.len()
    {
        codes = summary.clone();
    }
    if codes.len() > 5 {
        codes.truncate(5);
    }
    let board = codes
        .iter()
        .map(|code| {
            Card::from_code(code).ok_or_else(|| format!("unrecognized board card {code:?}"))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if board
        .iter()
        .any(|card| board.iter().filter(|c| *c == card).count() > 1)
    {
        return Err(format!("board {codes:?} lists a card twice"));
    }
    Ok(board)
}

/// Verifies that the engine's reconstructed board matches the real deal.
fn verify_board(state: &GameState, real: &[Card]) -> std::result::Result<(), String> {
    let engine = state.board();
    if engine.is_empty() {
        return Ok(());
    }
    let check = engine.len().min(real.len());
    if engine[..check] != real[..check] {
        return Err(format!(
            "board diverged: engine {:?} vs real {:?}",
            engine, real
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------- grading

/// Grades every decision point: solves the spot from the opponent's
/// perspective and accumulates the big-blind EV the played action gave up.
pub fn grade_points<R: Rng + ?Sized>(
    rng: &mut R,
    points: &[DecisionPoint],
    config: &MctsConfig,
) -> HandScore {
    let ranges: [Option<crate::range::hands::Range>; NUM_PLAYERS] = [None; NUM_PLAYERS];
    let mut score = HandScore::default();
    let mut per_player: HashMap<String, (u32, f64)> = HashMap::new();

    for point in points {
        if point.state.is_hand_over() || point.state.to_act() != Seat::Hero {
            score
                .problems
                .push(format!("unusable decision point for {}", point.actor_name));
            continue;
        }
        let mut candidates = mcts::candidates(&point.state);
        if !candidates.iter().any(|(action, _)| *action == point.played) {
            candidates.push((
                point.played,
                crate::decision::classify_played(&point.state, point.played),
            ));
        }
        let result = match mcts::solve_for_seat(
            rng,
            &point.state,
            &point.pins,
            &ranges,
            config,
            &candidates,
        ) {
            Ok(result) => result,
            Err(error) => {
                score
                    .problems
                    .push(format!("{}: {error}", point.actor_name));
                continue;
            }
        };
        let Some(best) = result.actions.first().map(|value| value.ev) else {
            score
                .problems
                .push(format!("{}: no candidate EVs", point.actor_name));
            continue;
        };
        let Some(played_ev) = result
            .actions
            .iter()
            .find(|value| value.action == point.played)
            .map(|value| value.ev)
        else {
            score.problems.push(format!(
                "{}: played action missing from the solve",
                point.actor_name
            ));
            continue;
        };
        let bb = f64::from(point.state.blind_level().big_blind);
        score.decisions += 1;
        score.ev_loss_bb_sum += ((best - played_ev).max(0.0)) / bb;
        let entry = per_player.entry(point.actor_name.clone()).or_default();
        entry.0 += 1;
        entry.1 += ((best - played_ev).max(0.0)) / bb;
    }

    let mut players: Vec<(String, u32, f64)> = per_player
        .into_iter()
        .map(|(name, (decisions, loss))| (name, decisions, loss / f64::from(decisions)))
        .collect();
    players.sort_by_key(|(_, decisions, _)| std::cmp::Reverse(*decisions));
    score.players = players;
    score
}

/// Both grading streams for one hand: the pooled opponent decisions (as
/// before) and the hero's own decisions in that same hand, graded
/// separately so the hero's number never leaks into the opponent pool that
/// drives the bot template.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HandGrades {
    pub opponent: HandScore,
    pub hero: HandScore,
}

/// Grades one imported hand's raw text end to end: episode → walk → grade,
/// for both the opponent decisions and the hero's own decisions.
pub fn score_hand(
    hand_id: &str,
    raw: &str,
    sb: i32,
    bb: i32,
    hero_cards: Option<[Card; 2]>,
    config: &MctsConfig,
) -> HandScoreWithId {
    let mut rng = seeded_rng(0x5EED_FACE ^ hand_id.len() as u64);
    let result = analyze(raw, sb, bb, hero_cards, config, &mut rng);
    HandScoreWithId {
        hand_id: hand_id.to_string(),
        grades: result,
    }
}

/// The identified wrapper returned by [`score_hand`]. The nested
/// [`HandGrades`] only holds "graded" outcomes on success.
#[derive(Clone, Debug, PartialEq)]
pub struct HandScoreWithId {
    pub hand_id: String,
    pub grades: HandGrades,
}

fn analyze<R: Rng + ?Sized>(
    raw: &str,
    sb: i32,
    bb: i32,
    hero_cards: Option<[Card; 2]>,
    config: &MctsConfig,
    rng: &mut R,
) -> HandGrades {
    match walk_and_grade(raw, sb, bb, hero_cards, config, rng) {
        Ok(grades) => grades,
        Err(problem) => {
            let failed = HandScore {
                decisions: 0,
                ev_loss_bb_sum: 0.0,
                players: Vec::new(),
                problems: vec![problem],
            };
            HandGrades {
                opponent: failed.clone(),
                hero: failed,
            }
        }
    }
}

fn walk_and_grade<R: Rng + ?Sized>(
    raw: &str,
    sb: i32,
    bb: i32,
    hero_cards: Option<[Card; 2]>,
    config: &MctsConfig,
    rng: &mut R,
) -> std::result::Result<HandGrades, String> {
    let episode =
        crate::hh::parse_episode(raw).ok_or_else(|| "no episode in the raw hand".to_string())?;
    let walked = walk_hand(&episode, sb, bb, hero_cards)?;
    Ok(HandGrades {
        opponent: grade_points(rng, &walked.opponent, config),
        hero: grade_points(rng, &walked.hero, config),
    })
}

// ---------------------------------------------------------------- reports

/// One analyzed opponent in the field report.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PlayerRow {
    pub name: String,
    pub decisions: u32,
    pub avg_ev_loss_bb: f64,
}

/// The pooled result over the rolling window ([`EV_ROLLING_WINDOW`] most
/// recent decisions): the opponents' summed decisions, their average
/// big-blind loss, and the resulting field skill — plus the same three
/// numbers for the hero's own decisions in the imported hands, kept
/// deliberately separate from the opponent pool (it never feeds the bot
/// template) and from the live-drill EV tracked in `hero_decisions`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FieldReport {
    pub hands_total: u32,
    pub hands_graded: u32,
    pub hands_failed: u32,
    pub decisions: i64,
    pub avg_ev_loss_bb: f64,
    pub skill: f64,
    pub hero_decisions: i64,
    pub hero_avg_ev_loss_bb: f64,
    pub hero_skill: f64,
    pub players: Vec<PlayerRow>,
    pub problems: Vec<String>,
}

impl FieldReport {
    pub fn empty() -> Self {
        Self {
            hands_total: 0,
            hands_graded: 0,
            hands_failed: 0,
            decisions: 0,
            avg_ev_loss_bb: 0.0,
            skill: 0.0,
            hero_decisions: 0,
            hero_avg_ev_loss_bb: 0.0,
            hero_skill: 0.0,
            players: Vec::new(),
            problems: Vec::new(),
        }
    }
}

/// The stored drill template both local bots play against.
#[derive(Clone, Debug, PartialEq)]
pub struct DrillTemplate {
    pub label: String,
    pub skill: f64,
    pub avg_ev_loss_bb: f64,
    pub decisions: i32,
}

// --------------------------------------------------------------------- job

/// The live state of the background analysis job, shared between the worker
/// and the status endpoint.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub enum JobState {
    #[default]
    Idle,
    Running {
        hands_done: u32,
        hands_total: u32,
    },
    Done(FieldReport),
}

/// One recent imported hand feeding the analysis window.
#[derive(Clone, Debug)]
pub struct RecentHand {
    pub hand_id: String,
    pub raw: String,
    pub sb: i32,
    pub bb: i32,
    pub hero_cards: Option<[Card; 2]>,
}

/// One stored per-hand analysis row: opponent decisions in that hand, and
/// the hero's own decisions in that same hand.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHandScore {
    pub hand_id: String,
    pub decisions: i32,
    pub ev_loss_bb_sum: f64,
    pub hero_decisions: i32,
    pub hero_ev_loss_bb_sum: f64,
}

/// Loads the most recent imported hands (newest first), capped at `limit`.
pub async fn load_recent_hands(pool: &PgPool, limit: i64) -> Result<Vec<RecentHand>> {
    let rows: Vec<(String, String, i32, i32, Option<String>)> = sqlx::query_as(
        "SELECT hand_id, raw, sb, bb, hero_cards
         FROM gg_hands
         ORDER BY played_at DESC, hand_id DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(hand_id, raw, sb, bb, hero_cards)| RecentHand {
            hand_id,
            raw,
            sb,
            bb,
            hero_cards: hero_cards.as_deref().and_then(parse_two_codes),
        })
        .collect())
}

/// Splits `"As Kh"` into concrete cards, when both codes parse.
fn parse_two_codes(text: &str) -> Option<[Card; 2]> {
    let codes: Vec<&str> = text.split_whitespace().take(2).collect();
    Some([
        Card::from_code(codes.first()?)?,
        Card::from_code(codes.get(1)?)?,
    ])
}

/// Loads the stored per-hand scores for the given hands (only rows that
/// exist).
pub async fn load_stored_scores(
    pool: &PgPool,
    hand_ids: &[String],
) -> Result<Vec<StoredHandScore>> {
    if hand_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<&str> = hand_ids.iter().map(String::as_str).collect();
    let rows: Vec<(String, i32, f64, i32, f64)> = sqlx::query_as(
        "SELECT hand_id, opponent_decisions, ev_loss_bb_sum, hero_decisions, hero_ev_loss_bb_sum
         FROM gg_hand_analysis
         WHERE hand_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(hand_id, decisions, ev_loss_bb_sum, hero_decisions, hero_ev_loss_bb_sum)| {
                StoredHandScore {
                    hand_id,
                    decisions,
                    ev_loss_bb_sum,
                    hero_decisions,
                    hero_ev_loss_bb_sum,
                }
            },
        )
        .collect())
}

/// Stores one hand's analysis result; already-analyzed hands are left alone.
pub async fn store_hand_score(pool: &PgPool, score: &StoredHandScore) -> Result<()> {
    sqlx::query(
        "INSERT INTO gg_hand_analysis
             (hand_id, opponent_decisions, ev_loss_bb_sum, hero_decisions, hero_ev_loss_bb_sum)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (hand_id) DO NOTHING",
    )
    .bind(&score.hand_id)
    .bind(score.decisions)
    .bind(score.ev_loss_bb_sum)
    .bind(score.hero_decisions)
    .bind(score.hero_ev_loss_bb_sum)
    .execute(pool)
    .await?;
    Ok(())
}

/// Locks the shared job state without panicking on poisoning.
pub fn job_guard(state: &Arc<Mutex<JobState>>) -> Result<std::sync::MutexGuard<'_, JobState>> {
    state
        .lock()
        .map_err(|_| crate::error::Error::Analytics("analysis job lock poisoned".into()))
}

/// Runs the full background analysis over the hand-history window and stores
/// the field template. Progress is published into `state` after every hand.
pub async fn run_job(
    pool: PgPool,
    state: Arc<Mutex<JobState>>,
    config: MctsConfig,
) -> Result<FieldReport> {
    let hands = load_recent_hands(&pool, ANALYSIS_WINDOW).await?;
    let total = hands.len() as u32;
    *job_guard(&state)? = JobState::Running {
        hands_done: 0,
        hands_total: total,
    };

    let ids: Vec<String> = hands.iter().map(|hand| hand.hand_id.clone()).collect();
    let stored = load_stored_scores(&pool, &ids).await?;
    let stored_ids: std::collections::HashSet<&str> =
        stored.iter().map(|row| row.hand_id.as_str()).collect();

    let mut hands_failed = 0u32;
    let mut hands_graded = stored.len() as u32;
    let mut problems: Vec<String> = Vec::new();
    let mut per_player: HashMap<String, (u32, f64)> = HashMap::new();

    for (index, hand) in hands.iter().enumerate() {
        if stored_ids.contains(hand.hand_id.as_str()) {
            continue;
        }
        let input = (hand.clone(), config);
        let outcome = tokio::task::spawn_blocking(move || {
            let (hand, config) = input;
            score_hand(
                &hand.hand_id,
                &hand.raw,
                hand.sb,
                hand.bb,
                hand.hero_cards,
                &config,
            )
        })
        .await
        .map_err(|error| {
            crate::error::Error::Analytics(format!("scoring task panicked: {error}"))
        })?;

        let grades = outcome.grades;
        for (name, player_decisions, avg_loss) in &grades.opponent.players {
            let entry = per_player.entry(name.clone()).or_default();
            entry.0 += *player_decisions;
            entry.1 += avg_loss * f64::from(*player_decisions);
        }
        for problem in &grades.opponent.problems {
            if problems.len() < 20 {
                problems.push(format!("hand {}: {problem}", hand.hand_id));
            }
        }
        if grades.hero.problems != grades.opponent.problems {
            for problem in &grades.hero.problems {
                if problems.len() < 20 {
                    problems.push(format!("hand {}: {problem}", hand.hand_id));
                }
            }
        }

        let has_data = grades.opponent.decisions > 0 || grades.hero.decisions > 0;
        if !has_data {
            if grades.opponent.problems.is_empty() && grades.hero.problems.is_empty() {
                hands_graded += 1;
            } else {
                hands_failed += 1;
            }
        } else {
            hands_graded += 1;
            if let Err(error) = store_hand_score(
                &pool,
                &StoredHandScore {
                    hand_id: hand.hand_id.clone(),
                    decisions: grades.opponent.decisions as i32,
                    ev_loss_bb_sum: grades.opponent.ev_loss_bb_sum,
                    hero_decisions: grades.hero.decisions as i32,
                    hero_ev_loss_bb_sum: grades.hero.ev_loss_bb_sum,
                },
            )
            .await
            {
                tracing::warn!(%error, hand_id = %hand.hand_id, "hand analysis could not be stored; result kept in memory");
            }
        }

        *job_guard(&state)? = JobState::Running {
            hands_done: index as u32 + 1,
            hands_total: total,
        };
    }

    let mut players: Vec<PlayerRow> = per_player
        .into_iter()
        .map(|(name, (player_decisions, loss))| PlayerRow {
            name,
            decisions: player_decisions,
            avg_ev_loss_bb: if player_decisions > 0 {
                loss / f64::from(player_decisions)
            } else {
                0.0
            },
        })
        .collect();
    players.sort_by_key(|player| std::cmp::Reverse(player.decisions));

    let (opponent_rolling, hero_rolling) = rolling_field_stats(&pool).await?;
    let report = FieldReport {
        hands_total: total,
        hands_graded,
        hands_failed,
        decisions: opponent_rolling.decisions,
        avg_ev_loss_bb: opponent_rolling.avg_ev_loss_bb,
        skill: opponent_rolling.skill,
        hero_decisions: hero_rolling.decisions,
        hero_avg_ev_loss_bb: hero_rolling.avg_ev_loss_bb,
        hero_skill: hero_rolling.skill,
        players,
        problems,
    };
    *job_guard(&state)? = JobState::Done(report.clone());
    Ok(report)
}

/// One side's (opponent pool, or hero) rolling stat over the most recent
/// [`EV_ROLLING_WINDOW`] graded decisions.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RollingStat {
    pub decisions: i64,
    pub avg_ev_loss_bb: f64,
    pub skill: f64,
}

fn rolling_stat(ev_loss_sum: f64, decisions: i64) -> RollingStat {
    RollingStat {
        decisions,
        avg_ev_loss_bb: if decisions > 0 {
            ev_loss_sum / decisions as f64
        } else {
            0.0
        },
        skill: skill_from(ev_loss_sum, decisions.min(i64::from(u32::MAX)) as u32),
    }
}

/// Both rolling stats — the pooled opponent field, then the hero — computed
/// purely from the [`gg_hand_analysis`] cache (no solver work), so this is
/// cheap enough to call on every hand-history page load rather than only
/// after a fresh analysis run. Returns `(opponent, hero)`.
pub async fn rolling_field_stats(pool: &PgPool) -> Result<(RollingStat, RollingStat)> {
    type Row = (String, i32, f64, i32, f64);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT h.hand_id,
                COALESCE(a.opponent_decisions, 0), COALESCE(a.ev_loss_bb_sum, 0),
                COALESCE(a.hero_decisions, 0), COALESCE(a.hero_ev_loss_bb_sum, 0)
         FROM gg_hands h
         LEFT JOIN gg_hand_analysis a ON a.hand_id = h.hand_id
         ORDER BY h.played_at DESC, h.hand_id DESC
         LIMIT $1",
    )
    .bind(ANALYSIS_WINDOW)
    .fetch_all(pool)
    .await?;

    let mut opp_decisions = 0i64;
    let mut opp_ev_sum = 0.0f64;
    let mut hero_decisions = 0i64;
    let mut hero_ev_sum = 0.0f64;
    for (_, decisions, ev_loss_bb_sum, hero_dec, hero_ev) in &rows {
        if opp_decisions < EV_ROLLING_WINDOW {
            opp_decisions += i64::from(*decisions);
            opp_ev_sum += ev_loss_bb_sum;
        }
        if hero_decisions < EV_ROLLING_WINDOW {
            hero_decisions += i64::from(*hero_dec);
            hero_ev_sum += hero_ev;
        }
        if opp_decisions >= EV_ROLLING_WINDOW && hero_decisions >= EV_ROLLING_WINDOW {
            break;
        }
    }
    Ok((
        rolling_stat(opp_ev_sum, opp_decisions),
        rolling_stat(hero_ev_sum, hero_decisions),
    ))
}

/// One tournament's average EV loss per decision — hero and the opponent
/// field, kept separate — over *every* analyzed hand in that tournament
/// (unbounded by [`EV_ROLLING_WINDOW`], unlike the pooled counters: a single
/// tournament is never anywhere near that window size).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TournamentEv {
    pub hero_decisions: i64,
    pub hero_avg_ev_loss_bb: f64,
    pub opponent_decisions: i64,
    pub opponent_avg_ev_loss_bb: f64,
}

/// Every tournament's average EV loss per decision, keyed by tournament id.
/// A tournament with no analyzed hands simply has no entry.
pub async fn tournament_ev_totals(pool: &PgPool) -> Result<HashMap<String, TournamentEv>> {
    type Row = (String, i64, f64, i64, f64);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT h.tournament_id,
                COALESCE(SUM(a.hero_decisions), 0), COALESCE(SUM(a.hero_ev_loss_bb_sum), 0),
                COALESCE(SUM(a.opponent_decisions), 0), COALESCE(SUM(a.ev_loss_bb_sum), 0)
         FROM gg_hands h
         JOIN gg_hand_analysis a ON a.hand_id = h.hand_id
         GROUP BY h.tournament_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(tournament_id, hero_decisions, hero_ev_sum, opponent_decisions, opponent_ev_sum)| {
                (
                    tournament_id,
                    TournamentEv {
                        hero_decisions,
                        hero_avg_ev_loss_bb: if hero_decisions > 0 {
                            hero_ev_sum / hero_decisions as f64
                        } else {
                            0.0
                        },
                        opponent_decisions,
                        opponent_avg_ev_loss_bb: if opponent_decisions > 0 {
                            opponent_ev_sum / opponent_decisions as f64
                        } else {
                            0.0
                        },
                    },
                )
            },
        )
        .collect())
}

/// The stored drill template (`drill_template` is a single-row table).
pub async fn save_template(
    pool: &PgPool,
    label: &str,
    skill: f64,
    avg_ev_loss_bb: f64,
    decisions: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO drill_template (id, label, skill, avg_ev_loss_bb, decisions, updated_at)
         VALUES (1, $1, $2, $3, $4, now())
         ON CONFLICT (id) DO UPDATE SET
             label = EXCLUDED.label,
             skill = EXCLUDED.skill,
             avg_ev_loss_bb = EXCLUDED.avg_ev_loss_bb,
             decisions = EXCLUDED.decisions,
             updated_at = now()",
    )
    .bind(label)
    .bind(skill)
    .bind(avg_ev_loss_bb)
    .bind(decisions)
    .execute(pool)
    .await?;
    Ok(())
}

/// Reads the stored drill template, if one was saved.
pub async fn load_template(pool: &PgPool) -> Result<Option<DrillTemplate>> {
    let row: Option<(String, f64, f64, i32)> = sqlx::query_as(
        "SELECT label, skill, avg_ev_loss_bb, decisions FROM drill_template WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map(|(label, skill, avg_ev_loss_bb, decisions)| DrillTemplate {
            label,
            skill,
            avg_ev_loss_bb,
            decisions,
        }),
    )
}

/// Removes the stored drill template.
pub async fn clear_template(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM drill_template WHERE id = 1")
        .execute(pool)
        .await?;
    Ok(())
}

/// The hero's average EV loss per decision, in big blinds, over the last
/// [`crate::analytics::CHART_WINDOW`] decisions — the same window backing
/// the playing table's chart.
pub async fn hero_avg_ev_loss(pool: &PgPool) -> Result<Option<f64>> {
    let row: Option<Option<f64>> = sqlx::query_scalar(
        "SELECT AVG(ev_loss) FROM (
             SELECT ev_loss FROM hero_decisions ORDER BY id DESC LIMIT $1
         ) recent",
    )
    .bind(crate::analytics::CHART_WINDOW as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.flatten())
}

/// The hero's skill on the same scale as the field template: the recent
/// average EV loss (over [`crate::analytics::CHART_WINDOW`] decisions) mapped
/// through [`skill_from`].
pub async fn hero_skill(pool: &PgPool) -> Result<Option<f64>> {
    let Some(avg) = hero_avg_ev_loss(pool).await? else {
        return Ok(None);
    };
    Ok(Some((1.0 - avg / SKILL_ZERO_LOSS_BB).clamp(0.0, 1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Rank;
    use crate::game::blinds::BlindLevel;
    use crate::hh::EpisodeAction;

    /// A real GGPoker hand block (win at showdown, 2-max).
    const SAMPLE_WIN: &str = "Poker Hand #SG4176965290: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level3(20/40) - 2026/08/21 15:07:44
Table '39856' 3-max Seat #3 is the button
Seat 2: Hero (525 in chips)
Seat 3: 14c11a2a (375 in chips)
14c11a2a: posts small blind 20
Hero: posts big blind 40
*** HOLE CARDS ***
Dealt to Hero [As Kh]
14c11a2a: calls 20
Hero: raises 40 to 80
14c11a2a: calls 40
*** FLOP *** [Jd 3c 8c]
Hero: bets 40
14c11a2a: calls 40
*** TURN *** [Jd 3c 8c] [Qd]
Hero: bets 40
14c11a2a: calls 40
*** RIVER *** [Jd 3c 8c Qd] [7s]
Hero: bets 40
14c11a2a: raises 175 to 215 and is all-in
Hero: calls 175
14c11a2a: shows [4d Td] (Queen high)
Hero: shows [As Kh] (Ace high)
*** SHOWDOWN ***
Hero collected 750 from pot
*** SUMMARY ***
Total pot 750 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [Jd 3c 8c Qd 7s]
Seat 2: Hero (big blind) showed [As Kh] and won (750) with Ace high
Seat 3: 14c11a2a (small blind) showed [4d Td] and lost with Queen high";

    /// A real GGPoker hand block (fold on the small blind, 2-max).
    const SAMPLE_FOLD_SB: &str = "Poker Hand #SG4176965222: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level3(20/40) - 2026/08/21 15:07:36
Table '39856' 3-max Seat #2 is the button
Seat 2: Hero (540 in chips)
Seat 3: 14c11a2a (360 in chips)
Hero: posts small blind 15
14c11a2a: posts big blind 30
*** HOLE CARDS ***
Dealt to Hero [6h 8d]
Hero: folds
Uncalled bet (15) returned to 14c11a2a
*** SHOWDOWN ***
14c11a2a collected 30 from pot
*** SUMMARY ***
Total pot 30 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Seat 2: Hero (small blind) folded before Flop
Seat 3: 14c11a2a (big blind) collected (30)";

    /// A real GGPoker hand block (bluff win, all-in river, 2-max).
    const SAMPLE_BLUFF_WIN: &str = "Poker Hand #SG4176963213: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:04:30
Table '39856' 3-max Seat #2 is the button
Seat 2: Hero (300 in chips)
Seat 3: 14c11a2a (600 in chips)
Hero: posts small blind 10
14c11a2a: posts big blind 20
*** HOLE CARDS ***
Dealt to Hero [3c Ac]
Hero: raises 20 to 40
14c11a2a: calls 20
*** FLOP *** [Ks 4s 3d]
14c11a2a: checks
Hero: bets 20
14c11a2a: calls 20
*** TURN *** [Ks 4s 3d] [6s]
14c11a2a: bets 120
Hero: calls 120
*** RIVER *** [Ks 4s 3d 6s] [2s]
14c11a2a: checks
Hero: bets 120 and is all-in
14c11a2a: folds
Uncalled bet (120) returned to Hero
*** SHOWDOWN ***
Hero collected 360 from pot
*** SUMMARY ***
Total pot 360 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [Ks 4s 3d 6s 2s]
Seat 2: Hero (small blind) won (360)
Seat 3: 14c11a2a (big blind) folded on the River";

    /// A real GGPoker 3-max hand with distinct button/small blind/big blind
    /// seats.
    const SAMPLE_THREE_MAX: &str = "Poker Hand #SG4176962837: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:03:55
Table '39856' 3-max Seat #2 is the button
Seat 1: facf7b06 (300 in chips)
Seat 2: Hero (290 in chips)
Seat 3: 14c11a2a (310 in chips)
14c11a2a: posts small blind 10
facf7b06: posts big blind 20
*** HOLE CARDS ***
Dealt to Hero [4c 7s]
Hero: folds
14c11a2a: raises 40 to 60
facf7b06: calls 40
*** FLOP *** [7c 6d 2h]
facf7b06: bets 240 and is all-in
14c11a2a: calls 240
facf7b06: shows [Ks 5s] (King high)
14c11a2a: shows [Qs Ad] (Ace high)
*** TURN *** [7c 6d 2h] [8s]
*** RIVER *** [7c 6d 2h 8s] [Qh]
*** SHOWDOWN ***
14c11a2a collected 600 from pot
*** SUMMARY ***
Total pot 600 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [7c 6d 2h 8s Qh]
Seat 1: facf7b06 (big blind) showed [Ks 5s] and lost with King high
Seat 2: Hero (button) folded before Flop (didn't bet)
Seat 3: 14c11a2a (small blind) showed [Qs Ad] and won (600) with a pair of Queens";

    fn hero_cards(text: &str) -> Option<[Card; 2]> {
        let codes: Vec<&str> = text.split_whitespace().collect();
        Some([
            Card::from_code(codes.first()?)?,
            Card::from_code(codes.get(1)?)?,
        ])
    }

    fn walk(raw: &str, sb: i32, bb: i32) -> Vec<DecisionPoint> {
        let episode = crate::hh::parse_episode(raw).expect("sample parses");
        walk_hand(&episode, sb, bb, hero_cards("As Kh"))
            .expect("sample walks")
            .opponent
    }

    fn walk_hero(raw: &str, sb: i32, bb: i32) -> Vec<DecisionPoint> {
        let episode = crate::hh::parse_episode(raw).expect("sample parses");
        walk_hand(&episode, sb, bb, hero_cards("As Kh"))
            .expect("sample walks")
            .hero
    }

    #[test]
    fn skill_maps_the_anchor_to_zero_and_perfect_play_to_one() {
        assert_eq!(skill_from(0.0, 100), 1.0, "no loss is solver-perfect");
        assert_eq!(
            skill_from(SKILL_ZERO_LOSS_BB * 100.0, 100),
            0.0,
            "the anchor is skill zero"
        );
        assert!(
            (skill_from(SKILL_ZERO_LOSS_BB / 2.0 * 100.0, 100) - 0.5).abs() < 1e-9,
            "half the anchor is skill 0.5"
        );
        assert!(
            (skill_from(SKILL_ZERO_LOSS_BB / 4.0, 1) - 0.75).abs() < 1e-9,
            "a quarter of the anchor is skill 0.75"
        );
        assert_eq!(skill_from(9e9, 100), 0.0, "massive loss clamps to zero");
        assert_eq!(skill_from(0.0, 0), 0.0, "no data is skill zero");
        assert_eq!(
            skill_from(-5.0, 1),
            1.0,
            "negative (impossible) loss clamps"
        );
    }

    #[test]
    fn walk_heads_up_showdown_hand_freezes_every_opponent_decision() {
        let points = walk(SAMPLE_WIN, 20, 40);
        // 14c11a2a: four calls plus the river all-in raise.
        assert_eq!(points.len(), 5);
        assert!(points.iter().all(|p| p.actor_name == "14c11a2a"));
        assert!(points.iter().all(|p| p.state.to_act() == Seat::Hero));
        assert!(
            points.iter().all(|p| !p.state.is_hand_over()),
            "decision points are live"
        );
        // First decision: facing a 20-chip call with 355 behind (375 - 20 SB).
        let first = &points[0];
        assert_eq!(first.played, Action::Call);
        assert_eq!(first.state.stack(Seat::Hero), 355);
        // The hero's As Kh follow the rotation into the pin array.
        let expected_hero = hero_cards("As Kh").unwrap();
        let pinned: Vec<Option<[Card; 2]>> = first.pins.to_vec();
        assert_eq!(pinned.iter().flatten().count(), 1);
        assert!(pinned.contains(&Some(expected_hero)));
        assert_eq!(first.state.blind_level(), BlindLevel::new(20, 40));
        // River raise is captured as an all-in push.
        assert_eq!(points[4].played, Action::AllIn);
        assert_eq!(points[4].state.street(), Street::River);
    }

    #[test]
    fn walk_captures_the_opponent_hand_when_shown_at_showdown() {
        let points = walk(SAMPLE_WIN, 20, 40);
        let revealed = Hand::from_cards(
            Card::from_code("4d").unwrap(),
            Card::from_code("Td").unwrap(),
        );
        assert!(
            points.iter().all(|p| p.opponent_hand == Some(revealed)),
            "the showdown reveal is known retroactively for every decision point in the hand"
        );
    }

    #[test]
    fn walk_leaves_the_opponent_hand_unknown_without_a_reveal() {
        let points = walk(SAMPLE_BLUFF_WIN, 10, 20);
        assert!(
            points.iter().all(|p| p.opponent_hand.is_none()),
            "the opponent folded — their cards were never shown"
        );
    }

    #[test]
    fn walk_handles_a_fold_win_without_a_board() {
        // The hero folds the small blind preflop: no street ever advances and
        // the hand still replays cleanly (zero opponent decisions, one hero
        // decision — the fold itself).
        let episode = crate::hh::parse_episode(SAMPLE_FOLD_SB).expect("sample parses");
        let walked = walk_hand(&episode, 15, 30, hero_cards("6h 8d")).expect("walk succeeds");
        assert!(walked.opponent.is_empty());
        assert_eq!(walked.hero.len(), 1);
        assert_eq!(walked.hero[0].played, Action::Fold);
        assert_eq!(walked.hero[0].actor_name, "Hero");
    }

    #[test]
    fn walk_captures_every_hero_decision_alongside_the_opponent_ones() {
        let hero_points = walk_hero(SAMPLE_WIN, 20, 40);
        // Hero: the preflop raise, then a bet on every later street, then the
        // river call of the all-in raise.
        assert_eq!(
            hero_points.iter().map(|p| p.played).collect::<Vec<_>>(),
            vec![
                // Hero is the big blind facing only the small blind's call
                // (to_call == 0), so this reads as a bet, not a raise — the
                // same convention `convert_action` uses everywhere else.
                Action::Bet(80),
                Action::Bet(40),
                Action::Bet(40),
                Action::Bet(40),
                Action::Call,
            ]
        );
        assert!(hero_points.iter().all(|p| p.actor_name == "Hero"));
        assert!(hero_points.iter().all(|p| p.state.to_act() == Seat::Hero));
        assert!(
            hero_points
                .iter()
                .all(|p| p.state.hero_cards() == hero_cards("As Kh").unwrap()),
            "the hero's real cards ride along after the rotation"
        );
    }

    #[test]
    fn walk_replays_a_three_max_hand_with_distinct_blinds() {
        let points = walk(SAMPLE_THREE_MAX, 10, 20);
        // 14c11a2a: preflop raise + flop call; facf7b06: preflop call + flop
        // all-in bet.
        assert_eq!(points.len(), 4);
        assert_eq!(points[0].actor_name, "14c11a2a");
        assert_eq!(points[0].played, Action::Raise(60));
        assert_eq!(points[1].actor_name, "facf7b06");
        assert_eq!(points[1].played, Action::Call);
        assert_eq!(points[2].actor_name, "facf7b06");
        assert_eq!(points[2].played, Action::AllIn);
        assert_eq!(points[2].state.street(), Street::Flop);
        assert_eq!(points[3].actor_name, "14c11a2a");
        assert_eq!(points[3].played, Action::Call);
        assert_eq!(points[3].state.street(), Street::Flop);
    }

    #[test]
    fn walk_replays_blind_posts_calls_bets_and_folds() {
        let points = walk(SAMPLE_BLUFF_WIN, 10, 20);
        // 14c11a2a: preflop call, flop check + call, turn bet, river check +
        // fold.
        assert_eq!(
            points.iter().map(|p| p.played).collect::<Vec<_>>(),
            vec![
                Action::Call,
                Action::Check,
                Action::Call,
                Action::Bet(120),
                Action::Check,
                Action::Fold
            ]
        );
    }

    #[test]
    fn convert_action_collapses_all_in_shapes() {
        let mut fake = EpisodeAction {
            seat_no: 1,
            verb: EpisodeVerb::Bet,
            amount: Some(100),
            to: None,
            all_in: true,
        };
        assert_eq!(convert_action(0, 100, 0, &fake).unwrap(), Action::AllIn);
        fake.all_in = false;
        assert_eq!(convert_action(0, 100, 0, &fake).unwrap(), Action::Bet(100));

        let raise_all_in = EpisodeAction {
            seat_no: 1,
            verb: EpisodeVerb::Raise,
            amount: Some(175),
            to: Some(215),
            all_in: true,
        };
        // contrib 40, stack 175: the raise puts all 175 additional chips in.
        assert_eq!(
            convert_action(40, 175, 40, &raise_all_in).unwrap(),
            Action::AllIn
        );
        let raise_full = EpisodeAction {
            seat_no: 1,
            verb: EpisodeVerb::Raise,
            amount: Some(40),
            to: Some(120),
            all_in: false,
        };
        assert_eq!(
            convert_action(80, 500, 60, &raise_full).unwrap(),
            Action::Raise(120)
        );
        // The big blind's option reads as a bet: nothing had to be called.
        assert_eq!(
            convert_action(40, 485, 0, &raise_full).unwrap(),
            Action::Bet(120)
        );

        let call_all_in = EpisodeAction {
            seat_no: 1,
            verb: EpisodeVerb::Call,
            amount: Some(175),
            to: None,
            all_in: true,
        };
        assert_eq!(
            convert_action(40, 175, 175, &call_all_in).unwrap(),
            Action::AllIn
        );
        assert!(convert_action(40, 200, 175, &call_all_in).unwrap() != Action::AllIn);

        let fold = EpisodeAction {
            seat_no: 1,
            verb: EpisodeVerb::Fold,
            amount: None,
            to: None,
            all_in: false,
        };
        assert_eq!(convert_action(0, 100, 0, &fold).unwrap(), Action::Fold);
        assert!(
            convert_action(
                0,
                100,
                0,
                &EpisodeAction {
                    seat_no: 1,
                    verb: EpisodeVerb::Check,
                    amount: None,
                    to: None,
                    all_in: false
                }
            )
            .is_ok()
        );
        // A check facing a bet is flagged: the real hand cannot be this.
        assert!(
            convert_action(
                0,
                100,
                40,
                &EpisodeAction {
                    seat_no: 1,
                    verb: EpisodeVerb::Check,
                    amount: None,
                    to: None,
                    all_in: false
                }
            )
            .is_err()
        );
    }

    #[test]
    fn real_board_cards_prefer_the_fullest_cumulative_board() {
        let episode = crate::hh::parse_episode(SAMPLE_WIN).expect("sample parses");
        let board = real_board_cards(&episode).unwrap();
        let codes: Vec<String> = board.iter().map(|card| card.to_code()).collect();
        assert_eq!(codes, vec!["Jd", "3c", "8c", "Qd", "7s"]);

        let fold = crate::hh::parse_episode(SAMPLE_FOLD_SB).expect("sample parses");
        assert_eq!(
            real_board_cards(&fold).unwrap(),
            Vec::<Card>::new(),
            "no board without streets or summary"
        );
    }

    #[test]
    fn grade_points_measures_big_blind_loss() {
        let points = walk(SAMPLE_WIN, 20, 40);
        let mut rng = seeded_rng(7);
        let score = grade_points(&mut rng, &points, &MctsConfig::test());
        assert_eq!(score.decisions, 5);
        assert!(score.ev_loss_bb_sum >= 0.0);
        assert!(
            score
                .players
                .iter()
                .all(|(_, decisions, avg)| { *decisions > 0 && avg.is_finite() && *avg >= 0.0 })
        );
    }

    #[test]
    fn grade_points_is_deterministic_for_a_seed() {
        let points = walk(SAMPLE_BLUFF_WIN, 10, 20);
        let mut a = seeded_rng(11);
        let mut b = seeded_rng(11);
        let first = grade_points(&mut a, &points, &MctsConfig::test());
        let second = grade_points(&mut b, &points, &MctsConfig::test());
        assert_eq!(first.decisions, second.decisions);
        assert_eq!(first.ev_loss_bb_sum, second.ev_loss_bb_sum);
    }

    #[test]
    fn score_hand_reports_unwalkable_hands_as_problems() {
        let score = score_hand("X", "garbage", 10, 20, None, &MctsConfig::test());
        assert_eq!(score.grades.opponent.decisions, 0);
        assert_eq!(score.grades.hero.decisions, 0);
        assert!(!score.grades.opponent.problems.is_empty());
        assert!(!score.grades.hero.problems.is_empty());
    }

    #[test]
    fn score_hand_grades_sample_hands_end_to_end() {
        for (raw, sb, bb) in [
            (SAMPLE_WIN, 20, 40),
            (SAMPLE_BLUFF_WIN, 10, 20),
            (SAMPLE_THREE_MAX, 10, 20),
            (SAMPLE_FOLD_SB, 15, 30),
        ] {
            let score = score_hand("H", raw, sb, bb, Some(dummy_hero()), &MctsConfig::test());
            assert!(
                score.grades.opponent.problems.is_empty(),
                "no problems for a real hand: {:?}",
                score.grades.opponent.problems
            );
            assert!(
                score.grades.hero.problems.is_empty(),
                "no hero problems for a real hand: {:?}",
                score.grades.hero.problems
            );
        }
    }

    fn dummy_hero() -> [Card; 2] {
        [
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::King, Suit::Hearts),
        ]
    }

    #[test]
    fn parse_two_codes_reads_pokercraft_cards() {
        assert_eq!(
            parse_two_codes("As Kh"),
            Some([
                Card::new(Rank::Ace, Suit::Spades),
                Card::new(Rank::King, Suit::Hearts)
            ])
        );
        assert_eq!(parse_two_codes("nonsense"), None);
        assert_eq!(parse_two_codes("As"), None);
    }

    #[test]
    fn job_state_serializes_and_defaults_idle() {
        assert_eq!(JobState::default(), JobState::Idle);
        let running = JobState::Running {
            hands_done: 3,
            hands_total: 9,
        };
        let json = serde_json::to_string(&running).unwrap();
        assert!(
            json.contains("\"Hands\"") || json.contains("hands_done"),
            "{json}"
        );
        assert!(json.contains("\"Running\""), "{json}");
        let done = JobState::Done(FieldReport::empty());
        let done_json = serde_json::to_string(&done).unwrap();
        assert!(done_json.contains("\"Done\""), "{done_json}");
        assert_ne!(done, JobState::Idle);
    }

    #[test]
    fn field_report_empty_state_is_consistent() {
        let empty = FieldReport::empty();
        assert_eq!(empty.decisions, 0);
        assert_eq!(empty.skill, 0.0);
        assert!(empty.players.is_empty());
    }

    // ---------------------------------------------------- database tests

    async fn test_pool() -> PgPool {
        crate::db::test_pool().await
    }

    fn unique(prefix: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "{prefix}_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    async fn insert_hand(pool: &PgPool, hand_id: &str, raw: &str) {
        let tournament_id = format!("T_{hand_id}");
        sqlx::query(
            "INSERT INTO gg_tournaments (id, name, started_at) VALUES ($1, 'Spin', '2026-01-01 00:00:00')",
        )
        .bind(&tournament_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gg_hands
                 (hand_id, tournament_id, played_at, sb, bb, position, table_size,
                  hero_cards, all_in, showdown, hero_won, invested, collected, net, raw)
             VALUES ($1, $2, '2026-01-01 00:00:00', 10, 20, 'BTN', 3,
                     'As Kh', false, false, false, 10, 0, -10, $3)",
        )
        .bind(hand_id)
        .bind(&tournament_id)
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn hand_scores_round_trip_incrementally() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let hand_a = unique("analysis_hand");
        let hand_b = unique("analysis_hand");
        insert_hand(&pool, &hand_a, SAMPLE_WIN).await;
        insert_hand(&pool, &hand_b, SAMPLE_BLUFF_WIN).await;

        let recent = load_recent_hands(&pool, ANALYSIS_WINDOW).await.unwrap();
        assert!(
            recent.iter().any(|hand| hand.hand_id == hand_a),
            "the fresh hand is in the window"
        );
        assert!(
            recent
                .iter()
                .any(|hand| hand.hand_id == hand_b && hand.hero_cards.is_some()),
            "hero cards restore from the stored text"
        );

        let ids = [hand_a.clone(), hand_b.clone()];
        assert!(load_stored_scores(&pool, &ids).await.unwrap().is_empty());
        store_hand_score(
            &pool,
            &StoredHandScore {
                hand_id: hand_a.clone(),
                decisions: 5,
                ev_loss_bb_sum: 1.25,
                hero_decisions: 4,
                hero_ev_loss_bb_sum: 0.75,
            },
        )
        .await
        .unwrap();
        // Re-storing never overwrites.
        store_hand_score(
            &pool,
            &StoredHandScore {
                hand_id: hand_a.clone(),
                decisions: 99,
                ev_loss_bb_sum: 99.0,
                hero_decisions: 99,
                hero_ev_loss_bb_sum: 99.0,
            },
        )
        .await
        .unwrap();
        let stored = load_stored_scores(&pool, &ids).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].hand_id, hand_a);
        assert_eq!(stored[0].decisions, 5);
        assert_eq!(stored[0].ev_loss_bb_sum, 1.25);
        assert_eq!(stored[0].hero_decisions, 4);
        assert_eq!(stored[0].hero_ev_loss_bb_sum, 0.75);

        sqlx::query("DELETE FROM gg_tournaments WHERE id = ANY($1)")
            .bind(vec![format!("T_{hand_a}"), format!("T_{hand_b}")])
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_job_grades_fresh_hands_and_reuses_stored_scores() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let hand_a = unique("analysis_job");
        let hand_b = unique("analysis_job");
        let hand_bad = unique("analysis_job");
        insert_hand(&pool, &hand_a, SAMPLE_WIN).await;
        insert_hand(&pool, &hand_b, SAMPLE_BLUFF_WIN).await;
        insert_hand(&pool, &hand_bad, "not a hand").await;

        let state = Arc::new(Mutex::new(JobState::Idle));
        let report = run_job(pool.clone(), state.clone(), MctsConfig::test())
            .await
            .unwrap();
        let ids = [hand_a.clone(), hand_b.clone(), hand_bad.clone()];
        let stored = load_stored_scores(&pool, &ids).await.unwrap();
        assert_eq!(
            stored.len(),
            2,
            "the two walkable hands are cached, the garbage one is not"
        );
        assert!(stored.iter().all(|row| row.decisions > 0), "{stored:?}");
        assert!(
            stored.iter().all(|row| row.hero_decisions > 0),
            "hero's own decisions are graded alongside the opponent's: {stored:?}"
        );
        let stored_decisions: i64 = stored.iter().map(|row| i64::from(row.decisions)).sum();
        assert!(
            report.decisions >= stored_decisions,
            "the report aggregates the stored window: {} >= {stored_decisions}",
            report.decisions
        );
        assert!(
            report.hero_decisions > 0,
            "the report's hero figures are populated too"
        );
        assert_eq!(
            job_guard(&state).unwrap().clone(),
            JobState::Done(report.clone()),
            "the job parks its final report in the shared state"
        );

        // A re-run finds the cached rows and grades nothing new.
        let state = Arc::new(Mutex::new(JobState::Idle));
        let again = run_job(pool.clone(), state.clone(), MctsConfig::test())
            .await
            .unwrap();
        assert_eq!(again.decisions, report.decisions);
        assert_eq!(again.avg_ev_loss_bb, report.avg_ev_loss_bb);
        assert_eq!(again.hero_decisions, report.hero_decisions);
        assert_eq!(again.hero_avg_ev_loss_bb, report.hero_avg_ev_loss_bb);

        sqlx::query("DELETE FROM gg_tournaments WHERE id = ANY($1)")
            .bind(vec![
                format!("T_{hand_a}"),
                format!("T_{hand_b}"),
                format!("T_{hand_bad}"),
            ])
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn drill_template_saves_loads_and_clears() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        clear_template(&pool).await.unwrap();
        assert_eq!(load_template(&pool).await.unwrap(), None);

        save_template(&pool, "My Spin&Gold field", 0.62, 0.4, 132)
            .await
            .unwrap();
        let template = load_template(&pool).await.unwrap().unwrap();
        assert_eq!(template.label, "My Spin&Gold field");
        assert_eq!(template.skill, 0.62);
        assert_eq!(template.avg_ev_loss_bb, 0.4);
        assert_eq!(template.decisions, 132);

        // A second save upserts the single row.
        save_template(&pool, "Updated", 0.8, 0.2, 200)
            .await
            .unwrap();
        let updated = load_template(&pool).await.unwrap().unwrap();
        assert_eq!(updated.label, "Updated");
        assert_eq!(updated.decisions, 200);

        clear_template(&pool).await.unwrap();
        assert_eq!(load_template(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn hero_avg_loss_and_skill_read_the_decision_history() {
        use crate::game::Street;

        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        type LossRow = (f64, i64);
        let before: LossRow = sqlx::query_as(
            "SELECT COALESCE(SUM(ev_loss), 0), COUNT(*) FROM (
                 SELECT ev_loss FROM hero_decisions ORDER BY id DESC LIMIT $1
             ) recent",
        )
        .bind(crate::analytics::CHART_WINDOW as i64)
        .fetch_one(&pool)
        .await
        .unwrap();
        let session = crate::analytics::start_session(&pool).await.unwrap();
        crate::analytics::persist_records(
            &pool,
            session,
            &[
                crate::analytics::PendingDecision {
                    hand_no: 1,
                    street: Street::Preflop,
                    played: "Call".into(),
                    optimal: "Fold".into(),
                    ev_loss: 2.0,
                    ev_loss_pot: 2.0,
                },
                crate::analytics::PendingDecision {
                    hand_no: 1,
                    street: Street::Flop,
                    played: "Check".into(),
                    optimal: "Bet(40)".into(),
                    ev_loss: 0.5,
                    ev_loss_pot: 0.5,
                },
            ],
        )
        .await
        .unwrap();

        let avg = hero_avg_ev_loss(&pool).await.unwrap().unwrap();
        let expected = (before.0 + 2.5) / (before.1 + 2) as f64;
        assert!(
            (avg - expected).abs() < 1e-9,
            "windowed average includes the two new losses: {avg} vs {expected}"
        );
        let skill = hero_skill(&pool).await.unwrap().unwrap();
        assert!((skill - (1.0 - avg / SKILL_ZERO_LOSS_BB).clamp(0.0, 1.0)).abs() < 1e-9);

        sqlx::query("DELETE FROM hero_sessions WHERE id = $1")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tournament_ev_totals_aggregates_hero_and_opponent_separately() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let hand_a = unique("tournament_ev");
        let hand_b = unique("tournament_ev");
        insert_hand(&pool, &hand_a, SAMPLE_WIN).await;
        insert_hand(&pool, &hand_b, SAMPLE_BLUFF_WIN).await;
        // insert_hand puts every hand in its own tournament ("T_{hand_id}"),
        // so re-point hand_b at hand_a's tournament to exercise the SUM
        // across two hands of the same tournament.
        let tournament_a = format!("T_{hand_a}");
        sqlx::query("UPDATE gg_hands SET tournament_id = $1 WHERE hand_id = $2")
            .bind(&tournament_a)
            .bind(&hand_b)
            .execute(&pool)
            .await
            .unwrap();

        store_hand_score(
            &pool,
            &StoredHandScore {
                hand_id: hand_a.clone(),
                decisions: 5,
                ev_loss_bb_sum: 1.0,
                hero_decisions: 4,
                hero_ev_loss_bb_sum: 0.4,
            },
        )
        .await
        .unwrap();
        store_hand_score(
            &pool,
            &StoredHandScore {
                hand_id: hand_b.clone(),
                decisions: 3,
                ev_loss_bb_sum: 0.6,
                hero_decisions: 2,
                hero_ev_loss_bb_sum: 0.2,
            },
        )
        .await
        .unwrap();

        let totals = tournament_ev_totals(&pool).await.unwrap();
        let ev = totals
            .get(&tournament_a)
            .expect("the merged tournament has an entry");
        assert_eq!(ev.opponent_decisions, 8);
        assert!((ev.opponent_avg_ev_loss_bb - (1.6 / 8.0)).abs() < 1e-9);
        assert_eq!(ev.hero_decisions, 6);
        assert!((ev.hero_avg_ev_loss_bb - (0.6 / 6.0)).abs() < 1e-9);

        sqlx::query("DELETE FROM gg_tournaments WHERE id = ANY($1)")
            .bind(vec![tournament_a, format!("T_{hand_b}")])
            .execute(&pool)
            .await
            .unwrap();
    }
}
