pub mod objective;

pub use objective::{DerivedRisk, SurvivalConfig, survival_score};

use std::cmp::Ordering;

use rand::Rng;

use crate::error::{Error, Result};
use crate::game::{Action, GameState, Seat};
use crate::mcts::{self, MctsConfig};
use crate::range::BetSize;
use crate::range::hands::Range;

/// One candidate action as seen by the decision layer: the solver's chip-EV
/// and risk estimates, plus the survivability score used for ranking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Analysis {
    pub action: Action,
    pub bucket: Option<BetSize>,
    pub ev: f64,
    pub variance: f64,
    pub bust_prob: f64,
    pub score: f64,
    pub visits: u64,
}

impl Analysis {
    /// The standard deviation of the action's payoff, in chips.
    pub fn sigma(&self) -> f64 {
        self.variance.sqrt()
    }
}

/// How a played action compares to the survivability-optimal one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayedEvaluation {
    /// The solver/risk analysis of the played action itself.
    pub analysis: Analysis,
    /// The chip EV given up relative to the optimal action, normalized to
    /// big blinds (0.0 when the played action is the optimal one). Meant for
    /// human display — an intuitive, real quantity ("you gave up 3 BB here").
    pub ev_loss_bb: f64,
    /// The same EV given up, normalized instead to the pot at the decision
    /// point (0.0 when the played action is the optimal one). This is what
    /// the blunder tracker calibrates against: a fixed BB amount means much
    /// more relative to a small preflop pot than to a big river one, so a
    /// pot-fraction basis is what actually makes a preflop mistake count as
    /// heavily as an equally-bad river mistake.
    pub ev_loss_pot: f64,
    /// Whether the played action is exactly the survivability-optimal one.
    pub is_optimal: bool,
}

/// How wide and deep the solve behind a decision actually went, echoed for
/// the UI so the search effort can be audited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchReport {
    /// Determinizations sampled from the opponent ranges.
    pub worlds: usize,
    /// Per-world iteration budget after street scaling.
    pub iterations: usize,
    /// Tree-depth cap in hero decisions after street scaling.
    pub max_depth: usize,
    /// Deepest tree node actually expanded.
    pub max_tree_depth: usize,
    /// Total tree nodes expanded across all worlds.
    pub nodes: usize,
    /// Total actions simulated in rollouts across all worlds.
    pub rollout_actions: u64,
}

/// The outcome of analyzing the hero's current decision node.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzedDecision {
    /// Every candidate, best survivability score first.
    pub ranking: Vec<Analysis>,
    /// The single strictly-best action: the highest survivability score,
    /// ties broken by chip EV, then bust probability, then variance.
    pub optimal: Analysis,
    /// The evaluation of the played action, when one was submitted.
    pub played: Option<PlayedEvaluation>,
    /// The solve's realized search effort.
    pub search: SearchReport,
}

/// Derives the survivability coefficients for the hero's stack, rescaled to
/// this hand's actual table context (see [`SurvivalConfig::for_hand`]) rather
/// than the static config alone.
fn hand_risk(state: &GameState, survival_config: &SurvivalConfig, stack: u32) -> Result<DerivedRisk> {
    let opponent_stacks: Vec<u32> = state
        .active_seats()
        .into_iter()
        .filter(|&seat| seat != Seat::Hero)
        .map(|seat| state.stack(seat))
        .collect();
    let big_blind = state.blind_level().big_blind;
    survival_config
        .for_hand(stack, &opponent_stacks, big_blind)
        .derive(stack)
}

/// Validates a player-submitted action against the current state: the hand
/// must be live, it must be the hero's turn, and the action must be legal.
pub fn validate_action(state: &GameState, action: Action) -> Result<()> {
    if state.is_hand_over() {
        return Err(Error::Decision(
            "cannot submit an action on a finished hand".into(),
        ));
    }
    if state.to_act() != Seat::Hero {
        return Err(Error::Decision("not the hero's turn to act".into()));
    }
    if !state.legal_actions().allows(action) {
        return Err(Error::Decision(format!(
            "illegal action {action:?} in the current state"
        )));
    }
    Ok(())
}

