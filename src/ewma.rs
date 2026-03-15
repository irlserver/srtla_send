/// Exponentially Weighted Moving Average filter.
///
/// Smooths a noisy measurement series by weighting recent samples more
/// heavily. Used for RTT change rate tracking and other time-series smoothing.
///
/// The smoothing factor `alpha` controls responsiveness:
/// - `alpha` near 1.0: tracks input closely (low smoothing)
/// - `alpha` near 0.0: retains history (high smoothing)
#[derive(Debug, Clone)]
pub struct Ewma {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl Ewma {
    /// Creates a new EWMA filter with the given smoothing factor (`0.0 < alpha ≤ 1.0`).
    pub fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha,
            initialized: false,
        }
    }

    /// Feeds a new measurement into the filter, updating the smoothed value.
    ///
    /// NaN or infinite measurements are silently ignored to prevent
    /// poisoning the smoothed value.
    pub fn update(&mut self, measurement: f64) {
        if measurement.is_nan() || measurement.is_infinite() {
            return;
        }
        if !self.initialized {
            self.value = measurement;
            self.initialized = true;
        } else {
            self.value = self.value * (1.0 - self.alpha) + measurement * self.alpha;
        }
    }

    /// Returns the current smoothed value.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Resets the filter to its uninitialized state.
    pub fn reset(&mut self) {
        self.value = 0.0;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ewma_logic() {
        let mut ewma = Ewma::new(0.5);

        ewma.update(10.0);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        // (10 * 0.5) + (20 * 0.5) = 15
        ewma.update(20.0);
        assert!((ewma.value() - 15.0).abs() < f64::EPSILON);

        // (15 * 0.5) + (30 * 0.5) = 22.5
        ewma.update(30.0);
        assert!((ewma.value() - 22.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_smoothing() {
        let mut ewma = Ewma::new(0.1);
        ewma.update(100.0);
        assert!((ewma.value() - 100.0).abs() < f64::EPSILON);

        // value = 100 * 0.9 + 0 * 0.1 = 90
        ewma.update(0.0);
        assert!((ewma.value() - 90.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_uninitialized_value_is_zero() {
        let ewma = Ewma::new(0.5);
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_alpha_one_follows_input() {
        let mut ewma = Ewma::new(1.0);
        ewma.update(10.0);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        ewma.update(50.0);
        assert!((ewma.value() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_alpha_near_zero_retains_history() {
        let mut ewma = Ewma::new(0.001);
        ewma.update(100.0);
        assert!((ewma.value() - 100.0).abs() < f64::EPSILON);

        // value = 100 * 0.999 + 0 * 0.001 = 99.9
        ewma.update(0.0);
        assert!((ewma.value() - 99.9).abs() < 0.01);
    }

    #[test]
    fn test_ewma_negative_values() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(-10.0);
        assert!((ewma.value() - (-10.0)).abs() < f64::EPSILON);

        ewma.update(10.0);
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_converges_to_constant() {
        let mut ewma = Ewma::new(0.5);
        for _ in 0..100 {
            ewma.update(42.0);
        }
        assert!((ewma.value() - 42.0).abs() < 0.001);
    }

    #[test]
    fn test_ewma_nan_guard() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(10.0);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        ewma.update(f64::NAN);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        ewma.update(f64::INFINITY);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        ewma.update(f64::NEG_INFINITY);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        ewma.update(20.0);
        assert!((ewma.value() - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_nan_on_first_sample() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(f64::NAN);
        // Should remain at zero (not initialized)
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);

        // First valid sample should initialize
        ewma.update(42.0);
        assert!((ewma.value() - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_reset() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(100.0);

        ewma.reset();
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);

        // Should re-initialize on next update
        ewma.update(50.0);
        assert!((ewma.value() - 50.0).abs() < f64::EPSILON);
    }
}
