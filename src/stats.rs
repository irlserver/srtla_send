//! Shared statistics for external telemetry consumers.
//!
//! Exports per-link metrics via the Unix socket `stats` command for external
//! consumers like gst-app's bitrate controller.
//!
//! ## Design Principles
//!
//! 1. **Raw metrics only**: Export values directly from `SrtlaConnection` without
//!    transformation or speculation. Let consumers decide how to interpret them.
//!
//! 2. **Match established formats**: Per-link metrics align with `ConnectionInfo`
//!    (the extended keepalive format sent to srtla_rec).
//!
//! 3. **Include quality_multiplier**: This is the exact value used by enhanced/
//!    rtt-threshold selection algorithms, useful for understanding sender behavior.
//!
//! 4. **Simple aggregates**: Only sums and counts, no derived calculations like
//!    "capacity estimation" that would require assumptions about packet sizes.

use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::config::ConfigSnapshot;
use crate::connection::SrtlaConnection;
use crate::sender::calculate_quality_multiplier;
use crate::utils::now_ms;

/// Per-link statistics.
///
/// Fields match `ConnectionInfo` (extended keepalive format) where applicable,
/// plus additional context useful for external monitoring.
#[derive(Clone, Debug, Serialize)]
pub struct LinkStats {
    /// Local IP address used for this link
    pub ip: IpAddr,
    /// Human-readable label (e.g., "host:port via ip")
    pub label: String,
    /// True if SRTLA registration completed (REG3 received)
    pub connected: bool,
    /// True if no packets received within timeout period
    pub timed_out: bool,

    // --- Core metrics (same as ConnectionInfo in extended keepalives) ---
    /// Congestion window size (packet count). Higher = more capacity.
    /// This is the primary capacity signal used by all selection modes.
    pub window: i32,
    /// Packets sent but not yet ACKed. Higher = more load on this link.
    pub in_flight: i32,
    /// Smoothed RTT in milliseconds. From EWMA of ACK round-trips.
    pub rtt_ms: u32,
    /// Total NAK count since connection established. Indicates packet loss.
    pub nak_count: i32,
    /// Current send bitrate in bytes/sec (measured, not estimated).
    pub bitrate_bps: u32,

    // --- RTT baseline tracking ---
    /// Dual-window minimum RTT baseline in milliseconds.
    pub rtt_min_ms: f64,
    /// Fast RTT tracker in milliseconds (quicker response to spikes).
    pub fast_rtt_ms: f64,

    // --- Selection algorithm context ---
    /// Base score: window / (in_flight + 1). Used by classic mode.
    /// Higher score = more available capacity on this link.
    pub base_score: i32,
    /// Quality multiplier (0.35 to 1.1) used by enhanced/rtt-threshold modes.
    /// - 1.1 = perfect (no NAKs ever)
    /// - 1.0 = normal
    /// - <1.0 = degraded due to recent NAKs
    /// - ~0.35 = heavily penalized (NAK burst)
    ///
    /// This is the EXACT multiplier used in `select_connection_idx()`.
    /// In classic mode, this is always 1.0 (quality scoring disabled).
    pub quality_multiplier: f64,
}

/// Aggregate statistics snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    /// Current scheduling mode: "classic", "enhanced", or "rtt-threshold"
    pub mode: String,
    /// Whether quality scoring is enabled (always false for classic mode)
    pub quality_enabled: bool,
    /// RTT delta threshold in ms (only relevant for rtt-threshold mode)
    pub rtt_delta_ms: u32,

    /// Number of links that are connected AND not timed out
    pub active_links: usize,
    /// Total configured links
    pub total_links: usize,

    /// Sum of window across active links
    pub total_window: i32,
    /// Sum of in_flight across active links
    pub total_in_flight: i32,

    /// Per-link details
    pub links: Vec<LinkStats>,
}

impl Default for StatsSnapshot {
    fn default() -> Self {
        Self {
            mode: "enhanced".to_string(),
            quality_enabled: true,
            rtt_delta_ms: 30,
            active_links: 0,
            total_links: 0,
            total_window: 0,
            total_in_flight: 0,
            links: Vec::new(),
        }
    }
}

/// Thread-safe container for stats export.
///
/// Updated by sender during housekeeping (~1s interval).
/// Read by config handler when `stats` command is received.
#[derive(Clone, Default)]
pub struct SharedStats {
    inner: Arc<RwLock<StatsSnapshot>>,
}

impl SharedStats {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StatsSnapshot::default())),
        }
    }

    /// Update stats from current connection state.
    pub fn update(&self, connections: &[SrtlaConnection], config: &ConfigSnapshot) {
        let current_time_ms = now_ms();
        let quality_enabled = config.quality_enabled && !config.mode.is_classic();

        let mut snapshot = StatsSnapshot {
            mode: format!("{}", config.mode),
            quality_enabled,
            rtt_delta_ms: config.rtt_delta_ms,
            total_links: connections.len(),
            ..Default::default()
        };

        for conn in connections {
            let timed_out = conn.is_timed_out();
            let is_active = conn.connected && !timed_out;

            // Quality multiplier: use actual selection algorithm calculation,
            // or 1.0 if quality scoring is disabled (classic mode)
            let quality_multiplier = if quality_enabled {
                calculate_quality_multiplier(conn, current_time_ms)
            } else {
                1.0
            };

            let link = LinkStats {
                ip: conn.local_ip,
                label: conn.label.clone(),
                connected: conn.connected,
                timed_out,
                window: conn.window,
                in_flight: conn.in_flight_packets,
                rtt_ms: conn.get_smooth_rtt_ms() as u32,
                nak_count: conn.total_nak_count(),
                bitrate_bps: (conn.current_bitrate_mbps() * 1_000_000.0 / 8.0) as u32,
                rtt_min_ms: conn.get_rtt_min_ms(),
                fast_rtt_ms: conn.get_fast_rtt_ms(),
                base_score: conn.get_score(),
                quality_multiplier,
            };

            if is_active {
                snapshot.active_links += 1;
                snapshot.total_window += conn.window;
                snapshot.total_in_flight += conn.in_flight_packets;
            }

            snapshot.links.push(link);
        }

        if let Ok(mut guard) = self.inner.write() {
            *guard = snapshot;
        }
    }

    /// Get current stats snapshot.
    pub fn get(&self) -> StatsSnapshot {
        self.inner
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.get()).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::SchedulingMode;

    #[test]
    fn test_shared_stats_new() {
        let stats = SharedStats::new();
        let snapshot = stats.get();
        assert_eq!(snapshot.active_links, 0);
        assert_eq!(snapshot.total_links, 0);
    }

    #[test]
    fn test_shared_stats_empty_update() {
        let stats = SharedStats::new();
        let config = ConfigSnapshot {
            mode: SchedulingMode::Enhanced,
            quality_enabled: true,
            exploration_enabled: false,
            rtt_delta_ms: 30,
        };
        stats.update(&[], &config);
        let snapshot = stats.get();
        assert_eq!(snapshot.mode, "enhanced");
        assert!(snapshot.quality_enabled);
    }

    #[test]
    fn test_to_json_contains_expected_fields() {
        let stats = SharedStats::new();
        let json = stats.to_json();
        assert!(json.contains("\"mode\""));
        assert!(json.contains("\"active_links\""));
        assert!(json.contains("\"total_window\""));
        assert!(json.contains("\"links\""));
    }
}
