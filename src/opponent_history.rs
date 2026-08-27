//! The opponent's historic action window: the trainer treats both bot seats
//! and the real imported field as one modeled "opponent" (the user's own
//! framing — "it is the same opponent, I'm just playing against two of the
//! same bot"). This module builds the window of that opponent's most recent
//! [`WINDOW`] actions — real imported hands (`gg_hands`) first, since those
//! only reveal hole cards at showdown, padded out with the trainer's own
//! locally-generated bot decisions (`local_opponent_actions`, where the
//! engine's true deal is always known) whenever `gg_hands` alone falls
//! short — and turns that window into two things:
//!
//! * a per-node [`Range`] prior (`contextual_ranges`), fed into the bots'
//!   own MCTS solve so they play the opponent's real tendencies instead of
//!   assuming a uniform range ([`load_range_model`]); and
//! * a plain-English historic read plus a starting-hand action-mix table for
//!   the coach panel ([`build_historic_read`], [`build_starting_hand_table`]).

use std::collections::HashMap;

use sqlx::PgPool;

use crate::card::Card;
use crate::db::{self, LocalOpponentAction, StoredRange};
use crate::error::Result;
use crate::game::{Action, GameState, Street};
use crate::range::hands::{HAND_COUNT, Hand, Range, all_hands};
use crate::range::sequence::{RangeResolver, SequenceNode, StackBucket, UniformPopulation};
use crate::range_cache::PgRangeStore;

/// How many of the most recent actions feed the window — `gg_hands` first,
/// padded by `local_opponent_actions`. Matches the sample size already used
/// for the drill field-skill grading window ([`crate::opponent_analysis::ANALYSIS_WINDOW`]).
pub const WINDOW: i64 = 1000;

/// The name of the single pooled opponent profile both bot seats (and the
/// imported field) are modeled as — there is only ever one row.
pub const POOLED_PROFILE_NAME: &str = "field";

/// A cell in the starting-hand table needs at least this many samples before
/// its action mix is shown as a real percentage rather than "too few hands".
pub const MIN_HAND_SAMPLES: u32 = 10;

// ------------------------------------------------------------------- node

/// A coarse decision-node bucket: street × whether a bet had to be faced.
/// Deliberately small — fine enough to be useful, coarse enough to stay
/// estimable from a ~1000-action window split across 169 hand classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    PfOpen,
    PfVsRaise,
    FlopLead,
    FlopVsBet,
    TurnLead,
    TurnVsBet,
    RiverLead,
    RiverVsBet,
}

impl Node {
    pub const ALL: [Node; 8] = [
        Node::PfOpen,
        Node::PfVsRaise,
        Node::FlopLead,
        Node::FlopVsBet,
        Node::TurnLead,
        Node::TurnVsBet,
        Node::RiverLead,
        Node::RiverVsBet,
    ];

    /// The stable string key stored in `contextual_ranges.node`.
    pub fn key(self) -> &'static str {
        match self {
            Node::PfOpen => "PF_OPEN",
            Node::PfVsRaise => "PF_VS_RAISE",
            Node::FlopLead => "FLOP_LEAD",
            Node::FlopVsBet => "FLOP_VS_BET",
            Node::TurnLead => "TURN_LEAD",
            Node::TurnVsBet => "TURN_VS_BET",
            Node::RiverLead => "RIVER_LEAD",
            Node::RiverVsBet => "RIVER_VS_BET",
        }
    }

    pub fn parse(key: &str) -> Option<Node> {
        Node::ALL.into_iter().find(|node| node.key() == key)
    }

    /// Whether this node is a preflop node — the starting-hand table only
    /// aggregates preflop decisions.
    pub fn is_preflop(self) -> bool {
        matches!(self, Node::PfOpen | Node::PfVsRaise)
    }
}

