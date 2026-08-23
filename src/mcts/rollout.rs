use rand::Rng;

use crate::card::{Card, Deck};
use crate::error::{Error, Result};
use crate::eval::HandClass;
use crate::game::{Action, ActionOutcome, GameState, Seat};
use crate::rng::{gen_index, weighted_index};

use super::actions::candidates;

/// Coarse strength tier of a made hand, used by the opponent rollout policy.
fn strength_tier(class: HandClass) -> f64 {
    match class {
        HandClass::HighCard => 0.0,
        HandClass::Pair => 1.0,
        HandClass::TwoPair | HandClass::Trips => 2.0,
        HandClass::Straight | HandClass::Flush => 3.0,
        HandClass::FullHouse | HandClass::Quads => 3.5,
        HandClass::StraightFlush => 4.0,
    }
}

/// Fold mass for the opponent policy: proportional to the price of calling
/// and falling with hand strength. Nutty holdings never fold.
fn fold_mass(tier: f64, price: f64) -> f64 {
    ((1.0 - tier / 4.0) * price).clamp(0.0, 0.95)
}

/// Raise mass: stronger hands raise more often, capped at 35%.
fn raise_mass(tier: f64) -> f64 {
    (0.02 + tier * 0.10).min(0.35)
}

/// Bet mass when checked to: stronger hands bet more often, capped at 60%.
fn bet_mass(tier: f64) -> f64 {
    (0.02 + tier * 0.18).min(0.6)
}

/// The price of calling as a share of the final pot (`to_call / (pot + to_call)`).
fn price_of(state: &GameState) -> f64 {
    let legal = state.legal_actions();
    if legal.call_amount == 0 {
        return 0.0;
    }
    let to_call = legal.call_amount as f64;
    let pot = state.total_pot() as f64;
    (to_call / (pot + to_call).max(1.0)).min(1.0)
}

/// Probability mass the opponent policy assigns to each candidate action in
/// `state` (index-aligned with [`candidates`](super::actions::candidates)).
///
/// A hand-strength/pot-odds heuristic: it folds weak hands in proportion to
/// the price of calling, calls lighter when the price is low, and bets or
/// raises more often with stronger holdings.
pub(crate) fn opponent_probs(state: &GameState) -> Vec<f64> {
    let cands = candidates(state);
    if cands.is_empty() {
        return Vec::new();
    }
    let tier = strength_tier(state.eval_hand(state.to_act()).class());
    let price = price_of(state);
    let to_call = state.legal_actions().call_amount as f64;

    let mut probs = vec![0.0f64; cands.len()];

    if to_call > 0.0 {
        let fold_index = cands.iter().position(|(a, _)| *a == Action::Fold);
        let call_index = cands.iter().position(|(a, _)| *a == Action::Call);
        let raise_indices: Vec<usize> = cands
            .iter()
            .enumerate()
            .filter(|(_, (a, _))| matches!(a, Action::Raise(_) | Action::Bet(_) | Action::AllIn))
            .map(|(i, _)| i)
            .collect();

        let p_fold = fold_mass(tier, price);
        let p_raise = if raise_indices.is_empty() {
            0.0
        } else {
            raise_mass(tier)
        };
        let p_call = (1.0 - p_fold - p_raise).max(0.0);

        if let Some(index) = fold_index {
            probs[index] = p_fold;
        }
        if let Some(index) = call_index {
            probs[index] = p_call;
        }
        for &index in &raise_indices {
            probs[index] = p_raise / raise_indices.len() as f64;
        }
    } else {
        let check_index = cands.iter().position(|(a, _)| *a == Action::Check);
        let bet_indices: Vec<usize> = cands
            .iter()
            .enumerate()
            .filter(|(_, (a, _))| matches!(a, Action::Bet(_) | Action::AllIn))
            .map(|(i, _)| i)
            .collect();

        let p_bet = if bet_indices.is_empty() {
            0.0
        } else {
            bet_mass(tier)
        };
        if let Some(index) = check_index {
            probs[index] = 1.0 - p_bet;
        }
        for &index in &bet_indices {
            probs[index] = p_bet / bet_indices.len() as f64;
        }
    }

    let total: f64 = probs.iter().sum();
    if total <= 0.0 {
        let each = 1.0 / probs.len() as f64;
        probs.fill(each);
    } else {
        for prob in &mut probs {
            *prob /= total;
        }
    }
    probs
}

