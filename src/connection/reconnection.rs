use tracing::{debug, info};

use crate::utils::now_ms;

const STARTUP_GRACE_MS: u64 = 1_500;
const BASE_RECONNECT_DELAY_MS: u64 = 5000;
const MAX_BACKOFF_DELAY_MS: u64 = 120_000;
const MAX_BACKOFF_COUNT: u32 = 5;

/// Reconnection state and backoff tracking
#[derive(Debug, Clone, Default)]
pub struct ReconnectionState {
    pub last_reconnect_attempt_ms: u64,
    pub reconnect_failure_count: u32,
    pub connection_established_ms: u64,
    pub startup_grace_deadline_ms: u64,
}

impl ReconnectionState {
    /// Calculate backoff delay based on failure count.
    fn backoff_delay(&self) -> u64 {
        let capped_failures = self.reconnect_failure_count.min(MAX_BACKOFF_COUNT);
        let delay = BASE_RECONNECT_DELAY_MS.saturating_mul(1u64 << capped_failures);
        delay.min(MAX_BACKOFF_DELAY_MS)
    }

    pub fn should_attempt_reconnect(&self) -> bool {
        let now = now_ms();

        if self.connection_established_ms == 0 {
            if now <= self.startup_grace_deadline_ms {
                return false;
            }
            // Match the C implementation during initial registration by retrying
            // roughly once per housekeeping pass (~1s cadence).
            if self.last_reconnect_attempt_ms == 0 {
                return true;
            }
            return now.saturating_sub(self.last_reconnect_attempt_ms) >= 1000;
        }

        if self.last_reconnect_attempt_ms == 0 {
            return true;
        }

        let time_since_last_attempt = now.saturating_sub(self.last_reconnect_attempt_ms);
        time_since_last_attempt >= self.backoff_delay()
    }

    pub fn record_attempt(&mut self, label: &str) {
        self.last_reconnect_attempt_ms = now_ms();

        // For initial registration we keep retry cadence fast and skip backoff
        if self.connection_established_ms == 0 {
            debug!(
                "{}: Initial registration retry scheduled (next attempt in ~1s)",
                label
            );
            return;
        }

        self.reconnect_failure_count = self.reconnect_failure_count.saturating_add(1);

        info!(
            "{}: Reconnect attempt #{}, next attempt in {}s",
            label,
            self.reconnect_failure_count,
            self.backoff_delay() / 1000
        );
    }

    pub fn mark_success(&mut self, label: &str) {
        if self.reconnect_failure_count > 0 {
            info!("{}: Reconnection successful, resetting backoff", label);
            self.reconnect_failure_count = 0;
        }
    }

    pub fn reset_startup_grace(&mut self) {
        self.startup_grace_deadline_ms = now_ms() + STARTUP_GRACE_MS;
    }
}
