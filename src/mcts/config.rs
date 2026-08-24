use std::time::Duration;

use crate::error::{Error, Result};
use crate::game::Street;

/// Parameters controlling one solver run: how many determinizations are
/// sampled from the opponent ranges and how deep each per-world search goes.
/// Effective budgets are scaled per street via [`MctsConfig::for_street`].
///
/// A "world" is one sampled opponent holding plus the remaining deck order;
/// every world gets its own isolated search, and the root action values are
/// averaged over worlds with their range probabilities.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MctsConfig {
    /// Number of opponent-hand determinizations sampled per solve.
    pub worlds: usize,
    /// MCTS iterations (root visits) per world.
    pub iterations: usize,
    /// UCB1 exploration constant, expressed in chips so it scales with the
    /// EV differences between actions.
    pub uct_c: f64,
    /// Maximum number of hero decisions explored in the tree per world before
    /// falling back to rollout policy.
    pub max_depth: usize,
    /// Minimum wall-clock time a decision keeps searching, even after the
    /// iteration budget is reached, so the tree has time to grow deeper.
    pub min_duration: Duration,
}

impl Default for MctsConfig {
    fn default() -> Self {
        Self {
            worlds: 32,
            iterations: 128,
            uct_c: 60.0,
            max_depth: 4,
            min_duration: Duration::from_secs(5),
        }
    }
}

impl MctsConfig {
    /// Fast preset used by the test suite: small but still representative.
    pub const fn test() -> Self {
        Self {
            worlds: 4,
            iterations: 32,
            uct_c: 40.0,
            max_depth: 3,
            min_duration: Duration::ZERO,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.worlds == 0 {
            return Err(Error::InvalidConfig(
                "mcts: `worlds` must be at least 1".into(),
            ));
        }
        if self.iterations == 0 {
            return Err(Error::InvalidConfig(
                "mcts: `iterations` must be at least 1".into(),
            ));
        }
        if !self.uct_c.is_finite() || self.uct_c <= 0.0 {
            return Err(Error::InvalidConfig(
                "mcts: `uct_c` must be positive and finite".into(),
            ));
        }
        Ok(())
    }

    /// The effective budget for a decision on `street`.
    ///
    /// Early streets branch over many more unknown runouts than the river, so
    /// a straight per-street budget either under-searches preflop (noisy junk
    /// hands get erratic "optimal" calls) or wastes time on the river. The
    /// multipliers spend the extra effort where the branching is:
    ///
    /// * preflop — 2× worlds, 2× iterations, one extra tree-depth cap,
    /// * flop — 1.5× worlds and iterations,
    /// * turn — 1.25× worlds and iterations,
    /// * river — unchanged.
    pub fn for_street(self, street: Street) -> Self {
        let (worlds_scale, iterations_scale, depth_extra) = match street {
            Street::Preflop => (2.0, 2.0, 1_usize),
            Street::Flop => (1.5, 1.5, 0),
            Street::Turn => (1.25, 1.25, 0),
            Street::River => (1.0, 1.0, 0),
        };
        Self {
            worlds: scale_up(self.worlds, worlds_scale),
            iterations: scale_up(self.iterations, iterations_scale),
            max_depth: self.max_depth + depth_extra,
            ..self
        }
    }
}

/// Rounds a budget up to the nearest whole unit, at least one.
fn scale_up(base: usize, scale: f64) -> usize {
    ((base as f64 * scale).ceil() as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        MctsConfig::default().validate().unwrap();
        assert_eq!(MctsConfig::default().worlds, 32);
        assert_eq!(MctsConfig::default().iterations, 128);
        assert_eq!(
            MctsConfig::default().min_duration,
            Duration::from_secs(5),
            "live searches keep thinking for at least five seconds"
        );
    }

    #[test]
    fn test_config_is_fast_and_valid() {
        let config = MctsConfig::test();
        config.validate().unwrap();
        assert!(config.worlds <= 8);
        assert!(config.iterations <= 64);
        assert_eq!(
            config.min_duration,
            Duration::ZERO,
            "tests skip the minimum-duration wait"
        );
    }

    #[test]
    fn zero_worlds_is_rejected() {
        let config = MctsConfig {
            worlds: 0,
            ..MctsConfig::default()
        };
        assert!(matches!(config.validate(), Err(Error::InvalidConfig(_))));
    }

    #[test]
    fn zero_iterations_is_rejected() {
        let config = MctsConfig {
            iterations: 0,
            ..MctsConfig::default()
        };
        assert!(matches!(config.validate(), Err(Error::InvalidConfig(_))));
    }

    #[test]
    fn bad_uct_constant_is_rejected() {
        for uct_c in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let config = MctsConfig {
                uct_c,
                ..MctsConfig::default()
            };
            assert!(matches!(config.validate(), Err(Error::InvalidConfig(_))));
        }
    }

    #[test]
    fn street_budgets_scale_effort_where_the_branching_is() {
        let base = MctsConfig {
            worlds: 10,
            iterations: 100,
            uct_c: 60.0,
            max_depth: 3,
            min_duration: Duration::ZERO,
        };

        let preflop = base.for_street(crate::game::Street::Preflop);
        assert_eq!(preflop.worlds, 20, "preflop doubles the worlds");
        assert_eq!(preflop.iterations, 200, "preflop doubles the iterations");
        assert_eq!(preflop.max_depth, 4, "preflop gains one tree-depth cap");

        let flop = base.for_street(crate::game::Street::Flop);
        assert_eq!(flop.worlds, 15);
        assert_eq!(flop.iterations, 150);
        assert_eq!(flop.max_depth, 3, "postflop keeps the depth cap");

        let turn = base.for_street(crate::game::Street::Turn);
        assert_eq!(turn.worlds, 13, "1.25× rounds up to the whole unit");

        assert_eq!(
            base.for_street(crate::game::Street::River),
            base,
            "the river keeps the base budget"
        );
        preflop.validate().unwrap();
    }
}
