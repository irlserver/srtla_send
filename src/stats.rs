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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::config::ConfigSnapshot;
use srtla_core::connection::SrtlaConnection;
use srtla_core::selection::calculate_quality_multiplier;
use srtla_core::selection::classifier::{ClassificationResult, WeakReason};
use srtla_core::selection::enhanced::in_flight_cap_packets;
use srtla_core::selection::link_cc::{CcState, LinkCcSnapshot};
use srtla_core::utils::now_ms;

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
    /// Smoothed RTT in milliseconds. From Kalman filter.
    pub rtt_ms: u32,
    /// Total NAK count since connection established. Indicates packet loss.
    pub nak_count: i32,
    /// Measured send rate in **bytes** per second (not estimated).
    ///
    /// Named for its unit on purpose. This was `bitrate_bps` while
    /// carrying bytes/sec, sitting next to genuinely bit-denominated
    /// fields like `cc_target_bps`, and feeding a Prometheus gauge whose
    /// name also claimed bits. Anything that compared the two, or
    /// graphed the gauge, was silently out by a factor of 8.
    pub bitrate_bytes_per_sec: u32,

    // --- RTT baseline tracking ---
    /// Dual-window minimum RTT baseline in milliseconds.
    pub rtt_min_ms: f64,
    /// Kalman RTT velocity in ms/sample (positive = rising, negative = falling).
    pub rtt_velocity: f64,

    // --- Selection algorithm context ---
    /// Base score: window / (in_flight + 1). Used by classic mode.
    /// Higher score = more available capacity on this link.
    pub base_score: i32,
    /// Quality multiplier (0.35 to 1.1) used by enhanced mode.
    /// - 1.1 = perfect (no NAKs ever)
    /// - 1.0 = normal
    /// - <1.0 = degraded due to recent NAKs
    /// - ~0.35 = heavily penalized (NAK burst)
    ///
    /// This is the EXACT multiplier used in `select_connection_idx()`.
    /// In classic mode, this is always 1.0 (quality scoring disabled).
    pub quality_multiplier: f64,

    // --- Weak-link classifier ---
    //
    // Output of `WeakLinkFilter::classify`. Currently informational only —
    // not consumed by selection. Once we soak the classifier behaviour
    // against real-world IRL traffic, the `weak` flag becomes an admission
    // gate in Enhanced selection.
    /// Whether the classifier flagged this link as weak this tick.
    pub weak: bool,
    /// Why the link was (or was not) flagged. One of: `healthy`, `high_rtt`,
    /// `no_traffic`, `low_share`, `bypassed`.
    pub weak_reason: String,
    /// This link's share of total throughput in permille (0..=1000).
    pub weak_share_permille: u32,
    /// Threshold the share was checked against (permille). Reflects
    /// entering vs leaving for hysteresis.
    pub weak_threshold_permille: u32,

    // --- Per-link CC soft cap ---
    //
    // Output of `LinkCcController::tick_all`. `cc_state`/`cc_backing_off`
    // are reported for telemetry and drive the CC controller's own
    // bitrate backoff; the routing-admission gate uses the sustained
    // `loss_degraded` latch, not the raw per-window backoff. `cc_target_bps`
    // scales the score multiplicatively via `enhanced::cc_soft_cap_multiplier`
    // so the scheduler steers traffic away from a link before it hits its
    // CC-predicted ceiling.
    /// Current state: `bootstrap` / `climbing` / `holding` /
    /// `backing_off` / `drain`.
    pub cc_state: String,
    /// Active climb sub-mode when `cc_state == climbing`. One of
    /// `normal` / `hai` / `fast_recovery`. `normal` for any other
    /// state (informational only).
    pub cc_climb_mode: String,
    /// Target sendable rate this link's CC believes is sustainable (bps).
    pub cc_target_bps: u64,
    /// Age-bucketed RTT EWMA (ms) — input to the CC state machine.
    pub cc_rtt_ewma_ms: f64,
    /// 1:3 weighted moving deviation around `cc_rtt_ewma_ms` (ms).
    pub cc_rtt_var_ms: f64,
    /// Lowest RTT ever observed on this link (ms).
    pub cc_rtt_min_ms: f64,
    /// Loss permille over the 1s rolling window.
    pub cc_loss_permille: u32,
    /// Time-decayed loss fraction (0..1) driving continuous phase
    /// demotion. Latches `cc_loss_degraded` once sustained high.
    pub cc_loss_ewma: f64,
    /// Whether the sustained-loss verdict has latched. A degraded link
    /// is demoted in score but kept schedulable, never removed.
    pub cc_loss_degraded: bool,

    // --- Adaptive batch-send regime ---
    /// Current per-connection batch-send regime. One of
    /// `low_activity` / `normal` / `high_load`. Driven from observed
    /// bitrate in housekeeping; dashboards can use it to explain why
    /// one link is batching more aggressively than another.
    pub batch_regime: String,

    // --- In-flight cap soft admission gate ---
    //
    // Derived from `cc_target_bps`: cap = (pps / 40) packets ≈ 25 ms of
    // sustainable in-flight. When `in_flight > in_flight_cap_packets`
    // the link is excluded from Enhanced selection while at least one
    // un-gated alternative is schedulable, bounding per-link queueing
    // delay before the CC controller has to back off on loss.
    /// In-flight cap in packets. `0` means "no signal" — the per-link
    /// CC hasn't published a `cc_target_bps` yet, so the cap is
    /// inactive.
    pub in_flight_cap_packets: u32,
    /// Whether the cap was active this tick (i.e. `in_flight` exceeded
    /// `in_flight_cap_packets`). When true and at least one other link
    /// is un-gated, this link is being skipped by Enhanced selection.
    pub in_flight_cap_active: bool,
}

