mod ack_nak;
pub mod batch_send;
mod bitrate;
mod congestion;
mod incoming;
mod reconnection;
mod rtt;
mod stall_gate;

use std::net::IpAddr;

pub use batch_send::{BATCH_SEND_SIZE, BatchSender, DrainedBatch, DrainedPacket};
pub use bitrate::BitrateTracker;
pub use congestion::CongestionControl;
pub use incoming::SrtlaIncoming;
pub use reconnection::ReconnectionState;
pub use rtt::RttTracker;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use srtla_protocol::*;
pub use stall_gate::{delivery_proof_is_timely, rejoin_ramp_multiplier, stall_rejoin_backoff_next};
use tracing::debug;

use crate::seq::NO_ACK_YET;

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

/// Lowest operator link weight (the IPs file's optional second column).
/// Weights are Moblin's "connection priorities": an integer 1..10.
pub const LINK_WEIGHT_MIN: u8 = 1;
/// Highest operator link weight.
pub const LINK_WEIGHT_MAX: u8 = 10;
/// Above this window a weighted link gets its full weight (Moblin's
/// `windowStableMaximum * windowMultiply`, = `WINDOW_DEF * WINDOW_MULT`).
pub const LINK_WEIGHT_FULL_ABOVE_WINDOW: i32 = 20 * WINDOW_MULT;
/// At or below this window the weight is ignored (Moblin's
/// `windowStableMinimum * windowMultiply`).
pub const LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW: i32 = 10 * WINDOW_MULT;

/// Score multiplier for a link with operator weight `weight` and congestion
/// window `window`, copied from Moblin's `RemoteConnection.score()`:
///
/// - `window > 20 000` (healthy, room to spare): the full weight;
/// - `10 000 < window <= 20 000` (the link is shedding): fades linearly from
///   the weight down to 1;
/// - `window <= 10 000`: 1 (the weight is ignored, plain capacity balancing).
///
/// A weight of 1 (the default for every link) is exactly 1.0 in every band, so
/// an unweighted IPs file schedules byte-for-byte as before. Used by classic
/// selection only.
#[inline]
pub fn link_weight_multiplier(window: i32, weight: u8) -> f32 {
    let weight = f32::from(weight.clamp(LINK_WEIGHT_MIN, LINK_WEIGHT_MAX));
    if window > LINK_WEIGHT_FULL_ABOVE_WINDOW {
        weight
    } else if window > LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW {
        let factor = (window - LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW) as f32
            / (LINK_WEIGHT_FULL_ABOVE_WINDOW - LINK_WEIGHT_NONE_AT_OR_BELOW_WINDOW) as f32;
        1.0 + (weight - 1.0) * factor
    } else {
        1.0
    }
}

/// Normalise operator weights so the lowest one is 1, keeping the differences
/// (Moblin's `updateConnectionPriorities`: `priority - lowest + 1`). Only the
/// ratio between links matters to the scheduler, and this keeps a file that
/// says `5`/`5` from behaving any differently from one that says nothing.
/// Inputs are clamped to `LINK_WEIGHT_MIN..=LINK_WEIGHT_MAX` first, so the
/// result stays in that range.
pub fn normalise_link_weights(weights: &mut [u8]) {
    for w in weights.iter_mut() {
        *w = (*w).clamp(LINK_WEIGHT_MIN, LINK_WEIGHT_MAX);
    }
    let Some(lowest) = weights.iter().copied().min() else {
        return;
    };
    for w in weights.iter_mut() {
        *w = *w - lowest + 1;
    }
}

/// One outstanding duplicate probe.
#[derive(Debug, Clone, Copy)]
pub struct ProbeRecord {
    /// Latest send of this sequence on this link. Drives the age purge.
    pub sent_ms: u64,
    /// The sequence was probed more than once on this link, so its ACK
    /// cannot say which copy it answers (Karn's ambiguity).
    pub ambiguous: bool,
}

/// A first-send NAK on this link that is not charged yet.
#[derive(Debug, Clone, Copy)]
pub struct PendingNak {
    pub sent_ms: u64,
    pub nak_ms: u64,
}

