use crate::game::{Action, GameState, Seat, Street};

/// Number of opponent seats (everyone but the hero).
const NUM_OPPONENTS: usize = 2;

/// Completed hands needed before the read graduates from the small-sample
/// disclaimer to an actual profile.
const MIN_HANDS_FOR_READ: usize = 5;

/// One opponent's session stats plus a point-in-time table snapshot, ready to
/// render in the coach-feedback panel.
#[derive(Clone, Debug, PartialEq)]
pub struct OpponentSnapshot {
    pub seat: Seat,
    pub hands: usize,
    /// Voluntarily put money in preflop, as a share of hands dealt.
    pub vpip_pct: f64,
    /// Raised preflop, as a share of hands dealt.
    pub pfr_pct: f64,
    /// Folded when facing a bet or raise, as a share of bets faced.
    pub fold_to_bet_pct: f64,
    /// Postflop bets and raises (the aggression numerator).
    pub postflop_bets: usize,
    /// Postflop calls (the aggression denominator).
    pub postflop_calls: usize,
    /// A player-friendly one-liner describing the opponent so far.
    pub read: String,
    pub stack: u32,
    pub folded: bool,
    pub all_in: bool,
    pub is_button: bool,
    pub is_small_blind: bool,
    pub is_big_blind: bool,
}

/// Live per-opponent HUD counters, fed every time an opponent acts.
///
/// VPIP counts an opponent once per hand the first time they voluntarily
/// commit chips preflop (calling, raising, or going all-in); blinds and
/// checks are not voluntary. PFR counts preflop raises — an all-in only
/// counts when it is not a call-for-less, i.e. when no bet was faced.
/// Fold-to-bet counts folds across all streets whenever a bet or raise had
/// to be answered. Aggression is the postflop bets-plus-raises-per-call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct OpponentTracker {
    hands: [usize; NUM_OPPONENTS],
    vpip: [usize; NUM_OPPONENTS],
    pfr: [usize; NUM_OPPONENTS],
    faced_bet: [usize; NUM_OPPONENTS],
    folded_to_bet: [usize; NUM_OPPONENTS],
    postflop_bets: [usize; NUM_OPPONENTS],
    postflop_calls: [usize; NUM_OPPONENTS],
    vpip_seen: [bool; NUM_OPPONENTS],
    pfr_seen: [bool; NUM_OPPONENTS],
}

impl OpponentTracker {
    /// Serializes the counters for tournament persistence across reconnects.
    pub fn to_snapshot(&self) -> crate::snapshot::OpponentCountersSnapshot {
        crate::snapshot::OpponentCountersSnapshot {
            hands: self.hands,
            vpip: self.vpip,
            pfr: self.pfr,
            faced_bet: self.faced_bet,
            folded_to_bet: self.folded_to_bet,
            postflop_bets: self.postflop_bets,
            postflop_calls: self.postflop_calls,
            vpip_seen: self.vpip_seen,
            pfr_seen: self.pfr_seen,
        }
    }

    /// Restores the counters saved by [`Self::to_snapshot`].
    pub fn from_snapshot(snapshot: &crate::snapshot::OpponentCountersSnapshot) -> Self {
        Self {
            hands: snapshot.hands,
            vpip: snapshot.vpip,
            pfr: snapshot.pfr,
            faced_bet: snapshot.faced_bet,
            folded_to_bet: snapshot.folded_to_bet,
            postflop_bets: snapshot.postflop_bets,
            postflop_calls: snapshot.postflop_calls,
            vpip_seen: snapshot.vpip_seen,
            pfr_seen: snapshot.pfr_seen,
        }
    }

    /// A new hand was dealt: every seated opponent gets another hand, and the
    /// per-hand VPIP/PFR flags reset.
    pub fn begin_hand(&mut self) {
        for hands in &mut self.hands {
            *hands += 1;
        }
        self.vpip_seen = [false; NUM_OPPONENTS];
        self.pfr_seen = [false; NUM_OPPONENTS];
    }

