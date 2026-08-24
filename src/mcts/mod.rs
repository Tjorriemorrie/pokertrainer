pub mod actions;
pub mod config;
pub mod rollout;
pub mod tree;
pub mod world;

pub use actions::candidates;
pub use config::MctsConfig;
pub use world::{World, WorldSampler};

use rand::Rng;

use crate::error::{Error, Result};
use crate::game::{Action, GameState, Seat};
use crate::range::BetSize;
use crate::range::hands::Range;

use tree::WorldSearch;

/// Per-world action statistics: action, mean rollout value, payoff variance,
/// bust probability, and total visits.
type WorldStats = Vec<(Action, f64, f64, f64, u64)>;
/// One world's weight and its action statistics.
type PerWorld = (f64, WorldStats);

/// The solver's estimate for one candidate action: its expectimax EV in
/// chips relative to the hero's stack at the decision point, the
/// visit-weighted variance of the payoff, the probability the hero busts
/// (ends the hand with an empty stack), and the aggregate number of visits
/// that backed it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActionValue {
    pub action: Action,
    pub bucket: Option<BetSize>,
    pub ev: f64,
    pub variance: f64,
    pub bust_prob: f64,
    pub visits: u64,
}

impl ActionValue {
    /// The standard deviation of the action's payoff, in chips.
    pub fn sigma(&self) -> f64 {
        self.variance.sqrt()
    }
}

/// The outcome of one solve: one EV per candidate action, sorted from best
/// to worst, expected value in chips — plus how deep and how wide the search
/// actually ran, so the UI can show and audit the solver's effort.
#[derive(Clone, Debug, PartialEq)]
pub struct SolveResult {
    pub actions: Vec<ActionValue>,
    pub worlds: usize,
    /// The effective per-world iteration budget after street scaling.
    pub iterations: usize,
    /// The effective tree-depth cap in hero decisions after street scaling.
    pub max_depth: usize,
    /// Total tree nodes expanded across all worlds.
    pub nodes: usize,
    /// The deepest tree node expanded across all worlds.
    pub max_tree_depth: usize,
    /// Total actions simulated in rollouts across all worlds.
    pub rollout_actions: u64,
}

/// Solves the hero's current decision.
///
/// Samples [`MctsConfig::worlds`] determinizations from the two opponent
/// ranges (`[Opponent1, Opponent2]`), runs an isolated expectimax-UCT search
/// per world over the hero's candidate actions, and returns the
/// range-probability-weighted action EVs — opponent holdings are never
/// averaged across worlds mid-search. The budget is scaled per street via
/// [`MctsConfig::for_street`], so early streets (more unknown runouts) get
/// the deeper search they need.
pub fn solve<R: Rng + ?Sized>(
    rng: &mut R,
    state: &GameState,
    ranges: &[Range; 2],
    config: &MctsConfig,
) -> Result<SolveResult> {
    solve_with_candidates(rng, state, ranges, config, &candidates(state))
}

/// Like [`solve`], but searches an explicitly supplied candidate set. Extra
/// candidates (e.g. a bet-slider amount the player chose) are searched on
/// equal footing with the standard buckets.
pub fn solve_with_candidates<R: Rng + ?Sized>(
    rng: &mut R,
    state: &GameState,
    ranges: &[Range; 2],
    config: &MctsConfig,
    root_candidates: &[(Action, Option<BetSize>)],
) -> Result<SolveResult> {
    config.validate()?;
    if state.is_hand_over() {
        return Err(Error::Solver("cannot solve a hand that is over".into()));
    }
    if state.to_act() != Seat::Hero {
        return Err(Error::Solver("solver requires the hero to act".into()));
    }

    let budget = config.for_street(state.street());
    let worlds = WorldSampler::sample(rng, state, ranges, budget.worlds)?;
    let baseline = state.stack(Seat::Hero);

    let mut per_world: Vec<PerWorld> = Vec::with_capacity(worlds.len());
    let mut total_nodes = 0usize;
    let mut max_tree_depth = 0usize;
    let mut total_rollout_actions = 0u64;
    for world in &worlds {
        let mut search = WorldSearch::new(
            world.build_state(state),
            &world.runout,
            baseline,
            root_candidates.to_vec(),
            budget,
        );
        let (values, stats) = search.run(rng)?;
        total_nodes += stats.nodes;
        max_tree_depth = max_tree_depth.max(stats.max_tree_depth);
        total_rollout_actions += stats.rollout_actions;
        per_world.push((
            world.weight,
            values
                .into_iter()
                .map(|(action, _bucket, value, variance, bust_prob, visits)| {
                    (action, value, variance, bust_prob, visits)
                })
                .collect(),
        ));
    }

    let combined = combine_world_values(&per_world)?;
    let mut actions: Vec<ActionValue> = root_candidates
        .iter()
        .filter_map(|(action, bucket)| {
            combined
                .iter()
                .find(|(a, _, _, _)| a == action)
                .map(|(_, ev, variance, bust_prob)| ActionValue {
                    action: *action,
                    bucket: *bucket,
                    ev: *ev,
                    variance: *variance,
                    bust_prob: *bust_prob,
                    visits: visits_for(action, &per_world),
                })
        })
        .collect();

    actions.sort_by(|a, b| b.ev.total_cmp(&a.ev));
    Ok(SolveResult {
        actions,
        worlds: worlds.len(),
        iterations: budget.iterations,
        max_depth: budget.max_depth,
        nodes: total_nodes,
        max_tree_depth,
        rollout_actions: total_rollout_actions,
    })
}

