//! Enhanced connection selection algorithm
//!
//! This module implements the enhanced SRTLA connection selection with:
//! - Quality-aware scoring based on NAK history
//! - RTT-aware bonuses for low-latency connections
//! - Score hysteresis to prevent flip-flopping (10%)
//! - Optional smart exploration of alternative connections
//!
//! The enhanced mode provides better connection quality awareness while
//! maintaining natural load distribution across all connections.

use tracing::debug;

use super::MIN_SWITCH_INTERVAL_MS;
use super::exploration::should_explore_now;
use crate::connection::SrtlaConnection;

/// Switching hysteresis: require new connection to be meaningfully better.
/// At 10%, this prevents noise-driven flip-flopping between connections with
/// similar scores while still allowing switches when one connection genuinely
/// degrades (e.g., higher in_flight due to congestion or packet loss).
const SWITCH_THRESHOLD: f64 = 1.10; // New connection must be 10% better

/// Floor on the per-link CC soft-cap multiplier. A link whose measured
/// throughput has saturated its `cc_target_bps` gets its score scaled
/// down to this fraction rather than zero — keeps a little keepalive
/// traffic flowing so the CC controller can still observe RTT and
/// loss for the recovery decision.
const CC_SOFT_CAP_FLOOR: f64 = 0.10;

/// Compute the CC soft-cap multiplier for a connection. Reads
/// `cc_target_bps` (set by `LinkCcController::tick_all`) and the
/// connection's measured bitrate; returns a value in `[CC_SOFT_CAP_FLOOR, 1.0]`
/// that the caller folds into the link's score.
///
/// Returns `1.0` (no cap) when:
/// - the CC controller hasn't published a target yet (`cc_target_bps == 0`),
/// - or measured throughput on this link is zero (idle link, plenty of headroom).
fn cc_soft_cap_multiplier(conn: &SrtlaConnection) -> f64 {
    let cap = conn.cc_target_bps;
    if cap == 0 {
        return 1.0;
    }
    let measured = conn.bitrate.current_bitrate_bps;
    if measured <= 0.0 {
        return 1.0;
    }
    let cap_f = cap as f64;
    let headroom = (cap_f - measured).max(0.0);
    (headroom / cap_f).clamp(CC_SOFT_CAP_FLOOR, 1.0)
}

/// Select best connection using enhanced algorithm with quality awareness
///
/// Returns the index of the connection with the best quality-adjusted score.
/// Implements time-based switch dampening to prevent rapid thrashing.
///
/// IMPORTANT: This function is called for EACH incoming SRT packet. The returned
/// connection index determines where that packet (and subsequent packets) will be routed.
/// Time-based dampening prevents changing the routing decision too frequently, ensuring
/// all packets continue flowing through the same connection during the cooldown period.
/// This is NOT a per-packet round-robin - it's a per-packet "best connection" selection
/// with dampening to prevent rapid switching under bursty network conditions.
///
/// # Arguments
/// * `conns` - Mutable slice of available connections (for quality cache updates)
/// * `last_idx` - Previously selected connection index (for hysteresis)
/// * `last_switch_time_ms` - Timestamp of last connection switch
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `enable_quality` - Whether to apply quality scoring
/// * `enable_explore` - Whether to enable smart exploration
#[inline(always)]
pub fn select_connection(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    last_switch_time_ms: u64,
    current_time_ms: u64,
    enable_quality: bool,
    enable_explore: bool,
) -> Option<usize> {
    // First pass: discover whether at least one non-weak connection
    // can carry the packet. The classifier marks links weak when their
    // RTT busts the chosen delay tier, when they fall below the
    // entering throughput-share threshold, or (in shadow-mode-promoted
    // form) when their CC is backing off on observed loss. If any
    // non-weak link is schedulable, the weak ones are excluded from
    // ranking. Otherwise we fall back to the full pool — better to
    // send on a weak link than to drop the packet.
    let any_non_weak_schedulable = conns
        .iter()
        .any(|c| !c.is_timed_out() && c.is_schedulable() && !c.weak && !c.cc_backing_off);

    // Score connections by base score; apply quality multiplier if enabled
    let mut best_idx: Option<usize> = None;
    let mut second_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut second_score: f64 = -1.0;
    let mut current_score: Option<f64> = None;

    for (i, c) in conns.iter_mut().enumerate() {
        if c.is_timed_out() || !c.is_schedulable() {
            continue;
        }
        if any_non_weak_schedulable && (c.weak || c.cc_backing_off) {
            continue;
        }
        let base = c.get_score() as f64;
        let cap_mult = cc_soft_cap_multiplier(c);
        let score = if !enable_quality {
            base * cap_mult
        } else {
            // Use cached quality multiplier (recalculates every 50ms)
            let quality_mult = c.get_cached_quality_multiplier(current_time_ms);
            let final_score = base * quality_mult * cap_mult;

            // Log quality issues and recoveries for debugging (cold path)
            log_quality_state(c, quality_mult, base, final_score);

            final_score
        };

        // Track current connection's score for hysteresis
        if Some(i) == last_idx {
            current_score = Some(score);
        }

        if score > best_score {
            second_score = best_score;
            second_idx = best_idx;
            best_score = score;
            best_idx = Some(i);
        } else if score > second_score {
            second_score = score;
            second_idx = Some(i);
        }
    }

    // Time-based switch dampening: prevent rapid thrashing under bursty scores
    // Check if we're within the minimum switch interval
    let time_since_last_switch_ms = current_time_ms.saturating_sub(last_switch_time_ms);
    let in_switch_cooldown = time_since_last_switch_ms < MIN_SWITCH_INTERVAL_MS;

    if let Some(last) = last_idx {
        // If proposing a different connection
        if best_idx != Some(last) {
            // Check if last connection is still valid
            let last_still_valid = last < conns.len()
                && !conns[last].is_timed_out()
                && conns[last].connected
                && conns[last].is_schedulable();

            // If in cooldown period and last connection is still valid, keep it
            if in_switch_cooldown && last_still_valid {
                debug!(
                    "Switch dampening: staying with current connection (cooldown: {}ms remaining)",
                    MIN_SWITCH_INTERVAL_MS.saturating_sub(time_since_last_switch_ms)
                );
                return Some(last);
            }

            // Apply score-based hysteresis if not in cooldown
            // If current connection is still valid and new best isn't significantly better
            if let Some(current) = current_score
                && best_score < current * SWITCH_THRESHOLD
            {
                // Only log occasionally to reduce spam
                if current_time_ms % 1000 < 10 {
                    debug!(
                        "Score hysteresis: staying with current connection (current: {:.1}, best: \
                         {:.1}, threshold: {:.1})",
                        current,
                        best_score,
                        current * SWITCH_THRESHOLD
                    );
                }
                return Some(last);
            }
        }
    }

    // Apply exploration if enabled (but respect cooldown to avoid rapid switching)
    let explore_now = if enable_explore && !in_switch_cooldown {
        should_explore_now(conns, best_idx, second_idx)
    } else {
        false
    };

    if explore_now {
        // Exploration wants to try second-best, but only if different from current
        if let (Some(second), Some(last)) = (second_idx, last_idx)
            && second != last
        {
            debug!("Exploration: trying second-best connection");
            return second_idx.or(best_idx);
        }
        // If second is same as current, just use best
        best_idx
    } else {
        best_idx
    }
}

