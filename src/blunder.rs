use std::collections::VecDeque;

use crate::error::{Error, Result};

/// Calibration parameters for the blunder-intervention engine.
///
/// Interventions are calibrated so that roughly one in `target_hands` hands
/// is interrupted: after a warm-up phase the trigger threshold is the
/// `(1 − p)`-quantile of the hero's own rolling EV-loss history, where
/// `p = 1 / (target_hands · A_hand)` and `A_hand` is the rolling
/// actions-per-hand ratio. All EV losses are measured in big blinds so a
/// river mistake in a large pot does not drown out equally bad preflop play.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlunderConfig {
    /// Interventions fire on roughly one hand in this many.
    pub target_hands: u32,
    /// Rolling window of recent hero EV losses used for percentile selection.
    pub history_actions: usize,
    /// Rolling window of recent hands used for the actions-per-hand ratio.
    pub history_hands: usize,
    /// Minimum number of recorded actions before the percentile threshold is
    /// trusted; until then a big-blind floor applies.
    pub warmup_actions: usize,
    /// The warm-up threshold in big blinds: `ev_loss >= fallback_bb`
    /// interrupts the decision.
    pub fallback_bb: f64,
    /// Lower/upper clamps on the trigger ratio `p`.
    pub min_trigger_ratio: f64,
    pub max_trigger_ratio: f64,
}

impl Default for BlunderConfig {
    fn default() -> Self {
        Self {
            target_hands: 3,
            history_actions: 300,
            history_hands: 300,
            warmup_actions: 24,
            fallback_bb: 2.0,
            min_trigger_ratio: 0.01,
            max_trigger_ratio: 0.5,
        }
    }
}

impl BlunderConfig {
    /// Fast preset used by the test suite: tiny windows so the percentile
    /// path is reached quickly.
    pub const fn test() -> Self {
        Self {
            target_hands: 3,
            history_actions: 8,
            history_hands: 8,
            warmup_actions: 4,
            fallback_bb: 2.0,
            min_trigger_ratio: 0.01,
            max_trigger_ratio: 0.5,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.target_hands == 0 {
            return Err(Error::InvalidConfig(
                "blunder: `target_hands` must be at least 1".into(),
            ));
        }
        if self.history_actions < 2 {
            return Err(Error::InvalidConfig(
                "blunder: `history_actions` must be at least 2".into(),
            ));
        }
        if self.history_hands < 2 {
            return Err(Error::InvalidConfig(
                "blunder: `history_hands` must be at least 2".into(),
            ));
        }
        if self.warmup_actions == 0 {
            return Err(Error::InvalidConfig(
                "blunder: `warmup_actions` must be at least 1".into(),
            ));
        }
        if !self.fallback_bb.is_finite() || self.fallback_bb < 0.0 {
            return Err(Error::InvalidConfig(
                "blunder: `fallback_bb` must be finite and non-negative".into(),
            ));
        }
        if !self.min_trigger_ratio.is_finite()
            || !self.max_trigger_ratio.is_finite()
            || self.min_trigger_ratio <= 0.0
            || self.max_trigger_ratio < self.min_trigger_ratio
        {
            return Err(Error::InvalidConfig(
                "blunder: trigger-ratio clamps must be finite and ordered".into(),
            ));
        }
        Ok(())
    }
}

/// The engine's rolling error-rate history: recent hero EV losses (in big
/// blinds) and the actions-per-hand ratio used to derive the dynamic
/// threshold.
#[derive(Clone, Debug)]
pub struct Tracker {
    config: BlunderConfig,
    /// Recent hero EV losses, oldest first; capped at `history_actions`.
    losses: VecDeque<f64>,
    /// Hero actions per completed hand, oldest first; capped at
    /// `history_hands` and reset across `reset`ed sessions.
    hand_actions: VecDeque<u32>,
    /// Hero actions taken in the hand currently being played.
    current_hand_actions: u32,
}

impl Tracker {
    pub fn new(config: BlunderConfig) -> Self {
        Self {
            config,
            losses: VecDeque::with_capacity(config.history_actions),
            hand_actions: VecDeque::with_capacity(config.history_hands),
            current_hand_actions: 0,
        }
    }

    /// Records one evaluated hero decision.
    pub fn record_action(&mut self, ev_loss: f64) {
        while self.losses.len() >= self.config.history_actions {
            self.losses.pop_front();
        }
        self.losses.push_back(ev_loss);
        self.current_hand_actions = self.current_hand_actions.saturating_add(1);
    }

