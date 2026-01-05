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
//! - Minimal hysteresis (2%) to prevent flip-flopping
//! - Optional smart exploration
//! - Time-based switch dampening to prevent rapid thrashing

pub mod classic;
pub mod enhanced;
pub mod exploration;
pub mod quality;

// Re-export for backward compatibility
pub use quality::calculate_quality_multiplier;

use crate::connection::SrtlaConnection;

/// Minimum time in milliseconds between connection switches
/// Prevents rapid thrashing when scores fluctuate due to bursty ACK/NAK patterns
/// Works in combination with score-based hysteresis for stable connection selection
pub const MIN_SWITCH_INTERVAL_MS: u64 = 500;

/// Select the best connection index based on mode and configuration
///
/// # Arguments
/// * `conns` - Mutable slice of connections (for quality cache updates in enhanced mode)
/// * `last_idx` - Previously selected connection (for hysteresis)
/// * `last_switch_time_ms` - Time of last switch (for time-based dampening)
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `enable_quality` - Enable quality scoring (enhanced mode only)
/// * `enable_explore` - Enable exploration (enhanced mode only)
/// * `classic` - Use classic mode algorithm
///
/// # Returns
/// The index of the selected connection, or None if no valid connections
#[inline(always)]
pub fn select_connection_idx(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    last_switch_time_ms: u64,
    current_time_ms: u64,
    enable_quality: bool,
    enable_explore: bool,
    classic: bool,
) -> Option<usize> {
    if classic {
        // Classic mode: simple capacity-based selection (no dampening, matches original C)
        classic::select_connection(conns)
    } else {
        // Enhanced mode: quality-aware selection with optional exploration and time-based dampening
        enhanced::select_connection(
            conns,
            last_idx,
            last_switch_time_ms,
            current_time_ms,
            enable_quality,
            enable_explore,
        )
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

        // Classic mode should pick connection 1 (highest score) even during cooldown
        let result = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            false,
            false,
            true, // classic mode
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
        let current_time_ms = last_switch_time_ms + 100; // Within cooldown

        // Enhanced mode should stay with connection 0 due to cooldown
        let result = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_ms,
            true,
            false,
            false, // enhanced mode
        );
        assert_eq!(
            result,
            Some(0),
            "Enhanced mode should enforce cooldown and stay with current connection"
        );

        // After cooldown expires, should allow switching
        let current_time_after_cooldown = last_switch_time_ms + 600; // Past cooldown
        let result_after = select_connection_idx(
            &mut connections,
            Some(0),
            last_switch_time_ms,
            current_time_after_cooldown,
            true,
            false,
            false,
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
        let result = select_connection_idx(&mut conns, None, 0, 0, false, false, false);
        assert_eq!(result, None);
    }
}
