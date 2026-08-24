use rand::Rng;

use crate::card::Card;
use crate::error::Result;
use crate::game::{Action, GameState, Seat};
use crate::range::BetSize;
use crate::rng::{gen_index, weighted_index};

use super::actions::candidates;
use super::config::MctsConfig;
use super::rollout::{opponent_probs, rollout, step};

/// The role a tree node plays in the expectimax search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The hero is to act: a maximization node expanded incrementally and
    /// selected with UCB1.
    Decision,
    /// An opponent is to act: a chance node whose children are all created
    /// up front with heuristic-policy probabilities.
    Chance,
    /// Rollout horizon or terminal state: value comes from a playout.
    Leaf,
}

/// An edge out of a node. `prob` is the policy probability for children of
/// chance nodes and always 1.0 for children of decision nodes.
#[derive(Clone, Debug)]
struct Child {
    action: Action,
    node: usize,
    prob: f64,
}

/// The candidate actions available at a decision node; empty for chance and
/// leaf nodes.
type Candidates = Vec<(Action, Option<BetSize>)>;

/// One rollout payoff in the chip space of the search: the hero's terminal
/// chip delta relative to the decision point, plus whether the hero busted
/// (ended the hand with an empty stack).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Payoff {
    pub(crate) value: f64,
    pub(crate) busted: bool,
}

/// The world-search output for one action: the action itself, its bucket
/// label, the mean rollout value, the visit-weighted payoff variance, the
/// bust probability, and total visits.
pub(crate) type WorldValue = (Action, Option<BetSize>, f64, f64, f64, u64);

/// How deep one world's search actually ran: the deepest tree node expanded,
/// how many nodes the tree holds in total, and how many actions were
/// simulated in the rollouts below the tree horizon. The tree-depth cap and
/// iteration budget live in the solve result, so the realized effort can be
/// compared against the configured budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SearchStats {
    pub(crate) max_tree_depth: usize,
    pub(crate) nodes: usize,
    pub(crate) rollout_actions: u64,
}

/// One node of a single-world search tree. Every node keeps its own copy of
/// the game state (boards and contributions are path-dependent), so worlds
/// never share information and sampled opponent holdings stay isolated.
struct Node {
    kind: Kind,
    state: GameState,
    /// How many runout cards this state has already consumed.
    offset: usize,
    /// Number of hero decisions taken along the path to this node.
    hero_depth: usize,
    visits: u64,
    value_sum: f64,
    value_sq_sum: f64,
    bust_sum: f64,
    children: Vec<Child>,
    untried: usize,
    candidates: Candidates,
}

/// Runs the per-world search: a UCB1 tree for the hero's decisions with
/// chance nodes for opponent replies, backing up expectimax-style.
pub(crate) struct WorldSearch<'a> {
    nodes: Vec<Node>,
    root: usize,
    runout: &'a [Card],
    baseline: u32,
    config: MctsConfig,
    root_candidates: Candidates,
    /// Deepest node expanded so far (in hero-decision depth).
    max_tree_depth: usize,
    /// Actions simulated in rollouts so far.
    rollout_actions: u64,
}

impl<'a> WorldSearch<'a> {
    pub(crate) fn new(
        root_state: GameState,
        runout: &'a [Card],
        baseline: u32,
        root_candidates: Candidates,
        config: MctsConfig,
    ) -> Self {
        let root = Node {
            kind: Kind::Decision,
            state: root_state,
            offset: 0,
            hero_depth: 0,
            visits: 0,
            value_sum: 0.0,
            value_sq_sum: 0.0,
            bust_sum: 0.0,
            children: Vec::new(),
            untried: 0,
            candidates: root_candidates.clone(),
        };
        Self {
            nodes: vec![root],
            root: 0,
            runout,
            baseline,
            config,
            root_candidates,
            max_tree_depth: 0,
            rollout_actions: 0,
        }
    }

    pub(crate) fn run<R: Rng + ?Sized>(
        &mut self,
        rng: &mut R,
    ) -> Result<(Vec<WorldValue>, SearchStats)> {
        for _ in 0..self.config.iterations {
            self.iterate(rng)?;
        }
        self.fill_unexpanded_root_candidates(rng)?;
        let values = self.root_values()?;
        Ok((
            values,
            SearchStats {
                max_tree_depth: self.max_tree_depth,
                nodes: self.nodes.len(),
                rollout_actions: self.rollout_actions,
            },
        ))
    }

