use std::fmt;

/// The three seats at a 3-max Spin and Gold table. `Hero` sits bottom-center,
/// `Opponent1` top-left, and `Opponent2` top-right. Clockwise order is
/// `Hero -> Opponent1 -> Opponent2 -> Hero`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seat {
    Hero,
    Opponent1,
    Opponent2,
}

impl Seat {
    pub const ALL: [Seat; 3] = [Seat::Hero, Seat::Opponent1, Seat::Opponent2];

    /// The next seat clockwise.
    pub fn next(self) -> Seat {
        match self {
            Seat::Hero => Seat::Opponent1,
            Seat::Opponent1 => Seat::Opponent2,
            Seat::Opponent2 => Seat::Hero,
        }
    }

    /// The previous seat clockwise.
    pub fn prev(self) -> Seat {
        match self {
            Seat::Hero => Seat::Opponent2,
            Seat::Opponent1 => Seat::Hero,
            Seat::Opponent2 => Seat::Opponent1,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Seat::Hero => 0,
            Seat::Opponent1 => 1,
            Seat::Opponent2 => 2,
        }
    }
}

impl fmt::Display for Seat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Seat::Hero => "Hero",
            Seat::Opponent1 => "Opponent 1",
            Seat::Opponent2 => "Opponent 2",
        };
        f.write_str(name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Street {
    Preflop,
    Flop,
    Turn,
    River,
}

impl Street {
    pub const ALL: [Street; 4] = [Street::Preflop, Street::Flop, Street::Turn, Street::River];

    pub fn next(self) -> Option<Street> {
        match self {
            Street::Preflop => Some(Street::Flop),
            Street::Flop => Some(Street::Turn),
            Street::Turn => Some(Street::River),
            Street::River => None,
        }
    }
}

impl fmt::Display for Street {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Street::Preflop => "Preflop",
            Street::Flop => "Flop",
            Street::Turn => "Turn",
            Street::River => "River",
        };
        f.write_str(name)
    }
}

/// Returns the seats in acting order for the given street.
///
/// In 3-max the button is the small blind. Preflop the player left of the big
/// blind acts first, then the button, then the big blind. Postflop the player
/// left of the button acts first.
///
/// `big_blind` is passed in rather than derived from `button.next()` because
/// heads-up (one seat eliminated) the physically adjacent seat isn't
/// necessarily the seat actually posting the big blind — the caller must
/// resolve that with elimination-aware lookup (`GameState::big_blind_seat`).
pub fn action_order(button: Seat, big_blind: Seat, street: Street) -> [Seat; 3] {
    let third = Seat::ALL
        .into_iter()
        .find(|&seat| seat != button && seat != big_blind)
        .unwrap_or(button);
    match street {
        Street::Preflop => [third, button, big_blind],
        _ => [big_blind, third, button],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_and_prev_round_trip() {
        for seat in Seat::ALL {
            assert_eq!(seat.next().prev(), seat);
            assert_eq!(seat.prev().next(), seat);
        }
    }

    #[test]
    fn next_cycles_through_all_seats() {
        assert_eq!(Seat::Hero.next(), Seat::Opponent1);
        assert_eq!(Seat::Opponent1.next(), Seat::Opponent2);
        assert_eq!(Seat::Opponent2.next(), Seat::Hero);
    }

    #[test]
    fn index_matches_enum_order() {
        assert_eq!(Seat::Hero.index(), 0);
        assert_eq!(Seat::Opponent1.index(), 1);
        assert_eq!(Seat::Opponent2.index(), 2);
    }

    #[test]
    fn street_progression() {
        assert_eq!(Street::Preflop.next(), Some(Street::Flop));
        assert_eq!(Street::Flop.next(), Some(Street::Turn));
        assert_eq!(Street::Turn.next(), Some(Street::River));
        assert_eq!(Street::River.next(), None);
    }

    #[test]
    fn preflop_order_is_left_of_bb_then_button_then_bb() {
        assert_eq!(
            action_order(Seat::Hero, Seat::Opponent1, Street::Preflop),
            [Seat::Opponent2, Seat::Hero, Seat::Opponent1]
        );
        assert_eq!(
            action_order(Seat::Opponent1, Seat::Opponent2, Street::Preflop),
            [Seat::Hero, Seat::Opponent1, Seat::Opponent2]
        );
        assert_eq!(
            action_order(Seat::Opponent2, Seat::Hero, Street::Preflop),
            [Seat::Opponent1, Seat::Opponent2, Seat::Hero]
        );
    }

    #[test]
    fn postflop_order_is_left_of_button_first() {
        assert_eq!(
            action_order(Seat::Hero, Seat::Opponent1, Street::Flop),
            [Seat::Opponent1, Seat::Opponent2, Seat::Hero]
        );
        assert_eq!(
            action_order(Seat::Opponent1, Seat::Opponent2, Street::Flop),
            [Seat::Opponent2, Seat::Hero, Seat::Opponent1]
        );
        assert_eq!(
            action_order(Seat::Opponent2, Seat::Hero, Street::Flop),
            [Seat::Hero, Seat::Opponent1, Seat::Opponent2]
        );
    }

    #[test]
    fn heads_up_preflop_order_skips_the_eliminated_seat() {
        // Opponent1 busted; button (Hero, the SB) must act before the real
        // big blind (Opponent2), not the physically-adjacent eliminated seat.
        assert_eq!(
            action_order(Seat::Hero, Seat::Opponent2, Street::Preflop),
            [Seat::Opponent1, Seat::Hero, Seat::Opponent2]
        );
    }

    #[test]
    fn heads_up_postflop_order_has_big_blind_first() {
        assert_eq!(
            action_order(Seat::Hero, Seat::Opponent2, Street::Flop),
            [Seat::Opponent2, Seat::Opponent1, Seat::Hero]
        );
    }

    #[test]
    fn display_names() {
        assert_eq!(Seat::Hero.to_string(), "Hero");
        assert_eq!(Seat::Opponent1.to_string(), "Opponent 1");
        assert_eq!(Seat::Opponent2.to_string(), "Opponent 2");
        assert_eq!(Street::Preflop.to_string(), "Preflop");
        assert_eq!(Street::River.to_string(), "River");
    }
}
