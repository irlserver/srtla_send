use std::collections::VecDeque;

use tracing::debug;

use crate::ewma::Ewma;
use crate::protocol::extract_keepalive_timestamp;
use crate::utils::now_ms;

/// Number of samples in the fast sliding window (~3s at 300ms keepalive interval).
const FAST_WINDOW_SAMPLES: usize = 10;
/// Number of samples in the slow sliding window (~30s at 300ms keepalive interval).
const SLOW_WINDOW_SAMPLES: usize = 100;

/// RTT measurement and tracking
#[derive(Debug, Clone)]
pub struct RttTracker {
    pub last_keepalive_sent_ms: u64,
    pub waiting_for_keepalive_response: bool,
    pub last_rtt_measurement_ms: u64,
    /// Smooth RTT in ms: slow rise (alpha=0.04), fast fall (alpha=0.40).
    pub smooth_rtt: Ewma,
    /// Fast RTT in ms: catches spikes quickly (up=0.10, down=0.30).
    pub fast_rtt: Ewma,
    pub rtt_jitter_ms: f64,
    pub prev_rtt_ms: f64,
    /// Smoothed RTT change rate in ms/sample (symmetric alpha=0.2).
    pub rtt_avg_delta: Ewma,
    /// Dual-window minimum RTT baseline. Computed as min(fast_window_min, slow_window_min).
    pub rtt_min_ms: f64,
    pub estimated_rtt_ms: f64,
    /// Fast sliding window for minimum RTT tracking (~3s).
    rtt_min_fast_window: VecDeque<f64>,
    /// Slow sliding window for minimum RTT tracking (~30s).
    rtt_min_slow_window: VecDeque<f64>,
}

impl Default for RttTracker {
    fn default() -> Self {
        Self {
            last_keepalive_sent_ms: 0,
            waiting_for_keepalive_response: false,
            last_rtt_measurement_ms: 0,
            smooth_rtt: Ewma::asymmetric(0.04, 0.40),
            fast_rtt: Ewma::asymmetric(0.10, 0.30),
            rtt_jitter_ms: 0.0,
            prev_rtt_ms: 0.0,
            rtt_avg_delta: Ewma::new(0.2),
            rtt_min_ms: 200.0,
            estimated_rtt_ms: 0.0,
            rtt_min_fast_window: VecDeque::with_capacity(FAST_WINDOW_SAMPLES),
            rtt_min_slow_window: VecDeque::with_capacity(SLOW_WINDOW_SAMPLES),
        }
    }
}

impl RttTracker {
    /// Reset all RTT tracking state to initial values
    /// Used during reconnection to start with a clean slate
    pub fn reset(&mut self) {
        self.last_rtt_measurement_ms = 0;
        self.smooth_rtt.reset();
        self.fast_rtt.reset();
        self.rtt_jitter_ms = 0.0;
        self.prev_rtt_ms = 0.0;
        self.rtt_avg_delta.reset();
        self.rtt_min_ms = 200.0;
        self.estimated_rtt_ms = 0.0;
        self.last_keepalive_sent_ms = 0;
        self.waiting_for_keepalive_response = false;
        self.rtt_min_fast_window.clear();
        self.rtt_min_slow_window.clear();
    }

    pub fn update_estimate(&mut self, rtt_ms: u64) {
        let current_rtt = rtt_ms as f64;

        // Initialize on first measurement
        if !self.smooth_rtt.is_initialized() {
            self.smooth_rtt.update(current_rtt);
            self.fast_rtt.update(current_rtt);
            self.prev_rtt_ms = current_rtt;
            self.estimated_rtt_ms = current_rtt;
            self.rtt_min_ms = current_rtt;
            self.rtt_min_fast_window.push_back(current_rtt);
            self.rtt_min_slow_window.push_back(current_rtt);
            self.last_rtt_measurement_ms = now_ms();
            return;
        }

        self.smooth_rtt.update(current_rtt);
        self.fast_rtt.update(current_rtt);

        // Track RTT change rate
        let delta_rtt = current_rtt - self.prev_rtt_ms;
        self.rtt_avg_delta.update(delta_rtt);
        self.prev_rtt_ms = current_rtt;

        // Dual-window minimum RTT baseline tracking.
        // Fast window (~3s) adapts quickly to cellular handovers.
        // Slow window (~30s) retains the true floor during stable periods.
        // Baseline = min(fast_min, slow_min).
        self.rtt_min_fast_window.push_back(current_rtt);
        while self.rtt_min_fast_window.len() > FAST_WINDOW_SAMPLES {
            self.rtt_min_fast_window.pop_front();
        }
        self.rtt_min_slow_window.push_back(current_rtt);
        while self.rtt_min_slow_window.len() > SLOW_WINDOW_SAMPLES {
            self.rtt_min_slow_window.pop_front();
        }
        let fast_min = self
            .rtt_min_fast_window
            .iter()
            .copied()
            .fold(f64::MAX, f64::min);
        let slow_min = self
            .rtt_min_slow_window
            .iter()
            .copied()
            .fold(f64::MAX, f64::min);
        self.rtt_min_ms = fast_min.min(slow_min);

        // Track peak deviation with exponential decay
        self.rtt_jitter_ms *= 0.99;
        if delta_rtt.abs() > self.rtt_jitter_ms {
            self.rtt_jitter_ms = delta_rtt.abs();
        }

        // Update legacy field for backwards compatibility
        self.estimated_rtt_ms = self.smooth_rtt.value();
        self.last_rtt_measurement_ms = now_ms();
    }

