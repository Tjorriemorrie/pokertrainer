//! The opponent's historic action window: the trainer treats both bot seats
//! and the real imported field as one modeled "opponent" (the user's own
//! framing — "it is the same opponent, I'm just playing against two of the
//! same bot"). This module builds the window of that opponent's most recent
//! [`HISTORY_WINDOW`] hands — real imported hands (`gg_hands`) first, since
//! those only reveal hole cards at showdown, combined with every
//! locally-generated bot decision (`local_opponent_actions`, where the
//! engine's true deal is always known) — and turns that window into two
//! things:
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

/// How many of the most recent imported hands feed the window — every
/// preflop-or-later decision from `gg_hands` combined with every row in
/// `local_opponent_actions`/`local_hero_actions` (those are already
/// individual actions, not hands, so this cap only bounds the `gg_hands`
/// side; local play is never large enough for that to matter in practice).
/// Large enough that in practice it just means "everything gathered so
/// far" — the starting-hand grid needs every sample it can get across 169
/// hand classes, unlike the drill field-skill grading window
/// ([`crate::opponent_analysis::ANALYSIS_WINDOW`]), which deliberately
/// stays small to track *recent* skill.
pub const HISTORY_WINDOW: i64 = 10_000;

/// The name of the single pooled opponent profile both bot seats (and the
/// imported field) are modeled as — there is only ever one row.
pub const POOLED_PROFILE_NAME: &str = "field";

/// A cell in the starting-hand table needs at least this many samples before
/// its action mix is shown at all — below this, a real percentage would be
/// too noisy to mean anything (a single fold reads as "folds 100%"). Once
/// graded, the cell's color still fades in with sample count rather than
/// snapping straight to full confidence at the threshold — see
/// `crate::server::views::RangeTableFragment::new`.
pub const MIN_HAND_SAMPLES: u32 = 1;

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

// --------------------------------------------------------------- position

/// A seat's position in this 3-max format: in Spin & Gold the button posts
/// the small blind, so there are only three roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Position {
    Button,
    BigBlind,
    /// Neither the button/small-blind nor the big blind — the third seat,
    /// first to act preflop. Never produced heads-up (that seat is
    /// eliminated and never on the clock).
    Third,
}

impl Position {
    pub const ALL: [Position; 3] = [Position::Button, Position::BigBlind, Position::Third];

    /// The stable string key stored alongside `node`/`stack_bucket`.
    pub fn key(self) -> &'static str {
        match self {
            Position::Button => "BUTTON",
            Position::BigBlind => "BIG_BLIND",
            Position::Third => "THIRD",
        }
    }

    pub fn parse(key: &str) -> Option<Position> {
        Position::ALL.into_iter().find(|pos| pos.key() == key)
    }
}

/// The position of whoever is currently on the clock, derived the same way
/// [`decision_node`]/[`decision_stack_bucket`] are — works on either an
/// unrotated live state or a walked/rotated historic state.
pub fn decision_position(state: &GameState) -> Position {
    let actor = state.to_act();
    if actor == state.button() {
        Position::Button
    } else if actor == state.big_blind_seat() {
        Position::BigBlind
    } else {
        Position::Third
    }
}

// --------------------------------------------------------------- category

/// A coarse action category: keeps the range/table aggregation simple
/// without needing exact bet sizing. `Shove` is split out from `BetRaise` so
/// the action-frequency model can tell a real all-in tendency apart from an
/// ordinary raise — the two read very differently to a real opponent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionCategory {
    Fold,
    CallCheck,
    BetRaise,
    Shove,
}

impl ActionCategory {
    pub fn of(action: Action) -> ActionCategory {
        match action {
            Action::Fold => ActionCategory::Fold,
            Action::Check | Action::Call => ActionCategory::CallCheck,
            Action::Bet(_) | Action::Raise(_) => ActionCategory::BetRaise,
            Action::AllIn => ActionCategory::Shove,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ActionCategory::Fold => "Fold",
            ActionCategory::CallCheck => "CallCheck",
            ActionCategory::BetRaise => "BetRaise",
            ActionCategory::Shove => "Shove",
        }
    }

    pub fn parse(text: &str) -> Option<ActionCategory> {
        match text {
            "Fold" => Some(ActionCategory::Fold),
            "CallCheck" => Some(ActionCategory::CallCheck),
            "BetRaise" => Some(ActionCategory::BetRaise),
            "Shove" => Some(ActionCategory::Shove),
            _ => None,
        }
    }
}

// ------------------------------------------------------------- aggressor

/// Whether a flop decision is a c-bet opportunity (leading with no bet
/// faced, as the hand's preflop aggressor) or a fold-to-c-bet opportunity
/// (facing a bet from that same aggressor) — `NotApplicable` everywhere
/// else, including flop decisions that don't meet either condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AggressorContext {
    NotApplicable,
    Aggressor,
    NotAggressor,
}

impl AggressorContext {
    pub const ALL: [AggressorContext; 3] = [
        AggressorContext::NotApplicable,
        AggressorContext::Aggressor,
        AggressorContext::NotAggressor,
    ];

    pub fn key(self) -> &'static str {
        match self {
            AggressorContext::NotApplicable => "NOT_APPLICABLE",
            AggressorContext::Aggressor => "AGGRESSOR",
            AggressorContext::NotAggressor => "NOT_AGGRESSOR",
        }
    }

    pub fn parse(key: &str) -> Option<AggressorContext> {
        AggressorContext::ALL.into_iter().find(|ctx| ctx.key() == key)
    }
}

/// Derives the aggressor context for one decision: only `FlopLead` (a c-bet
/// opportunity, gated on `was_preflop_aggressor`) and `FlopVsBet` (a
/// fold-to-c-bet opportunity, gated on `facing_cbet`) ever produce anything
/// other than `NotApplicable`.
pub fn aggressor_context(node: Node, was_preflop_aggressor: bool, facing_cbet: bool) -> AggressorContext {
    match node {
        Node::FlopLead => {
            if was_preflop_aggressor {
                AggressorContext::Aggressor
            } else {
                AggressorContext::NotAggressor
            }
        }
        Node::FlopVsBet => {
            if facing_cbet {
                AggressorContext::Aggressor
            } else {
                AggressorContext::NotAggressor
            }
        }
        _ => AggressorContext::NotApplicable,
    }
}

