use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
// mpsc is available in tokio::sync
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::{self, MTU};
use crate::registration::SrtlaRegistrationManager;
use crate::toggles::DynamicToggles;

pub const MIN_SWITCH_INTERVAL_MS: u64 = 500;
pub const MAX_SEQUENCE_TRACKING: usize = 10_000;
pub const GLOBAL_TIMEOUT_MS: u64 = 10_000;

pub struct PendingConnectionChanges {
    pub new_ips: Option<Vec<IpAddr>>,
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
            .collect::<Vec<_>>()
            .join(", ")
    );
    if ips.is_empty() {
        return Err(anyhow!("no IPs in list: {}", ips_file));
    }

    let local_listener = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, local_srt_port)))
        .await
        .context("bind local SRT UDP listener")?;
    info!("listening for SRT on 0.0.0.0:{}", local_srt_port);

    let mut connections = create_connections_from_ips(&ips, receiver_host, receiver_port).await;
    if connections.is_empty() {
        return Err(anyhow!("no uplinks available"));
    }

    let mut reg = SrtlaRegistrationManager::new();

    // Create instant ACK forwarding channel and shared client address
    let (instant_tx, instant_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let shared_client_addr = Arc::new(Mutex::new(None::<SocketAddr>));

    // Wrap local_listener in Arc for sharing
    let local_listener = Arc::new(local_listener);

    // Spawn instant forwarding task
    {
        let local_listener_clone = local_listener.clone();
        let shared_client_addr_clone = shared_client_addr.clone();
        tokio::spawn(async move {
            loop {
                match instant_rx.recv() {
                    Ok(ack_packet) => {
                        let client_addr = {
                            match shared_client_addr_clone.lock() {
                                Ok(addr_guard) => *addr_guard,
                                _ => None,
                            }
                        };
                        if let Some(client) = client_addr {
                            let _ = local_listener_clone.send_to(&ack_packet, client).await;
                        }
                    }
                    Err(_) => break, // Channel closed
                }
            }
        });
    }

    let mut recv_buf = vec![0u8; MTU];
    let mut interval = time::interval(Duration::from_millis(1));
    let mut last_client_addr: Option<SocketAddr> = None;
    // Sequence → connection index mapping for correct NAK attribution
    let mut seq_to_conn: HashMap<u32, usize> = HashMap::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut seq_order: VecDeque<u32> = VecDeque::with_capacity(MAX_SEQUENCE_TRACKING);
    // Stickiness
    let mut last_selected_idx: Option<usize> = None;
    let mut last_switch_time: Option<Instant> = None;
    // stickiness interval defined at module level

    // Connection failure tracking
    let mut all_failed_at: Option<Instant> = None;

    // Pending connection changes (applied safely between processing cycles)
    let mut pending_changes: Option<PendingConnectionChanges> = None;

    // Prepare SIGHUP stream (Unix only)
    #[cfg(unix)]
    let mut sighup = signal(SignalKind::hangup())?;

    // Main loop with conditional SIGHUP handling
    #[cfg(unix)]
    loop {
        tokio::select! {
            res = local_listener.recv_from(&mut recv_buf) => {
                handle_srt_packet(res, &mut recv_buf, &mut connections, &mut last_selected_idx, &mut last_switch_time, &mut seq_to_conn, &mut seq_order, &mut last_client_addr, &shared_client_addr, &toggles).await;
            }
            _ = interval.tick() => {
                // Apply any pending connection changes at a safe point
                if let Some(changes) = pending_changes.take() {
                    if let Some(new_ips) = changes.new_ips {
                        info!("applying queued connection changes: {} IPs", new_ips.len());
                        apply_connection_changes(&mut connections, &new_ips, &changes.receiver_host, changes.receiver_port, &mut last_selected_idx, &mut seq_to_conn).await;
                        info!("connection changes applied successfully");
                    }
                }

                let classic = toggles.classic_mode.load(std::sync::atomic::Ordering::Relaxed);
                handle_housekeeping(&mut connections, &mut reg, &instant_tx, last_client_addr, &local_listener, &mut seq_to_conn, classic, &mut all_failed_at).await?;
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
            }
        }
    }

    #[cfg(not(unix))]
    loop {
        tokio::select! {
            res = local_listener.recv_from(&mut recv_buf) => {
                handle_srt_packet(res, &mut recv_buf, &mut connections, &mut last_selected_idx, &mut last_switch_time, &mut seq_to_conn, &mut seq_order, &mut last_client_addr, &shared_client_addr, &toggles).await;
            }
            _ = interval.tick() => {
                // Apply any pending connection changes at a safe point
                if let Some(changes) = pending_changes.take() {
                    if let Some(new_ips) = changes.new_ips {
                        info!("applying queued connection changes: {} IPs", new_ips.len());
                        apply_connection_changes(&mut connections, &new_ips, &changes.receiver_host, changes.receiver_port, &mut last_selected_idx, &mut seq_to_conn).await;
                        info!("connection changes applied successfully");
                    }
                }

                let classic = toggles.classic_mode.load(std::sync::atomic::Ordering::Relaxed);
                handle_housekeeping(&mut connections, &mut reg, &instant_tx, last_client_addr, &local_listener, &mut seq_to_conn, classic, &mut all_failed_at).await?;
            }
        }
    }
}

