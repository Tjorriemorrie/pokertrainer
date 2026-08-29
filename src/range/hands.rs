use crate::card::{Card, Rank};

/// Number of distinct preflop hand classes (13 pairs + 78 suited + 78 offsuit).
pub const HAND_COUNT: usize = 169;
/// Side length of the 13×13 hand matrix.
pub const MATRIX_SIZE: usize = 13;

/// A 169-element hand-range distribution, indexed by [`Hand::index`].
pub type Range = [f32; HAND_COUNT];

/// Maps a rank to its 13×13 matrix index (`A = 0` down to `2 = 12`).
fn matrix_index(rank: Rank) -> usize {
    MATRIX_SIZE - 1 - rank as usize
}

/// Maps a 13×13 matrix index (`0 = A` down to `12 = 2`) back to a rank.
fn rank_from_matrix_index(index: usize) -> Rank {
    Rank::ALL[MATRIX_SIZE - 1 - index]
}

/// A single preflop hand class (e.g. `AA`, `AKs`, `72o`).
///
/// The canonical index is the 13×13 row-major grid used by the range heatmap:
/// `index = row * 13 + col`, where `row`/`col` are rank indices (`A = 0` down
/// to `2 = 12`). The diagonal holds pairs, the upper triangle suited hands,
/// and the lower triangle offsuit hands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Hand {
    /// The higher of the two ranks (equal for pairs).
    pub high: Rank,
    /// The lower of the two ranks (equal for pairs).
    pub low: Rank,
    /// `true` for suited hands; always `false` for pairs.
    pub suited: bool,
}

impl Hand {
    /// Builds a hand from its two ranks and suitedness. `high` must be at
    /// least `low`; pairs are represented with `high == low` and `suited == false`.
    pub const fn new(high: Rank, low: Rank, suited: bool) -> Hand {
        Hand { high, low, suited }
    }

    /// The hand at the given 13×13 row-major index (0..169).
    pub fn from_index(index: usize) -> Hand {
        assert!(index < HAND_COUNT, "hand index {index} out of range");
        let row = index / MATRIX_SIZE;
        let col = index % MATRIX_SIZE;
        let (high, low, suited) = if row == col {
            let rank = rank_from_matrix_index(row);
            (rank, rank, false)
        } else if row < col {
            (
                rank_from_matrix_index(row),
                rank_from_matrix_index(col),
                true,
            )
        } else {
            (
                rank_from_matrix_index(col),
                rank_from_matrix_index(row),
                false,
            )
        };
        Hand { high, low, suited }
    }

    /// The 13×13 row-major index of this hand.
    pub fn index(self) -> usize {
        let hi = matrix_index(self.high);
        let lo = matrix_index(self.low);
        if self.high == self.low {
            hi * MATRIX_SIZE + hi
        } else if self.suited {
            hi * MATRIX_SIZE + lo
        } else {
            lo * MATRIX_SIZE + hi
        }
    }

    /// The hand class for a specific two-card holding.
    pub fn from_cards(a: Card, b: Card) -> Hand {
        let (high, low) = if a.rank() >= b.rank() {
            (a.rank(), b.rank())
        } else {
            (b.rank(), a.rank())
        };
        let suited = high != low && a.suit() == b.suit();
        Hand { high, low, suited }
    }

    /// The conventional label, e.g. `"AA"`, `"AKs"`, `"72o"`.
    pub fn label(self) -> String {
        let hi = self.high.as_char();
        let lo = self.low.as_char();
        if self.high == self.low {
            format!("{hi}{lo}")
        } else if self.suited {
            format!("{hi}{lo}s")
        } else {
            format!("{hi}{lo}o")
        }
    }

    /// Number of specific card combinations: 6 for pairs, 4 for suited, 12 for offsuit.
    pub fn combos(self) -> u8 {
        if self.high == self.low {
            6
        } else if self.suited {
            4
        } else {
            12
        }
    }

    /// The `(row, col)` coordinates of this hand in the 13×13 heatmap.
    pub fn matrix_coords(self) -> (usize, usize) {
        let index = self.index();
        (index / MATRIX_SIZE, index % MATRIX_SIZE)
    }

