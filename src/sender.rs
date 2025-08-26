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
use tracing::{debug, info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::{self, MTU};
use crate::registration::SrtlaRegistrationManager;
use crate::toggles::DynamicToggles;

const MIN_SWITCH_INTERVAL_MS: u64 = 500;
const MAX_SEQUENCE_TRACKING: usize = 10_000;

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

    let mut connections = Vec::new();
    for ip in ips {
        match SrtlaConnection::connect_from_ip(ip, receiver_host, receiver_port).await {
            Ok(conn) => {
                info!("added uplink {} -> {}:{}", ip, receiver_host, receiver_port);
                connections.push(conn);
            }
            Err(e) => warn!("failed to add uplink {}: {}", ip, e),
        }
    }
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
                handle_housekeeping(&mut connections, &mut reg, &instant_tx, last_client_addr, &local_listener, &mut seq_to_conn).await?;
            }
            _ = sighup.recv() => {
                info!("received SIGHUP - reloading uplink IPs from {}", ips_file);
                if let Ok(new_ips) = read_ip_list(ips_file).await {
                    reconcile_connections(&mut connections, &new_ips, receiver_host, receiver_port).await;
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
                handle_housekeeping(&mut connections, &mut reg, &instant_tx, last_client_addr, &local_listener, &mut seq_to_conn).await?;
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
            let sel_idx = select_connection_idx(
                connections,
                if enable_stick {
                    *last_selected_idx
                } else {
                    None
                },
                if enable_stick {
                    *last_switch_time
                } else {
                    None
                },
                enable_quality,
                enable_explore,
                classic,
            );
            if let Some(sel_idx) = sel_idx {
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
        connections[i].perform_window_recovery();
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
    Ok(())
}

fn select_connection_idx(
    conns: &[SrtlaConnection],
    last_idx: Option<usize>,
    last_switch: Option<Instant>,
    enable_quality: bool,
    enable_explore: bool,
    classic: bool,
) -> Option<usize> {
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
        let score = if classic || !enable_quality {
            base
        } else {
            let mut quality_mult = 1.0f64;
            if let Some(tsn) = c.time_since_last_nak_ms() {
                if tsn < 2000 {
                    quality_mult = 0.1;
                } else if tsn < 5000 {
                    quality_mult = 0.5;
                } else if tsn < 10_000 {
                    quality_mult = 0.8;
                } else if c.total_nak_count() == 0 {
                    quality_mult = 1.2;
                }

                // Extra penalty for burst NAKs (multiple NAKs in short time)
                if c.nak_burst_count() > 1 && tsn < 5000 {
                    quality_mult *= 0.5; // Halve score for connections with NAK bursts
                }
            }
            (base * quality_mult).max(0.0)
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

async fn read_ip_list(path: &str) -> Result<Vec<IpAddr>> {
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

#[cfg(unix)]
async fn reconcile_connections(
    conns: &mut Vec<SrtlaConnection>,
    new_ips: &[IpAddr],
    host: &str,
    port: u16,
) {
    use std::collections::HashSet;
    let current_labels: HashSet<String> = conns.iter().map(|c| c.label.clone()).collect();
    let desired_labels: HashSet<String> = new_ips
        .iter()
        .map(|ip| format!("{}:{} via {}", host, port, ip))
        .collect();

    // Remove stale
    conns.retain(|c| desired_labels.contains(&c.label));

    // Add new
    for ip in new_ips {
        let label = format!("{}:{} via {}", host, port, ip);
        if !current_labels.contains(&label) {
            match SrtlaConnection::connect_from_ip(*ip, host, port).await {
                Ok(conn) => {
                    info!("added uplink {}", conn.label);
                    conns.push(conn);
                }
                Err(e) => warn!("failed to add uplink {}: {}", label, e),
            }
        }
    }
}
