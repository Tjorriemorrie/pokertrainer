use rand::Rng;

use crate::game::{Action, GameState, Seat, Street};
use crate::mcts::{self, MctsConfig};
use crate::range::BetSize;

/// Number of opponent seats (everyone but the hero).
pub const NUM_OPPONENTS: usize = 2;

/// Completed hands needed before the read graduates from the small-sample
/// disclaimer to an actual profile.
pub const MIN_HANDS_FOR_READ: usize = 5;

/// The field skill template the two bots play with: a single 0..1 level
/// score where 1 plays solver-perfect and 0 leaks big blinds on nearly every
/// decision. Derived from the opponent-skill analysis of the imported hands.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OpponentTemplate {
    pub skill: f64,
}

impl OpponentTemplate {
    /// Clamps a measured field skill into the valid 0..1 band.
    pub fn new(skill: f64) -> Self {
        Self {
            skill: skill.clamp(0.0, 1.0),
        }
    }
}

/// How many chips of EV spread separate the good and the bad action at
/// skill 0.5 — the softmax temperature scale for the skill-graded template.
const SKILL_TAU_CHIPS: f64 = 20.0;

/// Picks the bot's action at skill `template.skill`: one MCTS solve from the
/// bot's seat, then a softmax over the candidate EVs whose temperature
/// shrinks with skill — skill 1 plays the solver-best action, skill 0 is
/// nearly uniform across the legal candidates. Solve failures fall back to
/// the plain [`placeholder_action`] heuristic.
pub fn template_action<R: Rng + ?Sized>(
    rng: &mut R,
    state: &GameState,
    seat: Seat,
    config: &MctsConfig,
    template: &OpponentTemplate,
) -> Action {
    let rotated = state.rotated(seat);
    let skill = template.skill.clamp(0.0, 1.0);
    let solve = mcts::solve_for_seat(
        rng,
        &rotated,
        &[None; crate::game::NUM_PLAYERS],
        &[None; crate::game::NUM_PLAYERS],
        config,
        &mcts::candidates(&rotated),
    );
    let Ok(result) = solve else {
        return placeholder_action(rng, state);
    };
    let selected = skill_selection(rng, &result.actions, skill);
    selected.unwrap_or_else(|| placeholder_action(rng, state))
}

/// Weighted choice among the solver's candidate actions: EV-scaled softmax
/// with a skill temperature. High skill concentrates mass on the best EV;
/// low skill flattens into a near-uniform lottery (the BBs leak out exactly
/// the way a weaker field plays).
fn skill_selection<R: Rng + ?Sized>(
    rng: &mut R,
    values: &[crate::mcts::ActionValue],
    skill: f64,
) -> Option<Action> {
    let first = values.first()?;
    let tau = ((1.0 - skill) / skill * SKILL_TAU_CHIPS).clamp(1e-3, 1e6);
    let max_ev = values
        .iter()
        .map(|value| value.ev)
        .fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f32> = values
        .iter()
        .map(|value| (((value.ev - max_ev) / tau).exp()) as f32)
        .collect();
    let index = crate::rng::weighted_index(rng, &weights).unwrap_or(0);
    Some(values.get(index).unwrap_or(first).action)
}

/// Placeholder policy for the opponents: checks any free option, otherwise
/// mostly calls, occasionally folds, and rarely min-raises; bets the minimum
/// (sometimes half pot) when first in. Busted (zero-stack) seats always take
/// the only actions available to them. Legality is guaranteed by construction.
pub fn placeholder_action<R: Rng + ?Sized>(rng: &mut R, state: &GameState) -> Action {
    let legal = state.legal_actions();
    let seat = state.to_act();
    let stack = state.stack(seat);

    if stack == 0 {
        return if legal.can_check {
            Action::Check
        } else {
            Action::Fold
        };
    }

    let roll: u32 = rng.random_range(0..100);

    if legal.can_check {
        if legal.can_bet && roll >= 85 {
            let amount = if roll >= 93 {
                BetSize::HalfPot.to_raise_to(
                    state.total_pot(),
                    0,
                    state.blind_level().big_blind,
                    legal.min_bet,
                    state.stack(seat),
                )
            } else {
                legal.min_bet
            };
            return if amount >= state.stack(seat) {
                Action::AllIn
            } else {
                Action::Bet(amount)
            };
        }
        return Action::Check;
    }

    let min_raise_to = legal.min_raise_to;
    if roll < 15 {
        return Action::Fold;
    }
    if roll < 75 || !legal.can_raise {
        return if legal.can_call {
            Action::Call
        } else {
            Action::AllIn
        };
    }
    if legal.allows(Action::Raise(min_raise_to)) {
        Action::Raise(min_raise_to)
    } else if legal.can_all_in {
        Action::AllIn
    } else {
        Action::Call
    }
}

