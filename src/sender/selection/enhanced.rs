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

use super::exploration::pick_probe_target;
use super::link_cc::ASSUMED_SRT_PAYLOAD_BYTES;
use crate::connection::SrtlaConnection;

/// Headroom multiplier on the bandwidth-delay product for the per-link
/// in-flight cap. The cap is `BDP * 1.5`: a link should be allowed
/// roughly one BDP of packets in flight to keep its pipe full, plus 50%
/// slack for bursts before we steer elsewhere. A fixed packet budget
/// (the old `pps / 40` ≈ 25 ms) starves a high-RTT link that needs a
/// deeper pipe and over-fills a low-RTT one; scaling by the link's own
/// `rtt_min` makes the cap correct across fibre, cellular, and satellite.
const IN_FLIGHT_CAP_BDP_MULT: f64 = 1.5;

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

/// Score multiplier applied to a quality-gated link (`weak` or
/// `loss_degraded`) when at least one un-gated link is schedulable.
/// The link stays in the ranking at a crushed score instead of being
/// dropped outright. In steady state a healthy link's full score still
/// wins decisively, so routing is unchanged; the point is that the
/// demoted link remains eligible to be second-best (so exploration can
/// probe it) and keeps a trickle of data flowing. Without this, an
/// excluded link earns zero throughput share, which the classifier
/// reads as `NoTraffic`/`LowShare` and keeps flagging weak — a
/// self-sustaining starvation lock that never re-tests the link.
const GATED_LINK_PENALTY: f64 = 0.02;

/// In-flight cap (packets) as a bandwidth-delay product: the link's
/// predicted sustainable rate times its own minimum RTT, with
/// `IN_FLIGHT_CAP_BDP_MULT` headroom.
///
/// Returns `None` when there's no rate signal (`cc_target_bps == 0`,
/// i.e. the CC controller hasn't published a target yet) — selection
/// treats the cap as inactive in that case. `rtt_min_ms` is the link's
/// windowed minimum RTT; a non-positive value falls back to 1 ms so the
/// cap stays well-defined before the baseline is established.
///
/// `cap = max(1, cc_target_bps * rtt_min_s / 8 * 1.5 / packet_bytes)`.
/// Floored at 1 so even a very slow link can keep one packet in flight;
/// the cap bounds queueing delay, it does not gate the link entirely.
#[inline]
pub fn in_flight_cap_packets(cc_target_bps: u64, rtt_min_ms: f64) -> Option<i32> {
    if cc_target_bps == 0 {
        return None;
    }
    let rtt_ms = if rtt_min_ms.is_finite() && rtt_min_ms > 0.0 {
        rtt_min_ms
    } else {
        1.0
    };
    let bdp_bytes = (cc_target_bps as f64) * (rtt_ms / 1000.0) / 8.0 * IN_FLIGHT_CAP_BDP_MULT;
    let cap = (bdp_bytes / ASSUMED_SRT_PAYLOAD_BYTES as f64)
        .floor()
        .max(1.0);
    Some(cap.min(i32::MAX as f64) as i32)
}