    /// Records one opponent action with its context. Hero actions are
    /// ignored; `faced_bet` is true when the opponent had chips to call.
    pub fn record(&mut self, seat: Seat, action: Action, street: Street, faced_bet: bool) {
        let Some(index) = seat.index().checked_sub(1) else {
            return;
        };
        if index >= NUM_OPPONENTS {
            return;
        }

        if street == Street::Preflop {
            let voluntarily = matches!(action, Action::Call | Action::Raise(_) | Action::AllIn);
            if voluntarily && !self.vpip_seen[index] {
                self.vpip_seen[index] = true;
                self.vpip[index] += 1;
            }
            let raises =
                matches!(action, Action::Raise(_)) || (action == Action::AllIn && !faced_bet);
            if raises && !self.pfr_seen[index] {
                self.pfr_seen[index] = true;
                self.pfr[index] += 1;
            }
        }

        if faced_bet {
            self.faced_bet[index] += 1;
            if action == Action::Fold {
                self.folded_to_bet[index] += 1;
            }
        }

        if street != Street::Preflop {
            match action {
                Action::Bet(_) | Action::Raise(_) | Action::AllIn => {
                    self.postflop_bets[index] += 1;
                }
                Action::Call => self.postflop_calls[index] += 1,
                Action::Check | Action::Fold => {}
            }
        }
    }

    /// Snapshots both opponents against the current table state for display.
    pub fn snapshots(&self, state: &GameState) -> Vec<OpponentSnapshot> {
        let small_blind = state.small_blind_seat();
        let big_blind = state.big_blind_seat();
        [Seat::Opponent1, Seat::Opponent2]
            .iter()
            .map(|&seat| {
                let index = seat.index() - 1;
                let hands = self.hands[index];
                let vpip_pct = pct(self.vpip[index], hands);
                OpponentSnapshot {
                    seat,
                    hands,
                    vpip_pct,
                    pfr_pct: pct(self.pfr[index], hands),
                    fold_to_bet_pct: pct(self.folded_to_bet[index], self.faced_bet[index]),
                    postflop_bets: self.postflop_bets[index],
                    postflop_calls: self.postflop_calls[index],
                    read: read(
                        hands,
                        vpip_pct,
                        aggression(self.postflop_bets[index], self.postflop_calls[index]),
                    ),
                    stack: state.stack(seat),
                    folded: state.folded(seat),
                    all_in: state.all_in(seat),
                    is_button: state.button() == seat,
                    is_small_blind: small_blind == seat,
                    is_big_blind: big_blind == seat,
                }
            })
            .collect()
    }
}