/// Samples one opponent action from the [`opponent_probs`] policy.
pub(crate) fn opponent_action<R: Rng + ?Sized>(rng: &mut R, state: &GameState) -> Action {
    let cands = candidates(state);
    let probs = opponent_probs(state);
    let weights: Vec<f32> = probs.iter().map(|&p| p as f32).collect();
    if let Some(index) = weighted_index(rng, &weights)
        && let Some((action, _)) = cands.get(index)
    {
        return *action;
    }
    // Defensive fallback so a rollout never panics mid-hand.
    let legal = state.legal_actions();
    for action in [Action::Call, Action::Check, Action::AllIn, Action::Fold] {
        if legal.allows(action) {
            return action;
        }
    }
    Action::Check
}

/// Applies `action` to `state` and resolves its immediate consequences:
/// advancing the street (dealing from `runout` at `offset`) or running a
/// showdown when betting cannot continue. Returns the new runout offset.
///
/// `runout` holds only cards unknown at the root of the search, so the dealt
/// cards are exactly `runout[offset..]` and the offset advances by the number
/// of board cards dealt.
pub(crate) fn step(
    state: &mut GameState,
    action: Action,
    runout: &[Card],
    offset: usize,
) -> Result<usize> {
    let mut offset = offset;
    match state.apply_action(action)? {
        ActionOutcome::Continue | ActionOutcome::HandEnded => {}
        ActionOutcome::StreetEnded => {
            let mut deck = leftover_deck(runout, offset)?;
            let board_before = state.board().len();
            if state.can_continue_betting() && state.street().next().is_some() {
                state.advance_street(&mut deck)?;
                offset += state.board().len() - board_before;
            } else if !state.is_hand_over() {
                state.showdown(&mut deck)?;
                offset += state.board().len() - board_before;
            }
        }
    }
    Ok(offset)
}

fn leftover_deck(runout: &[Card], offset: usize) -> Result<Deck> {
    Deck::try_from_remaining(runout[offset..].to_vec())
        .ok_or_else(|| Error::Solver("runout exceeds deck capacity".into()))
}

