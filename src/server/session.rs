use rand::Rng;

use crate::card::Deck;
use crate::decision::{self, AnalyzedDecision, SurvivalConfig, validate_action};
use crate::error::Result;
use crate::game::blinds::BLIND_SCHEDULE;
use crate::game::{Action, ActionOutcome, GameState, HandEndReason, Seat, Street};
use crate::mcts::MctsConfig;
use crate::range::BetSize;
use crate::range::hands::{HAND_COUNT, Range};
use crate::rng::SeededRng;
use crate::server::views;

/// Maximum number of action-log lines kept for the table fragment.
const MAX_LOG_LINES: usize = 28;
/// A full hand consumes at most this many cards (6 hole + 5 board).
const MIN_DECK_FOR_HAND: usize = 11;

/// Events produced by one session step; the WebSocket layer maps them onto
/// [`super::protocol::ServerMessage`]s.
#[derive(Clone, Debug, PartialEq)]
pub enum TableEvent {
    /// The table state changed: render and send a [`TABLE_STATE_UPDATE`]
    /// fragment.
    ///
    /// [`TABLE_STATE_UPDATE`]: super::protocol::ServerMessage::TableStateUpdate
    State,
    /// The played action was worse than optimal: overlay a full tactical
    /// breakdown. Placeholder trigger — S8 replaces this with the dynamic
    /// ~1-in-3-hand threshold logic.
    TacticalOverlay {
        decision: AnalyzedDecision,
        hand_no: u64,
    },
    /// One evaluated action for the top-bar EV tracker. S9 adds the decimated
    /// 1,000-action dataset.
    ChartTick { action_index: u64, ev_loss: f64 },
}

/// A live table session: one game state, a deck, the solver configuration, and
/// a placeholder policy for the two opponents. Each WebSocket connection owns
/// one session.
pub struct TableSession {
    state: GameState,
    deck: Deck,
    mcts: MctsConfig,
    survival: SurvivalConfig,
    ranges: [Range; 2],
    hand_no: u64,
    action_no: u64,
    log: Vec<String>,
    rng: SeededRng,
}

impl TableSession {
    /// A fresh session at the first blind level with a shuffled deck.
    pub fn new(seed: u64, mcts: MctsConfig, survival: SurvivalConfig) -> Self {
        let mut rng = crate::rng::seeded_rng(seed);
        let deck = Deck::shuffled(&mut rng);
        Self {
            state: GameState::new(Seat::Hero, BLIND_SCHEDULE[0]),
            deck,
            mcts,
            survival,
            ranges: uniform_ranges(),
            hand_no: 0,
            action_no: 0,
            log: Vec::new(),
            rng,
        }
    }

    /// Continues a session from an already-dealt state (used by tests).
    pub fn resume(
        state: GameState,
        deck: Deck,
        hand_no: u64,
        seed: u64,
        mcts: MctsConfig,
        survival: SurvivalConfig,
    ) -> Self {
        Self {
            state,
            deck,
            mcts,
            survival,
            ranges: uniform_ranges(),
            hand_no,
            action_no: 0,
            log: Vec::new(),
            rng: crate::rng::seeded_rng(seed),
        }
    }

    pub fn state(&self) -> &GameState {
        &self.state
    }

    pub fn hand_no(&self) -> u64 {
        self.hand_no
    }

    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// Deals the next hand, rotating the button and reshuffling an exhausted
    /// deck.
    pub fn deal_next_hand(&mut self) -> crate::error::Result<()> {
        if self.deck.remaining() < MIN_DECK_FOR_HAND {
            self.deck.shuffle(&mut self.rng);
        }
        if self.hand_no == 0 {
            self.state.start_hand(&mut self.deck)?;
        } else {
            self.state.next_hand(&mut self.deck)?;
        }
        self.hand_no += 1;
        self.log_line(format!(
            "— Hand #{} — blinds {}/{}",
            self.hand_no,
            self.state.blind_level().small_blind,
            self.state.blind_level().big_blind
        ));
        Ok(())
    }