/// Source of arrival-report epochs: unique across the process, never zero.
static SRTLA_ACK_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn next_srtla_ack_epoch() -> u64 {
    SRTLA_ACK_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Read-only snapshot of a connection's metrics and telemetry at a point in time.
///
/// Returned by [`SrtlaConnection::telemetry`] so stats, status logging,
/// classifiers, quality scoring, and per-link CC all read one cohesive view
/// without reaching into internal trackers.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionTelemetry {
    pub rtt_ms: f64,
    pub rtt_min_ms: f64,
    pub rtt_velocity: f64,
    pub rtt_jitter_ms: f64,
    pub rtt_stable: bool,
    pub estimated_rtt_ms: f64,
    pub last_rtt_measurement_ms: u64,
    pub queue_building_suspected: bool,
    pub nak_count: i32,
    pub nak_burst_count: i32,
    pub time_since_last_nak_ms: Option<u64>,
    pub connection_established_ms: u64,
    pub bitrate_bps: f64,
    pub bitrate_mbps: f64,
    pub bytes_sent_total: u64,
    pub base_score: i32,
    pub batch_regime: &'static str,
    pub stall_latched: bool,
    pub stall_gate_events: u64,
    pub silence_pulls: u64,
    pub sole_carrier: bool,
    pub sole_carrier_excluded: bool,
    pub sole_carrier_elections: u64,
    pub data_packets_queued_total: u64,
    pub srtla_ack_entries_total: u64,
    pub last_srtla_ack_age_ms: Option<u64>,
    pub srtla_ack_epoch: u64,
    pub last_received_ms: Option<u64>,
}

/// Per-tick classifier and CC verdicts stamped onto a connection for Enhanced
/// selection to consume.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickSignals {
    pub weak: bool,
    /// True when the weak verdict is due to latency/queueing rather than low
    /// throughput share. A late link is held off unique payload; a share-weak
    /// link keeps a crushed-score trickle to earn its share back.
    pub weak_for_delay: bool,
    pub cc_backing_off: bool,
    pub cc_target_bps: u64,
    pub loss_degraded: bool,
}

/// A NAK left unresolved this long is charged as a loss. This covers a link
/// that goes quiet before its next SRTLA ACK could settle the NAK.
pub const PENDING_NAK_TIMEOUT_MS: u64 = 1_000;

