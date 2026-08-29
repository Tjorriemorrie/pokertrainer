use std::time::{Duration, Instant};

use rand::Rng;

use crate::card::{Card, Rank, Suit};
use crate::error::Result;
use crate::game::{Action, GameState, NUM_PLAYERS, Seat};
use crate::range::hands::Range;
use crate::rng::SeededRng;

use super::actions::candidates;
use super::config::MctsConfig;
use super::tree::{WorldSearch, observably_same};
use super::world::{World, WorldSampler};
use super::{ActionValue, PerWorld, SolveResult, combine_world_values, visits_for};

/// The realized game path between two consecutive hero decisions: the hero's
/// own action followed by the opponents' replies, in play order. Fed back to
/// the searcher so the tree is re-rooted on the played branch instead of
/// being rebuilt from zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PursuedPath {
    pub hero_action: Action,
    pub opponent_actions: Vec<Action>,
}

impl PursuedPath {
    /// The full edge sequence from the old root to the new decision node.
    fn edge_sequence(&self) -> Vec<Action> {
        std::iter::once(self.hero_action)
            .chain(self.opponent_actions.iter().copied())
            .collect()
    }
}

/// How a reshape request was served: how many world arenas were re-rooted on
/// the pursued path (keeping their accumulated statistics) and how many had
/// to be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReshapeReport {
    pub followed: usize,
    pub rebuilt: usize,
}

/// The lifecycle of the search behind the current hero decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearcherPhase {
    /// Still working toward the configured iteration budget.
    Searching,
    /// The iteration budget is reached but the minimum think time has not
    /// elapsed yet — the search keeps deepening.
    DepthReached,
    /// Both the budget and the minimum think time are met; the search keeps
    /// deepening until the wall budget ([`MctsConfig::max_duration`]) has
    /// elapsed and then idles until the next decision.
    Ready,
}

impl SearcherPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            SearcherPhase::Searching => "searching",
            SearcherPhase::DepthReached => "depth_reached",
            SearcherPhase::Ready => "ready",
        }
    }
}

/// The latest combined snapshot plus the progress the client renders in the
/// action dock's solver-depth badge. Published after every work chunk.
/// `decision` names the hero decision this status belongs to, so the client
/// can ignore stale updates queued behind a reshaped search.
#[derive(Clone, Debug, PartialEq)]
pub struct SearcherStatus {
    pub result: SolveResult,
    pub iterations_done: u64,
    pub target_iterations: u64,
    pub phase: SearcherPhase,
    pub decision: String,
}

/// Commands fed to the background solver task over its channel.
pub enum SearchCommand {
    /// Re-root the search at the given decision point; `path` carries the
    /// played hero action and the opponents' replies when the decision
    /// follows a submission. `decision` is the decision token echoed back in
    /// every status published for this root.
    Reshape {
        state: Box<GameState>,
        path: Option<PursuedPath>,
        hand_no: u64,
        decision: String,
    },
    /// Shut the worker down.
    Stop,
}

/// How long one blocking work chunk may run for before yielding and
/// publishing a progress update.
pub const CHUNK_WALL: Duration = Duration::from_millis(45);

/// A long-lived MCTS session: one sampled world (opponent determinization)
/// per search arena, kept and re-rooted across the hero's decisions so each
/// action reshapes the existing trees instead of discarding them. Worlds are
/// sampled per street-advance/hand (they must stay consistent with the
/// board); within one betting round the arenas themselves just follow the
/// played path. Every decision keeps searching until its wall budget
/// ([`MctsConfig::max_duration`]) has elapsed — the iteration target marks
/// the [`SearcherPhase::Ready`] transition, not the end of the work.
pub struct Searcher {
    worlds: Vec<World>,
    searches: Vec<WorldSearch>,
    rng: SeededRng,
    ranges: [Range; 2],
    config: MctsConfig,
    budget: MctsConfig,
    hand_no: u64,
    decision: String,
    epoch: u64,
    sweeps: u64,
    target: u64,
    started: Instant,
}