    fn iterate<R: Rng + ?Sized>(&mut self, rng: &mut R) -> Result<()> {
        let mut path = vec![self.root];
        loop {
            let index = *path.last().expect("path is never empty");
            let kind = self.nodes[index].kind;
            match kind {
                Kind::Leaf => {
                    let payoff = self.leaf_payoff(rng, index)?;
                    self.backprop(&path, payoff);
                    return Ok(());
                }
                Kind::Chance => {
                    let child = self.sample_chance_child(rng, index);
                    path.push(child);
                }
                Kind::Decision => {
                    let expanded = self.expand_one_child(index, rng)?;
                    if let Some(child) = expanded {
                        path.push(child);
                        let payoff = self.leaf_payoff(rng, child)?;
                        self.backprop(&path, payoff);
                        return Ok(());
                    }
                    let child = self.select_ucb(index);
                    path.push(child);
                }
            }
        }
    }

    /// Expands the next untried candidate of a decision node. Returns `None`
    /// when every candidate is already in the tree. Newly created chance
    /// children are fully expanded (cascade) and warm-started with one
    /// playout per child so every branch has a well-defined mean.
    fn expand_one_child<R: Rng + ?Sized>(
        &mut self,
        index: usize,
        rng: &mut R,
    ) -> Result<Option<usize>> {
        let (action, beyond_horizon) = {
            let node = &self.nodes[index];
            if node.kind != Kind::Decision {
                return Ok(None);
            }
            let Some(&(action, _bucket)) = node.candidates.get(node.untried) else {
                return Ok(None);
            };
            (action, node.hero_depth + 1 >= self.config.max_depth)
        };

        let mut next = self.clone_state(index);
        let parent_offset = self.nodes[index].offset;
        let next_offset = step(&mut next, action, self.runout, parent_offset)?;
        let hero_depth = self.nodes[index].hero_depth + 1;
        self.max_tree_depth = self.max_tree_depth.max(hero_depth);

        let kind = if next.is_hand_over() || next.folded(Seat::Hero) || beyond_horizon {
            Kind::Leaf
        } else if next.to_act() == Seat::Hero {
            Kind::Decision
        } else {
            Kind::Chance
        };
        let node_candidates = if kind == Kind::Decision {
            candidates(&next)
        } else {
            Vec::new()
        };
        let node = Node {
            kind,
            state: next,
            offset: next_offset,
            hero_depth,
            visits: 0,
            value_sum: 0.0,
            value_sq_sum: 0.0,
            bust_sum: 0.0,
            children: Vec::new(),
            untried: 0,
            candidates: node_candidates,
        };
        self.nodes.push(node);
        let child_index = self.nodes.len() - 1;
        self.nodes[index].children.push(Child {
            action,
            node: child_index,
            prob: 1.0,
        });
        self.nodes[index].untried += 1;

        if kind == Kind::Chance {
            self.expand_chance(child_index, rng)?;
        }
        Ok(Some(child_index))
    }

    /// Fully expands a chance node: creates one child per candidate action
    /// with policy probabilities, cascades into nested chance nodes, and
    /// warm-starts every direct child with one playout.
    fn expand_chance<R: Rng + ?Sized>(&mut self, index: usize, rng: &mut R) -> Result<()> {
        let probs = opponent_probs(&self.nodes[index].state);
        let created: Vec<(usize, Kind)> = {
            let parent_hero_depth = self.nodes[index].hero_depth;
            let mut created = Vec::new();
            let list = candidates(&self.nodes[index].state);
            for (position, (action, _bucket)) in list.into_iter().enumerate() {
                let mut next = self.clone_state(index);
                let next_offset = step(&mut next, action, self.runout, self.nodes[index].offset)?;
                let hero_depth = parent_hero_depth + 1;
                self.max_tree_depth = self.max_tree_depth.max(hero_depth);
                let beyond_horizon = hero_depth >= self.config.max_depth;
                let kind = if next.is_hand_over() || next.folded(Seat::Hero) || beyond_horizon {
                    Kind::Leaf
                } else if next.to_act() == Seat::Hero {
                    Kind::Decision
                } else {
                    Kind::Chance
                };
                let node_candidates = if kind == Kind::Decision {
                    candidates(&next)
                } else {
                    Vec::new()
                };
                let prob = probs.get(position).copied().unwrap_or(0.0);
                let node = Node {
                    kind,
                    state: next,
                    offset: next_offset,
                    hero_depth,
                    visits: 0,
                    value_sum: 0.0,
                    value_sq_sum: 0.0,
                    bust_sum: 0.0,
                    children: Vec::new(),
                    untried: 0,
                    candidates: node_candidates,
                };
                self.nodes.push(node);
                let node_index = self.nodes.len() - 1;
                self.nodes[index].children.push(Child {
                    action,
                    node: node_index,
                    prob,
                });
                created.push((node_index, kind));
            }
            created
        };

        let fallback = 1.0 / created.len().max(1) as f64;
        let total: f64 = self.nodes[index].children.iter().map(|c| c.prob).sum();
        for (node_index, _) in &created {
            let prob = self.nodes[index]
                .children
                .iter()
                .find(|c| c.node == *node_index)
                .map(|c| c.prob)
                .unwrap_or(0.0);
            let normalized = if total > 0.0 { prob / total } else { fallback };
            if let Some(child) = self.nodes[index]
                .children
                .iter_mut()
                .find(|c| c.node == *node_index)
            {
                child.prob = normalized;
            }
        }

        for (node_index, _) in &created {
            let payoff = self.leaf_payoff(rng, *node_index)?;
            self.backprop(&[index, *node_index], payoff);
        }
        for (node_index, kind) in &created {
            if *kind == Kind::Chance {
                self.expand_chance(*node_index, rng)?;
            }
        }
        Ok(())
    }

