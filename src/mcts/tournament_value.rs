//! Disposable experiment (not wired into production solving): estimates a
//! 3-max winner-take-all tournament state-value function
//!
//! ```text
//! V(hero, v1, blind_level) ~= P(hero eventually wins the tournament)
//! ```
//!
//! via Monte Carlo simulation of future hands under a frozen, non-circular
//! reference policy, then compares candidate-action rankings under
//! `V(resulting) - V(current)` against the current production reward
//! (`hero_stack_after - hero_stack_before`). Run it with:
//! `cargo test --release --lib -- --ignored --nocapture tournament_value_experiment`
//!
//! **Conclusion (2026-08-27): NO-GO.** Across 15 scripted premium-hand
//! decisions (pocket aces facing an all-in, at chip-leader through
//! very-short depths, at three blind levels) the table-based valuation never
//! changed the preferred action versus today's linear chip-EV reward
//! (0% flip rate) and never reproduced the old bust-risk over-folding bug.
//! The one place theory predicts a real difference — otherwise-identical
//! chip shares with a different number of live opponents (elimination
//! collapsing the field vs. a 3-way split) — showed only a small gap
//! (|ΔV| ≈ 0.004–0.018), consistent with this being a genuinely
//! winner-take-all format with no min-cash bubble to protect: the usual
//! ICM-style risk-aversion pressure most poker intuition assumes just isn't
//! present here, so replacing the reward buys little for a meaningful rise
//! in complexity (an offline table needing regeneration whenever the
//! reference policy or blind structure changes). Production's linear
//! chip-EV reward in `rollout::pay_off`/`tree::leaf_payoff` was left as-is.
//! This module is kept as a validated, rerunnable artifact in case that
//! changes (e.g. a real payout structure with placed prizes, where ICM-style
//! curvature is known to matter far more).
//!
//! Kept deliberately separate from `rollout.rs`/`tree.rs`: the whole point is
//! that this reference policy must never depend on, or feed back into, the
//! live MCTS being evaluated — see `play_hand`'s doc comment for the one
//! subtlety that required routing around `rollout.rs` rather than reusing it
//! directly.

use std::collections::HashMap;

use rand::Rng;
use rand::seq::SliceRandom;

use crate::card::{Card, Deck, Rank, Suit};
use crate::error::Result;
use crate::game::{Action, ActionOutcome, BLIND_SCHEDULE, GameState, NUM_PLAYERS, STARTING_STACK, Seat};
use crate::rng::weighted_index;

use super::actions::candidates;
use super::rollout::{hero_probs, opponent_probs, rollout, step};

/// Fixed chip total in play for the whole tournament (no rake): three
/// starting stacks. `V` is only ever evaluated on triples that sum to this.
const TOTAL_CHIPS: u32 = STARTING_STACK * 3;
/// Bucket width for the offline table's two free stack dimensions.
const STACK_STEP: u32 = 50;
/// Blind levels (`BLIND_SCHEDULE` indices) the experiment evaluates: an early,
/// mid, and shove/fold-depth level.
const LEVEL_INDICES: [usize; 3] = [0, 4, 8];
/// Simulated hands per blind level, standing in for the real 3-minute clock
/// (which this offline simulation has no use for directly).
const HANDS_PER_LEVEL: u32 = 3;
/// Safety valve bounding a pathological trial's length.
const MAX_HANDS_PER_TRIAL: u32 = 60;

/// The offline value table: `P(hero wins)` indexed by
/// `(blind_level_index, hero_bucket, v1_bucket)` — `v2` is implied by the
/// invariant chip total.
pub(crate) struct ValueTable {
    entries: HashMap<(usize, u32, u32), f64>,
}

fn round_bucket(chips: u32) -> u32 {
    let rounded = (chips + STACK_STEP / 2) / STACK_STEP * STACK_STEP;
    rounded.min(TOTAL_CHIPS)
}

impl ValueTable {
    /// Nearest-bucket lookup, clamped so `(hero_bucket, v1_bucket)` never
    /// implies a negative `v2`.
    pub(crate) fn value(&self, hero: u32, v1: u32, level_index: usize) -> f64 {
        let hero_b = round_bucket(hero.min(TOTAL_CHIPS));
        let v1_b = round_bucket(v1.min(TOTAL_CHIPS - hero_b));
        self.entries
            .get(&(level_index, hero_b, v1_b))
            .copied()
            .unwrap_or_else(|| f64::from(hero) / f64::from(TOTAL_CHIPS))
    }
}