/// Analyzes the hero's current decision: runs one MCTS solve, scores every
/// candidate with the survivability objective, and — when a played action is
/// supplied — evaluates it against the optimal action.
///
/// A played action that is not one of the standard bucket candidates (e.g. an
/// exact bet-slider amount) is injected into the root candidate set so its EV
/// and risk profile are measured exactly.
pub fn analyze<R: Rng + ?Sized>(
    rng: &mut R,
    state: &GameState,
    ranges: &[Range; 2],
    mcts_config: &MctsConfig,
    survival_config: &SurvivalConfig,
    played: Option<Action>,
) -> Result<AnalyzedDecision> {
    if state.is_hand_over() {
        return Err(Error::Decision("cannot analyze a hand that is over".into()));
    }
    if state.to_act() != Seat::Hero {
        return Err(Error::Decision("not the hero's turn to act".into()));
    }
    if let Some(action) = played {
        validate_action(state, action)?;
    }

    let mut candidates = mcts::candidates(state);
    if let Some(action) = played
        && !candidates.iter().any(|(existing, _)| *existing == action)
    {
        candidates.push((action, classify_played(state, action)));
    }

    let stack = state.stack(Seat::Hero);
    let risk = hand_risk(state, survival_config, stack)?;
    let result = mcts::solve_with_candidates(rng, state, ranges, mcts_config, &candidates)?;

    let mut analyses: Vec<Analysis> = result
        .actions
        .iter()
        .map(|value| Analysis {
            action: value.action,
            bucket: value.bucket,
            ev: value.ev,
            variance: value.variance,
            bust_prob: value.bust_prob,
            score: risk.score(value.ev, value.variance, value.bust_prob),
            visits: value.visits,
        })
        .collect();
    analyses.sort_by(rank_desc);

    let optimal = analyses
        .first()
        .copied()
        .ok_or_else(|| Error::Decision("no candidate actions to rank".into()))?;

    let big_blind = f64::from(state.blind_level().big_blind);
    let pot = f64::from(state.total_pot()).max(1.0);
    let played = played
        .map(|action| -> Result<PlayedEvaluation> {
            let analysis = analyses
                .iter()
                .find(|candidate| candidate.action == action)
                .copied()
                .ok_or_else(|| Error::Decision("played action missing from the analysis".into()))?;
            let ev_loss = (optimal.ev - analysis.ev).max(0.0);
            Ok(PlayedEvaluation {
                analysis,
                ev_loss_bb: ev_loss / big_blind,
                ev_loss_pot: ev_loss / pot,
                is_optimal: action == optimal.action,
            })
        })
        .transpose()?;

    Ok(AnalyzedDecision {
        ranking: analyses,
        optimal,
        played,
        search: SearchReport {
            worlds: result.worlds,
            iterations: result.iterations,
            max_depth: result.max_depth,
            max_tree_depth: result.max_tree_depth,
            nodes: result.nodes,
            rollout_actions: result.rollout_actions,
        },
    })
}

/// Scores a ready-made solver snapshot exactly like [`analyze`] scores a
/// fresh solve: no search runs, so submissions answer instantly from the
/// background searcher's latest result. Errors when the played action is not
/// covered by the snapshot (e.g. an off-bucket slider amount) — callers fall
/// back to a full [`analyze`] in that rare case.
pub fn analyze_snapshot(
    state: &GameState,
    snapshot: &crate::mcts::SolveResult,
    survival_config: &SurvivalConfig,
    played: Option<Action>,
) -> Result<AnalyzedDecision> {
    if state.is_hand_over() {
        return Err(Error::Decision("cannot analyze a hand that is over".into()));
    }
    if state.to_act() != Seat::Hero {
        return Err(Error::Decision("not the hero's turn to act".into()));
    }
    if let Some(action) = played {
        validate_action(state, action)?;
    }

    let stack = state.stack(Seat::Hero);
    let risk = hand_risk(state, survival_config, stack)?;

    let mut analyses: Vec<Analysis> = snapshot
        .actions
        .iter()
        .map(|value| Analysis {
            action: value.action,
            bucket: value.bucket,
            ev: value.ev,
            variance: value.variance,
            bust_prob: value.bust_prob,
            score: risk.score(value.ev, value.variance, value.bust_prob),
            visits: value.visits,
        })
        .collect();
    analyses.sort_by(rank_desc);

    let optimal = analyses
        .first()
        .copied()
        .ok_or_else(|| Error::Decision("no candidate actions to rank".into()))?;

    let big_blind = f64::from(state.blind_level().big_blind);
    let pot = f64::from(state.total_pot()).max(1.0);
    let played = played
        .map(|action| -> Result<PlayedEvaluation> {
            let analysis = analyses
                .iter()
                .find(|candidate| candidate.action == action)
                .copied()
                .ok_or_else(|| {
                    Error::Decision("played action missing from the analysis snapshot".into())
                })?;
            let ev_loss = (optimal.ev - analysis.ev).max(0.0);
            Ok(PlayedEvaluation {
                analysis,
                ev_loss_bb: ev_loss / big_blind,
                ev_loss_pot: ev_loss / pot,
                is_optimal: action == optimal.action,
            })
        })
        .transpose()?;

    Ok(AnalyzedDecision {
        ranking: analyses,
        optimal,
        played,
        search: SearchReport {
            worlds: snapshot.worlds,
            iterations: snapshot.iterations,
            max_depth: snapshot.max_depth,
            max_tree_depth: snapshot.max_tree_depth,
            nodes: snapshot.nodes,
            rollout_actions: snapshot.rollout_actions,
        },
    })
}

