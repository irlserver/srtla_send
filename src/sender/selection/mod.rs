//! Connection selection strategies for SRTLA bonding
//!
//! This module provides two connection selection strategies:
//!
//! ## Classic Mode
//! Matches the original C implementation exactly:
//! - Simple capacity-based selection
//! - No quality awareness
//! - Pure "pick highest window/(in_flight+1)" algorithm
//!
//! ## Enhanced Mode
//! Improved selection with quality awareness:
//! - Exponential NAK decay (smooth ~8s recovery)
//! - NAK burst detection and penalties
//! - RTT-aware scoring (small bonus for low latency)
//! - Hysteresis (10%) to prevent flip-flopping
//! - Time-based switch dampening to prevent rapid thrashing

mod classic;
pub mod classifier;
pub mod enhanced;
mod exploration;
pub mod link_cc;
mod quality;

// Re-export for backward compatibility
pub use quality::calculate_quality_multiplier;

use crate::config::ConfigSnapshot;
use crate::connection::SrtlaConnection;
use crate::mode::SchedulingMode;

/// Minimum time in milliseconds between connection switches
/// Prevents rapid thrashing when scores fluctuate due to bursty ACK/NAK patterns.
/// Aligned with FLUSH_INTERVAL_MS (15ms) so connections can rotate between batches
/// while avoiding intra-batch flip-flopping.
pub const MIN_SWITCH_INTERVAL_MS: u64 = 15;

/// Select the best connection index based on mode and configuration
///
/// # Arguments
/// * `conns` - Mutable slice of connections (for quality cache updates in enhanced mode)
/// * `last_idx` - Previously selected connection (for hysteresis)
/// * `last_switch_time_ms` - Time of last switch (for time-based dampening)
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `config` - Configuration snapshot with mode and settings
///
/// # Returns
/// The index of the selected connection, or None if no valid connections
#[inline(always)]
pub fn select_connection_idx(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    last_switch_time_ms: u64,
    current_time_ms: u64,
    config: &ConfigSnapshot,
) -> Option<usize> {
    // Stalled-link deselect (default on). A link is gated only when it is a
    // stalled black hole AND at least one healthier link can carry the traffic,
    // so the last usable link is never gated — the mode selectors then skip
    // `stall_gated` links exactly as they skip timed-out ones. Gating is a pure
    // selection penalty: a gated link keeps sending keepalives, and its next
    // keepalive-RTT sample clears the stall on its own (no blind reprobe).
    // Recomputed for every link on every call so the transient flag can never go
    // stale; collapses to clearing the flag when the guard is off.
    apply_stall_gate(conns, current_time_ms, config);

    match config.mode {
        SchedulingMode::Classic => {
            // Classic mode: simple capacity-based selection (no dampening, matches original C)
            classic::select_connection(conns)
        }
        SchedulingMode::Enhanced => {
            // Enhanced mode: quality-aware selection with optional exploration and time-based dampening
            enhanced::select_connection(
                conns,
                last_idx,
                last_switch_time_ms,
                current_time_ms,
                config.effective_quality_enabled(),
                config.effective_exploration_enabled(),
            )
        }
    }
}

/// Recompute the transient `stall_gated` flag on every link.
///
/// A link is gated when the guard is on, the link is stalled
/// ([`SrtlaConnection::is_stalled`]), and at least one non-stalled schedulable
/// link exists to carry the traffic. That "any healthy" guard guarantees we
/// never gate the last usable link, so the mode selectors can treat a gated
/// link as unschedulable without a fallback pass. When the guard is off (or no
/// link is stalled) every flag is cleared, restoring byte-for-byte baseline
/// selection.
#[inline]
fn apply_stall_gate(conns: &mut [SrtlaConnection], current_time_ms: u64, config: &ConfigSnapshot) {
    let min_in_flight = config.stall_min_in_flight;
    let stale_ms = config.stall_ack_stale_ms;

    let any_healthy = config.stall_deselect
        && conns.iter().any(|c| {
            !c.is_timed_out()
                && c.is_schedulable()
                && !c.is_stalled(current_time_ms, min_in_flight, stale_ms)
        });

    for c in conns.iter_mut() {
        // Short-circuit keeps `is_stalled` off the hot path when the guard is
        // off or nothing healthy exists to fail over to.
        c.stall_gated = any_healthy && c.is_stalled(current_time_ms, min_in_flight, stale_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_connections;
    use crate::utils::now_ms;

    #[test]
    fn test_select_connection_idx_classic() {
        // Test that classic mode always picks highest score, ignoring dampening
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Highest score
        connections[2].in_flight_packets = 10; // Lowest score

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 100; // Within cooldown

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };

        // Classic mode should pick connection 1 (highest score) even during cooldown
        let result = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );
        assert_eq!(
            result,
            Some(1),
            "Classic mode should pick highest score connection"
        );
    }

    #[test]
    fn test_select_connection_idx_enhanced() {
        // Test that enhanced mode enforces cooldown dampening
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        connections[0].in_flight_packets = 5; // Currently selected, lower score
        connections[1].in_flight_packets = 0; // Highest score
        connections[2].in_flight_packets = 10; // Lowest score

        let last_switch_time_ms = now_ms();
        let current_time_ms = last_switch_time_ms + 5; // Within 15ms cooldown

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };

        // Enhanced mode should stay with connection 0 due to cooldown
        let result = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            &config,
        );
        assert_eq!(
            result,
            Some(0),
            "Enhanced mode should enforce cooldown and stay with current connection"
        );

        // After cooldown expires, should allow switching
        let current_time_after_cooldown = last_switch_time_ms + 20; // Past 15ms cooldown
        let result_after = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_after_cooldown,
            &config,
        );
        assert_eq!(
            result_after,
            Some(1),
            "Enhanced mode should allow switching after cooldown expires"
        );
    }

    #[test]
    fn test_select_connection_idx_empty() {
        let mut conns: Vec<SrtlaConnection> = vec![];
        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            exploration_enabled: false,
            ..ConfigSnapshot::default()
        };
        let result = select_connection_idx(&mut conns, None, 0, 0, &config);
        assert_eq!(result, None);
    }
}