fn visits_for(action: &Action, per_world: &[PerWorld]) -> u64 {
    per_world
        .iter()
        .flat_map(|(_, values)| values.iter())
        .filter(|(a, _, _, _, _)| a == action)
        .map(|(_, _, _, _, visits)| *visits)
        .sum()
}

/// Merges per-world action statistics with the worlds' weights: the
/// expectimax step over sampled opponent holdings. Each world must report the
/// same set of actions in the same order; returns an empty vector for no
/// worlds. Variances combine as `E[x²] − E[x]²` over the world mixture.
fn combine_world_values(per_world: &[PerWorld]) -> Result<Vec<(Action, f64, f64, f64)>> {
    let Some((_, reference)) = per_world.first() else {
        return Ok(Vec::new());
    };
    let mut means = vec![0.0f64; reference.len()];
    let mut second_moments = vec![0.0f64; reference.len()];
    let mut busts = vec![0.0f64; reference.len()];

    let mut weight_sum = 0.0;
    for (weight, values) in per_world {
        if values.len() != reference.len() {
            return Err(Error::Solver(
                "worlds disagree on the candidate action set".into(),
            ));
        }
        weight_sum += *weight;
        for (index, (action, value, variance, bust_prob, _)) in values.iter().enumerate() {
            if reference[index].0 != *action {
                return Err(Error::Solver(
                    "worlds disagree on candidate ordering".into(),
                ));
            }
            means[index] += *weight * *value;
            second_moments[index] += *weight * (*variance + value * value);
            busts[index] += *weight * *bust_prob;
        }
    }

    Ok(reference
        .iter()
        .enumerate()
        .map(|(index, (action, _, _, _, _))| {
            let mean = means[index] / weight_sum;
            let variance = (second_moments[index] / weight_sum - mean * mean).max(0.0);
            (*action, mean, variance, busts[index] / weight_sum)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Deck, Rank, Suit};
    use crate::game::blinds::BlindLevel;
    use crate::range::hands::{HAND_COUNT, Hand};
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn card(rank: Rank, suit: Suit) -> Card {
        Card::new(rank, suit)
    }

    fn uniform() -> Range {
        [1.0 / HAND_COUNT as f32; HAND_COUNT]
    }

    #[test]
    fn combine_takes_the_probability_weighted_average() {
        let fold = Action::Fold;
        let call = Action::Call;
        let per_world = vec![
            (
                0.25,
                vec![(fold, 10.0, 0.0, 0.0, 100), (call, 0.0, 0.0, 0.0, 90)],
            ),
            (
                0.75,
                vec![(fold, 20.0, 0.0, 0.0, 80), (call, 0.0, 0.0, 0.0, 70)],
            ),
        ];
        let combined = combine_world_values(&per_world).unwrap();
        let mean = 0.25 * 10.0 + 0.75 * 20.0;
        let second = 0.25 * 100.0 + 0.75 * 400.0;
        assert_eq!(
            combined,
            vec![
                (fold, mean, second - mean * mean, 0.0),
                (call, 0.0, 0.0, 0.0)
            ]
        );
    }

    #[test]
    fn combine_mixes_variances_and_bust_probabilities() {
        let fold = Action::Fold;
        let all_in = Action::AllIn;
        let per_world = vec![
            (
                0.5,
                vec![(fold, 0.0, 0.0, 0.0, 10), (all_in, -50.0, 40.0, 0.8, 10)],
            ),
            (
                0.5,
                vec![(fold, 0.0, 0.0, 0.0, 12), (all_in, -60.0, 20.0, 0.6, 12)],
            ),
        ];
        let combined = combine_world_values(&per_world).unwrap();
        assert_eq!(combined[0], (fold, 0.0, 0.0, 0.0));
        let mean = -55.0;
        let second = 0.5 * (40.0 + 2500.0) + 0.5 * (20.0 + 3600.0);
        assert_eq!(combined[1], (all_in, mean, second - mean * mean, 0.7));
    }

    #[test]
    fn combine_rejects_mismatched_world_action_sets() {
        let ok = vec![
            (0.5, vec![(Action::Fold, 1.0, 0.0, 0.0, 1)]),
            (0.5, vec![(Action::Fold, 2.0, 0.0, 0.0, 2)]),
        ];
        assert!(combine_world_values(&ok).is_ok());

        let mismatched = vec![
            (
                0.5,
                vec![
                    (Action::Fold, 1.0, 0.0, 0.0, 1),
                    (Action::Call, 0.0, 0.0, 0.0, 0),
                ],
            ),
            (0.5, vec![(Action::Fold, 2.0, 0.0, 0.0, 2)]),
        ];
        assert!(matches!(
            combine_world_values(&mismatched),
            Err(Error::Solver(_))
        ));

        let reordered = vec![
            (
                0.5,
                vec![
                    (Action::Fold, 1.0, 0.0, 0.0, 1),
                    (Action::Call, 0.0, 0.0, 0.0, 0),
                ],
            ),
            (
                0.5,
                vec![
                    (Action::Call, 0.0, 0.0, 0.0, 0),
                    (Action::Fold, 2.0, 0.0, 0.0, 2),
                ],
            ),
        ];
        assert!(matches!(
            combine_world_values(&reordered),
            Err(Error::Solver(_))
        ));

        assert_eq!(combine_world_values(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn solve_rejects_non_hero_turns_and_finished_hands() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(1)))
            .unwrap();
        // Hero on the button: first actor preflop is Opponent 2.
        let mut rng = seeded_rng(2);
        let ranges = [uniform(), uniform()];
        assert!(matches!(
            solve(&mut rng, &state, &ranges, &MctsConfig::test()),
            Err(Error::Solver(_))
        ));

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        assert!(matches!(
            solve(&mut rng, &state, &ranges, &MctsConfig::test()),
            Err(Error::Solver(_))
        ));
    }

    #[test]
    fn same_seed_produces_identical_results() {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(3)))
            .unwrap();
        let ranges = [uniform(), uniform()];
        let mut a = seeded_rng(4);
        let mut b = seeded_rng(4);
        let result_a = solve(&mut a, &state, &ranges, &MctsConfig::test()).unwrap();
        let result_b = solve(&mut b, &state, &ranges, &MctsConfig::test()).unwrap();
        assert_eq!(result_a, result_b);
    }

    #[test]
    fn results_are_sorted_by_ev_and_cover_candidates() {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(5)))
            .unwrap();
        let ranges = [uniform(), uniform()];
        let mut rng = seeded_rng(6);
        let result = solve(&mut rng, &state, &ranges, &MctsConfig::test()).unwrap();
        assert!(!result.actions.is_empty());
        assert_eq!(result.actions.len(), candidates(&state).len());
        for pair in result.actions.windows(2) {
            assert!(pair[0].ev >= pair[1].ev, "actions not sorted by EV");
        }
        assert!(result.actions.iter().all(|a| a.visits >= 1));
        let budget = MctsConfig::test().for_street(state.street());
        assert_eq!(
            result.worlds, budget.worlds,
            "the street-scaled world count is reported"
        );
        assert_eq!(result.iterations, budget.iterations);
        assert_eq!(result.max_depth, budget.max_depth);
    }

    /// Deals a hand from a custom deck and plays to a river where Opponent 1
    /// bets 100 into the hero (hero on the button, checked to the river).
    /// Board: `Th Jh Qh` flop, `2d` turn, `4d` river; hero's real hole cards
    /// are overwritten by `hero_hand`.
    fn river_facing_bet(hero_hand: [Card; 2]) -> GameState {
        let custom: Vec<Card> = deck_with([
            card(Rank::Ten, Suit::Hearts),
            card(Rank::Jack, Suit::Hearts),
            card(Rank::Queen, Suit::Hearts),
            card(Rank::Two, Suit::Diamonds),
            card(Rank::Four, Suit::Diamonds),
        ]);
        let mut deck = Deck::try_from_remaining(custom).unwrap();

        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck).unwrap();
        state.set_hole_cards(Seat::Hero, hero_hand);

        // Preflop: Opponent 2 calls, hero calls, Opponent 1 (BB) checks.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        // Flop and turn: opponents check, hero checks behind.
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        // River: Opponent 1 bets 100, Opponent 2 calls.
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Bet(100)).unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.street(), crate::game::Street::River);
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    /// A full 52-card deck order whose first five board cards are the given
    /// runout; the six cards before that are harmless hole cards.
    fn deck_with(runout: [Card; 5]) -> Vec<Card> {
        let mut cards: Vec<Card> = Vec::new();
        for rank in [
            Rank::Two,
            Rank::Three,
            Rank::Four,
            Rank::Five,
            Rank::Six,
            Rank::Seven,
        ] {
            cards.push(card(rank, Suit::Clubs));
        }
        cards.extend(runout);
        for suit in Suit::ALL {
            for rank in Rank::ALL {
                let candidate = card(rank, suit);
                if !cards.contains(&candidate) {
                    cards.push(candidate);
                }
            }
        }
        assert_eq!(cards.len(), 52, "custom deck must hold exactly 52 cards");
        cards
    }

    fn ev_of(result: &SolveResult, action: Action) -> f64 {
        result
            .actions
            .iter()
            .find(|a| a.action == action)
            .expect("action should have an estimate")
            .ev
    }

    #[test]
    fn nut_straight_flush_prefers_call_over_fold() {
        // Hero holds Kh 9h; board Th Jh Qh 2d 4d makes a K-high straight
        // flush. The only better hand (royal) needs the hero's Kh.
        let state = river_facing_bet([
            card(Rank::Nine, Suit::Hearts),
            card(Rank::King, Suit::Hearts),
        ]);
        let mut rng = seeded_rng(7);
        let ranges = [uniform(), uniform()];
        let result = solve(&mut rng, &state, &ranges, &MctsConfig::test()).unwrap();
        let call = ev_of(&result, Action::Call);
        let fold = ev_of(&result, Action::Fold);
        assert!(fold.abs() < 1.0, "folding loses nothing, got {fold}");
        assert!(
            call > fold + 20.0,
            "nut hand must prefer calling: call {call}, fold {fold}"
        );
    }

    #[test]
    fn junk_hand_against_aces_prefers_folding_to_calling() {
        // Hero holds 72o against two players whose ranges are 100% AA: a
        // call never wins on this dry board.
        let state = river_facing_bet([
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
        ]);
        let mut rng = seeded_rng(8);
        let aces = pinned_aces();
        let result = solve(&mut rng, &state, &[aces, aces], &MctsConfig::test()).unwrap();
        let call = ev_of(&result, Action::Call);
        let fold = ev_of(&result, Action::Fold);
        assert!(fold.abs() < 1.0, "folding loses nothing, got {fold}");
        assert!(
            call < fold - 50.0,
            "dead hand must not call: call {call}, fold {fold}"
        );
    }

    fn pinned_aces() -> Range {
        let mut weights = [0.0f32; HAND_COUNT];
        weights[Hand::new(Rank::Ace, Rank::Ace, false).index()] = 1.0;
        weights
    }

    #[test]
    fn folding_has_zero_risk_and_bust_probability() {
        let state = river_facing_bet([
            card(Rank::Nine, Suit::Hearts),
            card(Rank::King, Suit::Hearts),
        ]);
        let mut rng = seeded_rng(10);
        let ranges = [uniform(), uniform()];
        let result = solve(&mut rng, &state, &ranges, &MctsConfig::test()).unwrap();
        let fold = result
            .actions
            .iter()
            .find(|a| a.action == Action::Fold)
            .expect("fold should have an estimate");
        assert_eq!(fold.variance, 0.0, "folding has no payoff variance");
        assert_eq!(fold.bust_prob, 0.0, "folding never busts");
        assert_eq!(fold.sigma(), 0.0);
        for action in &result.actions {
            assert!(action.variance.is_finite() && action.variance >= 0.0);
            assert!(action.bust_prob.is_finite() && (0.0..=1.0).contains(&action.bust_prob));
        }
    }

    #[test]
    fn solve_completes_within_budget() {
        let state = river_facing_bet([
            card(Rank::Nine, Suit::Hearts),
            card(Rank::King, Suit::Hearts),
        ]);
        let mut rng = seeded_rng(9);
        let start = std::time::Instant::now();
        let result = solve(
            &mut rng,
            &state,
            &[uniform(), uniform()],
            &MctsConfig::test(),
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(!result.actions.is_empty());
        assert!(
            elapsed.as_secs() < 10,
            "solver took {elapsed:?}, exceeds test budget"
        );
    }
}