/// Classifies the state currently on the clock into its node bucket. Works
/// from either an unrotated live state (the acting seat is a bot) or a
/// walked/rotated historic state (the acting seat occupies the hero role) —
/// both expose [`GameState::legal_actions`] and [`GameState::current_bet`]
/// for whoever is actually on the clock.
pub fn decision_node(state: &GameState) -> Node {
    let facing_bet = !state.legal_actions().can_check;
    match state.street() {
        Street::Preflop => {
            // Preflop always has *something* to call (at minimum the big
            // blind) unless the action folded back to the big blind's
            // option, so "facing a bet" isn't `can_check` here — it's
            // whether anyone raised beyond the big blind itself.
            if state.current_bet() > state.blind_level().big_blind {
                Node::PfVsRaise
            } else {
                Node::PfOpen
            }
        }
        Street::Flop => {
            if facing_bet {
                Node::FlopVsBet
            } else {
                Node::FlopLead
            }
        }
        Street::Turn => {
            if facing_bet {
                Node::TurnVsBet
            } else {
                Node::TurnLead
            }
        }
        Street::River => {
            if facing_bet {
                Node::RiverVsBet
            } else {
                Node::RiverLead
            }
        }
    }
}

/// The stack bucket for whoever is currently on the clock.
pub fn decision_stack_bucket(state: &GameState) -> StackBucket {
    StackBucket::from_stack(state.stack(state.to_act()), state.blind_level().big_blind)
}

// --------------------------------------------------------------- category

/// A coarse action category: keeps the range/table aggregation simple
/// without needing exact bet sizing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionCategory {
    Fold,
    CallCheck,
    BetRaise,
}

impl ActionCategory {
    pub fn of(action: Action) -> ActionCategory {
        match action {
            Action::Fold => ActionCategory::Fold,
            Action::Check | Action::Call => ActionCategory::CallCheck,
            Action::Bet(_) | Action::Raise(_) | Action::AllIn => ActionCategory::BetRaise,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ActionCategory::Fold => "Fold",
            ActionCategory::CallCheck => "CallCheck",
            ActionCategory::BetRaise => "BetRaise",
        }
    }

    pub fn parse(text: &str) -> Option<ActionCategory> {
        match text {
            "Fold" => Some(ActionCategory::Fold),
            "CallCheck" => Some(ActionCategory::CallCheck),
            "BetRaise" => Some(ActionCategory::BetRaise),
            _ => None,
        }
    }
}

// -------------------------------------------------------------- the window

/// One opponent decision in the combined window: which hand class it was,
/// the node it occurred at, the actor's stack bucket, and the coarse action
/// taken.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistoricAction {
    pub hand: Hand,
    pub node: Node,
    pub stack_bucket: StackBucket,
    pub category: ActionCategory,
}

/// Splits `"As Kh"` into a classified [`Hand`], when both codes parse.
fn parse_hole_cards(text: &str) -> Option<Hand> {
    let mut codes = text.split_whitespace();
    let a = Card::from_code(codes.next()?)?;
    let b = Card::from_code(codes.next()?)?;
    Some(Hand::from_cards(a, b))
}

impl HistoricAction {
    fn from_local(row: LocalOpponentAction) -> Option<HistoricAction> {
        Some(HistoricAction {
            hand: parse_hole_cards(&row.hole_cards)?,
            node: Node::parse(&row.node)?,
            stack_bucket: StackBucket::from_bb(row.stack_bucket as u32),
            category: ActionCategory::parse(&row.action)?,
        })
    }
}

/// Walks the most recent imported hands into [`HistoricAction`]s, keeping
/// only decisions where the opponent's cards were revealed at showdown —
/// most decisions have no reveal (folds never show) and are skipped here,
/// which is expected: [`load_action_window`] pads the shortfall from local
/// play, which always has ground-truth cards.
async fn load_gg_actions(pool: &PgPool, limit: i64) -> Result<Vec<HistoricAction>> {
    let hands = crate::opponent_analysis::load_recent_hands(pool, limit).await?;
    let mut actions = Vec::new();
    for hand in &hands {
        let Some(episode) = crate::hh::parse_episode(&hand.raw) else {
            continue;
        };
        let Ok(walked) =
            crate::opponent_analysis::walk_hand(&episode, hand.sb, hand.bb, hand.hero_cards)
        else {
            continue;
        };
        for point in walked.opponent {
            let Some(hand_class) = point.opponent_hand else {
                continue;
            };
            actions.push(HistoricAction {
                hand: hand_class,
                node: decision_node(&point.state),
                stack_bucket: decision_stack_bucket(&point.state),
                category: ActionCategory::of(point.played),
            });
        }
    }
    Ok(actions)
}

