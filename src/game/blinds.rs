/// A blind level: the small and big blind amounts in chips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlindLevel {
    pub small_blind: u32,
    pub big_blind: u32,
}

impl BlindLevel {
    pub const fn new(small_blind: u32, big_blind: u32) -> Self {
        Self {
            small_blind,
            big_blind,
        }
    }
}

/// The standard GGPoker Spin and Gold blind escalation (3-minute levels).
pub const BLIND_SCHEDULE: &[BlindLevel] = &[
    BlindLevel::new(10, 20),
    BlindLevel::new(15, 30),
    BlindLevel::new(20, 40),
    BlindLevel::new(25, 50),
    BlindLevel::new(30, 60),
    BlindLevel::new(40, 80),
    BlindLevel::new(50, 100),
    BlindLevel::new(60, 120),
    BlindLevel::new(80, 160),
    BlindLevel::new(100, 200),
    BlindLevel::new(125, 250),
    BlindLevel::new(150, 300),
    BlindLevel::new(200, 400),
    BlindLevel::new(250, 500),
    BlindLevel::new(300, 600),
    BlindLevel::new(400, 800),
    BlindLevel::new(500, 1000),
    BlindLevel::new(600, 1200),
    BlindLevel::new(800, 1600),
    BlindLevel::new(1000, 2000),
];

/// Returns the level following `current` in the schedule, or `None` at the top.
pub fn next_level(current: BlindLevel) -> Option<BlindLevel> {
    BLIND_SCHEDULE
        .iter()
        .position(|level| *level == current)
        .and_then(|index| BLIND_SCHEDULE.get(index + 1).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_starts_and_ends_as_expected() {
        assert_eq!(BLIND_SCHEDULE.len(), 20);
        assert_eq!(BLIND_SCHEDULE[0], BlindLevel::new(10, 20));
        assert_eq!(BLIND_SCHEDULE[19], BlindLevel::new(1000, 2000));
    }

    #[test]
    fn blinds_are_strictly_escalating() {
        for pair in BLIND_SCHEDULE.windows(2) {
            assert!(pair[1].small_blind > pair[0].small_blind);
            assert!(pair[1].big_blind > pair[0].big_blind);
        }
    }

    #[test]
    fn next_level_advances_and_terminates() {
        assert_eq!(
            next_level(BlindLevel::new(10, 20)),
            Some(BlindLevel::new(15, 30))
        );
        assert_eq!(
            next_level(BlindLevel::new(800, 1600)),
            Some(BlindLevel::new(1000, 2000))
        );
        assert_eq!(next_level(BlindLevel::new(1000, 2000)), None);
    }

    #[test]
    fn next_level_returns_none_for_unknown_levels() {
        assert_eq!(next_level(BlindLevel::new(13, 26)), None);
    }
}