    fn sample_chance_child<R: Rng + ?Sized>(&self, rng: &mut R, index: usize) -> usize {
        let node = &self.nodes[index];
        if node.children.is_empty() {
            return index;
        }
        let weights: Vec<f32> = node.children.iter().map(|c| c.prob as f32).collect();
        if let Some(position) = weighted_index(rng, &weights) {
            return node.children[position].node;
        }
        node.children[gen_index(rng, node.children.len())].node
    }

    fn select_ucb(&self, index: usize) -> usize {
        let node = &self.nodes[index];
        let visits = node.visits.max(1) as f64;
        let mut best: Option<(f64, usize)> = None;
        for child in &node.children {
            let child_visits = self.nodes[child.node].visits.max(1) as f64;
            let mean = self.nodes[child.node].value_sum / child_visits;
            let exploration = self.config.uct_c * (visits.ln() / child_visits).sqrt();
            let score = mean + exploration;
            match best {
                Some((best_score, _)) if best_score >= score => {}
                _ => best = Some((score, child.node)),
            }
        }
        best.map(|(_, node)| node)
            .or_else(|| node.children.first().map(|c| c.node))
            .unwrap_or(index)
    }

    fn leaf_payoff<R: Rng + ?Sized>(&mut self, rng: &mut R, index: usize) -> Result<Payoff> {
        let node = &self.nodes[index];
        if node.state.is_hand_over() {
            let stack = node.state.stack(Seat::Hero);
            return Ok(Payoff {
                value: stack as f64 - f64::from(self.baseline),
                busted: stack == 0,
            });
        }
        let mut state = node.state.clone_with_hole_cards(self.cards_of(node));
        let (payoff, actions) = rollout(rng, &mut state, self.runout, node.offset, self.baseline)?;
        self.rollout_actions += actions as u64;
        Ok(payoff)
    }

    fn cards_of(&self, node: &Node) -> [[Card; 2]; 3] {
        let mut cards = [
            [Card::new(crate::card::Rank::Two, crate::card::Suit::Clubs); 2],
            [Card::new(crate::card::Rank::Two, crate::card::Suit::Clubs); 2],
            [Card::new(crate::card::Rank::Two, crate::card::Suit::Clubs); 2],
        ];
        for (index, seat) in [Seat::Hero, Seat::Opponent1, Seat::Opponent2]
            .into_iter()
            .enumerate()
        {
            if let Some(hand) = node.state.hole_cards(seat) {
                cards[index] = hand;
            }
        }
        cards
    }

    fn clone_state(&self, index: usize) -> GameState {
        let node = &self.nodes[index];
        node.state.clone_with_hole_cards(self.cards_of(node))
    }

    fn backprop(&mut self, path: &[usize], payoff: Payoff) {
        for &index in path {
            let node = &mut self.nodes[index];
            node.visits += 1;
            node.value_sum += payoff.value;
            node.value_sq_sum += payoff.value * payoff.value;
            if payoff.busted {
                node.bust_sum += 1.0;
            }
        }
    }

