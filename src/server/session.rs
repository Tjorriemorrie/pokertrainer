use rand::Rng;

use crate::analytics::PendingDecision;
use crate::blunder::{BlunderConfig, Tracker};
use crate::card::Deck;
use crate::decision::{self, AnalyzedDecision, SurvivalConfig, validate_action};
use crate::error::{Error, Result};
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

/// A short sound cue attached to a state update; the client synthesizes it
/// with WebAudio — no audio files are shipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    /// Cards were dealt: a new hand begins (or the board grew).
    Deal,
    /// Chips were committed (blind, bet, call, raise, all-in).
    Chip,
    /// A player folded.
    Fold,
    /// A hand ended and chips were awarded.
    Win,
}

impl Sound {
    /// The wire tag rendered into the fragment's `data-sounds` attribute.
    pub fn tag(self) -> &'static str {
        match self {
            Sound::Deal => "deal",
            Sound::Chip => "chip",
            Sound::Fold => "fold",
            Sound::Win => "win",
        }
    }
}

/// Events produced by one session step; the WebSocket layer maps them onto
/// [`super::protocol::ServerMessage`]s.
#[derive(Clone, Debug, PartialEq)]
pub enum TableEvent {
    /// The table state changed: render and send a [`TABLE_STATE_UPDATE`]
    /// fragment. Sound cues accumulated since the previous update are drained
    /// by the WebSocket layer when the fragment is rendered.
    ///
    /// [`TABLE_STATE_UPDATE`]: super::protocol::ServerMessage::TableStateUpdate
    State,
    /// The played action was a calibrated blunder: overlay a full tactical
    /// breakdown and freeze the table until the review is confirmed.
    /// Intercepted decisions are held back — the game state is only advanced
    /// by [`TableSession::confirm_review`].
    TacticalOverlay {
        decision: AnalyzedDecision,
        hand_no: u64,
        /// Whether the state transition was halted (the client must send
        /// `REVIEW_DONE` to advance).
        intercepted: bool,
    },
    /// One evaluated action for the top-bar EV tracker; the decimated
    /// 1,000-action dataset arrives separately in chart snapshots.
    ChartTick { action_index: u64, ev_loss: f64 },
}

/// An intercepted submission held back by the blunder engine: the action is
/// replayed once the player confirms the review.
pub struct PendingInterception {
    action: Action,
    analyzed: AnalyzedDecision,
    action_index: u64,
}

/// A live table session: one game state, a deck, the solver configuration, the
/// blunder-intervention tracker, and a placeholder policy for the two
/// opponents. Each WebSocket connection owns one session.
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
    blunder_tracker: Tracker,
    pending: Option<PendingInterception>,
    records: Vec<PendingDecision>,
    /// Sound cues accumulated since the last rendered state update.
    sounds: Vec<Sound>,
}

impl TableSession {
    /// A fresh session at the first blind level with a shuffled deck.
    pub fn new(
        seed: u64,
        mcts: MctsConfig,
        survival: SurvivalConfig,
        blunder: BlunderConfig,
    ) -> Self {
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
            blunder_tracker: Tracker::new(blunder),
            pending: None,
            records: Vec::new(),
            sounds: Vec::new(),
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
        blunder: BlunderConfig,
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
            blunder_tracker: Tracker::new(blunder),
            pending: None,
            records: Vec::new(),
            sounds: Vec::new(),
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

    /// Queues one evaluated hero decision for persistence; the session
    /// stays database-free and the ownership of the write is the WebSocket
    /// layer's.
    fn record_decision(&mut self, analyzed: &AnalyzedDecision, played: Action, ev_loss: f64) {
        self.records.push(PendingDecision {
            hand_no: self.hand_no,
            street: self.state.street(),
            played: views::action_label(played),
            optimal: views::action_label(analyzed.optimal.action),
            ev_loss,
        });
    }

    /// Drains the decisions awaiting a database write.
    pub fn take_records(&mut self) -> Vec<PendingDecision> {
        std::mem::take(&mut self.records)
    }

    /// Drains the sound cues accumulated since the last rendered state
    /// update; the WebSocket layer attaches them to the fragment.
    pub fn take_sounds(&mut self) -> Vec<Sound> {
        std::mem::take(&mut self.sounds)
    }

    fn push_sound(&mut self, sound: Sound) {
        if self.sounds.len() < MAX_LOG_LINES {
            self.sounds.push(sound);
        }
    }

    /// The sound cue for an applied action: folds swish, checks are silent,
    /// everything that commits chips clacks.
    fn sound_for(action: Action) -> Option<Sound> {
        match action {
            Action::Fold => Some(Sound::Fold),
            Action::Check => None,
            Action::Call | Action::Bet(_) | Action::Raise(_) | Action::AllIn => Some(Sound::Chip),
        }
    }

    /// Test hook: seeds the blunder tracker's rolling EV-loss history.
    #[cfg(test)]
    fn prime_blunder_history(&mut self, ev_losses: &[f64]) {
        for &loss in ev_losses {
            self.blunder_tracker.record_action(loss);
        }
    }

    /// Test hook: parks an interception as if the dynamic threshold had
    /// fired, so callers can exercise the review-confirmation path directly.
    #[cfg(test)]
    pub(crate) fn stage_pending_interception(
        &mut self,
        action: Action,
        analyzed: AnalyzedDecision,
    ) {
        self.action_no += 1;
        self.pending = Some(PendingInterception {
            action,
            analyzed,
            action_index: self.action_no,
        });
    }

    /// Whether a blunder interception is currently awaiting review.
    pub fn has_pending_interception(&self) -> bool {
        self.pending.is_some()
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
            self.blunder_tracker.end_hand();
            self.state.next_hand(&mut self.deck)?;
        }
        self.hand_no += 1;
        self.push_sound(Sound::Deal);
        self.log_line(format!(
            "— Hand #{} — blinds {}/{}",
            self.hand_no,
            self.state.blind_level().small_blind,
            self.state.blind_level().big_blind
        ));
        Ok(())
    }

