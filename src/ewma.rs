/// Exponentially Weighted Moving Average filter with asymmetric alpha support.
///
/// Smooths a noisy measurement series by weighting recent samples more
/// heavily. Used throughout the codebase to smooth RTT, bandwidth, and
/// other time-series observations.
///
/// The smoothing factor `alpha` controls responsiveness:
/// - `alpha` near 1.0: tracks input closely (low smoothing)
/// - `alpha` near 0.0: retains history (high smoothing)
///
/// Asymmetric mode uses different alphas for rising vs falling measurements,
/// matching the existing pattern in `RttTracker` for fast-decrease/slow-increase
/// or vice versa.
#[derive(Debug, Clone)]
pub struct Ewma {
    value: f64,
    alpha_up: f64,
    alpha_down: f64,
    initialized: bool,
}

impl Ewma {
    /// Creates a new EWMA filter with symmetric smoothing factor (`0.0 < alpha ≤ 1.0`).
    pub fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha_up: alpha,
            alpha_down: alpha,
            initialized: false,
        }
    }

    /// Creates a new EWMA filter with asymmetric smoothing factors.
    ///
    /// - `alpha_up`: smoothing factor when measurement > current value (rising)
    /// - `alpha_down`: smoothing factor when measurement < current value (falling)
    ///
    /// Example: `Ewma::asymmetric(0.04, 0.40)` gives slow rise, fast fall —
    /// matching `RttTracker`'s smooth RTT behavior.
    pub fn asymmetric(alpha_up: f64, alpha_down: f64) -> Self {
        Self {
            value: 0.0,
            alpha_up,
            alpha_down,
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
            let alpha = if measurement > self.value {
                self.alpha_up
            } else {
                self.alpha_down
            };
            self.value = self.value * (1.0 - alpha) + measurement * alpha;
        }
    }

    /// Returns the current smoothed value.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Returns whether the filter has received at least one valid measurement.
    pub fn is_initialized(&self) -> bool {
        self.initialized
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

        // First update should set the value
        ewma.update(10.0);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        // Second update: (10 * 0.5) + (20 * 0.5) = 15
        ewma.update(20.0);
        assert!((ewma.value() - 15.0).abs() < f64::EPSILON);

        // Third update: (15 * 0.5) + (30 * 0.5) = 22.5
        ewma.update(30.0);
        assert!((ewma.value() - 22.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_smoothing() {
        let mut ewma = Ewma::new(0.1);
        ewma.update(100.0);
        assert!((ewma.value() - 100.0).abs() < f64::EPSILON);

        // Sudden drop: value = 100 * 0.9 + 0 * 0.1 = 90
        ewma.update(0.0);
        assert!((ewma.value() - 90.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_uninitialized_value_is_zero() {
        let ewma = Ewma::new(0.5);
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);
        assert!(!ewma.is_initialized());
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

        // NaN should be silently ignored
        ewma.update(f64::NAN);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        // Infinity should be silently ignored
        ewma.update(f64::INFINITY);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        ewma.update(f64::NEG_INFINITY);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        // Normal values should still work after NaN/Inf
        ewma.update(20.0);
        assert!((ewma.value() - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_nan_on_first_sample() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(f64::NAN);
        // Should remain uninitialized
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);
        assert!(!ewma.is_initialized());

        // First valid sample should initialize
        ewma.update(42.0);
        assert!((ewma.value() - 42.0).abs() < f64::EPSILON);
        assert!(ewma.is_initialized());
    }

    #[test]
    fn test_ewma_reset() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(100.0);
        assert!(ewma.is_initialized());

        ewma.reset();
        assert!(!ewma.is_initialized());
        assert!((ewma.value() - 0.0).abs() < f64::EPSILON);

        // Should re-initialize on next update
        ewma.update(50.0);
        assert!((ewma.value() - 50.0).abs() < f64::EPSILON);
        assert!(ewma.is_initialized());
    }

    // === Asymmetric EWMA tests ===

    #[test]
    fn test_asymmetric_slow_rise_fast_fall() {
        // Matches RttTracker's smooth RTT: alpha_up=0.04, alpha_down=0.40
        let mut ewma = Ewma::asymmetric(0.04, 0.40);
        ewma.update(100.0);
        assert!((ewma.value() - 100.0).abs() < f64::EPSILON);

        // Rising: slow response (alpha=0.04)
        // value = 100 * 0.96 + 200 * 0.04 = 96 + 8 = 104
        ewma.update(200.0);
        assert!((ewma.value() - 104.0).abs() < f64::EPSILON);

        // Falling: fast response (alpha=0.40)
        // value = 104 * 0.60 + 50 * 0.40 = 62.4 + 20 = 82.4
        ewma.update(50.0);
        assert!((ewma.value() - 82.4).abs() < f64::EPSILON);
    }

    #[test]
    fn test_asymmetric_fast_rise_slow_fall() {
        // Inverse: fast rise, slow fall
        let mut ewma = Ewma::asymmetric(0.40, 0.04);
        ewma.update(100.0);

        // Rising: fast response (alpha=0.40)
        // value = 100 * 0.60 + 200 * 0.40 = 60 + 80 = 140
        ewma.update(200.0);
        assert!((ewma.value() - 140.0).abs() < f64::EPSILON);

        // Falling: slow response (alpha=0.04)
        // value = 140 * 0.96 + 50 * 0.04 = 134.4 + 2.0 = 136.4
        ewma.update(50.0);
        assert!((ewma.value() - 136.4).abs() < f64::EPSILON);
    }

    #[test]
    fn test_asymmetric_converges_to_constant() {
        let mut ewma = Ewma::asymmetric(0.1, 0.3);
        for _ in 0..1000 {
            ewma.update(42.0);
        }
        assert!((ewma.value() - 42.0).abs() < 0.001);
    }

    #[test]
    fn test_asymmetric_nan_guard() {
        let mut ewma = Ewma::asymmetric(0.1, 0.3);
        ewma.update(100.0);
        ewma.update(f64::NAN);
        assert!((ewma.value() - 100.0).abs() < f64::EPSILON);
    }
}