    /// Guarantees every root candidate got at least one playout, so `solve`
    /// can report an EV for each action even when the iteration budget is
    /// smaller than the candidate count.
    fn fill_unexpanded_root_candidates<R: Rng + ?Sized>(&mut self, rng: &mut R) -> Result<()> {
        loop {
            match self.expand_one_child(self.root, rng)? {
                Some(child) => {
                    let payoff = self.leaf_payoff(rng, child)?;
                    self.backprop(&[self.root, child], payoff);
                }
                None => return Ok(()),
            }
        }
    }

    fn root_values(&self) -> Result<Vec<WorldValue>> {
        let mut values = Vec::new();
        for (action, bucket) in &self.root_candidates {
            let Some(child) = self.nodes[self.root]
                .children
                .iter()
                .find(|c| c.action == *action)
            else {
                continue;
            };
            let node = &self.nodes[child.node];
            let visits = node.visits.max(1) as f64;
            let value = node.value_sum / visits;
            let variance = (node.value_sq_sum / visits - value * value).max(0.0);
            let bust_prob = node.bust_sum / visits;
            values.push((*action, *bucket, value, variance, bust_prob, node.visits));
        }
        Ok(values)
    }

    /// The deterministic chance-node expectation `sum(prob * child mean)`,
    /// exposed for expectimax tests.
    #[cfg(test)]
    pub(crate) fn chance_value(&self, index: usize) -> Option<f64> {
        let node = self.nodes.get(index)?;
        if node.kind != Kind::Chance {
            return None;
        }
        let total: f64 = node.children.iter().map(|c| c.prob).sum();
        if total <= 0.0 {
            return None;
        }
        Some(
            node.children
                .iter()
                .map(|c| {
                    let child = &self.nodes[c.node];
                    c.prob * (child.value_sum / child.visits.max(1) as f64)
                })
                .sum::<f64>()
                / total,
        )
    }

