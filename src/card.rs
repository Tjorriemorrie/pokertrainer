use std::fmt;

use rand::seq::SliceRandom;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Suit {
    Clubs = 0,
    Diamonds = 1,
    Hearts = 2,
    Spades = 3,
}

impl Suit {
    pub const ALL: [Suit; 4] = [Suit::Clubs, Suit::Diamonds, Suit::Hearts, Suit::Spades];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Rank {
    Two = 0,
    Three = 1,
    Four = 2,
    Five = 3,
    Six = 4,
    Seven = 5,
    Eight = 6,
    Nine = 7,
    Ten = 8,
    Jack = 9,
    Queen = 10,
    King = 11,
    Ace = 12,
}

impl Rank {
    pub const ALL: [Rank; 13] = [
        Rank::Two,
        Rank::Three,
        Rank::Four,
        Rank::Five,
        Rank::Six,
        Rank::Seven,
        Rank::Eight,
        Rank::Nine,
        Rank::Ten,
        Rank::Jack,
        Rank::Queen,
        Rank::King,
        Rank::Ace,
    ];

    /// The single-character rank label (`'2'`..`'9'`, `'T'`, `'J'`, `'Q'`, `'K'`, `'A'`).
    pub const fn as_char(self) -> char {
        RANK_CHARS[self as usize] as char
    }
}

const RANK_CHARS: &[u8; 13] = b"23456789TJQKA";
const SUIT_CHARS: &[u8; 4] = b"cdhs";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Card(u8);

impl Card {
    pub const fn new(rank: Rank, suit: Suit) -> Card {
        Card(((suit as u8) << 4) | rank as u8)
    }

    pub const fn suit(self) -> Suit {
        match (self.0 >> 4) & 0b11 {
            0 => Suit::Clubs,
            1 => Suit::Diamonds,
            2 => Suit::Hearts,
            _ => Suit::Spades,
        }
    }

    pub const fn rank(self) -> Rank {
        match self.0 & 0x0F {
            0 => Rank::Two,
            1 => Rank::Three,
            2 => Rank::Four,
            3 => Rank::Five,
            4 => Rank::Six,
            5 => Rank::Seven,
            6 => Rank::Eight,
            7 => Rank::Nine,
            8 => Rank::Ten,
            9 => Rank::Jack,
            10 => Rank::Queen,
            11 => Rank::King,
            _ => Rank::Ace,
        }
    }

    pub const fn suit_index(self) -> usize {
        (self.0 >> 4) as usize
    }

    pub const fn rank_index(self) -> usize {
        (self.0 & 0x0F) as usize
    }

    pub fn to_code(self) -> String {
        format!(
            "{}{}",
            RANK_CHARS[self.rank_index()] as char,
            SUIT_CHARS[self.suit_index()] as char
        )
    }
}

impl fmt::Display for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_code())
    }
}

pub struct Deck {
    cards: [Card; 52],
    top: usize,
}

impl Default for Deck {
    fn default() -> Self {
        Self::new()
    }
}

impl Deck {
    pub fn new() -> Self {
        let mut cards = [Card::new(Rank::Two, Suit::Clubs); 52];
        let mut index = 0;
        for suit in Suit::ALL {
            for rank in Rank::ALL {
                cards[index] = Card::new(rank, suit);
                index += 1;
            }
        }
        Self { cards, top: 0 }
    }

    pub fn shuffled<R: rand::Rng + ?Sized>(rng: &mut R) -> Self {
        let mut deck = Self::new();
        deck.shuffle(rng);
        deck
    }

    pub fn shuffle<R: rand::Rng + ?Sized>(&mut self, rng: &mut R) {
        self.cards.shuffle(rng);
        self.top = 0;
    }

