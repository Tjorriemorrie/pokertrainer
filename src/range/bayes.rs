use crate::range::hands::{HAND_COUNT, Range};

/// Multiplies a prior range by per-hand action likelihoods and normalizes the
/// result to sum to 1.0. Returns an all-zero range when the product is
/// everywhere zero (no hand is consistent with the observed action).
pub fn bayes_update(prior: &Range, likelihood: &Range) -> Range {
    let mut posterior = [0.0f32; HAND_COUNT];
    for (out, (&p, &l)) in posterior.iter_mut().zip(prior.iter().zip(likelihood)) {
        *out = p * l;
    }
    normalize(&posterior)
}

/// Normalizes a range so its weights sum to 1.0. Returns an all-zero range
/// when the sum is zero or negative.
pub fn normalize(weights: &Range) -> Range {
    let total: f64 = weights.iter().map(|&w| f64::from(w)).sum();
    if total <= 0.0 {
        return [0.0f32; HAND_COUNT];
    }
    let mut out = [0.0f32; HAND_COUNT];
    for (o, &w) in out.iter_mut().zip(weights) {
        *o = (f64::from(w) / total) as f32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bayes_update_multiplies_and_normalizes() {
        let prior = [0.5f32; HAND_COUNT];
        let mut likelihood = [0.0f32; HAND_COUNT];
        likelihood[0] = 1.0;
        likelihood[1] = 1.0;
        let posterior = bayes_update(&prior, &likelihood);
        assert_eq!(posterior[0], 0.5);
        assert_eq!(posterior[1], 0.5);
        assert_eq!(posterior[2], 0.0);
        let sum: f32 = posterior.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bayes_update_narrows_uniform_prior() {
        let prior = [1.0f32 / HAND_COUNT as f32; HAND_COUNT];
        let mut likelihood = [0.0f32; HAND_COUNT];
        for weight in likelihood.iter_mut().take(10) {
            *weight = 1.0;
        }
        let posterior = bayes_update(&prior, &likelihood);
        for weight in posterior.iter().take(10) {
            assert!((*weight - 0.1).abs() < 1e-6);
        }
        for weight in posterior.iter().skip(10) {
            assert_eq!(*weight, 0.0);
        }
    }

    #[test]
    fn bayes_update_all_zero_likelihood_yields_zero_range() {
        let prior = [0.5f32; HAND_COUNT];
        let likelihood = [0.0f32; HAND_COUNT];
        assert_eq!(bayes_update(&prior, &likelihood), [0.0f32; HAND_COUNT]);
    }

    #[test]
    fn normalize_sums_to_one() {
        let weights = [2.0f32; HAND_COUNT];
        let normalized = normalize(&weights);
        let sum: f64 = normalized.iter().map(|&w| f64::from(w)).sum();
        assert!((sum - 1.0).abs() < 1e-6);
        assert!((normalized[0] - 1.0 / HAND_COUNT as f32).abs() < 1e-6);
    }

    #[test]
    fn normalize_zero_range_stays_zero() {
        assert_eq!(normalize(&[0.0f32; HAND_COUNT]), [0.0f32; HAND_COUNT]);
    }
}
