use crate::card::{Card, Deck, Rank, Suit};
use crate::error::{Error, Result};
use crate::eval::{self, Eval, HandClass};
use crate::game::action::{Action, LegalActions};
use crate::game::blinds::{BlindLevel, next_level};
use crate::game::pot::{Pot, compute_pots};
use crate::game::seat::{Seat, Street, action_order};

/// Starting stack for every player in a Spin and Gold hand.
pub const STARTING_STACK: u32 = 300;
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
    /// Contested pot shares won: what a seat actually took from the pot(s).
    /// Uncalled portions of a bet are *not* wins — they go in `returns`.
    pub awards: Vec<PotAward>,
    /// Uncalled bet portions handed back at showdown. These are not wins:
    /// a seat receiving one did not take anything from the pot.
    pub returns: Vec<PotAward>,
    pub pots: Vec<Pot>,
    pub revealed: Vec<(Seat, [Card; 2], HandClass)>,
}

/// Reorders a three-slot array so new slot `n` takes old slot `order[n]`.
fn shifted<T: Copy>(values: &[T; NUM_PLAYERS], order: &[usize; NUM_PLAYERS]) -> [T; NUM_PLAYERS] {
    [values[order[0]], values[order[1]], values[order[2]]]
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
    /// Seats that have busted out of the tournament (zero chips at the end of
    /// a hand). Unlike `folded`/`all_in`, this persists across hands: an
    /// eliminated seat is never dealt cards, never posts a blind, and is
    /// skipped in the action order.
    eliminated: [bool; NUM_PLAYERS],
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
            eliminated: [false; NUM_PLAYERS],
            to_act: Seat::Hero,
            hand_over: false,
            hand_result: None,
        }
    }

    /// Deals a fresh hand: resets per-hand state, deals two cards to each
    /// active (non-eliminated) seat, and posts the blinds.
    pub fn start_hand(&mut self, deck: &mut Deck) -> Result<()> {
        let active = self.active_seats().len();
        if active < 2 {
            return Err(Error::Game(
                "tournament is over — cannot deal another hand".into(),
            ));
        }
        if deck.remaining() < active * 2 {
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
            if !self.eliminated[seat.index()] {
                let first = deck
                    .deal()
                    .ok_or_else(|| Error::Game("deck exhausted".into()))?;
                let second = deck
                    .deal()
                    .ok_or_else(|| Error::Game("deck exhausted".into()))?;
                self.hole_cards[seat.index()] = [first, second];
            }
            seat = seat.next();
        }

        self.post_blind(self.button, self.blind_level.small_blind);
        self.post_blind(self.next_active(self.button), self.blind_level.big_blind);

        self.to_act = self.first_to_act();
        Ok(())
    }

    /// Rotates the button to the next active seat and deals the next hand.
    pub fn next_hand(&mut self, deck: &mut Deck) -> Result<()> {
        self.button = self.next_active(self.button);
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
        // A "bet" is a raise-to amount, so when the street already has chips
        // in (the big blind playing its option) the minimum is a real
        // minimum raise, never a bet back down to the current amount.
        let min_bet = self
            .current_bet
            .saturating_add(self.min_raise)
            .max(self.blind_level.big_blind)
            .min(self.street_contrib[seat.index()] + stack);
        let max_bet = self.street_contrib[seat.index()] + stack;

        let can_raise = to_call > 0 && stack > to_call && self.last_full_raise != Some(seat);
        let max_raise_to = self.street_contrib[seat.index()] + stack;
        let min_raise_to = (self.current_bet + self.min_raise).min(max_raise_to);

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
                let previous_bet = self.current_bet;
                self.commit(
                    seat,
                    amount.saturating_sub(self.street_contrib[seat.index()]),
                );
                self.current_bet = amount;
                let raise_size = amount - previous_bet;
                if raise_size >= self.min_raise {
                    self.min_raise = raise_size;
                    self.last_full_raise = Some(seat);
                }
                self.acted[seat.index()] = true;
            }
            Action::Raise(amount) => {
                let previous_bet = self.current_bet;
                self.commit(seat, amount - self.street_contrib[seat.index()]);
                self.current_bet = amount;
                let raise_size = amount - previous_bet;
                if raise_size >= self.min_raise {
                    self.min_raise = raise_size;
                    self.last_full_raise = Some(seat);
                }
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
            if !self.eliminated[seat.index()] {
                self.revealed[seat.index()] = true;
            }
        }

        let pots = compute_pots(&self.total_contrib, &self.folded, &self.eliminated);
        let mut awards: Vec<PotAward> = Vec::new();
        let mut returns: Vec<PotAward> = Vec::new();
        let mut revealed = Vec::new();

        for seat in Seat::ALL {
            if !self.folded[seat.index()] && !self.eliminated[seat.index()] {
                let hand = self.best_hand(seat);
                revealed.push((seat, self.hole_cards[seat.index()], hand.class()));
            }
        }

        for pot in &pots {
            // A pot only one seat is eligible for is an uncalled bet portion
            // being handed back — it is returned, never "won".
            if pot.eligible.len() == 1
                && let Some(&seat) = pot.eligible.first()
            {
                self.stacks[seat.index()] += pot.amount;
                if let Some(award) = returns.iter_mut().find(|a| a.seat == seat) {
                    award.amount += pot.amount;
                } else {
                    returns.push(PotAward {
                        seat,
                        amount: pot.amount,
                    });
                }
                continue;
            }
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
            returns,
            pots,
            revealed,
        };
        self.hand_result = Some(result.clone());
        self.mark_eliminated();
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

    /// Overwrites a seat's eliminated flag. Used by tests to render a table
    /// with a busted seat; the engine sets this itself at hand end.
    pub fn set_eliminated(&mut self, seat: Seat, eliminated: bool) {
        self.eliminated[seat.index()] = eliminated;
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

    /// The seat posting the small blind: in 3-max the button is the small
    /// blind.
    pub fn small_blind_seat(&self) -> Seat {
        self.button
    }

    /// The seat posting the big blind: the next active seat after the
    /// button, skipping eliminated seats (heads-up this is the only
    /// opponent).
    pub fn big_blind_seat(&self) -> Seat {
        self.next_active(self.button)
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

    /// The chips this seat has committed on the current street (blind posts
    /// count toward the preflop street). Drives the per-seat bet badges the
    /// UI renders in front of each player.
    pub fn street_contribution(&self, seat: Seat) -> u32 {
        self.street_contrib[seat.index()]
    }

    pub fn total_pot(&self) -> u32 {
        self.total_contrib.iter().sum()
    }

    pub fn pots(&self) -> Vec<Pot> {
        compute_pots(&self.total_contrib, &self.folded, &self.eliminated)
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

    /// Whether a seat has busted out of the tournament (zero chips at the end
    /// of a hand). Eliminated seats are skipped in every subsequent hand.
    pub fn eliminated(&self, seat: Seat) -> bool {
        self.eliminated[seat.index()]
    }

    /// The seats still in the tournament (not eliminated).
    pub fn active_seats(&self) -> Vec<Seat> {
        Seat::ALL
            .into_iter()
            .filter(|&seat| !self.eliminated[seat.index()])
            .collect()
    }

    /// The tournament winner once only one seat remains, or `None` while the
    /// tournament is still running.
    pub fn tournament_winner(&self) -> Option<Seat> {
        let active = self.active_seats();
        if active.len() == 1 {
            Some(active[0])
        } else {
            None
        }
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

    /// A copy of the state re-labeled so `new_hero` occupies the hero seat,
    /// preserving the acting order. The seat-perspective solver evaluates
    /// another seat's decision by rotating it into the hero role; pots,
    /// contributions, and betting flow are untouched.
    pub fn rotated(&self, new_hero: Seat) -> GameState {
        if new_hero == Seat::Hero {
            return self.clone_without_result();
        }
        let offset = new_hero.index();
        let order = [
            offset,
            (offset + 1) % NUM_PLAYERS,
            (offset + 2) % NUM_PLAYERS,
        ];
        let shift = |old: usize| (old + NUM_PLAYERS - offset) % NUM_PLAYERS;
        let mut state = self.clone_without_result();
        state.stacks = shifted(&self.stacks, &order);
        state.street_contrib = shifted(&self.street_contrib, &order);
        state.total_contrib = shifted(&self.total_contrib, &order);
        state.folded = shifted(&self.folded, &order);
        state.all_in = shifted(&self.all_in, &order);
        state.eliminated = shifted(&self.eliminated, &order);
        state.acted = shifted(&self.acted, &order);
        state.hole_cards = shifted(&self.hole_cards, &order);
        state.revealed = shifted(&self.revealed, &order);
        state.button = Seat::ALL[shift(self.button.index())];
        state.to_act = Seat::ALL[shift(self.to_act.index())];
        state.last_full_raise = self
            .last_full_raise
            .map(|seat| Seat::ALL[shift(seat.index())]);
        state
    }

    /// Serializes the complete live state (cards, stacks, betting, ordering)
    /// for tournament persistence.
    pub fn to_snapshot(&self) -> crate::snapshot::StateSnapshot {
        use crate::snapshot::{HandResultSnapshot, StateSnapshot};
        let hole_cards = self
            .hole_cards
            .iter()
            .map(|cards| [cards[0].to_code(), cards[1].to_code()])
            .collect();
        let hand_result = self.hand_result.as_ref().map(|result| {
            let reason = match result.reason {
                HandEndReason::Fold(_) => "fold",
                HandEndReason::Showdown => "showdown",
            }
            .to_string();
            let awards = result
                .awards
                .iter()
                .map(|award| (award.seat.index() as u8, award.amount))
                .collect();
            let returns = result
                .returns
                .iter()
                .map(|award| (award.seat.index() as u8, award.amount))
                .collect();
            HandResultSnapshot {
                reason,
                awards,
                returns,
            }
        });
        StateSnapshot {
            stacks: self.stacks,
            button: self.button.index() as u8,
            blind_small: self.blind_level.small_blind,
            blind_big: self.blind_level.big_blind,
            street: match self.street {
                Street::Preflop => 0,
                Street::Flop => 1,
                Street::Turn => 2,
                Street::River => 3,
            },
            board: self.board.iter().map(|card| card.to_code()).collect(),
            hole_cards,
            revealed: self.revealed,
            street_contrib: self.street_contrib,
            total_contrib: self.total_contrib,
            current_bet: self.current_bet,
            min_raise: self.min_raise,
            last_full_raise: self.last_full_raise.map(|seat| seat.index() as u8),
            acted: self.acted,
            folded: self.folded,
            all_in: self.all_in,
            eliminated: self.eliminated,
            to_act: self.to_act.index() as u8,
            hand_over: self.hand_over,
            hand_result,
        }
    }

    /// Rebuilds a live state from its persisted snapshot. Opponents' hidden
    /// card codes are restored as dealt cards, and a paused end-of-hand state
    /// keeps its win ribbon.
    pub fn from_snapshot(snapshot: &crate::snapshot::StateSnapshot) -> Result<GameState> {
        fn seat(index: u8) -> Result<Seat> {
            match index {
                0 => Ok(Seat::Hero),
                1 => Ok(Seat::Opponent1),
                2 => Ok(Seat::Opponent2),
                other => Err(Error::Game(format!("invalid seat index {other}"))),
            }
        }
        fn street(index: u8) -> Result<Street> {
            match index {
                0 => Ok(Street::Preflop),
                1 => Ok(Street::Flop),
                2 => Ok(Street::Turn),
                3 => Ok(Street::River),
                other => Err(Error::Game(format!("invalid street index {other}"))),
            }
        }
        fn code_to_card(code: &str) -> Result<Card> {
            Card::from_code(code).ok_or_else(|| Error::Game(format!("invalid card code {code:?}")))
        }

        let button = seat(snapshot.button)?;
        let to_act = seat(snapshot.to_act)?;
        let street = street(snapshot.street)?;
        let mut hole_cards = [[Card::new(Rank::Two, Suit::Clubs); 2]; NUM_PLAYERS];
        if snapshot.hole_cards.len() != NUM_PLAYERS {
            return Err(Error::Game(format!(
                "expected {} hole-card pairs, found {}",
                NUM_PLAYERS,
                snapshot.hole_cards.len()
            )));
        }
        for (seat_index, codes) in snapshot.hole_cards.iter().enumerate() {
            hole_cards[seat_index] = [code_to_card(&codes[0])?, code_to_card(&codes[1])?];
        }
        let board = snapshot
            .board
            .iter()
            .map(|code| code_to_card(code))
            .collect::<Result<Vec<_>>>()?;
        let last_full_raise = snapshot.last_full_raise.map(seat).transpose()?;

        let mut state = GameState {
            stacks: snapshot.stacks,
            button,
            blind_level: BlindLevel::new(snapshot.blind_small, snapshot.blind_big),
            street,
            board,
            hole_cards,
            revealed: snapshot.revealed,
            street_contrib: snapshot.street_contrib,
            total_contrib: snapshot.total_contrib,
            current_bet: snapshot.current_bet,
            min_raise: snapshot.min_raise,
            last_full_raise,
            acted: snapshot.acted,
            folded: snapshot.folded,
            all_in: snapshot.all_in,
            eliminated: snapshot.eliminated,
            to_act,
            hand_over: snapshot.hand_over,
            hand_result: None,
        };
        if let Some(result) = &snapshot.hand_result {
            state.hand_result = Some(crate::snapshot::reconstruct_hand_result(&state, result)?);
        }
        Ok(state)
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
            eliminated: self.eliminated,
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

    /// Marks every seat with zero chips as eliminated. Called once a hand
    /// ends (fold win or showdown), after the pot has been awarded, so a
    /// player who busts is out of the tournament from the next hand on.
    fn mark_eliminated(&mut self) {
        for seat in Seat::ALL {
            if self.stacks[seat.index()] == 0 {
                self.eliminated[seat.index()] = true;
            }
        }
    }

    /// The next non-eliminated seat clockwise from `seat` (wrapping). Used to
    /// rotate the button and find the big blind past busted seats.
    fn next_active(&self, seat: Seat) -> Seat {
        let mut candidate = seat.next();
        for _ in 0..NUM_PLAYERS {
            if !self.eliminated[candidate.index()] {
                return candidate;
            }
            candidate = candidate.next();
        }
        seat
    }

    fn first_to_act(&self) -> Seat {
        action_order(self.button, self.street)
            .into_iter()
            .find(|&seat| {
                !self.folded[seat.index()]
                    && !self.all_in[seat.index()]
                    && !self.eliminated[seat.index()]
            })
            .unwrap_or_else(|| action_order(self.button, self.street)[0])
    }

    fn active_players(&self) -> Vec<Seat> {
        Seat::ALL
            .into_iter()
            .filter(|&seat| !self.folded[seat.index()] && !self.eliminated[seat.index()])
            .collect()
    }

    fn advance_to_act(&mut self) {
        let mut seat = self.to_act.next();
        for _ in 0..NUM_PLAYERS {
            if !self.folded[seat.index()]
                && !self.all_in[seat.index()]
                && !self.eliminated[seat.index()]
            {
                self.to_act = seat;
                return;
            }
            seat = seat.next();
        }
    }

    fn round_complete(&self) -> bool {
        Seat::ALL.into_iter().all(|seat| {
            self.eliminated[seat.index()]
                || self.folded[seat.index()]
                || self.all_in[seat.index()]
                || (self.street_contrib[seat.index()] == self.current_bet
                    && self.acted[seat.index()])
        })
    }

    fn award_fold_win(&mut self, winner: Seat) {
        let total: u32 = self.total_contrib.iter().sum();
        self.stacks[winner.index()] += total;
        let pots = compute_pots(&self.total_contrib, &self.folded, &self.eliminated);
        self.hand_result = Some(HandResult {
            reason: HandEndReason::Fold(winner),
            awards: vec![PotAward {
                seat: winner,
                amount: total,
            }],
            returns: Vec::new(),
            pots,
            revealed: Vec::new(),
        });
        self.mark_eliminated();
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
        assert_eq!(state.stacks(), [300, 300, 300]);
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

        assert_eq!(state.stack(Seat::Hero), 290);
        assert_eq!(state.stack(Seat::Opponent1), 280);
        assert_eq!(state.stack(Seat::Opponent2), 300);
        assert_eq!(state.current_bet(), 20);
        assert_eq!(state.total_pot(), 30);
        assert_eq!(state.to_act(), Seat::Opponent2);
        assert!(state.hero_cards() != [card("2c"), card("2c")]);
        assert_eq!(state.hole_cards(Seat::Opponent1), None);
        assert_eq!(state.hole_cards(Seat::Opponent2), None);
    }

    #[test]
    fn street_contribution_tracks_this_streets_chips_only() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(1)).unwrap();

        // Blinds count toward the preflop street: button posted SB, BB seat BB.
        assert_eq!(state.street_contribution(Seat::Hero), 10);
        assert_eq!(state.street_contribution(Seat::Opponent1), 20);
        assert_eq!(state.street_contribution(Seat::Opponent2), 0);

        // Opponent 2 limps 20: their street chips grow to match the blind.
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.street_contribution(Seat::Opponent2), 20);
        assert_eq!(state.street_contribution(Seat::Hero), 10);

        // A new street zeroes every seat's street chips.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state.advance_street(&mut deck(2)).unwrap();
        for seat in Seat::ALL {
            assert_eq!(state.street_contribution(seat), 0, "{seat}");
        }
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
        assert_eq!(legal.max_raise_to, 300);
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
        // A "bet" is a raise-to amount, so the BB's option begins at a true
        // minimum raise: 20 in the pot plus the 20 minimum raise.
        assert_eq!(legal.min_bet, 40);
        assert!(legal.can_all_in);
    }

    #[test]
    fn big_blind_bet_closes_the_round_cleanly() {
        // Regression: hero is the BB (button on Opponent 2, Hand #3 layout),
        // both opponents limp, and the hero bets (raise-to) 60. The posted
        // blind must count toward that total — committing the full 60 again
        // left the hero at 80 while the opponents sat at 60, which wedged
        // `round_complete` and stranded the hand preflop with no flop.
        let mut state = GameState::new(Seat::Opponent2, level());
        state.start_hand(&mut deck(16)).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent1);
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Opponent2);
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let legal = state.legal_actions();
        assert!(legal.can_bet);
        assert_eq!(legal.min_bet, 40, "the BB's rebet is at least a min raise");
        assert!(legal.allows(Action::Bet(60)));

        assert_eq!(state.street_contribution(Seat::Hero), 20);
        state.apply_action(Action::Bet(60)).unwrap();
        assert_eq!(
            state.street_contribution(Seat::Hero),
            60,
            "the posted blind counts toward the bet total"
        );
        assert_eq!(state.current_bet(), 60);
        assert_eq!(state.stack(Seat::Hero), 240);

        assert_eq!(
            state.apply_action(Action::Call).unwrap(),
            ActionOutcome::Continue
        );
        assert_eq!(
            state.apply_action(Action::Call).unwrap(),
            ActionOutcome::StreetEnded,
            "both calls close the round, so the flop can be dealt"
        );
        for seat in Seat::ALL {
            assert_eq!(state.street_contribution(seat), 60, "{seat}");
        }

        state.advance_street(&mut Deck::default()).unwrap();
        assert_eq!(state.street(), Street::Flop);
        assert_eq!(state.board().len(), 3);
    }

    #[test]
    fn short_stack_min_raise_never_exceeds_all_in() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(5)).unwrap();

        assert_eq!(state.to_act(), Seat::Opponent2);
        state.apply_action(Action::Raise(180)).unwrap();

        assert_eq!(state.to_act(), Seat::Hero);
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.stack(Seat::Hero), 120);

        assert_eq!(state.to_act(), Seat::Opponent1);
        state.apply_action(Action::Fold).unwrap();

        state.advance_street(&mut deck(5)).unwrap();
        assert_eq!(state.street(), Street::Flop);
        assert_eq!(state.to_act(), Seat::Opponent2);
        state.apply_action(Action::Bet(100)).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let legal = state.legal_actions();
        assert!(legal.can_raise);
        assert!(legal.can_all_in);
        assert_eq!(legal.min_raise_to, 120);
        assert_eq!(legal.max_raise_to, 120);
        assert!(legal.min_raise_to <= legal.max_raise_to);
        assert!(legal.allows(Action::Raise(120)));
        assert!(!legal.allows(Action::Raise(121)));

        state.apply_action(Action::Raise(120)).unwrap();
        assert_eq!(state.current_bet(), 120);
        assert_eq!(state.stack(Seat::Hero), 0);
        assert_eq!(state.min_raise, 100);
        assert_eq!(state.last_full_raise, Some(Seat::Opponent2));
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
        assert_eq!(state.stack(Seat::Opponent1), 310);
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
        assert_eq!(state.stack(Seat::Hero), 600);
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
        assert_eq!(state.stack(Seat::Hero), 550);
    }

    /// The reported regression: hero's two pair (Aces + board Threes) beats an
    /// opponent's lone pair of board Threes. The opponent's uncalled 5 chips
    /// are returned, never mixed into the awards as a split.
    #[test]
    fn uncalled_excess_is_returned_not_split() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("2s")],
            [card("7h"), card("7d")],
            [card("Qs"), card("6d")],
        ];
        state.board = vec![card("3c"), card("Ac"), card("9d"), card("3h"), card("8d")];
        state.total_contrib = [210, 10, 215];
        state.folded = [false, true, false];

        let result = state.showdown(&mut Deck::new()).unwrap();
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 430
            }]
        );
        assert_eq!(
            result.returns,
            vec![PotAward {
                seat: Seat::Opponent2,
                amount: 5
            }]
        );
        let hero_class = result
            .revealed
            .iter()
            .find(|(seat, _, _)| *seat == Seat::Hero)
            .map(|(_, _, class)| *class)
            .unwrap();
        assert_eq!(hero_class, HandClass::TwoPair);
        assert_eq!(state.stack(Seat::Hero), 730);
        assert_eq!(state.stack(Seat::Opponent2), 305);
    }

    /// A seat that loses the pot but put in the most chips still gets its
    /// uncalled excess back — and it is not an award, so a lost hand never
    /// counts as a win.
    #[test]
    fn loser_gets_uncalled_chips_back_but_no_award() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("2d"), card("3d")],
            [card("5s"), card("5c")],
            [card("Kh"), card("Kd")],
        ];
        state.board = vec![card("4c"), card("4s"), card("9h"), card("Jd"), card("Ac")];
        state.total_contrib = [300, 10, 295];
        state.folded = [false, true, false];

        let result = state.showdown(&mut Deck::new()).unwrap();
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Opponent2,
                amount: 600
            }]
        );
        assert_eq!(
            result.returns,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 5
            }]
        );
        assert_eq!(state.stack(Seat::Hero), 305);
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
        assert_eq!(
            result.awards.iter().map(|a| a.amount).sum::<u32>(),
            STARTING_STACK * 3
        );
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

    #[test]
    fn showdown_marks_busted_seats_eliminated() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7c"), card("9c"), card("Jc"), card("3s")];
        state.stacks = [0, 0, 0];
        state.total_contrib = [500, 500, 500];
        state.folded = [false, false, false];

        state.showdown(&mut Deck::new()).unwrap();
        assert_eq!(state.stack(Seat::Hero), 1500);
        assert!(!state.eliminated(Seat::Hero));
        assert!(state.eliminated(Seat::Opponent1));
        assert!(state.eliminated(Seat::Opponent2));
        assert_eq!(state.tournament_winner(), Some(Seat::Hero));
    }

    #[test]
    fn showdown_skips_eliminated_seats() {
        // Opponent 1 busted out of an earlier hand: at the next hand's
        // showdown (heads-up between the hero and Opponent 2) they hold no
        // cards, reveal nothing, and contest no pot.
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7c"), card("9c"), card("Jc"), card("3s")];
        state.total_contrib = [100, 100, 100];
        state.folded = [false, false, false];
        state.eliminated[Seat::Opponent1.index()] = true;

        let result = state.showdown(&mut Deck::new()).unwrap();
        assert_eq!(result.revealed.len(), 2);
        assert!(
            result
                .revealed
                .iter()
                .all(|(seat, _, _)| *seat != Seat::Opponent1),
            "an eliminated seat shows no cards: {result:?}"
        );
        assert_eq!(
            state.hole_cards(Seat::Opponent1),
            None,
            "an eliminated seat's cards stay hidden after the showdown"
        );
        assert_eq!(
            result.awards,
            vec![PotAward {
                seat: Seat::Hero,
                amount: 300
            }],
            "the eliminated seat never contests the pot"
        );
        assert_eq!(state.stack(Seat::Hero), 600);
        assert_eq!(
            state.stack(Seat::Opponent1),
            300,
            "the eliminated seat never contests the pot"
        );
        assert!(
            state.tournament_winner().is_none(),
            "two seats remain in play"
        );
    }

    #[test]
    fn tournament_winner_is_none_while_multiple_seats_remain() {
        let state = GameState::new(Seat::Hero, level());
        assert_eq!(state.tournament_winner(), None);
        assert_eq!(state.active_seats().len(), 3);
    }

    #[test]
    fn next_active_skips_eliminated_seats() {
        let mut state = GameState::new(Seat::Hero, level());
        state.eliminated[Seat::Opponent1.index()] = true;
        assert_eq!(state.next_active(Seat::Hero), Seat::Opponent2);
        assert_eq!(state.next_active(Seat::Opponent2), Seat::Hero);
    }

    #[test]
    fn start_hand_skips_eliminated_seats_for_cards_and_blinds() {
        let mut state = GameState::new(Seat::Hero, level());
        state.eliminated[Seat::Opponent1.index()] = true;
        state.start_hand(&mut deck(13)).unwrap();

        // Only two active seats: the button (hero) posts the SB, the next
        // active seat (Opponent 2) posts the BB.
        assert_eq!(state.stack(Seat::Hero), 290);
        assert_eq!(state.stack(Seat::Opponent2), 280);
        assert_eq!(
            state.stack(Seat::Opponent1),
            300,
            "eliminated seat is untouched"
        );
        assert_eq!(state.total_pot(), 30);
        assert_eq!(state.to_act(), Seat::Opponent2);
    }

    #[test]
    fn next_hand_rotates_the_button_past_eliminated_seats() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(14)).unwrap();
        state.eliminated[Seat::Opponent1.index()] = true;
        state.next_hand(&mut deck(14)).unwrap();
        assert_eq!(state.button(), Seat::Opponent2);
    }

    #[test]
    fn dealing_with_one_active_seat_is_rejected() {
        let mut state = GameState::new(Seat::Hero, level());
        state.eliminated[Seat::Opponent1.index()] = true;
        state.eliminated[Seat::Opponent2.index()] = true;
        assert!(matches!(
            state.start_hand(&mut deck(15)),
            Err(Error::Game(_))
        ));
    }

    /// The full live state survives a snapshot round trip, including betting
    /// mid-hand details that no accessor must silently drop.
    #[test]
    fn snapshot_round_trip_preserves_the_complete_state() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(30)).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Raise(80)).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.advance_street(&mut deck(30)).unwrap();
        state.apply_action(Action::Bet(50)).unwrap();
        state.apply_action(Action::Raise(150)).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let revived = GameState::from_snapshot(&state.to_snapshot()).unwrap();
        assert_eq!(revived.to_snapshot(), state.to_snapshot());
        assert_eq!(revived.stacks(), state.stacks());
        assert_eq!(revived.button(), state.button());
        assert_eq!(revived.blind_level(), state.blind_level());
        assert_eq!(revived.street(), state.street());
        assert_eq!(revived.board(), state.board());
        assert_eq!(revived.current_bet(), state.current_bet());
        assert_eq!(revived.total_pot(), state.total_pot());
        assert_eq!(revived.to_act(), state.to_act());
        assert_eq!(revived.hero_cards(), state.hero_cards());
        assert_eq!(
            revived.hole_cards(Seat::Opponent1),
            state.hole_cards(Seat::Opponent1)
        );
        assert_eq!(
            revived.hole_cards(Seat::Opponent2),
            state.hole_cards(Seat::Opponent2)
        );
        assert_eq!(
            revived.legal_actions(),
            state.legal_actions(),
            "the resumed legal set (bet sizes, raise bounds) is identical"
        );

        // The revived state continues to play legally.
        let action = match revived.legal_actions() {
            legal if legal.can_check => Action::Check,
            legal if legal.can_call => Action::Call,
            _ => Action::Fold,
        };
        assert!(revived.legal_actions().allows(action));
    }

    #[test]
    fn snapshot_restores_a_finished_hand_with_its_award() {
        let mut state = GameState::new(Seat::Hero, level());
        state.hole_cards = [
            [card("As"), card("Ad")],
            [card("Kh"), card("Kd")],
            [card("Qh"), card("Qd")],
        ];
        state.board = vec![card("2c"), card("7c"), card("9c"), card("Jc"), card("3s")];
        state.total_contrib = [100, 100, 100];
        state.folded = [false, false, false];
        state.stacks = [400, 400, 400];
        state.showdown(&mut Deck::new()).unwrap();
        assert!(state.is_hand_over());

        let revived = GameState::from_snapshot(&state.to_snapshot()).unwrap();
        assert!(revived.is_hand_over());
        assert_eq!(
            revived.hand_result().unwrap().awards,
            state.hand_result().unwrap().awards
        );
    }

    #[test]
    fn malformed_state_snapshots_are_rejected() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck(31)).unwrap();
        let snapshot = state.to_snapshot();

        let mut bad = snapshot.clone();
        bad.button = 7;
        assert!(matches!(
            GameState::from_snapshot(&bad),
            Err(Error::Game(_))
        ));

        let mut bad = snapshot.clone();
        bad.street = 9;
        assert!(matches!(
            GameState::from_snapshot(&bad),
            Err(Error::Game(_))
        ));

        let mut bad = snapshot.clone();
        bad.to_act = 3;
        assert!(matches!(
            GameState::from_snapshot(&bad),
            Err(Error::Game(_))
        ));

        let mut bad = snapshot.clone();
        bad.board = vec!["Zz".to_string()];
        assert!(matches!(
            GameState::from_snapshot(&bad),
            Err(Error::Game(_))
        ));

        let mut bad = snapshot.clone();
        bad.hole_cards = vec![["As".to_string(), "Kd".to_string()]];
        assert!(matches!(
            GameState::from_snapshot(&bad),
            Err(Error::Game(_))
        ));
    }

    /// The snapshot of an all-in hand keeps the stack-zero state, so an
    /// all-in shove paused mid-hand never gains chips back on resume.
    #[test]
    fn snapshot_of_an_all_in_keeps_the_zero_stack() {
        // Button on Opponent 1 makes Opponent 2 the big blind, so the hero
        // (left of the big blind) is first to act preflop — shoving empties
        // the hero's stack at once.
        let mut state = GameState::new(Seat::Opponent1, level());
        state.start_hand(&mut deck(32)).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state.apply_action(Action::AllIn).unwrap();
        assert_eq!(state.stack(Seat::Hero), 0);
        assert!(state.all_in(Seat::Hero));

        let revived = GameState::from_snapshot(&state.to_snapshot()).unwrap();
        assert_eq!(revived.stacks(), state.stacks());
        assert!(revived.all_in(Seat::Hero));
        assert_eq!(revived.to_act(), state.to_act());
        assert_eq!(revived.current_bet(), state.current_bet());
        assert_eq!(revived.legal_actions(), state.legal_actions());
    }
}
