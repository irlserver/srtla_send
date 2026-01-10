use std::sync::atomic::Ordering;

use tracing::{info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::PKT_LOG_SIZE;
use crate::toggles::DynamicToggles;
use crate::utils::now_ms;

/// Comprehensive status monitoring for connections
///
/// Optimized to reduce CPU overhead:
/// - Early exit if INFO logging is disabled
/// - Single pass over connections to collect stats
/// - Avoided redundant iterations
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn log_connection_status(
    connections: &[SrtlaConnection],
    last_selected_idx: Option<usize>,
    toggles: &DynamicToggles,
) {
    // Early exit if INFO logging is not enabled - avoid all computation
    if !tracing::enabled!(tracing::Level::INFO) {
        return;
    }

    let total_connections = connections.len();

    // Single pass over connections to collect all stats
    let mut active_connections = 0usize;
    let mut total_bitrate_mbps = 0.0f64;
    let mut total_in_flight = 0usize;

    for conn in connections.iter() {
        if !conn.is_timed_out() {
            active_connections += 1;
        }
        total_bitrate_mbps += conn.current_bitrate_mbps();
        total_in_flight += conn.in_flight_packets as usize;
    }

    let timed_out_connections = total_connections - active_connections;

    // Packet log utilization
    let max_possible_entries = total_connections * PKT_LOG_SIZE;
    let log_utilization = if max_possible_entries > 0 {
        (total_in_flight as f64 / max_possible_entries as f64) * 100.0
    } else {
        0.0
    };

    info!("📊 Connection Status Report:");
    info!("  Total connections: {}", total_connections);
    info!("  Total bitrate: {:.2} Mbps", total_bitrate_mbps);
    info!(
        "  Active connections: {} ({:.1}%)",
        active_connections,
        if total_connections > 0 {
            (active_connections as f64 / total_connections as f64) * 100.0
        } else {
            0.0
        }
    );
    info!("  Timed out connections: {}", timed_out_connections);

    // Show toggle states
    info!(
        "  Toggles: classic={}, quality={}, exploration={}",
        toggles.classic_mode.load(Ordering::Relaxed),
        toggles.quality_scoring_enabled.load(Ordering::Relaxed),
        toggles.exploration_enabled.load(Ordering::Relaxed)
    );

    // Show packet log utilization
    info!(
        "  Packet log: {} entries used ({:.1}% of capacity)",
        total_in_flight, log_utilization
    );

    // Show last selected connection
    if let Some(idx) = last_selected_idx {
        if idx < connections.len() {
            info!("  Last selected: {}", connections[idx].label);
        } else {
            warn!("  Last selected index {} is out of bounds!", idx);
        }
    } else {
        info!("  Last selected: none");
    }

    // Show individual connection details
    for (i, conn) in connections.iter().enumerate() {
        let status = if conn.is_timed_out() {
            "⏰ TIMED_OUT"
        } else {
            "✅ ACTIVE"
        };
        let score = conn.get_score();

        // Avoid String allocation for common score cases
        let score_desc: std::borrow::Cow<'static, str> = match score {
            -1 => "DISCONNECTED".into(),
            0 => "AT_CAPACITY".into(),
            s => s.to_string().into(),
        };

        // Use elapsed seconds directly
        let last_recv = conn
            .last_received
            .map(|t| format!("{:.1}s ago", t.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "never".into());

        let last_send = conn
            .last_sent
            .map(|t| format!("{:.1}s ago", t.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "never".into());

        info!(
            "    [{}] {} {} - Score: {} - Last recv/send: {}/{} - Window: {} - In-flight: {} - \
             Bitrate: {:.2} Mbps",
            i,
            status,
            conn.label,
            score_desc,
            last_recv,
            last_send,
            conn.window,
            conn.in_flight_packets,
            conn.current_bitrate_mbps()
        );

        if conn.rtt.estimated_rtt_ms > 0.0 {
            info!(
                "        RTT: smooth={:.1}ms, fast={:.1}ms, jitter={:.1}ms, stable={} (last: \
                 {:.1}s ago)",
                conn.get_smooth_rtt_ms(),
                conn.get_fast_rtt_ms(),
                conn.get_rtt_jitter_ms(),
                conn.is_rtt_stable(),
                (now_ms().saturating_sub(conn.rtt.last_rtt_measurement_ms) as f64) / 1000.0
            );
        }
    }

    // Show any warnings
    if active_connections == 0 {
        warn!("⚠️  No active connections available!");
    } else if active_connections < total_connections / 2 {
        warn!("⚠️  Less than half of connections are active");
    }
}
