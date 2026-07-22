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

mod classic;
pub mod classifier;
pub mod enhanced;
pub mod link_cc;
mod quality;

// Re-export for backward compatibility
pub use quality::calculate_quality_multiplier;

use crate::config_snapshot::ConfigSnapshot;
use crate::connection::SrtlaConnection;
use crate::mode::SchedulingMode;

/// Select the best connection index based on mode and configuration
///
/// # Arguments
/// * `conns` - Mutable slice of connections (for quality cache updates in enhanced mode)
/// * `last_idx` - Previously selected connection (for hysteresis)
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `config` - Configuration snapshot with mode and settings
///
/// # Returns
/// The index of the selected connection, or None if no valid connections
#[inline(always)]
pub fn select_connection_idx(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    current_time_ms: u64,
    config: &ConfigSnapshot,
) -> Option<usize> {
    // Stalled-link deselect (default on). A link is gated only when its stall
    // latch is engaged AND at least one healthier link can carry the traffic,
    // so the last usable link is never gated — the mode selectors then skip
    // `stall_gated` links exactly as they skip timed-out ones. Gating is a pure
    // selection penalty: a gated link keeps sending keepalives plus a 1-in-N
    // duplicate-packet trickle (shell-side, see `handle_srt_packet`), and the
    // latch releases after a sustained run of fresh delivery proof (asymmetric
    // hysteresis — no blind reprobe, no single-sample flap). Recomputed for
    // every link on every call so the flag can never go stale; collapses to
    // clearing flag and latch when the guard is off.
    apply_stall_gate(conns, current_time_ms, config);

    match config.mode {
        SchedulingMode::Classic => {
            // Classic mode: simple capacity-based selection (no dampening, matches original C)
            classic::select_connection(conns, current_time_ms)
        }
        SchedulingMode::Enhanced => {
            // Enhanced mode: quality-aware selection with score hysteresis.
            enhanced::select_connection(
                conns,
                last_idx,
                current_time_ms,
                config.effective_quality_enabled(),
            )
        }
    }
}

/// Drive every link's stall latch and fast silence pull, and recompute its
/// `stall_gated` flag.
///
/// Two tiers share the flag. The sticky latch
/// ([`SrtlaConnection::update_stall_latch`]) engages the instant
/// [`SrtlaConnection::is_stalled`] fires and releases only after a sustained
/// run of fresh delivery proof. The fast silence pull
/// ([`SrtlaConnection::is_briefly_silent`]) is transient: a loaded link that
/// has heard nothing for ~2 RTTs is skipped right now and readmitted the
/// moment any byte arrives — it reacts an order of magnitude sooner than the
/// latch, so a sub-second stall (a HARQ pause, a handover) stops receiving
/// payload almost immediately instead of for the whole staleness window.
///
/// A link is gated only when at least one un-latched, un-pulled schedulable
/// link exists to carry the traffic. That "any healthy" guard guarantees we
/// never gate the last usable link, so the mode selectors can treat a gated
/// link as unschedulable without a fallback pass. When the guard is off,
/// flag, latch, and pull are all cleared, restoring byte-for-byte baseline
/// selection.
///
/// Also refreshes each link's `conn_timeout_ms` from the snapshot, so the
/// runtime-tunable liveness window reaches `is_timed_out` callers that do
/// not carry a config.
#[inline]
fn apply_stall_gate(conns: &mut [SrtlaConnection], current_time_ms: u64, config: &ConfigSnapshot) {
    for c in conns.iter_mut() {
        c.conn_timeout_ms = config.conn_timeout_ms;
    }

    if !config.stall_deselect {
        for c in conns.iter_mut() {
            c.stall_gated = false;
            c.silence_pulled = false;
            c.clear_stall_latch();
        }
        return;
    }

    let min_in_flight = config.stall_min_in_flight;
    let stale_ceiling_ms = config.stall_ack_stale_ms;

    for c in conns.iter_mut() {
        // Pull first: the latch's escalation path reads the fresh pull state.
        c.update_silence_pull(current_time_ms, min_in_flight, stale_ceiling_ms);
        c.update_stall_latch(current_time_ms, min_in_flight, stale_ceiling_ms);
    }

    let any_healthy = conns.iter().any(|c| {
        !c.is_timed_out(current_time_ms)
            && c.is_schedulable()
            && !c.stall_latched()
            && !c.silence_pulled
    });

    for c in conns.iter_mut() {
        c.stall_gated = any_healthy && (c.stall_latched() || c.silence_pulled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_connections;
    use crate::utils::now_ms;

    #[test]
    fn test_select_connection_idx_classic() {
        // Test that classic mode always picks highest score
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        connections[0].in_flight_packets = 5; // Lower score
        connections[1].in_flight_packets = 0; // Highest score
        connections[2].in_flight_packets = 10; // Lowest score

        let config = ConfigSnapshot {
            mode: SchedulingMode::Classic,
            quality_enabled: false,
            ..ConfigSnapshot::default()
        };

        let result = select_connection_idx(&mut connections, Some(0), now_ms(), &config);
        assert_eq!(
            result,
            Some(1),
            "Classic mode should pick highest score connection"
        );
    }

    #[test]
    fn test_enhanced_switches_immediately_when_clearly_better() {
        // Regression guard for the removed switch cooldown. Selection must be
        // free to re-decide on every packet: `get_score()` counts queued packets
        // as in-flight, so routing a packet de-prioritises its own link, and that
        // feedback loop is what bounds per-link queue depth. A time-based lock
        // would defer the switch below and let in-flight run away on link 0.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        connections[0].in_flight_packets = 5; // Currently selected, lower score
        connections[1].in_flight_packets = 0; // Far better score
        connections[2].in_flight_packets = 10; // Lowest score

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        };

        // Immediately after having selected link 0, with no elapsed time at all.
        let result = select_connection_idx(&mut connections, Some(0), now_ms(), &config);
        assert_eq!(
            result,
            Some(1),
            "Enhanced mode must switch to a clearly better link with no time-based delay"
        );
    }

    #[test]
    fn test_enhanced_hysteresis_holds_when_gain_is_marginal() {
        // Switching is damped in score space, not time: a link that is better by
        // less than SWITCH_THRESHOLD (10%) does not win the packet.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut connections = rt.block_on(create_test_connections(3));

        // score = window / (in_flight + 1), so 20 vs 19 in flight is only a ~5%
        // improvement -- inside the hysteresis band.
        connections[0].in_flight_packets = 20; // currently selected
        connections[1].in_flight_packets = 19; // marginally better
        connections[2].in_flight_packets = 40; // clearly worse

        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            ..ConfigSnapshot::default()
        };

        let result = select_connection_idx(&mut connections, Some(0), now_ms(), &config);
        assert_eq!(
            result,
            Some(0),
            "Enhanced mode should hold the current link when the alternative is <10% better"
        );
    }

    #[test]
    fn test_select_connection_idx_empty() {
        let mut conns: Vec<SrtlaConnection> = vec![];
        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: false,
            ..ConfigSnapshot::default()
        };
        let result = select_connection_idx(&mut conns, None, 0, &config);
        assert_eq!(result, None);
    }
}