/// One opponent's session stats plus a point-in-time table snapshot, ready to
/// render in the coach-feedback panel.
#[derive(Clone, Debug, PartialEq)]
pub struct OpponentSnapshot {
    pub seat: Seat,
    pub hands: usize,
    /// Voluntarily put money in preflop, as a share of hands dealt.
    pub vpip_pct: f64,
    /// Raised preflop, as a share of hands dealt.
    pub pfr_pct: f64,
    /// Folded when facing a bet or raise, as a share of bets faced.
    pub fold_to_bet_pct: f64,
    /// Postflop bets and raises (the aggression numerator).
    pub postflop_bets: usize,
    /// Postflop calls (the aggression denominator).
    pub postflop_calls: usize,
    /// A player-friendly one-liner describing the opponent so far.
    pub read: String,
    pub stack: u32,
    pub folded: bool,
    pub all_in: bool,
    pub is_button: bool,
    pub is_small_blind: bool,
    pub is_big_blind: bool,
}

/// Live per-opponent HUD counters, fed every time an opponent acts.
///
/// VPIP counts an opponent once per hand the first time they voluntarily
/// commit chips preflop (calling, raising, or going all-in); blinds and
/// checks are not voluntary. PFR counts preflop raises — an all-in only
/// counts when it is not a call-for-less, i.e. when no bet was faced.
/// Fold-to-bet counts folds across all streets whenever a bet or raise had
/// to be answered. Aggression is the postflop bets-plus-raises-per-call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct OpponentTracker {
    hands: [usize; NUM_OPPONENTS],
    vpip: [usize; NUM_OPPONENTS],
    pfr: [usize; NUM_OPPONENTS],
    faced_bet: [usize; NUM_OPPONENTS],
    folded_to_bet: [usize; NUM_OPPONENTS],
    postflop_bets: [usize; NUM_OPPONENTS],
    postflop_calls: [usize; NUM_OPPONENTS],
    vpip_seen: [bool; NUM_OPPONENTS],
    pfr_seen: [bool; NUM_OPPONENTS],
}

impl OpponentTracker {
    /// Serializes the counters for tournament persistence across reconnects.
    pub fn to_snapshot(&self) -> crate::snapshot::OpponentCountersSnapshot {
        crate::snapshot::OpponentCountersSnapshot {
            hands: self.hands,
            vpip: self.vpip,
            pfr: self.pfr,
            faced_bet: self.faced_bet,
            folded_to_bet: self.folded_to_bet,
            postflop_bets: self.postflop_bets,
            postflop_calls: self.postflop_calls,
            vpip_seen: self.vpip_seen,
            pfr_seen: self.pfr_seen,
        }
    }

    /// Restores the counters saved by [`Self::to_snapshot`].
    pub fn from_snapshot(snapshot: &crate::snapshot::OpponentCountersSnapshot) -> Self {
        Self {
            hands: snapshot.hands,
            vpip: snapshot.vpip,
            pfr: snapshot.pfr,
            faced_bet: snapshot.faced_bet,
            folded_to_bet: snapshot.folded_to_bet,
            postflop_bets: snapshot.postflop_bets,
            postflop_calls: snapshot.postflop_calls,
            vpip_seen: snapshot.vpip_seen,
            pfr_seen: snapshot.pfr_seen,
        }
    }