impl Searcher {
    /// Builds a searcher rooted at `state` (the hero's decision point), with
    /// worlds sampled from the opponent ranges for the current street.
    /// `decision` names the decision so progress statuses can be matched to
    /// the on-screen state.
    pub fn build(
        state: &GameState,
        ranges: [Range; 2],
        config: MctsConfig,
        hand_no: u64,
        decision: &str,
        rng: &mut SeededRng,
    ) -> Result<Self> {
        let budget = config.for_street(state.street());
        let worlds = WorldSampler::sample(rng, state, &ranges, budget.worlds)?;
        let mut searcher = Self {
            worlds,
            searches: Vec::new(),
            rng: seeded(rng),
            ranges,
            config,
            budget,
            hand_no,
            decision: decision.to_string(),
            epoch: 0,
            sweeps: 0,
            target: budget.iterations as u64,
            started: Instant::now(),
        };
        searcher.rebuild_arenas(state);
        searcher.fill_all()?;
        Ok(searcher)
    }

    pub fn hand_no(&self) -> u64 {
        self.hand_no
    }

    /// Whether the current decision's wall budget has not elapsed yet: work
    /// continues while it is true, regardless of the iteration target (the
    /// target only decides when the badge turns green).
    pub fn needs_work(&self) -> bool {
        self.started.elapsed() < self.config.max_duration
    }

    /// Re-roots the search at `state`, the hero's next decision point. With a
    /// pursued path the existing arenas are re-rooted on the played branch —
    /// visits, value sums and expansions survive. Arenas whose trees cannot
    /// represent the path (an unexpanded edge) keep their world but rebuild
    /// their tree; when the board itself changed (a street was dealt), the
    /// worlds are resampled so opponent holdings never clash with the felt.
    /// A new hand resamples everything. `decision` names the new decision.
    pub fn reshape(
        &mut self,
        state: &GameState,
        path: Option<&PursuedPath>,
        hand_no: u64,
        decision: &str,
    ) -> Result<ReshapeReport> {
        self.decision = decision.to_string();
        if hand_no != self.hand_no || path.is_none() || self.worlds.is_empty() {
            let mut rebuilt = Self::build(
                state,
                self.ranges,
                self.config,
                hand_no,
                decision,
                &mut self.rng,
            )?;
            rebuilt.epoch = self.epoch + 1;
            *self = rebuilt;
            return Ok(ReshapeReport {
                followed: 0,
                rebuilt: self.worlds.len(),
            });
        }

        self.budget = self.config.for_street(state.street());
        self.target = self.budget.iterations as u64;
        self.sweeps = 0;
        self.started = Instant::now();
        self.epoch += 1;

        if self
            .searches
            .first()
            .and_then(|arena| {
                arena
                    .root_state()
                    .map(|root| root.street() != state.street() || root.board() != state.board())
            })
            .unwrap_or(false)
        {
            self.worlds =
                WorldSampler::sample(&mut self.rng, state, &self.ranges, self.budget.worlds)?;
            self.rebuild_arenas(state);
            self.fill_all()?;
            return Ok(ReshapeReport {
                followed: 0,
                rebuilt: self.worlds.len(),
            });
        }

        self.reshape_following(state, path.expect("caller checked the path is present"))
    }

    /// Rebuilds every arena from the sampled worlds, rooted at `state`.
    fn rebuild_arenas(&mut self, state: &GameState) {
        let baseline = state.stack(Seat::Hero);
        let budget = self.budget;
        self.searches.clear();
        for world in &self.worlds {
            self.searches.push(WorldSearch::new(
                world.build_state(state),
                &world.runout,
                baseline,
                candidates(state),
                budget,
            ));
        }
    }

    /// Warm-starts every arena root so all candidates carry an estimate.
    fn fill_all(&mut self) -> Result<()> {
        for arena in &mut self.searches {
            arena.fill_unexpanded_root_candidates(&mut self.rng)?;
        }
        Ok(())
    }

    /// Re-roots one arena per world onto the played branch.
    fn reshape_following(
        &mut self,
        state: &GameState,
        path: &PursuedPath,
    ) -> Result<ReshapeReport> {
        let edges = path.edge_sequence();
        let baseline = state.stack(Seat::Hero);
        let mut followed = 0;
        let mut rebuilt = 0;
        for (index, search) in self.searches.iter_mut().enumerate() {
            let target = search.follow_path(&edges);
            let matched = target.is_some_and(|node| {
                search.is_decision(node)
                    && search
                        .node_state(node)
                        .is_some_and(|saved| observably_same(saved, state))
            });
            if matched {
                search.promote(target.expect("checked above"))?;
                search.fill_unexpanded_root_candidates(&mut self.rng)?;
                followed += 1;
            } else {
                let world = &self.worlds[index];
                *search = WorldSearch::new(
                    world.build_state(state),
                    &world.runout,
                    baseline,
                    candidates(state),
                    self.budget,
                );
                search.fill_unexpanded_root_candidates(&mut self.rng)?;
                rebuilt += 1;
            }
        }
        Ok(ReshapeReport { followed, rebuilt })
    }

