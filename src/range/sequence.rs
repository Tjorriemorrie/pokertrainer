use crate::error::Result;
use crate::range::hands::{HAND_COUNT, Range};
use crate::range_cache::{RangeKey, RangeStore};

/// Minimum number of observed hands before a player-specific range is trusted;
/// below this the resolver falls back to the population average.
pub const MIN_SAMPLE_HANDS: u32 = 30;

/// Effective stack-depth bucket, matching the `opponent_stats.stack_bucket`
/// and `contextual_ranges.stack_bucket` columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StackBucket {
    Bb10,
    Bb15,
    Bb25,
}

impl StackBucket {
    pub const ALL: [StackBucket; 3] = [StackBucket::Bb10, StackBucket::Bb15, StackBucket::Bb25];

    /// Buckets an effective stack measured in big blinds.
    pub fn from_bb(bb: u32) -> StackBucket {
        if bb <= 10 {
            StackBucket::Bb10
        } else if bb <= 15 {
            StackBucket::Bb15
        } else {
            StackBucket::Bb25
        }
    }

    /// Buckets an effective stack given the current big blind.
    pub fn from_stack(stack: u32, big_blind: u32) -> StackBucket {
        StackBucket::from_bb(stack / big_blind.max(1))
    }

    /// The integer value stored in the database (10, 15, or 25).
    pub fn as_i16(self) -> i16 {
        match self {
            StackBucket::Bb10 => 10,
            StackBucket::Bb15 => 15,
            StackBucket::Bb25 => 25,
        }
    }

    /// A human-readable label used in node keys.
    pub fn as_str(self) -> &'static str {
        match self {
            StackBucket::Bb10 => "10BB",
            StackBucket::Bb15 => "15BB",
            StackBucket::Bb25 => "25BB",
        }
    }
}

/// A game node identified by the acting player, their effective stack bucket,
/// and the abstracted action sequence (e.g. `BTN_OPEN_2BB_SB_FOLD`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SequenceNode {
    pub profile_id: i32,
    pub stack_bucket: StackBucket,
    pub sequence: String,
}

impl SequenceNode {
    pub fn new(profile_id: i32, stack_bucket: StackBucket, sequence: impl Into<String>) -> Self {
        Self {
            profile_id,
            stack_bucket,
            sequence: sequence.into(),
        }
    }

    /// A human-readable key combining the stack bucket and sequence.
    pub fn key(&self) -> String {
        format!("{}_{}", self.stack_bucket.as_str(), self.sequence)
    }
}

/// A resolved range, with the sample size backing it and whether the
/// population fallback was used.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedRange {
    pub weights: Range,
    pub sample_count: u32,
    pub used_population: bool,
}

/// Provides the population-average range used when a player's sample is too
/// small to trust.
pub trait PopulationSource {
    fn population_range(&self, node: &str, stack_bucket: StackBucket) -> Range;
}

/// A uniform population fallback: every hand equally likely.
#[derive(Clone, Copy, Debug, Default)]
pub struct UniformPopulation;

impl PopulationSource for UniformPopulation {
    fn population_range(&self, _node: &str, _stack_bucket: StackBucket) -> Range {
        [1.0 / HAND_COUNT as f32; HAND_COUNT]
    }
}

/// A Chen-score-weighted population fallback: proportional to each class's
/// [`crate::range::hands::Hand::chen_score`] instead of flat/uniform. Used
/// in place of [`UniformPopulation`] wherever the range being resolved is a
/// *preflop* opponent-holding prior — below [`MIN_SAMPLE_HANDS`] a literal
/// uniform fallback assumes the opponent's hand is as likely to be `72o` as
/// `AA`, which understates how much a real raise (or even just a real deal)
/// skews toward stronger starting hands, exactly the gap that made a
/// preflop reraise/call look far more profitable than it is.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChenPopulation;

impl PopulationSource for ChenPopulation {
    fn population_range(&self, _node: &str, _stack_bucket: StackBucket) -> Range {
        crate::range::hands::chen_prior()
    }
}

/// Resolves a sequence node to a range, falling back to the population average
/// when the player-specific sample is below [`MIN_SAMPLE_HANDS`].
pub struct RangeResolver<P> {
    population: P,
}

impl<P: PopulationSource> RangeResolver<P> {
    pub fn new(population: P) -> Self {
        Self { population }
    }