async fn handle_srt_packet(
    res: Result<(usize, SocketAddr), std::io::Error>,
    recv_buf: &mut [u8],
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    last_switch_time: &mut Option<Instant>,
    seq_to_conn: &mut HashMap<u32, usize>,
    seq_order: &mut VecDeque<u32>,
    last_client_addr: &mut Option<SocketAddr>,
    shared_client_addr: &Arc<Mutex<Option<SocketAddr>>>,
    toggles: &DynamicToggles,
) {
    match res {
        Ok((n, src)) => {
            if n == 0 {
                return;
            }
            let pkt = &recv_buf[..n];
            let seq = protocol::get_srt_sequence_number(pkt);
            // pick best connection (respect dynamic stickiness toggle)
            let enable_stick = toggles
                .stickiness_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let enable_quality = toggles
                .quality_scoring_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let enable_explore = toggles
                .exploration_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let classic = toggles
                .classic_mode
                .load(std::sync::atomic::Ordering::Relaxed);

            // Classic mode overrides all other toggles
            let effective_enable_stick = enable_stick && !classic;
            let effective_enable_quality = enable_quality && !classic;
            let effective_enable_explore = enable_explore && !classic;

            let sel_idx = select_connection_idx(
                connections,
                if effective_enable_stick {
                    *last_selected_idx
                } else {
                    None
                },
                if effective_enable_stick {
                    *last_switch_time
                } else {
                    None
                },
                effective_enable_quality,
                effective_enable_explore,
                classic,
            );
            if let Some(sel_idx) = sel_idx {
                // Safe access - connection changes only happen between processing cycles
                if sel_idx < connections.len() {
                    // record stickiness
                    if *last_selected_idx != Some(sel_idx) {
                        *last_selected_idx = Some(sel_idx);
                        *last_switch_time = Some(Instant::now());
                    }
                    let conn = &mut connections[sel_idx];
                    debug!("forward {} bytes (seq={:?}) via {}", n, seq, conn.label);
                    let _ = conn.send_data_with_tracking(pkt, seq).await;
                    if let Some(s) = seq {
                        // track mapping
                        if seq_to_conn.len() >= MAX_SEQUENCE_TRACKING {
                            if let Some(old) = seq_order.pop_front() {
                                seq_to_conn.remove(&old);
                            }
                        }
                        seq_to_conn.insert(s, sel_idx);
                        seq_order.push_back(s);
                    }
                }
            } else {
                warn!("no available connection to forward packet from {}", src);
            }
            *last_client_addr = Some(src);
            // Update shared client address for instant forwarding
            if let Ok(mut addr_guard) = shared_client_addr.lock() {
                *addr_guard = Some(src);
            }
        }
        Err(e) => warn!("error reading local SRT: {}", e),
    }
}