/// Plays one seat's decision using a frozen, non-circular reference policy.
///
/// Three-way hands use the range-aware `hero_probs` (rotated so the acting
/// seat plays the hero role). Heads-up hands fall back to the seat-agnostic
/// `opponent_probs` instead: `hero_probs`/`relative_strength` compares the
/// acting seat's hand against *both* `[Opponent1, Opponent2]` slots by
/// checking only `folded` (`start_hand` resets `folded` for every seat,
/// including eliminated ones, at the top of every hand) — never `eliminated`.
/// In a genuinely heads-up trial that would silently compare against a
/// never-dealt, eliminated seat's stale placeholder hole cards. Routed around
/// here rather than patched in `rollout.rs`, since production never actually
/// exercises this combination today (a fresh `WorldSampler`/search is only
/// ever built for the seats actually live in that specific hand); flagged as
/// a discovered but out-of-scope latent gap, not fixed as part of this
/// experiment.
fn play_hand<R: Rng + ?Sized>(rng: &mut R, state: &mut GameState, deck: &mut Deck) -> Result<()> {
    while !state.is_hand_over() {
        let seat = state.to_act();
        let (cands, probs) = if state.active_seats().len() >= 3 {
            let rotated = state.rotated(seat);
            (candidates(&rotated), hero_probs(&rotated))
        } else {
            (candidates(state), opponent_probs(state))
        };
        if cands.is_empty() {
            break;
        }
        let weights: Vec<f32> = probs.iter().map(|&p| p as f32).collect();
        let index = weighted_index(rng, &weights).unwrap_or(0);
        let action = cands[index].0;
        match state.apply_action(action)? {
            ActionOutcome::Continue | ActionOutcome::HandEnded => {}
            ActionOutcome::StreetEnded => {
                if state.can_continue_betting() && state.street().next().is_some() {
                    state.advance_street(deck)?;
                } else if !state.is_hand_over() {
                    state.showdown(deck)?;
                }
            }
        }
    }
    Ok(())
}

/// Plays one full tournament trial from `(hero0, v10, v20)` at `level_index`
/// to a single winner (or the `MAX_HANDS_PER_TRIAL` safety valve), returning
/// the winning seat.
fn simulate_trial<R: Rng + ?Sized>(
    rng: &mut R,
    hero0: u32,
    v10: u32,
    v20: u32,
    level_index: usize,
) -> Result<Seat> {
    let mut state = GameState::new(Seat::Hero, BLIND_SCHEDULE[level_index]);
    state.set_stack(Seat::Hero, hero0);
    state.set_stack(Seat::Opponent1, v10);
    state.set_stack(Seat::Opponent2, v20);
    for seat in Seat::ALL {
        if state.stack(seat) == 0 {
            state.set_eliminated(seat, true);
        }
    }

    let mut deck = Deck::shuffled(rng);
    state.start_hand(&mut deck)?;

    let mut hands = 0u32;
    loop {
        play_hand(rng, &mut state, &mut deck)?;
        hands += 1;
        for seat in Seat::ALL {
            if state.stack(seat) == 0 && !state.eliminated(seat) {
                state.set_eliminated(seat, true);
            }
        }
        if let Some(winner) = state.tournament_winner() {
            return Ok(winner);
        }
        if hands >= MAX_HANDS_PER_TRIAL {
            let active = state.active_seats();
            return Ok(*active
                .iter()
                .max_by_key(|&&seat| state.stack(seat))
                .expect("at least two active seats remain"));
        }
        if hands % HANDS_PER_LEVEL == 0 {
            state.advance_blind_level();
        }
        deck = Deck::shuffled(rng);
        state.next_hand(&mut deck)?;
    }
}

