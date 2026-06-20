use std::collections::VecDeque;

use tracing::debug;

use crate::ewma::Ewma;
use crate::kalman::{KalmanConfig, KalmanFilter};
use crate::protocol::extract_keepalive_timestamp;
use crate::utils::now_ms;

/// Number of samples in the fast sliding window (~3s at 300ms keepalive interval).
const FAST_WINDOW_SAMPLES: usize = 10;
/// Number of samples in the slow sliding window (~30s at 300ms keepalive interval).
const SLOW_WINDOW_SAMPLES: usize = 100;
/// Number of samples in the min-RTT sample filter.
const RTT_SAMPLE_FILTER_SIZE: usize = 15;

/// EWMA weight for the mean-absolute-successive-difference (MASD) of
/// RTT. MASD is the average step size between consecutive samples; it
/// measures jitter without being fooled by a slow standing-queue ramp
/// (a steady climb has small successive steps). A small alpha averages
/// over roughly the fast window.
const RTT_MASD_ALPHA: f64 = 0.1;

/// Queue-building trips when the delay gradient (recent floor lifted
/// above the long-term floor) exceeds this many MASD units. A genuine
/// standing queue lifts the short-window minimum while MASD stays low,
/// so the ratio crosses; pure jitter lifts MASD in step with any
/// gradient, so it does not.
const GRAD_TRIP_SIGMA: f64 = 3.0;

/// Floor on the queue-building trip threshold as a fraction of the
/// link's own minimum RTT, so a near-zero MASD on a very clean link
/// still needs a meaningful absolute gradient (5% of baseline) to trip.
const GRAD_TRIP_FLOOR_FRACTION: f64 = 0.05;

/// RTT measurement and tracking.
///
/// Uses a 2-state Kalman filter [value, velocity] as the primary smooth RTT
/// estimator (replaces EWMA). The Kalman filter naturally tracks trends,
/// providing both a smoothed value and a velocity (rate of change).
#[derive(Debug, Clone)]
pub struct RttTracker {
    pub last_keepalive_sent_ms: u64,
    pub waiting_for_keepalive_response: bool,
    pub last_rtt_measurement_ms: u64,
    /// Kalman filter: primary smooth RTT with trend detection.
    /// `.value()` = smoothed RTT in ms, `.velocity()` = ms/sample trend.
    pub kalman_rtt: KalmanFilter,
    pub rtt_jitter_ms: f64,
    pub prev_rtt_ms: f64,
    /// Smoothed RTT change rate in ms/sample (symmetric alpha=0.2).
    pub rtt_avg_delta: Ewma,
    /// Dual-window minimum RTT baseline. Computed as min(fast_window_min, slow_window_min).
    pub rtt_min_ms: f64,
    /// Minimum of the fast (~3s) window only. The recent propagation floor.
    pub rtt_min_fast_ms: f64,
    /// Minimum of the slow (~30s) window only. The long-term floor.
    pub rtt_min_slow_ms: f64,
    /// Mean absolute successive difference of RTT (ms): the jitter-immune
    /// queue-build detector compares the floor gradient against it.
    pub rtt_masd_ms: f64,
    pub estimated_rtt_ms: f64,
    /// Fast sliding window for minimum RTT tracking (~3s).
    rtt_min_fast_window: VecDeque<f64>,
    /// Slow sliding window for minimum RTT tracking (~30s).
    rtt_min_slow_window: VecDeque<f64>,
    /// 15-sample min filter applied before feeding dual-window baseline.
    rtt_sample_filter: VecDeque<f64>,
}

impl Default for RttTracker {
    fn default() -> Self {
        Self {
            last_keepalive_sent_ms: 0,
            waiting_for_keepalive_response: false,
            last_rtt_measurement_ms: 0,
            kalman_rtt: KalmanFilter::new(KalmanConfig::for_rtt()),
            rtt_jitter_ms: 0.0,
            prev_rtt_ms: 0.0,
            rtt_avg_delta: Ewma::new(0.2),
            rtt_min_ms: 200.0,
            rtt_min_fast_ms: 200.0,
            rtt_min_slow_ms: 200.0,
            rtt_masd_ms: 0.0,
            estimated_rtt_ms: 0.0,
            rtt_min_fast_window: VecDeque::with_capacity(FAST_WINDOW_SAMPLES),
            rtt_min_slow_window: VecDeque::with_capacity(SLOW_WINDOW_SAMPLES),
            rtt_sample_filter: VecDeque::with_capacity(RTT_SAMPLE_FILTER_SIZE),
        }
    }
}

