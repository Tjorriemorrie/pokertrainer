use rand::Rng;

use crate::analytics::{PendingDecision, PendingHandResult};
use crate::blunder::{BlunderConfig, Tracker};
use crate::card::{Card, Deck};
use crate::decision::{self, AnalyzedDecision, SurvivalConfig, validate_action};
use crate::error::{Error, Result};
use crate::game::blinds::BLIND_SCHEDULE;
use crate::game::{Action, ActionOutcome, GameState, HandEndReason, Seat, Street};
use crate::mcts::MctsConfig;
use crate::opponent::{OpponentSnapshot, OpponentTracker};
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
    /// One evaluated action for the top-bar EV tracker (EV loss in big
    /// blinds); the decimated 1,000-action dataset arrives separately in
    /// chart snapshots.
    ChartTick { action_index: u64, ev_loss: f64 },
}

/// An intercepted submission held back by the blunder engine: once the player
/// confirms the review, the coach's survivability-optimal action is applied to
/// the table in place of the blunder (the blunder itself stays what the
/// session history and EV chart record).
pub struct PendingInterception {
    action: Action,
    analyzed: AnalyzedDecision,
    action_index: u64,
}

/// The outcome of a finished tournament: who won, the final stacks, and the
/// hero's hand-level aggregates for the winner/loser modal and detail page.
#[derive(Clone, Debug, PartialEq)]
pub struct TournamentResult {
    pub won: bool,
    pub winner: Seat,
    pub final_stacks: [u32; 3],
    pub hands: u64,
    pub hands_won: u64,
    pub all_ins: u64,
}

