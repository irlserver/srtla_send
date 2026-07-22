//! Pure, hot-path configuration snapshot (core).
//!
//! Split out of `config` so the scheduler core can consume a config view without
//! the shell coupling that `DynamicConfig` carries (the control dispatcher and
//! `SharedStats`). The live `DynamicConfig::snapshot()` builds one of these once
//! per select iteration; this type depends only on `SchedulingMode`.

use crate::mode::SchedulingMode;

/// In-flight packet backlog at or above which a link is a stall candidate
/// under the `stall_deselect` guard (default on).
pub const STALL_MIN_IN_FLIGHT_PACKETS: i32 = 32;

/// Ceiling (ms) on the delivery-proof staleness window under
/// `stall_deselect`. The effective window is RTT-adaptive —
/// `clamp(STALL_STALE_RTT_MULT x smoothed RTT, STALL_STALE_FLOOR_MS, this)` —
/// so a fast link is pulled within hundreds of ms of going dark instead of
/// always waiting the full ceiling; the reaction window is exactly the run of
/// packets that will arrive late (reorder holes) at the receiver. A link with
/// no RTT baseline yet falls back to the ceiling. Kept well below
/// `CONN_TIMEOUT` (15 s): deselect is a selection penalty ONLY, never a
/// liveness/timeout shortcut.
pub const STALL_ACK_STALE_MS: u64 = 3000;

/// Multiplier on smoothed RTT for the adaptive staleness window. Delivery
/// proof on a loaded link normally arrives every RTT (earned SRTLA ACKs), so
/// four missed round-trips is a strong stall signal without tripping on a
/// single lost ACK.
pub const STALL_STALE_RTT_MULT: u64 = 4;

/// Floor (ms) on the adaptive staleness window. Sits above the 400-800 ms
/// HARQ stalls that are routine on bonded cellular, so a normal
/// retransmission pause never gates a link.
pub const STALL_STALE_FLOOR_MS: u64 = 1000;

/// Rejoin dwell as a multiple of the effective staleness window. A gated link
/// must hold continuous fresh delivery proof this long before it rejoins the
/// rotation: quick to drop, conservative to rejoin, so a still-marginal link
/// cannot flap back in and re-glitch the stream.
pub const STALL_REJOIN_DWELL_MULT: u64 = 2;

/// Duplicate-probe rate on a stall-gated link: one copy of every Nth routed
/// data packet is also sent on each gated link. The copies are redundant (the
/// SRT receiver dedups by sequence number), so a late or lost probe cannot
/// stall the receiver buffer, but a delivered one earns the link an SRTLA ACK
/// — real data-sized delivery proof, where keepalives alone only prove the
/// path echoes 38-byte control frames. ~1% overhead at the default.
pub const STALL_PROBE_ONE_IN_N: u32 = 100;

/// Floor (ms) for the fast silence-pull window. A *loaded* link receives
/// SRTLA ACKs continuously, so total inbound silence this long with a full
/// backlog is abnormal on any sane path; an idle link is never pulled (its
/// only inbound is the 1 s keepalive echo, so gaps are normal there). The
/// pull is transient — it clears on the very next inbound byte — which is
/// why it may react an order of magnitude faster than the sticky latch.
pub const SILENCE_PULL_FLOOR_MS: u64 = 250;

/// Multiplier on smoothed RTT for the silence-pull window, so a satellite
/// path is not pulled for a silence that is shorter than its own round trip.
/// Capped at the effective staleness window — beyond that the latch owns
/// the decision.
pub const SILENCE_PULL_RTT_MULT: u64 = 2;

/// Default per-link liveness timeout (ms): silence past this tears the link
/// down for re-registration. Matches the classic `CONN_TIMEOUT` behavior;
/// runtime-tunable so an operator (or belacoder, which knows the SRT latency
/// budget) can scale it to `max(default, 2 x latency)` — tearing down a link
/// the receiver buffer could have ridden out trades a warm resume for a cold
/// re-handshake.
pub const CONN_TIMEOUT_MS: u64 = srtla_protocol::CONN_TIMEOUT * 1000;

/// Clamp bounds for the runtime-configurable liveness timeout. The floor
/// keeps a misconfigured client from turning every RTT spike into a
/// teardown; the ceiling keeps a dead link from squatting its slot.
pub const CONN_TIMEOUT_MS_MIN: u64 = 1_000;
pub const CONN_TIMEOUT_MS_MAX: u64 = 60_000;

/// Snapshot of configuration for efficient hot-path access.
/// Call `DynamicConfig::snapshot()` once per select iteration to avoid
/// multiple atomic loads per packet in the hot path.
#[derive(Clone, Copy, Debug)]
pub struct ConfigSnapshot {
    pub mode: SchedulingMode,
    pub quality_enabled: bool,
    /// Stalled-link deselect (default ON). On, the selection layer excludes a
    /// link whose in-flight backlog is high while its last delivery proof has
    /// gone stale, provided at least one healthier link can carry the traffic.
    /// Off (`--no-stall-deselect`), selection is byte-for-byte unchanged.
    pub stall_deselect: bool,
    /// In-flight threshold for `stall_deselect` (default [`STALL_MIN_IN_FLIGHT_PACKETS`]).
    pub stall_min_in_flight: i32,
    /// Ceiling in ms on the RTT-adaptive delivery-proof staleness window for
    /// `stall_deselect` (default [`STALL_ACK_STALE_MS`]; see
    /// [`SrtlaConnection::effective_stall_stale_ms`](crate::connection::SrtlaConnection::effective_stall_stale_ms)).
    pub stall_ack_stale_ms: u64,
    /// Per-link liveness timeout in ms (default [`CONN_TIMEOUT_MS`]). Silence
    /// past this tears the link down and re-registers it.
    pub conn_timeout_ms: u64,
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            stall_deselect: true,
            stall_min_in_flight: STALL_MIN_IN_FLIGHT_PACKETS,
            stall_ack_stale_ms: STALL_ACK_STALE_MS,
            conn_timeout_ms: CONN_TIMEOUT_MS,
        }
    }
}

impl ConfigSnapshot {
    /// Check if quality scoring is effective for the current mode.
    /// Quality scoring only applies to enhanced mode.
    #[inline]
    pub fn effective_quality_enabled(&self) -> bool {
        self.quality_enabled && !self.mode.is_classic()
    }
}