    pub fn remaining(&self) -> usize {
        self.cards.len() - self.top
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn deal(&mut self) -> Option<Card> {
        if self.top >= self.cards.len() {
            return None;
        }
        let card = self.cards[self.top];
        self.top += 1;
        Some(card)
    }

    /// Builds a deck whose top contains the given cards in order (used by the
    /// solver to deal only the cards still unknown at a decision point).
    /// Returns `None` when more than 52 cards are supplied.
    pub fn try_from_remaining(cards: Vec<Card>) -> Option<Self> {
        if cards.len() > 52 {
            return None;
        }
        let mut deck = Self::new();
        deck.top = deck.cards.len() - cards.len();
        deck.cards[deck.top..].copy_from_slice(&cards);
        Some(deck)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::seeded_rng;

    #[test]
    fn card_packing_round_trips() {
        for suit in Suit::ALL {
            for rank in Rank::ALL {
                let card = Card::new(rank, suit);
                assert_eq!(card.rank(), rank);
                assert_eq!(card.suit(), suit);
                assert_eq!(card.rank_index(), rank as usize);
                assert_eq!(card.suit_index(), suit as usize);
            }
        }
    }

    #[test]
    fn card_codes_and_display() {
        assert_eq!(Card::new(Rank::Ace, Suit::Spades).to_code(), "As");
        assert_eq!(Card::new(Rank::Ten, Suit::Hearts).to_code(), "Th");
        assert_eq!(Card::new(Rank::Two, Suit::Clubs).to_code(), "2c");
        assert_eq!(format!("{}", Card::new(Rank::King, Suit::Diamonds)), "Kd");
    }

    #[test]
    fn deck_has_expected_order_and_deals_52_unique_cards() {
        let mut deck = Deck::new();
        assert_eq!(deck.remaining(), 52);
        assert!(!deck.is_empty());

        let mut seen: u64 = 0;
        let mut dealt = 0usize;
        while let Some(card) = deck.deal() {
            if dealt == 0 {
                assert_eq!(card, Card::new(Rank::Two, Suit::Clubs));
            }
            if dealt == 51 {
                assert_eq!(card, Card::new(Rank::Ace, Suit::Spades));
            }
            let bit = 1u64 << (card.suit_index() * 13 + card.rank_index());
            assert_eq!(seen & bit, 0, "duplicate card {card:?}");
            seen |= bit;
            dealt += 1;
        }
        assert_eq!(dealt, 52);
        assert_eq!(seen.count_ones(), 52);
        assert!(deck.is_empty());
        assert_eq!(deck.remaining(), 0);
        assert_eq!(deck.deal(), None);
    }

    #[test]
    fn shuffle_is_seed_deterministic() {
        let mut rng1 = seeded_rng(123);
        let mut deck1 = Deck::shuffled(&mut rng1);
        let mut rng2 = seeded_rng(123);
        let mut deck2 = Deck::shuffled(&mut rng2);
        for _ in 0..52 {
            assert_eq!(deck1.deal(), deck2.deal());
        }
        assert!(deck1.is_empty());
        assert!(deck2.is_empty());

        let mut rng3 = seeded_rng(124);
        let mut deck3 = Deck::shuffled(&mut rng3);
        let mut rng4 = seeded_rng(123);
        let mut deck4 = Deck::shuffled(&mut rng4);
        let mut differ = false;
        for _ in 0..52 {
            if deck3.deal() != deck4.deal() {
                differ = true;
            }
        }
        assert!(differ, "different seeds produced identical permutations");
    }

    #[test]
    fn reshuffle_restarts_dealing() {
        let mut rng = seeded_rng(5);
        let mut deck = Deck::shuffled(&mut rng);
        for _ in 0..10 {
            deck.deal().unwrap();
        }
        assert_eq!(deck.remaining(), 42);
        deck.shuffle(&mut rng);
        assert_eq!(deck.remaining(), 52);
        assert!(!deck.is_empty());
    }

    #[test]
    fn deck_from_remaining_deals_only_supplied_cards_in_order() {
        let cards = vec![
            Card::new(Rank::Ace, Suit::Spades),
            Card::new(Rank::King, Suit::Hearts),
            Card::new(Rank::Two, Suit::Clubs),
        ];
        let mut deck = Deck::try_from_remaining(cards.clone()).unwrap();
        assert_eq!(deck.remaining(), 3);
        for expected in cards {
            assert_eq!(deck.deal(), Some(expected));
        }
        assert!(deck.is_empty());
    }

    #[test]
    fn deck_from_remaining_rejects_oversized_input() {
        let oversize = vec![Card::new(Rank::Two, Suit::Clubs); 53];
        assert!(Deck::try_from_remaining(oversize).is_none());
    }
}