    /// Drives the opponents until a decision or the end of the hand is
    /// reached: hands that end without hero action are logged and followed by
    /// the next deal. Returns whether anything happened.
    pub fn pump(&mut self) -> Result<bool> {
        let mut acted = false;
        loop {
            if self.state.is_hand_over() {
                self.log_hand_result();
                self.deal_next_hand()?;
                acted = true;
                continue;
            }
            if self.state.to_act() == Seat::Hero {
                return Ok(acted);
            }
            let actor = self.state.to_act();
            let call_amount = self.state.legal_actions().call_amount;
            let action = placeholder_action(&mut self.rng, &self.state);
            apply_settled(&mut self.state, &mut self.deck, action)?;
            self.log_line(views::describe_action(actor, action, call_amount));
            acted = true;
        }
    }

    /// Validates, analyzes, and applies a hero action, returning the events to
    /// publish. The solve runs synchronously in the calling task (the local,
    /// single-user WebSocket connection).
    pub fn submit(&mut self, action: Action) -> Result<Vec<TableEvent>> {
        validate_action(&self.state, action)?;

        let analyzed = decision::analyze(
            &mut self.rng,
            &self.state,
            &self.ranges,
            &self.mcts,
            &self.survival,
            Some(action),
        )?;

        self.action_no += 1;
        let ev_loss = analyzed
            .played
            .as_ref()
            .map(|played| played.ev_loss)
            .unwrap_or(0.0);

        let mut events = Vec::new();
        if ev_loss > 0.0 {
            events.push(TableEvent::TacticalOverlay {
                decision: analyzed.clone(),
                hand_no: self.hand_no,
            });
        }
        events.push(TableEvent::ChartTick {
            action_index: self.action_no,
            ev_loss,
        });

        let call_amount = self.state.legal_actions().call_amount;
        self.log_line(views::describe_action(Seat::Hero, action, call_amount));
        apply_settled(&mut self.state, &mut self.deck, action)?;
        if self.state.is_hand_over() {
            self.log_hand_result();
        }
        self.pump()?;

        events.push(TableEvent::State);
        Ok(events)
    }

    fn log_line(&mut self, line: String) {
        while self.log.len() >= MAX_LOG_LINES {
            self.log.remove(0);
        }
        self.log.push(line);
    }

    fn log_hand_result(&mut self) {
        let Some(result) = self.state.hand_result() else {
            return;
        };
        let total: u32 = result.awards.iter().map(|award| award.amount).sum();
        match result.reason {
            HandEndReason::Fold(winner) => {
                self.log_line(format!("{winner} win {total} — everyone else folded"));
            }
            HandEndReason::Showdown => {
                let winners: Vec<String> = result
                    .awards
                    .iter()
                    .map(|award| format!("{} +{}", award.seat, award.amount))
                    .collect();
                self.log_line(format!("Showdown · {}", winners.join(" · ")));
            }
        }
    }
}

fn uniform_ranges() -> [Range; 2] {
    [
        [1.0 / HAND_COUNT as f32; HAND_COUNT],
        [1.0 / HAND_COUNT as f32; HAND_COUNT],
    ]
}

/// Applies an action and settles any street or hand boundary it creates:
/// advances streets that can continue, resolves showdowns (dealing out the
/// board) when betting cannot.
pub fn apply_settled(
    state: &mut GameState,
    deck: &mut Deck,
    action: Action,
) -> Result<ActionOutcome> {
    let outcome = state.apply_action(action)?;
    if outcome == ActionOutcome::StreetEnded {
        if state.can_continue_betting() && !matches!(state.street(), Street::River) {
            state.advance_street(deck)?;
        } else {
            state.showdown(deck)?;
        }
    }
    Ok(outcome)
}