/// A live table session: one game state, a deck, the solver configuration, the
/// blunder-intervention tracker, a placeholder policy for the two opponents,
/// and a live HUD tracker fed from that policy. Each WebSocket connection owns
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
    blunder_tracker: Tracker,
    pending: Option<PendingInterception>,
    records: Vec<PendingDecision>,
    /// Per-hand results (winner, hero all-in, hero bust) queued for
    /// persistence; the tournament detail page aggregates them.
    hand_results: Vec<PendingHandResult>,
    /// Whether the hero selected the all-in action at any point this hand.
    hero_all_in_this_hand: bool,
    opponents: OpponentTracker,
    /// Sound cues accumulated since the last rendered state update.
    sounds: Vec<Sound>,
    /// Opponent actions applied by [`Self::pump`] since the last drain; the
    /// WebSocket layer uses them to reshape the background solver's tree onto
    /// the played branch.
    pump_actions: Vec<Action>,
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
            hand_results: Vec::new(),
            hero_all_in_this_hand: false,
            opponents: OpponentTracker::default(),
            sounds: Vec::new(),
            pump_actions: Vec::new(),
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
            hand_results: Vec::new(),
            hero_all_in_this_hand: false,
            opponents: OpponentTracker::default(),
            sounds: Vec::new(),
            pump_actions: Vec::new(),
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

    /// The opponent range models fed to the solver.
    pub fn ranges(&self) -> [Range; 2] {
        self.ranges
    }

    /// Drains the opponent actions the last [`Self::pump`] applied, in play
    /// order — the `Reshape` path the background solver follows.
    pub fn take_pump_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pump_actions)
    }

    /// The live HUD snapshots for both opponents, rendered inside the coach
    /// feedback panel.
    pub fn opponent_snapshots(&self) -> Vec<OpponentSnapshot> {
        self.opponents.snapshots(&self.state)
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

    /// Drains the per-hand results awaiting a database write.
    pub fn take_hand_results(&mut self) -> Vec<PendingHandResult> {
        std::mem::take(&mut self.hand_results)
    }

    /// The tournament outcome once only one seat remains, or `None` while the
    /// tournament is still running. Aggregates the recorded hand results so
    /// the winner/loser modal and the detail page can be populated.
    pub fn tournament_result(&self) -> Option<TournamentResult> {
        let winner = self.state.tournament_winner()?;
        let hands_won = self
            .hand_results
            .iter()
            .filter(|result| result.hero_won)
            .count() as u64;
        let all_ins = self
            .hand_results
            .iter()
            .filter(|result| result.hero_all_in)
            .count() as u64;
        Some(TournamentResult {
            won: winner == Seat::Hero,
            winner,
            final_stacks: self.state.stacks(),
            hands: self.hand_no,
            hands_won,
            all_ins,
        })
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

    /// The action the coach will apply to the table once the pending
    /// interception is confirmed: the survivability-optimal action replacing
    /// the held-back blunder.
    pub fn resolving_action(&self) -> Option<Action> {
        self.pending
            .as_ref()
            .map(|pending| pending.analyzed.optimal.action)
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
        self.opponents.begin_hand();
        self.hero_all_in_this_hand = false;
        self.push_sound(Sound::Deal);
        self.log_line(format!(
            "— Hand #{} — blinds {}/{}",
            self.hand_no,
            self.state.blind_level().small_blind,
            self.state.blind_level().big_blind
        ));
        tracing::info!(
            hand_no = self.hand_no,
            button = %self.state.button(),
            small_blind = self.state.blind_level().small_blind,
            big_blind = self.state.blind_level().big_blind,
            stacks = ?self.state.stacks(),
            hero_cards = %hole_cards_text(self.state.hole_cards(Seat::Hero)),
            "hand dealt"
        );
        Ok(())
    }

    /// Deals the next hand after the result pause and drives the opponents
    /// until the hero must act. Called by the WebSocket layer once the client
    /// has shown the winner for a beat.
    pub fn advance_after_result(&mut self) -> Result<()> {
        tracing::info!(
            hand_won = self.hand_no,
            "result pause over — dealing the next hand"
        );
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
            let legal = self.state.legal_actions();
            let call_amount = legal.call_amount;
            let action = placeholder_action(&mut self.rng, &self.state);
            self.opponents
                .record(actor, action, self.state.street(), legal.call_amount > 0);
            self.pump_actions.push(action);
            let outcome = apply_settled(&mut self.state, &mut self.deck, action)?;
            tracing::info!(
                hand_no = self.hand_no,
                seat = %actor,
                action = %views::action_label(action),
                outcome = ?outcome,
                street = %self.state.street(),
                pot = self.state.total_pot(),
                to_act = %self.state.to_act(),
                "opponent action applied"
            );
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
        self.finish_submission(action, analyzed)
    }

    /// Like [`Self::submit`], but scores the decision against the background
    /// solver's latest snapshot instead of running a solve — the answer is
    /// instant. Off-bucket actions (a bet-slider amount the searcher never
    /// searched) fall back to a full synchronous analyze.
    pub fn submit_with_snapshot(
        &mut self,
        action: Action,
        snapshot: &crate::mcts::SolveResult,
    ) -> Result<Vec<TableEvent>> {
        validate_action(&self.state, action)?;
        if self.pending.is_some() {
            return Err(Error::Decision(
                "a blunder interception is pending review — confirm it first".into(),
            ));
        }

        let analyzed =
            match decision::analyze_snapshot(&self.state, snapshot, &self.survival, Some(action)) {
                Ok(analyzed) => analyzed,
                Err(_) => decision::analyze(
                    &mut self.rng,
                    &self.state,
                    &self.ranges,
                    &self.mcts,
                    &self.survival,
                    Some(action),
                )?,
            };
        self.finish_submission(action, analyzed)
    }

    /// The interception-and-apply pipeline shared by both submission paths.
    fn finish_submission(
        &mut self,
        action: Action,
        analyzed: AnalyzedDecision,
    ) -> Result<Vec<TableEvent>> {
        self.action_no += 1;
        let ev_loss = analyzed
            .played
            .as_ref()
            .map(|played| played.ev_loss_bb)
            .unwrap_or(0.0);

        let intercepted = self.blunder_tracker.should_intercept(ev_loss);
        self.blunder_tracker.record_action(ev_loss);

        if intercepted {
            tracing::info!(
                ev_loss,
                action = %views::action_label(action),
                threshold = %(self.blunder_tracker.threshold()),
                hand_no = self.hand_no,
                street = %self.state.street(),
                pot = self.state.total_pot(),
                stacks = ?self.state.stacks(),
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

    /// Applies the coach's best-EV action after the review confirmation: the
    /// held-back blunder is discarded on the table (but stays recorded in the
    /// history and EV chart), opponents act, and the chart tick plus new table
    /// state are published.
    pub fn confirm_review(&mut self) -> Result<Vec<TableEvent>> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| Error::Decision("no blunder interception is pending review".into()))?;
        let optimal = pending.analyzed.optimal.action;
        let ev_loss = pending
            .analyzed
            .played
            .as_ref()
            .map(|played| played.ev_loss_bb)
            .unwrap_or(0.0);
        tracing::info!(
            played = ?pending.action,
            optimal = ?optimal,
            ev_loss,
            hand_no = self.hand_no,
            "review confirmed — the coach's best-EV action replaces the blunder on the table"
        );
        self.record_decision(&pending.analyzed, pending.action, ev_loss);
        self.apply_submission(optimal, pending.action_index, ev_loss)
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
        if action == Action::AllIn {
            self.hero_all_in_this_hand = true;
        }
        if let Some(sound) = Self::sound_for(action) {
            self.push_sound(sound);
        }
        let outcome = apply_settled(&mut self.state, &mut self.deck, action)?;
        tracing::info!(
            hand_no = self.hand_no,
            action_index,
            action = %views::action_label(action),
            ev_loss,
            outcome = ?outcome,
            street = %self.state.street(),
            pot = self.state.total_pot(),
            stacks = ?self.state.stacks(),
            to_act = %self.state.to_act(),
            "hero action applied"
        );
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
        let Some(result) = self.state.hand_result().cloned() else {
            return;
        };
        let total: u32 = result.awards.iter().map(|award| award.amount).sum();
        let hero_won = result.awards.iter().any(|award| award.seat == Seat::Hero);
        let winner = result
            .awards
            .first()
            .map(|award| award.seat)
            .unwrap_or(Seat::Hero);
        self.hand_results.push(PendingHandResult {
            hand_no: self.hand_no,
            hero_won,
            hero_all_in: self.hero_all_in_this_hand,
            hero_busted: self.state.stack(Seat::Hero) == 0,
            winner_seat: winner.index() as i32,
        });
        match result.reason {
            HandEndReason::Fold(winner) => {
                self.log_line(format!("{winner} win {total} — everyone else folded"));
                tracing::info!(
                    hand_no = self.hand_no,
                    winner = %winner,
                    pot = total,
                    stacks = ?self.state.stacks(),
                    "hand finished — {winner} wins the pot uncontested"
                );
            }
            HandEndReason::Showdown => {
                for (seat, cards, class) in &result.revealed {
                    self.log_line(format!("{seat} shows {} {} ({class})", cards[0], cards[1]));
                }
                let winners: Vec<String> = result
                    .awards
                    .iter()
                    .map(|award| format!("{} +{}", award.seat, award.amount))
                    .collect();
                self.log_line(format!("Showdown · {}", winners.join(" · ")));
                tracing::info!(
                    hand_no = self.hand_no,
                    pot = total,
                    awards = ?result.awards,
                    stacks = ?self.state.stacks(),
                    "hand finished — showdown awarded"
                );
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

/// Formats optional hole cards for log lines: `"As Kh"` or `"—"`.
fn hole_cards_text(cards: Option<[Card; 2]>) -> String {
    match cards {
        Some([first, second]) => format!("{first} {second}"),
        None => "—".to_string(),
    }
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
    /// fires, the state is untouched, and `REVIEW_DONE` releases the table —
    /// playing the coach's best-EV action while the blunder stays recorded.
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

        let call_amount = session.state().legal_actions().call_amount;
        let events = session.confirm_review().unwrap();
        assert_eq!(
            session.resolving_action(),
            None,
            "no action remains parked after confirmation"
        );
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
            session
                .log()
                .iter()
                .any(|line| *line == views::describe_action(Seat::Hero, optimal, call_amount)),
            "the coach's best-EV action is the one applied to the table: {:?}",
            session.log()
        );
        assert!(
            session
                .log()
                .iter()
                .all(|line| *line != views::describe_action(Seat::Hero, alternative, call_amount)),
            "the blunder itself must never reach the table: {:?}",
            session.log()
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
                    if hands >= 8 || state.tournament_winner().is_some() {
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
            assert!(
                hands >= 1,
                "self-play hands always terminate for seed {seed}"
            );
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
                if finished >= 20 || state.tournament_winner().is_some() {
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
    fn submit_with_snapshot_answers_instantly_and_records_like_submit() {
        let state = river_facing_bet();
        let snapshot = mcts::solve(
            &mut seeded_rng(50),
            &state,
            &[uniform(), uniform()],
            &probe_config(),
        )
        .unwrap();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            51,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        let events = session
            .submit_with_snapshot(Action::Fold, &snapshot)
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            TableEvent::ChartTick {
                action_index: 1,
                ..
            }
        )));
        assert!(events.contains(&TableEvent::State));
        let records = session.take_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].played, "Fold");
    }

    /// An off-bucket slider amount is not in the snapshot: the submission
    /// falls back to a full inline solve so the played action still gets an
    /// exact evaluation.
    #[test]
    fn submit_with_snapshot_falls_back_for_off_bucket_amounts() {
        let mut state = river_facing_bet();
        state.set_stack(Seat::Hero, 400);
        let snapshot = mcts::solve(
            &mut seeded_rng(52),
            &state,
            &[uniform(), uniform()],
            &probe_config(),
        )
        .unwrap();
        let played = Action::Raise(250);
        assert!(
            !snapshot.actions.iter().any(|value| value.action == played),
            "fixture should use an off-bucket amount"
        );
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            53,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        let events = session.submit_with_snapshot(played, &snapshot).unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                TableEvent::ChartTick {
                    action_index: 1,
                    ..
                }
            )),
            "the played slider amount is evaluated and charted: {events:?}"
        );
        assert!(events.contains(&TableEvent::State));
        let records = session.take_records();
        assert_eq!(records[0].played, views::action_label(played));
    }

    /// The opponent actions applied between two hero decisions are recorded
    /// in play order and drained once the WebSocket layer consumes them.
    #[test]
    fn pump_actions_accumulate_in_order_and_drain() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            54,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        assert!(session.take_pump_actions().is_empty());
        session.submit(Action::Fold).unwrap();
        let actions = session.take_pump_actions();
        assert!(
            actions.iter().all(|action| matches!(
                action,
                Action::Fold
                    | Action::Check
                    | Action::Call
                    | Action::Bet(_)
                    | Action::Raise(_)
                    | Action::AllIn
            )),
            "recorded actions are real opponent moves: {actions:?}"
        );
        assert!(
            !actions.is_empty() || session.state().is_hand_over(),
            "opponents finished the hand after the fold"
        );
        assert!(
            session.take_pump_actions().is_empty(),
            "draining empties the buffer"
        );
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

    /// At showdown every revealed hand is logged, followed by the winner's
    /// payout, so the action log tells the whole story of the hand.
    #[test]
    fn showdown_logs_revealed_cards_and_winner_amount() {
        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            50,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        session.submit(Action::Call).unwrap();
        let log = session.log();
        assert!(
            log.iter().any(|line| line.contains("shows")),
            "revealed cards are logged at showdown: {log:?}"
        );
        assert!(
            log.iter().any(|line| line.starts_with("Showdown ·")),
            "the winner's payout is logged at showdown: {log:?}"
        );
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

    /// A finished hand queues a per-hand result recording the winner and the
    /// hero's all-in/bust status, and it drains like the decision records.
    #[test]
    fn a_finished_hand_queues_a_hand_result() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            60,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        assert!(session.take_hand_results().is_empty());
        session.submit(Action::Fold).unwrap();
        assert!(session.state().is_hand_over());

        let results = session.take_hand_results();
        assert_eq!(results.len(), 1, "one hand result per finished hand");
        let result = &results[0];
        assert_eq!(result.hand_no, 1);
        assert!(!result.hero_won, "the hero folded this hand");
        assert!(!result.hero_all_in, "the hero did not go all-in");
        assert!(!result.hero_busted, "the hero still has chips");
        assert!(
            result.winner_seat == Seat::Opponent1.index() as i32
                || result.winner_seat == Seat::Opponent2.index() as i32,
            "an opponent won the hand: {result:?}"
        );
        assert!(
            session.take_hand_results().is_empty(),
            "draining empties the hand-result queue"
        );
    }

    /// The tournament result is `None` while multiple seats remain and is
    /// populated once a single seat is left standing.
    #[test]
    fn tournament_result_is_none_until_one_seat_remains() {
        let state = river_facing_bet();
        let session = TableSession::resume(
            state,
            Deck::default(),
            1,
            61,
            probe_config(),
            survival(),
            never_intercepts(),
        );
        assert_eq!(session.tournament_result(), None);
    }
}