    pub async fn resolve<S: RangeStore>(
        &self,
        source: &S,
        node: &SequenceNode,
    ) -> Result<ResolvedRange> {
        let key = RangeKey {
            profile_id: node.profile_id,
            node: node.sequence.clone(),
            stack_bucket: node.stack_bucket.as_i16(),
        };
        match source.load_range(&key).await? {
            Some(stored) if stored.sample_count >= MIN_SAMPLE_HANDS => Ok(ResolvedRange {
                weights: stored.weights,
                sample_count: stored.sample_count,
                used_population: false,
            }),
            _ => Ok(ResolvedRange {
                weights: self
                    .population
                    .population_range(&node.sequence, node.stack_bucket),
                sample_count: 0,
                used_population: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::db::StoredRange;

    struct MockStore {
        data: Mutex<HashMap<(i32, String, i16), StoredRange>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }

        fn insert(&self, profile_id: i32, node: &str, bucket: i16, range: StoredRange) {
            self.data
                .lock()
                .unwrap()
                .insert((profile_id, node.to_string(), bucket), range);
        }
    }

    impl RangeStore for MockStore {
        async fn load_range(&self, key: &RangeKey) -> Result<Option<StoredRange>> {
            Ok(self
                .data
                .lock()
                .unwrap()
                .get(&(key.profile_id, key.node.clone(), key.stack_bucket))
                .cloned())
        }

        async fn store_range(&self, _key: &RangeKey, _range: &StoredRange) -> Result<()> {
            Ok(())
        }
    }

    fn stored(sample_count: u32) -> StoredRange {
        StoredRange {
            weights: [0.5; HAND_COUNT],
            sample_count,
        }
    }

    #[tokio::test]
    async fn resolves_player_range_when_sample_is_sufficient() {
        let store = MockStore::new();
        store.insert(7, "BTN_OPEN_2BB", 25, stored(30));
        let resolver = RangeResolver::new(UniformPopulation);
        let node = SequenceNode::new(7, StackBucket::Bb25, "BTN_OPEN_2BB");
        let resolved = resolver.resolve(&store, &node).await.unwrap();
        assert!(!resolved.used_population);
        assert_eq!(resolved.sample_count, 30);
        assert_eq!(resolved.weights, [0.5; HAND_COUNT]);
    }

    #[tokio::test]
    async fn falls_back_to_population_when_sample_is_insufficient() {
        let store = MockStore::new();
        store.insert(7, "BTN_OPEN_2BB", 25, stored(29));
        let resolver = RangeResolver::new(UniformPopulation);
        let node = SequenceNode::new(7, StackBucket::Bb25, "BTN_OPEN_2BB");
        let resolved = resolver.resolve(&store, &node).await.unwrap();
        assert!(resolved.used_population);
        assert_eq!(resolved.sample_count, 0);
        let expected = 1.0 / HAND_COUNT as f32;
        assert!((resolved.weights[0] - expected).abs() < 1e-6);
    }

    #[tokio::test]
    async fn falls_back_to_population_when_no_range_exists() {
        let store = MockStore::new();
        let resolver = RangeResolver::new(UniformPopulation);
        let node = SequenceNode::new(7, StackBucket::Bb10, "MISSING");
        let resolved = resolver.resolve(&store, &node).await.unwrap();
        assert!(resolved.used_population);
    }

    /// Regression for the "coach's fallback assumes any two cards" gap:
    /// below the trust threshold, a resolver built on [`ChenPopulation`]
    /// must fall back to a strength-shaped prior, not flat uniform — the
    /// nuts and the worst hand can't resolve to the same weight.
    #[tokio::test]
    async fn chen_population_fallback_favors_stronger_hands_over_the_worst_one() {
        let store = MockStore::new();
        let resolver = RangeResolver::new(ChenPopulation);
        let node = SequenceNode::new(7, StackBucket::Bb10, "MISSING");
        let resolved = resolver.resolve(&store, &node).await.unwrap();
        assert!(resolved.used_population);
        let aa = crate::range::hands::Hand::new(
            crate::card::Rank::Ace,
            crate::card::Rank::Ace,
            false,
        );
        let seven_deuce = crate::range::hands::Hand::new(
            crate::card::Rank::Seven,
            crate::card::Rank::Two,
            false,
        );
        assert!(resolved.weights[aa.index()] > resolved.weights[seven_deuce.index()]);
        assert!((resolved.weights.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn stack_bucket_boundaries() {
        assert_eq!(StackBucket::from_bb(0), StackBucket::Bb10);
        assert_eq!(StackBucket::from_bb(10), StackBucket::Bb10);
        assert_eq!(StackBucket::from_bb(11), StackBucket::Bb15);
        assert_eq!(StackBucket::from_bb(15), StackBucket::Bb15);
        assert_eq!(StackBucket::from_bb(16), StackBucket::Bb25);
        assert_eq!(StackBucket::from_bb(100), StackBucket::Bb25);
    }

    #[test]
    fn stack_bucket_from_stack_uses_big_blind() {
        assert_eq!(StackBucket::from_stack(200, 20), StackBucket::Bb10);
        assert_eq!(StackBucket::from_stack(300, 20), StackBucket::Bb15);
        assert_eq!(StackBucket::from_stack(500, 20), StackBucket::Bb25);
        assert_eq!(StackBucket::from_stack(500, 0), StackBucket::Bb25);
    }

    #[test]
    fn stack_bucket_round_trips_i16() {
        for bucket in StackBucket::ALL {
            let expected = match bucket {
                StackBucket::Bb10 => 10,
                StackBucket::Bb15 => 15,
                StackBucket::Bb25 => 25,
            };
            assert_eq!(bucket.as_i16(), expected);
        }
    }

    #[test]
    fn sequence_node_key_combines_bucket_and_sequence() {
        let node = SequenceNode::new(1, StackBucket::Bb25, "BTN_OPEN_2BB_SB_FOLD");
        assert_eq!(node.key(), "25BB_BTN_OPEN_2BB_SB_FOLD");
    }
}
