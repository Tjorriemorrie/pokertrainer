use rand::Rng;
use rand::SeedableRng;
use rand::rngs::ThreadRng;
use rand_chacha::ChaCha8Rng;

pub type SeededRng = ChaCha8Rng;

pub fn thread_rng() -> ThreadRng {
    rand::rng()
}

pub fn seeded_rng(seed: u64) -> SeededRng {
    SeededRng::seed_from_u64(seed)
}

pub fn gen_index<R: Rng + ?Sized>(rng: &mut R, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    rng.random_range(0..len)
}

/// Samples an index proportional to the given weights; returns `None` when
/// the pool is empty or all weights are zero (a never-sampled range).
pub fn weighted_index<R: Rng + ?Sized>(rng: &mut R, weights: &[f32]) -> Option<usize> {
    if weights.is_empty() {
        return None;
    }
    let total: f64 = weights.iter().map(|&w| f64::from(w)).sum();
    if total <= 0.0 {
        return None;
    }
    let target = rng.random::<f64>() * total;
    let mut cumulative = 0.0f64;
    for (index, &weight) in weights.iter().enumerate() {
        cumulative += f64::from(weight);
        if target < cumulative {
            return Some(index);
        }
    }
    Some(weights.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_rng_is_reproducible() {
        let mut a = seeded_rng(999);
        let mut b = seeded_rng(999);
        let mut c = seeded_rng(1000);
        for _ in 0..16 {
            assert_eq!(a.random_range(0..u64::MAX), b.random_range(0..u64::MAX));
        }
        assert_ne!(a.random_range(0..u64::MAX), c.random_range(0..u64::MAX));
    }

    #[test]
    fn thread_rng_produces_varied_values() {
        let mut rng = thread_rng();
        let first = rng.random_range(0..u64::MAX);
        assert_ne!(first, rng.random_range(0..u64::MAX));
    }

    #[test]
    fn gen_index_stays_in_bounds() {
        let mut rng = seeded_rng(1);
        for len in 1..=100usize {
            for _ in 0..100 {
                assert!(gen_index(&mut rng, len) < len);
            }
        }
        assert_eq!(gen_index(&mut rng, 0), 0);
    }

    #[test]
    fn weighted_index_edge_cases() {
        let mut rng = seeded_rng(2);
        assert_eq!(weighted_index(&mut rng, &[]), None);
        assert_eq!(weighted_index(&mut rng, &[0.0, 0.0]), None);
        assert_eq!(weighted_index(&mut rng, &[1.0]), Some(0));
        assert_eq!(weighted_index(&mut rng, &[0.0, 0.0, 1.0]), Some(2));
    }

    #[test]
    fn weighted_index_matches_distribution_roughly() {
        let mut rng = seeded_rng(3);
        let weights = [0.2f32, 0.3, 0.5];
        let mut hits = [0usize; 3];
        for _ in 0..100_000 {
            hits[weighted_index(&mut rng, &weights).unwrap()] += 1;
        }
        let total = 100_000f64;
        assert!((hits[0] as f64 / total - 0.2).abs() < 0.02);
        assert!((hits[1] as f64 / total - 0.3).abs() < 0.02);
        assert!((hits[2] as f64 / total - 0.5).abs() < 0.02);

        let mut rng2 = seeded_rng(3);
        let mut rng3 = seeded_rng(3);
        for _ in 0..100 {
            assert_eq!(
                weighted_index(&mut rng2, &weights),
                weighted_index(&mut rng3, &weights)
            );
        }
    }
}
