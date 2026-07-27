mod ack_nak;
pub mod batch_send;
mod bitrate;
mod congestion;
mod incoming;
mod reconnection;
mod rtt;

use std::net::IpAddr;

pub use batch_send::{BATCH_SEND_SIZE, BatchSender, DrainedPacket};
pub use bitrate::BitrateTracker;
pub use congestion::CongestionControl;
pub use incoming::SrtlaIncoming;
pub use reconnection::ReconnectionState;
pub use rtt::RttTracker;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use srtla_protocol::*;
use tracing::debug;

use crate::selection::classifier::WeakReason;

pub const STARTUP_GRACE_MS: u64 = 5_000;

/// Number of RTT probes required before a link transitions from Warming to Live.
const WARMING_RTT_PROBES: u32 = 2;
/// Maximum time in ms a link may stay in Warming before auto-promoting to Live.
/// Prevents links from getting stuck if RTT probes are slow or lost.
const WARMING_TIMEOUT_MS: u64 = 5_000;

/// Link lifecycle phase.
///
/// A phase *weights* a link's score; it does not remove the link. `Registering`
/// is the sole exception, and it is not a quality judgement: the receiver has
/// not returned REG3, so data sent on that link would be discarded by the
/// protocol itself.
///
/// This mirrors the model the phase machine was ported from, where the
/// scheduler multiplies a link's score by a per-phase weight and only a dead
/// link is filtered out. It also matches the rule the rest of this scheduler
/// follows: `weak` and `loss_degraded` crush a score but keep the link rankable
/// (`GATED_LINK_PENALTY`), and `stall_gated` only ever fires when a healthier
/// link exists. Nothing is hard-removed for quality.
///
/// `Warming` used to be a hard exclusion, which broke that rule in the one place
/// it mattered most: at go-live *every* link is warming, so the candidate pool
/// was empty and the sender dropped the stream until the first link was promoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkPhase {
    /// Waiting for REG3 handshake to complete.
    #[default]
    Registering,
    /// REG3 received, accumulating RTT probes. Usable, but de-rated: the link's
    /// RTT baseline is only a keepalive or two old, so the window/in-flight
    /// signal that drives selection is still coarse.
    Warming { rtt_probes: u32, entered_ms: u64 },
    /// Fully operational — scheduler may use this link.
    Live,
    /// Quality has degraded (high NAK rate / low quality multiplier, or
    /// a sustained loss EWMA). Scheduler still uses this link but its
    /// score is reduced. There is no removed/cooldown phase: a link is
    /// never excluded for quality, only de-prioritised, so it keeps the
    /// ACK traffic that proves its recovery. Truly dead links are pruned
    /// by `is_timed_out`/`CONN_TIMEOUT`.
    Degraded,
}

impl LinkPhase {
    /// Whether the scheduler is allowed to send data on this link.
    ///
    /// Only `Registering` is excluded, and only because the protocol forbids it
    /// (no REG3 yet). Every other phase is schedulable and expresses itself
    /// through [`LinkPhase::weight`] instead.
    pub fn is_schedulable(&self) -> bool {
        !matches!(self, LinkPhase::Registering)
    }

    /// Scheduling weight contributed by this phase, folded into the link's score.
    ///
    /// `Degraded` stays at 1.0 deliberately. Degradation is already priced in
    /// twice — by the quality multiplier that demoted the link in the first
    /// place, and by the `weak`/`loss_degraded` admission gates — so charging it
    /// a third time here would just double-count the same signal.
    pub fn weight(&self) -> f64 {
        match self {
            LinkPhase::Registering => 0.0,
            LinkPhase::Warming { .. } => 0.8,
            LinkPhase::Live | LinkPhase::Degraded => 1.0,
        }
    }
}

impl std::fmt::Display for LinkPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkPhase::Registering => write!(f, "registering"),
            LinkPhase::Warming { rtt_probes, .. } => write!(f, "warming({rtt_probes})"),
            LinkPhase::Live => write!(f, "live"),
            LinkPhase::Degraded => write!(f, "degraded"),
        }
    }
}

/// Interval in milliseconds between quality multiplier recalculations.
/// Caching reduces expensive exp() calls from every packet to ~20 times per second.
pub const QUALITY_CACHE_INTERVAL_MS: u64 = 50;

/// Cached quality multiplier to avoid expensive recalculations on every packet.
#[derive(Clone, Copy, Debug)]
pub struct CachedQuality {
    /// The cached quality multiplier value
    pub multiplier: f64,
    /// Timestamp when the multiplier was last calculated
    pub last_calculated_ms: u64,
}

impl Default for CachedQuality {
    fn default() -> Self {
        Self {
            multiplier: 1.0,
            last_calculated_ms: 0,
        }
    }
}

/// Share of its natural score a link should compete with while ramping back
/// in after a stall gate, rising linearly from
/// [`crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR`] to `1.0` over `ramp_ms`.
///
/// A gated link is the best-looking link in the pool, and that is an artefact.
/// It carries nothing but 1-in-N duplicate probes, so its in-flight count
/// drains to zero, while time-based window recovery keeps growing its window
/// because no traffic means no NAKs — and the score is `window / (in_flight +
/// 1)`. The instant the latch releases it therefore outranks every working
/// link by a wide margin, clears the switch hysteresis without effort, and
/// takes the whole stream. On a marginal link that refills the queue it just
/// drained, so it re-stalls and re-gates, which is a stable oscillation rather
/// than a recovery: librist measured the same loop on bonded cellular at a
/// ~15 s period, for as long as the link stayed marginal.
///
/// Ramping makes the link earn its share back under a growing load, so it
/// reveals congestion on the way up and settles at what it can actually carry.
/// `ramp_start == 0` (never gated) or an elapsed ramp both mean full score.
#[inline]
pub fn rejoin_ramp_multiplier(ramp_start_ms: u64, ramp_ms: u64, now_ms: u64) -> f64 {
    use crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR;
    if ramp_start_ms == 0 || ramp_ms == 0 {
        return 1.0;
    }
    let elapsed = now_ms.saturating_sub(ramp_start_ms);
    if elapsed >= ramp_ms {
        return 1.0;
    }
    let progress = elapsed as f64 / ramp_ms as f64;
    (STALL_REJOIN_RAMP_FLOOR + (1.0 - STALL_REJOIN_RAMP_FLOOR) * progress)
        .clamp(STALL_REJOIN_RAMP_FLOOR, 1.0)
}

/// Rejoin-dwell multiplier to serve after the stall latch engages again, given
/// the one it was serving, how long the last rejoin lasted, and how long it had
/// to last to count as recovered.
///
/// The ramp above prices a rejoining link's share correctly but cannot stop it
/// rejoining in the first place, and that is the half of the oscillation it
/// does not reach: the dwell only asks for sustained delivery proof, which a
/// drained link supplies trivially. A link carrying nothing but probes keeps a
/// fresh `last_ack_or_rtt_sample_ms` from those probes alone, so the rejoin
/// condition is met on schedule no matter what the path can carry under load.
/// A link that genuinely cannot hold its share therefore rejoins, refloods,
/// re-stalls and re-gates on a fixed period, forever.
///
/// Waiting longer is the only lever left, so the dwell doubles every time a
/// rejoin fails to outlast `probation_ms`, capped at
/// [`crate::config_snapshot::STALL_REJOIN_BACKOFF_MAX`]. A rejoin that does
/// hold resets it immediately, so a link recovering from a transient spike is
/// never penalised. `released_at_ms == 0` (never rejoined) is the first
/// engagement and is always free. `0` and `1` both mean 1x.
#[inline]
pub fn stall_rejoin_backoff_next(
    backoff: u32,
    released_at_ms: u64,
    held_for_ms: u64,
    probation_ms: u64,
) -> u32 {
    if released_at_ms == 0 || held_for_ms >= probation_ms {
        return 1;
    }
    let doubled = if backoff <= 1 {
        2
    } else {
        backoff.saturating_mul(2)
    };
    doubled.min(crate::config_snapshot::STALL_REJOIN_BACKOFF_MAX)
}