/// Loads the combined "latest [`WINDOW`] actions" for the opponent: real
/// imported hands first, padded out with local bot decisions whenever the
/// real hands alone (hole cards known only when shown) don't reach the
/// window size.
pub async fn load_action_window(pool: &PgPool, limit: i64) -> Result<Vec<HistoricAction>> {
    let mut actions = load_gg_actions(pool, limit).await?;
    if actions.len() as i64 > limit {
        actions.truncate(limit as usize);
    }
    let shortfall = limit - actions.len() as i64;
    if shortfall > 0 {
        let local = db::load_recent_local_opponent_actions(pool, shortfall).await?;
        actions.extend(local.into_iter().filter_map(HistoricAction::from_local));
    }
    Ok(actions)
}

// ----------------------------------------------------------- range model

/// Tallies the window into a per-node, per-stack-bucket 169-hand range: the
/// share of times each hand class showed up acting at that spot. Nodes with
/// no samples are simply absent from the map.
pub fn build_range_model(actions: &[HistoricAction]) -> HashMap<(Node, StackBucket), StoredRange> {
    let mut counts: HashMap<(Node, StackBucket), [u32; HAND_COUNT]> = HashMap::new();
    for action in actions {
        let entry = counts
            .entry((action.node, action.stack_bucket))
            .or_insert([0u32; HAND_COUNT]);
        entry[action.hand.index()] += 1;
    }
    counts
        .into_iter()
        .map(|(key, hand_counts)| {
            let total: u32 = hand_counts.iter().sum();
            let mut weights: Range = [0.0f32; HAND_COUNT];
            if total > 0 {
                for (weight, count) in weights.iter_mut().zip(hand_counts.iter()) {
                    *weight = *count as f32 / total as f32;
                }
            }
            (
                key,
                StoredRange {
                    weights,
                    sample_count: total,
                },
            )
        })
        .collect()
}

/// Persists the range model under the single pooled opponent profile.
pub async fn save_range_model(
    pool: &PgPool,
    profile_id: i32,
    model: &HashMap<(Node, StackBucket), StoredRange>,
) -> Result<()> {
    for ((node, bucket), range) in model {
        db::upsert_contextual_range(pool, profile_id, node.key(), bucket.as_i16(), range).await?;
    }
    Ok(())
}

/// The resolved per-node ranges a session loads once at start, used as the
/// solver's prior for the opponent's likely holdings instead of assuming a
/// uniform range. Nodes below [`crate::range::MIN_SAMPLE_HANDS`] are simply
/// absent — [`Self::resolve`] returns `None` for those, and callers fall
/// back to the existing uniform behavior.
#[derive(Clone, Debug, Default)]
pub struct OpponentRangeModel {
    ranges: HashMap<(Node, StackBucket), Range>,
}

impl OpponentRangeModel {
    pub fn resolve(&self, node: Node, bucket: StackBucket) -> Option<Range> {
        self.ranges.get(&(node, bucket)).copied()
    }
}

/// Loads the resolved range model for the pooled opponent profile, applying
/// the existing minimum-sample gate ([`RangeResolver`]) node by node.
pub async fn load_range_model(pool: &PgPool, profile_id: i32) -> Result<OpponentRangeModel> {
    let resolver = RangeResolver::new(UniformPopulation);
    let store = PgRangeStore::new(pool.clone());
    let mut ranges = HashMap::new();
    for node in Node::ALL {
        for bucket in StackBucket::ALL {
            let sequence_node = SequenceNode::new(profile_id, bucket, node.key());
            let resolved = resolver.resolve(&store, &sequence_node).await?;
            if !resolved.used_population {
                ranges.insert((node, bucket), resolved.weights);
            }
        }
    }
    Ok(OpponentRangeModel { ranges })
}

// ----------------------------------------------------- starting-hand table

/// One row of the starting-hand table: the opponent's preflop action mix
/// with this hand class over the window.
#[derive(Clone, Debug, PartialEq)]
pub struct HandRow {
    pub label: String,
    pub row: usize,
    pub col: usize,
    pub samples: u32,
    pub fold_pct: Option<f64>,
    pub call_pct: Option<f64>,
    pub raise_pct: Option<f64>,
}

