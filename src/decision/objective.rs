use crate::error::{Error, Result};

/// Risk parameters for survivability-based optimal-action selection.
///
/// Derived from CRRA utility `U(c) = c^(1-γ) / (1-γ)` over the hero's
/// terminal stack, second-order expanded around the current stack `S`
/// (Pratt-Arrow mean-variance form):
///
/// ```text
/// score = EV − λ·σ² − cost·P(bust)
/// ```
///
/// with λ = γ / (2·S) — the variance penalty shrinks when deep-stacked and
/// grows when short — and `cost = [U(S) − U(b)] / U′(S)`, the chip equivalent
/// of falling from `S` to the utility floor `b` (a bust). For γ = 1 the cost
/// reduces to `S·ln(S/b)`, the Kelly/log-utility objective: maximizing the
/// expected log growth of the stack, i.e. surviving the longest in a
/// winner-take-all tournament where only first place pays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurvivalConfig {
    /// CRRA risk-aversion coefficient. γ = 1 is log utility (Kelly). Larger
    /// values punish variance and bust outcomes harder; γ → 0 approaches raw
    /// chip EV.
    pub risk_aversion: f64,
    /// The stack (in chips) whose utility a bust is floored at. Clamped
    /// strictly below the hero's stack at derivation time.
    pub utility_floor: u32,
}

impl Default for SurvivalConfig {
    fn default() -> Self {
        Self {
            risk_aversion: 1.0,
            utility_floor: 1,
        }
    }
}

/// How far `for_hand` will scale `risk_aversion` up (comfortably covering
/// the table) or down (badly outchipped) from the configured baseline.
const STACK_SCALE_MIN: f64 = 0.25;
const STACK_SCALE_MAX: f64 = 2.0;

/// A short stack fighting to survive should keep taking its equity rather
/// than folding away every marginal edge (it blinds out either way), so a
/// meaningful survival floor is a handful of big blinds, not a literal chip.
const FLOOR_BIG_BLINDS: u32 = 2;

impl SurvivalConfig {
    /// Rescales this config to one hand's actual table context.
    ///
    /// `risk_aversion` is multiplied by the hero's stack relative to the
    /// average of the given (live) opponent stacks, clamped to
    /// [`STACK_SCALE_MIN`, `STACK_SCALE_MAX`]: comfortably covering the table
    /// tightens the survival objective toward protecting the lead, while
    /// being badly outchipped loosens it back toward chip EV — a short stack
    /// that folds every hand to dodge a hypothetical bust just blinds out
    /// anyway, so it should keep taking +EV risks with a live hand.
    ///
    /// `utility_floor` is replaced by [`FLOOR_BIG_BLINDS`] big blinds: the
    /// configured value treats "one chip" as the disaster reference point,
    /// which makes the bust-cost log-ratio (and thus the whole penalty)
    /// far larger than it should be for a stack that is merely short, not
    /// actually crippled.
    pub fn for_hand(&self, hero_stack: u32, opponent_stacks: &[u32], big_blind: u32) -> Self {
        let scale = if opponent_stacks.is_empty() {
            1.0
        } else {
            let avg = opponent_stacks.iter().copied().map(f64::from).sum::<f64>()
                / opponent_stacks.len() as f64;
            (f64::from(hero_stack) / avg.max(1.0)).clamp(STACK_SCALE_MIN, STACK_SCALE_MAX)
        };
        Self {
            risk_aversion: self.risk_aversion * scale,
            utility_floor: big_blind.saturating_mul(FLOOR_BIG_BLINDS).max(1),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !self.risk_aversion.is_finite() || self.risk_aversion <= 0.0 {
            return Err(Error::InvalidConfig(
                "survival: `risk_aversion` must be positive and finite".into(),
            ));
        }
        if self.utility_floor == 0 {
            return Err(Error::InvalidConfig(
                "survival: `utility_floor` must be at least 1 chip".into(),
            ));
        }
        Ok(())
    }

    /// Derives the per-decision risk coefficients for a hero stack `S`.
    pub fn derive(&self, stack: u32) -> Result<DerivedRisk> {
        self.validate()?;
        if stack == 0 {
            return Err(Error::Decision(
                "cannot derive survival parameters for a busted stack".into(),
            ));
        }
        let floor = effective_floor(self.utility_floor, stack);
        Ok(DerivedRisk {
            variance_coefficient: variance_coefficient(self.risk_aversion, stack),
            bust_cost: crra_bust_cost(self.risk_aversion, stack, floor),
        })
    }
}

/// The per-decision coefficients of the survivability score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DerivedRisk {
    /// λ = γ / (2·S): multiplies the payoff variance (in chips²).
    pub variance_coefficient: f64,
    /// The chip-equivalent cost of busting, multiplying P(bust).
    pub bust_cost: f64,
}

