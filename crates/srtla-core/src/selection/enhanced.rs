//! Enhanced connection selection algorithm
//!
//! This module implements the enhanced SRTLA connection selection with:
//! - Quality-aware scoring based on NAK history
//! - RTT-aware bonuses for low-latency connections
//! - Score hysteresis to prevent flip-flopping (10%)
//!
//! The enhanced mode provides better connection quality awareness while
//! maintaining natural load distribution across all connections.

use tracing::debug;

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
/// demoted link keeps a trickle of data flowing, which is what lets it
/// earn the ACK and loss samples that clear the gate. Without this, an
/// excluded link earns zero throughput share, which the classifier
/// reads as `NoTraffic`/`LowShare` and keeps flagging weak — a
/// self-sustaining starvation lock that never re-tests the link.
///
/// Measured: with a healthy peer available, a 70%-loss link is gated to
/// 0.00 Mbps, silently heals, and re-adopts itself ~7s later purely on
/// this trickle. That is why an explicit starved-link probe was tried
/// and dropped — it moved delivery 0.76 pts (t=0.75, n=15), i.e. not at
/// all, because this penalty already does the job.
const GATED_LINK_PENALTY: f64 = 0.02;

/// How much better a challenger must measure before it takes the sole-carrier
/// role, as a ratio of the incumbent's smoothed RTT. Two links inside this
/// margin are, for the purpose of this decision, the same link.
const SOLE_CARRIER_MARGIN: f64 = 2.0;

/// Minimum time a link holds the sole-carrier role before any challenger can
/// take it. Matched to the sustain the weak classifier needs (2 ticks at ~1 Hz)
/// to flip a verdict, so the role can never change faster than the evidence
/// that would justify changing it.
const SOLE_CARRIER_MIN_HOLD_MS: u64 = 2000;

/// Should the sole-carrier role move from the incumbent to the challenger?
///
/// Deliberately keyed on smoothed RTT rather than the selection score. The
/// score is `window / (in_flight + 1)`, and the incumbent is by definition the
/// one carrying the stream, so its in-flight count is high and the idle
/// challenger always looks better — scoring the handover would guarantee the
/// oscillation this function exists to prevent. RTT is the one signal that
/// stays honest about a link that is not being loaded.
///
/// An unmeasured RTT on either side (`<= 0`) blocks the handover: with no
/// evidence, the incumbent keeps the role.
#[inline]
pub fn sole_carrier_handover(
    incumbent_rtt_ms: f64,
    challenger_rtt_ms: f64,
    held_for_ms: u64,
    hold_min_ms: u64,
    margin: f64,
) -> bool {
    if held_for_ms < hold_min_ms {
        return false;
    }
    if !incumbent_rtt_ms.is_finite() || !challenger_rtt_ms.is_finite() {
        return false;
    }
    if incumbent_rtt_ms <= 0.0 || challenger_rtt_ms <= 0.0 {
        return false;
    }
    challenger_rtt_ms * margin <= incumbent_rtt_ms
}

