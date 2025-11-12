//! Connection exploration logic for enhanced mode
//!
//! This module implements smart exploration to discover better connections when:
//! - Current best connection is degrading (recent NAKs)
//! - Alternative connections have recovered from previous issues
//! - Periodic fallback exploration (safety net)

use tracing::debug;

use crate::connection::SrtlaConnection;
use crate::utils::elapsed_ms;

/// Determine if we should explore alternative connections
///
/// Returns true when exploration is likely to discover better options:
/// - Best connection has recent NAKs AND second-best has recovered
/// - Periodic exploration every 30s for 300ms (safety net)
pub fn should_explore_now(
    conns: &[SrtlaConnection],
    best_idx: Option<usize>,
    second_idx: Option<usize>,
) -> bool {
    // Need both best and second-best connections to explore
    let (best_idx, second_idx) = match (best_idx, second_idx) {
        (Some(b), Some(s)) => (b, s),
        _ => return false, // Not enough connections
    };

    if best_idx >= conns.len() || second_idx >= conns.len() {
        return false;
    }

    let best_conn = &conns[best_idx];
    let second_conn = &conns[second_idx];

    // Condition 1: Current best has recent NAKs (degrading)
    let best_degraded = best_conn
        .time_since_last_nak_ms()
        .map(|t| t < 3000)
        .unwrap_or(false);

    // Condition 2: Second-best has recovered from NAKs (potentially improved)
    let second_recovered = second_conn
        .time_since_last_nak_ms()
        .map(|t| t > 5000)
        .unwrap_or(true); // No NAKs = recovered

    // Condition 3: Periodic exploration as fallback (every 30s for 300ms)
    let periodic_exploration = (elapsed_ms() % 30000) < 300;

    // Explore if best is degraded AND second has recovered, OR periodic fallback
    let should_explore = (best_degraded && second_recovered) || periodic_exploration;

    if should_explore {
        debug!("Exploration: trying second-best connection");
    }

    should_explore
}

// Tests are in src/tests/sender_tests.rs