/// Monte Carlo estimate of `P(hero wins)` from `(hero, v1, v2)` at
/// `level_index`, over `trials` independent tournament simulations.
fn estimate_v<R: Rng + ?Sized>(
    rng: &mut R,
    hero: u32,
    v1: u32,
    v2: u32,
    level_index: usize,
    trials: u32,
) -> f64 {
    let mut wins = 0u32;
    for _ in 0..trials {
        if let Ok(winner) = simulate_trial(rng, hero, v1, v2, level_index)
            && winner == Seat::Hero
        {
            wins += 1;
        }
    }
    f64::from(wins) / f64::from(trials)
}

/// Builds the offline value table across the full bucket grid. `hero == 0`
/// and `v1 == v2 == 0` are resolved exactly (0.0 / 1.0) rather than
/// simulated, since both are definitionally certain outcomes and simulating
/// them would only spend trials to reproduce a known answer with noise.
pub(crate) fn build_table<R: Rng + ?Sized>(rng: &mut R, trials: u32) -> ValueTable {
    let mut entries = HashMap::new();
    for &level_index in &LEVEL_INDICES {
        let mut hero = 0u32;
        while hero <= TOTAL_CHIPS {
            let mut v1 = 0u32;
            while v1 + hero <= TOTAL_CHIPS {
                let v2 = TOTAL_CHIPS - hero - v1;
                let value = if hero == 0 {
                    0.0
                } else if v1 == 0 && v2 == 0 {
                    1.0
                } else {
                    estimate_v(rng, hero, v1, v2, level_index, trials)
                };
                entries.insert((level_index, hero, v1), value);
                v1 += STACK_STEP;
            }
            hero += STACK_STEP;
        }
    }
    ValueTable { entries }
}

// ---------------------------------------------------------------- scenarios

/// A hero-to-act decision with pocket aces facing a single opponent's
/// all-in (the other opponent forced to fold), built from a custom deck so
/// hero's forced hand can never collide with a card already dealt elsewhere
/// — the same technique `mcts::mod::tests::deck_with` uses for river spots,
/// applied preflop here. Opponents' hands are otherwise genuinely random
/// per call, so this is a real distribution of "villain shoves preflop",
/// not one fixed hand.
fn premium_vs_shove<R: Rng + ?Sized>(
    rng: &mut R,
    hero_stack: u32,
    shover_stack: u32,
    other_stack: u32,
    level_index: usize,
) -> Result<(GameState, Vec<Card>)> {
    let aces = [
        Card::new(Rank::Ace, Suit::Spades),
        Card::new(Rank::Ace, Suit::Hearts),
    ];
    let mut rest: Vec<Card> = Suit::ALL
        .into_iter()
        .flat_map(|suit| Rank::ALL.into_iter().map(move |rank| Card::new(rank, suit)))
        .filter(|card| !aces.contains(card))
        .collect();
    rest.shuffle(rng);
    // Deal order (see GameState::start_hand): button.next() first, i.e.
    // Opponent1, then Opponent2, then Hero (button) last — so the first four
    // cards go to the opponents and hero's two slots are forced to aces.
    let mut ordered = Vec::with_capacity(52);
    ordered.extend_from_slice(&rest[0..4]);
    ordered.extend_from_slice(&aces);
    ordered.extend_from_slice(&rest[4..]);

    let mut state = GameState::new(Seat::Hero, BLIND_SCHEDULE[level_index]);
    state.set_stack(Seat::Hero, hero_stack);
    state.set_stack(Seat::Opponent1, shover_stack);
    state.set_stack(Seat::Opponent2, other_stack);
    let mut deck = Deck::try_from_remaining(ordered)
        .ok_or_else(|| crate::error::Error::Solver("bad experiment deck".into()))?;
    state.start_hand(&mut deck)?;
    debug_assert_eq!(state.hero_cards(), aces, "deal order assumption held");

    // Degenerate case: hero's stack is so short relative to this blind level
    // that posting the blind alone already puts them all-in. Hero then has
    // no decision left to make this hand at all — `to_act()` can never reach
    // Hero (an all-in seat is skipped by `advance_to_act`), so forcing the
    // opponents to act would spin forever. Bail out instead; the caller
    // treats this as "not a valid decision point to compare" and skips it.
    if state.all_in(Seat::Hero) {
        return Err(crate::error::Error::Solver(
            "hero is already all-in from the blind post; no decision to compare".into(),
        ));
    }

    let mut acted = 0u32;
    while state.to_act() != Seat::Hero {
        if acted > NUM_PLAYERS as u32 {
            return Err(crate::error::Error::Solver(
                "forcing loop could not reach a hero decision".into(),
            ));
        }
        let legal = state.legal_actions();
        let action = if acted == 0 && legal.can_all_in {
            Action::AllIn
        } else if legal.can_fold {
            Action::Fold
        } else {
            Action::Check
        };
        acted += 1;
        state.apply_action(action)?;
    }
    let runout = deck.remaining_in_order();
    Ok((state, runout))
}

