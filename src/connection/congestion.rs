use std::cmp::min;

use tracing::{debug, warn};

use crate::protocol::*;
use crate::utils::now_ms;

const NAK_BURST_WINDOW_MS: u64 = 1000;
const NAK_BURST_LOG_THRESHOLD: i32 = 3;
const NORMAL_UTILIZATION_THRESHOLD: f64 = 0.85;
const FAST_UTILIZATION_THRESHOLD: f64 = 0.95;
const NORMAL_ACKS_REQUIRED: i32 = 4;
const FAST_ACKS_REQUIRED: i32 = 2;
const NORMAL_MIN_WAIT_MS: u64 = 2000;
const FAST_MIN_WAIT_MS: u64 = 500;
const NORMAL_INCREMENT_WAIT_MS: u64 = 1000;
const FAST_INCREMENT_WAIT_MS: u64 = 300;
const FAST_RECOVERY_DISABLE_WINDOW: i32 = 12_000;

/// Congestion control and NAK tracking
#[derive(Debug, Clone, Default)]
pub struct CongestionControl {
    pub nak_count: i32,
    pub last_nak_time_ms: u64,
    pub last_window_increase_ms: u64,
    pub consecutive_acks_without_nak: i32,
    pub fast_recovery_mode: bool,
    pub fast_recovery_start_ms: u64,
    pub nak_burst_count: i32,
    pub nak_burst_start_time_ms: u64,
}

impl CongestionControl {
    pub fn handle_nak(&mut self, window: &mut i32, seq: i32, label: &str) -> bool {
        let current_time = now_ms();
        self.nak_count = self.nak_count.saturating_add(1);

        let time_since_last_nak = current_time.saturating_sub(self.last_nak_time_ms);

        // Track NAK bursts
        if self.last_nak_time_ms > 0 && time_since_last_nak < NAK_BURST_WINDOW_MS {
            if self.nak_burst_count == 0 {
                self.nak_burst_count = 2;
                self.nak_burst_start_time_ms = self.last_nak_time_ms;
            } else {
                self.nak_burst_count = self.nak_burst_count.saturating_add(1);
            }
        } else {
            if self.nak_burst_count >= NAK_BURST_LOG_THRESHOLD {
                let burst_duration = current_time.saturating_sub(self.nak_burst_start_time_ms);
                warn!(
                    "{}: NAK burst ended - {} NAKs in {}ms",
                    label, self.nak_burst_count, burst_duration
                );
            }
            self.nak_burst_count = 0;
            self.nak_burst_start_time_ms = 0;
        }

        self.last_nak_time_ms = current_time;
        self.consecutive_acks_without_nak = 0;

        // Reduce window
        let old_window = *window;
        *window = (*window - WINDOW_DECR).max(WINDOW_MIN * WINDOW_MULT);

        if *window <= 3000 {
            let burst_info = if self.nak_burst_count > 1 {
                format!(" [BURST: {} NAKs]", self.nak_burst_count)
            } else {
                String::new()
            };
            warn!(
                "{}: NAK reduced window {} → {} (seq={}, total_naks={}{})",
                label, old_window, *window, seq, self.nak_count, burst_info
            );
        }

        // Enter fast recovery mode if window is very low
        if *window <= 2000 && !self.fast_recovery_mode {
            self.fast_recovery_mode = true;
            self.fast_recovery_start_ms = current_time;
            warn!(
                "{}: Enabling FAST RECOVERY MODE - window {}",
                label, *window
            );
        }

        true
    }

    pub fn handle_srtla_ack_specific_classic(
        &mut self,
        window: &mut i32,
        in_flight_packets: i32,
        seq: i32,
        label: &str,
    ) {
        // CLASSIC MODE: Exact C implementation
        // Window increase logic from C version (lines 291-293)
        // Only increase if in_flight_pkts*WINDOW_MULT > window
        if in_flight_packets * WINDOW_MULT > *window {
            let old = *window;
            // Note: WINDOW_INCR - 1 in C code
            *window = min(*window + WINDOW_INCR - 1, WINDOW_MAX * WINDOW_MULT);
            debug!(
                "{}: SRTLA ACK specific increased window {} → {} (seq={}, in_flight={}) [CLASSIC]",
                label, old, *window, seq, in_flight_packets
            );
        }
    }