/// S7 placeholder policy for the opponents: checks any free option, otherwise
/// mostly calls, occasionally folds, and rarely min-raises; bets the minimum
/// (sometimes half pot) when first in. Busted (zero-stack) seats always take
/// the only actions available to them. Legality is guaranteed by construction.
pub fn placeholder_action<R: Rng + ?Sized>(rng: &mut R, state: &GameState) -> Action {
    let legal = state.legal_actions();
    let seat = state.to_act();
    let stack = state.stack(seat);

    if stack == 0 {
        return if legal.can_check {
            Action::Check
        } else {
            Action::Fold
        };
    }

    let roll: u32 = rng.random_range(0..100);

    if legal.can_check {
        if legal.can_bet && roll >= 85 {
            let amount = if roll >= 93 {
                BetSize::HalfPot.to_raise_to(
                    state.total_pot(),
                    0,
                    state.blind_level().big_blind,
                    legal.min_bet,
                    state.stack(seat),
                )
            } else {
                legal.min_bet
            };
            return if amount >= state.stack(seat) {
                Action::AllIn
            } else {
                Action::Bet(amount)
            };
        }
        return Action::Check;
    }

    let min_raise_to = legal.min_raise_to;
    if roll < 15 {
        return Action::Fold;
    }
    if roll < 75 || !legal.can_raise {
        return if legal.can_call {
            Action::Call
        } else {
            Action::AllIn
        };
    }
    if legal.allows(Action::Raise(min_raise_to)) {
        Action::Raise(min_raise_to)
    } else if legal.can_all_in {
        Action::AllIn
    } else {
        Action::Call
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{Card, Deck, Rank, Suit};
    use crate::decision::SurvivalConfig;
    use crate::game::STARTING_STACK;
    use crate::game::Street;
    use crate::game::blinds::BlindLevel;
    use crate::mcts::{self};
    use crate::range::hands::{HAND_COUNT, Range};
    use crate::rng::seeded_rng;

    fn card(rank: Rank, suit: Suit) -> Card {
        Card::new(rank, suit)
    }

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn probe_config() -> MctsConfig {
        MctsConfig::test()
    }

    fn survival() -> SurvivalConfig {
        SurvivalConfig::default()
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

    /// Board `Th Jh Qh 2d 4d`; hero (button) faces Opponent 1's river bet of
    /// 100 with junk.
    fn river_facing_bet() -> GameState {
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
        state.set_hole_cards(
            Seat::Hero,
            [
                card(Rank::Seven, Suit::Diamonds),
                card(Rank::Two, Suit::Clubs),
            ],
        );

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

    fn uniform() -> Range {
        [1.0 / HAND_COUNT as f32; HAND_COUNT]
    }

    #[test]
    fn dealing_and_pumping_reach_the_hero() {
        let mut session = TableSession::new(41, probe_config(), survival());
        session.deal_next_hand().unwrap();
        session.pump().unwrap();
        assert_eq!(session.state().to_act(), Seat::Hero);
        assert_eq!(session.hand_no(), 1);
        assert!(!session.state().is_hand_over());
    }

    #[test]
    fn pump_stops_immediately_when_it_is_already_the_heros_turn() {
        let state = river_facing_bet();
        let mut session =
            TableSession::resume(state, Deck::default(), 3, 42, probe_config(), survival());
        assert!(!session.pump().unwrap());
        assert_eq!(session.state().to_act(), Seat::Hero);
        assert_eq!(session.hand_no(), 3);
    }

    #[test]
    fn submit_emits_a_chart_tick_and_a_state_update() {
        let state = river_facing_bet();
        let mut session =
            TableSession::resume(state, Deck::default(), 1, 43, probe_config(), survival());
        let events = session.submit(Action::Fold).unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                TableEvent::ChartTick {
                    action_index: 1,
                    ..
                }
            )),
            "every submitted action is charted"
        );
        assert!(events.contains(&TableEvent::State));
        assert_eq!(
            session.hand_no(),
            2,
            "folding here ends the hand; the next one is dealt"
        );
        assert!(!session.state().is_hand_over());
        assert_eq!(session.state().to_act(), Seat::Hero);
    }

    #[test]
    fn suboptimal_actions_trigger_the_overlay_optimal_ones_do_not() {
        let state = river_facing_bet();
        let mut probe_rng = seeded_rng(44);
        let probed = decision::analyze(
            &mut probe_rng,
            &state,
            &[uniform(), uniform()],
            &probe_config(),
            &survival(),
            None,
        )
        .unwrap();
        let optimal = probed.optimal.action;
        let alternative = *mcts::candidates(&state)
            .iter()
            .map(|(action, _)| action)
            .find(|action| **action != optimal)
            .expect("a river-facing state has multiple candidates");

        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            44,
            probe_config(),
            survival(),
        );
        let events = session.submit(alternative).unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TableEvent::TacticalOverlay { .. })),
            "a suboptimal play must trigger the tactical overlay"
        );

        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            44,
            probe_config(),
            survival(),
        );
        let events = session.submit(optimal).unwrap();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TableEvent::TacticalOverlay { .. })),
            "an optimal play must not trigger the tactical overlay"
        );
        assert!(events.iter().any(
            |event| matches!(event, TableEvent::ChartTick { ev_loss, .. } if *ev_loss == 0.0)
        ));
    }

    #[test]
    fn submit_rejects_illegal_actions_without_touching_the_state() {
        let state = river_facing_bet();
        let mut session =
            TableSession::resume(state, Deck::default(), 1, 45, probe_config(), survival());
        // Snapshot what the session looked like before the rejected action.
        let to_act = session.state().to_act();
        assert!(session.submit(Action::Check).is_err());
        assert!(!session.state().is_hand_over());
        assert_eq!(session.state().to_act(), to_act);
        assert_eq!(session.hand_no(), 1);
    }

    #[test]
    fn placeholder_action_is_always_legal() {
        for seed in 0..12 {
            let mut rng = seeded_rng(500 + seed);
            let mut deck = Deck::shuffled(&mut rng);
            let mut state = GameState::new(Seat::Hero, BLIND_SCHEDULE[0]);
            state.start_hand(&mut deck).unwrap();

            let mut hands = 0;
            let mut keeps_folding_faced_with_bet = false;
            for _ in 0..20_000 {
                if state.is_hand_over() {
                    assert_eq!(state.stacks().iter().sum::<u32>(), STARTING_STACK * 3);
                    hands += 1;
                    if hands >= 8 {
                        break;
                    }
                    if deck.remaining() < MIN_DECK_FOR_HAND {
                        deck.shuffle(&mut rng);
                    }
                    state.next_hand(&mut deck).unwrap();
                    continue;
                }
                let action = placeholder_action(&mut rng, &state);
                let legal = state.legal_actions();
                assert!(
                    legal.allows(action),
                    "policy produced illegal action {action:?} in {seed}: {legal:?}"
                );
                if !legal.can_fold {
                    keeps_folding_faced_with_bet = true;
                }
                apply_settled(&mut state, &mut deck, action).unwrap();
            }
            assert_eq!(hands, 8, "self-play hands always terminate for seed {seed}");
            assert!(keeps_folding_faced_with_bet, "acts against bets too");
        }
    }

    #[test]
    fn self_play_conserves_chips_across_hands() {
        let mut rng = seeded_rng(600);
        let mut deck = Deck::shuffled(&mut rng);
        let mut state = GameState::new(Seat::Hero, BLIND_SCHEDULE[0]);
        state.start_hand(&mut deck).unwrap();
        let mut finished = 0;
        loop {
            if state.is_hand_over() {
                finished += 1;
                if finished >= 20 {
                    break;
                }
                if deck.remaining() < MIN_DECK_FOR_HAND {
                    deck.shuffle(&mut rng);
                }
                state.next_hand(&mut deck).unwrap();
                continue;
            }
            let action = placeholder_action(&mut rng, &state);
            apply_settled(&mut state, &mut deck, action).unwrap();
        }
        assert_eq!(state.stacks().iter().sum::<u32>(), 1500);
    }

    #[test]
    fn one_hero_submission_can_complete_more_than_one_opponent_hand() {
        let state = river_facing_bet();
        let mut session =
            TableSession::resume(state, Deck::default(), 1, 47, probe_config(), survival());
        let events = session.submit(Action::Call).unwrap();
        assert!(!events.is_empty());
        assert_eq!(session.hand_no(), 2);
        assert!(!session.state().is_hand_over());
    }

    #[test]
    fn action_log_is_appended_and_trimmed() {
        let mut session = TableSession::new(48, probe_config(), survival());
        session.deal_next_hand().unwrap();
        for _ in 0..40 {
            session.log_line("filler line".to_string());
        }
        assert!(session.log().len() <= MAX_LOG_LINES);
        assert_eq!(session.log()[0], "filler line");
        session.log_line("final".to_string());
        assert_eq!(session.log().len(), MAX_LOG_LINES);
        assert_eq!(session.log().last().unwrap(), "final");
    }
}