impl RttTracker {
    /// Reset all RTT tracking state to initial values.
    /// Used during reconnection to start with a clean slate.
    pub fn reset(&mut self) {
        self.last_rtt_measurement_ms = 0;
        self.kalman_rtt.reset();
        self.rtt_jitter_ms = 0.0;
        self.prev_rtt_ms = 0.0;
        self.rtt_avg_delta.reset();
        self.rtt_min_ms = 200.0;
        self.rtt_min_fast_ms = 200.0;
        self.rtt_min_slow_ms = 200.0;
        self.rtt_masd_ms = 0.0;
        self.estimated_rtt_ms = 0.0;
        self.last_keepalive_sent_ms = 0;
        self.waiting_for_keepalive_response = false;
        self.rtt_min_fast_window.clear();
        self.rtt_min_slow_window.clear();
        self.rtt_sample_filter.clear();
    }

    pub fn update_estimate(&mut self, rtt_ms: u64) {
        let current_rtt = rtt_ms as f64;

        // Min-RTT sample filter: smooth jitter before feeding baseline tracker.
        self.rtt_sample_filter.push_back(current_rtt);
        while self.rtt_sample_filter.len() > RTT_SAMPLE_FILTER_SIZE {
            self.rtt_sample_filter.pop_front();
        }
        let filtered_rtt = self
            .rtt_sample_filter
            .iter()
            .copied()
            .fold(f64::MAX, f64::min);

        // Initialize on first measurement
        if !self.kalman_rtt.is_initialized() {
            self.kalman_rtt.update(current_rtt);
            self.prev_rtt_ms = current_rtt;
            self.estimated_rtt_ms = current_rtt;
            self.rtt_min_ms = filtered_rtt;
            self.rtt_min_fast_ms = filtered_rtt;
            self.rtt_min_slow_ms = filtered_rtt;
            self.rtt_masd_ms = 0.0;
            self.rtt_min_fast_window.push_back(filtered_rtt);
            self.rtt_min_slow_window.push_back(filtered_rtt);
            self.last_rtt_measurement_ms = now_ms();
            return;
        }

        // Kalman filter: primary smooth RTT with trend detection
        self.kalman_rtt.update(current_rtt);

        // Track RTT change rate
        let delta_rtt = current_rtt - self.prev_rtt_ms;
        self.rtt_avg_delta.update(delta_rtt);
        // Mean absolute successive difference (jitter magnitude). A slow
        // standing-queue ramp has small successive steps, so MASD stays
        // low even as the floor lifts — that asymmetry is what makes the
        // queue-build detector immune to jitter.
        self.rtt_masd_ms =
            self.rtt_masd_ms * (1.0 - RTT_MASD_ALPHA) + delta_rtt.abs() * RTT_MASD_ALPHA;
        self.prev_rtt_ms = current_rtt;

        // Dual-window minimum RTT baseline tracking (fed with filtered RTT).
        // Fast window (~3s) adapts quickly to cellular handovers.
        // Slow window (~30s) retains the true floor during stable periods.
        // Baseline = min(fast_min, slow_min).
        self.rtt_min_fast_window.push_back(filtered_rtt);
        while self.rtt_min_fast_window.len() > FAST_WINDOW_SAMPLES {
            self.rtt_min_fast_window.pop_front();
        }
        self.rtt_min_slow_window.push_back(filtered_rtt);
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
        self.rtt_min_fast_ms = fast_min;
        self.rtt_min_slow_ms = slow_min;
        self.rtt_min_ms = fast_min.min(slow_min);

        // Track peak deviation with exponential decay
        self.rtt_jitter_ms *= 0.99;
        if delta_rtt.abs() > self.rtt_jitter_ms {
            self.rtt_jitter_ms = delta_rtt.abs();
        }

        // Smoothed RTT from Kalman
        self.estimated_rtt_ms = self.kalman_rtt.value();
        self.last_rtt_measurement_ms = now_ms();
    }