// -------------------------------------------------------------- the window

/// One opponent decision in the combined window: which hand class it was,
/// the node it occurred at, the actor's stack bucket and position, the
/// coarse action taken, and the two booleans that let a flop decision be
/// read as a c-bet or fold-to-c-bet opportunity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistoricAction {
    pub hand: Hand,
    pub node: Node,
    pub stack_bucket: StackBucket,
    pub position: Position,
    pub category: ActionCategory,
    pub was_preflop_aggressor: bool,
    pub facing_cbet: bool,
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
            position: Position::parse(&row.position)?,
            category: ActionCategory::parse(&row.action)?,
            was_preflop_aggressor: row.was_preflop_aggressor,
            facing_cbet: row.facing_cbet,
        })
    }

    fn from_local_hero(row: crate::db::LocalHeroAction) -> Option<HistoricAction> {
        Some(HistoricAction {
            hand: parse_hole_cards(&row.hole_cards)?,
            node: Node::parse(&row.node)?,
            stack_bucket: StackBucket::from_bb(row.stack_bucket as u32),
            position: Position::parse(&row.position)?,
            category: ActionCategory::parse(&row.action)?,
            was_preflop_aggressor: row.was_preflop_aggressor,
            facing_cbet: row.facing_cbet,
        })
    }
}

/// Walks the most recent imported hands into [`HistoricAction`]s, keeping
/// only decisions where the opponent's cards were revealed at showdown —
/// most decisions have no reveal (folds never show) and are skipped here,
/// which is expected: [`load_action_window`] always adds local play (which
/// has ground-truth cards) on top. Also returns how many hands were looked
/// at, for the window's "how many hands is this built from" count.
async fn load_gg_actions(pool: &PgPool, limit: i64) -> Result<(Vec<HistoricAction>, usize)> {
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
                position: decision_position(&point.state),
                category: ActionCategory::of(point.played),
                was_preflop_aggressor: point.was_preflop_aggressor,
                facing_cbet: point.facing_cbet,
            });
        }
    }
    Ok((actions, hands.len()))
}

/// Loads the combined window for the opponent: every decision from the
/// `hand_limit` most recent imported hands, plus every locally-recorded bot
/// decision — real imported hands don't reveal an opponent's hole cards
/// unless the hand went to showdown, so local play (the engine's true deal
/// is always known there) is never just a fallback-when-thin, it's added in
/// full every time. The second element is how many distinct hands (imported
/// plus local) the window actually covers.
pub async fn load_action_window(
    pool: &PgPool,
    hand_limit: i64,
) -> Result<(Vec<HistoricAction>, usize)> {
    let (mut actions, gg_hands) = load_gg_actions(pool, hand_limit).await?;
    let local = db::load_recent_local_opponent_actions(pool, HISTORY_WINDOW).await?;
    let local_hands: std::collections::HashSet<i64> =
        local.iter().map(|row| row.hand_no).collect();
    let window_hands = gg_hands + local_hands.len();
    actions.extend(local.into_iter().filter_map(HistoricAction::from_local));
    Ok((actions, window_hands))
}

/// Walks the most recent imported hands into the *hero's own* [`HistoricAction`]s.
/// Unlike the opponent's window, the hero's hand is always known (it isn't a
/// showdown reveal) — hands with no recorded `hero_cards` are skipped
/// entirely, since [`crate::opponent_analysis::walk_hand`] falls back to a
/// placeholder deal when it has no real hero cards to seed the walk with.
async fn load_hero_gg_actions(pool: &PgPool, limit: i64) -> Result<(Vec<HistoricAction>, usize)> {
    let hands = crate::opponent_analysis::load_recent_hands(pool, limit).await?;
    let mut actions = Vec::new();
    for hand in &hands {
        let Some(hero_cards) = hand.hero_cards else {
            continue;
        };
        let Some(episode) = crate::hh::parse_episode(&hand.raw) else {
            continue;
        };
        let Ok(walked) =
            crate::opponent_analysis::walk_hand(&episode, hand.sb, hand.bb, Some(hero_cards))
        else {
            continue;
        };
        let hand_class = Hand::from_cards(hero_cards[0], hero_cards[1]);
        for point in walked.hero {
            actions.push(HistoricAction {
                hand: hand_class,
                node: decision_node(&point.state),
                stack_bucket: decision_stack_bucket(&point.state),
                position: decision_position(&point.state),
                category: ActionCategory::of(point.played),
                was_preflop_aggressor: point.was_preflop_aggressor,
                facing_cbet: point.facing_cbet,
            });
        }
    }
    Ok((actions, hands.len()))
}

/// Loads the combined window for the hero: every decision from the
/// `hand_limit` most recent imported hands, plus every locally-recorded hero
/// decision. Mirrors [`load_action_window`].
pub async fn load_hero_action_window(
    pool: &PgPool,
    hand_limit: i64,
) -> Result<(Vec<HistoricAction>, usize)> {
    let (mut actions, gg_hands) = load_hero_gg_actions(pool, hand_limit).await?;
    let local = db::load_recent_local_hero_actions(pool, HISTORY_WINDOW).await?;
    let local_hands: std::collections::HashSet<i64> =
        local.iter().map(|row| row.hand_no).collect();
    let window_hands = gg_hands + local_hands.len();
    actions.extend(local.into_iter().filter_map(HistoricAction::from_local_hero));
    Ok((actions, window_hands))
}

// ----------------------------------------------------------- range model

