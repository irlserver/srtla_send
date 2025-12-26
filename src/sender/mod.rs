mod housekeeping;
mod packet_handler;
mod selection;
mod sequence;
mod uplink;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow};
// Re-export public items used by tests
#[allow(unused_imports)]
pub use housekeeping::GLOBAL_TIMEOUT_MS;
use housekeeping::handle_housekeeping;
use packet_handler::{drain_packet_queue, handle_srt_packet, handle_uplink_packet};
#[allow(unused_imports)]
pub use selection::{calculate_quality_multiplier, select_connection_idx};
#[allow(unused_imports)]
pub use sequence::{
    MAX_SEQUENCE_TRACKING, SEQUENCE_MAP_CLEANUP_INTERVAL_MS, SEQUENCE_TRACKING_MAX_AGE_MS,
    SequenceTrackingEntry,
};
use smallvec::SmallVec;
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, info, warn};
use uplink::{ConnectionId, ReaderHandle, create_uplink_channel, sync_readers};

use crate::connection::SrtlaConnection;
use crate::protocol::PKT_LOG_SIZE;
use crate::registration::SrtlaRegistrationManager;
use crate::toggles::DynamicToggles;
use crate::utils::now_ms;

pub const HOUSEKEEPING_INTERVAL_MS: u64 = 1000;
const STATUS_LOG_INTERVAL_MS: u64 = 30_000;

pub struct PendingConnectionChanges {
    pub new_ips: Option<SmallVec<IpAddr, 4>>,
    pub receiver_host: String,
    pub receiver_port: u16,
}

