//! 2-state Kalman filter for RTT smoothing with trend detection.
//!
//! Tracks [value, velocity] to provide smooth RTT estimates that
//! naturally capture trends without the lag of pure EWMA.

/// Configuration for the Kalman filter.
#[derive(Debug, Clone)]
pub struct KalmanConfig {
    /// Process noise for the value state.
    pub q_value: f64,
    /// Process noise for the velocity state.
    pub q_velocity: f64,
    /// Measurement noise.
    pub r: f64,
}

impl KalmanConfig {
    /// Default configuration tuned for RTT smoothing.
    pub fn for_rtt() -> Self {
        Self {
            q_value: 0.5,
            q_velocity: 0.1,
            r: 2.0,
        }
    }
}

/// 2-state Kalman filter tracking [value, velocity].
#[derive(Debug, Clone)]
pub struct KalmanFilter {
    /// Estimated value (e.g., smoothed RTT in ms).
    x: f64,
    /// Estimated velocity (rate of change per update).
    v: f64,
    /// Error covariance matrix [2x2]: [p00, p01, p10, p11].
    p: [f64; 4],
    /// Configuration.
    config: KalmanConfig,
    /// Whether the filter has been initialized.
    initialized: bool,
}

impl KalmanFilter {
    pub fn new(config: KalmanConfig) -> Self {
        Self {
            x: 0.0,
            v: 0.0,
            p: [0.0; 4],
            config,
            initialized: false,
        }
    }

    /// Feed a new measurement, running predict + update.
    pub fn update(&mut self, measurement: f64) {
        if measurement.is_nan() || measurement.is_infinite() {
            return;
        }

        if !self.initialized {
            self.x = measurement;
            self.v = 0.0;
            self.p = [self.config.r, 0.0, 0.0, self.config.r];
            self.initialized = true;
            return;
        }

        // --- Predict ---
        // State transition: x_pred = x + v, v_pred = v
        let x_pred = self.x + self.v;
        let v_pred = self.v;

        // P_pred = F * P * F' + Q
        // F = [[1,1],[0,1]], so:
        // p00' = p00 + p10 + p01 + p11 + q_value
        // p01' = p01 + p11
        // p10' = p10 + p11
        // p11' = p11 + q_velocity
        let p00 = self.p[0] + self.p[2] + self.p[1] + self.p[3] + self.config.q_value;
        let p01 = self.p[1] + self.p[3];
        let p10 = self.p[2] + self.p[3];
        let p11 = self.p[3] + self.config.q_velocity;

        // --- Update ---
        // H = [1, 0]
        let y = measurement - x_pred; // innovation
        let s = p00 + self.config.r; // innovation covariance

        if s.abs() < 1e-12 {
            return;
        }

        let k0 = p00 / s; // Kalman gain
        let k1 = p10 / s;

        self.x = x_pred + k0 * y;
        self.v = v_pred + k1 * y;

        // P = (I - K*H) * P_pred
        self.p[0] = (1.0 - k0) * p00;
        self.p[1] = (1.0 - k0) * p01;
        self.p[2] = p10 - k1 * p00;
        self.p[3] = p11 - k1 * p01;
    }

    /// Current smoothed value.
    pub fn value(&self) -> f64 {
        self.x
    }

    /// Current estimated velocity (trend).
    pub fn velocity(&self) -> f64 {
        self.v
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn reset(&mut self) {
        self.x = 0.0;
        self.v = 0.0;
        self.p = [0.0; 4];
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initializes_on_first_measurement() {
        let mut kf = KalmanFilter::new(KalmanConfig::for_rtt());
        assert!(!kf.is_initialized());
        kf.update(50.0);
        assert!(kf.is_initialized());
        assert!((kf.value() - 50.0).abs() < f64::EPSILON);
        assert!((kf.velocity() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_tracks_constant_signal() {
        let mut kf = KalmanFilter::new(KalmanConfig::for_rtt());
        for _ in 0..100 {
            kf.update(42.0);
        }
        assert!(
            (kf.value() - 42.0).abs() < 0.1,
            "should converge to constant: {}",
            kf.value()
        );
        assert!(kf.velocity().abs() < 0.1);
    }

    #[test]
    fn test_tracks_rising_trend() {
        let mut kf = KalmanFilter::new(KalmanConfig::for_rtt());
        for i in 0..50 {
            kf.update(50.0 + i as f64);
        }
        assert!(
            kf.velocity() > 0.5,
            "velocity should be positive: {}",
            kf.velocity()
        );
        assert!(
            kf.value() > 90.0,
            "value should track rising input: {}",
            kf.value()
        );
    }

    #[test]
    fn test_smooths_noisy_signal() {
        let mut kf = KalmanFilter::new(KalmanConfig::for_rtt());
        for i in 0..100 {
            let noise = if i % 2 == 0 { 10.0 } else { -10.0 };
            kf.update(50.0 + noise);
        }
        assert!(
            (kf.value() - 50.0).abs() < 5.0,
            "should smooth noise: {}",
            kf.value()
        );
    }

    #[test]
    fn test_nan_guard() {
        let mut kf = KalmanFilter::new(KalmanConfig::for_rtt());
        kf.update(50.0);
        kf.update(f64::NAN);
        assert!((kf.value() - 50.0).abs() < 1.0);
    }

    #[test]
    fn test_reset() {
        let mut kf = KalmanFilter::new(KalmanConfig::for_rtt());
        kf.update(50.0);
        assert!(kf.is_initialized());
        kf.reset();
        assert!(!kf.is_initialized());
        assert!((kf.value() - 0.0).abs() < f64::EPSILON);
    }
}