/// Per-candidate-action EV under both valuations, averaged over `samples`
/// paired rollouts (same simulated hand outcome scores both valuations, so
/// the comparison isn't muddied by independent sampling noise).
struct ActionComparison {
    action: Action,
    chip_ev: f64,
    table_ev: f64,
}

/// Compares candidate actions across `samples` *independent worlds* — a
/// fresh opponent deal and board for every sample, each replaying the same
/// scripted "one opponent shoves, hero decides" script.
///
/// Earlier draft of this function reused one `premium_vs_shove` state/deal
/// across every sample: since this scenario is already fully committed at
/// the decision point (hero's own candidate action is the last real
/// decision — see `actions=1` in the by-hand diagnostic), there was no
/// residual randomness left to average over within a fixed deal, so
/// `samples` only bought (zero) extra precision on top of whatever one
/// random deal happened to produce — occasionally a wildly unrepresentative
/// number, exactly what the "very short" premium-hand row showed. Redrawing
/// per sample (still paired: one deal scores every candidate action) fixes
/// this and matches how production `WorldSearch` actually gets its EV
/// (averaged across many sampled worlds, not one).
fn compare_actions_over_worlds<R: Rng + ?Sized>(
    rng: &mut R,
    hero_stack: u32,
    shover_stack: u32,
    other_stack: u32,
    table: &ValueTable,
    level_index: usize,
    samples: u32,
) -> Result<Vec<ActionComparison>> {
    // (action, chip_sum, table_sum, count) — a plain Vec since `Action`
    // isn't `Hash`; at most a handful of candidates, so linear lookup is
    // fine.
    let mut sums: Vec<(Action, f64, f64, u32)> = Vec::new();
    let mut skipped = 0u32;

    for _ in 0..samples {
        let (state, runout) =
            match premium_vs_shove(rng, hero_stack, shover_stack, other_stack, level_index) {
                Ok(built) => built,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
        let baseline = state.stack(Seat::Hero);
        let baseline_v = table.value(
            state.stack(Seat::Hero),
            state.stack(Seat::Opponent1),
            level_index,
        );
        let cards = [Seat::Hero, Seat::Opponent1, Seat::Opponent2]
            .map(|seat| state.hole_cards(seat).unwrap_or([Card::new(Rank::Two, Suit::Clubs); 2]));

        for (action, _bucket) in candidates(&state) {
            let mut trial = state.clone_with_hole_cards(cards);
            let offset = step(&mut trial, action, &runout, 0)?;
            let (payoff, _) = rollout(rng, &mut trial, &runout, offset, baseline)?;
            let stacks = trial.stacks();
            let table_delta = table.value(
                stacks[Seat::Hero.index()],
                stacks[Seat::Opponent1.index()],
                level_index,
            ) - baseline_v;

            if let Some(entry) = sums.iter_mut().find(|(a, ..)| *a == action) {
                entry.1 += payoff.value;
                entry.2 += table_delta;
                entry.3 += 1;
            } else {
                sums.push((action, payoff.value, table_delta, 1));
            }
        }
    }
    if skipped > 0 {
        println!("    ({skipped}/{samples} worlds skipped: hero already all-in from the blind post)");
    }
    Ok(sums
        .into_iter()
        .map(|(action, chip_sum, table_sum, count)| ActionComparison {
            action,
            chip_ev: chip_sum / f64::from(count.max(1)),
            table_ev: table_sum / f64::from(count.max(1)),
        })
        .collect())
}

#[cfg(test)]
mod experiment {
    use super::*;
    use crate::rng::seeded_rng;

    const TABLE_TRIALS: u32 = 2000;
    const RANKING_SAMPLES: u32 = 1000;

    fn share(hero: u32) -> f64 {
        f64::from(hero) / f64::from(TOTAL_CHIPS)
    }

    /// The full experiment: builds the table, runs the §10-style sanity
    /// checks, prints the §6 diagnostic surface, runs the §7/§8 ranking
    /// comparisons (including the premium-hand regression check), computes
    /// the §9 effect-size metrics, and prints a §11 go/no-go verdict.
    ///
    /// Heavy (Monte Carlo over ~165 buckets); run explicitly:
    /// `cargo test --release -- --ignored --nocapture tournament_value_experiment`
    #[test]
    #[ignore = "heavy disposable Monte Carlo experiment; run explicitly"]
    fn tournament_value_experiment() {
        let mut rng = seeded_rng(2026_08_27);
        let table = build_table(&mut rng, TABLE_TRIALS);

        // ---- §10 sanity checks -------------------------------------------
        for &level_index in &LEVEL_INDICES {
            assert_eq!(table.value(0, 300, level_index), 0.0, "V(bust) must be 0");
            assert!(
                (table.value(TOTAL_CHIPS, 0, level_index) - 1.0).abs() < 1e-9,
                "V(all chips) must be 1"
            );
            let v = table.value(300, 300, level_index);
            assert!((0.0..=1.0).contains(&v), "V out of [0,1]: {v}");

            // Opponent-swap check: both opponent seats run the identical
            // policy, so V(hero,v1,v2) and V(hero,v2,v1) should be *close*,
            // not exactly equal — button/blind-post order is fixed to seat
            // identity, not to stack size, so a small residual gap here is a
            // genuine (if minor) positional effect, not necessarily a bug.
            let a = table.value(300, 200, level_index);
            let b = table.value(300, 400, level_index); // v1=400 <=> v2=200 here
            println!(
                "[sanity] level {level_index}: V(300,200,400)={a:.3} V(300,400,200)={b:.3} |Δ|={:.3}",
                (a - b).abs()
            );

            // Monotonicity: increasing hero's stack (holding v1 fixed, so v2
            // shrinks) must not decrease V.
            let low = table.value(200, 300, level_index);
            let high = table.value(500, 300, level_index);
            assert!(
                high >= low - 0.05,
                "monotonicity violated at level {level_index}: V(200,300)={low:.3} > V(500,300)={high:.3}"
            );
        }
        println!("[sanity] all checks passed\n");

        // ---- §6 diagnostics: V vs plain chip share ------------------------
        println!("[diagnostics] V(state) vs Hero/total, by blind level");
        println!(
            "{:>6} {:>6} {:>6} {:>6}  {:>8} {:>8}",
            "level", "hero", "v1", "v2", "V_table", "share"
        );
        let states: [(u32, u32, u32); 7] = [
            (300, 300, 300), // 10/10/10 (BB=20 -> 15bb each at level0)
            (600, 300, 0),   // 20/10/10-ish with the loser eliminated
            (600, 150, 150), // same hero share, opponent still split two ways
            (300, 600, 0),   // 10/20/10-ish
            (600, 150, 150), // duplicate kept intentionally close to prior row
            (750, 75, 75),   // hero dominant, both opponents crippled
            (150, 30, 720),  // hero very short vs. a crippled and a monster stack
        ];
        for &level_index in &LEVEL_INDICES {
            for &(hero, v1, v2) in &states {
                let v = table.value(hero, v1, level_index);
                println!(
                    "{level_index:>6} {hero:>6} {v1:>6} {v2:>6}  {:>8.3} {:>8.3}",
                    v,
                    share(hero)
                );
            }
        }
        println!();

        // ---- §7/§8 ranking comparison + premium-hand regression check ---
        struct Depth {
            label: &'static str,
            hero: u32,
            shover: u32,
            other: u32,
        }
        let depths = [
            Depth { label: "chip leader", hero: 750, shover: 100, other: 50 },
            Depth { label: "comfortable", hero: 500, shover: 250, other: 150 },
            Depth { label: "marginal", hero: 200, shover: 350, other: 350 },
            Depth { label: "short/at-risk", hero: 90, shover: 400, other: 410 },
            // 110 stays above the small blind hero (always the button here)
            // posts at every tested level, including level 8's 80/160 — a
            // smaller stack would go all-in from the blind post alone before
            // ever facing the scripted shove, leaving no decision to compare.
            Depth { label: "very short", hero: 110, shover: 430, other: 360 },
        ];
        let mut premium_regression = false;
        let mut flips = 0usize;
        let mut total_decisions = 0usize;
        let mut max_abs_delta_at_boundary = 0.0f64;

        for depth in &depths {
            for &level_index in &LEVEL_INDICES {
                let comparisons = compare_actions_over_worlds(
                    &mut rng,
                    depth.hero,
                    depth.shover,
                    depth.other,
                    &table,
                    level_index,
                    RANKING_SAMPLES,
                )
                .expect("comparison must run");
                if comparisons.is_empty() {
                    println!(
                        "[premium AA] depth={} level={} skipped: hero never reaches a decision (all-in from the blind post in every sampled world)",
                        depth.label, level_index
                    );
                    continue;
                }

                let chip_best = comparisons
                    .iter()
                    .max_by(|a, b| a.chip_ev.total_cmp(&b.chip_ev))
                    .unwrap();
                let table_best = comparisons
                    .iter()
                    .max_by(|a, b| a.table_ev.total_cmp(&b.table_ev))
                    .unwrap();

                println!(
                    "[premium AA] depth={} level={} hero={} shover={} other={}",
                    depth.label, level_index, depth.hero, depth.shover, depth.other
                );
                for c in &comparisons {
                    println!(
                        "    {:?}: chip_ev={:>8.2}  table_ev={:>8.4}",
                        c.action, c.chip_ev, c.table_ev
                    );
                }
                total_decisions += 1;
                if chip_best.action != table_best.action {
                    flips += 1;
                    println!(
                        "    ranking changed: chip prefers {:?}, table prefers {:?}",
                        chip_best.action, table_best.action
                    );
                }
                let call_is_dominant_under_chip_ev = comparisons
                    .iter()
                    .find(|c| c.action != Action::Fold)
                    .is_some_and(|call| {
                        call.chip_ev
                            >= comparisons
                                .iter()
                                .find(|c| c.action == Action::Fold)
                                .map(|f| f.chip_ev)
                                .unwrap_or(f64::MIN)
                    });
                if call_is_dominant_under_chip_ev
                    && table_best.action == Action::Fold
                {
                    premium_regression = true;
                    println!(
                        "    !! REGRESSION CANDIDATE: table valuation folds a hand chip-EV calls/shoves"
                    );
                }
            }
        }

        // Elimination-vs-no-elimination boundary, from the table itself
        // (same hero share, different field composition — the one pair
        // plain chip share provably cannot distinguish).
        for &level_index in &LEVEL_INDICES {
            let eliminated = table.value(600, 300, level_index); // heads-up, 20/10
            let three_way = table.value(600, 150, level_index); // still 3-way, 20/5/5
            let delta = (eliminated - three_way).abs();
            max_abs_delta_at_boundary = max_abs_delta_at_boundary.max(delta);
            println!(
                "[boundary] level {level_index}: V(600,300,elim)={eliminated:.3} V(600,150,150)={three_way:.3} |Δ|={delta:.3}"
            );
        }

        let flip_rate = flips as f64 / total_decisions as f64;
        println!(
            "\n[effect size] top-action flip rate: {flips}/{total_decisions} = {:.1}%",
            flip_rate * 100.0
        );
        println!(
            "[effect size] max |ΔV| at the elimination-vs-3-way boundary: {:.3}",
            max_abs_delta_at_boundary
        );
        println!("[effect size] premium-hand regression observed: {premium_regression}");

        // ---- §11 go/no-go --------------------------------------------------
        let verdict = if premium_regression {
            "NO-GO (disqualifying: a premium hand flipped to fold under the table valuation)"
        } else if flip_rate >= 0.10 {
            "GO (flip rate clears the 10% bar with no premium-hand regression)"
        } else if flip_rate <= 0.03 {
            "NO-GO (flip rate is within Monte Carlo noise at this trial count)"
        } else {
            "ITERATE (flip rate is in the ambiguous band; rerun with more trials before deciding)"
        };
        println!("\n[verdict] {verdict}");
        assert!(
            !premium_regression,
            "the experiment must not reproduce the old bust-risk over-folding bug"
        );
    }
}
