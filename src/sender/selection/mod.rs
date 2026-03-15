//! Connection selection strategies for SRTLA bonding
//!
//! This module provides three connection selection strategies:
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
//! - Optional smart exploration
//! - Time-based switch dampening to prevent rapid thrashing
//!
//! ## RTT-Threshold Mode
//! Groups links by RTT to reduce packet reordering:
//! - Links within min_rtt + delta are "fast"
//! - Strongly prefers fast links over slow ones
//! - Quality scoring applied within fast link group
//! - Falls back to slow links only when fast links saturated

pub mod blest;
mod classic;
pub mod edpf;
mod enhanced;
mod exploration;
pub mod iods;
mod quality;
pub mod sbd;

#[cfg(feature = "test-internals")]
pub mod rtt_threshold;
#[cfg(not(feature = "test-internals"))]
mod rtt_threshold;

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
        SchedulingMode::RttThreshold => {
            // RTT-threshold mode: prefer low-RTT links to reduce reordering
            rtt_threshold::select_connection(
                conns,
                last_idx,
                last_switch_time_ms,
                current_time_ms,
                config.rtt_delta_ms,
                config.effective_quality_enabled(),
            )
        }
        SchedulingMode::Edpf => {
            // EDPF mode: BLEST → IoDS → EDPF pipeline
            edpf_pipeline_select(conns, config)
        }
    }
}

// Thread-local SBD state shared between housekeeping (detect) and EDPF (query).
// Both run on the same tokio task so thread-local is safe and lock-free.
thread_local! {
    static SBD: std::cell::RefCell<sbd::SharedBottleneckDetector> =
        std::cell::RefCell::new(sbd::SharedBottleneckDetector::new());
}

/// Run shared bottleneck detection on the current set of connections.
///
/// Called from housekeeping once per tick. Updates the thread-local SBD
/// state that the EDPF pipeline reads during per-packet scheduling.
pub fn update_sbd(connections: &[SrtlaConnection]) {
    SBD.with(|cell| {
        cell.borrow_mut().detect(connections);
    });
}

/// EDPF pipeline: BLEST filters → SBD capacity reduction → IoDS ordering → EDPF argmin.
///
/// Matches strata's bonding.rs:30-35:
/// 1. BLEST filters out HoL-blocking links
/// 2. SBD reduces effective capacity for correlated links
/// 3. IoDS filters for monotonic ordering
/// 4. EDPF selects argmin(predicted_arrival) from remaining
fn edpf_pipeline_select(conns: &[SrtlaConnection], _config: &ConfigSnapshot) -> Option<usize> {
    const SRT_PKT_SIZE: usize = 1316;

    // Use thread-local BLEST and IoDS state
    thread_local! {
        static BLEST: std::cell::RefCell<blest::BlestFilter> =
            std::cell::RefCell::new(blest::BlestFilter::new());
        static IODS: std::cell::RefCell<iods::IodsFilter> =
            std::cell::RefCell::new(iods::IodsFilter::new());
    }

    BLEST.with(|blest_cell| {
        IODS.with(|iods_cell| {
            SBD.with(|sbd_cell| {
                let mut blest_filter = blest_cell.borrow_mut();
                let mut iods_filter = iods_cell.borrow_mut();
                let sbd_detector = sbd_cell.borrow();

                blest_filter.tick();

                // 1. BLEST filters out HoL-blocking links
                let candidates = blest_filter.filter(conns);

                // 2 & 3. IoDS + EDPF with SBD-aware arrival times.
                // When a link is part of a correlated group, its effective
                // capacity is reduced, increasing predicted arrival time.
                let sbd_factor = sbd_detector.capacity_reduction_factor();
                let arrival_fn = |idx: usize| {
                    let base = edpf::arrival_time(&conns[idx], SRT_PKT_SIZE)?;
                    if sbd_detector.is_correlated(idx) {
                        // Reduce effective capacity → increase arrival time.
                        // arrival ≈ (in_flight + pkt) / capacity + propagation
                        // Dividing the capacity portion by the factor is equivalent
                        // to multiplying the total arrival by 1/factor, but we
                        // use a simpler inflate: arrival / factor.
                        Some(base / sbd_factor)
                    } else {
                        Some(base)
                    }
                };

                let ordered = iods_filter.filter_valid(&candidates, arrival_fn);

                // EDPF selects argmin from SBD-adjusted arrivals, with fallbacks
                let selected =
                    select_sbd_adjusted(conns, &ordered, SRT_PKT_SIZE, &sbd_detector, sbd_factor)
                        .or_else(|| {
                            select_sbd_adjusted(
                                conns,
                                &candidates,
                                SRT_PKT_SIZE,
                                &sbd_detector,
                                sbd_factor,
                            )
                        })
                        .or_else(|| {
                            // Final fallback: all connections, SBD-adjusted
                            let all_indices: Vec<usize> = (0..conns.len()).collect();
                            select_sbd_adjusted(
                                conns,
                                &all_indices,
                                SRT_PKT_SIZE,
                                &sbd_detector,
                                sbd_factor,
                            )
                        });

                // Record the scheduled arrival for IoDS (use base arrival, not adjusted)
                if let Some(idx) = selected
                    && let Some(arrival) = edpf::arrival_time(&conns[idx], SRT_PKT_SIZE)
                {
                    iods_filter.record_scheduled(arrival);
                }

                selected
            })
        })
    })
}

/// Select the connection with lowest SBD-adjusted predicted arrival from a subset.
fn select_sbd_adjusted(
    conns: &[SrtlaConnection],
    indices: &[usize],
    pkt_size: usize,
    sbd_detector: &sbd::SharedBottleneckDetector,
    sbd_factor: f64,
) -> Option<usize> {
    let mut best_idx = None;
    let mut best_arrival = f64::MAX;

    for &i in indices {
        if i < conns.len()
            && let Some(mut arrival) = edpf::arrival_time(&conns[i], pkt_size)
        {
            if sbd_detector.is_correlated(i) {
                arrival /= sbd_factor;
            }
            if arrival < best_arrival {
                best_arrival = arrival;
                best_idx = Some(i);
            }
        }
    }

    best_idx
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
            rtt_delta_ms: 30,
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
            rtt_delta_ms: 30,
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
            rtt_delta_ms: 30,
        };
        let result = select_connection_idx(&mut conns, None, 0, 0, &config);
        assert_eq!(result, None);
    }
}