/// Builds the 169-row starting-hand action mix from the preflop decisions in
/// the window. Cells with fewer than [`MIN_HAND_SAMPLES`] carry `None`
/// percentages rather than a falsely precise number.
pub fn build_starting_hand_table(actions: &[HistoricAction]) -> Vec<HandRow> {
    let mut counts = [[0u32; 3]; HAND_COUNT];
    for action in actions {
        if !action.node.is_preflop() {
            continue;
        }
        let slot = match action.category {
            ActionCategory::Fold => 0,
            ActionCategory::CallCheck => 1,
            ActionCategory::BetRaise => 2,
        };
        counts[action.hand.index()][slot] += 1;
    }
    all_hands()
        .map(|hand| {
            let [fold, call, raise] = counts[hand.index()];
            let total = fold + call + raise;
            let (row, col) = hand.matrix_coords();
            let pct = |count: u32| {
                (total >= MIN_HAND_SAMPLES).then(|| count as f64 * 100.0 / total as f64)
            };
            HandRow {
                label: hand.label(),
                row,
                col,
                samples: total,
                fold_pct: pct(fold),
                call_pct: pct(call),
                raise_pct: pct(raise),
            }
        })
        .collect()
}

// ------------------------------------------------------------ historic read

/// The historic counterpart to [`crate::opponent::OpponentTracker`]'s live
/// read: a plain-English summary of the opponent's play over the window,
/// approximated from the flat action list (it can't replay per-hand VPIP the
/// way the live tracker does, since the window has no hand grouping) but
/// covering the same voluntary-preflop / raise / fold-to-bet / aggression
/// shape.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoricRead {
    pub actions: usize,
    pub voluntary_preflop_pct: f64,
    pub preflop_raise_pct: f64,
    pub fold_to_bet_pct: f64,
    pub aggression: Option<f64>,
    pub read: String,
}

/// Summarizes the window into a [`HistoricRead`].
pub fn build_historic_read(actions: &[HistoricAction]) -> HistoricRead {
    let mut preflop_total = 0u32;
    let mut preflop_voluntary = 0u32;
    let mut preflop_raise = 0u32;
    let mut faced_bet = 0u32;
    let mut folded_to_bet = 0u32;
    let mut postflop_bets = 0u32;
    let mut postflop_calls = 0u32;

    for action in actions {
        if action.node.is_preflop() {
            preflop_total += 1;
            if action.category != ActionCategory::Fold {
                preflop_voluntary += 1;
            }
            if action.category == ActionCategory::BetRaise {
                preflop_raise += 1;
            }
        } else {
            match action.category {
                ActionCategory::BetRaise => postflop_bets += 1,
                ActionCategory::CallCheck
                    if matches!(
                        action.node,
                        Node::FlopVsBet | Node::TurnVsBet | Node::RiverVsBet
                    ) =>
                {
                    postflop_calls += 1
                }
                _ => {}
            }
        }
        let node_faces_bet = matches!(
            action.node,
            Node::PfVsRaise | Node::FlopVsBet | Node::TurnVsBet | Node::RiverVsBet
        );
        if node_faces_bet {
            faced_bet += 1;
            if action.category == ActionCategory::Fold {
                folded_to_bet += 1;
            }
        }
    }

    let pct = |num: u32, den: u32| {
        if den == 0 {
            0.0
        } else {
            num as f64 * 100.0 / den as f64
        }
    };
    let voluntary_preflop_pct = pct(preflop_voluntary, preflop_total);
    let preflop_raise_pct = pct(preflop_raise, preflop_total);
    let fold_to_bet_pct = pct(folded_to_bet, faced_bet);
    let aggression = match (postflop_bets, postflop_calls) {
        (0, 0) => None,
        (_, 0) => Some(f64::INFINITY),
        (bets, calls) => Some(bets as f64 / calls as f64),
    };

    HistoricRead {
        actions: actions.len(),
        voluntary_preflop_pct,
        preflop_raise_pct,
        fold_to_bet_pct,
        aggression,
        read: crate::opponent::read(actions.len(), voluntary_preflop_pct, aggression),
    }
}

// ---------------------------------------------------------------- the job

/// Everything the coach panel and the live solver need from one refresh:
/// the window size actually gathered, the historic read, and the
/// starting-hand table. The range model itself is saved to the database and
/// loaded separately per session ([`load_range_model`]).
#[derive(Clone, Debug, PartialEq)]
pub struct HistorySummary {
    pub window_actions: usize,
    pub read: HistoricRead,
    pub table: Vec<HandRow>,
}

