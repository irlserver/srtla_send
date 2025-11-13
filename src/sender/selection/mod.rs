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
/// * `conns` - Slice of available connections
/// * `last_idx` - Previously selected connection (for hysteresis)
/// * `last_switch_time_ms` - Time of last switch (for time-based dampening)
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `enable_quality` - Enable quality scoring (enhanced mode only)
/// * `enable_explore` - Enable exploration (enhanced mode only)
/// * `classic` - Use classic mode algorithm
///
/// # Returns
/// The index of the selected connection, or None if no valid connections
pub fn select_connection_idx(
    conns: &[SrtlaConnection],
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

    #[test]
    fn test_select_connection_idx_classic() {
        // Test that classic mode uses classic algorithm
    }

    #[test]
    fn test_select_connection_idx_enhanced() {
        // Test that enhanced mode uses enhanced algorithm
    }

    #[test]
    fn test_select_connection_idx_empty() {
        let conns: Vec<SrtlaConnection> = vec![];
        let result = select_connection_idx(&conns, None, 0, 0, false, false, false);
        assert_eq!(result, None);
    }
}
