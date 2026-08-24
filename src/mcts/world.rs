use rand::Rng;
use rand::seq::SliceRandom;

use crate::card::{Card, Rank, Suit};
use crate::error::{Error, Result};
use crate::game::{GameState, NUM_PLAYERS, Seat};
use crate::range::hands::{HAND_COUNT, Hand, Range};
use crate::rng::{gen_index, weighted_index};

/// One determinization of the hidden information: every seat's hole cards
/// plus the remaining deck in its future deal order, together with the
/// exact probability of this world under the opponent ranges.
///
/// The hero's cards come from `GameState`; opponents' cards are sampled from
/// their ranges with card-removal (blocker) adjustment — eliminated seats get
/// placeholder cards only and are skipped entirely. Each world keeps its own
/// runout, so board cards dealt later are chance outcomes resolved per world
/// and mix out over the range-weighted average.
#[derive(Clone, Debug, PartialEq)]
pub struct World {
    pub hole_cards: [[Card; 2]; NUM_PLAYERS],
    pub runout: Vec<Card>,
    pub weight: f64,
}

impl World {
    /// A perfect-information snapshot of `state` for this world.
    pub fn build_state(&self, state: &GameState) -> GameState {
        state.clone_with_hole_cards(self.hole_cards)
    }
}

/// Samples opponent holdings (and their runouts) from range vectors.
pub struct WorldSampler;

impl WorldSampler {
    /// Samples `count` worlds for the given state and opponent ranges.
    ///
    /// Range weights are first adjusted for card removal: a hand class keeps
    /// only the concrete combos that are still live after the hero's cards
    /// and the board. Each sampled world's weight is the exact product of the
    /// per-opponent combo probabilities, self-normalized across the sample.
    pub fn sample<R: Rng + ?Sized>(
        rng: &mut R,
        state: &GameState,
        ranges: &[Range; 2],
        count: usize,
    ) -> Result<Vec<World>> {
        if count == 0 {
            return Err(Error::Solver("cannot sample zero worlds".into()));
        }

        let dead: Vec<Card> = state
            .hero_cards()
            .into_iter()
            .chain(state.board().iter().copied())
            .collect();

        let mut worlds = Vec::with_capacity(count);
        for _ in 0..count {
            let (hole_cards, weight) =
                draw_opponents(rng, state, ranges, &dead).ok_or_else(|| {
                    Error::Solver(
                        "opponent range leaves no hand consistent with the dead cards".into(),
                    )
                })?;
            worlds.push((hole_cards, weight));
        }

        let total: f64 = worlds.iter().map(|(_, w)| *w).sum();
        if total <= 0.0 {
            return Err(Error::Solver("sampled worlds carry no range mass".into()));
        }

        let mut out = Vec::with_capacity(count);
        for (hole_cards, weight) in worlds {
            // Only cards actually held by live seats count as dead: an
            // eliminated seat's placeholders must not remove cards from the
            // future runout.
            let mut dead_hands = dead.clone();
            for seat in Seat::ALL {
                if !state.eliminated(seat) {
                    dead_hands.extend(hole_cards[seat.index()]);
                }
            }
            let mut runout: Vec<Card> = all_cards()
                .filter(|card| !dead_hands.contains(card))
                .collect();
            runout.shuffle(rng);

            out.push(World {
                hole_cards,
                runout,
                weight: weight / total,
            });
        }
        Ok(out)
    }
}

/// Draws one holding per active (non-eliminated) opponent and returns the
/// joint exact probability. Eliminated seats keep placeholder cards and are
/// never sampled.
fn draw_opponents<R: Rng + ?Sized>(
    rng: &mut R,
    state: &GameState,
    ranges: &[Range; 2],
    dead: &[Card],
) -> Option<([[Card; 2]; NUM_PLAYERS], f64)> {
    let placeholder = [Card::new(Rank::Two, Suit::Clubs); 2];
    let mut dead = dead.to_vec();
    let mut hole_cards = [placeholder; NUM_PLAYERS];
    hole_cards[Seat::Hero.index()] = state.hero_cards();
    let mut weight = 1.0f64;

    for seat in [Seat::Opponent1, Seat::Opponent2] {
        if state.eliminated(seat) {
            continue;
        }
        let (combo, class_prob) = draw_holding(rng, &ranges[seat.index() - 1], &dead)?;
        dead.extend_from_slice(&combo);
        hole_cards[seat.index()] = combo;
        weight *= f64::from(class_prob);
    }
    Some((hole_cards, weight))
}

