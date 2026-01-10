//! Enhanced congestion control with fast recovery
//!
//! This module implements enhanced window management with:
//! - Same base window growth as classic mode
//! - Fast recovery mode for severe congestion
//! - Time-based progressive window recovery

use std::cmp::min;

use tracing::debug;

use crate::protocol::*;
use crate::utils::now_ms;

const NORMAL_MIN_WAIT_MS: u64 = 2000;
const FAST_MIN_WAIT_MS: u64 = 500;
const NORMAL_INCREMENT_WAIT_MS: u64 = 1000;
const FAST_INCREMENT_WAIT_MS: u64 = 300;
const FAST_RECOVERY_DISABLE_WINDOW: i32 = 12_000;

/// Handle SRTLA ACK with enhanced window management
///
/// Uses identical window growth logic to classic mode, but supports
/// fast recovery mode for better congestion handling.
pub fn handle_srtla_ack(
    window: &mut i32,
    in_flight_packets: i32,
    fast_recovery_mode: &mut bool,
    fast_recovery_start_ms: u64,
    label: &str,
) {
    // Enhanced mode: IDENTICAL window growth to classic mode
    // The only difference from classic is quality scoring in connection selection
    // This prevents thrashing while still avoiding bad connections

    // Use exact classic logic for window increase
    if in_flight_packets * WINDOW_MULT > *window {
        let old = *window;
        *window = min(*window + WINDOW_INCR - 1, WINDOW_MAX * WINDOW_MULT);

        if old != *window && old <= 10000 {
            debug!(
                "{}: ACK increased window {} → {} (in_flight={}, fast_mode={}) [ENHANCED]",
                label, old, *window, in_flight_packets, *fast_recovery_mode
            );
        }
    }

    // Fast recovery mode helps connections recover from severe congestion
    let current_time = now_ms();
    if *fast_recovery_mode && *window >= FAST_RECOVERY_DISABLE_WINDOW {
        *fast_recovery_mode = false;
        let recovery_duration = current_time.saturating_sub(fast_recovery_start_ms);
        debug!(
            "{}: Disabling FAST RECOVERY MODE after enhanced ACK recovery (window={}, \
             duration={}ms)",
            label, *window, recovery_duration
        );
    }
}

