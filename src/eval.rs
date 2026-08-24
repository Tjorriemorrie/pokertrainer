use std::fmt;

use crate::card::Card;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum HandClass {
    HighCard = 0,
    Pair = 1,
    TwoPair = 2,
    Trips = 3,
    Straight = 4,
    Flush = 5,
    FullHouse = 6,
    Quads = 7,
    StraightFlush = 8,
}

impl fmt::Display for HandClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            HandClass::HighCard => "High Card",
            HandClass::Pair => "Pair",
            HandClass::TwoPair => "Two Pair",
            HandClass::Trips => "Three of a Kind",
            HandClass::Straight => "Straight",
            HandClass::Flush => "Flush",
            HandClass::FullHouse => "Full House",
            HandClass::Quads => "Four of a Kind",
            HandClass::StraightFlush => "Straight Flush",
        };
        f.write_str(name)
    }
}

const CLASS_SHIFT: u32 = 20;
const WHEEL: u64 = 0x100F;
const STRAIGHT_MASK: u64 = 0x1FFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Eval {
    value: u32,
    class: HandClass,
}

impl Eval {
    fn pack(class: HandClass, ranks: [u8; 5]) -> Eval {
        let value = (u32::from(class as u8) << CLASS_SHIFT)
            | (u32::from(ranks[0]) << 16)
            | (u32::from(ranks[1]) << 12)
            | (u32::from(ranks[2]) << 8)
            | (u32::from(ranks[3]) << 4)
            | u32::from(ranks[4]);
        Eval { value, class }
    }

    pub fn class(self) -> HandClass {
        self.class
    }
}

/// Ranks a holding by its best 5-card hand; accepts 5 to 7 cards
/// (flop/turn/river combinations in hold'em).
pub fn evaluate(cards: &[Card]) -> Eval {
    debug_assert!(
        (5..=7).contains(&cards.len()),
        "evaluate expects 5 to 7 cards, got {}",
        cards.len()
    );

    let mut suits = [0u64; 4];
    for card in cards {
        suits[card.suit_index()] |= 1u64 << card.rank_index();
    }
    let rank_mask = (suits[0] | suits[1] | suits[2] | suits[3]) & STRAIGHT_MASK;

    for &mask in &suits {
        if mask.count_ones() >= 5
            && let Some(high) = straight_high(mask)
        {
            return Eval::pack(HandClass::StraightFlush, [high, 0, 0, 0, 0]);
        }
    }

    let mut counts = [0u8; 13];
    for rank in 0..13u8 {
        counts[rank as usize] = ((suits[0] >> rank) & 1) as u8
            + ((suits[1] >> rank) & 1) as u8
            + ((suits[2] >> rank) & 1) as u8
            + ((suits[3] >> rank) & 1) as u8;
    }

    if let Some(quads_rank) = find_quads(&counts) {
        let mut kicker = [0u8; 1];
        top_k(&counts, &[quads_rank], &mut kicker);
        return Eval::pack(HandClass::Quads, [quads_rank, kicker[0], 0, 0, 0]);
    }

    let mut trips = [u8::MAX; 2];
    let mut trips_len = 0usize;
    let mut pairs = [u8::MAX; 3];
    let mut pairs_len = 0usize;
    for rank in (0..13u8).rev() {
        match counts[rank as usize] {
            3 => {
                trips[trips_len] = rank;
                trips_len += 1;
            }
            2 => {
                pairs[pairs_len] = rank;
                pairs_len += 1;
            }
            _ => {}
        }
    }

    if trips_len > 0 && (trips_len >= 2 || pairs_len >= 1) {
        let low = if trips_len >= 2 { trips[1] } else { pairs[0] };
        return Eval::pack(HandClass::FullHouse, [trips[0], low, 0, 0, 0]);
    }

    for &mask in &suits {
        if mask.count_ones() >= 5 {
            return flush_pack(mask);
        }
    }

    if let Some(high) = straight_high(rank_mask) {
        return Eval::pack(HandClass::Straight, [high, 0, 0, 0, 0]);
    }

    if trips_len == 1 {
        let mut kickers = [0u8; 2];
        top_k(&counts, &[trips[0]], &mut kickers);
        return Eval::pack(HandClass::Trips, [trips[0], kickers[0], kickers[1], 0, 0]);
    }

    if pairs_len >= 2 {
        let mut kicker = [0u8; 1];
        top_k(&counts, &[pairs[0], pairs[1]], &mut kicker);
        return Eval::pack(HandClass::TwoPair, [pairs[0], pairs[1], kicker[0], 0, 0]);
    }

    if pairs_len == 1 {
        let mut kickers = [0u8; 3];
        top_k(&counts, &[pairs[0]], &mut kickers);
        return Eval::pack(
            HandClass::Pair,
            [pairs[0], kickers[0], kickers[1], kickers[2], 0],
        );
    }

    let mut high_cards = [0u8; 5];
    top_k(&counts, &[], &mut high_cards);
    Eval::pack(HandClass::HighCard, high_cards)
}