    /// Closes the current hand and starts counting actions for the next one.
    pub fn end_hand(&mut self) {
        while self.hand_actions.len() >= self.config.history_hands {
            self.hand_actions.pop_front();
        }
        self.hand_actions.push_back(self.current_hand_actions);
        self.current_hand_actions = 0;
    }

    /// Number of hero actions seen so far, capped at the configured window.
    pub fn recorded_actions(&self) -> usize {
        self.losses.len()
    }

    /// The rolling actions-per-hand ratio over the completed-hand window.
    /// The hand in progress only ever adds at most one pending action so
    /// early hands do not start with a misleadingly empty window.
    pub fn actions_per_hand(&self) -> f64 {
        if self.hand_actions.is_empty() {
            return 1.0;
        }
        let total: u32 = self.hand_actions.iter().sum();
        let completed: f64 = f64::from(total) / self.hand_actions.len() as f64;
        let pending: f64 =
            f64::from(self.current_hand_actions.min(1)) / self.hand_actions.len() as f64;
        (completed + pending).max(1.0)
    }

    /// The running average EV loss over the action window, the monitored
    /// error rate.
    pub fn mean_ev_loss(&self) -> f64 {
        if self.losses.is_empty() {
            return 0.0;
        }
        self.losses.iter().sum::<f64>() / self.losses.len() as f64
    }

    /// Intervene-target ratio: one trigger per `target_hands` hands spread
    /// over the expected actions in those hands, clamped per config.
    pub fn trigger_ratio(&self) -> f64 {
        let ratio = 1.0 / (f64::from(self.config.target_hands) * self.actions_per_hand());
        ratio.clamp(self.config.min_trigger_ratio, self.config.max_trigger_ratio)
    }

    /// The dynamic threshold in big blinds: an infinite threshold before any
    /// history (nothing fires), the floor during warm-up, then the
    /// `(1 − p)`-quantile of recent losses by nearest rank.
    pub fn threshold(&self) -> f64 {
        if self.losses.is_empty() {
            return f64::INFINITY;
        }
        if self.losses.len() < self.config.warmup_actions {
            return self.config.fallback_bb;
        }
        kth_largest(&self.losses, self.trigger_rank())
    }

    /// Whether the given EV loss (in big blinds) interrupts the current
    /// decision.
    pub fn should_intercept(&self, ev_loss: f64) -> bool {
        if ev_loss <= 0.0 {
            return false;
        }
        ev_loss >= self.threshold()
    }

    /// The rank of the threshold within the action window: the k-th largest
    /// loss, `k = ceil(p · n)`, clamped to a valid rank.
    fn trigger_rank(&self) -> usize {
        let n = self.losses.len();
        let k = (self.trigger_ratio() * n as f64).ceil() as usize;
        k.clamp(1, n)
    }
}

/// The k-th largest value of a slice (nearest-rank percentile).
fn kth_largest(losses: &VecDeque<f64>, k: usize) -> f64 {
    let mut sorted: Vec<f64> = losses.iter().copied().collect();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() - k]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BlunderConfig {
        BlunderConfig {
            target_hands: 3,
            history_actions: 8,
            history_hands: 4,
            warmup_actions: 4,
            fallback_bb: 2.0,
            min_trigger_ratio: 0.05,
            max_trigger_ratio: 0.5,
        }
    }

    fn record_many(tracker: &mut Tracker, losses: &[f64]) {
        for loss in losses {
            tracker.record_action(*loss);
        }
    }

    #[test]
    fn default_config_is_valid_and_matches_the_spec() {
        let config = BlunderConfig::default();
        config.validate().unwrap();
        assert_eq!(config.target_hands, 3);
        assert_eq!(config.history_actions, 300);
        assert_eq!(config.history_hands, 300);
        assert_eq!(config.warmup_actions, 24);
        assert_eq!(config.fallback_bb, 2.0);
        // Config::test() stays fast enough for the test suite.
        BlunderConfig::test().validate().unwrap();
        assert!(BlunderConfig::test().history_actions <= 16);
    }

