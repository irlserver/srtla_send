//! Classic connection selection algorithm
//!
//! This module implements the original SRTLA connection selection logic
//! that matches the C implementation exactly.
//!
//! Selection criteria:
//! - Pure capacity-based: score = window / (in_flight + 1)
//! - No quality awareness (no NAK penalties)
//! - No RTT consideration
//! - Simple "pick highest score" algorithm
//! - Optional operator link weights (Moblin's connection priorities): the score
//!   is multiplied by [`SrtlaConnection::weight_multiplier`], which is exactly
//!   1.0 for an unweighted link, so an IPs file without weights schedules as
//!   the C implementation does.

use crate::connection::SrtlaConnection;

/// Select best connection using classic SRTLA algorithm
///
/// Returns the index of the connection with the highest capacity score,
/// or None if all connections are timed out or disconnected.
///
/// This matches the original C implementation's behavior exactly.
/// No time-based dampening or hysteresis is applied in classic mode.
#[inline(always)]
pub fn select_connection(conns: &[SrtlaConnection], now_ms: u64) -> Option<usize> {
    let mut best_idx: Option<usize> = None;
    let mut best_score: i32 = -1;

    for (i, c) in conns.iter().enumerate() {
        // `stall_gated` is only ever set when a healthier link exists (see
        // `apply_stall_gate`), so skipping it here can never starve the pool.
        if c.is_timed_out(now_ms) || !c.is_schedulable() || c.stall_gated {
            continue;
        }
        let score = c.classic_weighted_score();
        if score > best_score {
            best_score = score;
            best_idx = Some(i);
        }
    }

    best_idx
}

// Tests are in src/tests/sender/sender_tests.rs