    pub fn handle_srtla_ack_enhanced(
        &mut self,
        window: &mut i32,
        in_flight_packets: i32,
        label: &str,
    ) {
        // Enhanced mode ACK handling with utilization thresholds and consecutive ACK
        // tracking
        let old_window = *window;
        let mut window_increased = false;
        let current_time = now_ms();

        // Only increase window if we haven't increased recently (slower recovery)
        if current_time.saturating_sub(self.last_window_increase_ms) > 200 {
            // Utilization thresholds for enhanced mode
            let utilization_threshold = if self.fast_recovery_mode {
                FAST_UTILIZATION_THRESHOLD
            } else {
                NORMAL_UTILIZATION_THRESHOLD
            };

            if (in_flight_packets as f64)
                < (*window as f64 * utilization_threshold / WINDOW_MULT as f64)
            {
                self.consecutive_acks_without_nak =
                    self.consecutive_acks_without_nak.saturating_add(1);

                // Conservative recovery - require more ACKs
                let acks_required = if self.fast_recovery_mode {
                    FAST_ACKS_REQUIRED
                } else {
                    NORMAL_ACKS_REQUIRED
                };

                if self.consecutive_acks_without_nak >= acks_required {
                    *window = min(*window + WINDOW_INCR, WINDOW_MAX * WINDOW_MULT);
                    window_increased = true;
                    self.last_window_increase_ms = current_time;
                    self.consecutive_acks_without_nak = 0; // Reset counter to prevent burst increases
                }
            }
        }

        // Log window recovery for diagnosis
        if window_increased && old_window <= 10000 {
            debug!(
                "{}: ACK increased window {} → {} (in_flight={}, consec_acks={}, fast_mode={}) \
                 [ENHANCED]",
                label,
                old_window,
                *window,
                in_flight_packets,
                self.consecutive_acks_without_nak,
                self.fast_recovery_mode
            );
        }

        if self.fast_recovery_mode && *window >= FAST_RECOVERY_DISABLE_WINDOW {
            self.fast_recovery_mode = false;
            let recovery_duration = current_time.saturating_sub(self.fast_recovery_start_ms);
            debug!(
                "{}: Disabling FAST RECOVERY MODE after enhanced ACK recovery (window={}, \
                 duration={}ms)",
                label, *window, recovery_duration
            );
        }
    }

    pub fn perform_window_recovery(&mut self, window: &mut i32, connected: bool, label: &str) {
        if !connected || *window >= WINDOW_MAX * WINDOW_MULT {
            return;
        }

        let now = now_ms();
        let time_since_last_nak = if self.last_nak_time_ms > 0 {
            Some(now.saturating_sub(self.last_nak_time_ms))
        } else {
            None
        };

        if let Some(tsn) = time_since_last_nak {
            if tsn >= NAK_BURST_WINDOW_MS && self.nak_burst_count > 0 {
                self.nak_burst_count = 0;
                self.nak_burst_start_time_ms = 0;
            }

            let min_wait_time = if self.fast_recovery_mode {
                FAST_MIN_WAIT_MS
            } else {
                NORMAL_MIN_WAIT_MS
            };
            let increment_wait = if self.fast_recovery_mode {
                FAST_INCREMENT_WAIT_MS
            } else {
                NORMAL_INCREMENT_WAIT_MS
            };

            if tsn > min_wait_time
                && now.saturating_sub(self.last_window_increase_ms) > increment_wait
            {
                let old_window = *window;
                // Conservative recovery multipliers (using cached values)
                let fast_mode_bonus = if self.fast_recovery_mode { 2 } else { 1 };

                // More conservative recovery based on how long since last NAK
                if tsn > 10_000 {
                    // No NAKs for 10+ seconds: moderate recovery (was 5s)
                    *window += WINDOW_INCR * 2 * fast_mode_bonus;
                } else if tsn > 7_000 {
                    // No NAKs for 7+ seconds: slow recovery (was 3s)
                    *window += WINDOW_INCR * fast_mode_bonus;
                } else if tsn > 5_000 {
                    // No NAKs for 5+ seconds: very slow recovery (was 1.5s)
                    *window += WINDOW_INCR * fast_mode_bonus;
                } else {
                    // Recent NAKs: minimal recovery (keep same as before)
                    *window += WINDOW_INCR * fast_mode_bonus;
                }

                *window = min(*window, WINDOW_MAX * WINDOW_MULT);
                self.last_window_increase_ms = now;

                if *window > old_window {
                    debug!(
                        "{}: Time-based window recovery {} → {} (no NAKs for {:.1}s, fast_mode={})",
                        label,
                        old_window,
                        *window,
                        (tsn as f64) / 1000.0,
                        self.fast_recovery_mode
                    );
                }

                if self.fast_recovery_mode && *window >= FAST_RECOVERY_DISABLE_WINDOW {
                    self.fast_recovery_mode = false;
                    debug!(
                        "{}: Disabling FAST RECOVERY MODE after time-based recovery (window={})",
                        label, *window
                    );
                }
            }
        }
    }

    pub fn time_since_last_nak_ms(&self) -> Option<u64> {
        if self.last_nak_time_ms == 0 {
            None
        } else {
            Some(now_ms().saturating_sub(self.last_nak_time_ms))
        }
    }
}