    /// Preflop starting-hand strength via the Chen Formula: a standard,
    /// well-known closed-form score, natively 0 (the worst unplayable
    /// hand) to 20 (`AA`). High-card points (`A=10, K=8, Q=7, J=6`, else
    /// `rank/2` rounded up to the nearest half), doubled (minimum 5) for a
    /// pair; otherwise +2 for suited, a gap penalty that grows with the
    /// rank distance between the two cards, and +1 back for a 0- or
    /// 1-gap hand whose high card is a jack or lower (straight-friendly
    /// connectors).
    pub fn chen_score(self) -> f64 {
        let high_points = match self.high {
            Rank::Ace => 10.0,
            Rank::King => 8.0,
            Rank::Queen => 7.0,
            Rank::Jack => 6.0,
            rank => (rank as u8 as f64 + 2.0) / 2.0,
        };

        if self.high == self.low {
            return (high_points * 2.0).max(5.0);
        }

        let mut score = high_points;
        if self.suited {
            score += 2.0;
        }
        let gap = self.high as i32 - self.low as i32 - 1;
        score -= match gap {
            0 => 0.0,
            1 => 1.0,
            2 => 2.0,
            3 => 4.0,
            _ => 5.0,
        };
        if gap <= 1 && self.high <= Rank::Jack {
            score += 1.0;
        }
        score.max(0.0)
    }

    /// [`Self::chen_score`] rescaled from its native 0..20 onto 0..4 — the
    /// same coarse strength ladder the postflop rollout-policy tiers use
    /// (see `crate::mcts::rollout::strength_tier`), so a preflop and
    /// postflop tier feed the same fold/call/raise-mass curve consistently.
    pub fn chen_tier(self) -> f64 {
        (self.chen_score() / 20.0 * 4.0).clamp(0.0, 4.0)
    }
}