/// Draws one concrete two-card holding: a hand class proportional to its
/// effective (blocker-adjusted) range weight, then a uniform live combo of
/// that class. Returns the cards and the exact probability of the draw.
fn draw_holding<R: Rng + ?Sized>(
    rng: &mut R,
    range: &Range,
    dead: &[Card],
) -> Option<([Card; 2], f32)> {
    let mut effective = [0.0f32; HAND_COUNT];
    let mut total = 0.0f64;
    for (index, &weight) in range.iter().enumerate() {
        let class = Hand::from_index(index);
        let live = live_combos(class, dead);
        let adjusted = f64::from(weight) * live as f64;
        effective[index] = adjusted as f32;
        total += adjusted;
    }
    if total <= 0.0 {
        return None;
    }

    let class_index = weighted_index(rng, &effective)?;
    let class = Hand::from_index(class_index);
    let live = live_combos(class, dead);
    let class_prob = (effective[class_index] as f64 / total) as f32;

    let options: Vec<[Card; 2]> = combos_of(class)
        .into_iter()
        .take(class.combos() as usize)
        .filter(|combo| !combo.iter().any(|card| dead.contains(card)))
        .collect();
    if options.is_empty() {
        return None;
    }
    let combo = options[gen_index(rng, options.len())];
    let draw_prob = class_prob / live as f32;
    Some((combo, draw_prob))
}

/// The concrete card combos of a hand class, in canonical order.
pub fn combos_of(hand: Hand) -> [[Card; 2]; 12] {
    let mut combos = [[Card::new(Rank::Two, Suit::Clubs); 2]; 12];
    let mut index = 0;
    let high = hand.high;
    let low = hand.low;
    let mut add = |a: Card, b: Card, index: &mut usize| {
        combos[*index] = [a, b];
        *index += 1;
    };

    if high == low {
        for a in 0..4 {
            for b in (a + 1)..4 {
                add(
                    Card::new(high, Suit::ALL[a]),
                    Card::new(high, Suit::ALL[b]),
                    &mut index,
                );
            }
        }
    } else if hand.suited {
        for suit in Suit::ALL {
            add(Card::new(high, suit), Card::new(low, suit), &mut index);
        }
    } else {
        for a in 0..4 {
            for b in 0..4 {
                if a != b {
                    add(
                        Card::new(high, Suit::ALL[a]),
                        Card::new(low, Suit::ALL[b]),
                        &mut index,
                    );
                }
            }
        }
    }
    combos
}

/// How many concrete combos of `hand` avoid every card in `dead`.
fn live_combos(hand: Hand, dead: &[Card]) -> usize {
    combos_of(hand)
        .into_iter()
        .take(hand.combos() as usize)
        .filter(|combo| !combo.iter().any(|card| dead.contains(card)))
        .count()
}