    #[test]
    fn invalid_configs_are_rejected() {
        for (field, value) in [
            (
                "target_hands",
                BlunderConfig {
                    target_hands: 0,
                    ..config()
                },
            ),
            (
                "history_actions",
                BlunderConfig {
                    history_actions: 1,
                    ..config()
                },
            ),
            (
                "history_hands",
                BlunderConfig {
                    history_hands: 1,
                    ..config()
                },
            ),
            (
                "warmup_actions",
                BlunderConfig {
                    warmup_actions: 0,
                    ..config()
                },
            ),
        ] {
            assert!(
                matches!(value.validate(), Err(Error::InvalidConfig(_))),
                "{field} should be rejected"
            );
        }
        for fallback_bb in [-1.0, f64::NAN, f64::INFINITY] {
            let bad = BlunderConfig {
                fallback_bb,
                ..config()
            };
            assert!(matches!(bad.validate(), Err(Error::InvalidConfig(_))));
        }
        for clamps in [(0.0, 0.5), (0.6, 0.5), (0.1, f64::NAN)] {
            let bad = BlunderConfig {
                min_trigger_ratio: clamps.0,
                max_trigger_ratio: clamps.1,
                ..config()
            };
            assert!(matches!(bad.validate(), Err(Error::InvalidConfig(_))));
        }
    }

    #[test]
    fn losses_and_hand_counts_are_windowed() {
        let mut tracker = Tracker::new(config());
        for loss in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0] {
            tracker.record_action(loss);
        }
        assert_eq!(tracker.recorded_actions(), 8, "window caps at 8 actions");
        assert_eq!(
            tracker.losses.iter().copied().collect::<Vec<_>>(),
            vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
        );