pub async fn run_sender_with_toggles(
    local_srt_port: u16,
    receiver_host: &str,
    receiver_port: u16,
    ips_file: &str,
    toggles: DynamicToggles,
) -> Result<()> {
    info!(
        "starting srtla_send: local_srt_port={}, receiver={}:{}, ips_file={}",
        local_srt_port, receiver_host, receiver_port, ips_file
    );
    let ips = read_ip_list(ips_file).await?;
    debug!(
        "uplink IPs loaded: {}",
        ips.iter()
            .map(|i| i.to_string())
            .collect::<SmallVec<_, 4>>()
            .join(", ")
    );
    if ips.is_empty() {
        return Err(anyhow!("no IPs in list: {}", ips_file));
    }

    let mut connections = create_connections_from_ips(&ips, receiver_host, receiver_port).await;
    if connections.is_empty() {
        return Err(anyhow!("no uplinks available"));
    }

    let local_listener = UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, local_srt_port)))
        .await
        .context("bind local SRT UDP listener")?;
    info!("listening for SRT on [::]:{}", local_srt_port);

    let mut reg = SrtlaRegistrationManager::new();

    reg.start_probing(&mut connections).await;

    let (packet_tx, mut packet_rx) = create_uplink_channel();
    let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
    sync_readers(&connections, &mut reader_handles, &packet_tx);

    // Create instant ACK forwarding channel (sends client addr with packet)
    let (instant_tx, mut instant_rx) =
        tokio::sync::mpsc::unbounded_channel::<(SocketAddr, SmallVec<u8, 64>)>();

    // Wrap local_listener in Arc for sharing
    let local_listener = Arc::new(local_listener);

    // Spawn instant forwarding task
    {
        let local_listener_clone = local_listener.clone();
        tokio::spawn(async move {
            while let Some((client_addr, ack_packet)) = instant_rx.recv().await {
                let _ = local_listener_clone.send_to(&ack_packet, client_addr).await;
            }
        });
    }

    let mut recv_buf = vec![0u8; crate::protocol::MTU];
    let mut housekeeping_timer = time::interval_at(
        Instant::now() + Duration::from_millis(HOUSEKEEPING_INTERVAL_MS),
        Duration::from_millis(HOUSEKEEPING_INTERVAL_MS),
    );
    housekeeping_timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut status_elapsed_ms: u64 = 0;
    let mut last_client_addr: Option<SocketAddr> = None;
    let mut seq_to_conn: HashMap<u32, sequence::SequenceTrackingEntry> =
        HashMap::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut seq_order: VecDeque<u32> = VecDeque::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut last_sequence_cleanup_ms: u64 = 0;
    let mut last_selected_idx: Option<usize> = None;
    let mut last_switch_time_ms: u64 = 0; // Track time of last connection switch
    let mut all_failed_at: Option<Instant> = None;
    let mut pending_changes: Option<PendingConnectionChanges> = None;

    // Prepare SIGHUP stream (Unix only)
    #[cfg(unix)]
    #[allow(unused_variables)]
    let mut sighup = signal(SignalKind::hangup())?;

    // Main loop - run housekeeping frequently like C version
    // Run housekeeping once before entering the main event loop so we start in a clean state.
    {
        let classic = toggles
            .classic_mode
            .load(std::sync::atomic::Ordering::Relaxed);
        if let Err(err) = handle_housekeeping(
            &mut connections,
            &mut reg,
            &mut seq_to_conn,
            &mut seq_order,
            &mut last_sequence_cleanup_ms,
            classic,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
        )
        .await
        {
            warn!("initial housekeeping failed: {err}");
        }
    }

    #[cfg(unix)]
    loop {
        tokio::select! {
            res = local_listener.recv_from(&mut recv_buf) => {
                handle_srt_packet(
                    res,
                    &mut recv_buf,
                    &mut connections,
                    &mut last_selected_idx,
                    &mut last_switch_time_ms,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_client_addr,
                    reg.has_connected,
                    &toggles,
                )
                .await;
                drain_packet_queue(
                    &mut packet_rx,
                    &mut connections,
                    &mut reg,
                    &instant_tx,
                    last_client_addr,
                    &local_listener,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &toggles,
                )
                .await;
            }
            packet = packet_rx.recv() => {
                if let Some(packet) = packet {
                    handle_uplink_packet(
                        packet,
                        &mut connections,
                        &mut reg,
                        &instant_tx,
                        last_client_addr,
                        &local_listener,
                        &mut seq_to_conn,
                        &mut seq_order,
                        &toggles,
                    ).await;
                    drain_packet_queue(
                        &mut packet_rx,
                        &mut connections,
                        &mut reg,
                        &instant_tx,
                        last_client_addr,
                        &local_listener,
                        &mut seq_to_conn,
                        &mut seq_order,
                        &toggles,
                    ).await;
                } else {
                    return Ok(());
                }
            }
            _ = housekeeping_timer.tick() => {
                let classic = toggles
                    .classic_mode
                    .load(std::sync::atomic::Ordering::Relaxed);
                if let Err(err) = handle_housekeeping(
                    &mut connections,
                    &mut reg,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_sequence_cleanup_ms,
                    classic,
                    &mut all_failed_at,
                    &mut reader_handles,
                    &packet_tx,
                ).await {
                    warn!("housekeeping failed: {err}");
                }

                if let Some(changes) = pending_changes.take()
                    && let Some(new_ips) = changes.new_ips
                {
                    info!("applying queued connection changes: {} IPs", new_ips.len());
                    apply_connection_changes(
                        &mut connections,
                        &new_ips,
                        &changes.receiver_host,
                        changes.receiver_port,
                        &mut last_selected_idx,
                        &mut seq_to_conn,
                        &mut seq_order,
                    ).await;
                    info!("connection changes applied successfully");
                    sync_readers(&connections, &mut reader_handles, &packet_tx);
                }

                status_elapsed_ms = status_elapsed_ms.saturating_add(HOUSEKEEPING_INTERVAL_MS);
                if status_elapsed_ms >= STATUS_LOG_INTERVAL_MS {
                    log_connection_status(&connections, &seq_to_conn, &seq_order, last_selected_idx, &toggles);
                    status_elapsed_ms = status_elapsed_ms.saturating_sub(STATUS_LOG_INTERVAL_MS);
                }

                sync_readers(&connections, &mut reader_handles, &packet_tx);
                drain_packet_queue(
                    &mut packet_rx,
                    &mut connections,
                    &mut reg,
                    &instant_tx,
                    last_client_addr,
                    &local_listener,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &toggles,
                )
                .await;
            }
            _ = sighup.recv() => {
                info!("received SIGHUP - queuing uplink IP reload from {}", ips_file);
                if let Ok(new_ips) = read_ip_list(ips_file).await {
                    pending_changes = Some(PendingConnectionChanges {
                        new_ips: Some(new_ips),
                        receiver_host: receiver_host.to_string(),
                        receiver_port,
                    });
                    info!("uplink IP changes queued for next processing cycle");
                }
                drain_packet_queue(
                    &mut packet_rx,
                    &mut connections,
                    &mut reg,
                    &instant_tx,
                    last_client_addr,
                    &local_listener,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &toggles,
                )
                .await;
            }
        }
    }

    #[cfg(not(unix))]
    loop {
        tokio::select! {
            res = local_listener.recv_from(&mut recv_buf) => {
                handle_srt_packet(
                    res,
                    &mut recv_buf,
                    &mut connections,
                    &mut last_selected_idx,
                    &mut last_switch_time_ms,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_client_addr,
                    reg.has_connected,
                    &toggles,
                )
                .await;
                drain_packet_queue(
                    &mut packet_rx,
                    &mut connections,
                    &mut reg,
                    &instant_tx,
                    last_client_addr,
                    &local_listener,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &toggles,
                )
                .await;
            }
            packet = packet_rx.recv() => {
                if let Some(packet) = packet {
                    handle_uplink_packet(
                        packet,
                        &mut connections,
                        &mut reg,
                        &instant_tx,
                        last_client_addr,
                        &local_listener,
                        &mut seq_to_conn,
                        &mut seq_order,
                        &toggles,
                    ).await;
                    drain_packet_queue(
                        &mut packet_rx,
                        &mut connections,
                        &mut reg,
                        &instant_tx,
                        last_client_addr,
                        &local_listener,
                        &mut seq_to_conn,
                        &mut seq_order,
                        &toggles,
                    ).await;
                } else {
                    return Ok(());
                }
            }
            _ = housekeeping_timer.tick() => {
                let classic = toggles
                    .classic_mode
                    .load(std::sync::atomic::Ordering::Relaxed);
                if let Err(err) = handle_housekeeping(
                    &mut connections,
                    &mut reg,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_sequence_cleanup_ms,
                    classic,
                    &mut all_failed_at,
                    &mut reader_handles,
                    &packet_tx,
                ).await {
                   warn!("housekeeping failed: {err}");
               }

               if let Some(changes) = pending_changes.take()
                   && let Some(new_ips) = changes.new_ips
               {
                   info!("applying queued connection changes: {} IPs", new_ips.len());
                   apply_connection_changes(
                       &mut connections,
                       &new_ips,
                       &changes.receiver_host,
                       changes.receiver_port,
                       &mut last_selected_idx,
                       &mut seq_to_conn,
                       &mut seq_order,
                   )
                   .await;
                   info!("connection changes applied successfully");
                    sync_readers(&connections, &mut reader_handles, &packet_tx);
                }

                status_elapsed_ms = status_elapsed_ms.saturating_add(HOUSEKEEPING_INTERVAL_MS);
                if status_elapsed_ms >= STATUS_LOG_INTERVAL_MS {
                    log_connection_status(&connections, &seq_to_conn, &seq_order, last_selected_idx, &toggles);
                    status_elapsed_ms = status_elapsed_ms.saturating_sub(STATUS_LOG_INTERVAL_MS);
                }

                sync_readers(&connections, &mut reader_handles, &packet_tx);
                drain_packet_queue(
                    &mut packet_rx,
                    &mut connections,
                    &mut reg,
                    &instant_tx,
                    last_client_addr,
                    &local_listener,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &toggles,
                )
                .await;
            }
        }
    }
}