    /// A new hand was dealt: every seated opponent gets another hand, and the
    /// per-hand VPIP/PFR flags reset.
    pub fn begin_hand(&mut self) {
        for hands in &mut self.hands {
            *hands += 1;
        }
        self.vpip_seen = [false; NUM_OPPONENTS];
        self.pfr_seen = [false; NUM_OPPONENTS];
    }

    /// Records one opponent action with its context. Hero actions are
    /// ignored; `faced_bet` is true when the opponent had chips to call.
    pub fn record(&mut self, seat: Seat, action: Action, street: Street, faced_bet: bool) {
        let Some(index) = seat.index().checked_sub(1) else {
            return;
        };
        if index >= NUM_OPPONENTS {
            return;
        }

        if street == Street::Preflop {
            let voluntarily = matches!(action, Action::Call | Action::Raise(_) | Action::AllIn);
            if voluntarily && !self.vpip_seen[index] {
                self.vpip_seen[index] = true;
                self.vpip[index] += 1;
            }
            let raises =
                matches!(action, Action::Raise(_)) || (action == Action::AllIn && !faced_bet);
            if raises && !self.pfr_seen[index] {
                self.pfr_seen[index] = true;
                self.pfr[index] += 1;
            }
        }

        if faced_bet {
            self.faced_bet[index] += 1;
            if action == Action::Fold {
                self.folded_to_bet[index] += 1;
            }
        }

        if street != Street::Preflop {
            match action {
                Action::Bet(_) | Action::Raise(_) | Action::AllIn => {
                    self.postflop_bets[index] += 1;
                }
                Action::Call => self.postflop_calls[index] += 1,
                Action::Check | Action::Fold => {}
            }
        }
    }

    /// Snapshots both opponents against the current table state for display.
    pub fn snapshots(&self, state: &GameState) -> Vec<OpponentSnapshot> {
        let small_blind = state.small_blind_seat();
        let big_blind = state.big_blind_seat();
        [Seat::Opponent1, Seat::Opponent2]
            .iter()
            .map(|&seat| {
                let index = seat.index() - 1;
                let hands = self.hands[index];
                let vpip_pct = pct(self.vpip[index], hands);
                OpponentSnapshot {
                    seat,
                    hands,
                    vpip_pct,
                    pfr_pct: pct(self.pfr[index], hands),
                    fold_to_bet_pct: pct(self.folded_to_bet[index], self.faced_bet[index]),
                    postflop_bets: self.postflop_bets[index],
                    postflop_calls: self.postflop_calls[index],
                    read: read(
                        hands,
                        vpip_pct,
                        aggression(self.postflop_bets[index], self.postflop_calls[index]),
                    ),
                    stack: state.stack(seat),
                    folded: state.folded(seat),
                    all_in: state.all_in(seat),
                    is_button: state.button() == seat,
                    is_small_blind: small_blind == seat,
                    is_big_blind: big_blind == seat,
                }
            })
            .collect()
    }
}