async fn handle_housekeeping(
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<Vec<u8>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, usize>,
    classic: bool,
    all_failed_at: &mut Option<Instant>,
) -> Result<()> {
    // housekeeping: receive responses, drive registration, send keepalives
    for i in 0..connections.len() {
        let incoming = connections[i].drain_incoming(i, reg, instant_tx).await?;
        if incoming.read_any { /* timestamps updated inside */ }

        // Apply ACKs to all connections like Java
        for ack in incoming.ack_numbers.iter() {
            for c in connections.iter_mut() {
                c.handle_srt_ack(*ack as i32);
            }
        }

        // Process SRTLA ACKs: first find specific packet, then apply global +1 to all connections
        // This matches the original implementation's register_srtla_ack behavior
        for srtla_ack in incoming.srtla_ack_numbers.iter() {
            // Phase 1: Find the connection that sent this specific packet
            let mut found = false;
            for c in connections.iter_mut() {
                if c.handle_srtla_ack_specific(*srtla_ack as i32) {
                    found = true;
                    break;
                }
            }
            debug!("SRTLA ACK {} processed (found={})", srtla_ack, found);

            // Phase 2: Apply +1 window increase to ALL active connections
            for c in connections.iter_mut() {
                c.handle_srtla_ack_global();
            }
        }

        // NAK attribution to the connection that originally sent the packet
        for nak in incoming.nak_numbers.iter() {
            if let Some(&idx) = seq_to_conn.get(nak) {
                if let Some(conn) = connections.get_mut(idx) {
                    conn.handle_nak(*nak as i32);
                }
            } else {
                // Fallback: attribute to the connection that received the NAK
                connections[i].handle_nak(*nak as i32);
            }
        }

        // Forward responses back to local SRT client
        if let Some(client) = last_client_addr {
            for pkt in incoming.forward_to_client.iter() {
                let _ = local_listener.send_to(pkt, client).await;
            }
        }

        if connections[i].needs_keepalive() {
            let _ = connections[i].send_keepalive().await;
        }
        if connections[i].needs_rtt_measurement() {
            let _ = connections[i].send_keepalive().await;
        }
        if !classic {
            connections[i].perform_window_recovery();
        }
        // Simple reconnect-on-timeout, then allow reg driver to proceed
        if connections[i].is_timed_out() {
            if connections[i].should_attempt_reconnect() {
                connections[i].record_reconnect_attempt();
                warn!(
                    "{} timed out; attempting reconnection",
                    connections[i].label
                );
                connections[i].mark_disconnected();
                match connections[i].reconnect().await {
                    Ok(()) => {
                        info!("{} reconnected; re-sending REG2", connections[i].label);
                        reg.trigger_broadcast_reg2(connections).await;
                    }
                    Err(e) => warn!("{} reconnect failed: {}", connections[i].label, e),
                }
            } else {
                debug!("{} timed out but in backoff period", connections[i].label);
            }
        }
    }
    // drive registration (send REG1/REG2 as needed)
    reg.reg_driver_send_if_needed(connections).await;

    // Check for connection failures and output appropriate error messages
    // This matches the C implementation's connection_housekeeping logic
    let active_connections = connections.iter().filter(|c| !c.is_timed_out()).count();

    if active_connections == 0 {
        if all_failed_at.is_none() {
            *all_failed_at = Some(Instant::now());
        }

        if reg.has_connected {
            error!("warning: no available connections");
        }

        // Timeout when all connections have failed
        if let Some(failed_at) = all_failed_at {
            if failed_at.elapsed().as_millis() > GLOBAL_TIMEOUT_MS as u128 {
                if reg.has_connected {
                    error!("Failed to re-establish any connections");
                    return Err(anyhow!("Failed to re-establish any connections"));
                } else {
                    error!("Failed to establish any initial connections");
                    return Err(anyhow!("Failed to establish any initial connections"));
                }
            }
        }
    } else {
        *all_failed_at = None;
    }

    Ok(())
}

/// Calculate quality multiplier for a connection based on NAK history
/// Returns a multiplier between 0.05 and 1.2
pub(crate) fn calculate_quality_multiplier(conn: &SrtlaConnection) -> f64 {
    if let Some(tsn) = conn.time_since_last_nak_ms() {
        let mut quality_mult = if tsn < 2000 {
            0.1
        } else if tsn < 5000 {
            0.5
        } else if tsn < 10_000 {
            0.8
        } else if conn.total_nak_count() == 0 {
            1.2
        } else {
            1.0
        };

        // Extra penalty for burst NAKs (multiple NAKs in short time)
        if conn.nak_burst_count() > 1 && tsn < 5000 {
            quality_mult *= 0.5; // Halve score for connections with NAK bursts
        }
        quality_mult
    } else if conn.total_nak_count() == 0 {
        // Bonus for connections that have never had NAKs
        1.2
    } else {
        1.0
    }
}

