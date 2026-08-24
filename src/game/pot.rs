use crate::game::seat::Seat;

/// A pot (main or side) with the chips it contains and the seats eligible to
/// win it. A pot with a single eligible seat represents an uncalled bet that
/// is returned to that seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pot {
    pub amount: u32,
    pub eligible: Vec<Seat>,
}

/// Computes main and side pots from each seat's total contribution, fold
/// status, and elimination status. Contributions are matched level-by-level
/// so that all-in players only contest the portion of the pot they covered;
/// eliminated seats are never eligible for any pot.
pub fn compute_pots(
    contributions: &[u32; 3],
    folded: &[bool; 3],
    eliminated: &[bool; 3],
) -> Vec<Pot> {
    let mut levels: Vec<u32> = contributions
        .iter()
        .copied()
        .filter(|&amount| amount > 0)
        .collect();
    levels.sort_unstable();
    levels.dedup();

    let mut pots = Vec::new();
    let mut previous = 0u32;
    for &level in &levels {
        let mut amount = 0u32;
        let mut eligible = Vec::new();
        for seat in Seat::ALL {
            if contributions[seat.index()] >= level {
                amount += level - previous;
                if !folded[seat.index()] && !eliminated[seat.index()] {
                    eligible.push(seat);
                }
            }
        }
        if amount > 0 {
            pots.push(Pot { amount, eligible });
        }
        previous = level;
    }
    pots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_contributions_form_a_single_main_pot() {
        let pots = compute_pots(&[100, 100, 100], &[false, false, false], &[false; 3]);
        assert_eq!(
            pots,
            vec![Pot {
                amount: 300,
                eligible: vec![Seat::Hero, Seat::Opponent1, Seat::Opponent2],
            }]
        );
    }

    #[test]
    fn short_all_in_creates_a_side_pot() {
        let pots = compute_pots(&[100, 100, 50], &[false, false, false], &[false; 3]);
        assert_eq!(
            pots,
            vec![
                Pot {
                    amount: 150,
                    eligible: vec![Seat::Hero, Seat::Opponent1, Seat::Opponent2],
                },
                Pot {
                    amount: 100,
                    eligible: vec![Seat::Hero, Seat::Opponent1],
                },
            ]
        );
    }

    #[test]
    fn folded_players_are_excluded_from_eligibility() {
        let pots = compute_pots(&[100, 50, 50], &[false, false, true], &[false; 3]);
        assert_eq!(
            pots,
            vec![
                Pot {
                    amount: 150,
                    eligible: vec![Seat::Hero, Seat::Opponent1],
                },
                Pot {
                    amount: 50,
                    eligible: vec![Seat::Hero],
                },
            ]
        );
    }

    #[test]
    fn uncalled_bet_is_returned_as_single_eligible_pot() {
        let pots = compute_pots(&[100, 50, 0], &[false, false, true], &[false; 3]);
        assert_eq!(
            pots,
            vec![
                Pot {
                    amount: 100,
                    eligible: vec![Seat::Hero, Seat::Opponent1],
                },
                Pot {
                    amount: 50,
                    eligible: vec![Seat::Hero],
                },
            ]
        );
    }

    #[test]
    fn zero_contributions_produce_no_pots() {
        assert!(compute_pots(&[0, 0, 0], &[false, false, false], &[false; 3]).is_empty());
    }

    #[test]
    fn eliminated_players_are_never_eligible() {
        // Busted out of an earlier hand: contribution back-cast to keep the
        // scenario focused on eligibility, not realism.
        let pots = compute_pots(
            &[100, 100, 0],
            &[false, false, false],
            &[false, true, false],
        );
        assert_eq!(
            pots,
            vec![Pot {
                amount: 200,
                eligible: vec![Seat::Hero],
            }]
        );
    }

    #[test]
    fn pot_amounts_sum_to_total_contributions() {
        let cases = [
            ([100, 100, 100], [false, false, false]),
            ([100, 100, 50], [false, false, false]),
            ([100, 50, 50], [false, false, true]),
            ([100, 50, 0], [false, false, true]),
            ([500, 500, 500], [false, false, false]),
            ([10, 20, 30], [true, false, false]),
        ];
        for (contributions, folded) in cases {
            let total: u32 = contributions.iter().sum();
            let pots = compute_pots(&contributions, &folded, &[false; 3]);
            assert_eq!(pots.iter().map(|p| p.amount).sum::<u32>(), total);
        }
    }
}