    /// Runs one bounded chunk of work: full arena sweeps (one root visit per
    /// world) until the chunk's wall deadline is reached. Returns the status
    /// worth publishing, or `None` once the decision's wall budget
    /// ([`MctsConfig::max_duration`]) has elapsed and there is nothing left to
    /// do.
    pub fn run_chunk(&mut self, wall: Duration) -> Result<Option<SearcherStatus>> {
        if !self.needs_work() {
            return Ok(None);
        }
        let deadline = Instant::now() + wall;
        while self.needs_work() {
            for search in &mut self.searches {
                search.run_sweeps(&mut self.rng, 1)?;
            }
            self.sweeps += 1;
            if Instant::now() >= deadline {
                break;
            }
        }
        Ok(Some(self.status()?))
    }

    /// Builds the combined across-worlds snapshot of the current root.
    pub fn status(&self) -> Result<SearcherStatus> {
        let mut per_world: Vec<PerWorld> = Vec::with_capacity(self.worlds.len());
        let mut nodes = 0usize;
        let mut max_tree_depth = 0usize;
        let mut rollout_actions = 0u64;
        for (world, arena) in self.worlds.iter().zip(&self.searches) {
            let stats = arena.stats();
            nodes += stats.nodes;
            max_tree_depth = max_tree_depth.max(stats.max_tree_depth);
            rollout_actions += stats.rollout_actions;
            let values = arena
                .root_values()?
                .into_iter()
                .map(|(action, _, value, variance, bust_prob, visits)| {
                    (action, value, variance, bust_prob, visits)
                })
                .collect();
            per_world.push((world.weight, values));
        }

        let combined = combine_world_values(&per_world)?;
        let mut actions: Vec<ActionValue> = self
            .root_candidates()
            .into_iter()
            .filter_map(|(action, bucket)| {
                combined
                    .iter()
                    .find(|(candidate, _, _, _)| candidate == &action)
                    .map(|(_, ev, variance, bust_prob)| ActionValue {
                        action,
                        bucket,
                        ev: *ev,
                        variance: *variance,
                        bust_prob: *bust_prob,
                        visits: visits_for(&action, &per_world),
                    })
            })
            .collect();
        actions.sort_by(|a, b| b.ev.total_cmp(&a.ev));

        Ok(SearcherStatus {
            result: SolveResult {
                actions,
                worlds: self.worlds.len(),
                iterations: self.budget.iterations,
                max_depth: self.budget.max_depth,
                nodes,
                max_tree_depth,
                rollout_actions,
            },
            iterations_done: self.sweeps,
            target_iterations: self.target,
            phase: self.phase(),
            decision: self.decision.clone(),
        })
    }

    /// The lifecycle phase of the current decision: still searching, depth
    /// reached but the minimum think time not yet elapsed, or fully ready
    /// (the badge turns green while the search keeps deepening until its
    /// wall budget).
    fn phase(&self) -> SearcherPhase {
        let depth_reached = self.sweeps >= self.target;
        let time_met = self.started.elapsed() >= self.config.min_duration;
        if depth_reached && time_met {
            SearcherPhase::Ready
        } else if depth_reached {
            SearcherPhase::DepthReached
        } else {
            SearcherPhase::Searching
        }
    }

    /// The root candidate set (identical across worlds: computed from
    /// public information).
    fn root_candidates(&self) -> Vec<(Action, Option<crate::range::BetSize>)> {
        self.searches
            .first()
            .map(|search| search.root_candidates().to_vec())
            .unwrap_or_default()
    }
}

/// Derives a successor rng from the current stream, preserving determinism
/// for tests while giving every rebuild an independent sequence.
fn seeded(parent: &mut SeededRng) -> SeededRng {
    crate::rng::seeded_rng(parent.random::<u64>())
}