pub fn select_connection_idx(
    conns: &[SrtlaConnection],
    last_idx: Option<usize>,
    last_switch: Option<Instant>,
    enable_quality: bool,
    enable_explore: bool,
    classic: bool,
) -> Option<usize> {
    // Classic mode: simple algorithm matching original implementation
    if classic {
        let mut best_idx: Option<usize> = None;
        let mut best_score: i32 = -1;

        for (i, c) in conns.iter().enumerate() {
            let score = c.get_score();
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        return best_idx;
    }

    // Enhanced mode: stickiness, quality scoring, exploration
    // Base: stickiness window
    if let (Some(idx), Some(ts)) = (last_idx, last_switch) {
        if ts.elapsed().as_millis() < (MIN_SWITCH_INTERVAL_MS as u128) {
            return Some(idx);
        }
    }
    // Exploration window: simple periodic exploration of second-best
    let explore_now = enable_explore && (Instant::now().elapsed().as_millis() % 5000) < 300;
    // Score connections by base score; apply quality multiplier unless classic
    let mut best_idx: Option<usize> = None;
    let mut second_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut second_score: f64 = -1.0;
    for (i, c) in conns.iter().enumerate() {
        let base = c.get_score() as f64;
        let score = if !enable_quality {
            base
        } else {
            let quality_mult = calculate_quality_multiplier(c);
            let final_score = (base * quality_mult).max(1.0);

            // Log quality analysis for debugging (only for low scores to avoid spam)
            if quality_mult < 1.0 {
                debug!(
                    "{} quality penalty: {:.2} (NAKs: {}, last: {}ms ago, burst: {}) base: {} → final: {}",
                    c.label,
                    quality_mult,
                    c.total_nak_count(),
                    c.time_since_last_nak_ms().unwrap_or(0),
                    c.nak_burst_count(),
                    base as i32,
                    final_score as i32
                );
            }

            final_score
        };
        if score > best_score {
            second_score = best_score;
            second_idx = best_idx;
            best_score = score;
            best_idx = Some(i);
        } else if score > second_score {
            second_score = score;
            second_idx = Some(i);
        }
    }
    if explore_now {
        second_idx.or(best_idx)
    } else {
        best_idx
    }
}

pub async fn read_ip_list(path: &str) -> Result<Vec<IpAddr>> {
    let text = std::fs::read_to_string(Path::new(path)).context("read IPs file")?;
    let mut out = Vec::new();
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

pub async fn apply_connection_changes(
    connections: &mut Vec<SrtlaConnection>,
    new_ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_to_conn: &mut HashMap<u32, usize>,
) {
    use std::collections::HashSet;

    let current_labels: HashSet<String> = connections.iter().map(|c| c.label.clone()).collect();
    let desired_labels: HashSet<String> = new_ips
        .iter()
        .map(|ip| format!("{}:{} via {}", receiver_host, receiver_port, ip))
        .collect();

    // Remove stale connections
    let old_len = connections.len();
    connections.retain(|c| desired_labels.contains(&c.label));

    // If connections were removed, reset selection state and clean up sequence tracking
    if connections.len() != old_len {
        info!("removed {} stale connections", old_len - connections.len());
        *last_selected_idx = None; // Reset selection to prevent index issues

        // Clean up sequence tracking for removed connections
        seq_to_conn.retain(|_, &mut conn_idx| conn_idx < connections.len());

        // Rebuild sequence tracking with correct indices
        let mut new_seq_to_conn = HashMap::with_capacity(seq_to_conn.len());
        for (seq, &old_idx) in seq_to_conn.iter() {
            if old_idx < connections.len() {
                new_seq_to_conn.insert(*seq, old_idx);
            }
        }
        *seq_to_conn = new_seq_to_conn;
    }

    // Add new connections
    let new_ips_needed: Vec<IpAddr> = new_ips
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
) -> Vec<SrtlaConnection> {
    let mut connections = Vec::new();
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
