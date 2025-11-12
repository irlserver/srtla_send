//! Classic connection selection algorithm
//!
//! This module implements the original SRTLA connection selection logic
//! that matches the C implementation exactly.
//!
//! Selection criteria:
//! - Pure capacity-based: score = window / (in_flight + 1)
//! - No quality awareness (no NAK penalties)
//! - No RTT consideration
//! - No exploration
//! - Simple "pick highest score" algorithm

use crate::connection::SrtlaConnection;

/// Select best connection using classic SRTLA algorithm
///
/// Returns the index of the connection with the highest capacity score,
/// or None if all connections are timed out or disconnected.
///
/// This matches the original C implementation's behavior exactly.
pub fn select_connection(conns: &[SrtlaConnection]) -> Option<usize> {
    let mut best_idx: Option<usize> = None;
    let mut best_score: i32 = -1;

    for (i, c) in conns.iter().enumerate() {
        if c.is_timed_out() {
            continue;
        }
        let score = c.get_score();
        if score > best_score {
            best_score = score;
            best_idx = Some(i);
        }
    }

    best_idx
}

// Tests are in src/tests/sender_tests.rs