/// A percentage with a zero-denominator guard.
fn pct(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

/// Postflop aggression: bets-plus-raises per call. `None` when there are no
/// postflop decisions at all; unbounded when an opponent only ever bet or
/// raised postflop and never called.
fn aggression(bets: usize, calls: usize) -> Option<f64> {
    match (bets, calls) {
        (0, 0) => None,
        (_, 0) => Some(f64::INFINITY),
        (bets, calls) => Some(bets as f64 / calls as f64),
    }
}

const AGGRESSIVE_AF: f64 = 2.0;
const PASSIVE_AF: f64 = 1.0;
const TIGHT_VPIP: f64 = 25.0;
const LOOSE_VPIP: f64 = 45.0;

/// The one-line player-friendly read of an opponent's play so far.
fn read(hands: usize, vpip_pct: f64, aggression: Option<f64>) -> String {
    if hands == 0 {
        return "No hands played yet.".to_string();
    }
    if hands < MIN_HANDS_FOR_READ {
        return format!(
            "Only {hands} hand{} so far — too early to profile.",
            if hands == 1 { "" } else { "s" }
        );
    }

    let tight = vpip_pct < TIGHT_VPIP;
    let loose = vpip_pct > LOOSE_VPIP;

    let Some(af) = aggression else {
        return if tight {
            "Tight preflop — plays few hands and decides most of them early.".to_string()
        } else if loose {
            "Loose preflop — plays lots of hands, but street play is still unmarked.".to_string()
        } else {
            "Average hand selection so far; not enough street play to judge yet.".to_string()
        };
    };

    let aggressive = af >= AGGRESSIVE_AF;
    let passive = af < PASSIVE_AF;
    if tight && aggressive {
        "Tight aggressive — selective hands, played hard.".to_string()
    } else if tight && passive {
        "Tight passive — plays few hands and rarely presses the gas.".to_string()
    } else if loose && aggressive {
        "Loose aggressive — in lots of pots and swinging.".to_string()
    } else if loose && passive {
        "Loose passive — plays many hands and calls instead of raising.".to_string()
    } else if tight {
        "Tight and measured — few hands with sensible sizing.".to_string()
    } else if loose {
        "Loose but controlled — wide range without overcommitting.".to_string()
    } else if aggressive {
        "Aggressive — pushes chips around on a normal range.".to_string()
    } else if passive {
        "Moderate but passive — sees flops and lets others drive.".to_string()
    } else {
        "Played a balanced game so far.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::blinds::BlindLevel;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn state() -> GameState {
        GameState::new(Seat::Opponent1, level())
    }

    #[test]
    fn hero_actions_are_ignored() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Hero, Action::Raise(60), Street::Preflop, false);
        tracker.record(Seat::Hero, Action::Bet(40), Street::Flop, false);
        let snapshots = tracker.snapshots(&state());
        assert!(snapshots.iter().all(|s| s.hands == 1 && s.vpip_pct == 0.0));
    }

    #[test]
    fn begin_hand_counts_both_opponents_and_resets_flags() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Call, Street::Preflop, true);
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Call, Street::Preflop, true);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].hands, 2);
        assert_eq!(snapshots[1].hands, 2);
        assert_eq!(snapshots[0].vpip_pct, 100.0);
        assert_eq!(snapshots[1].vpip_pct, 0.0);
    }

    #[test]
    fn vpip_counts_once_per_hand() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Call, Street::Preflop, true);
        tracker.record(Seat::Opponent1, Action::Raise(80), Street::Preflop, true);
        tracker.begin_hand();
        let snapshots = tracker.snapshots(&state());
        assert_eq!(
            snapshots[0].vpip_pct, 50.0,
            "two hands, one VPIP: a call-then-raise counts once"
        );
        assert_eq!(snapshots[0].pfr_pct, 50.0, "one of two hands was raised");
    }

    #[test]
    fn checks_blinds_and_folds_are_not_voluntary() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Check, Street::Preflop, false);
        tracker.record(Seat::Opponent2, Action::Fold, Street::Preflop, true);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].vpip_pct, 0.0);
        assert_eq!(snapshots[1].vpip_pct, 0.0);
    }

    #[test]
    fn preflop_all_in_raise_counts_pfr_only_without_a_bet_faced() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::AllIn, Street::Preflop, false);
        tracker.record(Seat::Opponent2, Action::AllIn, Street::Preflop, true);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].pfr_pct, 100.0, "open shove is an open raise");
        assert_eq!(snapshots[1].pfr_pct, 0.0, "call-for-less is not a raise");
        assert_eq!(snapshots[1].vpip_pct, 100.0, "but it is a voluntary commit");
    }

    #[test]
    fn fold_to_bet_tracks_all_streets() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Fold, Street::Flop, true);
        tracker.record(Seat::Opponent1, Action::Call, Street::Flop, true);
        tracker.record(Seat::Opponent1, Action::Check, Street::Flop, false);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].fold_to_bet_pct, 50.0);
    }

    #[test]
    fn postflop_aggression_separates_bets_from_calls() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        tracker.record(Seat::Opponent1, Action::Bet(40), Street::Flop, false);
        tracker.record(Seat::Opponent1, Action::Call, Street::Turn, true);
        tracker.record(Seat::Opponent1, Action::Check, Street::River, false);
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].postflop_bets, 1);
        assert_eq!(snapshots[0].postflop_calls, 1);
    }

    #[test]
    fn snapshots_carry_position_badges_and_table_status() {
        let mut tracker = OpponentTracker::default();
        tracker.begin_hand();
        let state = GameState::new(Seat::Opponent2, level());
        let snapshots = tracker.snapshots(&state);
        assert!(
            !snapshots[0].is_button && !snapshots[0].is_small_blind,
            "Opponent 1 sits left of the button and neither posts a blind"
        );
        assert!(
            !snapshots[0].is_big_blind,
            "the hero posts the big blind when Opponent 2 has the button"
        );
        assert!(snapshots[1].is_button && snapshots[1].is_small_blind);
        assert!(!snapshots[1].is_big_blind);
        assert_eq!(snapshots[0].stack, 500);
        assert!(!snapshots[0].folded && !snapshots[0].all_in);
    }

    #[test]
    fn snapshots_zero_divisions_stay_zero() {
        let tracker = OpponentTracker::default();
        let snapshots = tracker.snapshots(&state());
        assert_eq!(snapshots[0].fold_to_bet_pct, 0.0);
        assert_eq!(snapshots[0].hands, 0);
        assert_eq!(snapshots[0].read, "No hands played yet.");
    }

    #[test]
    fn aggression_is_none_infinite_or_a_ratio() {
        assert_eq!(aggression(0, 0), None);
        assert_eq!(aggression(3, 0), Some(f64::INFINITY));
        assert_eq!(aggression(3, 2), Some(1.5));
        assert_eq!(aggression(0, 4), Some(0.0));
    }

    #[test]
    fn read_keeps_small_samples_honest() {
        assert_eq!(read(0, 50.0, Some(1.5)), "No hands played yet.");
        assert_eq!(
            read(1, 100.0, Some(1.5)),
            "Only 1 hand so far — too early to profile."
        );
        assert_eq!(
            read(4, 100.0, Some(1.5)),
            "Only 4 hands so far — too early to profile."
        );
    }

    #[test]
    fn read_without_street_play_describes_preflop_only() {
        assert_eq!(
            read(10, 20.0, None),
            "Tight preflop — plays few hands and decides most of them early."
        );
        assert_eq!(
            read(10, 60.0, None),
            "Loose preflop — plays lots of hands, but street play is still unmarked."
        );
        assert_eq!(
            read(10, 35.0, None),
            "Average hand selection so far; not enough street play to judge yet."
        );
    }

    #[test]
    fn read_combines_looseness_and_aggression() {
        assert_eq!(
            read(10, 20.0, Some(3.0)),
            "Tight aggressive — selective hands, played hard."
        );
        assert_eq!(
            read(10, 20.0, Some(0.5)),
            "Tight passive — plays few hands and rarely presses the gas."
        );
        assert_eq!(
            read(10, 80.0, Some(3.0)),
            "Loose aggressive — in lots of pots and swinging."
        );
        assert_eq!(
            read(10, 80.0, Some(0.5)),
            "Loose passive — plays many hands and calls instead of raising."
        );
        assert_eq!(
            read(10, 20.0, Some(1.5)),
            "Tight and measured — few hands with sensible sizing."
        );
        assert_eq!(
            read(10, 80.0, Some(1.5)),
            "Loose but controlled — wide range without overcommitting."
        );
        assert_eq!(
            read(10, 35.0, Some(3.0)),
            "Aggressive — pushes chips around on a normal range."
        );
        assert_eq!(
            read(10, 35.0, Some(0.5)),
            "Moderate but passive — sees flops and lets others drive."
        );
        assert_eq!(read(10, 35.0, Some(1.5)), "Played a balanced game so far.");
    }

    #[test]
    fn read_thresholds_are_exclusive_at_the_boundaries() {
        assert_eq!(
            read(10, 25.0, Some(3.0)),
            read(10, 45.0, Some(3.0)),
            "25 and 45 VPIP read as neither tight nor loose"
        );
        assert_eq!(
            read(10, 35.0, Some(1.0)),
            "Played a balanced game so far.",
            "AF of exactly 1.0 reads as balanced, not passive"
        );
        assert_eq!(
            read(10, 35.0, Some(2.0)),
            "Aggressive — pushes chips around on a normal range.",
            "AF of exactly 2.0 reads as aggressive"
        );
    }

    #[test]
    fn read_only_overflows_without_postflop_calls() {
        assert_eq!(
            read(20, 35.0, Some(f64::INFINITY)),
            "Aggressive — pushes chips around on a normal range."
        );
        assert_eq!(
            read(20, 20.0, Some(f64::INFINITY)),
            "Tight aggressive — selective hands, played hard."
        );
        assert_eq!(
            read(20, 80.0, Some(f64::INFINITY)),
            "Loose aggressive — in lots of pots and swinging."
        );
    }
}
