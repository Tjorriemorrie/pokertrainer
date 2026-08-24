use crate::game::{Action, GameState, Street};
use crate::range::BetSize;

/// The GGPoker-style size buckets offered as raises/bets on each street.
/// (`AllIn` is added separately so all-in semantics stay exact.)
fn size_buckets(street: Street) -> &'static [BetSize] {
    match street {
        Street::Preflop => &[
            BetSize::Min,
            BetSize::ThreeBb,
            BetSize::FourBb,
            BetSize::Pot,
        ],
        _ => &[
            BetSize::Min,
            BetSize::ThirdPot,
            BetSize::HalfPot,
            BetSize::ThreeQuarterPot,
            BetSize::Pot,
            BetSize::Overbet,
        ],
    }
}

/// The candidate actions the hero may take in `state`, each paired with the
/// bet-size bucket it corresponds to (`None` for fold/check/call).
///
/// Bet/raise sizes come from the [`BetSize`] buckets converted to concrete
/// chips and clamped to the legal bounds; sizes that collapse onto the same
/// amount are deduplicated, and any size committing the whole stack becomes
/// `Action::AllIn`.
pub fn candidates(state: &GameState) -> Vec<(Action, Option<BetSize>)> {
    let legal = state.legal_actions();
    let mut out: Vec<(Action, Option<BetSize>)> = Vec::new();

    fn push(
        out: &mut Vec<(Action, Option<BetSize>)>,
        legal: &crate::game::LegalActions,
        action: Action,
        bucket: Option<BetSize>,
    ) {
        if !legal.allows(action) {
            return;
        }
        if out.iter().any(|(existing, _)| *existing == action) {
            return;
        }
        out.push((action, bucket));
    }

    push(&mut out, &legal, Action::Fold, None);
    push(&mut out, &legal, Action::Check, None);
    push(&mut out, &legal, Action::Call, None);

    let street = state.street();
    let pot = state.total_pot();
    let to_call = legal.call_amount;
    let big_blind = state.blind_level().big_blind;
    let stack = state.stack(state.to_act());

    if legal.can_bet && stack > 0 {
        for &bucket in size_buckets(street) {
            let amount = bucket
                .to_raise_to(pot, to_call, big_blind, legal.min_bet, stack)
                .clamp(legal.min_bet, legal.max_bet);
            if amount >= legal.max_bet {
                push(&mut out, &legal, Action::AllIn, Some(BetSize::AllIn));
            } else {
                let label = BetSize::classify(
                    street,
                    amount,
                    pot,
                    to_call,
                    big_blind,
                    legal.min_bet,
                    stack,
                );
                push(&mut out, &legal, Action::Bet(amount), Some(label));
            }
        }
    }

    if legal.can_raise && legal.min_raise_to <= legal.max_raise_to {
        let facing_raise_preflop =
            street == Street::Preflop && to_call > 0 && state.current_bet() > big_blind;
        let buckets: &[BetSize] = if facing_raise_preflop {
            &[
                BetSize::TwoX,
                BetSize::ThirdPot,
                BetSize::HalfPot,
                BetSize::ThreeQuarterPot,
                BetSize::Pot,
            ]
        } else {
            size_buckets(street)
        };
        for &bucket in buckets {
            let amount = bucket
                .to_raise_to(pot, to_call, big_blind, legal.min_raise_to, stack)
                .clamp(legal.min_raise_to, legal.max_raise_to);
            if amount >= legal.max_raise_to {
                push(&mut out, &legal, Action::AllIn, Some(BetSize::AllIn));
            } else {
                let label = if facing_raise_preflop {
                    bucket
                } else {
                    BetSize::classify(
                        street,
                        amount,
                        pot,
                        to_call,
                        big_blind,
                        legal.min_raise_to,
                        stack,
                    )
                };
                push(&mut out, &legal, Action::Raise(amount), Some(label));
            }
        }
    }

    push(&mut out, &legal, Action::AllIn, Some(BetSize::AllIn));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Deck, Rank, Suit};
    use crate::game::blinds::BlindLevel;
    use crate::game::seat::Seat;
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn actions_of(state: &GameState) -> Vec<Action> {
        candidates(state)
            .into_iter()
            .map(|(action, _)| action)
            .collect()
    }

    /// Deals a hand where the hero is first to act preflop (button on
    /// Opponent 1, so the order is Hero, Opponent 1, Opponent 2).
    fn hero_open_state() -> GameState {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(1)))
            .unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    /// Deals a hand with the hero on the button and plays to a flop where the
    /// hero is last to act facing two checks (pot 60, hero stack 470).
    fn hero_flop_bet_state() -> GameState {
        let mut state = GameState::new(Seat::Hero, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(2)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state
            .advance_street(&mut Deck::shuffled(&mut seeded_rng(3)))
            .unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    #[test]
    fn preflop_open_offers_fold_call_all_buckets_and_all_in() {
        let state = hero_open_state();
        let cands = candidates(&state);
        let actions = actions_of(&state);
        assert!(actions.contains(&Action::Fold));
        assert!(actions.contains(&Action::Call));
        assert!(!actions.contains(&Action::Check));
        assert!(actions.contains(&Action::Raise(40)));
        assert!(actions.contains(&Action::Raise(60)));
        assert!(actions.contains(&Action::Raise(70)));
        assert!(actions.contains(&Action::Raise(80)));
        assert!(actions.contains(&Action::AllIn));
        for (action, _) in &cands {
            assert!(
                state.legal_actions().allows(*action),
                "{action:?} not legal"
            );
        }
    }

    #[test]
    fn postflop_bet_spot_offers_check_buckets_and_all_in() {
        let state = hero_flop_bet_state();
        let actions = actions_of(&state);
        assert!(actions.contains(&Action::Check));
        assert!(!actions.contains(&Action::Fold));
        assert!(!actions.contains(&Action::Call));
        assert!(actions.contains(&Action::Bet(20)));
        assert!(actions.contains(&Action::Bet(30)));
        assert!(actions.contains(&Action::Bet(45)));
        assert!(actions.contains(&Action::Bet(60)));
        assert!(actions.contains(&Action::Bet(120)));
        assert!(actions.contains(&Action::AllIn));
    }

    #[test]
    fn facing_a_raise_preflop_offers_two_x_and_pot_fraction_raises() {
        // Hero calls, Opponent 1 raises to 100, Opponent 2 folds: the hero
        // now faces 80 more into a pot of 150 with 280 behind.
        let mut state = hero_open_state();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Raise(100)).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let cands = candidates(&state);
        let actions = actions_of(&state);
        assert!(actions.contains(&Action::Fold));
        assert!(actions.contains(&Action::Call));
        assert!(actions.contains(&Action::AllIn));
        // 2x the call (160) clamps to the min-raise (180).
        assert!(actions.contains(&Action::Raise(180)));
        // 1/2 pot (190) and 3/4 pot (245) survive; pot (300) collapses to all-in.
        assert!(actions.contains(&Action::Raise(190)));
        assert!(actions.contains(&Action::Raise(245)));
        assert!(
            cands
                .iter()
                .any(|(a, b)| *a == Action::Raise(180) && *b == Some(BetSize::TwoX))
        );
        assert!(
            cands
                .iter()
                .all(|(a, _)| !matches!(a, Action::Raise(amount) if *amount > 280)),
            "every offered raise stays within the hero stack: {cands:?}"
        );
    }

    #[test]
    fn sizes_clamping_to_the_stack_collapse_into_a_single_all_in() {
        let mut state = hero_flop_bet_state();
        state.set_stack(Seat::Hero, 60);

        let actions = actions_of(&state);
        assert_eq!(
            actions.iter().filter(|a| **a == Action::AllIn).count(),
            1,
            "expected exactly one all-in candidate: {actions:?}"
        );
        assert!(!actions.contains(&Action::Bet(60)));
        assert!(actions.contains(&Action::Check));
        assert!(actions.contains(&Action::Bet(20)));
    }

    #[test]
    fn short_facing_a_big_bet_offers_only_fold_call_and_all_in() {
        // Facing an 80-chip call with only 150 chips left, the min-raise
        // (to 180) exceeds the hero's stack, so only an all-in remains.
        let mut state = hero_open_state();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Raise(100)).unwrap();
        state.apply_action(Action::Fold).unwrap();
        state.set_stack(Seat::Hero, 150);
        assert_eq!(state.legal_actions().call_amount, 80);

        let actions = actions_of(&state);
        assert!(actions.contains(&Action::Fold));
        assert!(actions.contains(&Action::Call));
        assert!(actions.contains(&Action::AllIn));
        assert!(!actions.iter().any(|a| matches!(a, Action::Raise(_))));
    }

    #[test]
    fn candidate_buckets_round_trip_through_classify() {
        let state = hero_flop_bet_state();
        let legal = state.legal_actions();
        let (pot, to_call, bb, stack) = (
            state.total_pot(),
            legal.call_amount,
            state.blind_level().big_blind,
            state.stack(state.to_act()),
        );
        for (action, bucket) in candidates(&state) {
            let (amount, min_amount) = match action {
                Action::Bet(amount) => (amount, legal.min_bet),
                Action::Raise(amount) => (amount, legal.min_raise_to),
                _ => continue,
            };
            let relabeled =
                BetSize::classify(Street::Flop, amount, pot, to_call, bb, min_amount, stack);
            assert_eq!(bucket, Some(relabeled), "{amount} mislabeled");
        }
    }

    #[test]
    fn no_duplicate_candidates_anywhere() {
        for state in [hero_open_state(), hero_flop_bet_state()] {
            let mut seen: Vec<Action> = Vec::new();
            for (action, _) in candidates(&state) {
                assert!(!seen.contains(&action), "duplicate {action:?}");
                seen.push(action);
            }
        }
    }

    #[test]
    fn all_in_carries_bucket_label() {
        let state = hero_open_state();
        assert!(
            candidates(&state)
                .iter()
                .any(|(a, b)| *a == Action::AllIn && *b == Some(BetSize::AllIn))
        );
    }

    #[test]
    fn deal_cards_have_expected_suits_for_test_decks() {
        let mut deck = Deck::shuffled(&mut seeded_rng(1));
        let card = deck.deal().unwrap();
        assert!(matches!(
            card.suit(),
            Suit::Clubs | Suit::Diamonds | Suit::Hearts | Suit::Spades
        ));
        assert!(matches!(
            card.rank(),
            Rank::Two
                | Rank::Three
                | Rank::Four
                | Rank::Five
                | Rank::Six
                | Rank::Seven
                | Rank::Eight
                | Rank::Nine
                | Rank::Ten
                | Rank::Jack
                | Rank::Queen
                | Rank::King
                | Rank::Ace
        ));
    }
}
// debug