/// Whether the link is currently exceeding its in-flight cap. Used by
/// the admission gate alongside `weak` and `loss_degraded`. A capped
/// link is excluded from candidate ranking when at least one
/// non-capped, non-weak, non-loss-degraded link is schedulable.
#[inline(always)]
pub fn in_flight_cap_exceeded(c: &SrtlaConnection) -> bool {
    in_flight_cap_packets(c.cc_target_bps, c.get_rtt_min_ms())
        .map(|cap| c.in_flight_packets > cap)
        .unwrap_or(false)
}

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
///
/// IMPORTANT: This function is called for EACH incoming SRT packet, and it is
/// meant to be: `get_score()` counts a link's queued-but-unflushed packets as
/// in-flight, so routing a packet immediately lowers that link's own score.
/// Selection is therefore a closed feedback loop that bounds queue depth per
/// link — re-deciding on every packet is the mechanism, not thrashing.
///
/// Score hysteresis ([`SWITCH_THRESHOLD`]) still resists flip-flopping between
/// links whose scores are within noise of each other.
///
/// # Arguments
/// * `conns` - Mutable slice of available connections (for quality cache updates)
/// * `last_idx` - Previously selected connection index (for hysteresis)
/// * `current_time_ms` - Current timestamp in milliseconds
/// * `enable_quality` - Whether to apply quality scoring
/// * `enable_explore` - Whether to allow probing starved links
/// * `is_data` - Whether this packet carries an SRT sequence number (only data
///   packets can earn the ACK/NAK a probe exists to collect)
#[inline(always)]
pub fn select_connection(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    current_time_ms: u64,
    enable_quality: bool,
    enable_explore: bool,
    is_data: bool,
) -> Option<usize> {
    // First pass: discover whether at least one un-gated connection
    // can carry the packet. The classifier marks links weak when their
    // RTT busts the chosen delay tier (sustained, not a single blip),
    // when a queue is building, or when they fall below the entering
    // throughput-share threshold. The loss gate uses `loss_degraded` —
    // the 4s-sustained, hysteretic loss latch — rather than the raw
    // per-window `cc_backing_off`, so a single noisy loss window doesn't
    // demote routing weight (cc_backing_off still drives the CC
    // controller's own bitrate backoff; it just no longer gates routing).
    // The in-flight cap gates a link whose in-flight packets already
    // exceed its bandwidth-delay product (plus headroom), so the
    // scheduler doesn't pile more on while the link drains. If any
    // un-gated link is schedulable, the gated ones are excluded from
    // ranking. Otherwise we fall back to the full pool — better to send
    // on a gated link than to drop the packet.
    let any_unconstrained = conns.iter().any(|c| {
        !c.is_timed_out()
            && c.is_schedulable()
            && !c.weak
            && !c.loss_degraded
            && !c.stall_gated
            && !in_flight_cap_exceeded(c)
    });

    // Score connections by base score; apply quality multiplier if enabled.
    //
    // Only the best link is tracked. The runner-up used to matter because
    // exploration probed *second-best*; probing now targets starved links
    // instead (a healthy runner-up is already earning its own ACKs and needs no
    // probe), so its rank is no longer interesting.
    let mut best_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut current_score: Option<f64> = None;

    for (i, c) in conns.iter_mut().enumerate() {
        // A stall-gated link is a black hole with a healthier alternative
        // available (see `apply_stall_gate`); hard-skip it like a timed-out link
        // rather than crushing its score, since a trickle would only add latency.
        if c.is_timed_out() || !c.is_schedulable() || c.stall_gated {
            continue;
        }
        // Hard-skip only the in-flight cap: it bounds queueing delay and
        // is transient (self-clears as the link drains), so piling more
        // on is counterproductive. Quality gates (`weak`,
        // `loss_degraded`) instead crush the score but keep the link
        // rankable, so it is never starved into a permanent weak lock.
        if any_unconstrained && in_flight_cap_exceeded(c) {
            continue;
        }
        let quality_gated = any_unconstrained && (c.weak || c.loss_degraded);
        let gate_mult = if quality_gated {
            GATED_LINK_PENALTY
        } else {
            1.0
        };
        // The phase weight de-rates a warming link rather than excluding it. At
        // go-live every link is warming, so an exclusion here would empty the
        // candidate pool and drop the stream; an equal de-rating leaves the
        // relative ranking intact and traffic flows immediately.
        let base = c.get_score() as f64 * c.phase_weight();
        let cap_mult = cc_soft_cap_multiplier(c);
        let score = if !enable_quality {
            base * cap_mult * gate_mult
        } else {
            // Use cached quality multiplier (recalculates every 50ms)
            let quality_mult = c.get_cached_quality_multiplier(current_time_ms);
            let final_score = base * quality_mult * cap_mult * gate_mult;

            // Log quality issues and recoveries for debugging (cold path)
            log_quality_state(c, quality_mult, base, final_score);

            final_score
        };

        // Track current connection's score for hysteresis
        if Some(i) == last_idx {
            current_score = Some(score);
        }

        if score > best_score {
            best_score = score;
            best_idx = Some(i);
        }
    }

    // No time-based switch cooldown.
    //
    // There used to be one (`MIN_SWITCH_INTERVAL_MS`, 15ms), which pinned the
    // selector to the previously chosen link regardless of score. Its purpose was
    // not scheduling: `forward_via_connection` flushed the previous link's batch
    // on every switch, so per-packet switching emitted a one-packet batch each
    // time, and the cooldown suppressed that. Batches are per-connection and now
    // leave in a single `sendmmsg` on their own threshold/timer, so the flush is
    // gone and switching is free.
    //
    // Keeping the cooldown would be actively harmful: `get_score()` counts queued
    // packets as in-flight so that routing a packet immediately de-prioritises its
    // link. Holding the decision fixed for 15ms (~24 packets at the rate this
    // sender actually pushes) opens that feedback loop, and in-flight runs away on
    // whichever link the timer happened to park on.
    //
    // Score hysteresis below still damps flip-flopping between links whose scores
    // differ only by noise — that is a score-space guard, and costs no syscalls.
    if let Some(last) = last_idx {
        // If proposing a different connection
        if best_idx != Some(last) {
            // Apply score-based hysteresis: only move off the current link when
            // the new best is meaningfully better.
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

    // Probe a starved link with a single data packet, if one is due.
    //
    // Gated by `any_unconstrained`: the probe is paid for out of a healthy
    // link's capacity, so if nothing healthy is schedulable every packet is
    // needed for the stream and we never divert. Gated on `is_data` because a
    // probe exists to earn an ACK or NAK, and only data packets carry a
    // sequence number the receiver will acknowledge — diverting an SRT control
    // packet to a degraded link would delay SRT's own control loop and teach us
    // nothing.
    if enable_explore
        && is_data
        && any_unconstrained
        && let Some(best) = best_idx
        && let Some(probe) = pick_probe_target(conns, best, current_time_ms)
    {
        conns[probe].last_probe_ms = current_time_ms;
        return Some(probe);
    }

    best_idx
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
    fn in_flight_cap_no_signal() {
        // cc_target_bps == 0 → cap inactive regardless of in_flight.
        assert_eq!(in_flight_cap_packets(0, 50.0), None);
        let mut c = one_conn();
        c.cc_target_bps = 0;
        c.in_flight_packets = 10_000;
        assert!(!in_flight_cap_exceeded(&c));
    }

    #[test]
    fn in_flight_cap_floors_at_one() {
        // 100 kbps over a 20 ms RTT: BDP = 1e5 * 0.02 / 8 = 250 bytes,
        // x1.5 = 375 bytes < one packet, so the cap floors at 1.
        let cap = in_flight_cap_packets(100_000, 20.0).unwrap();
        assert_eq!(cap, 1);
    }

    #[test]
    fn in_flight_cap_scales_with_bdp() {
        // 10 Mbps over 50 ms: BDP = 1e7 * 0.05 / 8 = 62_500 bytes, x1.5
        // = 93_750, / 1316 ≈ 71 packets.
        let cap = in_flight_cap_packets(10_000_000, 50.0).unwrap();
        assert!((68..=74).contains(&cap), "got {cap}");
        // Same rate at 4x the RTT gives ~4x the cap (path-relative).
        let cap_high_rtt = in_flight_cap_packets(10_000_000, 200.0).unwrap();
        assert!(cap_high_rtt > cap * 3, "got {cap_high_rtt} vs {cap}");
    }

    #[test]
    fn in_flight_cap_engaged_when_exceeded() {
        let mut c = one_conn();
        c.cc_target_bps = 10_000_000;
        let cap = in_flight_cap_packets(c.cc_target_bps, c.get_rtt_min_ms()).unwrap();
        c.in_flight_packets = cap;
        assert!(
            !in_flight_cap_exceeded(&c),
            "at cap is allowed, only above triggers"
        );
        c.in_flight_packets = cap + 1;
        assert!(in_flight_cap_exceeded(&c));
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