fn straight_high(mask: u64) -> Option<u8> {
    let run = mask & (mask >> 1) & (mask >> 2) & (mask >> 3) & (mask >> 4);
    if run != 0 {
        return Some(63u8 - run.leading_zeros() as u8 + 4);
    }
    if mask & WHEEL == WHEEL {
        return Some(3);
    }
    None
}

fn find_quads(counts: &[u8; 13]) -> Option<u8> {
    (0..13u8).rev().find(|&rank| counts[rank as usize] == 4)
}

fn top_k(counts: &[u8; 13], excluded: &[u8], out: &mut [u8]) {
    let mut taken = 0;
    for rank in (0..13u8).rev() {
        if counts[rank as usize] > 0 && !excluded.contains(&rank) {
            out[taken] = rank;
            taken += 1;
            if taken == out.len() {
                return;
            }
        }
    }
}

fn flush_pack(mask: u64) -> Eval {
    let mut ranks = [0u8; 5];
    let mut taken = 0;
    for rank in (0..13u8).rev() {
        if mask >> rank & 1 == 1 {
            ranks[taken] = rank;
            taken += 1;
            if taken == 5 {
                break;
            }
        }
    }
    Eval::pack(HandClass::Flush, ranks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Deck, Rank, Suit};
    use crate::rng::seeded_rng;

    fn cards(codes: &str) -> Vec<Card> {
        codes
            .split_whitespace()
            .map(|code| {
                let bytes = code.as_bytes();
                assert_eq!(bytes.len(), 2, "invalid card code {code}");
                let rank = match bytes[0] {
                    b'2' => Rank::Two,
                    b'3' => Rank::Three,
                    b'4' => Rank::Four,
                    b'5' => Rank::Five,
                    b'6' => Rank::Six,
                    b'7' => Rank::Seven,
                    b'8' => Rank::Eight,
                    b'9' => Rank::Nine,
                    b'T' => Rank::Ten,
                    b'J' => Rank::Jack,
                    b'Q' => Rank::Queen,
                    b'K' => Rank::King,
                    b'A' => Rank::Ace,
                    other => panic!("invalid rank {}", other as char),
                };
                let suit = match bytes[1] {
                    b'c' => Suit::Clubs,
                    b'd' => Suit::Diamonds,
                    b'h' => Suit::Hearts,
                    b's' => Suit::Spades,
                    other => panic!("invalid suit {}", other as char),
                };
                Card::new(rank, suit)
            })
            .collect()
    }

    fn beats(winner: &str, loser: &str) {
        let w = evaluate(&cards(winner));
        let l = evaluate(&cards(loser));
        assert!(w > l, "{winner} ({w:?}) should beat {loser} ({l:?})");
    }

    fn ties(a: &str, b: &str) {
        assert_eq!(evaluate(&cards(a)), evaluate(&cards(b)));
    }

    #[test]
    fn curated_hand_orderings() {
        beats("As Ks Qs Js Ts", "9s 8s 7s 6s 5s");
        beats("2s 3s 4s 5s 6s", "Ah Ad Ac As Kd");
        beats("As 2s 3s 4s 5s", "Ah Ad Ac As Kd");
        beats("As Ks Qs Js Ts", "As 2s 3s 4s 5s");
        beats("Ah Ad Ac As Kd", "Ah Ad Ac As Qd");
        beats("Ah Ad Ac Kh Kd", "9h 9d 9c Ah Ad");
        beats("9h 9d 9c Kd Kh", "9h 9d 9c Qd Qh");
        beats("Ah Ad Ac Kh Kd", "Qs Js 9s 7s 3s");
        beats("Qs Js 9s 7s 3s", "As Kd Qh Jc Ts");
        beats("As Kd Qh Jc Ts", "Ah Ad Ac 7d 2s");
        beats("6s 5h 4d 3c 2s", "As 5h 4d 3c 2s");
        beats("As Kd Qh Jc Ts", "Kh Qd Jh Tc 9s");
        beats("Ah Kh 9h 7h 3h", "As Ks 9s 7s 2s");
        beats("Ah Ad Ac Kh Qs", "Ah Ad Ac Kh Jd");
        beats("Ah Ad Kh Kd 3c", "Ah Ad Qh Qd Jc");
        beats("Ah Ad Kh Kd 4c", "Ah Ad Kh Kd 3c");
        beats("Ah Ad Kh Qd 3c", "Ah Ad Kh Qd 2c");
        beats("Ah Kd 9c 7s 3h", "Ah Kd 9c 7s 2h");
        beats("Ah Ad Ac 7d 2s", "Kh Kd 5h 5d Ac");
        beats("Kh Kd 5h 5d Ac", "Qh Qd Ac 9s 3h");
        beats("Qh Qd Ac 9s 3h", "Ah Kd Qc Js 9h");
    }

    #[test]
    fn board_plays_and_counterfeiting() {
        ties("As Ks Qs Js Ts 2d 3c", "As Ks Qs Js Ts 6d 4h");
        beats("Ac As 7d 4c 2h Kd Qc", "Ac As 7d 4c 2h 6s 5d");
        beats("Kc Kd 9c 4s 2h 5d 4h", "Kc Kd 9c 4s 2h Ah 3h");
        beats("Ah 2d 3c 4h 5s Qd Kc", "Qh Qc As 9d 8s");
        beats("As Kd Qh Jc Ts 7d 3c", "Ad Ah Ac 9d 8s");
        ties("Ah Ad Kh Kd Qc 5c 2s", "As Ac Ks Kc Qh 5d 2h");
    }

    /// Live-fire regression: hero's Aces-and-Threes two pair (the Threes are
    /// shared on the board) beats the opponent's lone board-pair of Threes —
    /// the shared pair must not make the hands look equal.
    #[test]
    fn two_pair_beats_the_shared_board_pair() {
        beats("As 2s 3c Ac 9d 3h 8d", "Qs 6d 3c Ac 9d 3h 8d");
        assert_eq!(
            evaluate(&cards("As 2s 3c Ac 9d 3h 8d")).class(),
            HandClass::TwoPair
        );
        assert_eq!(
            evaluate(&cards("Qs 6d 3c Ac 9d 3h 8d")).class(),
            HandClass::Pair
        );
    }

    #[test]
    fn hand_class_names() {
        assert_eq!(
            evaluate(&cards("As Ks Qs Js Ts")).class().to_string(),
            "Straight Flush"
        );
        assert_eq!(
            evaluate(&cards("Ah Ad Ac As Kd")).class().to_string(),
            "Four of a Kind"
        );
        assert_eq!(
            evaluate(&cards("Ah Ad Ac Kh Kd")).class().to_string(),
            "Full House"
        );
        assert_eq!(
            evaluate(&cards("Qs Js 9s 7s 3s")).class().to_string(),
            "Flush"
        );
        assert_eq!(
            evaluate(&cards("As Kd Qh Jc Ts")).class().to_string(),
            "Straight"
        );
        assert_eq!(
            evaluate(&cards("Ah Ad Ac 7d 2s")).class().to_string(),
            "Three of a Kind"
        );
        assert_eq!(
            evaluate(&cards("Ah Ad Kh Kd 3c")).class().to_string(),
            "Two Pair"
        );
        assert_eq!(
            evaluate(&cards("Ah Ad Kh Qd 3c")).class().to_string(),
            "Pair"
        );
        assert_eq!(
            evaluate(&cards("Ah Kd 9c 7s 3h")).class().to_string(),
            "High Card"
        );
    }

    fn naive5(hand: &[Card; 5]) -> u32 {
        let mut ranks_desc = [0u8; 5];
        for (slot, card) in ranks_desc.iter_mut().zip(hand.iter()) {
            *slot = card.rank_index() as u8;
        }
        ranks_desc.sort_unstable_by(|a, b| b.cmp(a));

        let flush = hand.iter().all(|c| c.suit_index() == hand[0].suit_index());

        let mut sorted = ranks_desc;
        sorted.sort_unstable();
        let straight = (1..5).all(|i| sorted[i] == sorted[i - 1] + 1) || sorted == [0, 1, 2, 3, 12];
        let straight_high = if sorted == [0, 1, 2, 3, 12] {
            3
        } else {
            ranks_desc[0]
        };

        let mut counts = [0u8; 13];
        for &rank in &ranks_desc {
            counts[rank as usize] += 1;
        }

        let quads = (0..13u8).rev().find(|&r| counts[r as usize] == 4);
        let triples: Vec<u8> = (0..13u8)
            .rev()
            .filter(|&r| counts[r as usize] == 3)
            .collect();
        let pairs: Vec<u8> = (0..13u8)
            .rev()
            .filter(|&r| counts[r as usize] == 2)
            .collect();

        let (class, tiebreaks): (u8, [u8; 5]) = if flush && straight {
            (8, [straight_high, 0, 0, 0, 0])
        } else if let Some(q) = quads {
            let kicker = (0..13u8)
                .rev()
                .find(|&r| r != q && counts[r as usize] > 0)
                .unwrap();
            (7, [q, kicker, 0, 0, 0])
        } else if !triples.is_empty() && (triples.len() >= 2 || !pairs.is_empty()) {
            let low = if triples.len() >= 2 {
                triples[1]
            } else {
                pairs[0]
            };
            (6, [triples[0], low, 0, 0, 0])
        } else if flush {
            (
                5,
                [
                    ranks_desc[0],
                    ranks_desc[1],
                    ranks_desc[2],
                    ranks_desc[3],
                    ranks_desc[4],
                ],
            )
        } else if straight {
            (4, [straight_high, 0, 0, 0, 0])
        } else if let Some(&t) = triples.first() {
            let kickers: Vec<u8> = (0..13u8)
                .rev()
                .filter(|&r| r != t && counts[r as usize] > 0)
                .take(2)
                .collect();
            (3, [t, kickers[0], kickers[1], 0, 0])
        } else if pairs.len() >= 2 {
            let kickers: Vec<u8> = (0..13u8)
                .rev()
                .filter(|&r| r != pairs[0] && r != pairs[1] && counts[r as usize] > 0)
                .take(1)
                .collect();
            (2, [pairs[0], pairs[1], kickers[0], 0, 0])
        } else if let Some(&p) = pairs.first() {
            let kickers: Vec<u8> = (0..13u8)
                .rev()
                .filter(|&r| r != p && counts[r as usize] > 0)
                .take(3)
                .collect();
            (1, [p, kickers[0], kickers[1], kickers[2], 0])
        } else {
            (
                0,
                [
                    ranks_desc[0],
                    ranks_desc[1],
                    ranks_desc[2],
                    ranks_desc[3],
                    ranks_desc[4],
                ],
            )
        };

        (u32::from(class) << CLASS_SHIFT)
            | (u32::from(tiebreaks[0]) << 16)
            | (u32::from(tiebreaks[1]) << 12)
            | (u32::from(tiebreaks[2]) << 8)
            | (u32::from(tiebreaks[3]) << 4)
            | u32::from(tiebreaks[4])
    }

    fn naive_combos(
        hand: &[Card],
        combo: &mut [Card; 5],
        start: usize,
        depth: usize,
        best: &mut u32,
    ) {
        if depth == 5 {
            let value = naive5(combo);
            if value > *best {
                *best = value;
            }
            return;
        }
        for i in start..=(hand.len() - (5 - depth)) {
            combo[depth] = hand[i];
            naive_combos(hand, combo, i + 1, depth + 1, best);
        }
    }

    fn naive_best(hand: &[Card]) -> u32 {
        assert!((5..=7).contains(&hand.len()));
        let mut combo = [Card::new(Rank::Two, Suit::Clubs); 5];
        let mut best = 0u32;
        naive_combos(hand, &mut combo, 0, 0, &mut best);
        best
    }

    #[test]
    fn matches_naive_reference() {
        for &hand_size in &[5usize, 6, 7] {
            let mut rng = seeded_rng(42 + hand_size as u64);
            let mut deck = Deck::new();
            let samples = match hand_size {
                5 => 50_000usize,
                6 => 50_000,
                _ => 100_000,
            };
            for _ in 0..samples {
                if deck.remaining() < hand_size {
                    deck.shuffle(&mut rng);
                }
                let mut hand = [Card::new(Rank::Two, Suit::Clubs); 7];
                for slot in hand.iter_mut().take(hand_size) {
                    *slot = deck.deal().unwrap();
                }
                let eval = evaluate(&hand[..hand_size]);
                let naive = naive_best(&hand[..hand_size]);
                assert_eq!(
                    eval.value,
                    naive,
                    "mismatch on {} cards: {}",
                    hand_size,
                    hand[..hand_size]
                        .iter()
                        .map(|c| c.to_code())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                assert_eq!(eval.class() as u8, (naive >> CLASS_SHIFT) as u8);
            }
        }
    }

    #[test]
    fn eval_order_is_total_and_transitive() {
        let mut rng = seeded_rng(7);
        let mut deck = Deck::new();
        let mut evals = Vec::new();
        for _ in 0..64 {
            if deck.remaining() < 7 {
                deck.shuffle(&mut rng);
            }
            let mut hand = [Card::new(Rank::Two, Suit::Clubs); 7];
            for slot in hand.iter_mut() {
                *slot = deck.deal().unwrap();
            }
            evals.push(evaluate(&hand));
        }
        evals.sort();
        assert!(evals.windows(2).all(|w| w[0] <= w[1]));
    }
}
