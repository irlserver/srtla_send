use tracing::debug;

use crate::protocol::extract_keepalive_timestamp;
use crate::utils::now_ms;

/// RTT measurement and tracking
#[derive(Debug, Clone)]
pub struct RttTracker {
    pub last_keepalive_sent_ms: u64,
    pub waiting_for_keepalive_response: bool,
    pub last_rtt_measurement_ms: u64,
    pub smooth_rtt_ms: f64,
    pub fast_rtt_ms: f64,
    pub rtt_jitter_ms: f64,
    pub prev_rtt_ms: f64,
    pub rtt_avg_delta_ms: f64,
    pub rtt_min_ms: f64,
    pub estimated_rtt_ms: f64,
}

impl Default for RttTracker {
    fn default() -> Self {
        Self {
            last_keepalive_sent_ms: 0,
            waiting_for_keepalive_response: false,
            last_rtt_measurement_ms: 0,
            smooth_rtt_ms: 0.0,
            fast_rtt_ms: 0.0,
            rtt_jitter_ms: 0.0,
            prev_rtt_ms: 0.0,
            rtt_avg_delta_ms: 0.0,
            rtt_min_ms: 200.0,
            estimated_rtt_ms: 0.0,
        }
    }
}

impl RttTracker {
    /// Reset all RTT tracking state to initial values
    /// Used during reconnection to start with a clean slate
    pub fn reset(&mut self) {
        self.last_rtt_measurement_ms = 0;
        self.smooth_rtt_ms = 0.0;
        self.fast_rtt_ms = 0.0;
        self.rtt_jitter_ms = 0.0;
        self.prev_rtt_ms = 0.0;
        self.rtt_avg_delta_ms = 0.0;
        self.rtt_min_ms = 200.0;
        self.estimated_rtt_ms = 0.0;
        self.last_keepalive_sent_ms = 0;
        self.waiting_for_keepalive_response = false;
    }

    pub fn update_estimate(&mut self, rtt_ms: u64) {
        let current_rtt = rtt_ms as f64;

        // Initialize on first measurement
        if self.smooth_rtt_ms == 0.0 {
            self.smooth_rtt_ms = current_rtt;
            self.fast_rtt_ms = current_rtt;
            self.prev_rtt_ms = current_rtt;
            self.estimated_rtt_ms = current_rtt;
            self.last_rtt_measurement_ms = now_ms();
            return;
        }

        // Asymmetric smoothing for smooth RTT: fast decrease, slow increase
        if self.smooth_rtt_ms > current_rtt {
            self.smooth_rtt_ms = self.smooth_rtt_ms * 0.60 + current_rtt * 0.40;
        } else {
            self.smooth_rtt_ms = self.smooth_rtt_ms * 0.96 + current_rtt * 0.04;
        }

        // Asymmetric smoothing for fast RTT: catches spikes quickly
        if self.fast_rtt_ms > current_rtt {
            self.fast_rtt_ms = self.fast_rtt_ms * 0.70 + current_rtt * 0.30;
        } else {
            self.fast_rtt_ms = self.fast_rtt_ms * 0.90 + current_rtt * 0.10;
        }

        // Track RTT change rate
        let delta_rtt = current_rtt - self.prev_rtt_ms;
        self.rtt_avg_delta_ms = self.rtt_avg_delta_ms * 0.8 + delta_rtt * 0.2;
        self.prev_rtt_ms = current_rtt;

        // Track minimum RTT with slow decay, only update when stable
        self.rtt_min_ms *= 1.001;
        if current_rtt < self.rtt_min_ms && self.rtt_avg_delta_ms.abs() < 1.0 {
            self.rtt_min_ms = current_rtt;
        }

        // Track peak deviation with exponential decay
        self.rtt_jitter_ms *= 0.99;
        if delta_rtt.abs() > self.rtt_jitter_ms {
            self.rtt_jitter_ms = delta_rtt.abs();
        }

        // Update legacy field for backwards compatibility
        self.estimated_rtt_ms = self.smooth_rtt_ms;
        self.last_rtt_measurement_ms = now_ms();
    }

    pub fn is_stable(&self) -> bool {
        self.rtt_avg_delta_ms.abs() < 1.0
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
                    label, rtt, self.smooth_rtt_ms, self.fast_rtt_ms, self.rtt_jitter_ms
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
