use crate::game::Street;

/// A standardized bet/raise size bucket, matching GGPoker's shortcut buttons.
///
/// Preflop sizes are expressed as the raise-to amount in big blinds (2bb
/// min-raise, 3bb, 4bb, pot). Postflop sizes are expressed as a fraction of
/// the pot (1/3, 1/2, 3/4, pot, overbet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BetSize {
    /// Minimum legal raise/bet (preflop: 2bb min-raise; postflop: min-bet/min-raise).
    Min,
    /// Preflop open-raise to 3 big blinds.
    ThreeBb,
    /// Preflop open-raise to 4 big blinds.
    FourBb,
    /// Raise to double the call amount (GGPoker's default when facing a raise).
    TwoX,
    /// Postflop 1/3 pot.
    ThirdPot,
    /// Postflop 1/2 pot.
    HalfPot,
    /// Postflop 3/4 pot.
    ThreeQuarterPot,
    /// Pot-sized bet/raise.
    Pot,
    /// Larger than a pot-sized bet/raise (postflop).
    Overbet,
    /// All-in.
    AllIn,
}

impl BetSize {
    /// Classifies a bet/raise into a bucket.
    ///
    /// `raise_to` is the total chips committed on the current street (a
    /// "raise to" amount). `pot` is the pot before the action, `to_call` the
    /// chips needed to call (0 for a bet), `big_blind` the current big blind,
    /// `min_amount` the minimum legal raise-to (or bet) amount, and `stack`
    /// the actor's stack before the action.
    pub fn classify(
        street: Street,
        raise_to: u32,
        pot: u32,
        to_call: u32,
        big_blind: u32,
        min_amount: u32,
        stack: u32,
    ) -> BetSize {
        if raise_to >= stack {
            return BetSize::AllIn;
        }
        if raise_to <= min_amount {
            return BetSize::Min;
        }
        match street {
            Street::Preflop => {
                let bb = raise_to as f64 / big_blind.max(1) as f64;
                if bb <= 3.5 {
                    BetSize::ThreeBb
                } else if bb <= 4.5 {
                    BetSize::FourBb
                } else {
                    BetSize::Pot
                }
            }
            _ => {
                let increment = raise_to.saturating_sub(to_call);
                let pot_after_call = pot + to_call;
                let fraction = increment as f64 / pot_after_call.max(1) as f64;
                if fraction <= 0.416 {
                    BetSize::ThirdPot
                } else if fraction <= 0.625 {
                    BetSize::HalfPot
                } else if fraction <= 0.875 {
                    BetSize::ThreeQuarterPot
                } else if fraction <= 1.25 {
                    BetSize::Pot
                } else {
                    BetSize::Overbet
                }
            }
        }
    }

    /// Converts a bucket back into a concrete raise-to amount.
    ///
    /// Preflop BB-multiple buckets are absolute raise-to amounts (correct for
    /// open-raises); re-raise sizing is refined by the solver. The result is
    /// clamped to `[min_amount, stack]`.
    pub fn to_raise_to(
        self,
        pot: u32,
        to_call: u32,
        big_blind: u32,
        min_amount: u32,
        stack: u32,
    ) -> u32 {
        let amount = match self {
            BetSize::Min => min_amount,
            BetSize::ThreeBb => 3 * big_blind,
            BetSize::FourBb => 4 * big_blind,
            BetSize::TwoX => 2 * to_call,
            BetSize::ThirdPot => to_call + (pot + to_call) / 3,
            BetSize::HalfPot => to_call + (pot + to_call) / 2,
            BetSize::ThreeQuarterPot => to_call + 3 * (pot + to_call) / 4,
            BetSize::Pot => to_call + (pot + to_call),
            BetSize::Overbet => to_call + 2 * (pot + to_call),
            BetSize::AllIn => stack,
        };
        amount.clamp(min_amount.min(stack), stack)
    }

