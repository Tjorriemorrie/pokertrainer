use crate::card::{Card, Deck, Rank, Suit};
use crate::error::{Error, Result};
use crate::eval::{self, Eval, HandClass};
use crate::game::action::{Action, LegalActions};
use crate::game::blinds::{BlindLevel, next_level};
use crate::game::pot::{Pot, compute_pots};
use crate::game::seat::{Seat, Street, action_order};

/// Starting stack for every player in a Spin and Gold hand.
pub const STARTING_STACK: u32 = 500;
/// Number of seats at the table.
pub const NUM_PLAYERS: usize = 3;

/// The result of applying an action to the game state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The action was applied and play continues with the next actor.
    Continue,
    /// The betting round closed; the caller should advance the street or
    /// resolve a showdown.
    StreetEnded,
    /// The hand ended (all but one player folded).
    HandEnded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandEndReason {
    Fold(Seat),
    Showdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PotAward {
    pub seat: Seat,
    pub amount: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandResult {
    pub reason: HandEndReason,
    pub awards: Vec<PotAward>,
    pub pots: Vec<Pot>,
    pub revealed: Vec<(Seat, [Card; 2], HandClass)>,
}

/// The full 3-max Spin and Gold game state, modeled from the hero's
/// perspective: the hero's hole cards are always known, opponents' cards are
/// hidden until showdown.
pub struct GameState {
    stacks: [u32; NUM_PLAYERS],
    button: Seat,
    blind_level: BlindLevel,
    street: Street,
    board: Vec<Card>,
    hole_cards: [[Card; 2]; NUM_PLAYERS],
    revealed: [bool; NUM_PLAYERS],
    street_contrib: [u32; NUM_PLAYERS],
    total_contrib: [u32; NUM_PLAYERS],
    current_bet: u32,
    min_raise: u32,
    last_full_raise: Option<Seat>,
    acted: [bool; NUM_PLAYERS],
    folded: [bool; NUM_PLAYERS],
    all_in: [bool; NUM_PLAYERS],
    to_act: Seat,
    hand_over: bool,
    hand_result: Option<HandResult>,
}

impl GameState {
    pub fn new(button: Seat, blind_level: BlindLevel) -> Self {
        Self {
            stacks: [STARTING_STACK; NUM_PLAYERS],
            button,
            blind_level,
            street: Street::Preflop,
            board: Vec::new(),
            hole_cards: [[Card::new(Rank::Two, Suit::Clubs); 2]; NUM_PLAYERS],
            revealed: [true, false, false],
            street_contrib: [0; NUM_PLAYERS],
            total_contrib: [0; NUM_PLAYERS],
            current_bet: 0,
            min_raise: blind_level.big_blind,
            last_full_raise: None,
            acted: [false; NUM_PLAYERS],
            folded: [false; NUM_PLAYERS],
            all_in: [false; NUM_PLAYERS],
            to_act: Seat::Hero,
            hand_over: false,
            hand_result: None,
        }
    }

    /// Deals a fresh hand: resets per-hand state, deals two cards to each
    /// seat, and posts the blinds.
    pub fn start_hand(&mut self, deck: &mut Deck) -> Result<()> {
        if deck.remaining() < NUM_PLAYERS * 2 {
            return Err(Error::Game("not enough cards to deal a hand".into()));
        }

        self.board.clear();
        self.street = Street::Preflop;
        self.street_contrib = [0; NUM_PLAYERS];
        self.total_contrib = [0; NUM_PLAYERS];
        self.current_bet = 0;
        self.min_raise = self.blind_level.big_blind;
        self.last_full_raise = None;
        self.acted = [false; NUM_PLAYERS];
        self.folded = [false; NUM_PLAYERS];
        self.all_in = [false; NUM_PLAYERS];
        self.revealed = [true, false, false];
        self.hand_over = false;
        self.hand_result = None;

        let mut seat = self.button.next();
        for _ in 0..NUM_PLAYERS {
            let first = deck
                .deal()
                .ok_or_else(|| Error::Game("deck exhausted".into()))?;
            let second = deck
                .deal()
                .ok_or_else(|| Error::Game("deck exhausted".into()))?;
            self.hole_cards[seat.index()] = [first, second];
            seat = seat.next();
        }

        self.post_blind(self.button, self.blind_level.small_blind);
        self.post_blind(self.button.next(), self.blind_level.big_blind);

        self.to_act = self.first_to_act();
        Ok(())
    }

    /// Rotates the button and deals the next hand.
    pub fn next_hand(&mut self, deck: &mut Deck) -> Result<()> {
        self.button = self.button.next();
        self.start_hand(deck)
    }

    /// Advances to the next blind level, returning `false` at the top of the
    /// schedule.
    pub fn advance_blind_level(&mut self) -> bool {
        match next_level(self.blind_level) {
            Some(level) => {
                self.blind_level = level;
                true
            }
            None => false,
        }
    }

    pub fn set_blind_level(&mut self, level: BlindLevel) {
        self.blind_level = level;
    }

    /// The legal actions available to the current actor.
    pub fn legal_actions(&self) -> LegalActions {
        let seat = self.to_act;
        let stack = self.stacks[seat.index()];
        let to_call = self
            .current_bet
            .saturating_sub(self.street_contrib[seat.index()]);

        let can_fold = to_call > 0;
        let can_check = to_call == 0;
        let can_call = to_call > 0 && to_call < stack;
        let call_amount = to_call.min(stack);

        let can_bet = to_call == 0 && stack > 0;
        let min_bet = self.blind_level.big_blind.min(stack);
        let max_bet = stack;

        let can_raise = to_call > 0 && stack > to_call && self.last_full_raise != Some(seat);
        let min_raise_to = self.current_bet + self.min_raise;
        let max_raise_to = self.street_contrib[seat.index()] + stack;

        let can_all_in = stack > 0 && (to_call == 0 || stack <= to_call || can_raise);

        LegalActions {
            can_fold,
            can_check,
            can_call,
            call_amount,
            can_bet,
            min_bet,
            max_bet,
            can_raise,
            min_raise_to,
            max_raise_to,
            can_all_in,
        }
    }

    /// Applies an action for the current actor, returning the outcome.
    pub fn apply_action(&mut self, action: Action) -> Result<ActionOutcome> {
        if self.hand_over {
            return Err(Error::Game("hand is already over".into()));
        }
        let legal = self.legal_actions();
        if !legal.allows(action) {
            return Err(Error::Game(format!(
                "illegal action {action:?} for {}",
                self.to_act
            )));
        }

        let seat = self.to_act;
        match action {
            Action::Fold => {
                self.folded[seat.index()] = true;
                self.acted[seat.index()] = true;
            }
            Action::Check => {
                self.acted[seat.index()] = true;
            }
            Action::Call => {
                self.commit(seat, legal.call_amount);
                self.acted[seat.index()] = true;
            }
            Action::Bet(amount) => {
                self.commit(seat, amount);
                self.current_bet = amount;
                self.min_raise = amount;
                self.last_full_raise = Some(seat);
                self.acted[seat.index()] = true;
            }
            Action::Raise(amount) => {
                let previous_bet = self.current_bet;
                self.commit(seat, amount - self.street_contrib[seat.index()]);
                self.current_bet = amount;
                self.min_raise = amount - previous_bet;
                self.last_full_raise = Some(seat);
                self.acted[seat.index()] = true;
            }
            Action::AllIn => {
                let stack = self.stacks[seat.index()];
                let new_total = self.street_contrib[seat.index()] + stack;
                self.commit(seat, stack);
                if new_total > self.current_bet {
                    let raise_size = new_total - self.current_bet;
                    if raise_size >= self.min_raise {
                        self.min_raise = raise_size;
                        self.last_full_raise = Some(seat);
                    }
                    self.current_bet = new_total;
                }
                self.acted[seat.index()] = true;
            }
        }

        let active = self.active_players();
        if active.len() == 1 {
            self.award_fold_win(active[0]);
            self.hand_over = true;
            return Ok(ActionOutcome::HandEnded);
        }

        self.advance_to_act();

        if self.round_complete() {
            Ok(ActionOutcome::StreetEnded)
        } else {
            Ok(ActionOutcome::Continue)
        }
    }

    /// Deals the next street (flop/turn/river) after a betting round closes.
    pub fn advance_street(&mut self, deck: &mut Deck) -> Result<()> {
        if self.hand_over {
            return Err(Error::Game("hand is already over".into()));
        }
        let next = self
            .street
            .next()
            .ok_or_else(|| Error::Game("cannot advance past the river".into()))?;
        self.street = next;

        let cards_needed = match next {
            Street::Flop => 3,
            Street::Turn | Street::River => 1,
            Street::Preflop => unreachable!(),
        };
        for _ in 0..cards_needed {
            let card = deck
                .deal()
                .ok_or_else(|| Error::Game("deck exhausted".into()))?;
            self.board.push(card);
        }

        self.street_contrib = [0; NUM_PLAYERS];
        self.current_bet = 0;
        self.min_raise = self.blind_level.big_blind;
        self.last_full_raise = None;
        self.acted = [false; NUM_PLAYERS];

        self.to_act = self.first_to_act();
        Ok(())
    }

    /// Resolves a showdown, dealing any remaining board cards first.
    pub fn showdown(&mut self, deck: &mut Deck) -> Result<HandResult> {
        if self.hand_over {
            return Err(Error::Game("hand is already over".into()));
        }
        while self.board.len() < 5 {
            let card = deck
                .deal()
                .ok_or_else(|| Error::Game("deck exhausted".into()))?;
            self.board.push(card);
        }

        for seat in Seat::ALL {
            self.revealed[seat.index()] = true;
        }

        let pots = compute_pots(&self.total_contrib, &self.folded);
        let mut awards: Vec<PotAward> = Vec::new();
        let mut revealed = Vec::new();

        for seat in Seat::ALL {
            if !self.folded[seat.index()] {
                let hand = self.best_hand(seat);
                revealed.push((seat, self.hole_cards[seat.index()], hand.class()));
            }
        }

        for pot in &pots {
            let mut best: Option<(Eval, Vec<Seat>)> = None;
            for &seat in &pot.eligible {
                let eval = self.best_hand(seat);
                match &mut best {
                    None => best = Some((eval, vec![seat])),
                    Some((best_eval, winners)) => {
                        if eval > *best_eval {
                            *best_eval = eval;
                            winners.clear();
                            winners.push(seat);
                        } else if eval == *best_eval {
                            winners.push(seat);
                        }
                    }
                }
            }
            if let Some((_, winners)) = best {
                let share = pot.amount / winners.len() as u32;
                let remainder = pot.amount % winners.len() as u32;
                for (index, &seat) in winners.iter().enumerate() {
                    let amount = share + if index == 0 { remainder } else { 0 };
                    self.stacks[seat.index()] += amount;
                    if let Some(award) = awards.iter_mut().find(|a| a.seat == seat) {
                        award.amount += amount;
                    } else {
                        awards.push(PotAward { seat, amount });
                    }
                }
            }
        }

        self.hand_over = true;
        let result = HandResult {
            reason: HandEndReason::Showdown,
            awards,
            pots,
            revealed,
        };
        self.hand_result = Some(result.clone());
        Ok(result)
    }

    /// Whether at least two active players can still act (i.e. betting can
    /// continue). When false, the caller should resolve a showdown.
    pub fn can_continue_betting(&self) -> bool {
        self.active_players()
            .iter()
            .filter(|&&seat| !self.all_in[seat.index()])
            .count()
            >= 2
    }

    pub fn is_hand_over(&self) -> bool {
        self.hand_over
    }

    pub fn hand_result(&self) -> Option<&HandResult> {
        self.hand_result.as_ref()
    }

    pub fn stack(&self, seat: Seat) -> u32 {
        self.stacks[seat.index()]
    }

    /// Overwrites a seat's stack. Used by the session layer when re-seating
    /// or injecting test scenarios; the caller keeps betting state consistent.
    pub fn set_stack(&mut self, seat: Seat, amount: u32) {
        self.stacks[seat.index()] = amount;
    }

    /// Overwrites a seat's hole cards. Used by the session layer (and solver
    /// tests) to inject specific holdings; does not change reveal status.
    pub fn set_hole_cards(&mut self, seat: Seat, cards: [Card; 2]) {
        self.hole_cards[seat.index()] = cards;
    }

    pub fn stacks(&self) -> [u32; NUM_PLAYERS] {
        self.stacks
    }

    pub fn button(&self) -> Seat {
        self.button
    }

    pub fn blind_level(&self) -> BlindLevel {
        self.blind_level
    }

    pub fn street(&self) -> Street {
        self.street
    }

    pub fn board(&self) -> &[Card] {
        &self.board
    }

    pub fn to_act(&self) -> Seat {
        self.to_act
    }

    pub fn current_bet(&self) -> u32 {
        self.current_bet
    }

    pub fn to_call(&self, seat: Seat) -> u32 {
        self.current_bet
            .saturating_sub(self.street_contrib[seat.index()])
    }

    pub fn total_pot(&self) -> u32 {
        self.total_contrib.iter().sum()
    }

    pub fn pots(&self) -> Vec<Pot> {
        compute_pots(&self.total_contrib, &self.folded)
    }

    /// The strength of a seat's best current hand. Used by the solver's
    /// rollout policy and the feedback UI; safe to call at any point.
    pub fn eval_hand(&self, seat: Seat) -> Eval {
        self.best_hand(seat)
    }

    pub fn folded(&self, seat: Seat) -> bool {
        self.folded[seat.index()]
    }

    pub fn all_in(&self, seat: Seat) -> bool {
        self.all_in[seat.index()]
    }

    /// The hero's hole cards (always known).
    pub fn hero_cards(&self) -> [Card; 2] {
        self.hole_cards[Seat::Hero.index()]
    }

    /// A seat's hole cards, or `None` if they are not yet revealed to the
    /// hero (opponents' cards are hidden until showdown).
    pub fn hole_cards(&self, seat: Seat) -> Option<[Card; 2]> {
        if self.revealed[seat.index()] {
            Some(self.hole_cards[seat.index()])
        } else {
            None
        }
    }

    /// Clones the state with every seat's hole cards replaced by the given
    /// holdings. Solver-internal: builds the perfect-information snapshot a
    /// single determinization is searched on; street, pots and action flow
    /// are untouched.
    pub fn clone_with_hole_cards(&self, hole_cards: [[Card; 2]; NUM_PLAYERS]) -> GameState {
        let mut state = self.clone_without_result();
        state.hole_cards = hole_cards;
        state.revealed = [true; NUM_PLAYERS];
        state
    }

    fn clone_without_result(&self) -> GameState {
        GameState {
            stacks: self.stacks,
            button: self.button,
            blind_level: self.blind_level,
            street: self.street,
            board: self.board.clone(),
            hole_cards: self.hole_cards,
            revealed: self.revealed,
            street_contrib: self.street_contrib,
            total_contrib: self.total_contrib,
            current_bet: self.current_bet,
            min_raise: self.min_raise,
            last_full_raise: self.last_full_raise,
            acted: self.acted,
            folded: self.folded,
            all_in: self.all_in,
            to_act: self.to_act,
            hand_over: self.hand_over,
            hand_result: None,
        }
    }

    fn post_blind(&mut self, seat: Seat, amount: u32) {
        let posted = amount.min(self.stacks[seat.index()]);
        self.stacks[seat.index()] -= posted;
        self.street_contrib[seat.index()] += posted;
        self.total_contrib[seat.index()] += posted;
        if self.stacks[seat.index()] == 0 {
            self.all_in[seat.index()] = true;
        }
        if posted > self.current_bet {
            self.current_bet = posted;
        }
    }

    fn commit(&mut self, seat: Seat, amount: u32) {
        self.stacks[seat.index()] -= amount;
        self.street_contrib[seat.index()] += amount;
        self.total_contrib[seat.index()] += amount;
        if self.stacks[seat.index()] == 0 {
            self.all_in[seat.index()] = true;
        }
    }

    fn first_to_act(&self) -> Seat {
        action_order(self.button, self.street)
            .into_iter()
            .find(|&seat| !self.folded[seat.index()] && !self.all_in[seat.index()])
            .unwrap_or_else(|| action_order(self.button, self.street)[0])
    }

    fn active_players(&self) -> Vec<Seat> {
        Seat::ALL
            .into_iter()
            .filter(|&seat| !self.folded[seat.index()])
            .collect()
    }

    fn advance_to_act(&mut self) {
        let mut seat = self.to_act.next();
        for _ in 0..NUM_PLAYERS {
            if !self.folded[seat.index()] && !self.all_in[seat.index()] {
                self.to_act = seat;
                return;
            }
            seat = seat.next();
        }
    }

    fn round_complete(&self) -> bool {
        Seat::ALL.into_iter().all(|seat| {
            self.folded[seat.index()]
                || self.all_in[seat.index()]
                || (self.street_contrib[seat.index()] == self.current_bet
                    && self.acted[seat.index()])
        })
    }

    fn award_fold_win(&mut self, winner: Seat) {
        let total: u32 = self.total_contrib.iter().sum();
        self.stacks[winner.index()] += total;
        let pots = compute_pots(&self.total_contrib, &self.folded);
        self.hand_result = Some(HandResult {
            reason: HandEndReason::Fold(winner),
            awards: vec![PotAward {
                seat: winner,
                amount: total,
            }],
            pots,
            revealed: Vec::new(),
        });
    }

    fn best_hand(&self, seat: Seat) -> Eval {
        let mut cards = [Card::new(Rank::Two, Suit::Clubs); 7];
        cards[0] = self.hole_cards[seat.index()][0];
        cards[1] = self.hole_cards[seat.index()][1];
        for (index, &card) in self.board.iter().enumerate() {
            cards[2 + index] = card;
        }
        eval::evaluate(&cards)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::seeded_rng;

    fn card(code: &str) -> Card {
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
    }

    fn deck(seed: u64) -> Deck {
        Deck::shuffled(&mut seeded_rng(seed))
    }

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    #[test]
    fn new_initializes_stacks_and_button() {
        let state = GameState::new(Seat::Hero, level());
        assert_eq!(state.stacks(), [500, 500, 500]);
        assert_eq!(state.button(), Seat::Hero);
        assert_eq!(state.blind_level(), level());
        assert_eq!(state.street(), Street::Preflop);
        assert!(state.board().is_empty());
        assert!(!state.is_hand_over());
    }

    #[test]
    fn start_hand_deals_cards_and_posts_blinds() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(1)).unwrap();

        assert_eq!(state.stack(Seat::Hero), 490);
        assert_eq!(state.stack(Seat::Opponent1), 480);
        assert_eq!(state.stack(Seat::Opponent2), 500);
        assert_eq!(state.current_bet(), 20);
        assert_eq!(state.total_pot(), 30);
        assert_eq!(state.to_act(), Seat::Opponent2);
        assert!(state.hero_cards() != [card("2c"), card("2c")]);
        assert_eq!(state.hole_cards(Seat::Opponent1), None);
        assert_eq!(state.hole_cards(Seat::Opponent2), None);
    }

    #[test]
    fn start_hand_requires_enough_cards() {
        let mut state = GameState::new(Seat::Hero, level());
        let mut empty = Deck::new();
        for _ in 0..52 {
            empty.deal();
        }
        assert!(matches!(state.start_hand(&mut empty), Err(Error::Game(_))));
    }

    #[test]
    fn preflop_first_actor_can_fold_call_raise_or_all_in() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(1)).unwrap();
        let legal = state.legal_actions();
        assert!(legal.can_fold);
        assert!(!legal.can_check);
        assert!(legal.can_call);
        assert_eq!(legal.call_amount, 20);
        assert!(!legal.can_bet);
        assert!(legal.can_raise);
        assert_eq!(legal.min_raise_to, 40);
        assert_eq!(legal.max_raise_to, 500);
        assert!(legal.can_all_in);
    }

    #[test]
    fn big_blind_has_the_option_after_limps() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(1)).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent2);
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent1);

        let legal = state.legal_actions();
        assert!(!legal.can_fold);
        assert!(legal.can_check);
        assert!(!legal.can_call);
        assert!(legal.can_bet);
        assert_eq!(legal.min_bet, 20);
        assert!(legal.can_all_in);
    }

    #[test]
    fn full_hand_plays_to_showdown() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(7)).unwrap();

        // Preflop: everyone limps, BB checks.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        assert_eq!(state.street(), Street::Preflop);

        // Flop: everyone checks.
        state.advance_street(&mut deck(7)).unwrap();
        assert_eq!(state.street(), Street::Flop);
        assert_eq!(state.board().len(), 3);
        assert_eq!(state.to_act(), Seat::Opponent1);
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();

        // Turn: everyone checks.
        state.advance_street(&mut deck(7)).unwrap();
        assert_eq!(state.street(), Street::Turn);
        assert_eq!(state.board().len(), 4);
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();

        // River: everyone checks, then showdown.
        state.advance_street(&mut deck(7)).unwrap();
        assert_eq!(state.street(), Street::River);
        assert_eq!(state.board().len(), 5);
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.apply_action(Action::Check).unwrap();

        let result = state.showdown(&mut deck(7)).unwrap();
        assert_eq!(result.reason, HandEndReason::Showdown);
        assert_eq!(result.revealed.len(), 3);
        assert_eq!(result.awards.iter().map(|a| a.amount).sum::<u32>(), 60);
        assert!(state.is_hand_over());
    }

    #[test]
    fn fold_win_awards_the_pot() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(2)).unwrap();

        assert_eq!(
            state.apply_action(Action::Fold).unwrap(),
            ActionOutcome::Continue
        );
        assert_eq!(
            state.apply_action(Action::Fold).unwrap(),
            ActionOutcome::HandEnded
        );

        let result = state.hand_result().unwrap();
        assert_eq!(result.reason, HandEndReason::Fold(Seat::Opponent1));
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Opponent1,
                amount: 30
            }]
        );
        assert_eq!(state.stack(Seat::Opponent1), 510);
        assert!(state.is_hand_over());
    }

    #[test]
    fn showdown_awards_best_hand() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7c"), card("9c"), card("Jc"), card("3s")];
        state.total_contrib = [100, 100, 100];
        state.folded = [false, false, false];

        let result = state.showdown(&mut Deck::new()).unwrap();
        assert_eq!(result.reason, HandEndReason::Showdown);
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 300
            }]
        );
        assert_eq!(state.stack(Seat::Hero), 800);
    }

    #[test]
    fn eval_hand_ranks_seat_holdings() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7c"), card("9c"), card("Jc"), card("3s")];
        let hero = state.eval_hand(Seat::Hero);
        let opp1 = state.eval_hand(Seat::Opponent1);
        let opp2 = state.eval_hand(Seat::Opponent2);
        assert!(hero > opp1);
        assert!(opp1 > opp2);
        assert_eq!(hero.class(), crate::eval::HandClass::Pair);
    }

    #[test]
    fn set_stack_and_set_hole_cards_override_snapshot_fields() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(1)).unwrap();
        state.set_stack(Seat::Hero, 123);
        assert_eq!(state.stack(Seat::Hero), 123);
        let aces = [card("As"), card("Ad")];
        state.set_hole_cards(Seat::Hero, aces);
        assert_eq!(state.hero_cards(), aces);
    }

    #[test]
    fn clone_with_hole_cards_replaces_hands_and_keeps_action_state() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(1)).unwrap();
        state.apply_action(Action::Raise(60)).unwrap();

        let world_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        let clone = state.clone_with_hole_cards(world_cards);
        assert_eq!(clone.hero_cards(), world_cards[0]);
        assert_eq!(clone.hole_cards(Seat::Opponent1), Some(world_cards[1]));
        assert_eq!(clone.hole_cards(Seat::Opponent2), Some(world_cards[2]));
        assert_eq!(clone.to_act(), state.to_act());
        assert_eq!(clone.current_bet(), state.current_bet());
        assert_eq!(clone.total_pot(), state.total_pot());
        assert_eq!(clone.button(), state.button());
        assert_eq!(clone.street(), state.street());
        assert!(!clone.is_hand_over());
        assert_eq!(clone.hand_result(), None);
    }

    #[test]
    fn showdown_splits_ties_with_odd_chip_to_first_winner() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Ah"), card("Ac")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7d"), card("9h"), card("Js"), card("3c")];
        state.total_contrib = [101, 101, 101];
        state.folded = [false, false, false];

        let result = state.showdown(&mut Deck::new()).unwrap();
        let mut awards = result.awards.clone();
        awards.sort_by_key(|a| a.seat.index());
        assert_eq!(
            awards,
            vec![
                PotAward {
                    seat: Seat::Hero,
                    amount: 152
                },
                PotAward {
                    seat: Seat::Opponent1,
                    amount: 151
                },
            ]
        );
    }

    #[test]
    fn side_pot_is_awarded_separately() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7c"), card("9c"), card("Jc"), card("3s")];
        state.total_contrib = [100, 100, 50];
        state.folded = [false, false, false];

        let result = state.showdown(&mut Deck::new()).unwrap();
        assert_eq!(result.pots.len(), 2);
        let mut awards = result.awards.clone();
        awards.sort_by_key(|a| a.seat.index());
        assert_eq!(
            awards,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 250
            },]
        );
        assert_eq!(state.stack(Seat::Hero), 750);
    }

    #[test]
    fn all_in_for_less_does_not_reopen_action() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(3)).unwrap();

        // Opponent2 (first to act) raises to 100.
        state.apply_action(Action::Raise(100)).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        // Hero goes all-in for 150 (a raise of 50, less than the min raise of 80).
        state.stacks[Seat::Hero.index()] = 150;
        state.apply_action(Action::AllIn).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent1);

        // Opponent1 calls.
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent2);

        // Opponent2 faces the all-in but cannot re-raise (not a full raise).
        let legal = state.legal_actions();
        assert!(legal.can_call);
        assert!(!legal.can_raise);
    }

    #[test]
    fn illegal_actions_are_rejected() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(4)).unwrap();
        assert!(matches!(
            state.apply_action(Action::Check),
            Err(Error::Game(_))
        ));
        assert!(matches!(
            state.apply_action(Action::Bet(100)),
            Err(Error::Game(_))
        ));
        assert!(matches!(
            state.apply_action(Action::Raise(30)),
            Err(Error::Game(_))
        ));
    }

    #[test]
    fn actions_after_hand_over_are_rejected() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(5)).unwrap();
        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert!(matches!(
            state.apply_action(Action::Check),
            Err(Error::Game(_))
        ));
    }

    #[test]
    fn advance_street_past_river_is_rejected() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(6)).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck(6)).unwrap();
        state.advance_street(&mut deck(6)).unwrap();
        state.advance_street(&mut deck(6)).unwrap();
        assert_eq!(state.street(), Street::River);
        assert!(matches!(
            state.advance_street(&mut deck(6)),
            Err(Error::Game(_))
        ));
    }

    #[test]
    fn next_hand_rotates_the_button() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(8)).unwrap();
        assert_eq!(state.button(), Seat::Hero);
        state.next_hand(&mut deck(8)).unwrap();
        assert_eq!(state.button(), Seat::Opponent1);
        state.next_hand(&mut deck(8)).unwrap();
        assert_eq!(state.button(), Seat::Opponent2);
        state.next_hand(&mut deck(8)).unwrap();
        assert_eq!(state.button(), Seat::Hero);
    }

    #[test]
    fn blind_level_escalation() {
        let mut state = GameState::new(Seat::Hero, level());
        assert!(state.advance_blind_level());
        assert_eq!(state.blind_level(), BlindLevel::new(15, 30));
        state.set_blind_level(BlindLevel::new(1000, 2000));
        assert!(!state.advance_blind_level());
        assert_eq!(state.blind_level(), BlindLevel::new(1000, 2000));
    }

    #[test]
    fn short_stack_blind_posts_all_in() {
        let mut state = GameState::new(Seat::Hero, level());
        state.stacks[Seat::Hero.index()] = 5;
        state.start_hand(&mut deck(9)).unwrap();
        assert_eq!(state.stack(Seat::Hero), 0);
        assert!(state.all_in(Seat::Hero));
        assert_eq!(state.total_pot(), 25);
    }

    #[test]
    fn all_in_runout_resolves_showdown() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(10)).unwrap();

        // Everyone goes all-in preflop.
        state.apply_action(Action::AllIn).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        state.apply_action(Action::AllIn).unwrap();

        assert!(!state.can_continue_betting());
        let result = state.showdown(&mut deck(10)).unwrap();
        assert_eq!(result.reason, HandEndReason::Showdown);
        assert_eq!(result.awards.iter().map(|a| a.amount).sum::<u32>(), 1500);
    }

    #[test]
    fn bet_and_raise_flow_updates_current_bet() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(11)).unwrap();

        state.apply_action(Action::Raise(60)).unwrap();
        assert_eq!(state.current_bet(), 60);
        assert_eq!(state.to_act(), Seat::Hero);

        state.apply_action(Action::Raise(120)).unwrap();
        assert_eq!(state.current_bet(), 120);
        assert_eq!(state.to_act(), Seat::Opponent1);

        let legal = state.legal_actions();
        assert_eq!(legal.min_raise_to, 180);
    }

    #[test]
    fn pots_reflect_current_contributions() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(12)).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        assert_eq!(state.total_pot(), 60);
        assert_eq!(state.pots().len(), 1);
        assert_eq!(state.pots()[0].amount, 60);
    }
}