        for actions in [2, 1, 3, 0, 4] {
            for _ in 0..actions {
                tracker.record_action(0.0);
            }
            tracker.end_hand();
        }
        assert_eq!(
            tracker.hand_actions.iter().copied().collect::<Vec<_>>(),
            vec![1, 3, 0, 4],
            "hand window caps at 4 hands"
        );
    }

    #[test]
    fn actions_per_hand_averages_the_window_and_stays_at_least_one() {
        let mut tracker = Tracker::new(config());
        assert_eq!(
            tracker.actions_per_hand(),
            1.0,
            "empty window defaults to 1"
        );

        for actions in [3, 1] {
            for _ in 0..actions {
                tracker.record_action(0.0);
            }
            tracker.end_hand();
        }
        assert_eq!(tracker.actions_per_hand(), 2.0);

        tracker.end_hand();
        assert_eq!(tracker.actions_per_hand(), (3.0 + 1.0 + 0.0) / 3.0);
    }

    #[test]
    fn mean_ev_loss_is_the_monitored_error_rate() {
        let mut tracker = Tracker::new(config());
        assert_eq!(tracker.mean_ev_loss(), 0.0);
        record_many(&mut tracker, &[2.0, 4.0, 6.0]);
        assert_eq!(tracker.mean_ev_loss(), 4.0);
    }

    #[test]
    fn empty_history_never_intercepts_but_warmup_floor_applies_after_one_action() {
        let empty = Tracker::new(config());
        assert!(
            !empty.should_intercept(1000.0),
            "nothing fires before any history"
        );
        assert_eq!(empty.threshold(), f64::INFINITY);

        let mut tracker = Tracker::new(config());
        tracker.record_action(50.0);
        assert_eq!(
            tracker.threshold(),
            config().fallback_bb,
            "warm-up floor in big blinds"
        );
        assert!(!tracker.should_intercept(1.9));
        assert!(tracker.should_intercept(2.0));
    }

    #[test]
    fn warmup_uses_the_big_blind_floor() {
        let mut tracker = Tracker::new(config());
        record_many(&mut tracker, &[1.0, 1.0, 1.0]);
        assert_eq!(tracker.recorded_actions(), 3);
        assert!(tracker.recorded_actions() < tracker.config.warmup_actions);
        assert_eq!(
            tracker.threshold(),
            config().fallback_bb,
            "2BB floor while warming up"
        );
        assert!(!tracker.should_intercept(1.9));
        assert!(tracker.should_intercept(2.0));
    }

    #[test]
    fn optimal_play_never_intercepts() {
        let mut tracker = Tracker::new(config());
        record_many(&mut tracker, &[5.0, 5.0, 5.0, 5.0]);
        assert!(
            !tracker.should_intercept(0.0),
            "zero EV loss is never intercepted"
        );
        assert!(!tracker.should_intercept(-1.0));
    }

    #[test]
    fn warmup_exit_uses_nearest_rank_percentile() {
        let mut tracker = Tracker::new(config());
        // 4 actions => 1 per hand => ratio 1/3, clamped up to 0.05;
        // k = ceil(0.05 * 4) = 1 -> the single worst loss (40.0).
        record_many(&mut tracker, &[1.0, 2.0, 40.0, 3.0]);
        assert_eq!(tracker.trigger_ratio(), 1.0f64 / 3.0);
        assert_eq!(
            tracker.trigger_rank(),
            2,
            "k = ceil(4/3) = 2, the 2nd largest = 3.0"
        );
        assert_eq!(tracker.threshold(), 3.0);
        assert!(!tracker.should_intercept(2.9));
        assert!(tracker.should_intercept(3.0));
    }

    #[test]
    fn trigger_ratio_clamps_with_few_actions_per_hand() {
        let mut tracker = Tracker::new(config());
        // One hero action per hand: p = 1/3 would flood interventions.
        tracker.end_hand();
        tracker.end_hand();
        tracker.end_hand();
        tracker.end_hand();
        assert_eq!(tracker.actions_per_hand(), 1.0);
        assert_eq!(
            tracker.trigger_ratio(),
            1.0 / 3.0,
            "natural ratio within clamps"
        );

        let mut zeroes = Tracker::new(BlunderConfig {
            min_trigger_ratio: 0.4,
            ..config()
        });
        zeroes.end_hand();
        zeroes.end_hand();
        assert_eq!(
            zeroes.trigger_ratio(),
            0.4,
            "clamped up to the configured floor"
        );
    }

    #[test]
    fn trigger_ratio_stays_within_its_clamps() {
        let tracker = Tracker::new(config());
        let p = tracker.trigger_ratio();
        assert!((config().min_trigger_ratio..=config().max_trigger_ratio).contains(&p));
        assert_eq!(
            p,
            (1.0 / (3.0 * tracker.actions_per_hand()))
                .clamp(config().min_trigger_ratio, config().max_trigger_ratio)
        );

        let mut busy = Tracker::new(config());
        for _ in 0..7 {
            busy.record_action(2.0);
        }
        busy.end_hand();
        // One 7-action hand: A_hand = 7, natural p = 1/21 below the 0.05 clamp.
        assert_eq!(busy.actions_per_hand(), 7.0);
        assert_eq!(
            busy.trigger_ratio(),
            0.05,
            "clamped up to the configured floor"
        );
    }

    #[test]
    fn calibration_delivers_roughly_one_intervene_per_three_hands() {
        // Synthetic hero: 70% optimal (0 loss), 30% lognormal-ish errors.
        use rand::Rng;
        use rand::SeedableRng;
        use rand_chacha::ChaCha8Rng;

        let config = BlunderConfig {
            target_hands: 3,
            history_actions: 300,
            history_hands: 300,
            warmup_actions: 24,
            fallback_bb: 2.0,
            min_trigger_ratio: 0.01,
            max_trigger_ratio: 0.5,
        };
        let mut tracker = Tracker::new(config);
        let mut rng = ChaCha8Rng::seed_from_u64(2026);

        let mut actions = 0;
        let mut intercepts = 0;
        let mut hands = 0;
        let mut intercept_hands = 0u32;
        let mut intercepted_this_hand = false;
        for _ in 0..10_000 {
            let loss = if rng.random_bool(0.7) {
                0.0
            } else {
                0.5 + rng.random::<f64>() * 60.0
            };
            let fires = tracker.should_intercept(loss);
            intercepts += u64::from(fires);
            intercepted_this_hand |= fires;
            tracker.record_action(loss);
            actions += 1;
            if actions % 9 == 0 {
                tracker.end_hand();
                hands += 1;
                if intercepted_this_hand {
                    intercept_hands += 1;
                }
                intercepted_this_hand = false;
            }
        }

        let p = 1.0 / (3.0 * actions as f64 / hands as f64);
        let expected = p.clamp(0.01, 0.5);
        let rate = intercepts as f64 / actions as f64;
        assert!(
            (rate - expected).abs() < 0.02,
            "intercept rate {rate:.4} deviates from target {expected:.4}"
        );
        let per_hand = intercept_hands as f64 / hands as f64;
        assert!(
            (per_hand - 1.0 / 3.0).abs() < 0.1,
            "one intervention per ~3 hands expected, got {per_hand:.4}"
        );
    }
}