    /// The label used in abstracted sequence nodes.
    pub fn label(self) -> &'static str {
        match self {
            BetSize::Min => "MIN",
            BetSize::ThreeBb => "3BB",
            BetSize::FourBb => "4BB",
            BetSize::TwoX => "2X",
            BetSize::ThirdPot => "1/3POT",
            BetSize::HalfPot => "1/2POT",
            BetSize::ThreeQuarterPot => "3/4POT",
            BetSize::Pot => "POT",
            BetSize::Overbet => "OVERBET",
            BetSize::AllIn => "ALLIN",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflop_classifies_by_big_blind_multiples() {
        // Blinds 10/20, button open-raise: min = 40 (2bb).
        let (pot, to_call, bb, min, stack) = (30, 20, 20, 40, 500);
        assert_eq!(
            BetSize::classify(Street::Preflop, 40, pot, to_call, bb, min, stack),
            BetSize::Min
        );
        assert_eq!(
            BetSize::classify(Street::Preflop, 60, pot, to_call, bb, min, stack),
            BetSize::ThreeBb
        );
        assert_eq!(
            BetSize::classify(Street::Preflop, 80, pot, to_call, bb, min, stack),
            BetSize::FourBb
        );
        assert_eq!(
            BetSize::classify(Street::Preflop, 100, pot, to_call, bb, min, stack),
            BetSize::Pot
        );
        assert_eq!(
            BetSize::classify(Street::Preflop, 500, pot, to_call, bb, min, stack),
            BetSize::AllIn
        );
    }

    #[test]
    fn postflop_classifies_by_pot_fraction() {
        // Pot 100, no bet to call, min bet 20.
        let (pot, to_call, bb, min, stack) = (100, 0, 20, 20, 500);
        assert_eq!(
            BetSize::classify(Street::Flop, 20, pot, to_call, bb, min, stack),
            BetSize::Min
        );
        assert_eq!(
            BetSize::classify(Street::Flop, 33, pot, to_call, bb, min, stack),
            BetSize::ThirdPot
        );
        assert_eq!(
            BetSize::classify(Street::Flop, 50, pot, to_call, bb, min, stack),
            BetSize::HalfPot
        );
        assert_eq!(
            BetSize::classify(Street::Flop, 75, pot, to_call, bb, min, stack),
            BetSize::ThreeQuarterPot
        );
        assert_eq!(
            BetSize::classify(Street::Flop, 100, pot, to_call, bb, min, stack),
            BetSize::Pot
        );
        assert_eq!(
            BetSize::classify(Street::Flop, 150, pot, to_call, bb, min, stack),
            BetSize::Overbet
        );
        assert_eq!(
            BetSize::classify(Street::Flop, 500, pot, to_call, bb, min, stack),
            BetSize::AllIn
        );
    }

    #[test]
    fn postflop_raise_sizes_relative_to_pot_after_call() {
        // Pot 100, facing a 50 bet (to_call 50), min raise-to 100.
        let (pot, to_call, bb, min, stack) = (100, 50, 20, 100, 500);
        // Half-pot raise: 50 + 150/2 = 125.
        assert_eq!(
            BetSize::classify(Street::Flop, 125, pot, to_call, bb, min, stack),
            BetSize::HalfPot
        );
        // Pot-sized raise: 50 + 150 = 200.
        assert_eq!(
            BetSize::classify(Street::Flop, 200, pot, to_call, bb, min, stack),
            BetSize::Pot
        );
    }

    #[test]
    fn to_raise_to_round_trips_postflop_buckets() {
        let (pot, to_call, bb, min, stack) = (100, 0, 20, 20, 500);
        assert_eq!(BetSize::Min.to_raise_to(pot, to_call, bb, min, stack), 20);
        assert_eq!(
            BetSize::HalfPot.to_raise_to(pot, to_call, bb, min, stack),
            50
        );
        assert_eq!(
            BetSize::ThreeQuarterPot.to_raise_to(pot, to_call, bb, min, stack),
            75
        );
        assert_eq!(BetSize::Pot.to_raise_to(pot, to_call, bb, min, stack), 100);
        assert_eq!(
            BetSize::Overbet.to_raise_to(pot, to_call, bb, min, stack),
            200
        );
        assert_eq!(
            BetSize::AllIn.to_raise_to(pot, to_call, bb, min, stack),
            500
        );
    }

    #[test]
    fn to_raise_to_preflop_uses_big_blind() {
        let (pot, to_call, bb, min, stack) = (30, 20, 20, 40, 500);
        assert_eq!(
            BetSize::ThreeBb.to_raise_to(pot, to_call, bb, min, stack),
            60
        );
        assert_eq!(
            BetSize::FourBb.to_raise_to(pot, to_call, bb, min, stack),
            80
        );
    }

    #[test]
    fn to_raise_to_two_x_doubles_the_call() {
        let (pot, to_call, bb, min, stack) = (100, 70, 20, 140, 500);
        assert_eq!(BetSize::TwoX.to_raise_to(pot, to_call, bb, min, stack), 140);
    }

    #[test]
    fn to_raise_to_clamps_to_stack() {
        let (pot, to_call, bb, min, stack) = (100, 0, 20, 20, 60);
        assert_eq!(BetSize::Pot.to_raise_to(pot, to_call, bb, min, stack), 60);
        assert_eq!(
            BetSize::Overbet.to_raise_to(pot, to_call, bb, min, stack),
            60
        );
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(BetSize::Min.label(), "MIN");
        assert_eq!(BetSize::ThreeBb.label(), "3BB");
        assert_eq!(BetSize::FourBb.label(), "4BB");
        assert_eq!(BetSize::TwoX.label(), "2X");
        assert_eq!(BetSize::ThirdPot.label(), "1/3POT");
        assert_eq!(BetSize::HalfPot.label(), "1/2POT");
        assert_eq!(BetSize::ThreeQuarterPot.label(), "3/4POT");
        assert_eq!(BetSize::Pot.label(), "POT");
        assert_eq!(BetSize::Overbet.label(), "OVERBET");
        assert_eq!(BetSize::AllIn.label(), "ALLIN");
    }
}