/// Elect the one link that keeps carrying the payload while *every*
/// schedulable link is quality-gated, and hold that choice steady.
///
/// When no link is healthy the admission gates all lift, and selection falls
/// back to ranking the whole pool on raw score, per packet. That splits the
/// stream's unique sequence numbers across links whose delays differ by an
/// order of magnitude, and lets the winner change from one packet to the next
/// — the receiver's reorder buffer then stalls on whichever copy went down the
/// slow path. librist measured the same failure on bonded cellular: with two
/// legs swinging between ~100 ms and ~1200 ms the payload path swapped roughly
/// once a second, and committing to *either* leg would have been better than
/// alternating between them.
///
/// So one link is elected and the rest are held out. The role is sticky: the
/// incumbent keeps it until it stops being a candidate (timed out, no longer
/// schedulable, stall-gated) or a sibling measures [`SOLE_CARRIER_MARGIN`]
/// times better on smoothed RTT after the incumbent has served
/// [`SOLE_CARRIER_MIN_HOLD_MS`].
///
/// Returns the elected index while the election is active, `None` when it is
/// not — either because a healthy link exists (the normal case, gates handled
/// by the usual penalty path) or because no link is a candidate at all, in
/// which case the caller's existing full-pool fallback stands and the packet is
/// never dropped for want of a carrier.
fn elect_sole_carrier(
    conns: &mut [SrtlaConnection],
    current_time_ms: u64,
    any_quality_ok: bool,
) -> Option<usize> {
    if any_quality_ok {
        for c in conns.iter_mut() {
            release_sole_carrier(c, current_time_ms);
        }
        return None;
    }

    let candidate = |c: &SrtlaConnection| {
        !c.is_timed_out(current_time_ms) && c.is_schedulable() && !c.stall_gated
    };

    let mut incumbent: Option<usize> = None;
    // Lowest smoothed RTT wins, and a link with no RTT yet cannot win on
    // absence of evidence. When *nothing* has been measured (every link still
    // in its first seconds) fall back to raw capacity, which is the same
    // choice selection would have made anyway — the election's job there is
    // only to stop the pool alternating for want of a number.
    let mut best_measured: Option<(usize, f64)> = None;
    let mut best_unmeasured: Option<(usize, i32)> = None;
    for (i, c) in conns.iter().enumerate() {
        if !candidate(c) {
            continue;
        }
        let rtt = c.get_smooth_rtt_ms();
        if rtt > 0.0 {
            if best_measured.is_none_or(|(_, best)| rtt < best) {
                best_measured = Some((i, rtt));
            }
        } else {
            let score = c.get_score();
            if best_unmeasured.is_none_or(|(_, best)| score > best) {
                best_unmeasured = Some((i, score));
            }
        }
        if c.sole_carrier {
            incumbent = Some(i);
        }
    }
    let best = best_measured
        .map(|(i, _)| i)
        .or(best_unmeasured.map(|(i, _)| i));
    let best_rtt = best_measured.map(|(_, rtt)| rtt).unwrap_or(f64::MAX);

    let keep = match (incumbent, best) {
        (Some(inc), Some(b)) if b != inc => {
            let inc_rtt = conns[inc].get_smooth_rtt_ms();
            let held_for = current_time_ms.saturating_sub(conns[inc].sole_carrier_since_ms);
            if sole_carrier_handover(
                inc_rtt,
                best_rtt,
                held_for,
                SOLE_CARRIER_MIN_HOLD_MS,
                SOLE_CARRIER_MARGIN,
            ) {
                Some(b)
            } else {
                Some(inc)
            }
        }
        (Some(inc), _) => Some(inc),
        (None, b) => b,
    };

    for (i, c) in conns.iter_mut().enumerate() {
        if Some(i) == keep {
            // The carrier is back in the rotation, so any exclusion it was
            // serving has ended — and it drained while out, so it re-enters
            // on a ramp like any other rejoining link.
            if c.sole_carrier_excluded {
                c.arm_rejoin_ramp(current_time_ms, SOLE_CARRIER_MIN_HOLD_MS);
            }
            if !c.sole_carrier {
                debug!(
                    "{}: elected sole carrier (every link is quality gated)",
                    c.label
                );
                c.sole_carrier = true;
                c.sole_carrier_since_ms = current_time_ms;
                c.sole_carrier_elections += 1;
            }
            c.sole_carrier_excluded = false;
        } else {
            c.sole_carrier = false;
            c.sole_carrier_since_ms = 0;
            // Only links that could have carried are "excluded"; one that is
            // unschedulable or gated for other reasons is not being held out
            // by this election and must not be given a ramp for it.
            c.sole_carrier_excluded = keep.is_some() && candidate(c);
        }
    }

    keep
}