/// Perform time-based window recovery (enhanced mode only)
///
/// Progressively recovers window size based on time since last NAK:
/// - 10s+ no NAKs: aggressive recovery (200% rate)
/// - 7s+: moderate recovery (100% rate)
/// - 5s+: slow recovery (50% rate)
/// - <5s: minimal recovery (25% rate)
#[allow(clippy::too_many_arguments)]
pub fn perform_window_recovery(
    window: &mut i32,
    connected: bool,
    last_nak_time_ms: u64,
    nak_burst_count: &mut i32,
    nak_burst_start_time_ms: &mut u64,
    last_window_increase_ms: &mut u64,
    fast_recovery_mode: &mut bool,
    label: &str,
) {
    if !connected || *window >= WINDOW_MAX * WINDOW_MULT {
        return;
    }

    let now = now_ms();

    // Treat connections that never had NAKs as perfect connections.
    // Previously, last_nak_time_ms == 0 would skip recovery entirely, causing
    // connections to get stuck at low windows after reconnection if they
    // don't receive enough traffic for ACK-based growth.
    // Now we treat "never had NAK" as equivalent to "very long since NAK"
    // which enables aggressive recovery for these healthy connections.
    let time_since_last_nak = if last_nak_time_ms > 0 {
        now.saturating_sub(last_nak_time_ms)
    } else {
        // Never had a NAK - treat as perfect connection (very long time since NAK)
        // Use a large value that triggers aggressive recovery (>10s threshold)
        u64::MAX
    };

    // Clear NAK burst tracking if enough time has passed
    const NAK_BURST_WINDOW_MS: u64 = 1000;
    if time_since_last_nak >= NAK_BURST_WINDOW_MS && *nak_burst_count > 0 {
        *nak_burst_count = 0;
        *nak_burst_start_time_ms = 0;
    }

    let min_wait_time = if *fast_recovery_mode {
        FAST_MIN_WAIT_MS
    } else {
        NORMAL_MIN_WAIT_MS
    };
    let increment_wait = if *fast_recovery_mode {
        FAST_INCREMENT_WAIT_MS
    } else {
        NORMAL_INCREMENT_WAIT_MS
    };

    if time_since_last_nak > min_wait_time
        && now.saturating_sub(*last_window_increase_ms) > increment_wait
    {
        let old_window = *window;
        // Conservative recovery multipliers (using cached values)
        let fast_mode_bonus = if *fast_recovery_mode { 2 } else { 1 };

        // Progressive recovery based on how long since last NAK
        if time_since_last_nak > 10_000 {
            // No NAKs for 10+ seconds (or never): aggressive recovery (200% rate)
            *window += WINDOW_INCR * 2 * fast_mode_bonus;
        } else if time_since_last_nak > 7_000 {
            // No NAKs for 7+ seconds: moderate recovery (100% rate)
            *window += WINDOW_INCR * fast_mode_bonus;
        } else if time_since_last_nak > 5_000 {
            // No NAKs for 5+ seconds: slow recovery (50% rate)
            *window += WINDOW_INCR * fast_mode_bonus / 2;
        } else {
            // Recent NAKs: minimal recovery (25% rate)
            *window += WINDOW_INCR * fast_mode_bonus / 4;
        }

        *window = min(*window, WINDOW_MAX * WINDOW_MULT);
        *last_window_increase_ms = now;

        if *window > old_window {
            let time_str = if last_nak_time_ms == 0 {
                "never".to_string()
            } else {
                format!("{:.1}s", (time_since_last_nak as f64) / 1000.0)
            };
            debug!(
                "{}: Time-based window recovery {} → {} (last NAK: {}, fast_mode={})",
                label, old_window, *window, time_str, *fast_recovery_mode
            );
        }

        if *fast_recovery_mode && *window >= FAST_RECOVERY_DISABLE_WINDOW {
            *fast_recovery_mode = false;
            debug!(
                "{}: Disabling FAST RECOVERY MODE after time-based recovery (window={})",
                label, *window
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enhanced_ack_increases_window() {
        let mut window = 1500;
        let in_flight = 3;
        let mut fast_recovery = false;

        handle_srtla_ack(&mut window, in_flight, &mut fast_recovery, 0, "test");

        assert_eq!(window, 1500 + WINDOW_INCR - 1);
    }

    #[test]
    fn test_enhanced_ack_disables_fast_recovery() {
        let mut window = FAST_RECOVERY_DISABLE_WINDOW - 100;
        let in_flight = 100;
        let mut fast_recovery = true;
        let start_time = now_ms();

        // Increase window enough to trigger fast recovery disable
        for _ in 0..20 {
            handle_srtla_ack(
                &mut window,
                in_flight,
                &mut fast_recovery,
                start_time,
                "test",
            );
            if !fast_recovery {
                break;
            }
        }

        assert!(!fast_recovery);
    }

    #[test]
    fn test_window_recovery_progressive() {
        // Test that recovery rate increases with time since NAK
        let mut window = 5000;
        let last_nak = now_ms() - 10_500; // 10.5 seconds ago
        let mut nak_burst_count = 0;
        let mut nak_burst_start = 0;
        let mut last_increase = 0;
        let mut fast_recovery = false;

        perform_window_recovery(
            &mut window,
            true,
            last_nak,
            &mut nak_burst_count,
            &mut nak_burst_start,
            &mut last_increase,
            &mut fast_recovery,
            "test",
        );

        // Should have increased (aggressive recovery for 10s+)
        assert!(window > 5000);
    }

    #[test]
    fn test_window_recovery_with_no_nak_history() {
        // Test that connections that never had NAKs still get window recovery.
        // This prevents connections from getting stuck at low windows after
        // reconnection when they don't receive enough traffic for ACK-based growth.
        let mut window = 5000;
        let last_nak = 0; // Never had a NAK
        let mut nak_burst_count = 0;
        let mut nak_burst_start = 0;
        let mut last_increase = 0;
        let mut fast_recovery = false;

        perform_window_recovery(
            &mut window,
            true,
            last_nak,
            &mut nak_burst_count,
            &mut nak_burst_start,
            &mut last_increase,
            &mut fast_recovery,
            "test",
        );

        // Should have increased with aggressive recovery (treated as perfect connection)
        assert!(
            window > 5000,
            "Window should grow for connections with no NAK history, got {}",
            window
        );
        // Should get aggressive recovery rate (WINDOW_INCR * 2 = 60)
        assert_eq!(
            window,
            5000 + WINDOW_INCR * 2,
            "Should use aggressive recovery for no-NAK connections"
        );
    }

    #[test]
    fn test_window_recovery_no_nak_respects_increment_wait() {
        // Test that even no-NAK connections respect the increment wait time
        let mut window = 5000;
        let last_nak = 0; // Never had a NAK
        let mut nak_burst_count = 0;
        let mut nak_burst_start = 0;
        let mut last_increase = now_ms(); // Just increased
        let mut fast_recovery = false;

        perform_window_recovery(
            &mut window,
            true,
            last_nak,
            &mut nak_burst_count,
            &mut nak_burst_start,
            &mut last_increase,
            &mut fast_recovery,
            "test",
        );

        // Should NOT have increased (increment wait not elapsed)
        assert_eq!(
            window, 5000,
            "Window should not grow if increment wait hasn't elapsed"
        );
    }
}