pub struct SrtlaConnection {
    pub conn_id: u64,
    #[allow(dead_code)]
    // Shell production API: the sender binds egress sockets to this IP and
    // reports it in stats, so it is unconditionally public across the crate seam.
    pub local_ip: IpAddr,
    pub label: String,
    pub connected: bool,
    pub window: i32,
    pub in_flight_packets: i32,
    /// Packet log: maps sequence number -> send timestamp (ms).
    /// Uses FxHashMap for O(1) insert/remove instead of O(256) linear scan.
    #[cfg(feature = "test-internals")]
    pub packet_log: FxHashMap<i32, u64>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_log: FxHashMap<i32, u64>,
    /// Send timestamps of this link's outstanding *duplicate probes*, kept
    /// apart from [`Self::packet_log`] on purpose.
    ///
    /// A cumulative SRT ACK prunes everything at or below it from the packet
    /// log of every link, because the receiver genuinely has that data — but a
    /// probe's twin travelled the healthy link, so the sweep lands roughly one
    /// healthy round trip after the probe was sent, while the probing link's
    /// own SRTLA ACK is still in flight behind its much longer RTT. Sharing one
    /// log therefore deleted the probe's entry before its ACK could arrive, and
    /// the ACK then matched nothing: no delivery proof, no RTT sample. The
    /// slower the link, the more reliably it lost the race — so the links whose
    /// recovery these probes exist to measure were the ones least likely to be
    /// measured, and what did get through was biased toward the fastest probes.
    ///
    /// Entries here are immune to the cumulative sweep and expire by age.
    /// Probes are also deliberately absent from the packet log's in-flight
    /// accounting and from NAK attribution: a probe is not payload this link
    /// owes anyone.
    #[cfg(feature = "test-internals")]
    pub probe_log: FxHashMap<i32, u64>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) probe_log: FxHashMap<i32, u64>,
    /// Highest sequence number that has been cumulatively ACKed.
    /// Used to optimize cumulative ACK processing by skipping already-ACKed sequences.
    #[cfg(feature = "test-internals")]
    pub highest_acked_seq: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) highest_acked_seq: i32,
    pub last_received: Option<u64>,
    pub last_sent: Option<u64>,
    /// Timestamp of the last keepalive sent (for periodic telemetry)
    // Test-only exposure: production shell never touches this, but cross-crate
    // liveness tests stamp it directly.
    #[cfg(any(test, feature = "test-internals"))]
    pub last_keepalive_sent: Option<u64>,
    #[cfg(not(any(test, feature = "test-internals")))]
    pub(crate) last_keepalive_sent: Option<u64>,
    /// `now_ms()` of this link's last delivery proof: an EARNED ACK (this link
    /// owned an acked seq) or a keepalive-RTT response. Stamped ONLY at those
    /// two sites — NEVER on generic inbound bytes (unlike `last_received`), so a
    /// link that merely echoes traffic while its ACK/RTT path is dead still goes
    /// stale. `0` = no proof yet. Read only by the `stall_deselect` selection
    /// guard; never a liveness/timeout signal.
    pub last_ack_or_rtt_sample_ms: u64,
    /// Transient per-select flag: set by `select_connection_idx` when
    /// `stall_deselect` is on and this link's stall latch is engaged while a
    /// healthier link exists. Recomputed every select call and read only by the
    /// mode selectors in that same call; it is a selection penalty ONLY and
    /// never affects `is_timed_out`/re-registration.
    // Test-only exposure: production shell never reads this directly, but
    // cross-crate stall-detection tests assert on it.
    #[cfg(any(test, feature = "test-internals"))]
    pub stall_gated: bool,
    #[cfg(not(any(test, feature = "test-internals")))]
    pub(crate) stall_gated: bool,
    /// `now_ms()` when the stall latch engaged; `0` = not latched. The latch
    /// outlives the raw [`Self::is_stalled`] signal on purpose: once a link
    /// stalls, cumulative SRT ACKs (delivered via the healthy links) drain its
    /// backlog below the in-flight threshold, which clears `is_stalled` without
    /// the link having proven anything — un-gating on that alone re-feeds the
    /// black hole and flaps. Rejoin instead requires sustained delivery proof
    /// (see [`Self::update_stall_latch`]).
    pub(crate) stall_latched_since_ms: u64,
    /// Start of the current uninterrupted run of fresh delivery proof on a
    /// latched link; `0` = no run in progress. The latch clears once this run
    /// spans the rejoin dwell.
    pub(crate) stall_recovery_since_ms: u64,
    /// Cumulative count of stall-latch engagements. Monotonic over the
    /// connection's life (survives soft resets); exported in stats so field
    /// runs can tell a link that gated once from one that flaps.
    pub(crate) stall_gate_events: u64,
    /// Rolling counter driving the 1-in-N duplicate-probe cadence while gated
    /// (see [`crate::config_snapshot::STALL_PROBE_ONE_IN_N`]).
    pub(crate) stall_probe_counter: u32,
    /// `now_ms()` when the stall latch last released; `0` = never released.
    /// Compared against the probation window on the next engagement to decide
    /// whether that rejoin held (see [`stall_rejoin_backoff_next`]).
    pub(crate) stall_released_at_ms: u64,
    /// Multiplier on the rejoin dwell, doubled each time a rejoin fails to hold
    /// its probation and reset to 1 as soon as one does. `0` and `1` both mean
    /// 1x — see [`stall_rejoin_backoff_next`].
    pub(crate) stall_rejoin_backoff: u32,
    /// `now_ms()` when the stall latch last released; `0` = no ramp running.
    /// Enhanced selection scales this link's score up from
    /// [`crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR`] to full over
    /// [`Self::stall_rejoin_ramp_ms`] from this instant, so a link that just
    /// rejoined earns its share back under a growing load instead of seizing
    /// the stream on the first packet (see [`rejoin_ramp_multiplier`]).
    pub(crate) stall_rejoin_ramp_start_ms: u64,
    /// Length of the ramp above, snapshotted from the rejoin dwell when the
    /// latch released so the read path needs no config.
    pub(crate) stall_rejoin_ramp_ms: u64,
    /// Whether the running ramp was armed by the stall gate. Disabling that
    /// guard at runtime drops its ramps to restore baseline scoring, and must
    /// leave ramps armed by the other gates alone.
    pub(crate) stall_rejoin_ramp_from_stall_gate: bool,
    /// Elected to keep carrying the payload while every schedulable link is
    /// quality-gated. Sticky: held until this link is no longer the worst
    /// option, so the payload path cannot ping-pong between two failing links
    /// (see `selection::enhanced::elect_sole_carrier`).
    pub(crate) sole_carrier: bool,
    /// `now_ms()` when this link took the sole-carrier role; `0` = never.
    pub(crate) sole_carrier_since_ms: u64,
    /// True while the sole-carrier election is running and this link lost it.
    /// Kept across passes so the falling edge can arm the rejoin ramp.
    pub(crate) sole_carrier_excluded: bool,
    /// Cumulative count of sole-carrier handovers to this link: times it took
    /// the role *from another link*. Taking a vacant role is not counted, so
    /// this is a pure churn signal — a field log that shows it climbing every
    /// second or two is the ping-pong the stickiness exists to prevent, and
    /// says the margin or the minimum hold is wrong for these links. Monotonic
    /// over the connection's life.
    pub(crate) sole_carrier_elections: u64,
    /// Fast transient tier below the stall latch: true while this loaded link
    /// has received *nothing at all* for the silence-pull window (see
    /// [`Self::is_briefly_silent`]). Unlike the latch it keys on
    /// `last_received` — any inbound byte, not delivery proof — because it is
    /// recomputed on every selection pass and clears the instant the link
    /// speaks again, so the "an echoing link never goes stale" concern that
    /// forbids `last_received` for the latch does not apply. Set by
    /// `apply_stall_gate`; read only for rising-edge counting and selection.
    pub(crate) silence_pulled: bool,
    /// Cumulative silence-pull engagements (rising edges). Expected to tick
    /// on routine cellular HARQ stalls; a high rate is telemetry, not alarm.
    pub(crate) silence_pulls: u64,
    /// Per-link liveness timeout in ms. Refreshed from the config snapshot on
    /// every selection pass so `is_timed_out` (called from paths that do not
    /// carry a config) always sees the current runtime value.
    pub(crate) conn_timeout_ms: u64,
    // Sub-structs for organized state management
    pub rtt: RttTracker,
    #[cfg(feature = "test-internals")]
    pub congestion: CongestionControl,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) congestion: CongestionControl,
    #[cfg(feature = "test-internals")]
    pub bitrate: BitrateTracker,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) bitrate: BitrateTracker,
    pub reconnection: ReconnectionState,
    /// Cached quality multiplier for performance optimization.
    /// Recalculated every 50ms instead of on every packet.
    pub(crate) quality_cache: CachedQuality,
    /// Batch sender for optimized packet transmission.
    /// Buffers up to 16 packets before flushing, reducing syscall overhead.
    pub batch_sender: BatchSender,
    /// Link lifecycle phase — determines scheduler eligibility.
    #[cfg(feature = "test-internals")]
    pub phase: LinkPhase,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) phase: LinkPhase,
    /// Latest weak-link classifier verdict. Updated each housekeeping
    /// tick from `WeakLinkFilter::classify`. Consumed by Enhanced
    /// selection as an admission gate.
    pub weak: bool,
    /// Why the classifier called this link weak. Selection needs the reason,
    /// not just the verdict: a *late* link must be kept off unique payload
    /// entirely, while an under-used one has to keep carrying a little real
    /// traffic to earn back the share that clears the verdict.
    pub weak_reason: WeakReason,
    /// Transient per-select flag: this link is held out of the payload
    /// rotation on quality grounds — it is late (or loss-degraded) while a
    /// healthy link can carry, or it lost the sole-carrier election.
    /// Recomputed on every selection pass, like `stall_gated`. Read by the
    /// shell to decide which links get duplicate probes.
    pub(crate) quality_excluded: bool,
    /// Latest CC state from `LinkCcController::tick_all`. Drives the CC
    /// controller's own per-window bitrate backoff. It is intentionally
    /// *not* a routing-admission gate: `BackingOff` flips on a single
    /// loss window and would make selection twitchy, so the routing gate
    /// uses the sustained `loss_degraded` latch instead.
    pub cc_backing_off: bool,
    /// Latest `target_bps` from `LinkCcController::tick_all`. Consumed
    /// by Enhanced selection as a soft cap: when the link's measured
    /// throughput approaches this value the link's score is scaled
    /// down so the scheduler routes less traffic through it before
    /// loss actually fires. `0` means "no signal" — selection skips
    /// the cap.
    pub cc_target_bps: u64,
    /// Latched verdict from `LinkCongestionState`: the link's
    /// time-decayed loss EWMA has been sustained high (see
    /// `LOSS_DEGRADE_*`, ~4s sustain with hysteresis). Drives a graded
    /// demotion to `Degraded` in the phase machine *and* the Enhanced
    /// selection loss-admission gate. It never removes the link from
    /// scheduling (a genuinely dead link is handled by
    /// `is_timed_out`/`CONN_TIMEOUT`); a gated link keeps a trickle of
    /// traffic so the loss EWMA can recover and clear the latch.
    pub loss_degraded: bool,
}