fn all_cards() -> impl Iterator<Item = Card> {
    Suit::ALL
        .into_iter()
        .flat_map(|suit| Rank::ALL.into_iter().map(move |rank| Card::new(rank, suit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::game::Action;
    use crate::game::blinds::BlindLevel;
    use crate::rng::{gen_index, seeded_rng};

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn dealt_state() -> GameState {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(1)))
            .unwrap();
        state
    }

    fn uniform() -> Range {
        [1.0 / HAND_COUNT as f32; HAND_COUNT]
    }

    fn pinned(hand: Hand) -> Range {
        let mut weights = [0.0f32; HAND_COUNT];
        weights[hand.index()] = 1.0;
        weights
    }

    #[test]
    fn combos_of_produces_expected_counts_and_shapes() {
        let pair = combos_of(Hand::new(Rank::Ace, Rank::Ace, false));
        assert_eq!(pair.len(), 12);
        let suited = combos_of(Hand::new(Rank::Ace, Rank::King, true));
        for combo in suited.iter().take(4) {
            assert_eq!(combo[0].suit(), combo[1].suit());
            assert_eq!(combo[0].rank(), Rank::Ace);
            assert_eq!(combo[1].rank(), Rank::King);
        }
        let offsuit = combos_of(Hand::new(Rank::Ace, Rank::King, false));
        for combo in offsuit.iter().take(12) {
            assert_ne!(combo[0].suit(), combo[1].suit());
        }
    }

    #[test]
    fn sampled_worlds_are_consistent_and_weights_normalize() {
        let state = dealt_state();
        let uniform = [uniform(), uniform()];
        let mut rng = seeded_rng(7);
        let worlds = WorldSampler::sample(&mut rng, &state, &uniform, 100).unwrap();

        let total: f64 = worlds.iter().map(|w| w.weight).sum();
        assert!((total - 1.0).abs() < 1e-6, "weights must self-normalize");

        for world in &worlds {
            assert_eq!(world.hole_cards[Seat::Hero.index()], state.hero_cards());
            assert_eq!(world.runout.len(), 46);
            for hand in &world.hole_cards {
                assert!(!world.runout.contains(&hand[0]));
                assert!(!world.runout.contains(&hand[1]));
            }
            for board_card in state.board() {
                assert!(!world.runout.contains(board_card));
            }
        }
    }

    #[test]
    fn blockers_zero_out_dead_hand_classes() {
        let mut state = dealt_state();
        let aces = [
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::Ace, Suit::Diamonds),
        ];
        state.set_hole_cards(Seat::Hero, aces);
        let uniform = [uniform(), uniform()];
        let mut rng = seeded_rng(8);
        let worlds = WorldSampler::sample(&mut rng, &state, &uniform, 64).unwrap();

        for world in &worlds {
            for seat in [Seat::Opponent1, Seat::Opponent2] {
                for card in &world.hole_cards[seat.index()] {
                    assert_ne!(*card, aces[0], "As is dead but was dealt to an opponent");
                    assert_ne!(*card, aces[1], "Ad is dead but was dealt to an opponent");
                }
            }
            assert!(!world.runout.contains(&aces[0]));
            assert!(!world.runout.contains(&aces[1]));
        }
        assert_eq!(worlds.len(), 64);
    }

    #[test]
    fn pinned_ranges_always_sample_that_class() {
        let state = dealt_state();
        let deuces = Hand::new(Rank::Two, Rank::Two, false);
        let kings = Hand::new(Rank::King, Rank::King, false);
        let ranges = [pinned(deuces), pinned(kings)];

        let mut rng = seeded_rng(9);
        let worlds = WorldSampler::sample(&mut rng, &state, &ranges, 40).unwrap();
        for world in &worlds {
            assert_eq!(
                Hand::from_cards(
                    world.hole_cards[Seat::Opponent1.index()][0],
                    world.hole_cards[Seat::Opponent1.index()][1]
                ),
                deuces
            );
            assert_eq!(
                Hand::from_cards(
                    world.hole_cards[Seat::Opponent2.index()][0],
                    world.hole_cards[Seat::Opponent2.index()][1]
                ),
                kings
            );
        }
        let expected = 1.0 / 40.0;
        for world in &worlds {
            assert!((world.weight - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn impossible_range_yields_solver_error() {
        // Build a state where the hero holds As Ad and the flop contains Ah:
        // no AA combo is left for the opponents.
        let custom: Vec<Card> = {
            let mut cards = Vec::new();
            for rank in [
                Rank::Two,
                Rank::Three,
                Rank::Four,
                Rank::Five,
                Rank::Six,
                Rank::Seven,
            ] {
                cards.push(Card::new(rank, Suit::Clubs));
            }
            cards.push(Card::new(Rank::Ace, Suit::Hearts));
            cards.push(Card::new(Rank::Two, Suit::Diamonds));
            cards.push(Card::new(Rank::Three, Suit::Diamonds));
            for suit in Suit::ALL {
                for rank in Rank::ALL {
                    let candidate = Card::new(rank, suit);
                    if !cards.contains(&candidate) {
                        cards.push(candidate);
                    }
                }
            }
            cards
        };
        let mut deck = Deck::try_from_remaining(custom).unwrap();
        let mut state = GameState::new(Seat::Opponent1, level());
        state.start_hand(&mut deck).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck).unwrap();
        assert!(state.board().contains(&Card::new(Rank::Ace, Suit::Hearts)));
        state.set_hole_cards(
            Seat::Hero,
            [
                Card::new(Rank::Ace, Suit::Spades),
                Card::new(Rank::Ace, Suit::Diamonds),
            ],
        );

        let aces = pinned(Hand::new(Rank::Ace, Rank::Ace, false));
        let mut rng = seeded_rng(10);
        let result = WorldSampler::sample(&mut rng, &state, &[aces, uniform()], 8);
        assert!(matches!(result, Err(Error::Solver(_))));
    }

    #[test]
    fn eliminated_opponents_keep_placeholders_and_stay_live_in_runouts() {
        let mut state = dealt_state();
        state.set_eliminated(Seat::Opponent1, true);
        let uniform = [uniform(), uniform()];
        let mut rng = seeded_rng(17);
        let worlds = WorldSampler::sample(&mut rng, &state, &uniform, 16).unwrap();
        let placeholder = [Card::new(Rank::Two, Suit::Clubs); 2];

        for world in &worlds {
            assert_eq!(
                world.hole_cards[Seat::Opponent1.index()],
                placeholder,
                "an eliminated opponent is never dealt sampled cards"
            );
            assert_ne!(
                world.hole_cards[Seat::Opponent2.index()],
                placeholder,
                "the live opponent still draws from their range"
            );
            for hand in [Seat::Hero.index(), Seat::Opponent2.index()] {
                for card in &world.hole_cards[hand] {
                    assert!(!world.runout.contains(card));
                }
            }
            let live_cards = state
                .hero_cards()
                .into_iter()
                .chain(world.hole_cards[Seat::Opponent2.index()]);
            if live_cards.clone().all(|card| card != placeholder[0]) {
                assert!(
                    world.runout.contains(&placeholder[0]),
                    "placeholder cards never remove live cards from the runout"
                );
            }
            // 52 cards minus the hero's two and the one live opponent's two.
            assert_eq!(world.runout.len(), 48);
        }
    }

    #[test]
    fn zero_worlds_are_rejected() {
        let state = dealt_state();
        let mut rng = seeded_rng(11);
        let result = WorldSampler::sample(&mut rng, &state, &[uniform(), uniform()], 0);
        assert!(matches!(result, Err(Error::Solver(_))));
    }

    #[test]
    fn sampling_is_seed_deterministic() {
        let state = dealt_state();
        let uniform = [uniform(), uniform()];
        let mut a = seeded_rng(12);
        let mut b = seeded_rng(12);
        assert_eq!(
            WorldSampler::sample(&mut a, &state, &uniform, 16).unwrap(),
            WorldSampler::sample(&mut b, &state, &uniform, 16).unwrap()
        );
    }

    #[test]
    fn world_build_state_exposes_all_hands() {
        let state = dealt_state();
        let uniform = [uniform(), uniform()];
        let mut rng = seeded_rng(13);
        let worlds = WorldSampler::sample(&mut rng, &state, &uniform, 3).unwrap();
        for world in &worlds {
            let search_state = world.build_state(&state);
            assert_eq!(
                search_state.hole_cards(Seat::Hero),
                Some(state.hero_cards())
            );
            assert_eq!(
                search_state.hole_cards(Seat::Opponent1),
                Some(world.hole_cards[Seat::Opponent1.index()])
            );
            assert_eq!(search_state.board(), state.board());
            assert_eq!(search_state.to_act(), state.to_act());
        }
    }

    #[test]
    fn postflop_runouts_account_for_the_board() {
        let mut state = dealt_state();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state
            .advance_street(&mut Deck::shuffled(&mut seeded_rng(14)))
            .unwrap();
        assert_eq!(state.board().len(), 3);

        let uniform = [uniform(), uniform()];
        let mut rng = seeded_rng(15);
        let worlds = WorldSampler::sample(&mut rng, &state, &uniform, 8).unwrap();
        for world in &worlds {
            assert_eq!(world.runout.len(), 43);
            for board_card in state.board() {
                assert!(!world.runout.contains(board_card));
            }
        }
    }

    #[test]
    fn gen_index_covers_all_entries_when_rng_varies() {
        let mut rng = seeded_rng(16);
        let options = [[Card::new(Rank::Ace, Suit::Spades); 2]; 12];
        for len in 1..=options.len() {
            assert!(gen_index(&mut rng, len) < len);
        }
    }
}