/// A percentage with a zero-denominator guard.
fn pct(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

/// Postflop aggression: bets-plus-raises per call. `None` when there are no
/// postflop decisions at all; unbounded when an opponent only ever bet or
/// raised postflop and never called.
fn aggression(bets: usize, calls: usize) -> Option<f64> {
    match (bets, calls) {
        (0, 0) => None,
        (_, 0) => Some(f64::INFINITY),
        (bets, calls) => Some(bets as f64 / calls as f64),
    }
}

const AGGRESSIVE_AF: f64 = 2.0;
const PASSIVE_AF: f64 = 1.0;
const TIGHT_VPIP: f64 = 25.0;
const LOOSE_VPIP: f64 = 45.0;

/// The one-line player-friendly read of an opponent's play so far.
fn read(hands: usize, vpip_pct: f64, aggression: Option<f64>) -> String {
    if hands == 0 {
        return "No hands played yet.".to_string();
    }
    if hands < MIN_HANDS_FOR_READ {
        return format!(
            "Only {hands} hand{} so far — too early to profile.",
            if hands == 1 { "" } else { "s" }
        );
    }

    let tight = vpip_pct < TIGHT_VPIP;
    let loose = vpip_pct > LOOSE_VPIP;

    let Some(af) = aggression else {
        return if tight {
            "Tight preflop — plays few hands and decides most of them early.".to_string()
        } else if loose {
            "Loose preflop — plays lots of hands, but street play is still unmarked.".to_string()
        } else {
            "Average hand selection so far; not enough street play to judge yet.".to_string()
        };
    };

    let aggressive = af >= AGGRESSIVE_AF;
    let passive = af < PASSIVE_AF;
    if tight && aggressive {
        "Tight aggressive — selective hands, played hard.".to_string()
    } else if tight && passive {
        "Tight passive — plays few hands and rarely presses the gas.".to_string()
    } else if loose && aggressive {
        "Loose aggressive — in lots of pots and swinging.".to_string()
    } else if loose && passive {
        "Loose passive — plays many hands and calls instead of raising.".to_string()
    } else if tight {
        "Tight and measured — few hands with sensible sizing.".to_string()
    } else if loose {
        "Loose but controlled — wide range without overcommitting.".to_string()
    } else if aggressive {
        "Aggressive — pushes chips around on a normal range.".to_string()
    } else if passive {
        "Moderate but passive — sees flops and lets others drive.".to_string()
    } else {
        "Played a balanced game so far.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::game::blinds::BlindLevel;
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn state() -> GameState {
        GameState::new(Seat::Opponent1, level())
    }

    #[test]
    fn hero_actions_are_ignored() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Hero, Action::Raise(60), Street::Preflop, false);
        tracker.record(Seat::Hero, Action::Bet(40), Street::Flop, false);
        let snapshots = tracker.snapshots(&state());
        assert!(snapshots.iter().all(|s| s.hands == 1 && s.vpip_pct == 0.0));
    }

    #[test]
    fn begin_hand_counts_both_opponents_and_resets_flags() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Call, Street::Preflop, true);
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Call, Street::Preflop, true);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].hands, 2);
        assert_eq!(snapshots[1].hands, 2);
        assert_eq!(snapshots[0].vpip_pct, 100.0);
        assert_eq!(snapshots[1].vpip_pct, 0.0);
    }

    #[test]
    fn vpip_counts_once_per_hand() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Call, Street::Preflop, true);
        tracker.record(Seat::Opponent1, Action::Raise(80), Street::Preflop, true);
        tracker.begin_hand();
        let snapshots = tracker.snapshots(&state());
        assert_eq!(
            snapshots[0].vpip_pct, 50.0,
            "two hands, one VPIP: a call-then-raise counts once"
        );
        assert_eq!(snapshots[0].pfr_pct, 50.0, "one of two hands was raised");
    }

    #[test]
    fn checks_blinds_and_folds_are_not_voluntary() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Check, Street::Preflop, false);
        tracker.record(Seat::Opponent2, Action::Fold, Street::Preflop, true);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].vpip_pct, 0.0);
        assert_eq!(snapshots[1].vpip_pct, 0.0);
    }

    #[test]
    fn preflop_all_in_raise_counts_pfr_only_without_a_bet_faced() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::AllIn, Street::Preflop, false);
        tracker.record(Seat::Opponent2, Action::AllIn, Street::Preflop, true);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].pfr_pct, 100.0, "open shove is an open raise");
        assert_eq!(snapshots[1].pfr_pct, 0.0, "call-for-less is not a raise");
        assert_eq!(snapshots[1].vpip_pct, 100.0, "but it is a voluntary commit");
    }

    #[test]
    fn fold_to_bet_tracks_all_streets() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Fold, Street::Flop, true);
        tracker.record(Seat::Opponent1, Action::Call, Street::Flop, true);
        tracker.record(Seat::Opponent1, Action::Check, Street::Flop, false);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].fold_to_bet_pct, 50.0);
    }

    #[test]
    fn postflop_aggression_separates_bets_from_calls() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Bet(40), Street::Flop, false);
        tracker.record(Seat::Opponent1, Action::Call, Street::Turn, true);
        tracker.record(Seat::Opponent1, Action::Check, Street::River, false);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].postflop_bets, 1);
        assert_eq!(snapshots[0].postflop_calls, 1);
    }

    #[test]
    fn snapshots_carry_position_badges_and_table_status() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        let state = GameState::new(Seat::Opponent2, level());
        let snapshots = tracker.snapshots(&state);
        assert!(
            !snapshots[0].is_button && !snapshots[0].is_small_blind,
            "Opponent 1 sits left of the button and neither posts a blind"
        );
        assert!(
            !snapshots[0].is_big_blind,
            "the hero posts the big blind when Opponent 2 has the button"
        );
        assert!(snapshots[1].is_button && snapshots[1].is_small_blind);
        assert!(!snapshots[1].is_big_blind);
        assert_eq!(snapshots[0].stack, 300);
        assert!(!snapshots[0].folded && !snapshots[0].all_in);
    }

    #[test]
    fn snapshots_zero_divisions_stay_zero() {
        let tracker = OpponentTracker::default();
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].fold_to_bet_pct, 0.0);
        assert_eq!(snapshots[0].hands, 0);
        assert_eq!(snapshots[0].read, "No hands played yet.");
    }

    #[test]
    fn aggression_is_none_infinite_or_a_ratio() {
        assert_eq!(aggression(0, 0), None);
        assert_eq!(aggression(3, 0), Some(f64::INFINITY));
        assert_eq!(aggression(3, 2), Some(1.5));
        assert_eq!(aggression(0, 4), Some(0.0));
    }

    #[test]
    fn read_keeps_small_samples_honest() {
        assert_eq!(read(0, 50.0, Some(1.5)), "No hands played yet.");
        assert_eq!(
            read(1, 100.0, Some(1.5)),
            "Only 1 hand so far — too early to profile."
        );
        assert_eq!(
            read(4, 100.0, Some(1.5)),
            "Only 4 hands so far — too early to profile."
        );
    }

    #[test]
    fn read_without_street_play_describes_preflop_only() {
        assert_eq!(
            read(10, 20.0, None),
            "Tight preflop — plays few hands and decides most of them early."
        );
        assert_eq!(
            read(10, 60.0, None),
            "Loose preflop — plays lots of hands, but street play is still unmarked."
        );
        assert_eq!(
            read(10, 35.0, None),
            "Average hand selection so far; not enough street play to judge yet."
        );
    }

    #[test]
    fn read_combines_looseness_and_aggression() {
        assert_eq!(
            read(10, 20.0, Some(3.0)),
            "Tight aggressive — selective hands, played hard."
        );
        assert_eq!(
            read(10, 20.0, Some(0.5)),
            "Tight passive — plays few hands and rarely presses the gas."
        );
        assert_eq!(
            read(10, 80.0, Some(3.0)),
            "Loose aggressive — in lots of pots and swinging."
        );
        assert_eq!(
            read(10, 80.0, Some(0.5)),
            "Loose passive — plays many hands and calls instead of raising."
        );
        assert_eq!(
            read(10, 20.0, Some(1.5)),
            "Tight and measured — few hands with sensible sizing."
        );
        assert_eq!(
            read(10, 80.0, Some(1.5)),
            "Loose but controlled — wide range without overcommitting."
        );
        assert_eq!(
            read(10, 35.0, Some(3.0)),
            "Aggressive — pushes chips around on a normal range."
        );
        assert_eq!(
            read(10, 35.0, Some(0.5)),
            "Moderate but passive — sees flops and lets others drive."
        );
        assert_eq!(read(10, 35.0, Some(1.5)), "Played a balanced game so far.");
    }

    #[test]
    fn read_thresholds_are_exclusive_at_the_boundaries() {
        assert_eq!(
            read(10, 25.0, Some(3.0)),
            read(10, 45.0, Some(3.0)),
            "25 and 45 VPIP read as neither tight nor loose"
        );
        assert_eq!(
            read(10, 35.0, Some(1.0)),
            "Played a balanced game so far.",
            "AF of exactly 1.0 reads as balanced, not passive"
        );
        assert_eq!(
            read(10, 35.0, Some(2.0)),
            "Aggressive — pushes chips around on a normal range.",
            "AF of exactly 2.0 reads as aggressive"
        );
    }

    #[test]
    fn read_only_overflows_without_postflop_calls() {
        assert_eq!(
            read(20, 35.0, Some(f64::INFINITY)),
            "Aggressive — pushes chips around on a normal range."
        );
        assert_eq!(
            read(20, 20.0, Some(f64::INFINITY)),
            "Tight aggressive — selective hands, played hard."
        );
        assert_eq!(
            read(20, 80.0, Some(f64::INFINITY)),
            "Loose aggressive — in lots of pots and swinging."
        );
    }

    // ------------------------------------------------------ field template

    fn value(action: Action, ev: f64) -> crate::mcts::ActionValue {
        crate::mcts::ActionValue {
            action,
            bucket: None,
            ev,
            variance: 0.0,
            bust_prob: 0.0,
            visits: 100,
        }
    }

    fn dealt_state() -> GameState {
        let mut state = GameState::new(Seat::Opponent1, BlindLevel::new(10, 20));
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(3)))
            .unwrap();
        state
    }

    #[test]
    fn template_skill_is_clamped_into_the_band() {
        assert_eq!(OpponentTemplate::new(0.62).skill, 0.62);
        assert_eq!(OpponentTemplate::new(1.7).skill, 1.0);
        assert_eq!(OpponentTemplate::new(-0.4).skill, 0.0);
    }

    #[test]
    fn skill_selection_concentrates_on_the_best_ev_as_skill_grows() {
        let values = vec![
            value(Action::Call, 50.0),
            value(Action::Fold, 0.0),
            value(Action::AllIn, -50.0),
        ];
        let mut rng = seeded_rng(1);
        for _ in 0..64 {
            assert_eq!(
                skill_selection(&mut rng, &values, 1.0),
                Some(Action::Call),
                "skill 1 always takes the solver-best action"
            );
        }
        let mut seen: Vec<Action> = Vec::new();
        for seed in 0..256u64 {
            let mut rng = seeded_rng(seed);
            let action = skill_selection(&mut rng, &values, 0.0).unwrap();
            if !seen.contains(&action) {
                seen.push(action);
            }
        }
        assert_eq!(
            seen.len(),
            3,
            "skill 0 scatters across every legal candidate: {seen:?}"
        );
    }

    #[test]
    fn skill_selection_tolerates_empty_candidates() {
        let mut rng = seeded_rng(2);
        assert_eq!(skill_selection(&mut rng, &[], 0.5), None);
    }

    #[test]
    fn template_action_is_always_legal_and_skill_graded() {
        let state = dealt_state();
        let config = MctsConfig::test();
        for skill in [0.0, 0.3, 0.62, 1.0] {
            let template = OpponentTemplate::new(skill);
            for seed in 0..6u64 {
                let mut rng = seeded_rng(seed);
                let seat = state.to_act();
                let action = template_action(&mut rng, &state, seat, &config, &template);
                assert!(
                    state.legal_actions().allows(action),
                    "skill {skill} seed {seed} produced illegal {action:?}"
                );
            }
        }
    }

    #[test]
    fn template_action_is_seed_deterministic() {
        let state = dealt_state();
        let template = OpponentTemplate::new(0.62);
        let mut a = seeded_rng(7);
        let mut b = seeded_rng(7);
        assert_eq!(
            template_action(
                &mut a,
                &state,
                state.to_act(),
                &MctsConfig::test(),
                &template
            ),
            template_action(
                &mut b,
                &state,
                state.to_act(),
                &MctsConfig::test(),
                &template
            )
        );
    }
}