/// Log quality state for debugging (cold path, marked for optimizer hints)
#[cold]
#[inline(never)]
fn log_quality_state(c: &SrtlaConnection, quality_mult: f64, base: f64, final_score: f64) {
    if quality_mult < 0.8 {
        debug!(
            "{} quality degraded: {:.2} (NAKs: {}, last: {}ms ago, burst: {}) base: {} → final: {}",
            c.label,
            quality_mult,
            c.total_nak_count(),
            c.time_since_last_nak_ms().unwrap_or(0),
            c.nak_burst_count(),
            base as i32,
            final_score as i32
        );
    } else if quality_mult < 1.0 && c.nak_burst_count() > 0 {
        debug!(
            "{} quality recovering: {:.2} (burst: {})",
            c.label,
            quality_mult,
            c.nak_burst_count()
        );
    }
}

// Most enhanced-mode integration tests live in src/tests/sender_tests.rs;
// the pure cap-helper unit tests sit here so they don't drag in the
// async runtime needed to spin up test connections.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::SrtlaConnection;
    use crate::test_helpers::create_test_connections;

    fn one_conn() -> SrtlaConnection {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(create_test_connections(1)).pop().unwrap()
    }

    #[test]
    fn cap_no_signal_returns_unity() {
        let c = one_conn();
        // cc_target_bps default 0 → no cap.
        assert!((cc_soft_cap_multiplier(&c) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cap_idle_link_returns_unity() {
        let mut c = one_conn();
        c.cc_target_bps = 1_000_000;
        c.bitrate.current_bitrate_bps = 0.0;
        // Plenty of headroom on an idle link.
        assert!((cc_soft_cap_multiplier(&c) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cap_at_target_falls_to_floor() {
        let mut c = one_conn();
        c.cc_target_bps = 1_000_000;
        c.bitrate.current_bitrate_bps = 1_000_000.0;
        // Saturated → floor multiplier (10%).
        let m = cc_soft_cap_multiplier(&c);
        assert!((m - CC_SOFT_CAP_FLOOR).abs() < f64::EPSILON, "got {m}");
    }

    #[test]
    fn cap_half_target_returns_half() {
        let mut c = one_conn();
        c.cc_target_bps = 1_000_000;
        c.bitrate.current_bitrate_bps = 500_000.0;
        let m = cc_soft_cap_multiplier(&c);
        assert!((m - 0.5).abs() < 0.01, "got {m}");
    }
}