    #[cfg(test)]
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// A node's role, indexable in tests without exposing internals.
    #[cfg(test)]
    pub(crate) fn node_kind(&self, index: usize) -> Option<&'static str> {
        self.nodes.get(index).map(|n| match n.kind {
            Kind::Decision => "decision",
            Kind::Chance => "chance",
            Kind::Leaf => "leaf",
        })
    }

    /// `(children count, sum of child policy probs)` for a node.
    #[cfg(test)]
    pub(crate) fn children_summary(&self, index: usize) -> Option<(usize, f64)> {
        self.nodes.get(index).map(|n| {
            let sum = n.children.iter().map(|c| c.prob).sum();
            (n.children.len(), sum)
        })
    }

    /// `(visits, value_sum)` of a node's child edge.
    #[cfg(test)]
    pub(crate) fn child_stats(&self, parent: usize, position: usize) -> Option<(u64, f64)> {
        let node = self.nodes.get(parent)?;
        let child_index = node.children.get(position)?.node;
        let child = &self.nodes[child_index];
        Some((child.visits, child.value_sum))
    }

    #[cfg(test)]
    pub(crate) fn child_probs(&self, parent: usize) -> Option<Vec<f64>> {
        Some(
            self.nodes
                .get(parent)?
                .children
                .iter()
                .map(|c| c.prob)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Deck;
    use crate::game::blinds::BlindLevel;
    use crate::mcts::world::{World, WorldSampler};
    use crate::range::hands::{HAND_COUNT, Range};
    use crate::rng::seeded_rng;

    fn level() -> BlindLevel {
        BlindLevel::new(10, 20)
    }

    fn hero_open_state() -> GameState {
        let mut state = GameState::new(Seat::Opponent1, level());
        state
            .start_hand(&mut Deck::shuffled(&mut seeded_rng(1)))
            .unwrap();
        state
    }

    fn uniform_ranges() -> [Range; 2] {
        [[1.0f32 / HAND_COUNT as f32; HAND_COUNT]; 2]
    }

    fn make_world() -> (GameState, World) {
        let state = hero_open_state();
        let mut rng = seeded_rng(21);
        let world = WorldSampler::sample(&mut rng, &state, &uniform_ranges(), 1)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        (state, world)
    }

    fn make_search<'a>(state: &GameState, world: &'a World) -> WorldSearch<'a> {
        WorldSearch::new(
            world.build_state(state),
            &world.runout,
            state.stack(Seat::Hero),
            candidates(state),
            MctsConfig::test(),
        )
    }

    #[test]
    fn run_returns_an_estimate_for_every_root_candidate() {
        let (state, world) = make_world();
        let mut search = make_search(&state, &world);
        let mut rng = seeded_rng(22);
        let (values, stats) = search.run(&mut rng).unwrap();
        assert_eq!(values.len(), candidates(&state).len());
        for (action, _, value, _, _, visits) in &values {
            assert!(value.is_finite(), "{action:?} EV not finite");
            assert!(*visits >= 1, "{action:?} never visited");
            assert!(candidates(&state).iter().any(|(a, _)| a == action));
        }
        assert!(
            stats.nodes > 1,
            "the search grows past the root: {} nodes",
            stats.nodes
        );
        assert!(
            stats.rollout_actions > 0,
            "simulations below the horizon count actions"
        );
    }

    #[test]
    fn root_gets_visited_every_iteration() {
        let (state, world) = make_world();
        let mut search = make_search(&state, &world);
        let mut rng = seeded_rng(23);
        let _ = search.run(&mut rng).unwrap();
        let iterations = MctsConfig::test().iterations as u64;
        assert!(
            search.nodes[search.root].visits >= iterations,
            "root visits {} < iterations {iterations}",
            search.nodes[search.root].visits
        );
    }

    #[test]
    fn chance_children_have_normalized_policy_probs() {
        let (state, world) = make_world();
        let mut search = make_search(&state, &world);
        let mut rng = seeded_rng(24);
        let _ = search.run(&mut rng).unwrap();

        let mut found = false;
        for index in 0..search.node_count() {
            if search.node_kind(index) == Some("chance") {
                let (count, sum) = search.children_summary(index).unwrap();
                assert!(count >= 2);
                assert!((sum - 1.0).abs() < 1e-6, "chance probs sum to {sum}");
                found = true;
            }
        }
        assert!(found, "no chance node in the tree");
    }

    #[test]
    fn chance_value_is_the_policy_weighted_child_mean() {
        let (state, world) = make_world();
        let mut search = make_search(&state, &world);
        let mut rng = seeded_rng(25);
        let _ = search.run(&mut rng).unwrap();

        let mut checked = 0;
        for index in 0..search.node_count() {
            if search.node_kind(index) != Some("chance") {
                continue;
            }
            let probs = search.child_probs(index).unwrap();
            let (count, _) = search.children_summary(index).unwrap();
            let mut hand_sum = 0.0;
            for (position, prob) in probs.iter().enumerate().take(count) {
                let (visits, value_sum) = search.child_stats(index, position).unwrap();
                assert!(visits >= 1, "warm start should visit every chance child");
                hand_sum += prob * (value_sum / visits as f64);
            }
            let expectation = search.chance_value(index).unwrap();
            assert!((hand_sum - expectation).abs() < 1e-9);
            assert!(expectation.is_finite());
            checked += 1;
        }
        assert!(checked >= 1, "no chance node found");
    }

    #[test]
    fn select_ucb_picks_a_real_child() {
        let (state, world) = make_world();
        let mut search = make_search(&state, &world);
        let mut rng = seeded_rng(26);
        for _ in 0..2 {
            search.iterate(&mut rng).unwrap();
        }
        let selected = search.select_ucb(search.root);
        assert!(selected != search.root);
    }

    #[test]
    fn deterministic_inputs_produce_identical_values() {
        let state = hero_open_state();
        let config = MctsConfig::test();
        let mut rng_a = seeded_rng(27);
        let mut rng_b = seeded_rng(27);

        let worlds_a = WorldSampler::sample(&mut rng_a, &state, &uniform_ranges(), 2).unwrap();
        let worlds_b = WorldSampler::sample(&mut rng_b, &state, &uniform_ranges(), 2).unwrap();
        assert_eq!(worlds_a, worlds_b);

        for (world_a, world_b) in worlds_a.iter().zip(&worlds_b) {
            let mut search_a = WorldSearch::new(
                world_a.build_state(&state),
                &world_a.runout,
                state.stack(Seat::Hero),
                candidates(&state),
                config,
            );
            let mut search_b = WorldSearch::new(
                world_b.build_state(&state),
                &world_b.runout,
                state.stack(Seat::Hero),
                candidates(&state),
                config,
            );
            let values_a = search_a.run(&mut rng_a).unwrap();
            let values_b = search_b.run(&mut rng_b).unwrap();
            assert_eq!(values_a, values_b);
        }
    }
}