impl DerivedRisk {
    /// `EV − λ·σ² − cost·P(bust)`: the survivability score of one action.
    pub fn score(&self, ev: f64, variance: f64, bust_prob: f64) -> f64 {
        ev - self.variance_coefficient * variance - self.bust_cost * bust_prob
    }
}

/// λ = γ / (2·S), the Pratt-Arrow variance penalty of CRRA utility.
pub fn variance_coefficient(risk_aversion: f64, stack: u32) -> f64 {
    risk_aversion / (2.0 * f64::from(stack))
}

/// The utility floor, clamped into `[1, S − 1]` so the bust cost stays
/// finite and strictly positive (it is zero when the hero is already all-in).
pub fn effective_floor(floor: u32, stack: u32) -> u32 {
    floor.min(stack.saturating_sub(1)).max(1)
}

/// `[U(S) − U(b)] / U′(S)`, the bust cost in chips for CRRA utility with
/// coefficient `γ`. For γ = 1 this is exactly `S·ln(S/b)` (log/Kelly).
pub fn crra_bust_cost(risk_aversion: f64, stack: u32, floor: u32) -> f64 {
    let s = f64::from(stack);
    let b = f64::from(effective_floor(floor, stack));
    if (risk_aversion - 1.0).abs() < 1e-12 {
        s * (s / b).ln()
    } else {
        (s - s.powf(risk_aversion) * b.powf(1.0 - risk_aversion)) / (1.0 - risk_aversion)
    }
}

