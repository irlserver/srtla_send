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

/// Staleness window (ms) for a link's last delivery proof (earned-ACK or
/// keepalive-RTT sample) under `stall_deselect`. A stall candidate whose last
/// proof is older than this is treated as stalled. Kept well below
/// `CONN_TIMEOUT` (15 s): deselect is a selection penalty ONLY, never a
/// liveness/timeout shortcut.
pub const STALL_ACK_STALE_MS: u64 = 3000;

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
    /// Delivery-proof staleness window in ms for `stall_deselect`
    /// (default [`STALL_ACK_STALE_MS`]).
    pub stall_ack_stale_ms: u64,
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            stall_deselect: true,
            stall_min_in_flight: STALL_MIN_IN_FLIGHT_PACKETS,
            stall_ack_stale_ms: STALL_ACK_STALE_MS,
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