    /// Deals the next hand after the result pause and drives the opponents
    /// until the hero must act. Called by the WebSocket layer once the client
    /// has shown the winner for a beat.
    pub fn advance_after_result(&mut self) -> Result<()> {
        self.deal_next_hand()?;
        self.pump()?;
        Ok(())
    }

    /// Drives the opponents until a decision or the end of the hand is
    /// reached. When the hand ends the result is logged and the win sound
    /// queued, but the next deal is deferred to [`Self::advance_after_result`]
    /// so the winner stays on screen for a beat. Returns whether anything
    /// happened.
    pub fn pump(&mut self) -> Result<bool> {
        let mut acted = false;
        if self.state.is_hand_over() {
            return Ok(acted);
        }
        loop {
            if self.state.to_act() == Seat::Hero {
                return Ok(acted);
            }
            let actor = self.state.to_act();
            let call_amount = self.state.legal_actions().call_amount;
            let action = placeholder_action(&mut self.rng, &self.state);
            apply_settled(&mut self.state, &mut self.deck, action)?;
            if let Some(sound) = Self::sound_for(action) {
                self.push_sound(sound);
            }
            self.log_line(views::describe_action(actor, action, call_amount));
            acted = true;
            if self.state.is_hand_over() {
                self.log_hand_result();
                return Ok(acted);
            }
        }
    }

    /// Validates, analyzes, and applies a hero action, returning the events to
    /// publish. The solve runs synchronously in the calling task (the local,
    /// single-user WebSocket connection).
    ///
    /// When the played action's EV loss clears the calibrated dynamic
    /// threshold the state transition is halted — the action is parked in
    /// [`PendingInterception`] and only replayed by [`Self::confirm_review`].
    pub fn submit(&mut self, action: Action) -> Result<Vec<TableEvent>> {
        validate_action(&self.state, action)?;
        if self.pending.is_some() {
            return Err(Error::Decision(
                "a blunder interception is pending review — confirm it first".into(),
            ));
        }

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

        let intercepted = self
            .blunder_tracker
            .should_intercept(ev_loss, self.state.blind_level().big_blind);
        self.blunder_tracker.record_action(ev_loss);

        if intercepted {
            tracing::info!(
                ev_loss,
                threshold = %(self.blunder_tracker.threshold(
                    self.state.blind_level().big_blind
                )),
                hand_no = self.hand_no,
                "blunder intercepted — freezing the state transition"
            );
            self.pending = Some(PendingInterception {
                action,
                analyzed: analyzed.clone(),
                action_index: self.action_no,
            });
            return Ok(vec![TableEvent::TacticalOverlay {
                decision: analyzed,
                hand_no: self.hand_no,
                intercepted: true,
            }]);
        }

        self.record_decision(&analyzed, action, ev_loss);
        self.apply_submission(action, self.action_no, ev_loss)
    }