/// Total Laplace pseudo-count blended across all 169 hand classes before
/// normalizing a range, distributed according to [`chen_prior`] rather than
/// split evenly (i.e. class `i` gets `RANGE_SMOOTHING_TOTAL * chen_prior()[i]`
/// pseudo-observations, not `RANGE_SMOOTHING_TOTAL / HAND_COUNT` each).
/// [`MIN_SAMPLE_HANDS`] only gates *whether* a node's range is trusted at
/// all — it doesn't stop a barely-passing sample (e.g. 30-70 real actions
/// spread across all 169 hand classes) from leaving most classes at a
/// literal zero count. Solved against a range with hard zeros, the MCTS
/// treats every unseen class — routinely including premium hands like
/// AA/KK/AKs that simply haven't come up yet in a small window — as
/// *impossible* for the opponent to hold, which inflates a bluff/thin-value
/// raise's apparent fold equity far beyond what the real (unobserved) tail
/// of the opponent's range would allow. Smoothing keeps every class
/// reachable, shrinking hard toward uniform while the sample is this small
/// and easing off as real observations accumulate.
///
/// The pseudo-count must total to *far less* than one hand's worth of pull
/// per class — spreading a full pseudo-observation over every one of the
/// 169 classes (the previous per-class `RANGE_SMOOTHING = 1.0`) adds up to
/// 169 pseudo-observations, which dwarfs the real signal in any
/// locally-trained window (tens to low hundreds of samples) and leaves
/// every learned range reading as barely-distinguishable-from-uniform —
/// exactly the failure this smoothing exists to prevent, just reached from
/// the opposite direction.
const RANGE_SMOOTHING_TOTAL: f32 = 8.0;

/// The free-text `contextual_ranges.node` key for
/// [`build_preflop_raise_range_model`] — not a member of [`Node`] (that enum
/// classifies which decision *node* a player is at) but persisted through
/// the same table/[`RangeResolver`]/[`SequenceNode`] machinery, which keys
/// purely on strings.
const PF_RAISER_NODE_KEY: &str = "PF_RAISER";

/// A fixed, non-uniform prior over the 169 hand classes — proportional to
/// each class's [`Hand::chen_score`] (+1 so the very worst class still
/// keeps a small nonzero share) — used to smooth [`build_range_model`] and
/// [`build_preflop_raise_range_model`] instead of a flat/uniform prior.
///
/// Regression for the "raise 4h8d into a real raise, call 70 more" coaching
/// complaint: a *flat* pseudo-count still shrinks a thin sample toward
/// "every hand equally likely", which a 169-way empirical distribution can
/// never actually distinguish from noise at the sample sizes one local
/// session accumulates (69 raises spread across 169 classes is under half
/// an observation per class) — so even the raiser-specific range kept
/// reading as barely-distinguishable-from-uniform, with premiums like AKo
/// not even cracking the top 15 while things like K7o and J4o did. Shrinking
/// toward "preflop raises skew toward higher Chen-score hands" instead is
/// the actually-informative prior a thin sample should fall back to.
fn chen_prior() -> [f32; HAND_COUNT] {
    let mut weights = [0.0f32; HAND_COUNT];
    for hand in all_hands() {
        weights[hand.index()] = hand.chen_score() as f32 + 1.0;
    }
    let total: f32 = weights.iter().sum();
    for weight in &mut weights {
        *weight /= total;
    }
    weights
}

/// Laplace-smooths a per-hand-class tally into a normalized 169-hand range
/// plus its raw sample count — the shared math behind
/// [`build_range_model`] and [`build_preflop_raise_range_model`]. Shrinks
/// toward [`chen_prior`] rather than a flat/uniform prior.
fn smoothed_range(hand_counts: &[u32; HAND_COUNT]) -> StoredRange {
    let total: u32 = hand_counts.iter().sum();
    let prior = chen_prior();
    let denom = total as f32 + RANGE_SMOOTHING_TOTAL;
    let mut weights: Range = [0.0f32; HAND_COUNT];
    for (index, (weight, count)) in weights.iter_mut().zip(hand_counts.iter()).enumerate() {
        *weight = (*count as f32 + RANGE_SMOOTHING_TOTAL * prior[index]) / denom;
    }
    StoredRange {
        weights,
        sample_count: total,
    }
}

/// Tallies the window into a per-node, per-stack-bucket 169-hand range: the
/// share of times each hand class showed up acting at that spot, Laplace-
/// smoothed (see [`RANGE_SMOOTHING_TOTAL`]) so a thin sample never rules a hand
/// class out entirely. Nodes with no samples are simply absent from the map.
///
/// This pools every action taken at the node together (fold, call, and
/// raise alike), so it approximates "whoever hasn't decided yet at this
/// spot" reasonably well — it is not the range of a seat that has *already*
/// acted here. A seat that has voluntarily raised or shoved preflop this
/// hand is self-selected into a much narrower range than this pooled prior
/// shows; see [`build_preflop_raise_range_model`] for that case.
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
        .map(|(key, hand_counts)| (key, smoothed_range(&hand_counts)))
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

/// Tallies the window's preflop raises and shoves (opens and reraises
/// alike) into a per-stack-bucket range, Laplace-smoothed like
/// [`build_range_model`]: what hands the field actually shows when it
/// *voluntarily raises* preflop, rather than [`build_range_model`]'s
/// `PfOpen`/`PfVsRaise` ranges, which also mix in the folds and calls seen
/// at those same nodes and so understate how much a real raise narrows the
/// range. Fed into the solver as a seat's range specifically when that seat
/// has already raised or shoved preflop this hand — see
/// [`OpponentRangeModel::resolve_preflop_raiser`].
pub fn build_preflop_raise_range_model(
    actions: &[HistoricAction],
) -> HashMap<StackBucket, StoredRange> {
    let mut counts: HashMap<StackBucket, [u32; HAND_COUNT]> = HashMap::new();
    for action in actions {
        if !action.node.is_preflop()
            || !matches!(action.category, ActionCategory::BetRaise | ActionCategory::Shove)
        {
            continue;
        }
        let entry = counts.entry(action.stack_bucket).or_insert([0u32; HAND_COUNT]);
        entry[action.hand.index()] += 1;
    }
    counts
        .into_iter()
        .map(|(bucket, hand_counts)| (bucket, smoothed_range(&hand_counts)))
        .collect()
}