impl Default for HistorySummary {
    /// The empty window: no history gathered yet (no pool, or nothing to
    /// gather from). Every table cell stays ungraded rather than showing a
    /// false zero.
    fn default() -> Self {
        HistorySummary {
            window_actions: 0,
            read: build_historic_read(&[]),
            table: build_starting_hand_table(&[]),
        }
    }
}

/// Everything a session loads once at start about "the opponent": the
/// resolved per-node range priors for the bots' own solve, and the historic
/// read plus starting-hand table for the coach panel. Bundled together since
/// both come from the same window in one pass ([`refresh`] plus
/// [`load_range_model`]).
#[derive(Clone, Debug, Default)]
pub struct OpponentModel {
    pub ranges: OpponentRangeModel,
    pub historic: HistorySummary,
}

/// Rebuilds the opponent history window, saves the refreshed range model
/// under the pooled profile, and returns the coach-panel summary. Call this
/// after importing new `gg_hands`, and periodically as local play
/// accumulates.
pub async fn refresh(pool: &PgPool) -> Result<HistorySummary> {
    let actions = load_action_window(pool, WINDOW).await?;
    let profile_id = db::upsert_opponent_profile(pool, POOLED_PROFILE_NAME, "FIELD").await?;
    let model = build_range_model(&actions);
    save_range_model(pool, profile_id, &model).await?;
    Ok(HistorySummary {
        window_actions: actions.len(),
        read: build_historic_read(&actions),
        table: build_starting_hand_table(&actions),
    })
}

