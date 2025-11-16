//! Classic congestion control matching original C implementation
//!
//! This module implements window management that exactly matches the
//! original SRTLA C code behavior.

use std::cmp::min;

use tracing::debug;

use crate::protocol::*;

/// Handle SRTLA ACK with specific window increase logic (classic mode)
///
/// Matches C implementation lines 291-293:
/// Only increase if in_flight_pkts*WINDOW_MULT > window
pub fn handle_srtla_ack_specific(window: &mut i32, in_flight_packets: i32, seq: i32, label: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classic_ack_increases_window() {
        let mut window = 1500;
        let in_flight = 3; // 3 * 1000 = 3000 > 1500, should increase

        handle_srtla_ack_specific(&mut window, in_flight, 100, "test");

        assert_eq!(window, 1500 + WINDOW_INCR - 1);
    }

    #[test]
    fn test_classic_ack_no_increase_when_window_high() {
        let mut window = 5000;
        let in_flight = 3; // 3 * 1000 = 3000 < 5000, should NOT increase

        handle_srtla_ack_specific(&mut window, in_flight, 100, "test");

        assert_eq!(window, 5000); // No change
    }

    #[test]
    fn test_classic_ack_respects_max_window() {
        let mut window = WINDOW_MAX * WINDOW_MULT - 10;
        let in_flight = 100; // Large enough to trigger increase

        handle_srtla_ack_specific(&mut window, in_flight, 100, "test");

        assert!(window <= WINDOW_MAX * WINDOW_MULT);
    }
}