    /// Applies the intercepted action after the review confirmation: replays
    /// the held-back submission, lets opponents act, and publishes the chart
    /// tick and new table state.
    pub fn confirm_review(&mut self) -> Result<Vec<TableEvent>> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| Error::Decision("no blunder interception is pending review".into()))?;
        let ev_loss = pending
            .analyzed
            .played
            .as_ref()
            .map(|played| played.ev_loss)
            .unwrap_or(0.0);
        tracing::info!(
            action = ?pending.action,
            hand_no = self.hand_no,
            "review confirmed — applying the intercepted action"
        );
        self.record_decision(&pending.analyzed, pending.action, ev_loss);
        self.apply_submission(pending.action, pending.action_index, ev_loss)
    }

    /// Applies one validated hero action and publishes its events: a chart
    /// tick plus the refreshed table state.
    fn apply_submission(
        &mut self,
        action: Action,
        action_index: u64,
        ev_loss: f64,
    ) -> Result<Vec<TableEvent>> {
        let call_amount = self.state.legal_actions().call_amount;
        self.log_line(views::describe_action(Seat::Hero, action, call_amount));
        if let Some(sound) = Self::sound_for(action) {
            self.push_sound(sound);
        }
        apply_settled(&mut self.state, &mut self.deck, action)?;
        if self.state.is_hand_over() {
            self.log_hand_result();
        }
        self.pump()?;

        Ok(vec![
            TableEvent::ChartTick {
                action_index,
                ev_loss,
            },
            TableEvent::State,
        ])
    }

    fn log_line(&mut self, line: String) {
        while self.log.len() >= MAX_LOG_LINES {
            self.log.remove(0);
        }
        self.log.push(line);
    }

    fn log_hand_result(&mut self) {
        self.push_sound(Sound::Win);
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

/// Placeholder policy for the opponents: checks any free option, otherwise
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
    use crate::blunder::BlunderConfig;
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

    /// Test preset: the warm-up floor is unreachable, so nothing ever
    /// intercepts — suboptimal actions apply immediately without feedback.
    fn never_intercepts() -> BlunderConfig {
        BlunderConfig {
            fallback_bb: f64::MAX,
            ..BlunderConfig::default()
        }
    }

    /// Test preset: a zero-chip floor means every non-optimal decision
    /// intercepts (a one-entry history is enough to leave the empty state).
    fn always_intercepts() -> BlunderConfig {
        BlunderConfig {
            fallback_bb: 0.0,
            ..BlunderConfig::default()
        }
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
        let mut session = TableSession::new(41, probe_config(), survival(), never_intercepts());
        session.deal_next_hand().unwrap();
        session.pump().unwrap();
        assert_eq!(session.state().to_act(), Seat::Hero);
        assert_eq!(session.hand_no(), 1);
        assert!(!session.state().is_hand_over());
    }

    #[test]
    fn pump_stops_immediately_when_it_is_already_the_heros_turn() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            3,
            42,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        assert!(!session.pump().unwrap());
        assert_eq!(session.state().to_act(), Seat::Hero);
        assert_eq!(session.hand_no(), 3);
    }

    #[test]
    fn submit_emits_a_chart_tick_and_a_state_update() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            43,
            probe_config(),
            survival(),
            never_intercepts(),
        );
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
            1,
            "the hand-over state stays visible until the result pause ends"
        );
        assert!(
            session.state().is_hand_over(),
            "folding here ends the hand; the deal waits for the result pause"
        );
        session.advance_after_result().unwrap();
        assert_eq!(session.hand_no(), 2);
        assert!(!session.state().is_hand_over());
        assert_eq!(session.state().to_act(), Seat::Hero);
    }

    /// Every applied hero submission queues a database record carrying the
    /// hand, street, played/optimal labels, and the EV lost.
    #[test]
    fn every_applied_submission_queues_a_decision_record() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            43,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        session.submit(Action::Fold).unwrap();

        let records = session.take_records();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.hand_no, 1);
        assert_eq!(record.street, Street::River);
        assert_eq!(record.played, "Fold");
        assert!(
            !record.optimal.is_empty(),
            "the optimal action is stored too"
        );
        assert!(record.ev_loss >= 0.0);
        assert!(
            session.take_records().is_empty(),
            "draining empties the queue"
        );
    }

    /// Below the dynamic threshold there is no feedback at all, and the
    /// action applies without any overlay.
    #[test]
    fn suboptimal_plays_below_the_threshold_apply_without_feedback() {
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
            never_intercepts(),
        );
        let events = session.submit(alternative).unwrap();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TableEvent::TacticalOverlay { .. })),
            "below the dynamic threshold no overlay is shown"
        );
        assert!(
            events.iter().any(
                |event| matches!(event, TableEvent::ChartTick { ev_loss, .. } if *ev_loss > 0.0)
            )
        );
        assert!(events.contains(&TableEvent::State));
        assert!(!session.has_pending_interception());
    }

    /// Above the dynamic threshold the action is held back: only the overlay
    /// fires, the state is untouched, and `REVIEW_DONE` replays it.
    #[test]
    fn blunders_above_the_threshold_intercept_and_await_review() {
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
            always_intercepts(),
        );
        session.prime_blunder_history(&[5.0]);

        let events = session.submit(alternative).unwrap();
        assert_eq!(
            events.len(),
            1,
            "an interception publishes only the overlay"
        );
        assert!(
            matches!(
                &events[0],
                TableEvent::TacticalOverlay {
                    intercepted: true,
                    hand_no: 1,
                    ..
                }
            ),
            "the overlay must be flagged as intercepted: {events:?}"
        );
        assert!(session.has_pending_interception());
        assert_eq!(session.state().to_act(), Seat::Hero, "the game is frozen");
        assert_eq!(session.hand_no(), 1, "still the same hand");
        assert!(
            session.take_records().is_empty(),
            "nothing is recorded while the interception awaits review"
        );

        let stuck = session.submit(Action::Call);
        assert!(
            matches!(stuck, Err(Error::Decision(_))),
            "submissions are blocked while a review is pending"
        );

        let events = session.confirm_review().unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                TableEvent::ChartTick {
                    action_index: 1,
                    ev_loss,
                } if *ev_loss > 0.0
            )),
            "the chart tick is published on confirmation"
        );
        assert!(events.contains(&TableEvent::State));
        assert!(!session.has_pending_interception());
        let records = session.take_records();
        assert_eq!(
            records.len(),
            1,
            "intercepted actions are recorded on confirmation"
        );
        assert_eq!(records[0].played, views::action_label(alternative));
        assert_eq!(
            records[0].optimal,
            views::action_label(probed.optimal.action)
        );
        assert_eq!(
            records[0].street,
            Street::River,
            "the frozen street is stored"
        );
        assert!(
            session.log().iter().any(|line| line.starts_with("You ")),
            "the intercepted action is logged when applied"
        );

        assert!(
            matches!(session.confirm_review(), Err(Error::Decision(_))),
            "a second confirmation has nothing to replay"
        );
    }

    /// Even with a zero-chip threshold, an optimal play never intercepts.
    #[test]
    fn optimal_plays_never_intercept() {
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

        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            44,
            probe_config(),
            survival(),
            always_intercepts(),
        );
        session.prime_blunder_history(&[5.0]);
        let events = session.submit(probed.optimal.action).unwrap();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TableEvent::TacticalOverlay { .. })),
            "an optimal play must not trigger the tactical overlay"
        );
        assert!(events.iter().any(
            |event| matches!(event, TableEvent::ChartTick { ev_loss, .. } if *ev_loss == 0.0)
        ));
        assert!(!session.has_pending_interception());
    }

    #[test]
    fn submit_rejects_illegal_actions_without_touching_the_state() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            45,
            probe_config(),
            survival(),
            never_intercepts(),
        );
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
    fn one_submission_runs_opponents_to_the_hand_over_state_then_advances() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            47,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        let events = session.submit(Action::Call).unwrap();
        assert!(!events.is_empty());
        assert_eq!(session.hand_no(), 1, "the result stays on screen first");
        assert!(
            session.state().is_hand_over(),
            "calling here ends the hand at showdown"
        );
        session.advance_after_result().unwrap();
        assert_eq!(session.hand_no(), 2);
        assert!(!session.state().is_hand_over());
        assert_eq!(session.state().to_act(), Seat::Hero);
    }

    #[test]
    fn action_log_is_appended_and_trimmed() {
        let mut session = TableSession::new(48, probe_config(), survival(), never_intercepts());
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

    /// Sound cues accumulate with the actions they describe and are
    /// drained by the WebSocket layer when the state fragment is rendered.
    /// The deal sound waits for the result pause: the win shows first.
    #[test]
    fn state_updates_accumulate_and_drain_sound_cues() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            49,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        session.submit(Action::Fold).unwrap();

        let sounds = session.take_sounds();
        assert!(
            sounds.contains(&Sound::Fold),
            "the hero's fold cues a fold sound: {sounds:?}"
        );
        assert!(
            sounds.contains(&Sound::Win),
            "ending the hand cues the win sound: {sounds:?}"
        );
        assert!(
            !sounds.contains(&Sound::Deal),
            "the deal sound waits for the result pause: {sounds:?}"
        );

        session.advance_after_result().unwrap();
        assert!(
            session.take_sounds().contains(&Sound::Deal),
            "the freshly dealt hand cues a deal sound"
        );
        assert!(
            session.take_sounds().is_empty(),
            "draining empties the cue buffer"
        );
    }

    /// Checks stay silent — no chip sound without committing chips.
    #[test]
    fn sound_for_maps_actions_to_cues() {
        assert_eq!(TableSession::sound_for(Action::Fold), Some(Sound::Fold));
        assert_eq!(TableSession::sound_for(Action::Check), None);
        assert_eq!(TableSession::sound_for(Action::Call), Some(Sound::Chip));
        assert_eq!(TableSession::sound_for(Action::Bet(100)), Some(Sound::Chip));
        assert_eq!(
            TableSession::sound_for(Action::Raise(200)),
            Some(Sound::Chip)
        );
        assert_eq!(TableSession::sound_for(Action::AllIn), Some(Sound::Chip));
    }
}
