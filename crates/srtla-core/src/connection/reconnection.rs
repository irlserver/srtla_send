use tracing::{debug, info};

use super::STARTUP_GRACE_MS;
use crate::config_snapshot::{RECONNECT_FAST_RETRY_ATTEMPTS, RECONNECT_FAST_RETRY_MS};
const BASE_RECONNECT_DELAY_MS: u64 = 5000;
const MAX_BACKOFF_DELAY_MS: u64 = 120_000;
const MAX_BACKOFF_COUNT: u32 = 5;

/// Reconnection state and backoff tracking
#[derive(Debug, Clone)]
pub struct ReconnectionState {
    pub last_reconnect_attempt_ms: u64,
    pub reconnect_failure_count: u32,
    pub connection_established_ms: u64,
    pub startup_grace_deadline_ms: u64,
    /// Fast-retry window for a link that WAS established (see
    /// [`RECONNECT_FAST_RETRY_MS`]). Refreshed from the runtime
    /// `ConfigSnapshot` alongside `conn_timeout_ms`.
    pub fast_retry_ms: u64,
    pub fast_retry_attempts: u32,
}

impl Default for ReconnectionState {
    fn default() -> Self {
        Self {
            last_reconnect_attempt_ms: 0,
            reconnect_failure_count: 0,
            connection_established_ms: 0,
            startup_grace_deadline_ms: 0,
            fast_retry_ms: RECONNECT_FAST_RETRY_MS,
            fast_retry_attempts: RECONNECT_FAST_RETRY_ATTEMPTS,
        }
    }
}

impl ReconnectionState {
    /// Calculate backoff delay based on failure count: `fast_retry_ms` for the
    /// first `fast_retry_attempts` failures (matching the C reference, which
    /// re-opens the socket on every housekeeping pass), then exponential from
    /// `BASE_RECONNECT_DELAY_MS`, capped at `MAX_BACKOFF_DELAY_MS`. With
    /// `fast_retry_attempts == 0` this is the plain ladder.
    fn backoff_delay(&self) -> u64 {
        if self.reconnect_failure_count < self.fast_retry_attempts {
            return self.fast_retry_ms;
        }
        let capped_failures =
            (self.reconnect_failure_count - self.fast_retry_attempts).min(MAX_BACKOFF_COUNT);
        let delay = BASE_RECONNECT_DELAY_MS.saturating_mul(1u64 << capped_failures);
        delay.min(MAX_BACKOFF_DELAY_MS)
    }

    pub fn should_attempt_reconnect(&self, now: u64) -> bool {
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

    pub fn record_attempt(&mut self, label: &str, now: u64) {
        self.last_reconnect_attempt_ms = now;

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
            "{}: Reconnect attempt #{}, next attempt in {}ms",
            label,
            self.reconnect_failure_count,
            self.backoff_delay()
        );
    }

    pub fn mark_success(&mut self, label: &str) {
        if self.reconnect_failure_count > 0 {
            info!("{}: Reconnection successful, resetting backoff", label);
            self.reconnect_failure_count = 0;
        }
    }