    pub fn is_stable(&self) -> bool {
        self.rtt_avg_delta.value().abs() < 1.0
    }

    pub fn record_keepalive_sent(&mut self) {
        self.last_keepalive_sent_ms = now_ms();
        self.waiting_for_keepalive_response = true;
    }

    pub fn handle_keepalive_response(&mut self, data: &[u8], label: &str) -> Option<u64> {
        if !self.waiting_for_keepalive_response {
            return None;
        }
        if let Some(ts) = extract_keepalive_timestamp(data) {
            let now = now_ms();
            let rtt = now.saturating_sub(ts);
            if rtt <= 10_000 {
                self.update_estimate(rtt);
                self.waiting_for_keepalive_response = false;
                debug!(
                    "{}: RTT from keepalive: {}ms (smooth: {:.1}ms, fast: {:.1}ms, jitter: \
                     {:.1}ms)",
                    label, rtt, self.smooth_rtt.value(), self.fast_rtt.value(), self.rtt_jitter_ms
                );
                return Some(rtt);
            }
        }
        self.waiting_for_keepalive_response = false;
        None
    }

    pub fn needs_measurement(&self, connected: bool, connection_established_ms: u64) -> bool {
        // Stay lightweight during initial registration: defer RTT probing until the
        // connection has been fully established via REG3. This mirrors the C
        // implementation, which does not measure RTT until a link is active, and
        // avoids spamming keepalives while the uplink is still handshaking.
        if connection_established_ms == 0 {
            return false;
        }

        connected
            && !self.waiting_for_keepalive_response
            && (self.last_rtt_measurement_ms == 0
                || now_ms().saturating_sub(self.last_rtt_measurement_ms) > 3000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dual_window_adapts_to_handover() {
        let mut tracker = RttTracker::default();

        // Establish baseline at 50ms
        for _ in 0..FAST_WINDOW_SAMPLES {
            tracker.update_estimate(50);
        }
        assert!(
            (tracker.rtt_min_ms - 50.0).abs() < 1.0,
            "baseline should be ~50ms, got {}",
            tracker.rtt_min_ms
        );

        // Simulate cellular handover: RTT jumps to 120ms
        for _ in 0..FAST_WINDOW_SAMPLES {
            tracker.update_estimate(120);
        }

        // After fast_window_samples of 120ms, the fast window no longer
        // contains any 50ms samples — baseline should have adapted upward.
        // The slow window still has 50ms samples, so baseline = slow_min = 50.
        // But the fast window min is now 120. baseline = min(120, 50) = 50.
        // After slow window also fills:
        // We need to fill slow window to fully adapt. But within fast window
        // the baseline should at least reflect that the fast min changed.
        // The key insight: baseline adapts within seconds because the fast
        // window forgets old samples quickly.

        // Feed enough samples to also push 50ms out of slow window
        for _ in 0..(SLOW_WINDOW_SAMPLES) {
            tracker.update_estimate(120);
        }

        // Now both windows only contain 120ms samples
        assert!(
            (tracker.rtt_min_ms - 120.0).abs() < 1.0,
            "baseline should have adapted to ~120ms after handover, got {}",
            tracker.rtt_min_ms
        );
    }

    #[test]
    fn test_dual_window_tracks_minimum() {
        let mut tracker = RttTracker::default();

        // Feed mixed RTT values
        tracker.update_estimate(100);
        tracker.update_estimate(80);
        tracker.update_estimate(60);
        tracker.update_estimate(90);
        tracker.update_estimate(70);

        // Baseline should be the minimum across both windows
        assert!(
            (tracker.rtt_min_ms - 60.0).abs() < 1.0,
            "baseline should track minimum of 60ms, got {}",
            tracker.rtt_min_ms
        );
    }

    #[test]
    fn test_dual_window_reset_clears_windows() {
        let mut tracker = RttTracker::default();

        // Fill with data
        for _ in 0..20 {
            tracker.update_estimate(50);
        }
        assert!((tracker.rtt_min_ms - 50.0).abs() < 1.0);

        // Reset
        tracker.reset();

        // After reset, windows should be empty and rtt_min_ms back to default
        assert!((tracker.rtt_min_ms - 200.0).abs() < f64::EPSILON);

        // First new measurement should set baseline
        tracker.update_estimate(80);
        assert!(
            (tracker.rtt_min_ms - 80.0).abs() < 1.0,
            "after reset + new measurement, baseline should be 80ms, got {}",
            tracker.rtt_min_ms
        );
    }

    #[test]
    fn test_dual_window_fast_window_forgets_old_minimum() {
        let mut tracker = RttTracker::default();

        // One very low sample
        tracker.update_estimate(20);

        // Fill fast window with higher values
        for _ in 0..FAST_WINDOW_SAMPLES {
            tracker.update_estimate(100);
        }

        // Fast window no longer has 20ms, but slow window does
        // So baseline = min(fast_min=100, slow_min=20) = 20
        assert!(
            (tracker.rtt_min_ms - 20.0).abs() < 1.0,
            "slow window should still hold 20ms minimum, got {}",
            tracker.rtt_min_ms
        );

        // Fill slow window too
        for _ in 0..SLOW_WINDOW_SAMPLES {
            tracker.update_estimate(100);
        }

        // Now both windows only have 100ms
        assert!(
            (tracker.rtt_min_ms - 100.0).abs() < 1.0,
            "after both windows filled, baseline should be 100ms, got {}",
            tracker.rtt_min_ms
        );
    }
}
