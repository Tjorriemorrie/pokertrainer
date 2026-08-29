use crate::analytics::{PendingDecision, PendingHandResult};
use crate::blunder::{BlunderConfig, Tracker};
use crate::card::{Card, Deck};
use crate::decision::{self, AnalyzedDecision, validate_action};
use crate::error::{Error, Result};
use crate::game::blinds::BLIND_SCHEDULE;
use crate::game::{Action, ActionOutcome, GameState, HandEndReason, Seat, Street};
use crate::mcts::MctsConfig;
use crate::opponent::{
    MergedOpponentSnapshot, OpponentTemplate, OpponentTracker, placeholder_action, template_action,
};
use crate::opponent_history::{
    ActionCategory, OpponentModel, decision_node, decision_position, decision_stack_bucket,
};
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
        decision: Box<AnalyzedDecision>,
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
/// confirms the review, the coach's highest-EV action is applied to
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
    /// The field skill template the two bots play with; `None` falls back to
    /// the placeholder heuristic.
    template: Option<OpponentTemplate>,
    /// The opponent's historic model, loaded once at session start: the
    /// per-node range priors that let the bots' own MCTS solve reason about
    /// the opponent's real tendencies instead of assuming a uniform range,
    /// plus the historic read and starting-hand table rendered in the coach
    /// panel. Defaults to empty (uniform ranges, no history) when nothing
    /// has been gathered yet.
    opponent_model: OpponentModel,
    hand_no: u64,
    action_no: u64,
    log: Vec<String>,
    rng: SeededRng,
    blunder_tracker: Tracker,
    pending: Option<PendingInterception>,
    /// Set just before submitting a check-fold's `Check`, consumed exactly
    /// once (in [`Self::apply_submission`]) when that action is actually
    /// applied to the table.
    check_fold_requested: bool,
    /// The street on which a check-fold Check was applied — armed until the
    /// hero's next decision on that street, when [`Self::pump`] auto-folds if
    /// an opponent has raised, or expires unused if the street changes first.
    pending_check_fold: Option<Street>,
    records: Vec<PendingDecision>,
    /// Local bot decisions (with their true dealt cards) queued for
    /// persistence into `local_opponent_actions` — the fallback/fill source
    /// for the opponent history window; see [`crate::opponent_history`].
    local_actions: Vec<crate::db::LocalOpponentAction>,
    /// The hero's own local decisions (with their true dealt cards) queued
    /// for persistence into `local_hero_actions` — mirrors `local_actions`,
    /// but fills out the hero's own starting-hand window.
    local_hero_actions: Vec<crate::db::LocalHeroAction>,
    /// Per-hand results (winner, hero all-in, hero bust) queued for
    /// persistence; the tournament detail page aggregates them.
    hand_results: Vec<PendingHandResult>,
    /// Whether the hero selected the all-in action at any point this hand.
    hero_all_in_this_hand: bool,
    opponents: OpponentTracker,
    /// The last seat to bet/raise/all-in preflop this hand (across every
    /// seat, hero included) — the hand's c-bet context. `None` once a new
    /// hand starts, until someone raises. See [`Self::settle_action`], the
    /// single choke point every seat's action passes through, for where
    /// this is maintained.
    preflop_aggressor: Option<Seat>,
    /// The last seat to bet/raise/all-in on the flop this hand — lets a
    /// later flop actor be told whether the bet they're facing came from
    /// the preflop aggressor (a c-bet) or someone else.
    flop_bettor: Option<Seat>,
    /// Sound cues accumulated since the last rendered state update.
    sounds: Vec<Sound>,
    /// Opponent actions applied by [`Self::pump`] since the last drain; the
    /// WebSocket layer uses them to reshape the background solver's tree onto
    /// the played branch.
    pump_actions: Vec<Action>,
}

