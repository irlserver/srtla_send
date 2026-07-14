//! Starved-link probing for enhanced mode.
//!
//! Scoring alone cannot rehabilitate a link it has demoted. A link that is
//! `weak`, `loss_degraded`, or parked over its in-flight cap scores far below
//! its healthy peers, so it wins no packets; winning no packets means it earns
//! no ACKs and no loss samples; and without fresh samples the signals that
//! demoted it never clear. The link stays demoted because it is demoted.
//! `GATED_LINK_PENALTY` softens this by keeping a gated link *rankable* rather
//! than excluded, but a crushed score still loses every comparison against a
//! healthy link, so in practice the trickle never arrives.
//!
//! Keepalives do flow on every link regardless of selection, and their RTT
//! samples prove liveness — that is what lets `stall_deselect` un-gate itself.
//! What a keepalive cannot prove is *capacity*: whether the link can carry data
//! without losing it. Only data can answer that.
//!
//! So this module answers exactly one question: which starved link, if any,
//! should receive a single data packet right now, in order to earn the ACK or
//! NAK that will let the scoring layer re-evaluate it?
//!
//! The probe is deliberately small and rare:
//!
//! - **One packet at a time.** Selection is re-run per packet, so the packet
//!   after a probe already sees the probe queued (`get_score()` counts queued
//!   packets as in-flight) and routes normally. A probe is a single diversion,
//!   not a mode the scheduler enters.
//! - **Rate-limited per link** ([`PROBE_INTERVAL_MS`]), so a persistently bad
//!   link cannot bleed throughput. At 200ms and a typical 1600 pkt/s, one
//!   starved link costs well under 0.1% of packets.
//! - **Only when a healthy link exists.** Probing is a luxury paid for out of
//!   spare capacity; if nothing healthy is schedulable, every packet is needed
//!   for the stream and normal scoring already routes to the least-bad link.
//! - **Only starved links.** A link that is already winning traffic is
//!   generating its own ACKs and needs no probe.
//!
//! This replaces an earlier design that probed the *second-best* link (usually
//! healthy, and already carrying traffic) on a 30-second periodic timer, with
//! no budget — it diverted roughly half of all traffic for as long as its
//! trigger held, and leaned on the scheduler's switch cooldown as an accidental
//! rate limiter.

use tracing::debug;

use super::enhanced::in_flight_cap_exceeded;
use crate::connection::SrtlaConnection;

/// Minimum time between probe packets sent to the same starved link.
///
/// Long enough that a probe costs a negligible share of the stream, short
/// enough that a link which recovers is noticed within a few hundred
/// milliseconds rather than after a stall the viewer would see.
pub const PROBE_INTERVAL_MS: u64 = 200;

/// Is this link starved — demoted by a quality gate such that it wins no
/// traffic, and therefore cannot earn the samples that would clear the gate?
///
/// `stall_gated` is deliberately excluded: a stalled link is a suspected black
/// hole with a healthy alternative already carrying the stream, and it has its
/// own liveness-proven recovery path (a keepalive RTT sample clears it). Aiming
/// data at it would only add latency to a packet we expect to be swallowed.
#[inline]
fn is_starved(c: &SrtlaConnection) -> bool {
    c.weak || c.loss_degraded || in_flight_cap_exceeded(c)
}

/// Choose a starved link to receive one probe packet, or `None`.
///
/// `best_idx` is the link normal scoring would have picked; the caller must
/// only call this when that link is healthy (i.e. some un-gated link is
/// schedulable), so the probe is never funded out of a stream that has nowhere
/// good to go.
///
/// Among eligible links the least-recently-probed one wins, so several starved
/// links take turns instead of one hogging the budget.
pub fn pick_probe_target(
    conns: &[SrtlaConnection],
    best_idx: usize,
    current_time_ms: u64,
) -> Option<usize> {
    conns
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            *i != best_idx
                && !c.is_timed_out()
                && c.connected
                && c.is_schedulable()
                && !c.stall_gated
                && is_starved(c)
                // `last_probe_ms == 0` (never probed) is always due: the
                // subtraction against a wall-clock timestamp dwarfs the interval.
                && current_time_ms.saturating_sub(c.last_probe_ms) >= PROBE_INTERVAL_MS
        })
        .min_by_key(|(_, c)| c.last_probe_ms)
        .map(|(i, c)| {
            debug!(
                "{}: probing starved link (weak={}, loss_degraded={}, in_flight={})",
                c.label, c.weak, c.loss_degraded, c.in_flight_packets
            );
            i
        })
}

// Tests are in src/tests/sender_tests.rs