/// Aggregate statistics snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    /// Current scheduling mode: "classic" or "enhanced"
    pub mode: String,
    /// Whether quality scoring is enabled (always false for classic mode)
    pub quality_enabled: bool,

    /// Number of links that are connected AND not timed out
    pub active_links: usize,
    /// Total configured links
    pub total_links: usize,

    /// Sum of window across active links
    pub total_window: i32,
    /// Sum of in_flight across active links
    pub total_in_flight: i32,

    // --- Weak-link classifier output ---
    /// Estimated max delay budget the classifier derived this tick (ms).
    /// Zero when classification was bypassed (e.g. under the throughput floor).
    pub weak_link_estimated_max_delay_ms: u32,
    /// Delay tier the cascade chose this tick (ms).
    pub weak_link_selected_delay_ms: u32,

    /// Per-link details
    pub links: Vec<LinkStats>,
}

impl Default for StatsSnapshot {
    fn default() -> Self {
        Self {
            mode: "enhanced".to_string(),
            quality_enabled: true,
            active_links: 0,
            total_links: 0,
            total_window: 0,
            total_in_flight: 0,
            weak_link_estimated_max_delay_ms: 0,
            weak_link_selected_delay_ms: 0,
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
    ///
    /// `classification` carries the weak-link classifier's per-tick output.
    /// Pass `None` when the classifier is disabled or unavailable; the weak
    /// fields are populated with neutral defaults in that case.
    pub fn update(
        &self,
        connections: &[SrtlaConnection],
        config: &ConfigSnapshot,
        classification: Option<&ClassificationResult>,
        link_cc: Option<&HashMap<u64, LinkCcSnapshot>>,
    ) {
        let current_time_ms = now_ms();
        let quality_enabled = config.quality_enabled && !config.mode.is_classic();

        let mut snapshot = StatsSnapshot {
            mode: format!("{}", config.mode),
            quality_enabled,
            total_links: connections.len(),
            weak_link_estimated_max_delay_ms: classification
                .map(|c| c.estimated_max_delay_ms)
                .unwrap_or(0),
            weak_link_selected_delay_ms: classification.map(|c| c.selected_delay_ms).unwrap_or(0),
            ..Default::default()
        };

        for conn in connections {
            let timed_out = conn.is_timed_out(current_time_ms);
            let is_active = conn.connected && !timed_out;

            // Quality multiplier: use actual selection algorithm calculation,
            // or 1.0 if quality scoring is disabled (classic mode)
            let quality_multiplier = if quality_enabled {
                calculate_quality_multiplier(conn, current_time_ms)
            } else {
                1.0
            };

            let weak_entry =
                classification.and_then(|c| c.per_link.iter().find(|e| e.conn_id == conn.conn_id));
            let (weak, weak_reason, weak_share, weak_threshold) = match weak_entry {
                Some(e) => (
                    e.weak,
                    weak_reason_str(e.reason).to_string(),
                    e.share_permille,
                    e.threshold_permille,
                ),
                None => (false, "unknown".to_string(), 0, 0),
            };

            let cc_entry = link_cc.and_then(|m| m.get(&conn.conn_id).copied());
            let (
                cc_state,
                cc_climb_mode,
                cc_target_bps,
                cc_rtt_ewma,
                cc_rtt_var,
                cc_rtt_min,
                cc_loss_pm,
                cc_loss_ewma,
                cc_loss_degraded,
            ) = match cc_entry {
                Some(s) => (
                    cc_state_str(s.state).to_string(),
                    s.climb_mode.as_str().to_string(),
                    s.target_bps,
                    s.rtt_ewma_ms,
                    s.rtt_var_ms,
                    s.rtt_min_ms,
                    s.loss_permille,
                    s.loss_ewma,
                    s.loss_degraded,
                ),
                None => (
                    "unknown".to_string(),
                    "normal".to_string(),
                    0,
                    0.0,
                    0.0,
                    0.0,
                    0,
                    0.0,
                    false,
                ),
            };

            let cap = in_flight_cap_packets(cc_target_bps, conn.get_rtt_min_ms());
            let in_flight_cap_pkts = cap.unwrap_or(0).max(0) as u32;
            let in_flight_cap_active = cap.map(|c| conn.in_flight_packets > c).unwrap_or(false);

            let link = LinkStats {
                ip: conn.local_ip,
                label: conn.label.clone(),
                connected: conn.connected,
                timed_out,
                window: conn.window,
                in_flight: conn.in_flight_packets,
                rtt_ms: conn.get_smooth_rtt_ms() as u32,
                nak_count: conn.total_nak_count(),
                bitrate_bytes_per_sec: (conn.current_bitrate_mbps() * 1_000_000.0 / 8.0) as u32,
                rtt_min_ms: conn.get_rtt_min_ms(),
                rtt_velocity: conn.get_rtt_velocity(),
                base_score: conn.get_score(),
                quality_multiplier,
                weak,
                weak_reason,
                weak_share_permille: weak_share,
                weak_threshold_permille: weak_threshold,
                cc_state,
                cc_climb_mode,
                cc_target_bps,
                cc_rtt_ewma_ms: cc_rtt_ewma,
                cc_rtt_var_ms: cc_rtt_var,
                cc_rtt_min_ms: cc_rtt_min,
                cc_loss_permille: cc_loss_pm,
                cc_loss_ewma,
                cc_loss_degraded,
                batch_regime: conn.batch_sender.regime().as_str().to_string(),
                in_flight_cap_packets: in_flight_cap_pkts,
                in_flight_cap_active,
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

fn weak_reason_str(reason: WeakReason) -> &'static str {
    match reason {
        WeakReason::Healthy => "healthy",
        WeakReason::HighRtt => "high_rtt",
        WeakReason::QueueBuilding => "queue_building",
        WeakReason::NoTraffic => "no_traffic",
        WeakReason::LowShare => "low_share",
        WeakReason::Bypassed => "bypassed",
    }
}

fn cc_state_str(state: CcState) -> &'static str {
    state.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use srtla_core::mode::SchedulingMode;

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
            ..ConfigSnapshot::default()
        };
        stats.update(&[], &config, None, None);
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