impl SrtlaConnection {
    /// Build a fresh, socket-free connection in the `Registering` phase.
    ///
    /// Pure: the shell creates the socket and the matching `ConnIo` separately
    /// (see `sender::connections::connect_uplink`) and stores them under this
    /// `conn_id`. `now` anchors the startup grace window and the bitrate
    /// tracker; `conn_id` is generated shell-side so it can key the I/O map.
    pub fn new_registering(conn_id: u64, label: String, local_ip: IpAddr, now: u64) -> Self {
        Self {
            conn_id,
            local_ip,
            label,
            connected: false,
            window: WINDOW_DEF * WINDOW_MULT,
            in_flight_packets: 0,
            packet_log: FxHashMap::with_capacity_and_hasher(PKT_LOG_SIZE, Default::default()),
            probe_log: FxHashMap::default(),
            highest_acked_seq: i32::MIN,
            last_received: None,
            last_sent: None,
            last_keepalive_sent: None,
            last_ack_or_rtt_sample_ms: 0,
            stall_gated: false,
            stall_latched_since_ms: 0,
            stall_recovery_since_ms: 0,
            stall_gate_events: 0,
            stall_probe_counter: 0,
            stall_released_at_ms: 0,
            stall_rejoin_backoff: 0,
            stall_rejoin_ramp_start_ms: 0,
            stall_rejoin_ramp_ms: 0,
            stall_rejoin_ramp_from_stall_gate: false,
            sole_carrier: false,
            sole_carrier_since_ms: 0,
            sole_carrier_excluded: false,
            sole_carrier_elections: 0,
            silence_pulled: false,
            silence_pulls: 0,
            conn_timeout_ms: crate::config_snapshot::CONN_TIMEOUT_MS,
            rtt: RttTracker::default(),
            congestion: CongestionControl::default(),
            bitrate: BitrateTracker::new(now),
            reconnection: ReconnectionState {
                startup_grace_deadline_ms: now + STARTUP_GRACE_MS,
                ..Default::default()
            },
            quality_cache: CachedQuality::default(),
            batch_sender: BatchSender::new(),
            phase: LinkPhase::Registering,
            weak: false,
            weak_reason: crate::selection::classifier::WeakReason::Healthy,
            quality_excluded: false,
            cc_backing_off: false,
            cc_target_bps: 0,
            loss_degraded: false,
        }
    }

    #[inline(always)]
    pub fn get_score(&self) -> i32 {
        if !self.connected {
            return -1;
        }
        // Mirror C's select_conn(): score = window / (in_flight + 1).
        // Include queued (not-yet-flushed) packets so the score drops immediately
        // when a packet is queued, matching C's reg_pkt() which increments
        // in_flight_pkts per packet before the next select_conn() call.
        let total_in_flight = self
            .in_flight_packets
            .saturating_add(self.batch_sender.queued_count());
        let denom = total_in_flight.saturating_add(1).max(1);
        self.window / denom
    }

    /// Queue a data packet for batched sending.
    ///
    /// Returns true if the batch queue is full and needs to be flushed.
    /// The caller should call `flush_batch()` when this returns true or when
    /// the flush timer fires.
    #[inline]
    pub fn queue_data_packet(&mut self, data: &[u8], seq: Option<u32>, send_time_ms: u64) -> bool {
        // Track bytes for bitrate calculation (tracked at queue time)
        self.bitrate.update_on_send(data.len() as u64);
        self.batch_sender.queue_packet(data, seq, send_time_ms)
    }