pub async fn read_ip_list(path: &str) -> Result<SmallVec<IpAddr, 4>> {
    let text = std::fs::read_to_string(Path::new(path)).context("read IPs file")?;
    let mut out = SmallVec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        match IpAddr::from_str(l) {
            Ok(ip) => out.push(ip),
            Err(e) => warn!("skip invalid IP '{}': {}", l, e),
        }
    }
    Ok(out)
}

pub(crate) async fn apply_connection_changes(
    connections: &mut SmallVec<SrtlaConnection, 4>,
    new_ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_to_conn: &mut HashMap<u32, sequence::SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
) {
    let current_labels: HashSet<String> = connections.iter().map(|c| c.label.clone()).collect();
    let desired_labels: HashSet<String> = new_ips
        .iter()
        .map(|ip| format!("{}:{} via {}", receiver_host, receiver_port, ip))
        .collect();

    // Remove stale connections
    let old_len = connections.len();
    connections.retain(|c| desired_labels.contains(&c.label));

    // If connections were removed, reset selection state and remap sequence
    // tracking to use stable conn_id instead of stale conn_idx
    if connections.len() != old_len {
        info!("removed {} stale connections", old_len - connections.len());
        *last_selected_idx = None;

        // Retain only entries whose conn_id still exists in the connections vector
        let valid_conn_ids: std::collections::HashSet<u64> =
            connections.iter().map(|c| c.conn_id).collect();
        seq_to_conn.retain(|_, entry| valid_conn_ids.contains(&entry.conn_id));

        seq_order.retain(|seq| seq_to_conn.contains_key(seq));
    }

    // Add new connections
    let new_ips_needed: SmallVec<IpAddr, 4> = new_ips
        .iter()
        .copied()
        .filter(|ip| {
            let label = format!("{}:{} via {}", receiver_host, receiver_port, ip);
            !current_labels.contains(&label)
        })
        .collect();

    if !new_ips_needed.is_empty() {
        let mut new_connections =
            create_connections_from_ips(&new_ips_needed, receiver_host, receiver_port).await;
        let added_count = new_connections.len();
        connections.append(&mut new_connections);

        if added_count > 0 {
            info!("added {} new connections", added_count);
        }
    }
}

