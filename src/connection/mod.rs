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
use tracing::debug;

use crate::protocol::*;

pub(crate) const STARTUP_GRACE_MS: u64 = 5_000;

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

pub struct SrtlaConnection {
    pub(crate) conn_id: u64,
    #[allow(dead_code)]
    #[cfg(feature = "test-internals")]
    pub local_ip: IpAddr,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) local_ip: IpAddr,
    pub label: String,
    #[cfg(feature = "test-internals")]
    pub connected: bool,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) connected: bool,
    #[cfg(feature = "test-internals")]
    pub window: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) window: i32,
    #[cfg(feature = "test-internals")]
    pub in_flight_packets: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) in_flight_packets: i32,
    /// Packet log: maps sequence number -> send timestamp (ms).
    /// Uses FxHashMap for O(1) insert/remove instead of O(256) linear scan.
    #[cfg(feature = "test-internals")]
    pub packet_log: FxHashMap<i32, u64>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_log: FxHashMap<i32, u64>,
    /// Highest sequence number that has been cumulatively ACKed.
    /// Used to optimize cumulative ACK processing by skipping already-ACKed sequences.
    #[cfg(feature = "test-internals")]
    pub highest_acked_seq: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) highest_acked_seq: i32,
    #[cfg(feature = "test-internals")]
    pub last_received: Option<u64>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_received: Option<u64>,
    #[cfg(feature = "test-internals")]
    pub last_sent: Option<u64>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_sent: Option<u64>,
    /// Timestamp of the last keepalive sent (for periodic telemetry)
    pub(crate) last_keepalive_sent: Option<u64>,
    /// `now_ms()` of this link's last delivery proof: an EARNED ACK (this link
    /// owned an acked seq) or a keepalive-RTT response. Stamped ONLY at those
    /// two sites — NEVER on generic inbound bytes (unlike `last_received`), so a
    /// link that merely echoes traffic while its ACK/RTT path is dead still goes
    /// stale. `0` = no proof yet. Read only by the `stall_deselect` selection
    /// guard; never a liveness/timeout signal.
    pub(crate) last_ack_or_rtt_sample_ms: u64,
    /// Transient per-select flag: set by `select_connection_idx` when
    /// `stall_deselect` is on and this link is a stalled black hole while a
    /// healthier link exists. Recomputed every select call and read only by the
    /// mode selectors in that same call; it is a selection penalty ONLY and
    /// never affects `is_timed_out`/re-registration.
    pub(crate) stall_gated: bool,
    // Sub-structs for organized state management
    #[cfg(feature = "test-internals")]
    pub rtt: RttTracker,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) rtt: RttTracker,
    #[cfg(feature = "test-internals")]
    pub congestion: CongestionControl,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) congestion: CongestionControl,
    #[cfg(feature = "test-internals")]
    pub bitrate: BitrateTracker,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) bitrate: BitrateTracker,
    #[cfg(feature = "test-internals")]
    pub reconnection: ReconnectionState,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) reconnection: ReconnectionState,
    /// Cached quality multiplier for performance optimization.
    /// Recalculated every 50ms instead of on every packet.
    pub(crate) quality_cache: CachedQuality,
    /// Batch sender for optimized packet transmission.
    /// Buffers up to 16 packets before flushing, reducing syscall overhead.
    pub(crate) batch_sender: BatchSender,
    /// Link lifecycle phase — determines scheduler eligibility.
    #[cfg(feature = "test-internals")]
    pub phase: LinkPhase,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) phase: LinkPhase,
    /// Latest weak-link classifier verdict. Updated each housekeeping
    /// tick from `WeakLinkFilter::classify`. Consumed by Enhanced
    /// selection as an admission gate.
    pub(crate) weak: bool,
    /// Latest CC state from `LinkCcController::tick_all`. Drives the CC
    /// controller's own per-window bitrate backoff. It is intentionally
    /// *not* a routing-admission gate: `BackingOff` flips on a single
    /// loss window and would make selection twitchy, so the routing gate
    /// uses the sustained `loss_degraded` latch instead.
    pub(crate) cc_backing_off: bool,
    /// Latest `target_bps` from `LinkCcController::tick_all`. Consumed
    /// by Enhanced selection as a soft cap: when the link's measured
    /// throughput approaches this value the link's score is scaled
    /// down so the scheduler routes less traffic through it before
    /// loss actually fires. `0` means "no signal" — selection skips
    /// the cap.
    pub(crate) cc_target_bps: u64,
    /// Latched verdict from `LinkCongestionState`: the link's
    /// time-decayed loss EWMA has been sustained high (see
    /// `LOSS_DEGRADE_*`, ~4s sustain with hysteresis). Drives a graded
    /// demotion to `Degraded` in the phase machine *and* the Enhanced
    /// selection loss-admission gate. It never removes the link from
    /// scheduling (a genuinely dead link is handled by
    /// `is_timed_out`/`CONN_TIMEOUT`); a gated link keeps a trickle of
    /// traffic so the loss EWMA can recover and clear the latch.
    pub(crate) loss_degraded: bool,
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
            highest_acked_seq: i32::MIN,
            last_received: None,
            last_sent: None,
            last_keepalive_sent: None,
            last_ack_or_rtt_sample_ms: 0,
            stall_gated: false,
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

    /// `stall_deselect` signal (pure read; never mutates). True for a connected
    /// link whose in-flight backlog is at or above `min_in_flight` AND whose
    /// last delivery proof (earned-ACK or keepalive-RTT sample) is older than
    /// `stale_ms`. `now_ms` is the selection clock.
    ///
    /// A link that has produced no proof yet (`last_ack_or_rtt_sample_ms == 0`)
    /// is never stalled: a fresh burst before its first ACK must not be
    /// mistaken for a black hole. A genuinely dead-from-birth link is pruned by
    /// `is_timed_out`/`CONN_TIMEOUT`, not here. This is a selection penalty
    /// input ONLY — it never affects `is_timed_out`/re-registration/CONN_TIMEOUT.
    #[inline]
    pub(crate) fn is_stalled(&self, now_ms: u64, min_in_flight: i32, stale_ms: u64) -> bool {
        self.connected
            && self.in_flight_packets >= min_in_flight
            && self.last_ack_or_rtt_sample_ms != 0
            && now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms) >= stale_ms
    }

    /// Whether this link has gone silent past `CONN_TIMEOUT`.
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
                .is_none_or(|lr| now.saturating_sub(lr) >= CONN_TIMEOUT * 1000);
        }

        // For established connections, check normal timeout
        if let Some(lr) = self.last_received {
            now.saturating_sub(lr) >= CONN_TIMEOUT * 1000
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
    pub(crate) fn clear_pre_registration_state(&mut self, now_ms: u64) {
        if !self.packet_log.is_empty() || self.congestion.nak_count > 0 {
            debug!(
                "{}: clearing pre-registration state ({} in-flight, {} NAKs)",
                self.label,
                self.packet_log.len(),
                self.congestion.nak_count
            );
        }
        self.packet_log.clear();
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
        self.highest_acked_seq = i32::MIN;
        self.batch_sender.reset();
        self.phase = LinkPhase::Registering;
        // A reset link has no delivery proof; clear the stall signal so it is
        // not classed as stalled the instant it reconnects with a backlog.
        self.last_ack_or_rtt_sample_ms = 0;
        self.stall_gated = false;
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