    /// Queue a duplicate probe: a redundant copy of a packet whose unique copy
    /// went out on another link, sent to keep this link measurable while it is
    /// held out of the payload rotation.
    ///
    /// Returns true if the batch queue is full and needs flushing.
    ///
    /// The sequence is recorded in [`Self::probe_log`] rather than the packet
    /// log, and queued as untracked so the batch drain does not register it
    /// in-flight. That keeps three things honest: the probe survives the
    /// cumulative-ACK sweep long enough for its own SRTLA ACK to arrive (the
    /// whole point of sending it), it never inflates an in-flight count that
    /// represents payload this link owes, and a NAK for that sequence stays
    /// attributed to the link that actually carried the stream data.
    pub fn queue_probe_packet(&mut self, data: &[u8], seq: u32, send_time_ms: u64) -> bool {
        self.bitrate.update_on_send(data.len() as u64);
        self.record_probe(seq as i32, send_time_ms);
        self.batch_sender.queue_packet(data, None, send_time_ms)
    }

    /// Record an outstanding probe, expiring stale entries when the log grows.
    ///
    /// Probes are only ever removed by their own ACK, which may never come, so
    /// the log needs an upper bound. Anything older than the longest round trip
    /// this sender will believe cannot still be answered.
    fn record_probe(&mut self, seq: i32, send_time_ms: u64) {
        use crate::config_snapshot::{PROBE_LOG_MAX_AGE_MS, PROBE_LOG_SOFT_CAP};
        if self.probe_log.len() >= PROBE_LOG_SOFT_CAP {
            let cutoff = send_time_ms.saturating_sub(PROBE_LOG_MAX_AGE_MS);
            self.probe_log.retain(|_, sent| *sent >= cutoff);
            // Still full of live entries: this link is being probed faster than
            // it can answer, so the oldest are the least useful. Start over
            // rather than grow without bound.
            if self.probe_log.len() >= PROBE_LOG_SOFT_CAP {
                self.probe_log.clear();
            }
        }
        self.probe_log.insert(seq, send_time_ms);
    }

    /// Check if the batch queue needs time-based flushing (15ms interval)
    #[inline]
    pub fn needs_batch_flush(&self, now_ms: u64) -> bool {
        self.batch_sender.needs_time_flush(now_ms)
    }

    /// Check if there are any packets in the batch queue
    #[inline]
    pub fn has_queued_packets(&self) -> bool {
        self.batch_sender.has_queued_packets()
    }

    /// Drain the batch queue for transmission.
    ///
    /// Pure builder: registers each tracked packet as in-flight, stamps
    /// `last_sent`, and returns the queued datagrams. The shell sends them (see
    /// `net::send_all_datagrams`) and calls [`Self::mark_for_recovery`]
    /// if the send fails. In-flight registration is optimistic — a failed send
    /// triggers `mark_for_recovery`, which clears `packet_log` anyway, so the
    /// registrations never leak. Empty when nothing was queued.
    pub fn take_batch(&mut self, now: u64) -> SmallVec<DrainedPacket, 32> {
        let batch = self.batch_sender.drain(now);
        if batch.is_empty() {
            return batch;
        }
        for (_, seq, send_time_ms) in &batch {
            if let Some(s) = seq {
                self.register_packet(*s as i32, *send_time_ms);
            }
        }
        self.last_sent = Some(now);
        batch
    }

    /// Build an extended keepalive packet and record that it was sent.
    ///
    /// Pure builder: the shell transmits the returned bytes on this link's
    /// socket. State (`last_sent`, `last_keepalive_sent`, RTT-probe arming) is
    /// updated optimistically against the injected clock — a dropped keepalive
    /// is just a lost UDP datagram the next tick re-sends, so there is nothing
    /// to roll back on a failed send.
    pub fn keepalive_packet(&mut self, now: u64) -> [u8; SRTLA_KEEPALIVE_EXT_LEN] {
        // Create extended keepalive with connection info (telemetry for receiver)
        let info = ConnectionInfo {
            conn_id: self.conn_id as u32,
            window: self.window,
            in_flight: self.in_flight_packets,
            rtt_ms: self.rtt.kalman_rtt.value() as u32,
            nak_count: self.congestion.nak_count as u32,
            bitrate_bytes_per_sec: (self.bitrate.current_bitrate_bps / 8.0) as u32,
        };
        let pkt = create_keepalive_packet_ext(info, now);
        self.last_sent = Some(now);
        self.last_keepalive_sent = Some(now);
        // Only set waiting flag and timestamp when we intend to measure RTT
        if !self.rtt.waiting_for_keepalive_response
            && (self.rtt.last_rtt_measurement_ms == 0
                || now.saturating_sub(self.rtt.last_rtt_measurement_ms) > 3000)
        {
            self.rtt.record_keepalive_sent(now);
        }
        pkt
    }

    /// Stamp `last_sent` after the shell transmits an out-of-band packet
    /// (registration REG1/REG2) on this link's socket.
    #[inline]
    pub fn note_sent(&mut self, now: u64) {
        self.last_sent = Some(now);
    }

    /// Build a REG2 probe packet and arm the startup grace window.
    ///
    /// Pure builder: the shell sends the returned bytes. `now` is both the
    /// grace-window anchor and the probe's send timestamp.
    pub fn probe_reg2_packet(
        &mut self,
        probe_id: &[u8; SRTLA_ID_LEN],
        now: u64,
    ) -> [u8; SRTLA_TYPE_REG2_LEN] {
        let pkt = create_reg2_packet(probe_id);
        self.reconnection.startup_grace_deadline_ms = now + STARTUP_GRACE_MS;
        pkt
    }

    pub fn is_rtt_stable(&self) -> bool {
        self.rtt.is_stable()
    }

    pub fn get_smooth_rtt_ms(&self) -> f64 {
        // The 2-state Kalman filter can overshoot negative on a sharp high->low
        // RTT transition; a negative RTT is meaningless and would leak into the
        // selection/CC math, so clamp it. Callers that need to tell a never-measured
        // link from a genuine ~0 already test `smooth_rtt <= 0.0`.
        self.rtt.kalman_rtt.value().max(0.0)
    }

    /// RTT velocity (trend) in ms/sample from the Kalman filter.
    /// Positive = rising RTT (congestion building), negative = falling.
    pub fn get_rtt_velocity(&self) -> f64 {
        self.rtt.kalman_rtt.velocity()
    }

    pub fn get_rtt_min_ms(&self) -> f64 {
        self.rtt.rtt_min_ms
    }

    pub fn get_rtt_jitter_ms(&self) -> f64 {
        self.rtt.rtt_jitter_ms
    }

    /// Whether this link's RTT shows a standing queue forming (the
    /// recent propagation floor lifted above the long-term floor),
    /// distinct from jitter. Consumed by the weak-link classifier as an
    /// early-warning signal so the scheduler eases off before the queue
    /// turns into loss.
    pub fn queue_building_suspected(&self) -> bool {
        self.rtt.queue_building_suspected()
    }

    pub fn needs_rtt_measurement(&self, now_ms: u64) -> bool {
        self.rtt.needs_measurement(
            self.connected,
            self.reconnection.connection_established_ms,
            now_ms,
        )
    }

    pub fn needs_keepalive(&self, now_ms: u64) -> bool {
        // Send keepalive every IDLE_TIME (1s) unconditionally on all connections.
        // Moblin does this with standard 10-byte keepalives; we use extended 38-byte
        // keepalives to provide the receiver with telemetry (window, RTT, NAKs, bitrate).
        if !self.connected {
            return false;
        }

        match self.last_keepalive_sent {
            None => true,
            Some(last) => now_ms.saturating_sub(last) >= IDLE_TIME * 1000,
        }
    }