/// Plays the remainder of the hand from `state` to the end and returns the
/// hero's chip delta, defined as the hero's final stack minus `baseline`
/// (the hero's stack at the decision point).
///
/// The hero plays a uniform random policy (this is the playout below the tree
/// horizon); opponents play the heuristic [`opponent_probs`] policy.
pub(crate) fn rollout<R: Rng + ?Sized>(
    rng: &mut R,
    state: &mut GameState,
    runout: &[Card],
    offset: usize,
    baseline: u32,
) -> Result<f64> {
    let mut offset = offset;
    while !state.is_hand_over() {
        let seat = state.to_act();
        let action = if seat == Seat::Hero {
            let actions = candidates(state);
            if actions.is_empty() {
                return Ok(state.stack(Seat::Hero) as f64 - f64::from(baseline));
            }
            actions[gen_index(rng, actions.len())].0
        } else {
            opponent_action(rng, state)
        };
        offset = step(state, action, runout, offset)?;
    }
    Ok(state.stack(Seat::Hero) as f64 - f64::from(baseline))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::card::{Card, Deck, Rank, Suit};
    use crate::eval::HandClass;
    use crate::game::blinds::BlindLevel;
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn hero_open_state() -> GameState {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(1)))
            .unwrap();
        state
    }

    #[test]
    fn strength_tiers_form_a_ladder() {
        assert_eq!(strength_tier(HandClass::HighCard), 0.0);
        assert_eq!(strength_tier(HandClass::Pair), 1.0);
        assert_eq!(strength_tier(HandClass::TwoPair), 2.0);
        assert_eq!(strength_tier(HandClass::Trips), 2.0);
        assert_eq!(strength_tier(HandClass::Straight), 3.0);
        assert_eq!(strength_tier(HandClass::Flush), 3.0);
        assert_eq!(strength_tier(HandClass::FullHouse), 3.5);
        assert_eq!(strength_tier(HandClass::Quads), 3.5);
        assert_eq!(strength_tier(HandClass::StraightFlush), 4.0);
    }

    #[test]
    fn fold_mass_respects_strength_and_price() {
        assert_eq!(fold_mass(4.0, 0.5), 0.0, "nuts must never fold");
        assert_eq!(fold_mass(4.0, 1.0), 0.0);
        assert!(
            (fold_mass(0.0, 0.9) - 0.9).abs() < 1e-9,
            "junk folds at high price"
        );
        assert_eq!(fold_mass(0.0, 0.05), 0.05, "junk calls at low price");
        assert!(fold_mass(0.0, 1.0) <= 0.95, "fold mass is capped");
        assert!(
            fold_mass(3.0, 0.5) < fold_mass(0.0, 0.5),
            "strength lowers fold mass"
        );
    }

    #[test]
    fn raise_and_bet_mass_grow_with_strength_and_are_capped() {
        assert!(raise_mass(0.0) < raise_mass(4.0));
        assert!(raise_mass(4.0) <= 0.35);
        assert!(bet_mass(0.0) < bet_mass(4.0));
        assert!(bet_mass(4.0) <= 0.6);
    }

    #[test]
    fn opponent_probs_are_a_valid_distribution() {
        let state = hero_open_state();
        let cands = candidates(&state);
        let probs = opponent_probs(&state);
        assert_eq!(probs.len(), cands.len());
        assert!(
            (probs.iter().sum::<f64>() - 1.0).abs() < 1e-9,
            "sums to one"
        );
        assert!(probs.iter().all(|&p| p >= 0.0));
    }

    #[test]
    fn opponent_action_is_always_legal_and_seed_stable() {
        let state = hero_open_state();
        for seed in 0..64u64 {
            let mut rng = seeded_rng(seed);
            let action = opponent_action(&mut rng, &state);
            assert!(
                state.legal_actions().allows(action),
                "seed {seed} produced illegal {action:?}"
            );
        }
        let mut a = seeded_rng(99);
        let mut b = seeded_rng(99);
        assert_eq!(
            opponent_action(&mut a, &state),
            opponent_action(&mut b, &state)
        );
    }

    #[test]
    fn step_advances_streets_and_offsets() {
        let mut state = hero_open_state();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        let runout: Vec<Card> = (0..46)
            .map(|i| {
                Card::new(
                    match i % 13 {
                        0 => Rank::Two,
                        _ => Rank::Three,
                    },
                    Suit::Clubs,
                )
            })
            .collect();
        // BB check closes the street: advance deals the flop (3 cards).
        let offset = step(&mut state, Action::Check, &runout, 0).unwrap();
        assert_eq!(offset, 3);
        assert_eq!(state.street(), crate::game::Street::Flop);
    }

    #[test]
    fn rollout_terminates_and_scores_stacks() {
        let state = hero_open_state();
        let ranges = [[1.0f32 / 169.0; 169]; 2];
        let mut rng = seeded_rng(2);
        let worlds =
            crate::mcts::world::WorldSampler::sample(&mut rng, &state, &ranges, 8).unwrap();
        for world in &worlds {
            let mut replica = world.build_state(&state);
            let baseline = state.stack(Seat::Hero);
            let delta = rollout(&mut rng, &mut replica, &world.runout, 0, baseline).unwrap();
            assert!(delta.is_finite());
            assert!(delta.abs() <= 1500.0, "impossible stack swing {delta}");
        }
    }

    #[test]
    fn rollout_from_terminal_state_is_instant() {
        let mut state = hero_open_state();
        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(state.is_hand_over());
        let dummy = [Card::new(Rank::Two, Suit::Clubs); 2];
        let mut replica = state.clone_with_hole_cards([dummy; 3]);
        let mut rng = seeded_rng(3);
        let delta = rollout(&mut rng, &mut replica, &[], 0, 490).unwrap();
        assert!((delta - (replica.stack(Seat::Hero) as f64 - 490.0)).abs() < 1e-9);
    }

    #[test]
    fn many_rollouts_never_panic_and_vary() {
        let state = hero_open_state();
        let ranges = [[1.0f32 / 169.0; 169]; 2];
        let mut rng = seeded_rng(4);
        let worlds =
            crate::mcts::world::WorldSampler::sample(&mut rng, &state, &ranges, 32).unwrap();
        let mut seen = HashSet::new();
        for world in &worlds {
            let mut replica = world.build_state(&state);
            let delta = rollout(&mut rng, &mut replica, &world.runout, 0, 490).unwrap();
            assert!(delta.is_finite());
            seen.insert(delta.to_bits());
        }
        assert!(seen.len() > 1, "rollouts should vary");
    }

    #[test]
    fn price_of_is_zero_when_not_facing_a_bet() {
        let mut state = hero_open_state();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state
            .advance_street(&mut Deck::shuffled(&mut seeded_rng(5)))
            .unwrap();
        state.apply_action(Action::Check).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        assert_eq!(price_of(&state), 0.0);
    }

    #[test]
    fn hero_rollout_policy_covers_legal_candidates() {
        let state = hero_open_state();
        let legal = state.legal_actions();
        let actions = candidates(&state);
        for (action, _) in actions {
            assert!(legal.allows(action));
        }
    }
}