/// Clear a link's sole-carrier state, arming the rejoin ramp if this ends an
/// exclusion. A link held out of the rotation drained its backlog while its
/// window kept recovering, so it comes back with the same unearned score
/// advantage a stall-gated link has, and needs the same treatment.
#[inline]
fn release_sole_carrier(c: &mut SrtlaConnection, current_time_ms: u64) {
    if c.sole_carrier_excluded {
        c.arm_rejoin_ramp(current_time_ms, SOLE_CARRIER_MIN_HOLD_MS);
        c.sole_carrier_excluded = false;
    }
    c.sole_carrier = false;
    c.sole_carrier_since_ms = 0;
}

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
#[inline(always)]
pub fn select_connection(
    conns: &mut [SrtlaConnection],
    last_idx: Option<usize>,
    current_time_ms: u64,
    enable_quality: bool,
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
        !c.is_timed_out(current_time_ms)
            && c.is_schedulable()
            && !c.weak
            && !c.loss_degraded
            && !c.stall_gated
            && !in_flight_cap_exceeded(c)
    });

    // Sole-carrier election. Keyed on the quality gates alone, deliberately
    // excluding the in-flight cap: the cap is a transient that clears as the
    // link drains, and pinning the whole stream to one link because every link
    // is momentarily over its BDP would make a saturated pool worse. Only
    // sustained delay/loss verdicts — the states librist muted on — justify
    // committing to a single carrier.
    let any_quality_ok = conns.iter().any(|c| {
        !c.is_timed_out(current_time_ms)
            && c.is_schedulable()
            && !c.weak
            && !c.loss_degraded
            && !c.stall_gated
    });
    let sole_carrier = elect_sole_carrier(conns, current_time_ms, any_quality_ok);

    // Decide who is held out of the payload rotation entirely, as opposed to
    // merely demoted. Two cases, both about latency rather than capacity:
    //
    //  - A *late* link (delay-weak, or loss-degraded) while a healthy link can
    //    carry. Every unique sequence number committed to a link running a
    //    second behind is a hole the receiver's reorder buffer has to wait on,
    //    so the demoted-but-still-carrying trickle is itself the glitch. Such
    //    a link is instead probed with duplicates by the shell, which costs
    //    the receiver nothing (it dedups by sequence) and still earns the ACK
    //    and RTT samples the recovery decision needs.
    //  - Anyone who lost the sole-carrier election.
    //
    // A link that is weak only for *share* reasons keeps its crushed-score
    // trickle: it is not late, it is under-used, and unique traffic is exactly
    // what it needs to earn back the throughput share that clears the verdict.
    for (i, c) in conns.iter_mut().enumerate() {
        let schedulable = !c.is_timed_out(current_time_ms) && c.is_schedulable() && !c.stall_gated;
        let late = c.loss_degraded || (c.weak && c.weak_reason.is_delay());
        c.quality_excluded = schedulable
            && ((any_unconstrained && late) || (sole_carrier.is_some() && Some(i) != sole_carrier));
    }

    // Score connections by base score; apply quality multiplier if enabled.
    //
    // Only the best link is tracked; nothing consumes the runner-up's rank.
    let mut best_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut current_score: Option<f64> = None;

    for (i, c) in conns.iter_mut().enumerate() {
        // A stall-gated link is a black hole with a healthier alternative
        // available (see `apply_stall_gate`); hard-skip it like a timed-out link
        // rather than crushing its score, since a trickle would only add latency.
        if c.is_timed_out(current_time_ms) || !c.is_schedulable() || c.stall_gated {
            continue;
        }
        // Held out on quality grounds: a late link while a healthy one can
        // carry, or a loser of the sole-carrier election. Both cases are
        // computed above and both guarantee some other link is still
        // rankable, so this can never empty the candidate pool.
        if c.quality_excluded {
            continue;
        }
        // Hard-skip only the in-flight cap: it bounds queueing delay and
        // is transient (self-clears as the link drains), so piling more
        // on is counterproductive.
        if any_unconstrained && in_flight_cap_exceeded(c) {
            continue;
        }
        // Whatever weak links are left here are weak for share reasons only
        // — the late ones were held out above. Crush the score but keep them
        // rankable: the resulting trickle of real traffic is what lets a
        // share-weak link earn back the share that clears the verdict, and
        // without it the classifier reads the starvation it caused as more
        // evidence of weakness.
        let quality_gated = any_unconstrained && c.weak;
        let gate_mult = if quality_gated {
            GATED_LINK_PENALTY
        } else {
            1.0
        };
        // The phase weight de-rates a warming link rather than excluding it. At
        // go-live every link is warming, so an exclusion here would empty the
        // candidate pool and drop the stream; an equal de-rating leaves the
        // relative ranking intact and traffic flows immediately.
        // The rejoin ramp de-rates a link that just came out of a stall gate.
        // Its score is inflated by the gating itself — no payload means the
        // in-flight count drained to zero while time-based window recovery
        // kept growing the window — so without the ramp it wins the first
        // packet after release outright and refills the queue it just drained.
        let base =
            c.get_score() as f64 * c.phase_weight() * c.rejoin_ramp_multiplier(current_time_ms);
        let cap_mult = cc_soft_cap_multiplier(c);
        let score = if !enable_quality {
            base * cap_mult * gate_mult
        } else {
            // Use cached quality multiplier (recalculates every 50ms)
            let quality_mult = c.get_cached_quality_multiplier(current_time_ms);
            let final_score = base * quality_mult * cap_mult * gate_mult;

            // Log quality issues and recoveries for debugging (cold path)
            log_quality_state(c, quality_mult, base, final_score, current_time_ms);

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

    best_idx
}

/// Log quality state for debugging (cold path, marked for optimizer hints)
#[cold]
#[inline(never)]
fn log_quality_state(
    c: &SrtlaConnection,
    quality_mult: f64,
    base: f64,
    final_score: f64,
    now_ms: u64,
) {
    if quality_mult < 0.8 {
        debug!(
            "{} quality degraded: {:.2} (NAKs: {}, last: {}ms ago, burst: {}) base: {} → final: {}",
            c.label,
            quality_mult,
            c.total_nak_count(),
            c.time_since_last_nak_ms(now_ms).unwrap_or(0),
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

    fn two_conns() -> smallvec::SmallVec<SrtlaConnection, 4> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(create_test_connections(2))
    }

    /// Mark a link quality-gated with a given smoothed RTT.
    fn failing(c: &mut SrtlaConnection, rtt_ms: f64) {
        c.weak = true;
        c.rtt.kalman_rtt.update(rtt_ms);
        // Kalman needs a couple of samples to sit on the value.
        for _ in 0..12 {
            c.rtt.kalman_rtt.update(rtt_ms);
        }
    }

    #[test]
    fn handover_needs_both_a_clear_margin_and_a_served_minimum() {
        // librist's field numbers: 126 vs 85 is noise between two failing
        // legs; 1162 vs 406 is a genuinely better path.
        assert!(!sole_carrier_handover(126.0, 85.0, 10_000, 2000, 2.0));
        assert!(sole_carrier_handover(1162.0, 406.0, 10_000, 2000, 2.0));
        // ...but not before the incumbent has served its minimum.
        assert!(!sole_carrier_handover(1162.0, 406.0, 1999, 2000, 2.0));
        // Exactly at the margin and exactly at the minimum both count.
        assert!(sole_carrier_handover(800.0, 400.0, 2000, 2000, 2.0));
    }

    #[test]
    fn handover_refuses_to_act_on_an_unmeasured_link() {
        // No RTT on either side is not evidence of a better path.
        assert!(!sole_carrier_handover(0.0, 400.0, 10_000, 2000, 2.0));
        assert!(!sole_carrier_handover(1200.0, 0.0, 10_000, 2000, 2.0));
        assert!(!sole_carrier_handover(f64::NAN, 400.0, 10_000, 2000, 2.0));
    }

    #[test]
    fn no_election_while_a_healthy_link_exists() {
        let mut conns = two_conns();
        failing(&mut conns[0], 900.0);
        let now = 100_000;
        assert_eq!(elect_sole_carrier(&mut conns, now, true), None);
        assert!(!conns[0].is_sole_carrier() && !conns[1].is_sole_carrier());
        assert!(!conns[0].is_sole_carrier_excluded());
    }

    #[test]
    fn every_link_failing_elects_one_and_holds_it() {
        let mut conns = two_conns();
        failing(&mut conns[0], 900.0);
        failing(&mut conns[1], 1000.0);
        let now = 100_000;

        // Lowest smoothed RTT wins the first election.
        assert_eq!(elect_sole_carrier(&mut conns, now, false), Some(0));
        assert!(conns[0].is_sole_carrier());
        assert!(conns[1].is_sole_carrier_excluded());
        assert_eq!(conns[0].sole_carrier_elections(), 1);

        // Link 1 pulls marginally ahead. Re-running the election every packet
        // on the instantaneous measurement is exactly what made librist's
        // payload path swap legs once a second, so the role must not move.
        failing(&mut conns[0], 1100.0);
        failing(&mut conns[1], 900.0);
        for tick in 0..10 {
            assert_eq!(
                elect_sole_carrier(&mut conns, now + tick * 1000, false),
                Some(0),
                "a marginally better sibling must not take the role"
            );
        }
        assert_eq!(conns[0].sole_carrier_elections(), 1, "no churn");
    }

    #[test]
    fn a_clearly_better_link_takes_the_role_once_the_hold_expires() {
        let mut conns = two_conns();
        failing(&mut conns[0], 1200.0);
        failing(&mut conns[1], 1300.0);
        let now = 100_000;
        assert_eq!(elect_sole_carrier(&mut conns, now, false), Some(0));

        // Link 1 is now several times better — but the incumbent has only
        // just taken the role.
        failing(&mut conns[1], 300.0);
        assert_eq!(
            elect_sole_carrier(&mut conns, now + SOLE_CARRIER_MIN_HOLD_MS - 1, false),
            Some(0)
        );
        // Past the minimum hold, the handover goes through.
        let t = now + SOLE_CARRIER_MIN_HOLD_MS;
        assert_eq!(elect_sole_carrier(&mut conns, t, false), Some(1));
        assert!(conns[1].is_sole_carrier() && !conns[0].is_sole_carrier());
        assert_eq!(conns[1].sole_carrier_elections(), 1);

        // The link that just came back in ramps its share up rather than
        // resuming at the score its idle time inflated.
        assert!(conns[1].rejoin_ramp_multiplier(t) < 1.0);
    }

    #[test]
    fn the_role_moves_off_a_link_that_stops_being_a_candidate() {
        let mut conns = two_conns();
        failing(&mut conns[0], 900.0);
        failing(&mut conns[1], 5000.0);
        let now = 100_000;
        assert_eq!(elect_sole_carrier(&mut conns, now, false), Some(0));

        // The incumbent stalls out. Stickiness must not outrank a link
        // being unusable, even though the sibling measures far worse.
        conns[0].stall_gated = true;
        assert_eq!(elect_sole_carrier(&mut conns, now + 100, false), Some(1));
    }

    #[test]
    fn ending_the_election_ramps_the_excluded_link_back_in() {
        let mut conns = two_conns();
        failing(&mut conns[0], 900.0);
        failing(&mut conns[1], 1000.0);
        let now = 100_000;
        elect_sole_carrier(&mut conns, now, false);
        assert!(conns[1].is_sole_carrier_excluded());

        // A link recovers, so the election ends and everyone competes again.
        let t = now + 5000;
        assert_eq!(elect_sole_carrier(&mut conns, t, true), None);
        assert!(!conns[1].is_sole_carrier_excluded());
        assert!(
            conns[1].rejoin_ramp_multiplier(t) < 1.0,
            "a link released from exclusion drained while out, so it must ramp rather than seize \
             the stream on its inflated score"
        );
    }

    #[test]
    fn a_late_link_is_held_out_of_the_rotation_entirely() {
        use crate::selection::classifier::WeakReason;

        let mut conns = two_conns();
        let now = crate::utils::now_ms();
        conns[0].weak = true;
        conns[0].weak_reason = WeakReason::HighRtt;
        // Link 0 would win on raw score: the healthy link is the busy one.
        conns[1].in_flight_packets = 40;

        assert_eq!(select_connection(&mut conns, None, now, true), Some(1));
        assert!(
            conns[0].is_quality_excluded(),
            "a late link must be held out, not trickled: every unique sequence number on it is a \
             hole the receiver waits for"
        );
    }

    #[test]
    fn an_under_used_link_keeps_its_trickle_of_real_traffic() {
        use crate::selection::classifier::WeakReason;

        let mut conns = two_conns();
        let now = crate::utils::now_ms();
        conns[0].weak = true;
        conns[0].weak_reason = WeakReason::LowShare;
        conns[1].in_flight_packets = 40;

        assert_eq!(select_connection(&mut conns, None, now, true), Some(1));
        assert!(
            !conns[0].is_quality_excluded(),
            "share weakness is not lateness — the link needs real traffic to earn back the share \
             that clears the verdict"
        );
    }

    #[test]
    fn a_loss_degraded_link_is_held_out_whatever_the_weak_reason() {
        let mut conns = two_conns();
        let now = crate::utils::now_ms();
        conns[0].loss_degraded = true;
        conns[1].in_flight_packets = 40;

        assert_eq!(select_connection(&mut conns, None, now, true), Some(1));
        assert!(conns[0].is_quality_excluded());
    }

    #[test]
    fn nothing_is_held_out_when_no_healthy_link_can_carry() {
        use crate::selection::classifier::WeakReason;

        // Both links late: the exclusion must not fire on every link at once.
        // One is elected to carry and the other is held out, but a link is
        // always returned.
        let mut conns = two_conns();
        let now = crate::utils::now_ms();
        for c in conns.iter_mut() {
            c.weak = true;
            c.weak_reason = WeakReason::HighRtt;
        }
        let picked = select_connection(&mut conns, None, now, true);
        assert!(picked.is_some(), "selection must never drop the packet");
        let picked = picked.unwrap();
        assert!(!conns[picked].is_quality_excluded());
        assert!(conns[picked].is_sole_carrier());
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