    pub fn perform_window_recovery(&mut self, now_ms: u64) {
        let velocity = self.rtt.kalman_rtt.velocity();
        self.congestion.perform_window_recovery(
            &mut self.window,
            self.connected,
            velocity,
            &self.label,
            now_ms,
        );
    }

    /// Record an RTT probe and advance warming → live if enough probes collected.
    pub fn record_rtt_probe(&mut self) {
        if let LinkPhase::Warming { rtt_probes, .. } = &mut self.phase {
            *rtt_probes += 1;
            if *rtt_probes >= WARMING_RTT_PROBES {
                debug!("{}: warming complete, transitioning to Live", self.label);
                self.phase = LinkPhase::Live;
            }
        }
    }

    /// Drive phase transitions based on current connection health.
    ///
    /// Called from housekeeping. A degraded link stays **schedulable**:
    /// demotion only lowers its score (via quality + the `Degraded`
    /// phase), it never removes the link. Removing a link starves it of
    /// the ACK traffic that proves its own recovery, which on bonded
    /// cellular turns a transient HARQ stall (400-800ms) into a
    /// self-sustaining false death. A genuinely unresponsive link is
    /// pruned by `is_timed_out`/`CONN_TIMEOUT`, not here.
    pub fn update_phase(&mut self, now_ms: u64) {
        const DEGRADED_QUALITY_THRESHOLD: f64 = 0.5;
        const DEGRADED_NAK_BURST_THRESHOLD: i32 = 5;

        // Combined degradation signal: the fast NAK-quality path catches
        // mild degradation; the sustained loss-EWMA verdict
        // (`loss_degraded`, latched with hysteresis in
        // `LinkCongestionState`) catches a link that is genuinely
        // shedding most of its traffic without a binary kill.
        let nak_degraded = self.quality_cache.multiplier < DEGRADED_QUALITY_THRESHOLD
            && self.congestion.nak_burst_count >= DEGRADED_NAK_BURST_THRESHOLD;
        let nak_recovered = self.quality_cache.multiplier >= DEGRADED_QUALITY_THRESHOLD
            && self.congestion.nak_burst_count < DEGRADED_NAK_BURST_THRESHOLD;

        match self.phase {
            // Auto-promote to Live if warming takes too long.
            LinkPhase::Warming { entered_ms, .. }
                if now_ms.saturating_sub(entered_ms) >= WARMING_TIMEOUT_MS =>
            {
                debug!(
                    "{}: warming timeout ({}ms), auto-promoting to Live",
                    self.label, WARMING_TIMEOUT_MS
                );
                self.phase = LinkPhase::Live;
            }
            LinkPhase::Live if nak_degraded || self.loss_degraded => {
                debug!(
                    "{}: Live -> Degraded (quality={:.2}, nak_burst={}, loss_degraded={})",
                    self.label,
                    self.quality_cache.multiplier,
                    self.congestion.nak_burst_count,
                    self.loss_degraded
                );
                self.phase = LinkPhase::Degraded;
            }
            // Recover to Live only when both signals clear: the fast
            // NAK-quality path AND the latched loss-EWMA verdict.
            LinkPhase::Degraded if nak_recovered && !self.loss_degraded => {
                debug!(
                    "{}: Degraded -> Live (quality={:.2})",
                    self.label, self.quality_cache.multiplier
                );
                self.phase = LinkPhase::Live;
            }
            // Registering, plus Warming/Live/Degraded whose guards did
            // not fire, hold their phase.
            _ => {}
        }
    }

    /// Whether this link is eligible for packet scheduling.
    pub fn is_schedulable(&self) -> bool {
        self.phase.is_schedulable()
    }

    /// Scheduling weight contributed by this link's phase
    /// (see [`LinkPhase::weight`]).
    #[inline(always)]
    pub fn phase_weight(&self) -> f64 {
        self.phase.weight()
    }

    /// Effective delivery-proof staleness window in ms: RTT-adaptive between
    /// [`crate::config_snapshot::STALL_STALE_FLOOR_MS`] and the configured
    /// ceiling. Proof on a loaded link arrives every RTT (earned SRTLA ACKs),
    /// so the window scales with the path instead of always waiting a fixed
    /// 3 s — on a 50 ms link that fixed wait was ~3 s of payload committed to
    /// a black hole. No RTT baseline yet falls back to the ceiling; a ceiling
    /// configured below the floor wins (operator override).
    #[inline]
    pub fn effective_stall_stale_ms(&self, ceiling_ms: u64) -> u64 {
        use crate::config_snapshot::{STALL_STALE_FLOOR_MS, STALL_STALE_RTT_MULT};
        let srtt = self.get_smooth_rtt_ms();
        if srtt <= 0.0 {
            return ceiling_ms;
        }
        ((srtt as u64).saturating_mul(STALL_STALE_RTT_MULT))
            .max(STALL_STALE_FLOOR_MS)
            .min(ceiling_ms)
    }