/// The pooled opponent profile's id, creating the row if it doesn't exist
/// yet. Cheap to call per session — `upsert_opponent_profile` is a single
/// indexed upsert.
pub async fn pooled_profile_id(pool: &PgPool) -> Result<i32> {
    db::upsert_opponent_profile(pool, POOLED_PROFILE_NAME, "FIELD").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Rank;
    use crate::range::hands::Hand as RangeHand;

    // ------------------------------------------------------------ samples
    //
    // The same real GGPoker hand blocks used in opponent_analysis's tests
    // (a showdown win and a bluff win) — reused here to exercise
    // `decision_node` against every node it can classify, walked through the
    // real replayer rather than hand-built states.

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

    fn nodes_for(raw: &str, sb: i32, bb: i32) -> Vec<(Node, ActionCategory)> {
        let episode = crate::hh::parse_episode(raw).expect("sample parses");
        let points = crate::opponent_analysis::walk_hand(&episode, sb, bb, None)
            .expect("sample walks")
            .opponent;
        points
            .iter()
            .map(|point| {
                (
                    decision_node(&point.state),
                    ActionCategory::of(point.played),
                )
            })
            .collect()
    }

    #[test]
    fn decision_node_classifies_the_full_showdown_hand() {
        assert_eq!(
            nodes_for(SAMPLE_WIN, 20, 40),
            vec![
                (Node::PfOpen, ActionCategory::CallCheck),
                (Node::PfVsRaise, ActionCategory::CallCheck),
                (Node::FlopVsBet, ActionCategory::CallCheck),
                (Node::TurnVsBet, ActionCategory::CallCheck),
                (Node::RiverVsBet, ActionCategory::BetRaise),
            ],
            "SB calling the open BB is PF_OPEN (no raise yet); calling the \
             raise, and every later street facing a bet, all classify as \
             the *_VS_* node"
        );
    }

    #[test]
    fn decision_node_classifies_leads_and_a_river_fold() {
        assert_eq!(
            nodes_for(SAMPLE_BLUFF_WIN, 10, 20),
            vec![
                // Hero (SB) opens to 40 over the 20 big blind, so the BB's
                // preflop call already faces a raise, not an open.
                (Node::PfVsRaise, ActionCategory::CallCheck),
                (Node::FlopLead, ActionCategory::CallCheck),
                (Node::FlopVsBet, ActionCategory::CallCheck),
                (Node::TurnLead, ActionCategory::BetRaise),
                (Node::RiverLead, ActionCategory::CallCheck),
                (Node::RiverVsBet, ActionCategory::Fold),
            ],
            "checking or betting first (no bet to call) is the *_LEAD node; \
             folding to the hero's river shove is RIVER_VS_BET"
        );
    }

    // ---------------------------------------------------------------- node

    #[test]
    fn node_key_round_trips_for_every_variant() {
        for node in Node::ALL {
            assert_eq!(Node::parse(node.key()), Some(node));
        }
        assert_eq!(Node::parse("NOT_A_NODE"), None);
    }

    #[test]
    fn only_preflop_nodes_report_is_preflop() {
        assert!(Node::PfOpen.is_preflop());
        assert!(Node::PfVsRaise.is_preflop());
        for node in [
            Node::FlopLead,
            Node::FlopVsBet,
            Node::TurnLead,
            Node::TurnVsBet,
            Node::RiverLead,
            Node::RiverVsBet,
        ] {
            assert!(!node.is_preflop());
        }
    }

    // ------------------------------------------------------------ category

    #[test]
    fn action_category_covers_every_action_kind() {
        assert_eq!(ActionCategory::of(Action::Fold), ActionCategory::Fold);
        assert_eq!(ActionCategory::of(Action::Check), ActionCategory::CallCheck);
        assert_eq!(ActionCategory::of(Action::Call), ActionCategory::CallCheck);
        assert_eq!(
            ActionCategory::of(Action::Bet(40)),
            ActionCategory::BetRaise
        );
        assert_eq!(
            ActionCategory::of(Action::Raise(80)),
            ActionCategory::BetRaise
        );
        assert_eq!(ActionCategory::of(Action::AllIn), ActionCategory::BetRaise);
    }

    #[test]
    fn action_category_label_round_trips() {
        for category in [
            ActionCategory::Fold,
            ActionCategory::CallCheck,
            ActionCategory::BetRaise,
        ] {
            assert_eq!(ActionCategory::parse(category.label()), Some(category));
        }
        assert_eq!(ActionCategory::parse("Whatever"), None);
    }

    // -------------------------------------------------------------- window

    fn hand(high: Rank, low: Rank, suited: bool) -> RangeHand {
        RangeHand::new(high, low, suited)
    }

    fn action(
        hand: RangeHand,
        node: Node,
        bucket: StackBucket,
        category: ActionCategory,
    ) -> HistoricAction {
        HistoricAction {
            hand,
            node,
            stack_bucket: bucket,
            category,
        }
    }

    #[test]
    fn parse_hole_cards_reads_pokercraft_codes() {
        assert_eq!(
            parse_hole_cards("As Kh"),
            Some(hand(Rank::Ace, Rank::King, false))
        );
        assert_eq!(
            parse_hole_cards("Qc Qd"),
            Some(hand(Rank::Queen, Rank::Queen, false))
        );
        assert_eq!(parse_hole_cards("nonsense"), None);
        assert_eq!(parse_hole_cards("As"), None);
    }

    #[test]
    fn local_action_round_trips_through_historic_action() {
        let row = LocalOpponentAction {
            node: Node::FlopVsBet.key().to_string(),
            stack_bucket: StackBucket::Bb15.as_i16(),
            hole_cards: "Qc Qd".to_string(),
            action: ActionCategory::BetRaise.label().to_string(),
        };
        let historic = HistoricAction::from_local(row).expect("well-formed row parses");
        assert_eq!(historic.hand, hand(Rank::Queen, Rank::Queen, false));
        assert_eq!(historic.node, Node::FlopVsBet);
        assert_eq!(historic.stack_bucket, StackBucket::Bb15);
        assert_eq!(historic.category, ActionCategory::BetRaise);
    }

    #[test]
    fn local_action_with_an_unknown_node_or_action_does_not_parse() {
        let bad_node = LocalOpponentAction {
            node: "MADE_UP".to_string(),
            stack_bucket: 15,
            hole_cards: "Qc Qd".to_string(),
            action: "BetRaise".to_string(),
        };
        assert_eq!(HistoricAction::from_local(bad_node), None);

        let bad_cards = LocalOpponentAction {
            node: "FLOP_LEAD".to_string(),
            stack_bucket: 15,
            hole_cards: "garbage".to_string(),
            action: "Fold".to_string(),
        };
        assert_eq!(HistoricAction::from_local(bad_cards), None);
    }

    // ------------------------------------------------------------- range

    #[test]
    fn build_range_model_normalizes_counts_per_node_and_bucket() {
        let aa = hand(Rank::Ace, Rank::Ace, false);
        let kk = hand(Rank::King, Rank::King, false);
        let actions = vec![
            action(
                aa,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::BetRaise,
            ),
            action(
                aa,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::BetRaise,
            ),
            action(
                kk,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::BetRaise,
            ),
            // A different node/bucket must not mix into the first bucket.
            action(
                aa,
                Node::PfOpen,
                StackBucket::Bb10,
                ActionCategory::BetRaise,
            ),
        ];
        let model = build_range_model(&actions);
        let main = &model[&(Node::PfOpen, StackBucket::Bb25)];
        assert_eq!(main.sample_count, 3);
        assert!((main.weights[aa.index()] - 2.0 / 3.0).abs() < 1e-6);
        assert!((main.weights[kk.index()] - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(main.weights.iter().sum::<f32>(), 1.0);

        let other_bucket = &model[&(Node::PfOpen, StackBucket::Bb10)];
        assert_eq!(other_bucket.sample_count, 1);
        assert_eq!(other_bucket.weights[aa.index()], 1.0);
    }

    #[test]
    fn build_range_model_is_empty_for_an_empty_window() {
        assert!(build_range_model(&[]).is_empty());
    }

    // ------------------------------------------------------- hand table

    #[test]
    fn starting_hand_table_gates_low_sample_cells() {
        let aa = hand(Rank::Ace, Rank::Ace, false);
        let seven_two = hand(Rank::Seven, Rank::Two, false);
        let mut actions: Vec<HistoricAction> = (0..12)
            .map(|_| {
                action(
                    aa,
                    Node::PfOpen,
                    StackBucket::Bb25,
                    ActionCategory::BetRaise,
                )
            })
            .collect();
        // Below MIN_HAND_SAMPLES: stays ungraded.
        actions.push(action(
            seven_two,
            Node::PfVsRaise,
            StackBucket::Bb25,
            ActionCategory::Fold,
        ));
        // Postflop decisions never feed the preflop table.
        actions.push(action(
            aa,
            Node::FlopVsBet,
            StackBucket::Bb25,
            ActionCategory::Fold,
        ));

        let table = build_starting_hand_table(&actions);
        let aa_row = table.iter().find(|row| row.label == "AA").unwrap();
        assert_eq!(aa_row.samples, 12, "the flop decision does not count");
        assert_eq!(aa_row.raise_pct, Some(100.0));
        assert_eq!(aa_row.fold_pct, Some(0.0));

        let seven_two_row = table.iter().find(|row| row.label == "72o").unwrap();
        assert_eq!(seven_two_row.samples, 1);
        assert_eq!(
            seven_two_row.fold_pct, None,
            "below MIN_HAND_SAMPLES, cells stay ungraded rather than showing false precision"
        );
    }

    #[test]
    fn starting_hand_table_covers_every_hand_class_even_unseen() {
        let table = build_starting_hand_table(&[]);
        assert_eq!(table.len(), HAND_COUNT);
        assert!(
            table
                .iter()
                .all(|row| row.samples == 0 && row.fold_pct.is_none())
        );
    }

    // ------------------------------------------------------------ read

    #[test]
    fn historic_read_computes_voluntary_raise_and_fold_to_bet_rates() {
        let aa = hand(Rank::Ace, Rank::Ace, false);
        let seven_two = hand(Rank::Seven, Rank::Two, false);
        let actions = vec![
            // Preflop: 2 voluntary (1 raise), 1 fold.
            action(
                aa,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::BetRaise,
            ),
            action(
                aa,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::CallCheck,
            ),
            action(
                seven_two,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::Fold,
            ),
            // Facing a raise: one fold out of two.
            action(
                aa,
                Node::PfVsRaise,
                StackBucket::Bb25,
                ActionCategory::CallCheck,
            ),
            action(
                seven_two,
                Node::PfVsRaise,
                StackBucket::Bb25,
                ActionCategory::Fold,
            ),
            // Postflop aggression: one bet, one call.
            action(
                aa,
                Node::FlopLead,
                StackBucket::Bb25,
                ActionCategory::BetRaise,
            ),
            action(
                aa,
                Node::FlopVsBet,
                StackBucket::Bb25,
                ActionCategory::CallCheck,
            ),
        ];
        let read = build_historic_read(&actions);
        assert_eq!(read.actions, 7);
        // Preflop: 5 preflop decisions total (PfOpen x3 + PfVsRaise x2), 3 voluntary.
        assert!((read.voluntary_preflop_pct - 60.0).abs() < 1e-9);
        assert!((read.preflop_raise_pct - 20.0).abs() < 1e-9);
        // Faced a bet three times (PfVsRaise x2, FlopVsBet x1), folded once.
        assert!((read.fold_to_bet_pct - 100.0 / 3.0).abs() < 1e-9);
        assert_eq!(read.aggression, Some(1.0));
        assert!(!read.read.is_empty());
    }

    #[test]
    fn historic_read_handles_an_empty_window() {
        let read = build_historic_read(&[]);
        assert_eq!(read.actions, 0);
        assert_eq!(read.voluntary_preflop_pct, 0.0);
        assert_eq!(read.aggression, None);
        assert_eq!(read.read, "No hands played yet.");
    }

    // --------------------------------------------------------- database

    async fn test_pool() -> sqlx::PgPool {
        crate::db::test_pool().await
    }

    #[tokio::test]
    async fn local_opponent_actions_round_trip_and_fill_the_shortfall() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        let before = db::load_recent_local_opponent_actions(&pool, 5)
            .await
            .unwrap();
        let rows = vec![
            LocalOpponentAction {
                node: Node::PfOpen.key().to_string(),
                stack_bucket: StackBucket::Bb25.as_i16(),
                hole_cards: "As Ks".to_string(),
                action: ActionCategory::BetRaise.label().to_string(),
            },
            LocalOpponentAction {
                node: Node::FlopVsBet.key().to_string(),
                stack_bucket: StackBucket::Bb10.as_i16(),
                hole_cards: "7c 2d".to_string(),
                action: ActionCategory::Fold.label().to_string(),
            },
        ];
        db::insert_local_opponent_actions(&pool, &rows)
            .await
            .unwrap();

        let after = db::load_recent_local_opponent_actions(&pool, (before.len() + 2) as i64)
            .await
            .unwrap();
        assert_eq!(after.len(), before.len() + 2);
        // Newest first: the two just-inserted rows lead, most-recent last-in first-out.
        assert_eq!(after[0].hole_cards, "7c 2d");
        assert_eq!(after[1].hole_cards, "As Ks");

        let limited = db::load_recent_local_opponent_actions(&pool, 0)
            .await
            .unwrap();
        assert!(limited.is_empty());
    }

    #[tokio::test]
    async fn range_model_saves_and_loads_through_the_pooled_profile() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let name = format!(
            "test_field_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let profile_id = db::upsert_opponent_profile(&pool, &name, "FIELD")
            .await
            .unwrap();

        // Below MIN_SAMPLE_HANDS: falls back to the uniform population, so
        // the resolved model carries nothing for this node.
        let aa = crate::range::hands::Hand::new(Rank::Ace, Rank::Ace, false);
        let sparse = vec![action(
            aa,
            Node::RiverLead,
            StackBucket::Bb25,
            ActionCategory::BetRaise,
        )];
        let model = build_range_model(&sparse);
        save_range_model(&pool, profile_id, &model).await.unwrap();
        let resolved = load_range_model(&pool, profile_id).await.unwrap();
        assert_eq!(
            resolved.resolve(Node::RiverLead, StackBucket::Bb25),
            None,
            "a single sample stays below MIN_SAMPLE_HANDS and falls back to uniform"
        );

        // At/above MIN_SAMPLE_HANDS: the resolved range is the trained one.
        let dense: Vec<HistoricAction> = (0..40)
            .map(|_| action(aa, Node::TurnVsBet, StackBucket::Bb15, ActionCategory::Fold))
            .collect();
        let model = build_range_model(&dense);
        save_range_model(&pool, profile_id, &model).await.unwrap();
        let resolved = load_range_model(&pool, profile_id).await.unwrap();
        let range = resolved
            .resolve(Node::TurnVsBet, StackBucket::Bb15)
            .expect("40 samples clears MIN_SAMPLE_HANDS");
        assert_eq!(range[aa.index()], 1.0);

        sqlx::query("DELETE FROM contextual_ranges WHERE profile_id = $1")
            .bind(profile_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM opponent_profiles WHERE id = $1")
            .bind(profile_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_completes_against_an_empty_database_state() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        // No gg_hands and no local actions is a valid (if uninformative)
        // starting state — the job must not error, just report nothing.
        let summary = refresh(&pool).await.unwrap();
        assert_eq!(summary.table.len(), HAND_COUNT);
        assert!(summary.read.actions <= summary.window_actions);
    }
}