pub async fn create_connections_from_ips(
    ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
) -> SmallVec<SrtlaConnection, 4> {
    let mut connections = SmallVec::new();
    for ip in ips {
        match SrtlaConnection::connect_from_ip(*ip, receiver_host, receiver_port).await {
            Ok(conn) => {
                info!("added uplink {}", conn.label);
                connections.push(conn);
            }
            Err(e) => warn!(
                "failed to add uplink {} -> {}:{}: {}",
                ip, receiver_host, receiver_port, e
            ),
        }
    }
    connections
}

/// Comprehensive status monitoring for connections
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn log_connection_status(
    connections: &[SrtlaConnection],
    seq_to_conn: &HashMap<u32, sequence::SequenceTrackingEntry>,
    seq_order: &VecDeque<u32>,
    last_selected_idx: Option<usize>,
    toggles: &DynamicToggles,
) {
    let total_connections = connections.len();
    let active_connections = connections.iter().filter(|c| !c.is_timed_out()).count();
    let timed_out_connections = total_connections - active_connections;

    // Calculate total bitrate across all connections
    let total_bitrate_mbps: f64 = connections.iter().map(|c| c.current_bitrate_mbps()).sum();

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

    info!(
        "  Sequence tracking: {} mappings ({:.1}% capacity), {} in queue",
        seq_to_conn.len(),
        (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0,
        seq_order.len()
    );

    // Show packet log utilization
    let total_log_entries: usize = connections
        .iter()
        .map(|c| c.in_flight_packets as usize)
        .sum();
    let max_possible_entries = connections.len() * PKT_LOG_SIZE;
    let log_utilization = if max_possible_entries > 0 {
        (total_log_entries as f64 / max_possible_entries as f64) * 100.0
    } else {
        0.0
    };
    info!(
        "  Packet log: {} entries used ({:.1}% of capacity)",
        total_log_entries, log_utilization
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
        let score_desc: String = match score {
            -1 => "DISCONNECTED".to_string(),
            0 => "AT_CAPACITY".to_string(),
            _ => score.to_string(),
        };

        let last_recv = conn
            .last_received
            .map(|t| format!("{:.1}s ago", t.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "never".to_string());

        let last_send = conn
            .last_sent
            .map(|t| format!("{:.1}s ago", t.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "never".to_string());

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
