use crate::error::{Error, Result};

/// Parameters controlling one solver run: how many determinizations are
/// sampled from the opponent ranges and how deep each per-world search goes.
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
}

impl Default for MctsConfig {
    fn default() -> Self {
        Self {
            worlds: 32,
            iterations: 128,
            uct_c: 60.0,
            max_depth: 4,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        MctsConfig::default().validate().unwrap();
        assert_eq!(MctsConfig::default().worlds, 32);
        assert_eq!(MctsConfig::default().iterations, 128);
    }

    #[test]
    fn test_config_is_fast_and_valid() {
        let config = MctsConfig::test();
        config.validate().unwrap();
        assert!(config.worlds <= 8);
        assert!(config.iterations <= 64);
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
}