    /// `stall_deselect` signal (pure read; never mutates). True for a connected
    /// link whose in-flight backlog is at or above `min_in_flight` AND whose
    /// last delivery proof (earned-ACK or keepalive-RTT sample) is older than
    /// the effective staleness window ([`Self::effective_stall_stale_ms`] of
    /// `stale_ceiling_ms`). `now_ms` is the selection clock.
    ///
    /// A link that has produced no proof yet (`last_ack_or_rtt_sample_ms == 0`)
    /// is never stalled: a fresh burst before its first ACK must not be
    /// mistaken for a black hole. A genuinely dead-from-birth link is pruned by
    /// `is_timed_out`/`CONN_TIMEOUT`, not here. This is a selection penalty
    /// input ONLY — it never affects `is_timed_out`/re-registration/CONN_TIMEOUT.
    #[inline]
    pub fn is_stalled(&self, now_ms: u64, min_in_flight: i32, stale_ceiling_ms: u64) -> bool {
        self.connected
            && self.in_flight_packets >= min_in_flight
            && self.last_ack_or_rtt_sample_ms != 0
            && now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms)
                >= self.effective_stall_stale_ms(stale_ceiling_ms)
    }

    /// Drive the asymmetric stall latch: quick to drop, conservative to
    /// rejoin. Called by `apply_stall_gate` on every selection pass.
    ///
    /// Engaging is immediate once [`Self::is_stalled`] fires. Rejoining
    /// requires an *uninterrupted* run of fresh delivery proof spanning
    /// [`crate::config_snapshot::STALL_REJOIN_DWELL_MULT`] x the effective
    /// staleness window: a single keepalive echo (or the backlog draining via
    /// cumulative ACKs carried by the healthy links) is not evidence the path
    /// can deliver again, and un-gating on it flaps payload back onto a
    /// still-marginal link. Proof going stale mid-run resets the run.
    pub fn update_stall_latch(&mut self, now_ms: u64, min_in_flight: i32, stale_ceiling_ms: u64) {
        // Escalation from the fast tier: a silence-pulled link receives no
        // payload, so cumulative ACKs drain the backlog and the raw
        // `is_stalled` in-flight precondition can never fire again. If the
        // pull is still held when the delivery proof goes fully stale, the
        // stall is sustained, not a micro-pause — latch it so recovery goes
        // through the dwell and probes instead of a cold readmit.
        let proof_fully_stale = self.last_ack_or_rtt_sample_ms != 0
            && now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms)
                >= self.effective_stall_stale_ms(stale_ceiling_ms);
        if self.is_stalled(now_ms, min_in_flight, stale_ceiling_ms)
            || (self.silence_pulled && proof_fully_stale)
        {
            if self.stall_latched_since_ms == 0 {
                // Judge the rejoin that just ended before starting a new gate:
                // one that could not outlast its probation earns a longer wait
                // for the next retry, one that did clears the penalty outright.
                let probation_ms = self
                    .effective_stall_stale_ms(stale_ceiling_ms)
                    .saturating_mul(crate::config_snapshot::STALL_REJOIN_PROBATION_MULT);
                self.stall_rejoin_backoff = stall_rejoin_backoff_next(
                    self.stall_rejoin_backoff,
                    self.stall_released_at_ms,
                    now_ms.saturating_sub(self.stall_released_at_ms),
                    probation_ms,
                );
                debug!(
                    "{}: stall latch engaged, rejoin dwell now {}x",
                    self.label, self.stall_rejoin_backoff
                );
                self.stall_latched_since_ms = now_ms;
                self.stall_gate_events += 1;
            }
            self.stall_recovery_since_ms = 0;
            return;
        }
        if self.stall_latched_since_ms == 0 {
            return;
        }

        let stale_ms = self.effective_stall_stale_ms(stale_ceiling_ms);
        let proof_fresh = self.last_ack_or_rtt_sample_ms != 0
            && now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms) < stale_ms;
        if !proof_fresh {
            self.stall_recovery_since_ms = 0;
            return;
        }
        if self.stall_recovery_since_ms == 0 {
            self.stall_recovery_since_ms = now_ms;
        }
        let base_dwell_ms =
            stale_ms.saturating_mul(crate::config_snapshot::STALL_REJOIN_DWELL_MULT);
        // The wait stretches while rejoins keep failing to hold, so a link that
        // cannot carry its share is still retried — just far enough apart that
        // the stream stops paying a transition for every attempt. Only the
        // *wait* scales: the ramp below stays at the base dwell, since how
        // gently a link should be reloaded does not depend on how long it sat
        // out, and scaling it too would leave a link at the ramp floor for
        // minutes after it finally recovered.
        let dwell_ms = base_dwell_ms.saturating_mul(self.stall_rejoin_backoff.max(1) as u64);
        if now_ms.saturating_sub(self.stall_recovery_since_ms) >= dwell_ms {
            debug!(
                "{}: stall latch released after sustained proof ({}ms dwell), ramping share back \
                 over {}ms",
                self.label, dwell_ms, base_dwell_ms
            );
            self.stall_latched_since_ms = 0;
            self.stall_recovery_since_ms = 0;
            self.stall_released_at_ms = now_ms;
            // Arm the share ramp: the link has proven it can deliver probes,
            // not that it can carry the stream (see `rejoin_ramp_multiplier`).
            self.arm_rejoin_ramp(now_ms, base_dwell_ms, true);
        }
    }

    /// Start the post-rejoin share ramp, unless one is already running.
    ///
    /// Called wherever a link stops being held out of the payload rotation:
    /// the stall latch releasing, a sole-carrier election ending, or a quality
    /// exclusion lifting. All three leave the link with a drained backlog and
    /// an inflated score it did not earn, which is what the ramp prices out
    /// (see [`rejoin_ramp_multiplier`]).
    ///
    /// An in-progress ramp is never restarted. The gates above can re-arm at
    /// classifier cadence when a verdict flaps, and restarting each time would
    /// pin a link at the ramp floor for as long as the flapping lasts — the
    /// link would never finish earning its share back, which is its own kind
    /// of starvation. `from_stall_gate` records who armed it, so disabling the
    /// stall guard at runtime can drop the ramps that guard created without
    /// touching anyone else's.
    #[inline]
    pub(crate) fn arm_rejoin_ramp(&mut self, now_ms: u64, ramp_ms: u64, from_stall_gate: bool) {
        if self.rejoin_ramp_multiplier(now_ms) < 1.0 {
            return;
        }
        self.stall_rejoin_ramp_start_ms = now_ms;
        self.stall_rejoin_ramp_ms = ramp_ms;
        self.stall_rejoin_ramp_from_stall_gate = from_stall_gate;
    }

    /// Whether this link is currently held out of the payload rotation on
    /// quality grounds. The shell reads this to decide which links get
    /// duplicate probes; stats and selection read the underlying flags.
    #[inline(always)]
    pub fn is_quality_excluded(&self) -> bool {
        self.quality_excluded
    }

    /// Drop every flag owned by the Enhanced quality gates.
    ///
    /// Only Enhanced selection maintains these, and the scheduling mode is
    /// switchable at runtime, so Classic has to clear them rather than leave
    /// them frozen at whatever they held when the mode changed. The rejoin
    /// ramp is *not* cleared here: it is a scoring de-rate that Classic
    /// ignores anyway, and it should still be running if the mode switches
    /// back before it elapses.
    pub(crate) fn clear_quality_gate_state(&mut self) {
        self.quality_excluded = false;
        self.sole_carrier = false;
        self.sole_carrier_excluded = false;
        self.sole_carrier_since_ms = 0;
    }

    /// Whether this link currently holds the sole-carrier role (stats export).
    #[inline(always)]
    pub fn is_sole_carrier(&self) -> bool {
        self.sole_carrier
    }

    /// Whether this link is currently held out of the rotation because a
    /// sibling holds the sole-carrier role (stats export).
    #[inline(always)]
    pub fn is_sole_carrier_excluded(&self) -> bool {
        self.sole_carrier_excluded
    }

    /// Cumulative sole-carrier handovers *from another link* over its life.
    #[inline(always)]
    pub fn sole_carrier_elections(&self) -> u64 {
        self.sole_carrier_elections
    }

    /// Fraction of its natural score this link should compete with right now,
    /// in `[STALL_REJOIN_RAMP_FLOOR, 1.0]`. `1.0` whenever no ramp is running.
    #[inline]
    pub fn rejoin_ramp_multiplier(&self, now_ms: u64) -> f64 {
        rejoin_ramp_multiplier(
            self.stall_rejoin_ramp_start_ms,
            self.stall_rejoin_ramp_ms,
            now_ms,
        )
    }

    /// Rejoin-dwell multiplier this link is currently serving (stats/tests).
    /// `1` whenever no backoff has been earned.
    #[inline(always)]
    pub fn stall_rejoin_backoff(&self) -> u32 {
        self.stall_rejoin_backoff.max(1)
    }

    /// Whether a post-rejoin share ramp is currently running (stats/telemetry).
    #[inline(always)]
    pub fn is_rejoin_ramping(&self, now_ms: u64) -> bool {
        self.rejoin_ramp_multiplier(now_ms) < 1.0
    }

    /// Whether the stall latch is currently engaged (independent of whether a
    /// healthy alternative exists to make it an active routing gate).
    #[inline(always)]
    pub fn stall_latched(&self) -> bool {
        self.stall_latched_since_ms != 0
    }

    /// Clear the stall latch without touching the event counter. Used when the
    /// guard is disabled at runtime so selection returns to baseline instantly.
    pub(crate) fn clear_stall_latch(&mut self) {
        self.stall_latched_since_ms = 0;
        self.stall_recovery_since_ms = 0;
        // With the guard off there are no rejoins to judge, so the escalation
        // must not survive to lengthen the first dwell if it is turned back on.
        self.stall_released_at_ms = 0;
        self.stall_rejoin_backoff = 0;
        // Including any ramp this guard armed: with it off, its contribution
        // to scoring must be gone. Ramps armed by the sole-carrier election or
        // the quality exclusion survive — those gates are independent of this
        // one and still running, and wiping their ramps here would silently
        // undo them on every selection pass.
        if self.stall_rejoin_ramp_from_stall_gate {
            self.stall_rejoin_ramp_start_ms = 0;
            self.stall_rejoin_ramp_ms = 0;
            self.stall_rejoin_ramp_from_stall_gate = false;
        }
    }

    /// Whether this link is currently stall-gated (routing view; stats export).
    #[inline(always)]
    pub fn is_stall_gated(&self) -> bool {
        self.stall_gated
    }

    /// Cumulative stall-latch engagements over this connection's life.
    #[inline(always)]
    pub fn stall_gate_events(&self) -> u64 {
        self.stall_gate_events
    }

    /// Duplicate-probe cadence on a gated link: true once per
    /// [`crate::config_snapshot::STALL_PROBE_ONE_IN_N`] calls. The caller
    /// invokes this once per routed data packet for each gated link, mirroring
    /// librist's per-leg trickle counter.
    #[inline]
    pub fn stall_probe_due(&mut self) -> bool {
        self.stall_probe_counter += 1;
        if self.stall_probe_counter >= crate::config_snapshot::STALL_PROBE_ONE_IN_N {
            self.stall_probe_counter = 0;
            return true;
        }
        false
    }

    /// RTT-scaled window (ms) of total inbound silence after which a loaded
    /// link is transiently pulled: `max(floor, 2 x smoothed RTT)`, capped at
    /// the effective staleness window (beyond that the latch owns the
    /// decision, so the two tiers cannot disagree about who acts).
    #[inline]
    pub fn silence_pull_window_ms(&self, stale_ceiling_ms: u64) -> u64 {
        use crate::config_snapshot::{SILENCE_PULL_FLOOR_MS, SILENCE_PULL_RTT_MULT};
        let srtt = self.get_smooth_rtt_ms();
        let base = if srtt <= 0.0 {
            SILENCE_PULL_FLOOR_MS
        } else {
            ((srtt as u64).saturating_mul(SILENCE_PULL_RTT_MULT)).max(SILENCE_PULL_FLOOR_MS)
        };
        base.min(self.effective_stall_stale_ms(stale_ceiling_ms))
    }

    /// Fast silence signal (pure read): a connected link with a full backlog
    /// (`in_flight >= min_in_flight`) that has received *nothing* — under
    /// load, SRTLA ACKs arrive continuously, so total silence is abnormal —
    /// for [`Self::silence_pull_window_ms`]. The routing pull this drives is
    /// transient and self-clearing (any inbound byte refreshes
    /// `last_received`), the deliberate opposite of the sticky latch: fast to
    /// react during a stall the link may ride out (e.g. a HARQ pause), free
    /// to readmit the moment the link speaks. Never a liveness signal.
    #[inline]
    pub fn is_briefly_silent(
        &self,
        now_ms: u64,
        min_in_flight: i32,
        stale_ceiling_ms: u64,
    ) -> bool {
        if !self.connected || self.in_flight_packets < min_in_flight {
            return false;
        }
        let Some(last_received) = self.last_received else {
            return false;
        };
        now_ms.saturating_sub(last_received) >= self.silence_pull_window_ms(stale_ceiling_ms)
    }

    /// Drive the silence-pull flag and its rising-edge counter. Called by
    /// `apply_stall_gate` on every selection pass, BEFORE the latch update
    /// (the latch escalates from this flag).
    ///
    /// The pull engages only on the loaded-and-silent signal, but releases
    /// only when the link actually SPEAKS (a fresh inbound byte) — not when
    /// its backlog drains. Cumulative SRT ACKs delivered via the healthy
    /// links clear a pulled link's in-flight within an RTT, so releasing on
    /// drain would readmit the still-silent link, refill it, and re-pull it
    /// in a tight duty cycle that both dribbles payload into the hole and
    /// starves the latch of its in-flight precondition.
    pub(crate) fn update_silence_pull(
        &mut self,
        now_ms: u64,
        min_in_flight: i32,
        stale_ceiling_ms: u64,
    ) {
        if self.is_briefly_silent(now_ms, min_in_flight, stale_ceiling_ms) {
            if !self.silence_pulled {
                self.silence_pulls += 1;
                debug!("{}: silence pull engaged", self.label);
            }
            self.silence_pulled = true;
            return;
        }
        if !self.silence_pulled {
            return;
        }
        let window = self.silence_pull_window_ms(stale_ceiling_ms);
        let spoke = self
            .last_received
            .is_some_and(|lr| now_ms.saturating_sub(lr) < window);
        if spoke || !self.connected {
            self.silence_pulled = false;
        }
        // Otherwise: drained but still mute — hold the pull until the link
        // speaks or the latch takes over.
    }

    /// Cumulative silence-pull engagements over this connection's life.
    #[inline(always)]
    pub fn silence_pulls(&self) -> u64 {
        self.silence_pulls
    }

    /// Whether this link has gone silent past its liveness timeout
    /// (`conn_timeout_ms`, default `CONN_TIMEOUT`; runtime-tunable so the
    /// window can scale with the receiver's latency budget).
    ///
    /// `last_received` is a `now_ms()` monotonic millisecond stamp (the single
    /// clock this whole codebase runs on), so the timeout is a plain difference
    /// against the caller's `now_ms`. Tests drive it by stamping `last_received` a
    /// chosen interval in the past (e.g. `now_ms() - (CONN_TIMEOUT + 1) * 1000`);
    /// they no longer advance a tokio virtual clock, because this compares against
    /// the monotonic clock, not `tokio::time::Instant`.
    #[inline(always)]
    pub fn is_timed_out(&self, now_ms: u64) -> bool {
        let now = now_ms;
        // During initial registration (not yet connected), allow grace period
        if !self.connected {
            // If this connection was never established (connection_established_ms == 0),
            // check if we're still within the startup grace period
            if self.reconnection.connection_established_ms == 0
                && now < self.reconnection.startup_grace_deadline_ms
            {
                return false;
            }
            // After grace period, or for connections that were previously established,
            // if we've never received anything or haven't received in a while, consider it timed out
            return self
                .last_received
                .is_none_or(|lr| now.saturating_sub(lr) >= self.conn_timeout_ms);
        }

        // For established connections, check normal timeout
        if let Some(lr) = self.last_received {
            now.saturating_sub(lr) >= self.conn_timeout_ms
        } else {
            false
        }
    }

    /// Clear state accumulated during pre-registration phase.
    ///
    /// Called when REG3 is received to prevent phantom in-flight counts
    /// and early NAK penalties from persisting into the connected state.
    /// Before REG3, `forward_via_connection()` may have queued and sent
    /// data packets, creating `packet_log` entries that will never be
    /// properly ACKed. Early NAKs from these packets would also penalize
    /// the connection's quality score during startup.
    pub fn clear_pre_registration_state(&mut self, now_ms: u64) {
        if !self.packet_log.is_empty() || self.congestion.nak_count > 0 {
            debug!(
                "{}: clearing pre-registration state ({} in-flight, {} NAKs)",
                self.label,
                self.packet_log.len(),
                self.congestion.nak_count
            );
        }
        self.packet_log.clear();
        self.probe_log.clear();
        self.in_flight_packets = 0;
        self.highest_acked_seq = i32::MIN;
        self.congestion.reset();
        self.batch_sender.reset();
        self.quality_cache = CachedQuality::default();
        // REG3 received — begin warming phase
        self.phase = LinkPhase::Warming {
            rtt_probes: 0,
            entered_ms: now_ms,
        };
    }

    /// Reset core connection state (window, packet tracking, batch queue).
    /// Used by both mark_for_recovery and reset_state.
    fn reset_core_state(&mut self) {
        self.connected = false;
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log.clear();
        self.probe_log.clear();
        self.highest_acked_seq = i32::MIN;
        self.batch_sender.reset();
        self.phase = LinkPhase::Registering;
        // A reset link has no delivery proof; clear the stall signal so it is
        // not classed as stalled the instant it reconnects with a backlog.
        // The `stall_gate_events` counter deliberately survives: it counts
        // engagements over the link's life, and a reset is often the *result*
        // of the stall it just latched on.
        self.last_ack_or_rtt_sample_ms = 0;
        self.stall_gated = false;
        self.stall_latched_since_ms = 0;
        self.stall_recovery_since_ms = 0;
        self.stall_probe_counter = 0;
        // The rejoin backoff is a judgement about a path that a reset link no
        // longer has: it comes back through registration with a fresh socket
        // and a cold window, so the next gate starts from the base dwell.
        self.stall_released_at_ms = 0;
        self.stall_rejoin_backoff = 0;
        // The rejoin ramp exists to stop an inflated window from seizing the
        // stream; a reset link goes back to the default window with an empty
        // packet log, which is the same cold start every link makes at
        // startup, so there is nothing left to ramp.
        self.stall_rejoin_ramp_start_ms = 0;
        self.stall_rejoin_ramp_ms = 0;
        self.stall_rejoin_ramp_from_stall_gate = false;
        // A reset link cannot be carrying anything, so it cannot hold the role.
        // `sole_carrier_elections` survives like the other event counters.
        self.sole_carrier = false;
        self.sole_carrier_since_ms = 0;
        self.sole_carrier_excluded = false;
        // `silence_pulls` survives like `stall_gate_events`: both count
        // engagements over the link's life.
        self.silence_pulled = false;
    }

    /// Mark connection for recovery (C-style), similar to setting last_rcvd = 1.
    /// Soft reset: clears packet state but preserves congestion/bitrate stats.
    pub fn mark_for_recovery(&mut self) {
        self.last_received = None;
        self.last_keepalive_sent = None;
        self.rtt.last_keepalive_sent_ms = 0;
        self.rtt.waiting_for_keepalive_response = false;
        self.reset_core_state();
        // Set grace deadline to 0 to indicate this connection is immediately timed out
        // (matches C behavior of setting last_rcvd = 1)
        self.reconnection.startup_grace_deadline_ms = 0;
    }

    pub fn time_since_last_nak_ms(&self, now_ms: u64) -> Option<u64> {
        self.congestion.time_since_last_nak_ms(now_ms)
    }

    pub fn total_nak_count(&self) -> i32 {
        self.congestion.nak_count
    }

    pub fn nak_burst_count(&self) -> i32 {
        self.congestion.nak_burst_count
    }

    pub fn connection_established_ms(&self) -> u64 {
        self.reconnection.connection_established_ms
    }

    /// Get the cached quality multiplier, recalculating if stale.
    ///
    /// This is more efficient than calling `calculate_quality_multiplier()` on every packet
    /// because it only recalculates every 50ms.
    #[inline(always)]
    pub fn get_cached_quality_multiplier(&mut self, current_time_ms: u64) -> f64 {
        use crate::selection::calculate_quality_multiplier;

        if current_time_ms.saturating_sub(self.quality_cache.last_calculated_ms)
            >= QUALITY_CACHE_INTERVAL_MS
        {
            self.quality_cache.multiplier = calculate_quality_multiplier(self, current_time_ms);
            self.quality_cache.last_calculated_ms = current_time_ms;
        }
        self.quality_cache.multiplier
    }

    pub fn should_attempt_reconnect(&self, now_ms: u64) -> bool {
        self.reconnection.should_attempt_reconnect(now_ms)
    }

    pub fn record_reconnect_attempt(&mut self, now_ms: u64) {
        self.reconnection.record_attempt(&self.label, now_ms);
    }

    pub fn mark_reconnect_success(&mut self) {
        self.reconnection.mark_success(&self.label);
    }

    /// Calculate current bitrate
    pub fn calculate_bitrate(&mut self, now_ms: u64) {
        self.bitrate.calculate(now_ms);
    }

    /// Get current bitrate in Mbps
    pub fn current_bitrate_mbps(&self) -> f64 {
        self.bitrate.mbps()
    }

    /// Pick the batch regime for this connection from its observed
    /// bitrate. Called from housekeeping each tick; the underlying
    /// `BatchSender::set_regime` is a cheap field write — no-op cost
    /// when the regime is unchanged.
    pub fn recompute_batch_regime(&mut self) {
        let regime =
            crate::connection::batch_send::BatchRegime::from_bps(self.bitrate.current_bitrate_bps);
        self.batch_sender.set_regime(regime);
    }

    /// Reset connection state after the shell replaced this link's socket.
    /// Full reset: clears all state including congestion/bitrate stats. `now`
    /// is the injected clock (was an ambient `now_ms()` read). Socket creation
    /// and the `mark_reconnect_success`/grace-reset bookkeeping live in the
    /// shell's `reconnect_uplink`.
    pub fn reset_for_reconnect(&mut self, now: u64) {
        self.last_received = None;
        self.reset_core_state();

        // Reset submodule state
        self.congestion.reset();
        self.rtt.reset();
        self.bitrate.reset(now);

        // Reset reconnection tracking
        self.reconnection.last_reconnect_attempt_ms = now;
        self.reconnection.reconnect_failure_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR;

    #[test]
    fn ramp_is_inert_when_no_ramp_is_running() {
        // Never gated.
        assert_eq!(rejoin_ramp_multiplier(0, 1000, 500), 1.0);
        // Armed with a zero-length dwell (degenerate config).
        assert_eq!(rejoin_ramp_multiplier(100, 0, 500), 1.0);
        // Ramp already elapsed.
        assert_eq!(rejoin_ramp_multiplier(100, 1000, 1100), 1.0);
        assert_eq!(rejoin_ramp_multiplier(100, 1000, 9999), 1.0);
    }

    #[test]
    fn ramp_rises_linearly_from_the_floor_to_full() {
        assert_eq!(
            rejoin_ramp_multiplier(100, 1000, 100),
            STALL_REJOIN_RAMP_FLOOR
        );
        let quarter = rejoin_ramp_multiplier(100, 1000, 350);
        let half = rejoin_ramp_multiplier(100, 1000, 600);
        let three_quarters = rejoin_ramp_multiplier(100, 1000, 850);
        assert!((quarter - (STALL_REJOIN_RAMP_FLOOR + 0.95 * 0.25)).abs() < 1e-9);
        assert!((half - (STALL_REJOIN_RAMP_FLOOR + 0.95 * 0.50)).abs() < 1e-9);
        assert!((three_quarters - (STALL_REJOIN_RAMP_FLOOR + 0.95 * 0.75)).abs() < 1e-9);
        assert!(quarter < half && half < three_quarters && three_quarters < 1.0);
    }

    #[test]
    fn ramp_never_scores_a_rejoining_link_to_zero() {
        // The link has to carry something, or it can never reveal how it
        // behaves under load and the ramp would gate it forever.
        for elapsed in 0..10 {
            let m = rejoin_ramp_multiplier(1000, 100_000, 1000 + elapsed);
            assert!(m >= STALL_REJOIN_RAMP_FLOOR, "ramp dropped to {m}");
        }
    }

    #[test]
    fn ramp_tolerates_a_clock_that_went_backwards() {
        assert_eq!(
            rejoin_ramp_multiplier(1000, 500, 900),
            STALL_REJOIN_RAMP_FLOOR
        );
    }
}