/// Persists the preflop-raiser range model under the single pooled opponent
/// profile, reusing `contextual_ranges` with the free-text
/// [`PF_RAISER_NODE_KEY`] in place of a real [`Node`] key.
pub async fn save_preflop_raise_range_model(
    pool: &PgPool,
    profile_id: i32,
    model: &HashMap<StackBucket, StoredRange>,
) -> Result<()> {
    for (bucket, range) in model {
        db::upsert_contextual_range(pool, profile_id, PF_RAISER_NODE_KEY, bucket.as_i16(), range)
            .await?;
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
    preflop_raiser_ranges: HashMap<StackBucket, Range>,
}

impl OpponentRangeModel {
    pub fn resolve(&self, node: Node, bucket: StackBucket) -> Option<Range> {
        self.ranges.get(&(node, bucket)).copied()
    }

    /// The range a seat shows when it has voluntarily raised or shoved
    /// preflop this hand — see [`build_preflop_raise_range_model`]. Callers
    /// should prefer this over [`Self::resolve`] for a seat that has
    /// already raised, since the pooled per-node range mixes in the folds
    /// and calls seen at the same decision point.
    pub fn resolve_preflop_raiser(&self, bucket: StackBucket) -> Option<Range> {
        self.preflop_raiser_ranges.get(&bucket).copied()
    }

    /// Builds a model directly from resolved entries, bypassing the DB —
    /// for tests that need `resolve` to return a specific range.
    #[cfg(test)]
    pub(crate) fn from_entries(entries: HashMap<(Node, StackBucket), Range>) -> Self {
        Self {
            ranges: entries,
            preflop_raiser_ranges: HashMap::new(),
        }
    }

    /// Builds a model directly from resolved entries plus resolved
    /// raiser-range entries, bypassing the DB — for tests that need both
    /// `resolve` and [`Self::resolve_preflop_raiser`] to return distinct
    /// ranges at once (so a test can tell which one a caller actually used).
    #[cfg(test)]
    pub(crate) fn from_entries_with_raiser(
        entries: HashMap<(Node, StackBucket), Range>,
        raiser_entries: HashMap<StackBucket, Range>,
    ) -> Self {
        Self {
            ranges: entries,
            preflop_raiser_ranges: raiser_entries,
        }
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
    let mut preflop_raiser_ranges = HashMap::new();
    for bucket in StackBucket::ALL {
        let sequence_node = SequenceNode::new(profile_id, bucket, PF_RAISER_NODE_KEY);
        let resolved = resolver.resolve(&store, &sequence_node).await?;
        if !resolved.used_population {
            preflop_raiser_ranges.insert(bucket, resolved.weights);
        }
    }
    Ok(OpponentRangeModel {
        ranges,
        preflop_raiser_ranges,
    })
}

// ------------------------------------------------------- frequency model

/// The same minimum-sample threshold [`RangeResolver`] uses for ranges,
/// reused here so an under-sampled action-frequency entry is treated with
/// the same skepticism.
pub const MIN_SAMPLE_ACTIONS: u32 = crate::range::sequence::MIN_SAMPLE_HANDS;

/// One resolved action-category mix (fold/call-check/raise/shove, summing
/// to 1) for a `(Node, StackBucket, Position, AggressorContext)` spot, plus
/// the sample size backing it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CategoryFrequency {
    pub fold: f32,
    pub call_check: f32,
    pub raise: f32,
    pub shove: f32,
    pub sample_count: u32,
}

/// Tallies the window into a per-node/stack-bucket/position/aggressor-context
/// action-category mix: the share of times the opponent took each category
/// of action at that spot. Spots with no samples are simply absent from the
/// map, mirroring [`build_range_model`].
pub fn build_action_frequency_model(
    actions: &[HistoricAction],
) -> HashMap<(Node, StackBucket, Position, AggressorContext), CategoryFrequency> {
    let mut counts: HashMap<(Node, StackBucket, Position, AggressorContext), [u32; 4]> =
        HashMap::new();
    for action in actions {
        let ctx = aggressor_context(action.node, action.was_preflop_aggressor, action.facing_cbet);
        let entry = counts
            .entry((action.node, action.stack_bucket, action.position, ctx))
            .or_insert([0u32; 4]);
        let slot = match action.category {
            ActionCategory::Fold => 0,
            ActionCategory::CallCheck => 1,
            ActionCategory::BetRaise => 2,
            ActionCategory::Shove => 3,
        };
        entry[slot] += 1;
    }
    counts
        .into_iter()
        .map(|(key, [fold, call_check, raise, shove])| {
            let total = fold + call_check + raise + shove;
            let freq = |count: u32| {
                if total > 0 {
                    count as f32 / total as f32
                } else {
                    0.0
                }
            };
            (
                key,
                CategoryFrequency {
                    fold: freq(fold),
                    call_check: freq(call_check),
                    raise: freq(raise),
                    shove: freq(shove),
                    sample_count: total,
                },
            )
        })
        .collect()
}

/// Persists the action-frequency model under the single pooled opponent
/// profile.
pub async fn save_action_frequency_model(
    pool: &PgPool,
    profile_id: i32,
    model: &HashMap<(Node, StackBucket, Position, AggressorContext), CategoryFrequency>,
) -> Result<()> {
    for ((node, bucket, position, ctx), frequency) in model {
        db::upsert_contextual_action_frequency(
            pool,
            profile_id,
            node.key(),
            bucket.as_i16(),
            position.key(),
            ctx.key(),
            &db::StoredCategoryFrequency {
                fold_pct: frequency.fold,
                call_check_pct: frequency.call_check,
                raise_pct: frequency.raise,
                shove_pct: frequency.shove,
                sample_count: frequency.sample_count,
            },
        )
        .await?;
    }
    Ok(())
}

/// The resolved action-frequency entries a session loads once at start, used
/// to sample which category of action a bot takes in a given spot before
/// the MCTS solve picks the best concrete play within that category. Spots
/// below [`MIN_SAMPLE_ACTIONS`] are simply absent — [`Self::resolve`]
/// returns `None` for those, and callers fall back to the existing
/// skill-softmax behavior over the full candidate set.
#[derive(Clone, Debug, Default)]
pub struct ActionFrequencyModel {
    frequencies: HashMap<(Node, StackBucket, Position, AggressorContext), CategoryFrequency>,
}

impl ActionFrequencyModel {
    pub fn resolve(
        &self,
        node: Node,
        bucket: StackBucket,
        position: Position,
        ctx: AggressorContext,
    ) -> Option<CategoryFrequency> {
        self.frequencies.get(&(node, bucket, position, ctx)).copied()
    }

    /// Builds a model directly from resolved entries, bypassing the DB —
    /// for tests that need `resolve` to return a specific frequency.
    #[cfg(test)]
    pub(crate) fn from_entries(
        entries: HashMap<(Node, StackBucket, Position, AggressorContext), CategoryFrequency>,
    ) -> Self {
        Self {
            frequencies: entries,
        }
    }
}

/// Loads the resolved action-frequency model for the pooled opponent
/// profile, applying the [`MIN_SAMPLE_ACTIONS`] gate entry by entry.
pub async fn load_action_frequency_model(
    pool: &PgPool,
    profile_id: i32,
) -> Result<ActionFrequencyModel> {
    let mut frequencies = HashMap::new();
    for node in Node::ALL {
        for bucket in StackBucket::ALL {
            for position in Position::ALL {
                for ctx in AggressorContext::ALL {
                    let Some(stored) = db::load_contextual_action_frequency(
                        pool,
                        profile_id,
                        node.key(),
                        bucket.as_i16(),
                        position.key(),
                        ctx.key(),
                    )
                    .await?
                    else {
                        continue;
                    };
                    if stored.sample_count < MIN_SAMPLE_ACTIONS {
                        continue;
                    }
                    frequencies.insert(
                        (node, bucket, position, ctx),
                        CategoryFrequency {
                            fold: stored.fold_pct,
                            call_check: stored.call_check_pct,
                            raise: stored.raise_pct,
                            shove: stored.shove_pct,
                            sample_count: stored.sample_count,
                        },
                    );
                }
            }
        }
    }
    Ok(ActionFrequencyModel { frequencies })
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
            ActionCategory::BetRaise | ActionCategory::Shove => 2,
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
            if matches!(action.category, ActionCategory::BetRaise | ActionCategory::Shove) {
                preflop_raise += 1;
            }
        } else {
            match action.category {
                ActionCategory::BetRaise | ActionCategory::Shove => postflop_bets += 1,
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
/// the number of distinct hands the window covers, the historic read, and
/// the starting-hand table. The range model itself is saved to the database
/// and loaded separately per session ([`load_range_model`]).
#[derive(Clone, Debug, PartialEq)]
pub struct HistorySummary {
    pub window_hands: usize,
    pub read: HistoricRead,
    pub table: Vec<HandRow>,
}

impl Default for HistorySummary {
    /// The empty window: no history gathered yet (no pool, or nothing to
    /// gather from). Every table cell stays ungraded rather than showing a
    /// false zero.
    fn default() -> Self {
        HistorySummary {
            window_hands: 0,
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
///
/// `hero_historic` is the hero's own mirror of `historic` ([`refresh_hero`])
/// — no range model, since nothing solves against the hero's own range.
#[derive(Clone, Debug, Default)]
pub struct OpponentModel {
    pub ranges: OpponentRangeModel,
    pub frequencies: ActionFrequencyModel,
    pub historic: HistorySummary,
    pub hero_historic: HistorySummary,
}

/// Rebuilds the opponent history window, saves the refreshed range model
/// under the pooled profile, and returns the coach-panel summary. Call this
/// after importing new `gg_hands`, and periodically as local play
/// accumulates.
pub async fn refresh(pool: &PgPool) -> Result<HistorySummary> {
    let (actions, window_hands) = load_action_window(pool, HISTORY_WINDOW).await?;
    let profile_id = db::upsert_opponent_profile(pool, POOLED_PROFILE_NAME, "FIELD").await?;
    let model = build_range_model(&actions);
    save_range_model(pool, profile_id, &model).await?;
    let raise_model = build_preflop_raise_range_model(&actions);
    save_preflop_raise_range_model(pool, profile_id, &raise_model).await?;
    let frequency_model = build_action_frequency_model(&actions);
    save_action_frequency_model(pool, profile_id, &frequency_model).await?;
    Ok(HistorySummary {
        window_hands,
        read: build_historic_read(&actions),
        table: build_starting_hand_table(&actions),
    })
}

/// Rebuilds the hero's own starting-hand window into the coach-panel
/// summary. Mirrors [`refresh`], minus the range-model persistence — the
/// hero's own historic actions aren't used as anyone's solver prior.
pub async fn refresh_hero(pool: &PgPool) -> Result<HistorySummary> {
    let (actions, window_hands) = load_hero_action_window(pool, HISTORY_WINDOW).await?;
    Ok(HistorySummary {
        window_hands,
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
    use crate::game::Seat;
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
                (Node::RiverVsBet, ActionCategory::Shove),
            ],
            "SB calling the open BB is PF_OPEN (no raise yet); calling the \
             raise, and every later street facing a bet, all classify as \
             the *_VS_* node; the river raise is an all-in push, so it's a \
             Shove rather than a plain BetRaise"
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

    // ------------------------------------------------------------ position

    #[test]
    fn position_key_round_trips_for_every_variant() {
        for position in Position::ALL {
            assert_eq!(Position::parse(position.key()), Some(position));
        }
        assert_eq!(Position::parse("NOT_A_POSITION"), None);
    }

    #[test]
    fn decision_position_classifies_button_big_blind_and_third_seat() {
        let level = crate::game::blinds::BlindLevel::new(10, 20);
        let mut deck = crate::card::Deck::shuffled(&mut crate::rng::seeded_rng(1));
        let mut state = GameState::new(Seat::Hero, level);
        state.start_hand(&mut deck).unwrap();
        // Hero is the button (small blind); Opponent1 is the big blind
        // (next active seat after the button); Opponent2 is the third seat,
        // first to act preflop.
        assert_eq!(state.to_act(), Seat::Opponent2);
        assert_eq!(decision_position(&state), Position::Third);

        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        assert_eq!(decision_position(&state), Position::Button);

        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent1);
        assert_eq!(decision_position(&state), Position::BigBlind);
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
        assert_eq!(ActionCategory::of(Action::AllIn), ActionCategory::Shove);
    }

    #[test]
    fn action_category_label_round_trips() {
        for category in [
            ActionCategory::Fold,
            ActionCategory::CallCheck,
            ActionCategory::BetRaise,
            ActionCategory::Shove,
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
        action_with_context(hand, node, bucket, Position::Third, category, false, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn action_with_context(
        hand: RangeHand,
        node: Node,
        bucket: StackBucket,
        position: Position,
        category: ActionCategory,
        was_preflop_aggressor: bool,
        facing_cbet: bool,
    ) -> HistoricAction {
        HistoricAction {
            hand,
            node,
            stack_bucket: bucket,
            position,
            category,
            was_preflop_aggressor,
            facing_cbet,
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
            hand_no: 1,
            position: Position::Button.key().to_string(),
            was_preflop_aggressor: false,
            facing_cbet: true,
        };
        let historic = HistoricAction::from_local(row).expect("well-formed row parses");
        assert_eq!(historic.hand, hand(Rank::Queen, Rank::Queen, false));
        assert_eq!(historic.node, Node::FlopVsBet);
        assert_eq!(historic.stack_bucket, StackBucket::Bb15);
        assert_eq!(historic.position, Position::Button);
        assert_eq!(historic.category, ActionCategory::BetRaise);
        assert!(!historic.was_preflop_aggressor);
        assert!(historic.facing_cbet);
    }

    #[test]
    fn local_hero_action_round_trips_through_historic_action() {
        let row = crate::db::LocalHeroAction {
            node: Node::FlopVsBet.key().to_string(),
            stack_bucket: StackBucket::Bb15.as_i16(),
            hole_cards: "Qc Qd".to_string(),
            action: ActionCategory::BetRaise.label().to_string(),
            hand_no: 1,
            position: Position::Button.key().to_string(),
            was_preflop_aggressor: false,
            facing_cbet: true,
        };
        let historic = HistoricAction::from_local_hero(row).expect("well-formed row parses");
        assert_eq!(historic.hand, hand(Rank::Queen, Rank::Queen, false));
        assert_eq!(historic.node, Node::FlopVsBet);
        assert_eq!(historic.stack_bucket, StackBucket::Bb15);
        assert_eq!(historic.position, Position::Button);
        assert_eq!(historic.category, ActionCategory::BetRaise);
        assert!(!historic.was_preflop_aggressor);
        assert!(historic.facing_cbet);
    }

    #[test]
    fn local_action_with_an_unknown_node_or_action_does_not_parse() {
        let bad_node = LocalOpponentAction {
            node: "MADE_UP".to_string(),
            stack_bucket: 15,
            hole_cards: "Qc Qd".to_string(),
            action: "BetRaise".to_string(),
            hand_no: 1,
            position: Position::Third.key().to_string(),
            was_preflop_aggressor: false,
            facing_cbet: false,
        };
        assert_eq!(HistoricAction::from_local(bad_node), None);

        let bad_cards = LocalOpponentAction {
            node: "FLOP_LEAD".to_string(),
            stack_bucket: 15,
            hole_cards: "garbage".to_string(),
            action: "Fold".to_string(),
            hand_no: 1,
            position: Position::Third.key().to_string(),
            was_preflop_aggressor: false,
            facing_cbet: false,
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
        let prior = chen_prior();
        let main_denom = 3.0 + RANGE_SMOOTHING_TOTAL;
        assert!(
            (main.weights[aa.index()] - (2.0 + RANGE_SMOOTHING_TOTAL * prior[aa.index()]) / main_denom)
                .abs()
                < 1e-6
        );
        assert!(
            (main.weights[kk.index()] - (1.0 + RANGE_SMOOTHING_TOTAL * prior[kk.index()]) / main_denom)
                .abs()
                < 1e-6
        );
        // AA still outweighs KK (more raw hits, and AA's Chen prior is the
        // biggest of any class), which still outweighs a hand class that
        // never showed up in the window and has essentially no Chen-prior
        // pull either — smoothing damps the raw frequencies toward the
        // prior without erasing the real signal.
        let seven_deuce = hand(Rank::Seven, Rank::Two, false);
        assert!(main.weights[aa.index()] > main.weights[kk.index()]);
        assert!(main.weights[kk.index()] > main.weights[seven_deuce.index()]);
        assert!(
            (main.weights.iter().sum::<f32>() - 1.0).abs() < 1e-5,
            "smoothed weights still sum to 1"
        );

        let other_bucket = &model[&(Node::PfOpen, StackBucket::Bb10)];
        assert_eq!(other_bucket.sample_count, 1);
        let other_denom = 1.0 + RANGE_SMOOTHING_TOTAL;
        assert!(
            (other_bucket.weights[aa.index()]
                - (1.0 + RANGE_SMOOTHING_TOTAL * prior[aa.index()]) / other_denom)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn build_range_model_is_empty_for_an_empty_window() {
        assert!(build_range_model(&[]).is_empty());
    }

    /// Regression for the "raise 4-8o into a real raise" coaching complaint:
    /// a thin-but-trusted window (above [`MIN_SAMPLE_HANDS`], the gate
    /// `RangeResolver` uses to decide the range is worth using at all) can
    /// still leave most of the 169 hand classes at a raw zero count simply
    /// because they haven't come up yet — including the premium hands a real
    /// opponent's raising range is built from. Without smoothing, the solver
    /// reads that zero as "impossible", not "unobserved", and hero's re-raise
    /// looks great purely because the model has ruled out AA/KK/AKo. The
    /// smoothed range must never rule out a hand class outright.
    #[test]
    fn build_range_model_never_zeroes_out_an_unobserved_premium_hand() {
        let seven_deuce = hand(Rank::Seven, Rank::Two, false);
        let actions: Vec<HistoricAction> = (0..40)
            .map(|_| {
                action(
                    seven_deuce,
                    Node::PfVsRaise,
                    StackBucket::Bb15,
                    ActionCategory::Fold,
                )
            })
            .collect();
        let model = build_range_model(&actions);
        let range = &model[&(Node::PfVsRaise, StackBucket::Bb15)];
        assert_eq!(range.sample_count, 40);

        let aa = hand(Rank::Ace, Rank::Ace, false);
        let kk = hand(Rank::King, Rank::King, false);
        let ako = hand(Rank::Ace, Rank::King, false);
        for premium in [aa, kk, ako] {
            assert!(
                range.weights[premium.index()] > 0.0,
                "{} must stay reachable even with zero raw observations",
                premium.label()
            );
        }
    }

    // ---------------------------------------------------- raiser range

    /// The regression above ([`build_range_model_never_zeroes_out_an_unobserved_premium_hand`])
    /// keeps the pooled `PfVsRaise` range from hard-zeroing premiums, but
    /// that range still pools *every* action taken facing a raise — folds
    /// and calls included — so it stays far wider than what a seat that
    /// actually raised would show. `build_preflop_raise_range_model` is the
    /// fix for that seat specifically: it only tallies hands from raises
    /// and shoves, so a raiser's assumed range comes out tight, not pooled.
    #[test]
    fn build_preflop_raise_range_model_only_tallies_raises_and_shoves() {
        let aa = hand(Rank::Ace, Rank::Ace, false);
        let seven_deuce = hand(Rank::Seven, Rank::Two, false);
        let actions = vec![
            action(aa, Node::PfOpen, StackBucket::Bb25, ActionCategory::BetRaise),
            action(aa, Node::PfVsRaise, StackBucket::Bb25, ActionCategory::Shove),
            // A fold and a call at the very same nodes must not count —
            // only the seat's own raise/shove decisions narrow its range.
            action(
                seven_deuce,
                Node::PfOpen,
                StackBucket::Bb25,
                ActionCategory::Fold,
            ),
            action(
                seven_deuce,
                Node::PfVsRaise,
                StackBucket::Bb25,
                ActionCategory::CallCheck,
            ),
            // Postflop raises never count either — this is a preflop-only
            // range.
            action(
                seven_deuce,
                Node::FlopLead,
                StackBucket::Bb25,
                ActionCategory::BetRaise,
            ),
        ];
        let model = build_preflop_raise_range_model(&actions);
        let range = &model[&StackBucket::Bb25];
        assert_eq!(range.sample_count, 2, "only the two raise/shove actions count");
        assert!(range.weights[aa.index()] > range.weights[seven_deuce.index()]);
    }

    #[test]
    fn build_preflop_raise_range_model_is_empty_for_an_empty_window() {
        assert!(build_preflop_raise_range_model(&[]).is_empty());
    }

    // ------------------------------------------------------ frequency model

    #[test]
    fn build_action_frequency_model_conditions_on_position_and_aggressor_context() {
        let aa = hand(Rank::Ace, Rank::Ace, false);
        let actions = vec![
            // Button, PfVsRaise: 2 raises (one a shove), 1 fold — 3-bet% is
            // queryable here as raise+shove rate.
            action_with_context(
                aa,
                Node::PfVsRaise,
                StackBucket::Bb25,
                Position::Button,
                ActionCategory::BetRaise,
                false,
                false,
            ),
            action_with_context(
                aa,
                Node::PfVsRaise,
                StackBucket::Bb25,
                Position::Button,
                ActionCategory::Shove,
                false,
                false,
            ),
            action_with_context(
                aa,
                Node::PfVsRaise,
                StackBucket::Bb25,
                Position::Button,
                ActionCategory::Fold,
                false,
                false,
            ),
            // Same node/bucket but a different position must not mix in.
            action_with_context(
                aa,
                Node::PfVsRaise,
                StackBucket::Bb25,
                Position::BigBlind,
                ActionCategory::Fold,
                false,
                false,
            ),
            // FlopLead as the preflop aggressor (a c-bet) vs. not (must not
            // mix into the same bucket).
            action_with_context(
                aa,
                Node::FlopLead,
                StackBucket::Bb25,
                Position::Button,
                ActionCategory::BetRaise,
                true,
                false,
            ),
            action_with_context(
                aa,
                Node::FlopLead,
                StackBucket::Bb25,
                Position::Button,
                ActionCategory::CallCheck,
                false,
                false,
            ),
        ];
        let model = build_action_frequency_model(&actions);

        let three_bet_spot = &model[&(
            Node::PfVsRaise,
            StackBucket::Bb25,
            Position::Button,
            AggressorContext::NotApplicable,
        )];
        assert_eq!(three_bet_spot.sample_count, 3);
        assert!((three_bet_spot.raise - 1.0 / 3.0).abs() < 1e-6);
        assert!((three_bet_spot.shove - 1.0 / 3.0).abs() < 1e-6);
        assert!((three_bet_spot.fold - 1.0 / 3.0).abs() < 1e-6);

        let other_position = &model[&(
            Node::PfVsRaise,
            StackBucket::Bb25,
            Position::BigBlind,
            AggressorContext::NotApplicable,
        )];
        assert_eq!(other_position.sample_count, 1);
        assert_eq!(other_position.fold, 1.0);

        let cbet_spot = &model[&(
            Node::FlopLead,
            StackBucket::Bb25,
            Position::Button,
            AggressorContext::Aggressor,
        )];
        assert_eq!(cbet_spot.sample_count, 1);
        assert_eq!(cbet_spot.raise, 1.0, "c-bet% is the raise rate when leading as the aggressor");

        let non_cbet_spot = &model[&(
            Node::FlopLead,
            StackBucket::Bb25,
            Position::Button,
            AggressorContext::NotAggressor,
        )];
        assert_eq!(non_cbet_spot.sample_count, 1);
        assert_eq!(non_cbet_spot.call_check, 1.0);
    }

    #[test]
    fn build_action_frequency_model_is_empty_for_an_empty_window() {
        assert!(build_action_frequency_model(&[]).is_empty());
    }

    // ------------------------------------------------------- hand table

    #[test]
    fn starting_hand_table_grades_a_single_observed_sample_but_not_an_unseen_hand() {
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
        // A single sample is thin, but real data with real data beats
        // showing nothing — MIN_HAND_SAMPLES(1) grades it.
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
            seven_two_row.fold_pct,
            Some(100.0),
            "one real sample is still graded, not hidden as false precision"
        );

        let unseen_row = table.iter().find(|row| row.label == "72s").unwrap();
        assert_eq!(unseen_row.samples, 0);
        assert_eq!(
            unseen_row.fold_pct, None,
            "a hand class with zero samples stays ungraded — there is nothing to show"
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
                hand_no: 42,
                position: Position::Button.key().to_string(),
                was_preflop_aggressor: false,
                facing_cbet: false,
            },
            LocalOpponentAction {
                node: Node::FlopVsBet.key().to_string(),
                stack_bucket: StackBucket::Bb10.as_i16(),
                hole_cards: "7c 2d".to_string(),
                action: ActionCategory::Fold.label().to_string(),
                hand_no: 43,
                position: Position::BigBlind.key().to_string(),
                was_preflop_aggressor: false,
                facing_cbet: true,
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
        assert_eq!(after[0].hand_no, 43);
        assert_eq!(after[1].hole_cards, "As Ks");
        assert_eq!(after[1].hand_no, 42);

        let limited = db::load_recent_local_opponent_actions(&pool, 0)
            .await
            .unwrap();
        assert!(limited.is_empty());
    }

    #[tokio::test]
    async fn local_hero_actions_round_trip_and_fill_the_shortfall() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;

        let before = db::load_recent_local_hero_actions(&pool, 5).await.unwrap();
        let rows = vec![
            crate::db::LocalHeroAction {
                node: Node::PfOpen.key().to_string(),
                stack_bucket: StackBucket::Bb25.as_i16(),
                hole_cards: "As Ks".to_string(),
                action: ActionCategory::BetRaise.label().to_string(),
                hand_no: 42,
                position: Position::Button.key().to_string(),
                was_preflop_aggressor: false,
                facing_cbet: false,
            },
            crate::db::LocalHeroAction {
                node: Node::FlopVsBet.key().to_string(),
                stack_bucket: StackBucket::Bb10.as_i16(),
                hole_cards: "7c 2d".to_string(),
                action: ActionCategory::Fold.label().to_string(),
                hand_no: 43,
                position: Position::BigBlind.key().to_string(),
                was_preflop_aggressor: false,
                facing_cbet: true,
            },
        ];
        db::insert_local_hero_actions(&pool, &rows).await.unwrap();

        let after = db::load_recent_local_hero_actions(&pool, (before.len() + 2) as i64)
            .await
            .unwrap();
        assert_eq!(after.len(), before.len() + 2);
        // Newest first: the two just-inserted rows lead, most-recent last-in first-out.
        assert_eq!(after[0].hole_cards, "7c 2d");
        assert_eq!(after[0].hand_no, 43);
        assert_eq!(after[1].hole_cards, "As Ks");
        assert_eq!(after[1].hand_no, 42);

        let limited = db::load_recent_local_hero_actions(&pool, 0).await.unwrap();
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
        let expected_aa = (40.0 + RANGE_SMOOTHING_TOTAL * chen_prior()[aa.index()])
            / (40.0 + RANGE_SMOOTHING_TOTAL);
        assert!((range[aa.index()] - expected_aa).abs() < 1e-6);
        let kk = crate::range::hands::Hand::new(Rank::King, Rank::King, false);
        assert!(
            range[kk.index()] > 0.0,
            "smoothing keeps every hand class reachable, even at 40/40 observed AA"
        );

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
    async fn action_frequency_model_saves_and_loads_through_the_pooled_profile() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let name = format!(
            "test_field_freq_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let profile_id = db::upsert_opponent_profile(&pool, &name, "FIELD")
            .await
            .unwrap();
        let aa = crate::range::hands::Hand::new(Rank::Ace, Rank::Ace, false);

        // Below MIN_SAMPLE_ACTIONS: the entry is dropped on load.
        let sparse = vec![action(
            aa,
            Node::RiverLead,
            StackBucket::Bb25,
            ActionCategory::BetRaise,
        )];
        let model = build_action_frequency_model(&sparse);
        save_action_frequency_model(&pool, profile_id, &model)
            .await
            .unwrap();
        let resolved = load_action_frequency_model(&pool, profile_id).await.unwrap();
        assert_eq!(
            resolved.resolve(
                Node::RiverLead,
                StackBucket::Bb25,
                Position::Third,
                AggressorContext::NotApplicable
            ),
            None,
            "a single sample stays below MIN_SAMPLE_ACTIONS and is dropped"
        );

        // At/above MIN_SAMPLE_ACTIONS: the resolved frequency is the trained one.
        let dense: Vec<HistoricAction> = (0..40)
            .map(|_| action(aa, Node::TurnVsBet, StackBucket::Bb15, ActionCategory::Shove))
            .collect();
        let model = build_action_frequency_model(&dense);
        save_action_frequency_model(&pool, profile_id, &model)
            .await
            .unwrap();
        let resolved = load_action_frequency_model(&pool, profile_id).await.unwrap();
        let frequency = resolved
            .resolve(
                Node::TurnVsBet,
                StackBucket::Bb15,
                Position::Third,
                AggressorContext::NotApplicable,
            )
            .expect("40 samples clears MIN_SAMPLE_ACTIONS");
        assert_eq!(frequency.shove, 1.0);
        assert_eq!(frequency.sample_count, 40);

        sqlx::query("DELETE FROM contextual_action_frequencies WHERE profile_id = $1")
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
        assert!(
            summary.read.actions == 0 || summary.window_hands > 0,
            "any recorded decision must have come from at least one hand"
        );
    }

    #[tokio::test]
    async fn refresh_hero_completes_against_an_empty_database_state() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let summary = refresh_hero(&pool).await.unwrap();
        assert_eq!(summary.table.len(), HAND_COUNT);
        assert!(
            summary.read.actions == 0 || summary.window_hands > 0,
            "any recorded decision must have come from at least one hand"
        );
    }
}