pub struct SrtlaConnection {
    pub conn_id: u64,
    // Shell production API: the sender binds egress sockets to this IP and
    // reports it in stats, so it is unconditionally public across the crate seam.
    pub local_ip: IpAddr,
    pub label: String,
    pub connected: bool,
    pub window: i32,
    pub in_flight_packets: i32,
    /// Packet log: sequence -> send timestamp (ms), or `None` for a resend
    /// (a repeat on this link or a retransmit-flagged copy), whose ACK may
    /// belong to an earlier copy and so cannot time a round trip. In enhanced
    /// mode a NAK for a `None` entry is not a congestion signal, since it
    /// usually repeats a loss report for an earlier copy. A NAK for a first
    /// send moves the entry to [`Self::pending_naks`] instead of charging it.
    /// Uses FxHashMap for O(1) insert/remove instead of O(256) linear scan.
    #[cfg(feature = "test-internals")]
    pub packet_log: FxHashMap<i32, Option<u64>>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_log: FxHashMap<i32, Option<u64>>,
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
    /// owes anyone. A sequence probed twice on this link is kept but marked
    /// ambiguous, so its ACK still proves delivery and yields no RTT sample.
    #[cfg(feature = "test-internals")]
    pub probe_log: FxHashMap<i32, ProbeRecord>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) probe_log: FxHashMap<i32, ProbeRecord>,
    /// Enhanced mode first-send NAKs that wait for this link's SRTLA ACKs to
    /// say whether the packet was lost or only reordered across links. They
    /// leave in-flight accounting at the NAK and are charged to the congestion
    /// window only once the evidence shows a loss, or after
    /// [`PENDING_NAK_TIMEOUT_MS`].
    #[cfg(feature = "test-internals")]
    pub pending_naks: FxHashMap<i32, PendingNak>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) pending_naks: FxHashMap<i32, PendingNak>,
    /// Highest sequence number that has been cumulatively ACKed, or
    /// [`NO_ACK_YET`] before the first ACK (and after every reset).
    ///
    /// Used to optimize cumulative ACK processing by skipping already-ACKed
    /// sequences. Compare it with the serial helpers in [`crate::seq`], never
    /// with `<`/`>`: SRT sequences are 31-bit and wrap.
    #[cfg(feature = "test-internals")]
    pub highest_acked_seq: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) highest_acked_seq: i32,
    pub last_received: Option<u64>,
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
    /// Payload packets handed to this link since its last delivery proof
    /// ([`Self::last_ack_or_rtt_sample_ms`]). Reset only where that proof is
    /// stamped (and on a reset), never by NAKs or cumulative SRT ACKs: those
    /// prune the packet log but say nothing about whether *this* path
    /// delivered, which is exactly why the in-flight count cannot see a dead
    /// link at moderate rates (see
    /// [`crate::config_snapshot::STALL_MIN_UNPROVEN_SENDS`]). Probes are not
    /// counted; they are not payload this link owes.
    #[cfg(any(test, feature = "test-internals"))]
    pub unproven_sends: u32,
    #[cfg(not(any(test, feature = "test-internals")))]
    pub(crate) unproven_sends: u32,
    /// Send time of the oldest of those unproven packets; `0` when there are
    /// none. A burst only counts once its first packet has had a feedback
    /// round trip to be answered (see [`Self::unproven_sends_due`]).
    pub(crate) first_unproven_send_ms: u64,
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
    /// Whether the current silence pull engaged on a full in-flight backlog
    /// rather than on unproven sends alone. Decides how long the pull waits
    /// before escalating to the latch (see [`Self::update_stall_latch`]).
    pub(crate) silence_pull_on_backlog: bool,
    /// Cumulative silence-pull engagements (rising edges). Expected to tick
    /// on routine cellular HARQ stalls; a high rate is telemetry, not alarm.
    pub(crate) silence_pulls: u64,
    /// SRT DATA datagrams queued on this link: first sends, retransmits and
    /// duplicate probes. SRT control traffic forwarded over the link is not
    /// counted. Monotonic for the life of this connection object.
    pub(crate) data_packets_queued_total: u64,
    /// Sequence entries in SRTLA ACKs that arrived on this link while it was
    /// registered. The receiver lists a sequence in an SRTLA ACK only on the
    /// link the packet arrived on, so these are arrival reports for this
    /// path. Every entry counts, duplicates included. Monotonic for the life
    /// of this connection object.
    pub(crate) srtla_ack_entries_total: u64,
    /// When the last SRTLA ACK arrived on this link in the current epoch, `0`
    /// before the first. Keepalive replies never stamp it.
    pub(crate) last_srtla_ack_ms: u64,
    /// Changes when this connection is created or its socket is replaced.
    /// Unique across the process, so a link removed and re-added never
    /// repeats an earlier epoch.
    pub(crate) srtla_ack_epoch: u64,
    /// Per-link liveness timeout in ms. Refreshed from the config snapshot on
    /// every selection pass so `is_timed_out` (called from paths that do not
    /// carry a config) always sees the current runtime value.
    pub(crate) conn_timeout_ms: u64,
    /// One-way delivery deadline in ms — the receive buffer the SRT peer
    /// declared in its handshake. Refreshed from the config snapshot alongside
    /// `conn_timeout_ms`, for the same reason: `update_stall_latch` runs from
    /// paths that do not carry a config. Zero until the handshake crosses (see
    /// [`crate::config_snapshot::ConfigSnapshot::negotiated_latency_ms`]).
    pub(crate) delay_budget_ms: u32,
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
    /// Whether the weak verdict is due to latency/queueing rather than low
    /// throughput share. Selection needs the distinction: a *late* link must
    /// be kept off unique payload entirely, while an under-used one has to
    /// keep carrying a little real traffic to earn back its share.
    pub weak_for_delay: bool,
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
    /// Operator link weight from the IPs file (`<ip> <weight>`), normalised so
    /// the lowest link is 1. Scales this link's classic score through
    /// [`link_weight_multiplier`]. Survives recovery/reconnect resets: it is
    /// configuration, not link state; only a reload changes it.
    pub link_weight: u8,
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
            pending_naks: FxHashMap::default(),
            highest_acked_seq: NO_ACK_YET,
            last_received: None,
            last_keepalive_sent: None,
            last_ack_or_rtt_sample_ms: 0,
            unproven_sends: 0,
            first_unproven_send_ms: 0,
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
            silence_pull_on_backlog: false,
            silence_pulls: 0,
            data_packets_queued_total: 0,
            srtla_ack_entries_total: 0,
            last_srtla_ack_ms: 0,
            srtla_ack_epoch: next_srtla_ack_epoch(),
            conn_timeout_ms: crate::config_snapshot::CONN_TIMEOUT_MS,
            delay_budget_ms: 0,
            rtt: RttTracker::default(),
            congestion: CongestionControl::default(),
            bitrate: BitrateTracker::new(now),
            reconnection: ReconnectionState::new_with_grace(now),
            quality_cache: CachedQuality::default(),
            batch_sender: BatchSender::new(),
            phase: LinkPhase::Registering,
            weak: false,
            weak_for_delay: false,
            quality_excluded: false,
            cc_backing_off: false,
            cc_target_bps: 0,
            loss_degraded: false,
            link_weight: LINK_WEIGHT_MIN,
        }
    }

    /// Set this link's operator weight (clamped to
    /// `LINK_WEIGHT_MIN..=LINK_WEIGHT_MAX`). Returns the previous weight if it
    /// changed.
    pub fn set_link_weight(&mut self, weight: u8) -> Option<u8> {
        let clamped = weight.clamp(LINK_WEIGHT_MIN, LINK_WEIGHT_MAX);
        if self.link_weight == clamped {
            return None;
        }
        let prev = self.link_weight;
        self.link_weight = clamped;
        Some(prev)
    }

    /// Classic-mode score scaled by this link's operator weight (see
    /// [`link_weight_multiplier`]).
    #[inline(always)]
    pub(crate) fn classic_weighted_score(&self) -> i32 {
        let score = self.get_score();
        if self.link_weight <= 1 || score <= 0 {
            return score;
        }
        (score as f32 * self.weight_multiplier()) as i32
    }

    /// Enhanced-mode base score: capacity score weighted by lifecycle phase
    /// and any active post-rejoin share ramp.
    #[inline(always)]
    pub(crate) fn effective_base_score(&self, now_ms: u64) -> f64 {
        self.get_score() as f64 * self.phase.weight() * self.rejoin_ramp_multiplier(now_ms)
    }

    /// Apply per-tick classifier and CC verdicts from housekeeping.
    pub fn apply_tick_signals(&mut self, signals: TickSignals) {
        self.weak = signals.weak;
        self.weak_for_delay = signals.weak_for_delay;
        self.cc_backing_off = signals.cc_backing_off;
        self.cc_target_bps = signals.cc_target_bps;
        self.loss_degraded = signals.loss_degraded;
    }

    /// Read-only snapshot of this connection's telemetry and metrics at `now_ms`.
    pub fn telemetry(&self, now_ms: u64) -> ConnectionTelemetry {
        ConnectionTelemetry {
            rtt_ms: self.rtt.smooth_rtt_ms(),
            rtt_min_ms: self.rtt.rtt_min_ms,
            rtt_velocity: self.rtt.velocity(),
            rtt_jitter_ms: self.rtt.rtt_jitter_ms,
            rtt_stable: self.rtt.is_stable(),
            estimated_rtt_ms: self.rtt.estimated_rtt_ms,
            last_rtt_measurement_ms: self.rtt.last_rtt_measurement_ms,
            queue_building_suspected: self.rtt.queue_building_suspected(),
            nak_count: self.congestion.nak_count,
            nak_burst_count: self.congestion.nak_burst_count,
            time_since_last_nak_ms: self.congestion.time_since_last_nak_ms(now_ms),
            connection_established_ms: self.reconnection.established_ms(),
            bitrate_bps: self.bitrate.current_bitrate_bps,
            bitrate_mbps: self.bitrate.mbps(),
            bytes_sent_total: self.bitrate.bytes_sent_total,
            base_score: self.get_score(),
            batch_regime: self.batch_sender.regime().as_str(),
            stall_latched: self.stall_latched(),
            stall_gate_events: self.stall_gate_events,
            silence_pulls: self.silence_pulls,
            sole_carrier: self.sole_carrier,
            sole_carrier_excluded: self.sole_carrier_excluded,
            sole_carrier_elections: self.sole_carrier_elections,
            data_packets_queued_total: self.data_packets_queued_total,
            srtla_ack_entries_total: self.srtla_ack_entries_total,
            last_srtla_ack_age_ms: self.last_srtla_ack_age_ms(now_ms),
            srtla_ack_epoch: self.srtla_ack_epoch,
            last_received_ms: self.last_received,
        }
    }

    /// Classic-mode score multiplier from this link's operator weight at its
    /// current window (see [`link_weight_multiplier`]).
    #[inline(always)]
    pub fn weight_multiplier(&self) -> f32 {
        link_weight_multiplier(self.window, self.link_weight)
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
        if seq.is_some() {
            self.data_packets_queued_total = self.data_packets_queued_total.saturating_add(1);
        }
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
        self.data_packets_queued_total = self.data_packets_queued_total.saturating_add(1);
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
            self.probe_log.retain(|_, p| p.sent_ms >= cutoff);
            // Still full of live entries: this link is being probed faster than
            // it can answer, so the oldest are the least useful. Start over
            // rather than grow without bound.
            if self.probe_log.len() >= PROBE_LOG_SOFT_CAP {
                self.probe_log.clear();
            }
        }
        self.probe_log
            .entry(seq)
            .and_modify(|p| {
                p.sent_ms = send_time_ms;
                p.ambiguous = true;
            })
            .or_insert(ProbeRecord {
                sent_ms: send_time_ms,
                ambiguous: false,
            });
    }

    /// Queue a duplicate probe on this link if it is currently held out of the
    /// payload rotation and its 1-in-N probe cadence is due.
    ///
    /// Returns `Some(needs_flush)` when a probe was queued, or `None` when
    /// skipped. Marks the copy with the SRT retransmit flag (`R` bit) so an
    /// SRTLA-patched receiver excludes it from its reorder-hold estimator.
    pub fn maybe_queue_probe_packet(
        &mut self,
        pkt: &[u8],
        seq: u32,
        send_time_ms: u64,
    ) -> Option<bool> {
        if !(self.stall_gated || self.quality_excluded) || !self.connected {
            return None;
        }
        if !self.stall_probe_due() {
            return None;
        }
        let mut probe: SmallVec<u8, 1500> = SmallVec::from_slice_copy(pkt);
        SrtData::set_retransmit(&mut probe);
        Some(self.queue_probe_packet(&probe, seq, send_time_ms))
    }

    /// Whether this connection has queued packets or needs a time-based batch flush.
    #[inline]
    pub fn has_batch_work(&self, now_ms: u64) -> bool {
        self.batch_sender.has_queued_packets() || self.batch_sender.needs_time_flush(now_ms)
    }

    /// Whether the event loop should keep the 15 ms batch/NAK flush timer armed.
    #[inline]
    pub fn needs_flush_timer(&self) -> bool {
        self.batch_sender.has_queued_packets() || !self.pending_naks.is_empty()
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

    /// Check if there are any pending NAKs awaiting timeout or SRTLA ACK settlement.
    #[inline]
    pub fn has_pending_naks(&self) -> bool {
        !self.pending_naks.is_empty()
    }

    /// Drain the batch queue for transmission.
    ///
    /// Pure builder: registers each tracked packet as in-flight and returns the
    /// queued datagrams. The shell sends them (see `net::send_all_datagrams`)
    /// and must, on *any* send error, put the link into recovery — the shell
    /// funnels every flush path through one helper so none can skip it (see
    /// `sender::packet_handler::flush_connection`).
    ///
    /// In-flight registration is deliberately optimistic and covers the whole
    /// batch, including a suffix a partial send never put on the wire:
    /// [`Self::mark_for_recovery`] clears `packet_log` and zeroes
    /// `in_flight_packets` wholesale, so registering the confirmed prefix only
    /// would buy nothing and would cost a second pass over the batch on the hot
    /// path. What makes the accounting truthful is the guarantee that a failed
    /// flush always recovers.
    ///
    /// Does **not** stamp `last_sent`: that records when bytes actually reached
    /// the socket, so the shell stamps it with [`Self::note_sent`] once the I/O
    /// is confirmed. Empty when nothing was queued.
    pub fn take_batch(&mut self, now: u64) -> DrainedBatch {
        let batch = self.batch_sender.drain(now);
        if batch.is_empty() {
            return batch;
        }
        for (data, seq, send_time_ms) in batch.iter() {
            if let Some(s) = seq {
                let seq = s as i32;
                self.register_packet(seq, send_time_ms);
                if self.unproven_sends == 0 {
                    self.first_unproven_send_ms = send_time_ms;
                }
                self.unproven_sends = self.unproven_sends.saturating_add(1);
                // A NAK usually clears the original from the log before SRT
                // resends it, so the repeat check in `register_packet` cannot
                // see this copy is a resend. The retransmit flag can.
                if SrtData::is_retransmit(data) {
                    self.packet_log.insert(seq, None);
                }
            }
        }
        batch
    }

    /// Return a drained batch's backing buffers to this connection's `BatchSender`
    /// so subsequent flushes reuse the allocation without hitting the allocator.
    #[inline]
    pub fn recycle_batch(&mut self, batch: DrainedBatch) {
        self.batch_sender.recycle(batch);
    }

    /// Build an extended keepalive packet and record that it was sent.
    ///
    /// Pure builder: the shell transmits the returned bytes on this link's
    /// socket. State (`last_keepalive_sent`, RTT-probe arming) is updated
    /// optimistically against the injected clock — a dropped keepalive is just
    /// a lost UDP datagram the next tick re-sends, so there is nothing to roll
    /// back on a failed send.
    pub fn keepalive_packet(&mut self, now: u64) -> [u8; SRTLA_KEEPALIVE_EXT_LEN] {
        // Create extended keepalive with connection info (telemetry for receiver)
        let info = ConnectionInfo {
            conn_id: self.conn_id as u32,
            window: self.window,
            in_flight: self.in_flight_packets,
            rtt_ms: self.rtt.raw_kalman_rtt_ms() as u32,
            nak_count: self.congestion.nak_count as u32,
            bitrate_bytes_per_sec: (self.bitrate.current_bitrate_bps / 8.0) as u32,
        };
        let pkt = Keepalive::encode_ext(info, now);
        self.last_keepalive_sent = Some(now);
        // Only set waiting flag and timestamp when we intend to measure RTT
        self.rtt.maybe_record_keepalive_sent(now);
        pkt
    }

    /// Run one housekeeping tick for a non-timed-out connection: build a
    /// keepalive if periodic keepalive or RTT probing is due, recover the
    /// congestion window (enhanced mode), recalculate bitrate, advance the
    /// link lifecycle phase, and adapt the batch-send regime.
    pub fn housekeeping_tick(
        &mut self,
        classic: bool,
        now_ms: u64,
    ) -> Option<[u8; SRTLA_KEEPALIVE_EXT_LEN]> {
        let keepalive = if self.needs_keepalive(now_ms) || self.needs_rtt_measurement(now_ms) {
            Some(self.keepalive_packet(now_ms))
        } else {
            None
        };
        if !classic {
            self.perform_window_recovery(now_ms);
        }
        self.calculate_bitrate(now_ms);
        self.update_phase(now_ms);
        self.recompute_batch_regime();
        keepalive
    }

    /// Decide whether a timed-out connection should attempt a reconnect on
    /// this tick and, if so, record the attempt. Returns `true` when the
    /// caller should rebuild the socket.
    pub fn try_begin_reconnect(&mut self, now_ms: u64) -> bool {
        if !self.reconnection.should_attempt_reconnect(now_ms) {
            return false;
        }
        self.reconnection.record_attempt(&self.label, now_ms);
        true
    }

    /// Reset the initial-registration startup grace window from `now_ms`.
    pub fn reset_startup_grace(&mut self, now_ms: u64) {
        self.reconnection.reset_startup_grace(now_ms);
    }

    /// Transition this link into the registered state after REG3 arrives.
    pub fn on_registered(&mut self, now_ms: u64) {
        self.clear_pre_registration_state(now_ms);
        self.connected = true;
        self.last_received = Some(now_ms);
        self.reconnection.mark_established(&self.label, now_ms);
    }

    /// Mark this link unregistered after REG_ERR arrives.
    pub fn on_registration_error(&mut self) {
        self.connected = false;
        self.last_received = None;
    }

    /// Record inbound liveness from any non-registration packet on this link.
    ///
    /// Liveness only counts for a link that is registered, or has never been
    /// (initial registration keeps its own grace/timeout handling). A link
    /// that WAS established and is in recovery (`connected == false` until
    /// REG3) stays timed out so housekeeping rebuilds its socket and re-sends
    /// REG2 instead of parking it in limbo.
    pub fn note_packet_received(&mut self, now_ms: u64) {
        if self.connected || !self.reconnection.is_established() {
            self.last_received = Some(now_ms);
        }
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
        let pkt = RegPacket::reg2(probe_id);
        self.reconnection.reset_startup_grace(now);
        pkt
    }

    pub fn is_rtt_stable(&self) -> bool {
        self.rtt.is_stable()
    }

    pub fn get_smooth_rtt_ms(&self) -> f64 {
        self.rtt.smooth_rtt_ms()
    }

    /// RTT velocity (trend) in ms/sample from the Kalman filter.
    /// Positive = rising RTT (congestion building), negative = falling.
    pub fn get_rtt_velocity(&self) -> f64 {
        self.rtt.velocity()
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
        self.rtt
            .needs_measurement(self.connected, self.reconnection.established_ms(), now_ms)
    }

    pub fn needs_keepalive(&self, now_ms: u64) -> bool {
        // Send keepalive every IDLE_TIME (1s) unconditionally on all connections.
        // Moblin does this with standard 10-byte keepalives; we use extended 38-byte
        // keepalives to provide the receiver with telemetry (window, RTT, NAKs, bitrate).
        if !self.connected {
            return false;
        }

        // A keepalive that slips a tick leaves a 2 s gap in the echoes, which
        // is also the delivery proof a stall-gated link rejoins on.
        match self.last_keepalive_sent {
            None => true,
            Some(last) => {
                now_ms.saturating_sub(last) + reconnection::TICK_SLACK_MS >= IDLE_TIME * 1000
            }
        }
    }

    pub fn perform_window_recovery(&mut self, now_ms: u64) {
        let velocity = self.rtt.velocity();
        self.congestion.perform_window_recovery(
            &mut self.window,
            self.connected,
            velocity,
            &self.label,
            now_ms,
        );
    }

    /// Fold an echoed keepalive into RTT and stall-gate state.
    ///
    /// The echo is an RTT sample only when the tracker armed a measurement,
    /// but it is delivery proof for `stall_deselect` every time. Arming waits
    /// for 3 s without any RTT sample, which a stall-latched link fed only by
    /// sparse probe ACKs rarely reaches; tying proof to it would leave the
    /// latch's rejoin dwell, which needs fresh proof throughout, unable to
    /// complete. Pairs with the SRTLA ACK sites (the per-sequence match in
    /// `ack_nak.rs` and `note_srtla_ack_proof`); together they let a recovered
    /// link un-gate itself without the scheduler probing blindly.
    pub fn handle_keepalive_echo(&mut self, data: &[u8], now_ms: u64) {
        if self
            .rtt
            .handle_keepalive_response(data, &self.label, now_ms)
            .is_some()
        {
            self.record_rtt_probe();
        }
        if rtt::keepalive_echo_proves_delivery(data, now_ms) {
            self.note_delivery_proof(now_ms);
        }
    }

    /// Stamp delivery proof: an SRTLA ACK that matched a packet this link
    /// sent (including a probe), any SRTLA ACK arriving on a registered link,
    /// or a keepalive echo. The only place the unproven-send count resets.
    #[inline]
    pub(crate) fn note_delivery_proof(&mut self, now_ms: u64) {
        self.last_ack_or_rtt_sample_ms = now_ms;
        self.unproven_sends = 0;
        self.first_unproven_send_ms = 0;
    }

    /// Delivery proof from an SRTLA ACK packet that arrived on this link.
    ///
    /// `srtla_rec` sends one on a link only after packets arrived on that
    /// link, so it proves the path delivers even when none of its sequences
    /// are still in this link's packet log. That is the normal case on a link
    /// whose return path is slower than a sibling's: the SRT ACK `srtla_rec`
    /// sends on every link prunes the packets over the faster path first, and
    /// NAKs can clear and re-route them before the slower link's own ACK
    /// arrives. No RTT sample is taken here.
    ///
    /// The shell calls this only for a registered link and only for datagrams
    /// its current socket received. A delayed duplicate of an old ACK on that
    /// socket still counts, stamped with the time it arrived: the signal is
    /// recent per-link feedback from the receiver, not a per-packet delivery
    /// receipt, and its age can understate how long ago the path last
    /// delivered by up to that delay.
    #[inline]
    pub fn note_srtla_ack_proof(&mut self, now_ms: u64) {
        self.note_delivery_proof(now_ms);
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

    /// Count the sequence entries of one SRTLA ACK that arrived on this link
    /// and stamp delivery proof for this path. Ignored before registration
    /// completes or when `entries == 0`.
    #[inline]
    pub fn note_srtla_ack_arrival(&mut self, entries: usize, now_ms: u64) {
        if !self.connected || entries == 0 {
            return;
        }
        self.srtla_ack_entries_total = self.srtla_ack_entries_total.saturating_add(entries as u64);
        self.last_srtla_ack_ms = now_ms;
        self.note_delivery_proof(now_ms);
    }

    /// SRT DATA datagrams queued on this link over this connection's life.
    #[inline(always)]
    pub fn data_packets_queued_total(&self) -> u64 {
        self.data_packets_queued_total
    }

    /// SRTLA ACK sequence entries that arrived on this link while registered.
    #[inline(always)]
    pub fn srtla_ack_entries_total(&self) -> u64 {
        self.srtla_ack_entries_total
    }

    /// Milliseconds since an SRTLA ACK last arrived on this link in the
    /// current epoch, `None` before the first.
    #[inline]
    pub fn last_srtla_ack_age_ms(&self, now_ms: u64) -> Option<u64> {
        (self.last_srtla_ack_ms != 0).then(|| now_ms.saturating_sub(self.last_srtla_ack_ms))
    }

    /// The current arrival-report epoch (see [`Self::note_srtla_ack_arrival`]).
    #[inline(always)]
    pub fn srtla_ack_epoch(&self) -> u64 {
        self.srtla_ack_epoch
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
            if self.reconnection.in_startup_grace(now) {
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
        self.pending_naks.clear();
        self.in_flight_packets = 0;
        // Packets forwarded before REG3 were sent before the receiver knew the
        // link; it owes no proof for them.
        self.unproven_sends = 0;
        self.first_unproven_send_ms = 0;
        self.highest_acked_seq = NO_ACK_YET;
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
        self.pending_naks.clear();
        self.highest_acked_seq = NO_ACK_YET;
        self.batch_sender.reset();
        self.phase = LinkPhase::Registering;
        // A reset link has no delivery proof; clear the stall signal so it is
        // not classed as stalled the instant it reconnects with a backlog.
        // The `stall_gate_events` counter deliberately survives: it counts
        // engagements over the link's life, and a reset is often the *result*
        // of the stall it just latched on.
        self.last_ack_or_rtt_sample_ms = 0;
        self.unproven_sends = 0;
        self.first_unproven_send_ms = 0;
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
        self.silence_pull_on_backlog = false;
    }

    /// Mark connection for recovery (C-style), similar to setting last_rcvd = 1.
    /// Soft reset: clears packet state but preserves congestion/bitrate stats.
    pub fn mark_for_recovery(&mut self) {
        self.last_received = None;
        self.last_keepalive_sent = None;
        self.rtt.clear_pending_keepalive();
        self.reset_core_state();
        // Set grace deadline to 0 to indicate this connection is immediately timed out
        // (matches C behavior of setting last_rcvd = 1)
        self.reconnection.expire_startup_grace();
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
        self.reconnection.established_ms()
    }

    /// Get the cached quality multiplier, recomputing it via `compute` when the
    /// 50ms cache window has elapsed.
    ///
    /// The formula lives in `crate::selection::quality` so `connection` stays
    /// strictly below `selection` with no circular dependency.
    #[inline(always)]
    pub(crate) fn cached_quality_multiplier(
        &mut self,
        current_time_ms: u64,
        compute: impl FnOnce(&SrtlaConnection, u64) -> f64,
    ) -> f64 {
        if current_time_ms.saturating_sub(self.quality_cache.last_calculated_ms)
            >= QUALITY_CACHE_INTERVAL_MS
        {
            self.quality_cache.multiplier = compute(self, current_time_ms);
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
    /// bitrate. Called from housekeeping each tick.
    pub fn recompute_batch_regime(&mut self) {
        self.batch_sender
            .update_regime_for_bps(self.bitrate.current_bitrate_bps);
    }

    /// Reset connection state after the shell replaced this link's socket.
    /// Full reset: clears all state including congestion/bitrate stats. `now`
    /// is the injected clock. Socket creation lives in the shell's
    /// `rebuild_uplink_socket`.
    pub fn reset_for_reconnect(&mut self, now: u64) {
        self.last_received = None;
        self.reset_core_state();
        // A new socket starts a new arrival-report epoch: ACK age restarts, and
        // consumers rebaseline their deltas when the epoch changes.
        self.srtla_ack_epoch = next_srtla_ack_epoch();
        self.last_srtla_ack_ms = 0;

        // Reset submodule state
        self.congestion.reset();
        self.rtt.reset();
        self.bitrate.reset(now);

        // The attempt count survives: only REG3 proves the link is back.
        self.reconnection.on_socket_rebuilt(now);
    }
}