    pub fn is_stable(&self) -> bool {
        self.rtt_avg_delta.value().abs() < 1.0
    }

    /// Jitter-immune delay gradient (ms): how far the recent (fast)
    /// propagation floor has lifted above the long-term (slow) floor.
    /// A pure-jitter link keeps both minima at the propagation floor, so
    /// the gradient stays near zero; a genuine standing queue lifts the
    /// recent floor above the long-term one. Clamped at zero (a falling
    /// RTT is not queue building).
    pub fn rtt_gradient_ms(&self) -> f64 {
        (self.rtt_min_fast_ms - self.rtt_min_slow_ms).max(0.0)
    }

    /// True when the delay gradient indicates a standing queue forming,
    /// rather than jitter. Trips when the gradient exceeds
    /// `GRAD_TRIP_SIGMA` MASD units, floored at `GRAD_TRIP_FLOOR_FRACTION`
    /// of the link's own minimum RTT so a very clean link still needs a
    /// meaningful absolute rise. Returns false until the baseline is
    /// established.
    pub fn queue_building_suspected(&self) -> bool {
        if !self.kalman_rtt.is_initialized() || !self.rtt_min_ms.is_finite() {
            return false;
        }
        let trip =
            (GRAD_TRIP_SIGMA * self.rtt_masd_ms).max(GRAD_TRIP_FLOOR_FRACTION * self.rtt_min_ms);
        self.rtt_gradient_ms() > trip
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
            // Reject rtt == 0 (same-ms reply or future timestamp from clock skew):
            // a 0ms RTT is not a real sample and would seed rtt_min_ms = 0, making
            // the link look artificially fast. Matches the ACK path (ack_nak.rs).
            if rtt > 0 && rtt <= 10_000 {
                self.update_estimate(rtt);
                self.waiting_for_keepalive_response = false;
                debug!(
                    "{}: RTT from keepalive: {}ms (kalman: {:.1}ms, velocity: {:.2}ms/s, jitter: \
                     {:.1}ms)",
                    label,
                    rtt,
                    self.kalman_rtt.value(),
                    self.kalman_rtt.velocity(),
                    self.rtt_jitter_ms
                );
                return Some(rtt);
            }
        }
        self.waiting_for_keepalive_response = false;
        None
    }

    pub fn needs_measurement(&self, connected: bool, connection_established_ms: u64) -> bool {
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

        // Simulate cellular handover: RTT jumps to 120ms.
        let flush_count = RTT_SAMPLE_FILTER_SIZE + SLOW_WINDOW_SAMPLES;
        for _ in 0..flush_count {
            tracker.update_estimate(120);
        }

        assert!(
            (tracker.rtt_min_ms - 120.0).abs() < 1.0,
            "baseline should have adapted to ~120ms after handover, got {}",
            tracker.rtt_min_ms
        );
    }

    #[test]
    fn test_dual_window_tracks_minimum() {
        let mut tracker = RttTracker::default();

        tracker.update_estimate(100);
        tracker.update_estimate(80);
        tracker.update_estimate(60);
        tracker.update_estimate(90);
        tracker.update_estimate(70);

        assert!(
            (tracker.rtt_min_ms - 60.0).abs() < 1.0,
            "baseline should track minimum of 60ms, got {}",
            tracker.rtt_min_ms
        );
    }

    #[test]
    fn test_dual_window_reset_clears_windows() {
        let mut tracker = RttTracker::default();

        for _ in 0..20 {
            tracker.update_estimate(50);
        }
        assert!((tracker.rtt_min_ms - 50.0).abs() < 1.0);

        tracker.reset();

        assert!((tracker.rtt_min_ms - 200.0).abs() < f64::EPSILON);

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

        tracker.update_estimate(20);

        for _ in 0..FAST_WINDOW_SAMPLES {
            tracker.update_estimate(100);
        }

        assert!(
            (tracker.rtt_min_ms - 20.0).abs() < 1.0,
            "slow window should still hold 20ms minimum, got {}",
            tracker.rtt_min_ms
        );

        let flush_count = RTT_SAMPLE_FILTER_SIZE + SLOW_WINDOW_SAMPLES;
        for _ in 0..flush_count {
            tracker.update_estimate(100);
        }

        assert!(
            (tracker.rtt_min_ms - 100.0).abs() < 1.0,
            "after both windows filled, baseline should be 100ms, got {}",
            tracker.rtt_min_ms
        );
    }

    #[test]
    fn test_queue_building_ignores_pure_jitter() {
        let mut tracker = RttTracker::default();
        // High-amplitude jitter around a stable floor: both the fast and
        // slow minima sit on the 40ms floor, so the gradient stays ~0
        // even though MASD is large.
        for i in 0..80 {
            let rtt = if i % 2 == 0 { 40 } else { 60 };
            tracker.update_estimate(rtt);
        }
        assert!(
            !tracker.queue_building_suspected(),
            "pure jitter must not be read as a standing queue (gradient={:.1}, masd={:.1})",
            tracker.rtt_gradient_ms(),
            tracker.rtt_masd_ms
        );
    }

    #[test]
    fn test_queue_building_trips_on_standing_queue() {
        let mut tracker = RttTracker::default();
        // Establish a low long-term floor.
        for _ in 0..30 {
            tracker.update_estimate(20);
        }
        assert!(!tracker.queue_building_suspected());
        // Steady ramp (small successive steps -> low MASD) that lifts the
        // recent floor well above the long-term floor still held by the
        // slow window.
        let mut rtt = 20u64;
        for _ in 0..60 {
            rtt += 2;
            tracker.update_estimate(rtt);
        }
        assert!(
            tracker.queue_building_suspected(),
            "a sustained delay ramp must trip the detector (gradient={:.1}, masd={:.1})",
            tracker.rtt_gradient_ms(),
            tracker.rtt_masd_ms
        );
    }

    #[test]
    fn test_kalman_smooths_rtt() {
        let mut tracker = RttTracker::default();

        // Feed stable RTT
        for _ in 0..50 {
            tracker.update_estimate(50);
        }
        assert!(
            (tracker.estimated_rtt_ms - 50.0).abs() < 1.0,
            "Kalman should converge to stable value: {}",
            tracker.estimated_rtt_ms
        );

        // Feed rising RTT — velocity should go positive
        for _ in 0..20 {
            tracker.update_estimate(80);
        }
        assert!(
            tracker.kalman_rtt.velocity() > 0.0 || tracker.estimated_rtt_ms > 60.0,
            "should track rising RTT: est={}, vel={}",
            tracker.estimated_rtt_ms,
            tracker.kalman_rtt.velocity()
        );
    }

    #[test]
    fn test_keepalive_zero_rtt_rejected() {
        // A keepalive reply whose timestamp is >= now (same-ms reply or future
        // timestamp from clock skew) yields rtt == 0 via saturating_sub. A 0ms RTT
        // is not a real sample and must be rejected, or it would initialize the
        // filter and seed rtt_min_ms = 0, making the link look artificially fast.
        // Parity with the ACK path (ack_nak.rs).
        let mut tracker = RttTracker::default();
        assert!((tracker.rtt_min_ms - 200.0).abs() < f64::EPSILON);

        tracker.record_keepalive_sent();
        assert!(tracker.waiting_for_keepalive_response);

        let future_ts = now_ms() + 1_000_000;
        let mut pkt = [0u8; 10];
        pkt[0..2].copy_from_slice(&crate::protocol::SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        pkt[2..10].copy_from_slice(&future_ts.to_be_bytes());

        let rtt = tracker.handle_keepalive_response(&pkt, "test");

        assert_eq!(rtt, None, "zero-RTT keepalive must be rejected");
        assert!(
            !tracker.kalman_rtt.is_initialized(),
            "zero-RTT keepalive must not initialize the Kalman filter"
        );
        assert!(
            (tracker.rtt_min_ms - 200.0).abs() < f64::EPSILON,
            "rtt_min_ms must stay at the default baseline, got {}",
            tracker.rtt_min_ms
        );
        assert!(
            !tracker.waiting_for_keepalive_response,
            "the keepalive-wait flag must be cleared after a rejected reply"
        );
    }
}