impl TableSession {
    /// A fresh session at the first blind level with a shuffled deck and the
    /// given starting stack (the drill's resolved chip count, sampled from
    /// the hero's tournament history).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seed: u64,
        mcts: MctsConfig,
        blunder: BlunderConfig,
        template: Option<OpponentTemplate>,
        starting_stack: u32,
        opponent_model: OpponentModel,
    ) -> Self {
        let mut rng = crate::rng::seeded_rng(seed);
        let deck = Deck::shuffled(&mut rng);
        let mut state = GameState::new(Seat::Hero, BLIND_SCHEDULE[0]);
        for seat in Seat::ALL {
            state.set_stack(seat, starting_stack);
        }
        Self {
            state,
            deck,
            mcts,
            template,
            opponent_model,
            hand_no: 0,
            action_no: 0,
            log: Vec::new(),
            rng,
            blunder_tracker: Tracker::new(blunder),
            pending: None,
            check_fold_requested: false,
            pending_check_fold: None,
            records: Vec::new(),
            local_actions: Vec::new(),
            local_hero_actions: Vec::new(),
            hand_results: Vec::new(),
            hero_all_in_this_hand: false,
            opponents: OpponentTracker::default(),
            preflop_aggressor: None,
            flop_bettor: None,
            sounds: Vec::new(),
            pump_actions: Vec::new(),
        }
    }

    /// Continues a session from an already-dealt state (used by tests).
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        state: GameState,
        deck: Deck,
        hand_no: u64,
        seed: u64,
        mcts: MctsConfig,
        blunder: BlunderConfig,
        template: Option<OpponentTemplate>,
    ) -> Self {
        Self {
            state,
            deck,
            mcts,
            template,
            opponent_model: OpponentModel::default(),
            hand_no,
            action_no: 0,
            log: Vec::new(),
            rng: crate::rng::seeded_rng(seed),
            blunder_tracker: Tracker::new(blunder),
            pending: None,
            check_fold_requested: false,
            pending_check_fold: None,
            records: Vec::new(),
            local_actions: Vec::new(),
            local_hero_actions: Vec::new(),
            hand_results: Vec::new(),
            hero_all_in_this_hand: false,
            opponents: OpponentTracker::default(),
            preflop_aggressor: None,
            flop_bettor: None,
            sounds: Vec::new(),
            pump_actions: Vec::new(),
        }
    }

    /// Rebuilds a session from a persisted tournament snapshot: the exact
    /// game state (street, bets, board, stacks), the remaining deck in deal
    /// order, the counters, the action log, and the opponent HUD counters.
    #[allow(clippy::too_many_arguments)]
    pub fn from_snapshot(
        snapshot: &crate::snapshot::TournamentSnapshot,
        seed: u64,
        mcts: MctsConfig,
        blunder: BlunderConfig,
        opponent_model: OpponentModel,
    ) -> Result<Self> {
        let state = GameState::from_snapshot(&snapshot.state)?;
        let deck_cards = snapshot
            .deck
            .iter()
            .map(|code| {
                Card::from_code(code)
                    .ok_or_else(|| Error::Game(format!("invalid deck card {code:?}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let deck = Deck::try_from_remaining(deck_cards)
            .ok_or_else(|| Error::Game("persisted deck holds more than 52 cards".to_string()))?;
        let mut log = snapshot.log.clone();
        while log.len() > MAX_LOG_LINES {
            log.remove(0);
        }
        Ok(Self {
            state,
            deck,
            mcts,
            template: snapshot.template_skill.map(OpponentTemplate::new),
            opponent_model,
            hand_no: snapshot.hand_no,
            action_no: snapshot.action_no,
            log,
            rng: crate::rng::seeded_rng(seed),
            blunder_tracker: Tracker::new(blunder),
            pending: None,
            check_fold_requested: false,
            pending_check_fold: None,
            records: Vec::new(),
            local_actions: Vec::new(),
            local_hero_actions: Vec::new(),
            hand_results: Vec::new(),
            hero_all_in_this_hand: false,
            opponents: OpponentTracker::from_snapshot(&snapshot.opponents),
            // Not persisted: a resumed mid-hand table starts with no known
            // c-bet context for the remainder of that one hand, the same
            // kind of one-time approximation `TournamentSnapshot` already
            // accepts elsewhere (see e.g. `local_action_hand_no`'s history).
            preflop_aggressor: None,
            flop_bettor: None,
            sounds: Vec::new(),
            pump_actions: Vec::new(),
        })
    }

    /// Replays stored decisions through the blunder tracker so a new game
    /// starts already calibrated to the hero's history and a resumed table
    /// keeps its history.
    pub fn hydrate_blunder(&mut self, history: &[(i32, i64, f64)]) {
        self.blunder_tracker.hydrate(history);
    }

    /// Serializes the complete resumable table: state, deck order, counters,
    /// log, and HUD counters.
    pub fn to_snapshot(&self) -> crate::snapshot::TournamentSnapshot {
        crate::snapshot::TournamentSnapshot {
            state: self.state.to_snapshot(),
            deck: self
                .deck
                .remaining_in_order()
                .iter()
                .map(|card| card.to_code())
                .collect(),
            hand_no: self.hand_no,
            action_no: self.action_no,
            log: self.log.clone(),
            opponents: self.opponents.to_snapshot(),
            template_skill: self.template.map(|template| template.skill),
        }
    }

    pub fn state(&self) -> &GameState {
        &self.state
    }

    pub fn hand_no(&self) -> u64 {
        self.hand_no
    }

    /// The number of hero submissions applied (or intercepted) so far this
    /// session — part of the decision token.
    pub fn action_no(&self) -> u64 {
        self.action_no
    }

    /// The identity of the decision currently on screen: hand, hero actions
    /// applied, and street. It is `Some` only when the hero is the one to act
    /// in a live hand — exactly the states that render the action dock — and
    /// changes whenever the decision does, so the client can tell fresh
    /// solver statuses apart from stale ones queued behind a reshaped search.
    pub fn decision_token(&self) -> Option<String> {
        if self.state.is_hand_over() || self.state.to_act() != Seat::Hero {
            return None;
        }
        Some(format!(
            "h{}-a{}-{}",
            self.hand_no,
            self.action_no,
            self.state.street().to_string().to_lowercase()
        ))
    }

    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// The opponent range priors fed to the solver for the decision the hero
    /// currently faces, one per opponent seat — see [`Self::range_for_seat`].
    pub fn ranges(&self) -> [Range; 2] {
        [
            self.range_for_seat(Seat::Opponent1),
            self.range_for_seat(Seat::Opponent2),
        ]
    }

    /// The opponent range prior for one seat: the learned per-node/stack-
    /// bucket range (the same model the bots' own play already draws from)
    /// when the sample is trusted, falling back to a uniform "any two
    /// cards" prior otherwise.
    ///
    /// A seat that has already voluntarily raised or shoved preflop this
    /// hand is self-selected into a narrower range than the pooled
    /// per-node prior shows (that prior mixes in every fold and call seen
    /// at the same node too) — such a seat resolves against
    /// [`crate::opponent_history::OpponentRangeModel::resolve_preflop_raiser`]
    /// instead. A seat that hasn't acted yet resolves against hero's own
    /// current node, same as before this seat split — both seats are
    /// otherwise modeled as one population.
    fn range_for_seat(&self, seat: Seat) -> Range {
        let bucket = decision_stack_bucket(&self.state);
        if self.state.street() == Street::Preflop && self.preflop_aggressor == Some(seat)
            && let Some(raiser_range) = self.opponent_model.ranges.resolve_preflop_raiser(bucket)
        {
            return raiser_range;
        }
        let node = decision_node(&self.state);
        self.opponent_model
            .ranges
            .resolve(node, bucket)
            .unwrap_or_else(|| uniform_ranges()[0])
    }

    /// Drains the opponent actions the last [`Self::pump`] applied, in play
    /// order — the `Reshape` path the background solver follows.
    pub fn take_pump_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pump_actions)
    }

    /// The two bot seats' session-so-far stats merged into one read — both
    /// seats are the same modeled opponent, so the coach panel shows a
    /// single card instead of two.
    pub fn merged_opponent_snapshot(&self) -> MergedOpponentSnapshot {
        self.opponents.merged_snapshot()
    }

    /// The opponent's historic read and starting-hand table, loaded once at
    /// session start — rendered in the coach panel alongside the live
    /// session-so-far read.
    pub fn opponent_history(&self) -> &crate::opponent_history::HistorySummary {
        &self.opponent_model.historic
    }

    /// The hero's own historic read and starting-hand table, loaded once at
    /// session start — mirrors [`Self::opponent_history`].
    pub fn hero_history(&self) -> &crate::opponent_history::HistorySummary {
        &self.opponent_model.hero_historic
    }

    /// Queues one evaluated hero decision for persistence; the session
    /// stays database-free and the ownership of the write is the WebSocket
    /// layer's. Also queues the same decision into `local_hero_actions`
    /// (node/bucket/true dealt cards, coarse action) — the fallback/fill
    /// source for the hero's own starting-hand window, mirroring the bot
    /// recording in [`Self::pump`].
    fn record_decision(
        &mut self,
        analyzed: &AnalyzedDecision,
        played: Action,
        ev_loss: f64,
        ev_loss_pot: f64,
    ) {
        self.records.push(PendingDecision {
            hand_no: self.hand_no,
            street: self.state.street(),
            played: views::action_label(played),
            optimal: views::action_label(analyzed.optimal.action),
            ev_loss,
            ev_loss_pot,
        });
        let node = decision_node(&self.state);
        let stack_bucket = decision_stack_bucket(&self.state);
        let position = decision_position(&self.state);
        let hole_cards = self.state.hero_cards();
        self.local_hero_actions.push(crate::db::LocalHeroAction {
            node: node.key().to_string(),
            stack_bucket: stack_bucket.as_i16(),
            hole_cards: format!("{} {}", hole_cards[0].to_code(), hole_cards[1].to_code()),
            action: ActionCategory::of(played).label().to_string(),
            hand_no: self.hand_no as i64,
            position: position.key().to_string(),
            was_preflop_aggressor: self.was_preflop_aggressor(Seat::Hero),
            facing_cbet: self.facing_cbet(),
        });
    }

    /// Drains the decisions awaiting a database write.
    pub fn take_records(&mut self) -> Vec<PendingDecision> {
        std::mem::take(&mut self.records)
    }

    /// Drains the local bot decisions awaiting a database write into
    /// `local_opponent_actions`.
    pub fn take_local_actions(&mut self) -> Vec<crate::db::LocalOpponentAction> {
        std::mem::take(&mut self.local_actions)
    }

    /// Drains the local hero decisions awaiting a database write into
    /// `local_hero_actions`.
    pub fn take_local_hero_actions(&mut self) -> Vec<crate::db::LocalHeroAction> {
        std::mem::take(&mut self.local_hero_actions)
    }

    /// Drains the per-hand results awaiting a database write.
    pub fn take_hand_results(&mut self) -> Vec<PendingHandResult> {
        std::mem::take(&mut self.hand_results)
    }

    /// The tournament outcome once the hero busts out or a single seat is
    /// left standing, or `None` while the tournament is still running. The
    /// opponents never play on after the hero busts: the moment the hero's
    /// last chip is gone the tournament ends and the chip-leading opponent is
    /// recorded as the winner. Aggregates the recorded hand results so the
    /// winner/loser modal and the detail page can be populated.
    pub fn tournament_result(&self) -> Option<TournamentResult> {
        let winner = match self.state.tournament_winner() {
            Some(seat) => seat,
            None => {
                if !self.state.eliminated(Seat::Hero) {
                    return None;
                }
                let mut leader = Seat::Opponent1;
                for seat in Seat::ALL {
                    if !self.state.eliminated(seat)
                        && self.state.stack(seat) > self.state.stack(leader)
                    {
                        leader = seat;
                    }
                }
                leader
            }
        };
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
    /// interception is confirmed: the highest-EV action replacing
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
        self.preflop_aggressor = None;
        self.flop_bettor = None;
        self.hero_all_in_this_hand = false;
        self.push_sound(Sound::Deal);
        let small_blind = self.state.small_blind_seat();
        let big_blind = self.state.big_blind_seat();
        self.log_line(format!(
            "— Hand #{} — blinds {}/{}",
            self.hand_no,
            self.state.blind_level().small_blind,
            self.state.blind_level().big_blind,
        ));
        self.log_line(format!("Dealer: {}", actor_name(self.state.button())));
        self.log_line(format!(
            "{} post SB {}",
            actor_name(small_blind),
            self.state.street_contribution(small_blind)
        ));
        self.log_line(format!(
            "{} post BB {}",
            actor_name(big_blind),
            self.state.street_contribution(big_blind)
        ));
        self.log_line(format!(
            "You are dealt {}",
            hole_cards_text(self.state.hole_cards(Seat::Hero))
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
        if self.tournament_result().is_some() {
            return Err(Error::Game(
                "the tournament is over — no further hands are dealt".into(),
            ));
        }
        tracing::info!(
            hand_won = self.hand_no,
            "result pause over — dealing the next hand"
        );
        self.deal_next_hand()?;
        // pending_check_fold is always None right after a fresh deal, so
        // pump()'s events can never be non-empty here.
        self.pump()?;
        Ok(())
    }

    /// Drives the opponents until a decision or the end of the hand is
    /// reached. When the hand ends the result is logged and the win sound
    /// queued, but the next deal is deferred to [`Self::advance_after_result`]
    /// so the winner stays on screen for a beat. Returns whether anything
    /// happened, plus any table events produced by an auto-resolved
    /// check-fold (a pending check-fold folding once an opponent raises) —
    /// most callers have no such events and can ignore the second element.
    pub fn pump(&mut self) -> Result<(bool, Vec<TableEvent>)> {
        let mut acted = false;
        if self.state.is_hand_over() {
            return Ok((acted, Vec::new()));
        }
        loop {
            if self.state.to_act() == Seat::Hero {
                if self.pending_check_fold == Some(self.state.street())
                    && self.state.legal_actions().call_amount > 0
                {
                    self.pending_check_fold = None;
                    let events = self.submit(Action::Fold)?;
                    return Ok((true, events));
                }
                return Ok((acted, Vec::new()));
            }
            let actor = self.state.to_act();
            let legal = self.state.legal_actions();
            let call_amount = legal.call_amount;
            // Captured before the action settles: the node/bucket/position/
            // true-cards/c-bet-context describe the decision the actor is
            // currently facing.
            let node = decision_node(&self.state);
            let stack_bucket = decision_stack_bucket(&self.state);
            let position = decision_position(&self.state);
            let hole_cards = self.state.rotated(actor).hero_cards();
            let was_preflop_aggressor = self.was_preflop_aggressor(actor);
            let facing_cbet = self.facing_cbet();
            let action = match self.template {
                Some(template) => template_action(
                    &mut self.rng,
                    &self.state,
                    actor,
                    &self.mcts,
                    &template,
                    &self.opponent_model.ranges,
                    &self.opponent_model.frequencies,
                    was_preflop_aggressor,
                    facing_cbet,
                    self.preflop_aggressor,
                ),
                None => placeholder_action(&mut self.rng, &self.state),
            };
            self.opponents
                .record(actor, action, self.state.street(), legal.call_amount > 0);
            self.local_actions.push(crate::db::LocalOpponentAction {
                node: node.key().to_string(),
                stack_bucket: stack_bucket.as_i16(),
                hole_cards: format!("{} {}", hole_cards[0].to_code(), hole_cards[1].to_code()),
                action: ActionCategory::of(action).label().to_string(),
                hand_no: self.hand_no as i64,
                position: position.key().to_string(),
                was_preflop_aggressor,
                facing_cbet,
            });
            self.pump_actions.push(action);
            let outcome = self.settle_action(action)?;
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
                return Ok((acted, Vec::new()));
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

        let ranges = self.ranges();
        let analyzed = decision::analyze(
            &mut self.rng,
            &self.state,
            &ranges,
            &self.mcts,
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

        let analyzed = match decision::analyze_snapshot(&self.state, snapshot, Some(action)) {
            Ok(analyzed) => analyzed,
            Err(_) => {
                let ranges = self.ranges();
                decision::analyze(&mut self.rng, &self.state, &ranges, &self.mcts, Some(action))?
            }
        };
        self.finish_submission(action, analyzed)
    }

    /// Checks now, arming an auto-fold for the hero's next decision on this
    /// street if an opponent raises before action returns. The check itself
    /// is graded exactly like [`Self::submit`] (including blunder
    /// interception); the later auto-fold, if triggered, is graded the same
    /// way in turn — see [`Self::pump`].
    pub fn submit_check_fold(&mut self) -> Result<Vec<TableEvent>> {
        self.check_fold_requested = true;
        let result = self.submit(Action::Check);
        if result.is_err() {
            self.check_fold_requested = false;
        }
        result
    }

    /// Like [`Self::submit_check_fold`], but scored against the background
    /// solver's snapshot — see [`Self::submit_with_snapshot`].
    pub fn submit_check_fold_with_snapshot(
        &mut self,
        snapshot: &crate::mcts::SolveResult,
    ) -> Result<Vec<TableEvent>> {
        self.check_fold_requested = true;
        let result = self.submit_with_snapshot(Action::Check, snapshot);
        if result.is_err() {
            self.check_fold_requested = false;
        }
        result
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
        let ev_loss_pot = analyzed
            .played
            .as_ref()
            .map(|played| played.ev_loss_pot)
            .unwrap_or(0.0);

        let intercepted = self.blunder_tracker.should_intercept(ev_loss_pot);
        self.blunder_tracker.record_action(ev_loss_pot);

        if intercepted {
            tracing::info!(
                ev_loss,
                ev_loss_pot,
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
                decision: Box::new(analyzed),
                hand_no: self.hand_no,
                intercepted: true,
            }]);
        }

        self.record_decision(&analyzed, action, ev_loss, ev_loss_pot);
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
        let ev_loss_pot = pending
            .analyzed
            .played
            .as_ref()
            .map(|played| played.ev_loss_pot)
            .unwrap_or(0.0);
        tracing::info!(
            played = ?pending.action,
            optimal = ?optimal,
            ev_loss,
            ev_loss_pot,
            hand_no = self.hand_no,
            "review confirmed — the coach's best-EV action replaces the blunder on the table"
        );
        self.record_decision(&pending.analyzed, pending.action, ev_loss, ev_loss_pot);
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
        let street = self.state.street();
        let check_fold_wanted = std::mem::take(&mut self.check_fold_requested);
        let outcome = self.settle_action(action)?;
        if action == Action::Check && check_fold_wanted {
            self.pending_check_fold = Some(street);
        }
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
        let (_, extra_events) = self.pump()?;

        let mut events = vec![TableEvent::ChartTick {
            action_index,
            ev_loss,
        }];
        events.extend(extra_events);
        events.push(TableEvent::State);
        Ok(events)
    }

    /// Applies one action and settles the street/hand boundaries it creates,
    /// appending action-log lines for any board cards that were dealt as a
    /// result (flop/turn/river — showdown run-outs narrate all three). Every
    /// seat's action (hero included) passes through here, making it the one
    /// place that can maintain `preflop_aggressor`/`flop_bettor` for the
    /// whole hand regardless of who acts.
    fn settle_action(&mut self, action: Action) -> Result<ActionOutcome> {
        self.note_aggressor(self.state.to_act(), self.state.street(), action);
        let board_before = self.state.board().len();
        let outcome = apply_settled(&mut self.state, &mut self.deck, action)?;
        let board = self.state.board().to_vec();
        if board.len() > board_before {
            if board_before < 3 && board.len() >= 3 {
                self.log_line(format!("Flop {}", cards_text(&board[..3])));
            }
            if board_before < 4 && board.len() >= 4 {
                self.log_line(format!("Turn {}", board[3]));
            }
            if board_before < 5 && board.len() >= 5 {
                self.log_line(format!("River {}", board[4]));
            }
            self.push_sound(Sound::Deal);
        }
        Ok(outcome)
    }

    /// Updates the hand's c-bet context after seeing `actor` take `action`
    /// on `street` — called from [`Self::settle_action`] with the state as
    /// it stood *before* the action, so it records who becomes the new
    /// aggressor rather than double-counting the action just taken.
    fn note_aggressor(&mut self, actor: Seat, street: Street, action: Action) {
        let is_aggressive = matches!(action, Action::Bet(_) | Action::Raise(_) | Action::AllIn);
        match street {
            Street::Preflop if is_aggressive => self.preflop_aggressor = Some(actor),
            Street::Flop if is_aggressive => self.flop_bettor = Some(actor),
            _ => {}
        }
    }

    /// Whether `seat` was the last seat to bet/raise/all-in preflop this
    /// hand — a c-bet opportunity when `seat` then leads the flop.
    fn was_preflop_aggressor(&self, seat: Seat) -> bool {
        self.preflop_aggressor == Some(seat)
    }

    /// Whether the flop bet currently being faced came from the preflop
    /// aggressor — a fold-to-c-bet opportunity for whoever is on the clock.
    fn facing_cbet(&self) -> bool {
        self.state.street() == Street::Flop
            && self.flop_bettor.is_some()
            && self.flop_bettor == self.preflop_aggressor
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
                self.log_line(format!("{winner} wins {total} — everyone else folded"));
                tracing::info!(
                    hand_no = self.hand_no,
                    winner = %winner,
                    pot = total,
                    stacks = ?self.state.stacks(),
                    "hand finished — {winner} wins the pot uncontested"
                );
            }
            HandEndReason::Showdown => {
                let board = self.state.board().to_vec();
                self.log_line(format!("Board {}", cards_text(&board)));
                for (seat, cards, class) in &result.revealed {
                    self.log_line(format!("{seat} shows {} {} ({class})", cards[0], cards[1]));
                }
                for r in &result.returns {
                    self.log_line(format!(
                        "Uncalled bet ({}) returned to {}",
                        r.amount, r.seat
                    ));
                }
                if let [award] = result.awards.as_slice() {
                    let class_text = result
                        .revealed
                        .iter()
                        .find(|(seat, _, _)| *seat == award.seat)
                        .map(|(_, _, class)| format!(" with {class}"))
                        .unwrap_or_default();
                    self.log_line(format!("{} wins {}{class_text}", award.seat, award.amount));
                } else {
                    let shares: Vec<String> = result
                        .awards
                        .iter()
                        .map(|award| format!("{} +{}", award.seat, award.amount))
                        .collect();
                    self.log_line(format!("Split pot · {}", shares.join(" · ")));
                }
                tracing::info!(
                    hand_no = self.hand_no,
                    pot = total,
                    awards = ?result.awards,
                    returns = ?result.returns,
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

/// Formats dealt board cards as space-separated codes for log lines:
/// `"2c 7h Kd"`.
fn cards_text(cards: &[Card]) -> String {
    cards
        .iter()
        .map(|card| card.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The action-log name of a seat: the hero is addressed as "You", opponents
/// by their seat names.
fn actor_name(seat: Seat) -> &'static str {
    match seat {
        Seat::Hero => "You",
        Seat::Opponent1 => "Opponent 1",
        Seat::Opponent2 => "Opponent 2",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blunder::BlunderConfig;
    use crate::card::{Card, Deck, Rank, Suit};
    use crate::error::Error;
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

    /// Test preset: with an empty rolling history nothing can ever cross the
    /// threshold (it is infinite), so a fresh session's first decision never
    /// intercepts — suboptimal actions apply immediately without feedback.
    /// A test that submits more than once and still needs every decision to
    /// stay unintercepted should pair this with
    /// `prime_blunder_history(&[LARGE])` so later decisions can't cross a
    /// small earlier loss.
    fn never_intercepts() -> BlunderConfig {
        BlunderConfig::default()
    }

    /// Test preset: paired with `prime_blunder_history(&[0.0])` at the call
    /// site so the rolling history is non-empty and its one entry — the
    /// threshold, with a single-point history — is 0.0: any non-optimal
    /// decision (EV loss > 0.0) then intercepts.
    fn always_intercepts() -> BlunderConfig {
        BlunderConfig::default()
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
        let mut session = TableSession::new(
            41,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            OpponentModel::default(),
        );
        session.deal_next_hand().unwrap();
        session.pump().unwrap();
        assert_eq!(session.state().to_act(), Seat::Hero);
        assert_eq!(session.hand_no(), 1);
        assert!(!session.state().is_hand_over());
    }

    /// A fresh drill seats every player with the resolved starting chip
    /// count, not a stack baked into the engine.
    #[test]
    fn new_seats_everyone_with_the_resolved_starting_stack() {
        let session = TableSession::new(
            77,
            probe_config(),
            never_intercepts(),
            None,
            420,
            OpponentModel::default(),
        );
        assert_eq!(session.state().stacks(), [420, 420, 420]);
    }

    /// The solver prior the coach uses for the hero's decision comes from
    /// the learned opponent-range model, resolved for the exact node/stack
    /// bucket the hero is facing — not a hardcoded uniform "any two cards"
    /// prior. A session with no history (or no sample yet for this node)
    /// still falls back to uniform.
    #[test]
    fn ranges_resolve_from_the_opponent_model_and_fall_back_to_uniform() {
        let mut session = TableSession::new(
            41,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            OpponentModel::default(),
        );
        session.deal_next_hand().unwrap();
        let uniform = [1.0f32 / HAND_COUNT as f32; HAND_COUNT];
        assert_eq!(
            session.ranges(),
            [uniform, uniform],
            "no history yet — falls back to uniform"
        );

        let node = crate::opponent_history::decision_node(session.state());
        let bucket = crate::opponent_history::decision_stack_bucket(session.state());
        let mut tight = [0.0f32; HAND_COUNT];
        tight[0] = 1.0; // AA
        let mut entries = std::collections::HashMap::new();
        entries.insert((node, bucket), tight);
        let model = OpponentModel {
            ranges: crate::opponent_history::OpponentRangeModel::from_entries(entries),
            frequencies: Default::default(),
            historic: Default::default(),
            hero_historic: Default::default(),
        };
        let mut session = TableSession::new(
            41,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            model,
        );
        session.deal_next_hand().unwrap();
        assert_eq!(
            session.ranges(),
            [tight, tight],
            "a resolved node/bucket range feeds both opponent seats"
        );
    }

    /// Regression for the "raise 4-8o into a real raise" coaching complaint:
    /// before this fix, both opponent seats always resolved the same pooled
    /// per-node range, so a seat that had just voluntarily raised preflop
    /// was modeled with the *same* wide "whoever hasn't decided yet" prior
    /// as a seat still to act — making a reraise over the actual raiser look
    /// far more profitable than it is. The raiser's own seat must now read a
    /// distinct, narrower prior once it has raised.
    #[test]
    fn ranges_use_the_preflop_raisers_own_prior_for_the_seat_that_raised() {
        let mut session = TableSession::new(
            41,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            OpponentModel::default(),
        );
        session.deal_next_hand().unwrap();

        let node = crate::opponent_history::decision_node(session.state());
        let bucket = crate::opponent_history::decision_stack_bucket(session.state());
        let mut pooled = [0.0f32; HAND_COUNT];
        pooled[0] = 1.0; // AA — the generic "whoever is at this node" prior.
        let mut entries = std::collections::HashMap::new();
        entries.insert((node, bucket), pooled);

        let seven_deuce = crate::range::hands::Hand::new(Rank::Seven, Rank::Two, false).index();
        let mut raiser = [0.0f32; HAND_COUNT];
        raiser[seven_deuce] = 1.0;
        let mut raiser_entries = std::collections::HashMap::new();
        raiser_entries.insert(bucket, raiser);

        let model = OpponentModel {
            ranges: crate::opponent_history::OpponentRangeModel::from_entries_with_raiser(
                entries,
                raiser_entries,
            ),
            frequencies: Default::default(),
            historic: Default::default(),
            hero_historic: Default::default(),
        };
        let mut session = TableSession::new(
            41,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            model,
        );
        session.deal_next_hand().unwrap();

        assert_eq!(
            session.ranges(),
            [pooled, pooled],
            "nobody has raised yet — both seats fall back to the pooled node prior"
        );

        session.preflop_aggressor = Some(Seat::Opponent2);
        assert_eq!(
            session.ranges(),
            [pooled, raiser],
            "only the seat that actually raised switches to the raiser prior"
        );

        session.preflop_aggressor = Some(Seat::Opponent1);
        assert_eq!(
            session.ranges(),
            [raiser, pooled],
            "the prior follows whichever seat is recorded as the raiser"
        );
    }

    /// Each deal names the button, logs both blind posts, and logs the
    /// hero's dealt hand, so the action log shows exactly what every seat
    /// committed and what the hero was dealt before any action.
    #[test]
    fn deals_log_the_button_and_the_blind_posts() {
        let mut session = TableSession::new(
            41,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            OpponentModel::default(),
        );
        session.deal_next_hand().unwrap();
        let log = session.log();
        assert!(
            log.contains(&"— Hand #1 — blinds 10/20".to_string()),
            "{log:?}"
        );
        assert!(log.contains(&"Dealer: You".to_string()), "{log:?}");
        assert!(log.contains(&"You post SB 10".to_string()), "{log:?}");
        assert!(
            log.contains(&"Opponent 1 post BB 20".to_string()),
            "{log:?}"
        );
        let hero_cards = session.state().hole_cards(Seat::Hero).unwrap();
        assert!(
            log.contains(&format!("You are dealt {} {}", hero_cards[0], hero_cards[1])),
            "{log:?}"
        );
    }

    /// The next hand logs the rotated button and the moved blind posts.
    #[test]
    fn the_next_deal_logs_the_rotated_button_and_blinds() {
        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            71,
            probe_config(),
            never_intercepts(),
            None,
        );
        session.submit(Action::Fold).unwrap();
        session.advance_after_result().unwrap();
        assert_eq!(session.hand_no(), 2);
        let log = session.log();
        assert!(
            log.contains(&"— Hand #2 — blinds 10/20".to_string()),
            "{log:?}"
        );
        assert!(
            log.contains(&"Dealer: Opponent 1".to_string()),
            "{log:?}"
        );
        assert!(
            log.contains(&"Opponent 1 post SB 10".to_string()),
            "{log:?}"
        );
        assert!(
            log.contains(&"Opponent 2 post BB 20".to_string()),
            "{log:?}"
        );
    }

    /// Every street action is logged and, when a street is dealt or run out,
    /// the board cards appear in the log under their street name.
    #[test]
    fn board_cards_are_logged_as_streets_are_dealt() {
        // Hand layout: button on Opponent 2, the hero is the BB; both
        // opponents limp so the hero faces a bet/check decision.
        let mut deck = Deck::default();
        let mut state = GameState::new(Seat::Opponent2, level());
        state.start_hand(&mut deck).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let mut session = TableSession::resume(
            state,
            deck,
            3,
            63,
            probe_config(),
            never_intercepts(),
            None,
        );

        // Preflop closes on the hero's bet: it is a "raise to" total, so the
        // posted blind is not committed twice (regression: the street used
        // to wedge open and never deal a flop).
        session.settle_action(Action::Bet(60)).unwrap();
        session.settle_action(Action::Call).unwrap();
        session.settle_action(Action::Call).unwrap();
        assert_eq!(session.state().street(), Street::Flop);
        assert_eq!(session.state().board().len(), 3);
        let board = session.state().board();
        let flop = format!("Flop {} {} {}", board[0], board[1], board[2]);
        assert!(
            session.log().contains(&flop),
            "flop cards are logged: {:?}",
            session.log()
        );

        // Postflop the hero acts first (button on Opponent 2). Checks
        // around deal the turn and river, each logged with its card.
        session.settle_action(Action::Check).unwrap();
        session.settle_action(Action::Check).unwrap();
        session.settle_action(Action::Check).unwrap();
        assert_eq!(session.state().street(), Street::Turn);
        let board = session.state().board();
        let turn = format!("Turn {}", board[3]);
        assert!(
            session.log().contains(&turn),
            "turn card is logged: {:?}",
            session.log()
        );

        session.settle_action(Action::Check).unwrap();
        session.settle_action(Action::Check).unwrap();
        session.settle_action(Action::Check).unwrap();
        assert_eq!(session.state().street(), Street::River);
        assert_eq!(session.state().board().len(), 5);
        let board = session.state().board();
        let river = format!("River {}", board[4]);
        assert!(
            session.log().contains(&river),
            "river card is logged: {:?}",
            session.log()
        );
    }

    /// All-in preflop: the showdown runs out the whole board at once, and the
    /// log narrates flop, turn, and river.
    #[test]
    fn showdown_runout_logs_every_street() {
        let mut deck = Deck::default();
        let mut state = GameState::new(Seat::Opponent1, level());
        state.start_hand(&mut deck).unwrap();

        let mut session = TableSession::resume(
            state,
            deck,
            2,
            64,
            probe_config(),
            never_intercepts(),
            None,
        );
        // Drive an all-in snowball deterministically: hero, Opponent 2, then
        // Opponent 1 all shove; the hand ends at showdown with 5 board cards.
        session.settle_action(Action::AllIn).unwrap();
        session.settle_action(Action::AllIn).unwrap();
        session.settle_action(Action::AllIn).unwrap();
        assert!(session.state().is_hand_over());

        let board = session.state().board();
        assert_eq!(board.len(), 5);
        let lines = session.log();
        assert!(
            lines.contains(&format!("Flop {} {} {}", board[0], board[1], board[2])),
            "{lines:?}"
        );
        assert!(lines.contains(&format!("Turn {}", board[3])), "{lines:?}");
        assert!(lines.contains(&format!("River {}", board[4])), "{lines:?}");
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
            never_intercepts(),
            None,
        );
        assert!(!session.pump().unwrap().0);
        assert_eq!(session.state().to_act(), Seat::Hero);
        assert_eq!(session.hand_no(), 3);
    }

    #[test]
    fn decision_token_tracks_the_on_screen_decision() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            3,
            47,
            probe_config(),
            never_intercepts(),
            None,
        );
        assert_eq!(session.action_no(), 0);
        assert_eq!(
            session.decision_token().as_deref(),
            Some("h3-a0-river"),
            "hand, action count, and street name the current decision"
        );

        session.submit(Action::Call).unwrap();
        assert_eq!(
            session.decision_token(),
            None,
            "no decision token while the hand result shows"
        );
        assert_eq!(session.action_no(), 1);

        session.advance_after_result().unwrap();
        assert_eq!(
            session.decision_token().as_deref(),
            Some("h4-a1-preflop"),
            "the next hand starts a fresh preflop decision"
        );
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
            never_intercepts(),
            None,
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
            never_intercepts(),
            None,
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
            never_intercepts(),
            None,
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
            always_intercepts(),
            None,
        );
        session.prime_blunder_history(&[0.0]);

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
            None,
        )
        .unwrap();

        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            44,
            probe_config(),
            always_intercepts(),
            None,
        );
        session.prime_blunder_history(&[0.0]);
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
            never_intercepts(),
            None,
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
        assert_eq!(state.stacks().iter().sum::<u32>(), STARTING_STACK * 3);
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
            never_intercepts(),
            None,
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
            never_intercepts(),
            None,
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

    /// Every actually-applied hero decision queues into `local_hero_actions`
    /// (true dealt cards, coarse action) — the fallback/fill source for the
    /// hero's own starting-hand window, mirroring the bot recording exercised
    /// by [`pump`](TableSession::pump).
    #[test]
    fn hero_decisions_queue_local_hero_actions_for_persistence() {
        let state = river_facing_bet();
        let hero_cards = state.hero_cards();
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
            never_intercepts(),
            None,
        );
        assert!(session.take_local_hero_actions().is_empty());
        session
            .submit_with_snapshot(Action::Fold, &snapshot)
            .unwrap();
        let actions = session.take_local_hero_actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, "Fold");
        assert_eq!(
            actions[0].hole_cards,
            format!("{} {}", hero_cards[0].to_code(), hero_cards[1].to_code())
        );
        assert!(
            session.take_local_hero_actions().is_empty(),
            "draining empties the buffer"
        );
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
            never_intercepts(),
            None,
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
            never_intercepts(),
            None,
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
        let mut session = TableSession::new(
            48,
            probe_config(),
            never_intercepts(),
            None,
            STARTING_STACK,
            OpponentModel::default(),
        );
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

    /// At showdown the full board is logged first, then every revealed hand,
    /// followed by a line stating
    /// the winner (with the winning hand class) or the split-pot shares, so
    /// the action log tells the whole story of the hand.
    #[test]
    fn showdown_logs_revealed_cards_and_winner_amount() {
        let mut session = TableSession::resume(
            river_facing_bet(),
            Deck::default(),
            1,
            50,
            probe_config(),
            never_intercepts(),
            None,
        );
        session.submit(Action::Call).unwrap();
        let log = session.log();
        let board_line = session
            .state()
            .board()
            .iter()
            .map(|card| card.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let board = format!("Board {board_line}");
        let board_pos = log
            .iter()
            .position(|line| line.eq(&board))
            .expect("the full board is logged at showdown");
        let first_reveal = log
            .iter()
            .position(|line| line.contains("shows"))
            .expect("revealed cards are logged at showdown");
        assert!(
            board_pos < first_reveal,
            "the board line comes before the revealed hands: {log:?}"
        );
        assert!(
            log.iter().any(|line| line.contains("shows")),
            "revealed cards are logged at showdown: {log:?}"
        );
        assert!(
            log.iter()
                .any(|line| line.contains(" wins ") || line.starts_with("Split pot")),
            "who won or how the pot was shared is logged at showdown: {log:?}"
        );
    }

    /// The lived bug: hero's Aces-and-board-Threes two pair beats the
    /// opponent's lone board pair, and the opponent's uncalled 5 chips come
    /// back — the log crowns the hero alone and states the return. An
    /// uncalled return is never logged as a split.
    #[test]
    fn uncalled_excess_is_logged_as_a_return_not_a_split() {
        let custom: Vec<Card> = deck_with([
            card(Rank::Three, Suit::Spades),
            card(Rank::Ace, Suit::Clubs),
            card(Rank::Nine, Suit::Diamonds),
            card(Rank::Three, Suit::Hearts),
            card(Rank::Eight, Suit::Diamonds),
        ]);
        let mut deck = Deck::try_from_remaining(custom).unwrap();
        let mut state = GameState::new(Seat::Opponent2, level());
        state.set_stack(Seat::Hero, 210);
        state.set_stack(Seat::Opponent1, 130);
        state.set_stack(Seat::Opponent2, 215);
        state.start_hand(&mut deck).unwrap();
        state.set_hole_cards(
            Seat::Hero,
            [card(Rank::Ace, Suit::Spades), card(Rank::Two, Suit::Spades)],
        );
        state.set_hole_cards(
            Seat::Opponent2,
            [
                card(Rank::Queen, Suit::Spades),
                card(Rank::Six, Suit::Diamonds),
            ],
        );

        // Button is Opponent 2, so preflop runs Opponent 1 -> Opponent 2 ->
        // Hero. Opponent 1 folds without investing; Opponent 2 pushes all-in.
        assert_eq!(state.to_act(), Seat::Opponent1);
        state.apply_action(Action::Fold).unwrap();
        state.apply_action(Action::AllIn).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);

        let mut session = TableSession::resume(
            state,
            deck,
            1,
            50,
            probe_config(),
            never_intercepts(),
            None,
        );
        // The hero calls all-in for 210 short of the 215 bet: the missing 5
        // is an uncalled portion of Opponent 2's bet, returned at showdown.
        session.submit(Action::AllIn).unwrap();
        assert!(session.state().is_hand_over());
        let log = session.log();
        assert!(
            log.iter()
                .any(|line| line == "Uncalled bet (5) returned to Opponent 2"),
            "the uncalled chips are logged as a return: {log:?}"
        );
        assert!(
            log.iter().any(|line| line.starts_with("Hero wins 420")),
            "the hero is crowned the sole winner: {log:?}"
        );
        assert!(
            !log.iter().any(|line| line.starts_with("Split pot")),
            "an uncalled return is not a split: {log:?}"
        );
        assert_eq!(session.hand_results.len(), 1);
        assert!(session.hand_results[0].hero_won);
    }

    /// A fold-out hand states who won in the log, so every hand ends with an
    /// explicit winner line.
    #[test]
    fn fold_out_logs_the_winner() {
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut Deck::default()).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            50,
            probe_config(),
            never_intercepts(),
            None,
        );
        session.submit(Action::Fold).unwrap();
        let log = session.log();
        assert!(
            log.iter()
                .any(|line| { line.contains(" wins ") && line.contains("everyone else folded") }),
            "the fold-out winner is stated in the log: {log:?}"
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
            never_intercepts(),
            None,
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
            never_intercepts(),
            None,
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
            never_intercepts(),
            None,
        );
        assert_eq!(session.tournament_result(), None);
    }

    /// Losing the last chip ends the tournament on the spot — even with both
    /// opponents still in the hand count, the chip leader is recorded as the
    /// winner and the hero gets the loss.
    #[test]
    fn busting_the_hero_ends_the_tournament_with_the_chip_leader() {
        let mut state = river_facing_bet();
        state.set_stack(Seat::Hero, 0);
        state.set_stack(Seat::Opponent1, 420);
        state.set_stack(Seat::Opponent2, 1080);
        state.set_eliminated(Seat::Hero, true);
        let session = TableSession::resume(
            state,
            Deck::default(),
            23,
            62,
            probe_config(),
            never_intercepts(),
            None,
        );
        let result = session
            .tournament_result()
            .expect("a busted hero ends the tournament");
        assert!(!result.won, "the hero lost");
        assert_eq!(result.winner, Seat::Opponent2, "the chip leader wins");
        assert_eq!(result.final_stacks, [0, 420, 1080]);
        assert_eq!(result.hands, 23);
    }

    /// Once the hero is out no further hand is dealt — the opponents never
    /// play on and the result pause cannot advance the table.
    #[test]
    fn the_table_cannot_advance_after_the_hero_busts() {
        let mut state = river_facing_bet();
        state.set_stack(Seat::Hero, 0);
        state.set_eliminated(Seat::Hero, true);
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            23,
            63,
            probe_config(),
            never_intercepts(),
            None,
        );
        assert!(
            matches!(session.advance_after_result(), Err(Error::Game(_))),
            "dealing on is refused once the tournament is over"
        );
        assert_eq!(session.hand_no(), 23, "no new hand was dealt");
    }

    /// An opponent busting while the hero still has chips keeps the
    /// tournament running — only the hero busting (or a sole survivor) ends
    /// it.
    #[test]
    fn an_opponent_busting_does_not_end_the_tournament() {
        let mut state = river_facing_bet();
        state.set_stack(Seat::Opponent1, 0);
        state.set_eliminated(Seat::Opponent1, true);
        let session = TableSession::resume(
            state,
            Deck::default(),
            4,
            64,
            probe_config(),
            never_intercepts(),
            None,
        );
        assert_eq!(session.tournament_result(), None, "the hero is still in");
    }

    /// A mid-hand snapshot round-trips every live fact: stacks, street, board,
    /// per-seat contributions, current bet, the actor, the deck order, the
    /// counters, and the action log.
    #[test]
    fn snapshot_round_trip_preserves_the_live_table() {
        let mut deck = Deck::default();
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck).unwrap();
        // Opponent 2 limps, hero raises, Opponent 1 calls — a live betting
        // state with different per-seat contributions.
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Raise(60)).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.advance_street(&mut deck).unwrap();
        state.apply_action(Action::Bet(40)).unwrap();

        let mut session = TableSession::resume(
            state,
            deck,
            7,
            62,
            probe_config(),
            never_intercepts(),
            None,
        );
        session.action_no = 15; // pinned via the test hook so the decision token round-trips
        session.log_line("You bet 40".to_string());

        let before = session.to_snapshot();
        let restored = TableSession::from_snapshot(
            &before,
            999,
            probe_config(),
            never_intercepts(),
            OpponentModel::default(),
        )
        .unwrap();

        let live = &session.state;
        let revived = restored.state();
        assert_eq!(revived.stacks(), live.stacks());
        assert_eq!(revived.button(), live.button());
        assert_eq!(revived.blind_level(), live.blind_level());
        assert_eq!(revived.street(), live.street());
        assert_eq!(revived.board(), live.board());
        assert_eq!(
            revived.street_contribution(Seat::Hero),
            live.street_contribution(Seat::Hero)
        );
        assert_eq!(
            revived.street_contribution(Seat::Opponent1),
            live.street_contribution(Seat::Opponent1)
        );
        assert_eq!(
            revived.street_contribution(Seat::Opponent2),
            live.street_contribution(Seat::Opponent2)
        );
        assert_eq!(revived.total_pot(), live.total_pot());
        assert_eq!(revived.current_bet(), live.current_bet());
        assert_eq!(revived.to_act(), live.to_act());
        assert_eq!(
            revived.folded(Seat::Opponent2),
            live.folded(Seat::Opponent2)
        );
        assert_eq!(revived.all_in(Seat::Hero), live.all_in(Seat::Hero));
        assert_eq!(revived.hero_cards(), live.hero_cards());
        assert_eq!(
            revived.hole_cards(Seat::Opponent1),
            live.hole_cards(Seat::Opponent1)
        );
        assert_eq!(restored.hand_no(), session.hand_no());
        assert_eq!(restored.action_no(), session.action_no());
        assert_eq!(restored.log(), session.log());
        assert_eq!(
            restored.to_snapshot().deck,
            before.deck,
            "deck order survives"
        );
        assert_eq!(
            restored.to_snapshot().opponents,
            before.opponents,
            "the HUD counters survive"
        );
        // The revived legal set is identical: the exact bet sizes resume.
        assert_eq!(
            revived.legal_actions().min_bet,
            live.legal_actions().min_bet
        );
        assert_eq!(
            revived.legal_actions().max_bet,
            live.legal_actions().max_bet
        );
    }

    /// A snapshot taken while the win ribbon shows resumes the same finished
    /// hand (with its award), and the next deal continues normally.
    #[test]
    fn snapshot_of_a_paused_hand_keeps_the_win_ribbon() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            3,
            63,
            probe_config(),
            never_intercepts(),
            None,
        );
        session.submit(Action::Fold).unwrap();
        assert!(session.state().is_hand_over());
        let awards = session
            .state()
            .hand_result()
            .expect("the hand result is live")
            .awards
            .clone();

        let snapshot = session.to_snapshot();
        let mut resumed = TableSession::from_snapshot(
            &snapshot,
            1000,
            probe_config(),
            never_intercepts(),
            OpponentModel::default(),
        )
        .unwrap();
        assert!(
            resumed.state().is_hand_over(),
            "the paused hand stays finished"
        );
        assert_eq!(
            resumed.state().hand_result().unwrap().awards,
            awards,
            "the win ribbon amounts survive"
        );
        assert_eq!(resumed.hand_no(), 3);

        resumed.advance_after_result().unwrap();
        assert_eq!(resumed.hand_no(), 4);
        assert!(resumed.state().to_act() != Seat::Hero || !resumed.state().is_hand_over());
    }

    /// A resuming table counts the persisted decisions into the blunder
    /// tracker, so the interception threshold keeps its exact history.
    #[test]
    fn hydrate_blunder_rebuilds_the_rolling_history() {
        let state = river_facing_bet();
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            2,
            64,
            probe_config(),
            never_intercepts(),
            None,
        );
        session.hydrate_blunder(&[(9, 1, 0.0), (9, 1, 3.0), (9, 2, 1.5)]);
        assert_eq!(session.blunder_tracker.recorded_actions(), 3);
    }

    /// A corrupted snapshot (bad card code, oversized deck) is rejected with
    /// a game error instead of silently inventing a table.
    #[test]
    fn corrupted_snapshots_are_rejected() {
        let state = river_facing_bet();
        let session = TableSession::resume(
            state,
            Deck::default(),
            1,
            65,
            probe_config(),
            never_intercepts(),
            None,
        );
        let mut snapshot = session.to_snapshot();
        snapshot.deck = vec!["Xx".to_string()];
        assert!(matches!(
            TableSession::from_snapshot(
                &snapshot,
                1,
                probe_config(),
                never_intercepts(),
                OpponentModel::default()
            ),
            Err(Error::Game(_))
        ));

        let mut snapshot = session.to_snapshot();
        snapshot.deck = (0..53).map(|_| "2c".to_string()).collect();
        assert!(matches!(
            TableSession::from_snapshot(
                &snapshot,
                1,
                probe_config(),
                never_intercepts(),
                OpponentModel::default()
            ),
            Err(Error::Game(_))
        ));

        let mut snapshot = session.to_snapshot();
        snapshot.state.board = vec!["Xz".to_string()];
        assert!(matches!(
            TableSession::from_snapshot(
                &snapshot,
                1,
                probe_config(),
                never_intercepts(),
                OpponentModel::default()
            ),
            Err(Error::Game(_))
        ));

        let mut snapshot = session.to_snapshot();
        snapshot.state.hole_cards.pop();
        assert!(matches!(
            TableSession::from_snapshot(
                &snapshot,
                1,
                probe_config(),
                never_intercepts(),
                OpponentModel::default()
            ),
            Err(Error::Game(_))
        ));
    }

    /// Deals a hand with the button on Opponent 2 (so Hero, the small blind,
    /// acts first postflop) and plays preflop to a flop where Hero faces no
    /// action yet this street.
    fn hero_checked_to_on_flop(seed: u64) -> GameState {
        let mut state = GameState::new(Seat::Opponent2, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(seed)))
            .unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Call).unwrap();
        state.apply_action(Action::Check).unwrap();
        state
            .advance_street(&mut Deck::shuffled(&mut seeded_rng(seed + 1)))
            .unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        assert!(state.legal_actions().can_check);
        state
    }

    /// Seed 0: Hero checks the flop first to act, Opponent 1 bets, Opponent 2
    /// calls, and action returns to Hero facing the bet — the pre-armed fold
    /// fires there, and the two opponents play the hand out to showdown.
    #[test]
    fn check_fold_auto_folds_when_an_opponent_raises() {
        let state = hero_checked_to_on_flop(0);
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            0,
            probe_config(),
            never_intercepts(),
            None,
        );
        // Two decisions are submitted below (the check, then the pre-armed
        // fold); prime the rolling history so the first submission can't
        // become the lone-point threshold the second one crosses — see
        // `never_intercepts`'s doc comment.
        session.prime_blunder_history(&[1000.0]);

        let events = session.submit_check_fold().unwrap();

        assert_eq!(
            session.log(),
            [
                "You check",
                "Opponent 1 bet 30",
                "Opponent 2 call 30",
                "You fold",
                "Turn 2c",
                "Opponent 1 check",
                "River 3c",
                "Opponent 2 check",
                "Opponent 1 check",
                "Opponent 2 check",
                "Board 8s As Ad 2c 3c",
                "Opponent 1 shows 8c 3c (Two Pair)",
                "Opponent 2 shows Jc 4h (Pair)",
                "Opponent 1 wins 120 with Two Pair",
            ],
            "the pre-armed fold fires the moment action returns to the hero"
        );
        assert!(session.state().is_hand_over());
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TableEvent::ChartTick { .. }))
                .count(),
            2,
            "the check and the auto-fold are each graded and charted separately: {events:?}"
        );
        assert_eq!(events.last(), Some(&TableEvent::State));

        let records = session.take_records();
        assert_eq!(
            records.len(),
            2,
            "the coach grades the check and the later fold as two separate decisions"
        );
        assert_eq!(records[0].played, views::action_label(Action::Check));
        assert_eq!(records[1].played, views::action_label(Action::Fold));
    }

    /// Like [`check_fold_auto_folds_when_an_opponent_raises`], but with a
    /// zero-chip blunder threshold: the auto-fold is intercepted exactly like
    /// a manually-submitted Fold would be, freezing the table for review
    /// instead of silently applying.
    #[test]
    fn check_fold_auto_fold_can_be_intercepted_like_a_live_decision() {
        let state = hero_checked_to_on_flop(160);
        let mut session = TableSession::resume(
            state,
            Deck::default(),
            1,
            160,
            probe_config(),
            always_intercepts(),
            None,
        );
        session.prime_blunder_history(&[0.0]);

        let events = session.submit_check_fold().unwrap();

        assert_eq!(
            session.log(),
            ["You check", "Opponent 1 bet 30", "Opponent 2 fold"],
            "the auto-fold is held back, not logged, while the review is pending"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                TableEvent::TacticalOverlay {
                    intercepted: true,
                    ..
                }
            )),
            "the pre-armed fold must go through the same interception pipeline as a live fold: {events:?}"
        );
        assert!(session.has_pending_interception());
        assert_eq!(
            session.state().to_act(),
            Seat::Hero,
            "the table is frozen facing the bet, awaiting review"
        );
        assert!(session.state().legal_actions().call_amount > 0);

        session.confirm_review().unwrap();
        assert!(
            !session.has_pending_interception(),
            "confirming the review replays the coach's optimal action"
        );
        let records = session.take_records();
        assert_eq!(
            records.len(),
            2,
            "both the check and the (intercepted) fold are recorded"
        );
        assert_eq!(records[1].played, views::action_label(Action::Fold));
        assert_ne!(
            records[1].optimal,
            views::action_label(Action::Fold),
            "the coach's optimal action replaces the blunder on the table"
        );
    }
}