/// Iterates over all 169 hand classes in index order.
pub fn all_hands() -> impl Iterator<Item = Hand> {
    (0..HAND_COUNT).map(Hand::from_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Rank, Suit};

    fn card(rank: Rank, suit: Suit) -> Card {
        Card::new(rank, suit)
    }

    #[test]
    fn chen_score_ranks_known_hands_in_the_textbook_order() {
        let aa = Hand::new(Rank::Ace, Rank::Ace, false);
        let kk = Hand::new(Rank::King, Rank::King, false);
        let two_two = Hand::new(Rank::Two, Rank::Two, false);
        let ako = Hand::new(Rank::Ace, Rank::King, false);
        let aks = Hand::new(Rank::Ace, Rank::King, true);
        let seven_deuce = Hand::new(Rank::Seven, Rank::Two, false);
        let jack_ten_suited = Hand::new(Rank::Jack, Rank::Ten, true);

        assert_eq!(aa.chen_score(), 20.0, "AA is the textbook maximum");
        assert_eq!(kk.chen_score(), 16.0);
        assert_eq!(two_two.chen_score(), 5.0, "even 22 gets the pair floor of 5");
        assert_eq!(ako.chen_score(), 10.0, "connected, no suited/gap adjustment");
        assert_eq!(aks.chen_score(), 12.0, "suited adds 2");
        assert_eq!(
            jack_ten_suited.chen_score(),
            9.0,
            "6 (jack) + 2 (suited) + 1 (0-gap connector bonus)"
        );

        // Regression for the "raise 4h8d into a real raise" coaching
        // complaint: the postflop-oriented eval_hand degenerated preflop to
        // "pocket pair or not", so a premium non-paired hand like AKo
        // scored *below* garbage like 72o (which accidentally paired
        // best_hand's placeholder padding card). Chen scoring must not
        // repeat that.
        assert!(
            ako.chen_score() > seven_deuce.chen_score(),
            "AKo must outscore 72o"
        );
        assert!(
            two_two.chen_score() > seven_deuce.chen_score(),
            "even the smallest pair outscores unpaired junk"
        );

        for hand in all_hands() {
            let score = hand.chen_score();
            assert!((0.0..=20.0).contains(&score), "{score} out of 0..=20 range");
            let tier = hand.chen_tier();
            assert!((0.0..=4.0).contains(&tier), "{tier} out of 0..=4 range");
        }
    }

    #[test]
    fn index_round_trips_all_169_hands() {
        for index in 0..HAND_COUNT {
            let hand = Hand::from_index(index);
            assert_eq!(hand.index(), index, "index {index} did not round-trip");
        }
    }

    #[test]
    fn matrix_coords_match_index() {
        for index in 0..HAND_COUNT {
            let hand = Hand::from_index(index);
            let (row, col) = hand.matrix_coords();
            assert_eq!(row * MATRIX_SIZE + col, index);
        }
    }

    #[test]
    fn diagonal_is_pairs() {
        for row in 0..MATRIX_SIZE {
            let hand = Hand::from_index(row * MATRIX_SIZE + row);
            assert_eq!(hand.high, hand.low);
            assert!(!hand.suited);
            assert_eq!(hand.combos(), 6);
        }
    }

    #[test]
    fn diagonal_runs_aces_to_twos() {
        let labels: Vec<String> = (0..MATRIX_SIZE)
            .map(|row| Hand::from_index(row * MATRIX_SIZE + row).label())
            .collect();
        assert_eq!(
            labels,
            [
                "AA", "KK", "QQ", "JJ", "TT", "99", "88", "77", "66", "55", "44", "33", "22"
            ]
        );
    }

    #[test]
    fn upper_triangle_is_suited_lower_is_offsuit() {
        for row in 0..MATRIX_SIZE {
            for col in 0..MATRIX_SIZE {
                if row == col {
                    continue;
                }
                let hand = Hand::from_index(row * MATRIX_SIZE + col);
                if row < col {
                    assert!(hand.suited, "({row},{col}) should be suited");
                    assert_eq!(hand.combos(), 4);
                    assert!(hand.high > hand.low);
                } else {
                    assert!(!hand.suited, "({row},{col}) should be offsuit");
                    assert_eq!(hand.combos(), 12);
                    assert!(hand.high > hand.low);
                }
            }
        }
    }

    #[test]
    fn labels_are_canonical() {
        assert_eq!(Hand::from_index(0).label(), "AA");
        assert_eq!(Hand::from_index(1).label(), "AKs");
        assert_eq!(Hand::from_index(13).label(), "AKo");
        assert_eq!(Hand::from_index(168).label(), "22");
        assert_eq!(Hand::new(Rank::Seven, Rank::Two, false).label(), "72o");
        assert_eq!(Hand::new(Rank::Ace, Rank::King, true).label(), "AKs");
    }

    #[test]
    fn from_cards_classifies_suitedness_and_order() {
        let ak_suited = Hand::from_cards(
            card(Rank::Ace, Suit::Spades),
            card(Rank::King, Suit::Spades),
        );
        assert_eq!(ak_suited, Hand::new(Rank::Ace, Rank::King, true));
        assert_eq!(ak_suited.index(), 1);

        let ak_offsuit = Hand::from_cards(
            card(Rank::King, Suit::Hearts),
            card(Rank::Ace, Suit::Spades),
        );
        assert_eq!(ak_offsuit, Hand::new(Rank::Ace, Rank::King, false));
        assert_eq!(ak_offsuit.index(), 13);

        let pair = Hand::from_cards(
            card(Rank::Queen, Suit::Clubs),
            card(Rank::Queen, Suit::Diamonds),
        );
        assert_eq!(pair, Hand::new(Rank::Queen, Rank::Queen, false));
        assert_eq!(pair.combos(), 6);
    }

    #[test]
    fn all_hands_covers_every_index_once() {
        let mut seen = [false; HAND_COUNT];
        for hand in all_hands() {
            assert!(!seen[hand.index()], "duplicate hand {}", hand.label());
            seen[hand.index()] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn hand_count_matches_db_range_size() {
        assert_eq!(HAND_COUNT, crate::db::RANGE_SIZE);
    }
}
