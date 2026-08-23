/// A player action. `Bet` and `Raise` amounts are the total chips the player
/// commits on the current street (a "raise to" amount), not the increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Fold,
    Check,
    Call,
    Bet(u32),
    Raise(u32),
    AllIn,
}

/// The set of legal actions for the current actor, with concrete chip amounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegalActions {
    pub can_fold: bool,
    pub can_check: bool,
    pub can_call: bool,
    pub call_amount: u32,
    pub can_bet: bool,
    pub min_bet: u32,
    pub max_bet: u32,
    pub can_raise: bool,
    pub min_raise_to: u32,
    pub max_raise_to: u32,
    pub can_all_in: bool,
}

impl LegalActions {
    /// Whether the given action is legal under this action set.
    pub fn allows(&self, action: Action) -> bool {
        match action {
            Action::Fold => self.can_fold,
            Action::Check => self.can_check,
            Action::Call => self.can_call,
            Action::Bet(amount) => self.can_bet && amount >= self.min_bet && amount <= self.max_bet,
            Action::Raise(amount) => {
                self.can_raise && amount >= self.min_raise_to && amount <= self.max_raise_to
            }
            Action::AllIn => self.can_all_in,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legal() -> LegalActions {
        LegalActions {
            can_fold: true,
            can_check: false,
            can_call: true,
            call_amount: 20,
            can_bet: false,
            min_bet: 0,
            max_bet: 0,
            can_raise: true,
            min_raise_to: 40,
            max_raise_to: 500,
            can_all_in: true,
        }
    }

    #[test]
    fn allows_matches_flags() {
        let l = legal();
        assert!(l.allows(Action::Fold));
        assert!(!l.allows(Action::Check));
        assert!(l.allows(Action::Call));
        assert!(!l.allows(Action::Bet(100)));
        assert!(l.allows(Action::AllIn));
    }

    #[test]
    fn bet_and_raise_respect_amount_bounds() {
        let l = legal();
        assert!(l.allows(Action::Raise(40)));
        assert!(l.allows(Action::Raise(500)));
        assert!(!l.allows(Action::Raise(39)));
        assert!(!l.allows(Action::Raise(501)));
    }

    #[test]
    fn bet_bounds_are_enforced() {
        let l = LegalActions {
            can_bet: true,
            min_bet: 20,
            max_bet: 100,
            ..legal()
        };
        assert!(l.allows(Action::Bet(20)));
        assert!(l.allows(Action::Bet(100)));
        assert!(!l.allows(Action::Bet(19)));
        assert!(!l.allows(Action::Bet(101)));
    }
}