    pub fn reset_startup_grace(&mut self, now: u64) {
        self.startup_grace_deadline_ms = now + STARTUP_GRACE_MS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link that was established (REG3 seen) and has just timed out.
    fn established() -> ReconnectionState {
        ReconnectionState {
            connection_established_ms: 1_000,
            ..Default::default()
        }
    }

    fn established_with(fast_retry_ms: u64, fast_retry_attempts: u32) -> ReconnectionState {
        ReconnectionState {
            fast_retry_ms,
            fast_retry_attempts,
            ..established()
        }
    }

    /// Record an attempt at `t` and return the delay until the next one is
    /// allowed, asserting the boundary is exact.
    fn wait_after_attempt(state: &mut ReconnectionState, t: u64) -> u64 {
        state.record_attempt("test", t);
        let mut wait = 1;
        while !state.should_attempt_reconnect(t + wait) {
            wait += 1;
            assert!(wait <= MAX_BACKOFF_DELAY_MS, "never allowed a retry");
        }
        assert!(!state.should_attempt_reconnect(t + wait - 1));
        wait
    }

    /// The waits between the first `n` consecutive failed attempts.
    fn schedule(mut state: ReconnectionState, n: usize) -> Vec<u64> {
        let mut t = 50_000;
        let mut waits = Vec::new();
        for _ in 0..n {
            let w = wait_after_attempt(&mut state, t);
            waits.push(w);
            t += w;
        }
        waits
    }

    /// The schedule before the fast-retry window existed: `5000 << n`, capped.
    const LADDER: [u64; 8] = [
        10_000, 20_000, 40_000, 80_000, 120_000, 120_000, 120_000, 120_000,
    ];

    #[test]
    fn defaults_are_one_second_for_four_attempts() {
        let state = ReconnectionState::default();
        assert_eq!(state.fast_retry_ms, 1000);
        assert_eq!(state.fast_retry_attempts, 4);
    }

    #[test]
    fn established_link_retries_after_one_second() {
        let mut state = established();
        let t = 50_000;
        assert!(state.should_attempt_reconnect(t));
        state.record_attempt("test", t);
        assert!(!state.should_attempt_reconnect(t + 999));
        assert!(state.should_attempt_reconnect(t + 1000));
    }

    #[test]
    fn fast_window_then_exponential_ladder() {
        // Attempts 1-3 wait 1 s; the 4th recorded failure starts the ladder, so
        // the wait before the 5th attempt is 5 s, then it doubles to the cap.
        assert_eq!(
            schedule(established(), 11),
            vec![
                1000, 1000, 1000, 5000, 10_000, 20_000, 40_000, 80_000, 120_000, 120_000, 120_000
            ]
        );
    }

    #[test]
    fn fifth_attempt_waits_five_seconds() {
        let mut state = established();
        let mut t = 50_000;
        for _ in 0..3 {
            state.record_attempt("test", t);
            t += 1000;
            assert!(state.should_attempt_reconnect(t));
        }
        state.record_attempt("test", t);
        assert_eq!(state.reconnect_failure_count, 4);
        assert!(!state.should_attempt_reconnect(t + 4999));
        assert!(state.should_attempt_reconnect(t + 5000));
    }

    #[test]
    fn zero_attempts_is_the_plain_ladder() {
        // Opt-out: with no fast-retry window the schedule is exactly the
        // pre-existing `5000 << failure_count` ladder.
        assert_eq!(schedule(established_with(1000, 0), 8), LADDER.to_vec());
        assert_eq!(
            schedule(established_with(3000, 0), 8),
            LADDER.to_vec(),
            "the fast cadence must be irrelevant when the window is empty"
        );
    }

    #[test]
    fn window_is_configurable() {
        assert_eq!(
            schedule(established_with(2000, 2), 5),
            vec![2000, 5000, 10_000, 20_000, 40_000]
        );
    }

    #[test]
    fn success_resets_to_fast_retry() {
        let mut state = established();
        for i in 0..6 {
            state.record_attempt("test", 50_000 + i);
        }
        state.mark_success("test");
        assert_eq!(wait_after_attempt(&mut state, 200_000), 1000);
    }

    #[test]
    fn initial_registration_cadence_unchanged() {
        let mut state = ReconnectionState {
            startup_grace_deadline_ms: 10_000,
            fast_retry_ms: 5000,
            fast_retry_attempts: 0,
            ..Default::default()
        };
        assert!(!state.should_attempt_reconnect(10_000));
        assert!(state.should_attempt_reconnect(10_001));
        state.record_attempt("test", 10_001);
        assert_eq!(state.reconnect_failure_count, 0);
        assert!(!state.should_attempt_reconnect(11_000));
        assert!(state.should_attempt_reconnect(11_001));
    }
}