/// The full survivability score of one action estimate.
pub fn survival_score(
    ev: f64,
    variance: f64,
    bust_prob: f64,
    stack: u32,
    config: &SurvivalConfig,
) -> Result<f64> {
    let risk = config.derive(stack)?;
    Ok(risk.score(ev, variance, bust_prob))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_kelly_over_one_chip() {
        let config = SurvivalConfig::default();
        config.validate().unwrap();
        assert_eq!(config.risk_aversion, 1.0);
        assert_eq!(config.utility_floor, 1);
    }

    #[test]
    fn invalid_configs_are_rejected() {
        for risk_aversion in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let config = SurvivalConfig {
                risk_aversion,
                ..SurvivalConfig::default()
            };
            assert!(matches!(config.validate(), Err(Error::InvalidConfig(_))));
        }
        let config = SurvivalConfig {
            utility_floor: 0,
            ..SurvivalConfig::default()
        };
        assert!(matches!(config.validate(), Err(Error::InvalidConfig(_))));
    }

    #[test]
    fn variance_coefficient_halves_with_stack() {
        assert_eq!(variance_coefficient(1.0, 500), 0.001);
        assert_eq!(variance_coefficient(1.0, 250), 0.002);
        assert_eq!(variance_coefficient(2.0, 500), 0.002);
    }

    #[test]
    fn effective_floor_is_clamped_below_the_stack() {
        assert_eq!(effective_floor(1, 500), 1);
        assert_eq!(effective_floor(50, 500), 50);
        assert_eq!(
            effective_floor(50, 30),
            29,
            "floor must stay below the stack"
        );
        assert_eq!(effective_floor(50, 2), 1);
        assert_eq!(effective_floor(10, 1), 1);
    }

    #[test]
    fn kelly_bust_cost_is_stack_times_log_stack_over_floor() {
        assert_eq!(crra_bust_cost(1.0, 500, 1), 500.0 * 500.0f64.ln());
        assert_eq!(
            crra_bust_cost(1.0, 500, 250),
            500.0 * (500.0f64 / 250.0).ln()
        );
    }

    #[test]
    fn crra_bust_cost_approaches_the_kelly_limit() {
        let kelly = crra_bust_cost(1.0, 500, 1);
        for gamma in [1.0 + 1e-9, 1.0 - 1e-9] {
            let crra = crra_bust_cost(gamma, 400, 7);
            let kelly = crra_bust_cost(1.0, 400, 7);
            assert!(
                (crra - kelly).abs() < 1e-3,
                "γ={gamma} bust cost {crra} deviates from Kelly {kelly}"
            );
        }
        assert!(kelly > 0.0);
        assert!(crra_bust_cost(2.0, 400, 7) > 0.0);
    }

    #[test]
    fn derived_risk_quantifies_deep_vs_short_stack_pressure() {
        let config = SurvivalConfig::default();
        let deep = config.derive(500).unwrap();
        let short = config.derive(100).unwrap();
        assert_eq!(deep.variance_coefficient, 0.001);
        assert_eq!(short.variance_coefficient, 0.005);
        assert!(deep.bust_cost > short.bust_cost);
    }

    #[test]
    fn derive_rejects_a_busted_stack() {
        let config = SurvivalConfig::default();
        assert!(matches!(config.derive(0), Err(Error::Decision(_))));
    }

    #[test]
    fn score_trades_ev_variance_and_bust_probability() {
        let config = SurvivalConfig {
            risk_aversion: 1.0,
            utility_floor: 100,
        };
        let risk = config.derive(500).unwrap();
        assert_eq!(risk.score(10.0, 0.0, 0.0), 10.0);
        assert_eq!(
            risk.score(10.0, 400.0, 0.0),
            10.0 - risk.variance_coefficient * 400.0
        );
        assert_eq!(risk.score(10.0, 0.0, 0.5), 10.0 - risk.bust_cost * 0.5);

        let score = survival_score(5.0, 16.0, 0.2, 500, &config).unwrap();
        assert_eq!(score, risk.score(5.0, 16.0, 0.2));
    }

    #[test]
    fn a_busty_action_ranks_below_an_equal_ev_safe_one() {
        let config = SurvivalConfig::default();
        let risk = config.derive(500).unwrap();
        let safe = risk.score(0.0, 100.0, 0.0);
        let busty = risk.score(0.0, 100.0, 0.05);
        assert!(safe > busty);
    }

    #[test]
    fn for_hand_loosens_risk_aversion_for_a_short_stack() {
        let config = SurvivalConfig::default();
        let scaled = config.for_hand(230, &[640], 10);
        assert!(scaled.risk_aversion < config.risk_aversion);
        assert_eq!(scaled.risk_aversion, config.risk_aversion * (230.0 / 640.0));
    }

    #[test]
    fn for_hand_tightens_risk_aversion_for_a_covering_stack() {
        let config = SurvivalConfig::default();
        let scaled = config.for_hand(900, &[300, 300], 10);
        assert_eq!(scaled.risk_aversion, config.risk_aversion * STACK_SCALE_MAX);
    }

    #[test]
    fn for_hand_clamps_the_scale_and_keeps_a_positive_floor() {
        let config = SurvivalConfig::default();
        let crippled = config.for_hand(10, &[2000], 10);
        assert_eq!(crippled.risk_aversion, config.risk_aversion * STACK_SCALE_MIN);
        assert_eq!(crippled.utility_floor, 20);

        let no_opponents = config.for_hand(500, &[], 10);
        assert_eq!(no_opponents.risk_aversion, config.risk_aversion);
    }

    #[test]
    fn for_hand_replaces_the_one_chip_floor_with_big_blinds() {
        let config = SurvivalConfig::default();
        let scaled = config.for_hand(230, &[640], 25);
        assert_eq!(scaled.utility_floor, 50);
    }
}
