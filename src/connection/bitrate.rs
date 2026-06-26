use crate::utils::now_ms;

/// Bitrate measurement and tracking
#[derive(Debug, Clone)]
pub struct BitrateTracker {
    pub bytes_sent_total: u64,
    pub bytes_sent_window: u64,
    pub last_rate_update_ms: u64,
    pub current_bitrate_bps: f64,
}

impl Default for BitrateTracker {
    fn default() -> Self {
        Self {
            bytes_sent_total: 0,
            bytes_sent_window: 0,
            last_rate_update_ms: now_ms(),
            current_bitrate_bps: 0.0,
        }
    }
}

impl BitrateTracker {
    /// Reset all bitrate tracking state to start fresh measurement window
    pub fn reset(&mut self) {
        self.bytes_sent_total = 0;
        self.bytes_sent_window = 0;
        self.last_rate_update_ms = now_ms();
        self.current_bitrate_bps = 0.0;
    }

    /// Update bitrate tracking when bytes are sent (matches Android C implementation)
    #[inline]
    pub fn update_on_send(&mut self, bytes_sent: u64) {
        self.bytes_sent_total = self.bytes_sent_total.saturating_add(bytes_sent);
    }

    /// Calculate current bitrate over a 2-second window (matching Android C implementation)
    pub fn calculate(&mut self) {
        const BITRATE_UPDATE_INTERVAL_MS: u64 = 2000;

        let now = now_ms();
        let time_diff_ms = now.saturating_sub(self.last_rate_update_ms);

        if time_diff_ms >= BITRATE_UPDATE_INTERVAL_MS {
            let bytes_diff = self.bytes_sent_total.saturating_sub(self.bytes_sent_window);

            if time_diff_ms > 0 {
                // Convert to bits per second: (bytes * 8 * 1000) / milliseconds
                let bits = bytes_diff.saturating_mul(8);
                self.current_bitrate_bps = (bits as f64 * 1000.0) / time_diff_ms as f64;
            }

            self.last_rate_update_ms = now;
            self.bytes_sent_window = self.bytes_sent_total;
        }
    }

    /// Get current bitrate in Mbps
    pub fn mbps(&self) -> f64 {
        self.current_bitrate_bps / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_send_raises_estimate() {
        let mut t = BitrateTracker::default();
        assert_eq!(t.current_bitrate_bps, 0.0);

        // Backdate the window so the next calculate() crosses the 2s interval.
        t.last_rate_update_ms = now_ms().saturating_sub(2_500);
        t.update_on_send(500_000);
        assert_eq!(t.bytes_sent_total, 500_000);

        t.calculate();
        assert!(
            t.current_bitrate_bps > 0.0,
            "sending bytes must raise the estimate, got {}",
            t.current_bitrate_bps
        );
    }

    #[test]
    fn bitrate_idle_decay() {
        let mut t = BitrateTracker::default();

        // Establish a non-zero estimate.
        t.last_rate_update_ms = now_ms().saturating_sub(2_500);
        t.update_on_send(500_000);
        t.calculate();
        assert!(t.current_bitrate_bps > 0.0);

        // Next window with no further sends: bytes_diff == 0 -> estimate decays to 0.
        t.last_rate_update_ms = now_ms().saturating_sub(2_500);
        t.calculate();
        assert_eq!(
            t.current_bitrate_bps, 0.0,
            "an idle window must decay the estimate to zero"
        );
    }

    #[test]
    fn bitrate_wire_bytes_basis() {
        let mut t = BitrateTracker::default();

        let before = now_ms().saturating_sub(4_000);
        t.last_rate_update_ms = before;
        t.bytes_sent_window = 0;
        t.update_on_send(1_000_000);

        t.calculate();

        // calculate() stamps last_rate_update_ms with the now_ms() it used, so the
        // exact elapsed window is recoverable for a precise expectation.
        let elapsed = t.last_rate_update_ms.saturating_sub(before);
        let expected = (1_000_000u64 * 8) as f64 * 1000.0 / elapsed as f64;
        assert!(
            (t.current_bitrate_bps - expected).abs() < 1.0,
            "bitrate is wire-bytes/s x8 (bps): got {}, expected {}",
            t.current_bitrate_bps,
            expected
        );
    }
}