/// A clone of the observable decision point: hole cards only where the hero
/// can see them (opponent holdings are placeholders — world snapshots
/// overwrite them anyway).
pub(crate) fn observable_clone(state: &GameState) -> GameState {
    let dummy = [Card::new(Rank::Two, Suit::Clubs); 2];
    let mut cards = [dummy; NUM_PLAYERS];
    cards[Seat::Hero.index()] = state.hero_cards();
    for seat in [Seat::Opponent1, Seat::Opponent2] {
        if let Some(hand) = state.hole_cards(seat) {
            cards[seat.index()] = hand;
        }
    }
    state.clone_with_hole_cards(cards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::game::Street;
    use crate::game::blinds::BlindLevel;
    use crate::range::BetSize;
    use crate::range::hands::{HAND_COUNT, Range};
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn uniform_ranges() -> [Range; 2] {
        [[1.0 / HAND_COUNT as f32; HAND_COUNT]; 2]
    }

    /// The decision token shared by preflop decision tests.
    const TOKEN: &str = "h1-a0-preflop";

    /// The token of the reshaped decision in tests that follow a played path.
    const NEXT_TOKEN: &str = "h1-a1-preflop";

    /// A fast preset whose tiny iteration target is reached well inside the
    /// wall budget even on slow, instrumented test runs.
    fn quick_config() -> MctsConfig {
        MctsConfig {
            iterations: 4,
            ..MctsConfig::test()
        }
    }

    /// Hero posts the small blind on Opponent 1's button; Opponent 2 calls so
    /// the hero is first to face the big blind.
    fn preflop_decision() -> GameState {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(101)))
            .unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        state
    }

    /// Plays a hero raise that gets re-raised within the same street: Opponent 1
    /// re-raises to 100 and Opponent 2 folds, returning the live state at the
    /// hero's next decision plus the pursued path. No cards are dealt, so the
    /// whole path stays inside one betting round.
    fn raised_decision() -> (GameState, PursuedPath) {
        let mut state = preflop_decision();
        let raise = candidates(&state)
            .into_iter()
            .find_map(|(action, bucket)| {
                matches!(bucket, Some(BetSize::FourBb)).then_some(action)
            })
            .expect("preflop open offers a 4bb raise");
        state.apply_action(raise).unwrap();
        let re_raise = candidates(&state)
            .into_iter()
            .find_map(|(action, _)| matches!(action, Action::Raise(_)).then_some(action))
            .expect("facing a raise offers a re-raise");
        state.apply_action(re_raise).unwrap();
        state.apply_action(Action::Fold).unwrap();
        assert_eq!(
            state.to_act(),
            Seat::Hero,
            "the re-raise puts the hero back to act"
        );
        assert_eq!(state.street(), Street::Preflop, "the round is still open");
        (
            state,
            PursuedPath {
                hero_action: raise,
                opponent_actions: vec![re_raise, Action::Fold],
            },
        )
    }

    #[test]
    fn build_ships_a_valid_snapshot_covering_every_candidate() {
        let state = preflop_decision();
        let mut rng = seeded_rng(102);
        let searcher = Searcher::build(
            &state,
            uniform_ranges(),
            MctsConfig::test(),
            1,
            TOKEN,
            &mut rng,
        )
        .unwrap();
        let status = searcher.status().unwrap();
        let result = &status.result;
        assert_eq!(
            result.worlds,
            MctsConfig::test().for_street(Street::Preflop).worlds
        );
        assert!(!result.actions.is_empty());
        for candidate in candidates(&state) {
            let value = result
                .actions
                .iter()
                .find(|value| value.action == candidate.0)
                .expect("every root candidate has an estimate");
            assert!(value.ev.is_finite());
            assert!(value.visits >= 1, "filled before the first status");
        }
        assert_eq!(status.iterations_done, 0);
        assert_eq!(status.phase, SearcherPhase::Searching);
        assert_eq!(status.decision, TOKEN, "the decision token is echoed");
    }

    #[test]
    fn chunks_advance_progress_monotonically_until_the_wall_budget() {
        let state = preflop_decision();
        let mut rng = seeded_rng(103);
        let mut searcher =
            Searcher::build(&state, uniform_ranges(), quick_config(), 1, TOKEN, &mut rng).unwrap();
        let mut previous = 0u64;
        let mut progressed = false;
        let mut saw_ready = false;
        while let Some(status) = searcher.run_chunk(Duration::from_millis(5)).unwrap() {
            assert_eq!(status.decision, TOKEN, "every status names its decision");
            assert!(status.iterations_done > previous);
            if previous >= searcher.target {
                assert_eq!(
                    status.phase,
                    SearcherPhase::Ready,
                    "past the target the badge stays green while work continues"
                );
                saw_ready = true;
            }
            previous = status.iterations_done;
            progressed = true;
        }
        assert!(progressed, "at least one chunk ran");
        assert!(saw_ready, "the search reached its iteration target");
        assert!(
            searcher
                .run_chunk(Duration::from_millis(5))
                .unwrap()
                .is_none(),
            "once the wall budget has elapsed the search idles"
        );
    }

    #[test]
    fn a_ready_search_keeps_refining_past_the_target() {
        let state = preflop_decision();
        let mut rng = seeded_rng(107);
        let mut searcher =
            Searcher::build(&state, uniform_ranges(), quick_config(), 1, TOKEN, &mut rng).unwrap();
        let mut status = None;
        for _ in 0..500 {
            let chunk = searcher.run_chunk(Duration::from_millis(5)).unwrap();
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.phase == SearcherPhase::Ready {
                status = Some(chunk);
                break;
            }
        }
        let status = status.expect("the search turns green before the wall budget");
        let ready_sweeps = status.iterations_done;
        assert!(ready_sweeps >= searcher.target);
        let later = searcher
            .run_chunk(Duration::from_millis(5))
            .unwrap()
            .expect("the wall budget has not elapsed yet");
        assert_eq!(later.phase, SearcherPhase::Ready);
        assert!(
            later.iterations_done > ready_sweeps,
            "a ready search keeps deepening beyond the target"
        );
    }

    #[test]
    fn depth_reached_bridges_the_budget_and_the_minimum_think_time() {
        let state = preflop_decision();
        let mut rng = seeded_rng(108);
        let config = MctsConfig {
            iterations: 1,
            min_duration: Duration::from_secs(3600),
            ..MctsConfig::test()
        };
        let mut searcher =
            Searcher::build(&state, uniform_ranges(), config, 1, TOKEN, &mut rng).unwrap();
        let status = searcher
            .run_chunk(Duration::from_millis(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            status.phase,
            SearcherPhase::DepthReached,
            "the budget is met but the minimum think time has not elapsed"
        );
        assert!(
            searcher.needs_work(),
            "the wall budget keeps the search working after the target"
        );
        let deeper = searcher
            .run_chunk(Duration::from_millis(5))
            .unwrap()
            .expect("more chunks arrive while the wall budget lasts");
        assert_eq!(deeper.phase, SearcherPhase::DepthReached);
        assert!(deeper.iterations_done > status.iterations_done);
    }

    #[test]
    fn reshaping_on_the_played_path_keeps_the_trees() {
        let state = preflop_decision();
        let mut rng = seeded_rng(104);
        let mut searcher =
            Searcher::build(&state, uniform_ranges(), quick_config(), 1, TOKEN, &mut rng).unwrap();
        // Spend a few sweeps so the arenas have history worth keeping.
        let _ = searcher.run_chunk(Duration::from_millis(20)).unwrap();
        let (next, path) = raised_decision();
        let report = searcher.reshape(&next, Some(&path), 1, NEXT_TOKEN).unwrap();
        assert_eq!(
            report,
            ReshapeReport {
                followed: searcher.worlds.len(),
                rebuilt: 0,
            },
            "the whole played path lives inside the expanded trees"
        );
        assert_eq!(searcher.hand_no(), 1);
        assert_eq!(
            searcher.status().unwrap().decision,
            NEXT_TOKEN,
            "statuses follow the reshaped decision token"
        );
        assert_eq!(
            searcher.status().unwrap().iterations_done,
            0,
            "a fresh decision starts its progress clock"
        );
        let result = searcher.status().unwrap().result;
        for candidate in candidates(&next) {
            assert!(
                result.actions.iter().any(|v| v.action == candidate.0),
                "the promoted root covers its candidates"
            );
        }
        // Keep searching from the promoted root: it still converges.
        let mut previous = 0u64;
        for _ in 0..500 {
            let Some(status) = searcher.run_chunk(Duration::from_millis(5)).unwrap() else {
                break;
            };
            assert!(status.iterations_done >= previous);
            previous = status.iterations_done;
            if previous >= searcher.target {
                break;
            }
        }
        assert_eq!(
            searcher.status().unwrap().phase,
            SearcherPhase::Ready,
            "the reshaped search reaches its budget"
        );
    }

    #[test]
    fn a_mismatched_state_rebuilds_arenas_but_keeps_the_worlds() {
        let state = preflop_decision();
        let mut rng = seeded_rng(105);
        let mut searcher = Searcher::build(
            &state,
            uniform_ranges(),
            MctsConfig::test(),
            1,
            TOKEN,
            &mut rng,
        )
        .unwrap();
        let (mut next, path) = raised_decision();
        next.set_stack(Seat::Hero, next.stack(Seat::Hero) - 10);
        let worlds = searcher.worlds.len();
        let report = searcher.reshape(&next, Some(&path), 1, NEXT_TOKEN).unwrap();
        assert_eq!(report.followed, 0, "stack mismatch cannot follow");
        assert_eq!(report.rebuilt, worlds);
        assert_eq!(searcher.status().unwrap().decision, NEXT_TOKEN);
    }

    #[test]
    fn a_new_hand_resamples_everything() {
        let state = preflop_decision();
        let mut rng = seeded_rng(106);
        let mut searcher = Searcher::build(
            &state,
            uniform_ranges(),
            MctsConfig::test(),
            1,
            TOKEN,
            &mut rng,
        )
        .unwrap();
        let hand2 = preflop_decision();
        let report = searcher.reshape(&hand2, None, 2, "h2-a0-preflop").unwrap();
        assert_eq!(report.followed, 0);
        assert!(report.rebuilt >= 1);
        assert_eq!(searcher.hand_no(), 2);
        assert_eq!(searcher.status().unwrap().decision, "h2-a0-preflop");
        let result = searcher.status().unwrap().result;
        for candidate in candidates(&hand2) {
            assert!(result.actions.iter().any(|v| v.action == candidate.0));
        }
    }

    #[test]
    fn reported_depth_never_exceeds_the_street_budget_across_a_pumped_flop() {
        let mut deck = Deck::shuffled(&mut seeded_rng(992));
        let mut state = GameState::new(Seat::Hero, level());
        state.start_hand(&mut deck).unwrap();
        // Opponent 2 acts first preflop; drive to the hero (BTN).
        state.apply_action(Action::Call).unwrap();
        assert_eq!(state.to_act(), Seat::Hero);
        let mut rng = seeded_rng(991);
        let mut searcher = Searcher::build(
            &state,
            uniform_ranges(),
            MctsConfig::test(),
            1,
            TOKEN,
            &mut rng,
        )
        .unwrap();
        let _ = searcher.run_chunk(Duration::from_millis(20)).unwrap();
        // Hero calls, Opponent 1 (BB) checks; the flop is dealt and reached
        // by the opponent pump with the hero last to act.
        let hero_action = Action::Call;
        state.apply_action(hero_action).unwrap();
        let pump = [Action::Check];
        state.apply_action(pump[0]).unwrap();
        state.advance_street(&mut deck).unwrap();
        // Opponents act first on the flop until the hero's turn.
        let flop_pump: Vec<Action> = {
            let mut acts = Vec::new();
            while state.to_act() != Seat::Hero {
                let legal = state.legal_actions();
                let action = if legal.call_amount > 0 {
                    Action::Call
                } else {
                    Action::Check
                };
                state.apply_action(action).unwrap();
                acts.push(action);
            }
            acts
        };
        assert_eq!(state.street(), Street::Flop);
        let mut opponent_actions = vec![pump[0]];
        opponent_actions.extend(flop_pump);
        let report = searcher
            .reshape(
                &state,
                Some(&PursuedPath {
                    hero_action,
                    opponent_actions,
                }),
                1,
                "h1-a1-flop",
            )
            .unwrap();
        assert_eq!(
            report.rebuilt,
            searcher.worlds.len(),
            "street change rebuilds"
        );
        for _ in 0..50 {
            if !searcher.needs_work() {
                break;
            }
            searcher.run_chunk(Duration::from_millis(5)).unwrap();
        }
        let status = searcher.status().unwrap();
        assert_eq!(
            status.result.max_depth,
            MctsConfig::test().for_street(Street::Flop).max_depth
        );
        assert!(
            status.result.max_tree_depth <= status.result.max_depth,
            "realized depth {} exceeded the cap {}",
            status.result.max_tree_depth,
            status.result.max_depth
        );
    }

    #[test]
    fn observable_clone_keeps_only_observable_game_state() {
        let state = preflop_decision();
        let cloned = observable_clone(&state);
        assert!(observably_same(&state, &cloned));
        assert_eq!(cloned.hero_cards(), state.hero_cards());
        assert_ne!(
            cloned.hole_cards(Seat::Opponent1),
            state.hole_cards(Seat::Opponent1),
            "the clone masks the true opponent holdings"
        );
    }
}