/// Classifies a played bet/raise amount into a size bucket for feedback.
/// Shared with the opponent-skill analyzer, which grades actions the same
/// way.
pub(crate) fn classify_played(state: &GameState, action: Action) -> Option<BetSize> {
    let legal = state.legal_actions();
    let (amount, min_amount) = match action {
        Action::Bet(amount) => (amount, legal.min_bet),
        Action::Raise(amount) => (amount, legal.min_raise_to),
        _ => return None,
    };
    Some(BetSize::classify(
        state.street(),
        amount,
        state.total_pot(),
        legal.call_amount,
        state.blind_level().big_blind,
        min_amount,
        state.stack(Seat::Hero),
    ))
}

/// A total order over analyses: highest survivability score first, then
/// higher chip EV, then lower bust probability, then lower payoff variance.
/// `sort_by` is stable, so candidate order breaks any remaining tie.
fn rank_desc(a: &Analysis, b: &Analysis) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then(b.ev.total_cmp(&a.ev))
        .then(a.bust_prob.total_cmp(&b.bust_prob))
        .then(a.variance.total_cmp(&b.variance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Deck, Rank, Suit};
    use crate::game::Seat;
    use crate::game::Street;
    use crate::game::blinds::BlindLevel;
    use crate::game::state::GameState;
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

    fn pinned(hand: Hand) -> Range {
        let mut weights = [0.0f32; HAND_COUNT];
        weights[hand.index()] = 1.0;
        weights
    }

    fn aces() -> Range {
        pinned(Hand::new(Rank::Ace, Rank::Ace, false))
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

    /// Deals a hand and plays to a river where Opponent 1 bets 100 into the
    /// hero (hero on the button, checked to the river). The hero's real hole
    /// cards are overwritten by `hero_hand`.
    /// Board: `Th Jh Qh` flop, `2d` turn, `4d` river.
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

        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Bet(100)).unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.street(), Street::River);
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    fn junk_hand() -> [Card; 2] {
        [
            card(Rank::Seven, Suit::Diamonds),
            card(Rank::Two, Suit::Clubs),
        ]
    }

    #[test]
    fn validate_action_rejects_bad_submissions() {
        let state = river_facing_bet(junk_hand());
        assert!(validate_action(&state, Action::Call).is_ok());

        assert!(matches!(
            validate_action(&state, Action::Check),
            Err(Error::Decision(_))
        ));
        assert!(matches!(
            validate_action(&state, Action::Bet(400)),
            Err(Error::Decision(_))
        ));
    }

    #[test]
    fn validate_action_rejects_wrong_turn_and_finished_hands() {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(11)))
            .unwrap();
        let legal_call = state.legal_actions().can_call;
        let action = if legal_call {
            Action::Call
        } else {
            Action::Check
        };
        assert!(matches!(
            validate_action(&state, action),
            Err(Error::Decision(_))
        ));

        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        assert!(matches!(
            validate_action(&state, Action::Check),
            Err(Error::Decision(_))
        ));
    }

    #[test]
    fn analyze_ranks_by_survival_score_and_reports_ev_loss() {
        let state = river_facing_bet(junk_hand());
        let mut rng = seeded_rng(12);
        let result = analyze(
            &mut rng,
            &state,
            &[aces(), aces()],
            &MctsConfig::test(),
            &SurvivalConfig::default(),
            Some(Action::Call),
        )
        .unwrap();

        for pair in result.ranking.windows(2) {
            assert!(
                rank_desc(&pair[0], &pair[1]) != Ordering::Greater,
                "ranking not sorted by survivability"
            );
        }
        assert_eq!(result.ranking.len(), mcts::candidates(&state).len());
        assert_eq!(result.ranking[0], result.optimal);

        let fold = result
            .ranking
            .iter()
            .find(|a| a.action == Action::Fold)
            .unwrap();
        assert_eq!(fold.bust_prob, 0.0);
        assert_eq!(fold.variance, 0.0);
        assert_eq!(fold.sigma(), 0.0);

        let played = result.played.unwrap();
        assert!(!played.is_optimal);
        assert_eq!(
            played.ev_loss_bb,
            (result.optimal.ev - played.analysis.ev).max(0.0) / 20.0,
            "EV loss is reported in big blinds (BB = 20)"
        );
    }

    #[test]
    fn all_in_bust_probability_is_measured() {
        // Hero faces a river bet with a 100-chip stack: only fold and all-in.
        let mut state = river_facing_bet(junk_hand());
        state.set_stack(Seat::Hero, 100);
        let mut rng = seeded_rng(13);
        let result = analyze(
            &mut rng,
            &state,
            &[uniform(), uniform()],
            &MctsConfig::test(),
            &SurvivalConfig::default(),
            Some(Action::Fold),
        )
        .unwrap();

        let fold = result
            .ranking
            .iter()
            .find(|a| a.action == Action::Fold)
            .unwrap();
        let all_in = result
            .ranking
            .iter()
            .find(|a| a.action == Action::AllIn)
            .unwrap();
        assert_eq!(fold.bust_prob, 0.0);
        assert!(all_in.bust_prob > 0.0, "calling for the stack can bust");
        assert!(all_in.variance > 0.0);

        let played = result.played.unwrap();
        assert!(played.is_optimal, "folding should be optimal here");
        assert_eq!(played.ev_loss_bb, 0.0);
    }

    #[test]
    fn slider_amounts_are_injected_as_candidates() {
        let mut state = river_facing_bet(junk_hand());
        state.set_stack(Seat::Hero, 400);
        let played = Action::Raise(250);
        assert!(
            !mcts::candidates(&state)
                .iter()
                .any(|(action, _)| *action == played),
            "fixture should use an off-bucket amount"
        );

        let mut rng = seeded_rng(14);
        let result = analyze(
            &mut rng,
            &state,
            &[uniform(), uniform()],
            &MctsConfig::test(),
            &SurvivalConfig::default(),
            Some(played),
        )
        .unwrap();

        let analysis = result.ranking.iter().find(|a| a.action == played).unwrap();
        assert!(analysis.bucket.is_some());
        assert_eq!(result.ranking.len(), mcts::candidates(&state).len() + 1);
        assert_eq!(result.played.unwrap().analysis.action, played);
    }

    #[test]
    fn analyze_rejects_invalid_requests() {
        let mut rng = seeded_rng(15);
        let ranges = [uniform(), uniform()];
        let config = MctsConfig::test();
        let survival = SurvivalConfig::default();

        let state = river_facing_bet(junk_hand());
        assert!(matches!(
            analyze(
                &mut rng,
                &state,
                &ranges,
                &config,
                &survival,
                Some(Action::Check)
            ),
            Err(Error::Decision(_))
        ));

        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(16)))
            .unwrap();
        assert!(matches!(
            analyze(
                &mut rng,
                &state,
                &ranges,
                &config,
                &survival,
                Some(Action::Call)
            ),
            Err(Error::Decision(_))
        ));

        let mut finished = GameState::new(Seat::Hero, level());
        finished
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(16)))
            .unwrap();
        finished.apply_action(Action::Fold).unwrap();
        finished.apply_action(Action::Fold).unwrap();
        assert!(finished.is_hand_over());
        assert!(matches!(
            analyze(&mut rng, &finished, &ranges, &config, &survival, None),
            Err(Error::Decision(_))
        ));
    }

    #[test]
    fn analyze_snapshot_scores_identically_to_a_fresh_solve() {
        let state = river_facing_bet(junk_hand());
        let ranges = [aces(), aces()];
        let snapshot =
            mcts::solve(&mut seeded_rng(12), &state, &ranges, &MctsConfig::test()).unwrap();
        let from_snapshot = analyze_snapshot(
            &state,
            &snapshot,
            &SurvivalConfig::default(),
            Some(Action::Call),
        )
        .unwrap();
        let full = analyze(
            &mut seeded_rng(12),
            &state,
            &ranges,
            &MctsConfig::test(),
            &SurvivalConfig::default(),
            Some(Action::Call),
        )
        .unwrap();
        assert_eq!(from_snapshot, full, "snapshot scoring shortcuts the solve");
    }

    #[test]
    fn analyze_snapshot_rejects_missing_actions_and_bad_states() {
        let mut state = river_facing_bet(junk_hand());
        state.set_stack(Seat::Hero, 400);
        let snapshot = mcts::solve(
            &mut seeded_rng(20),
            &state,
            &[uniform(), uniform()],
            &MctsConfig::test(),
        )
        .unwrap();
        let off_bucket = Action::Raise(250);
        assert!(
            mcts::candidates(&state)
                .iter()
                .all(|(candidate, _)| *candidate != off_bucket),
            "fixture should use an off-bucket amount"
        );
        assert!(matches!(
            analyze_snapshot(
                &state,
                &snapshot,
                &SurvivalConfig::default(),
                Some(off_bucket)
            ),
            Err(Error::Decision(_))
        ));

        let mut wrong = GameState::new(Seat::Hero, level());
        wrong
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(21)))
            .unwrap();
        assert!(matches!(
            analyze_snapshot(&wrong, &snapshot, &SurvivalConfig::default(), None),
            Err(Error::Decision(_))
        ));
    }

    #[test]
    fn analyze_is_deterministic_for_a_seed() {
        let state = river_facing_bet(junk_hand());
        let ranges = [uniform(), uniform()];
        let config = MctsConfig::test();
        let survival = SurvivalConfig::default();

        let mut a = seeded_rng(17);
        let mut b = seeded_rng(17);
        let first = analyze(&mut a, &state, &ranges, &config, &survival, None).unwrap();
        let second = analyze(&mut b, &state, &ranges, &config, &survival, None).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn score_and_ev_agree_but_risk_resolves_close_calls() {
        let config = SurvivalConfig::default();
        let risk = config.derive(500).unwrap();

        let safe = Analysis {
            action: Action::Call,
            bucket: None,
            ev: 10.0,
            variance: 100.0,
            bust_prob: 0.0,
            score: risk.score(10.0, 100.0, 0.0),
            visits: 1,
        };
        let busty = Analysis {
            action: Action::AllIn,
            bucket: Some(BetSize::AllIn),
            ev: 20.0,
            variance: 90_000.0,
            bust_prob: 0.4,
            score: risk.score(20.0, 90_000.0, 0.4),
            visits: 1,
        };
        assert!(safe.ev < busty.ev);
        assert_eq!(rank_desc(&safe, &busty), Ordering::Less);
    }

    /// Regression for the junk-hand coaching complaint: with 6♠3♦ preflop
    /// against two 100%-AA ranges the solver must resolve fold as the best
    /// action — no "you should've bet" noise from an under-searched preflop.
    #[test]
    fn junk_preflop_hand_never_recommends_betting_against_aces() {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(18)))
            .unwrap();
        state.set_hole_cards(
            Seat::Hero,
            [
                card(Rank::Six, Suit::Spades),
                card(Rank::Three, Suit::Diamonds),
            ],
        );
        assert_eq!(state.to_act(), Seat::Hero);
        assert_eq!(state.street(), Street::Preflop);

        let mut rng = seeded_rng(19);
        let result = analyze(
            &mut rng,
            &state,
            &[aces(), aces()],
            &MctsConfig::test(),
            &SurvivalConfig::default(),
            None,
        )
        .unwrap();

        assert_eq!(
            result.optimal.action,
            Action::Fold,
            "63o cannot open against two pinned AA ranges: {:#?}",
            result.ranking
        );
        for candidate in &result.ranking {
            if matches!(
                candidate.action,
                Action::Bet(_) | Action::Raise(_) | Action::AllIn
            ) {
                assert!(
                    candidate.score <= result.optimal.score,
                    "a raise scored above fold for 63o: {candidate:?}"
                );
            }
        }
        assert!(
            result.search.iterations >= MctsConfig::test().iterations,
            "the preflop street budget was applied"
        );
    }
}
