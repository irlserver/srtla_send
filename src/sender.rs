use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use smallvec::SmallVec;
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
// mpsc is available in tokio::sync
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::connection::{SrtlaConnection, SrtlaIncoming};
use crate::protocol::{self, MTU, PKT_LOG_SIZE};
use crate::registration::SrtlaRegistrationManager;
use crate::toggles::DynamicToggles;
use crate::utils::now_ms;

pub const MAX_SEQUENCE_TRACKING: usize = 10_000;
pub const SEQUENCE_TRACKING_MAX_AGE_MS: u64 = 5000;
pub const SEQUENCE_MAP_CLEANUP_INTERVAL_MS: u64 = 5000;
pub const GLOBAL_TIMEOUT_MS: u64 = 10_000;
pub const HOUSEKEEPING_INTERVAL_MS: u64 = 1000;
const STATUS_LOG_INTERVAL_MS: u64 = 30_000;

pub(crate) struct SequenceTrackingEntry {
    pub(crate) conn_id: u64,
    pub(crate) timestamp_ms: u64,
}

impl SequenceTrackingEntry {
    fn is_expired(&self, current_time_ms: u64) -> bool {
        current_time_ms.saturating_sub(self.timestamp_ms) > SEQUENCE_TRACKING_MAX_AGE_MS
    }
}

pub struct PendingConnectionChanges {
    pub new_ips: Option<SmallVec<IpAddr, 4>>,
    pub receiver_host: String,
    pub receiver_port: u16,
}

type ConnectionId = u64;

struct ReaderHandle {
    handle: JoinHandle<()>,
}

struct UplinkPacket {
    conn_id: ConnectionId,
    bytes: SmallVec<u8, 64>,
}

fn spawn_reader(
    conn_id: ConnectionId,
    label: String,
    socket: Arc<UdpSocket>,
    packet_tx: UnboundedSender<UplinkPacket>,
) -> ReaderHandle {
    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; MTU];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((n, _)) if n > 0 => {
                    let packet = SmallVec::from_slice(&buf[..n]);
                    if packet_tx
                        .send(UplinkPacket {
                            conn_id,
                            bytes: packet,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("{}: uplink recv error: {}", label, err);
                    if packet_tx
                        .send(UplinkPacket {
                            conn_id,
                            bytes: SmallVec::new(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    // Allow brief pause before retrying to avoid tight error loops.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });
    ReaderHandle { handle }
}

fn sync_readers(
    connections: &[SrtlaConnection],
    readers: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) {
    let mut active_ids = HashSet::with_capacity(connections.len());
    for conn in connections {
        active_ids.insert(conn.conn_id);
        readers.entry(conn.conn_id).or_insert_with(|| {
            spawn_reader(
                conn.conn_id,
                conn.label.clone(),
                conn.socket.clone(),
                packet_tx.clone(),
            )
        });
    }

    readers.retain(|conn_id, reader| {
        if active_ids.contains(conn_id) {
            true
        } else {
            reader.handle.abort();
            false
        }
    });
}

fn restart_reader_for(
    conn: &SrtlaConnection,
    readers: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) {
    if let Some(reader) = readers.remove(&conn.conn_id) {
        reader.handle.abort();
    }
    readers.insert(
        conn.conn_id,
        spawn_reader(
            conn.conn_id,
            conn.label.clone(),
            conn.socket.clone(),
            packet_tx.clone(),
        ),
    );
}

#[allow(clippy::too_many_arguments)]
async fn process_connection_events(
    idx: usize,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    _seq_order: &mut VecDeque<u32>,
    classic: bool,
    incoming_override: Option<SrtlaIncoming>,
) -> Result<()> {
    if idx >= connections.len() {
        return Ok(());
    }

    let incoming = if let Some(overridden) = incoming_override {
        overridden
    } else {
        connections[idx]
            .drain_incoming(idx, reg, instant_tx)
            .await?
    };

    if !incoming.read_any
        && incoming.ack_numbers.is_empty()
        && incoming.nak_numbers.is_empty()
        && incoming.srtla_ack_numbers.is_empty()
        && incoming.forward_to_client.is_empty()
    {
        return Ok(());
    }

    for ack in incoming.ack_numbers.iter() {
        for c in connections.iter_mut() {
            c.handle_srt_ack(*ack as i32);
        }
    }

    for srtla_ack in incoming.srtla_ack_numbers.iter() {
        for c in connections.iter_mut() {
            if c.handle_srtla_ack_specific(*srtla_ack as i32, classic) {
                break;
            }
        }
        for c in connections.iter_mut() {
            c.handle_srtla_ack_global();
        }
    }

    for nak in incoming.nak_numbers.iter() {
        let mut handled = false;
        if let Some(entry) = seq_to_conn.get(nak) {
            let current_time = now_ms();
            if !entry.is_expired(current_time) {
                if let Some(conn) = connections.iter_mut().find(|c| c.conn_id == entry.conn_id) {
                    conn.handle_nak(*nak as i32);
                    handled = true;
                }
            }
        }

        if !handled {
            for conn in connections.iter_mut() {
                if conn.handle_nak(*nak as i32) {
                    break;
                }
            }
        }
    }

    if let Some(client) = last_client_addr {
        for pkt in incoming.forward_to_client.iter() {
            let _ = local_listener.send_to(pkt, client).await;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_uplink_packet(
    packet: UplinkPacket,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    toggles: &DynamicToggles,
) {
    if packet.bytes.is_empty() {
        return;
    }
    if let Some(idx) = connections.iter().position(|c| c.conn_id == packet.conn_id) {
        match connections[idx]
            .process_packet(idx, reg, instant_tx, &packet.bytes)
            .await
        {
            Ok(incoming) => {
                let classic = toggles
                    .classic_mode
                    .load(std::sync::atomic::Ordering::Relaxed);
                if let Err(err) = process_connection_events(
                    idx,
                    connections,
                    reg,
                    instant_tx,
                    last_client_addr,
                    local_listener,
                    seq_to_conn,
                    seq_order,
                    classic,
                    Some(incoming),
                )
                .await
                {
                    warn!("failed to apply uplink {} packet: {err}", packet.conn_id);
                }
            }
            Err(err) => warn!(
                "failed to process packet for uplink {}: {}",
                packet.conn_id, err
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_packet_queue(
    packet_rx: &mut UnboundedReceiver<UplinkPacket>,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    toggles: &DynamicToggles,
) {
    while let Ok(packet) = packet_rx.try_recv() {
        handle_uplink_packet(
            packet,
            connections,
            reg,
            instant_tx,
            last_client_addr,
            local_listener,
            seq_to_conn,
            seq_order,
            toggles,
        )
        .await;
    }
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

    let (packet_tx, mut packet_rx) = unbounded_channel::<UplinkPacket>();
    let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
    sync_readers(&connections, &mut reader_handles, &packet_tx);

    // Create instant ACK forwarding channel and shared client address
    let (instant_tx, instant_rx) = std::sync::mpsc::channel::<SmallVec<u8, 64>>();
    let shared_client_addr = Arc::new(Mutex::new(None::<SocketAddr>));

    // Wrap local_listener in Arc for sharing
    let local_listener = Arc::new(local_listener);

    // Spawn instant forwarding task
    {
        let local_listener_clone = local_listener.clone();
        let shared_client_addr_clone = shared_client_addr.clone();
        tokio::spawn(async move {
            while let Ok(ack_packet) = instant_rx.recv() {
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
        });
    }

    let mut recv_buf = vec![0u8; MTU];
    let mut housekeeping_timer = time::interval_at(
        Instant::now() + Duration::from_millis(HOUSEKEEPING_INTERVAL_MS),
        Duration::from_millis(HOUSEKEEPING_INTERVAL_MS),
    );
    housekeeping_timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut status_elapsed_ms: u64 = 0;
    let mut last_client_addr: Option<SocketAddr> = None;
    let mut seq_to_conn: HashMap<u32, SequenceTrackingEntry> =
        HashMap::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut seq_order: VecDeque<u32> = VecDeque::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut last_sequence_cleanup_ms: u64 = 0;
    let mut last_selected_idx: Option<usize> = None;
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
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_client_addr,
                    &shared_client_addr,
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
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_client_addr,
                    &shared_client_addr,
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

#[allow(clippy::too_many_arguments)]
async fn handle_srt_packet(
    res: Result<(usize, SocketAddr), std::io::Error>,
    recv_buf: &mut [u8],
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_client_addr: &mut Option<SocketAddr>,
    shared_client_addr: &Arc<Mutex<Option<SocketAddr>>>,
    registration_complete: bool,
    toggles: &DynamicToggles,
) {
    match res {
        Ok((n, src)) => {
            if n == 0 {
                return;
            }
            let pkt = &recv_buf[..n];
            let seq = protocol::get_srt_sequence_number(pkt);
            if !registration_complete {
                let sel_idx = if let Some(idx) = last_selected_idx {
                    if let Some(conn) = connections.get(*idx) {
                        if conn.connected && !conn.is_timed_out() {
                            Some(*idx)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    connections
                        .iter()
                        .enumerate()
                        .find(|(_, c)| !c.is_timed_out())
                        .map(|(i, _)| i)
                        .or_else(|| {
                            connections
                                .first()
                                .and_then(|c| if !c.is_timed_out() { Some(0) } else { None })
                        })
                };
                if let Some(sel_idx) = sel_idx {
                    forward_via_connection(
                        sel_idx,
                        pkt,
                        seq,
                        connections,
                        last_selected_idx,
                        seq_to_conn,
                        seq_order,
                        last_client_addr,
                        shared_client_addr,
                        src,
                    )
                    .await;
                }
                return;
            }
            let enable_quality = toggles
                .quality_scoring_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let enable_explore = toggles
                .exploration_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let classic = toggles
                .classic_mode
                .load(std::sync::atomic::Ordering::Relaxed);

            let effective_enable_quality = enable_quality && !classic;
            let effective_enable_explore = enable_explore && !classic;

            let sel_idx = select_connection_idx(
                connections,
                *last_selected_idx,
                effective_enable_quality,
                effective_enable_explore,
                classic,
                Instant::now(),
            );
            if let Some(sel_idx) = sel_idx {
                forward_via_connection(
                    sel_idx,
                    pkt,
                    seq,
                    connections,
                    last_selected_idx,
                    seq_to_conn,
                    seq_order,
                    last_client_addr,
                    shared_client_addr,
                    src,
                )
                .await;
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

#[allow(clippy::too_many_arguments)]
async fn forward_via_connection(
    sel_idx: usize,
    pkt: &[u8],
    seq: Option<u32>,
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_client_addr: &mut Option<SocketAddr>,
    shared_client_addr: &Arc<Mutex<Option<SocketAddr>>>,
    src: SocketAddr,
) {
    if sel_idx >= connections.len() {
        return;
    }
    if *last_selected_idx != Some(sel_idx) {
        if let Some(prev_idx) = *last_selected_idx {
            if prev_idx < connections.len() {
                debug!(
                    "Connection switch: {} → {} (seq: {:?})",
                    connections[prev_idx].label, connections[sel_idx].label, seq
                );
            }
        } else {
            debug!(
                "Initial connection selected: {} (seq: {:?})",
                connections[sel_idx].label, seq
            );
        }
        *last_selected_idx = Some(sel_idx);
    }
    let conn = &mut connections[sel_idx];
    if let Err(e) = conn.send_data_with_tracking(pkt, seq).await {
        warn!(
            "{}: sendto() failed, marking for recovery: {}",
            conn.label, e
        );
        conn.mark_for_recovery();
    }
    if let Some(s) = seq {
        if seq_to_conn.len() >= MAX_SEQUENCE_TRACKING
            && let Some(old) = seq_order.pop_front()
        {
            seq_to_conn.remove(&old);
        }
        seq_to_conn.insert(
            s,
            SequenceTrackingEntry {
                conn_id: connections[sel_idx].conn_id,
                timestamp_ms: now_ms(),
            },
        );
        seq_order.push_back(s);
    }
    *last_client_addr = Some(src);
    if let Ok(mut addr_guard) = shared_client_addr.lock() {
        *addr_guard = Some(src);
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_housekeeping(
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_sequence_cleanup_ms: &mut u64,
    classic: bool,
    all_failed_at: &mut Option<Instant>,
    reader_handles: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) -> Result<()> {
    // If we're waiting on a REG2 response past the timeout, proactively retry REG1
    let current_ms = now_ms();
    let _ = reg.clear_pending_if_timed_out(current_ms);

    if reg.is_probing() {
        let was_probing = true;
        reg.check_probing_complete();
        // If probing just completed, reset grace period for the selected connection
        if !reg.is_probing() && was_probing {
            if let Some(idx) = reg.get_selected_connection_idx() {
                if let Some(conn) = connections.get_mut(idx) {
                    conn.startup_grace_deadline_ms = current_ms + 1500;
                    debug!(
                        "{}: Reset grace period after being selected for initial registration",
                        conn.label
                    );
                }
            }
        }
    }

    // housekeeping: drive registration, send keepalives
    for (i, conn) in connections.iter_mut().enumerate() {
        // Simple reconnect-on-timeout, then allow reg driver to proceed
        if conn.is_timed_out() {
            if conn.should_attempt_reconnect() {
                let label = conn.label.clone();
                conn.record_reconnect_attempt();
                warn!("{} timed out; attempting full socket reconnection", label);
                // Perform full socket reconnection
                if let Err(e) = conn.reconnect().await {
                    warn!("{} failed to reconnect: {}", label, e);
                    // Fall back to mark_for_recovery if reconnect fails
                    conn.mark_for_recovery();
                } else {
                    restart_reader_for(conn, reader_handles, packet_tx);
                }

                match reg.pending_reg2_idx() {
                    Some(idx) if idx == i => {
                        info!("{} marked for recovery; re-sending REG1", label);
                        reg.send_reg1_to(i, conn).await;
                    }
                    Some(_) => {
                        debug!(
                            "{} timed out but another uplink is awaiting REG2; deferring",
                            label
                        );
                    }
                    None => {
                        info!("{} marked for recovery; re-sending REG2", label);
                        reg.send_reg2_to(i, conn).await;
                    }
                }
            } else {
                debug!("{} timed out but in retry interval", conn.label);
            }
            continue;
        }

        if conn.needs_keepalive() {
            let _ = conn.send_keepalive().await;
        }
        if conn.needs_rtt_measurement() {
            let _ = conn.send_keepalive().await;
        }
        if !classic {
            conn.perform_window_recovery();
        }
    }

    // Update active connections count (matches C implementation behavior)
    // C code resets active_connections=0 then counts non-timed-out connections
    reg.update_active_connections(connections);

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
        if let Some(failed_at) = all_failed_at
            && failed_at.elapsed().as_millis() > GLOBAL_TIMEOUT_MS as u128
        {
            if reg.has_connected {
                error!("Failed to re-establish any connections");
                return Err(anyhow!("Failed to re-establish any connections"));
            } else {
                error!("Failed to establish any initial connections");
                return Err(anyhow!("Failed to establish any initial connections"));
            }
        }
    } else {
        *all_failed_at = None;
    }

    cleanup_expired_sequence_tracking(seq_to_conn, seq_order, last_sequence_cleanup_ms);

    Ok(())
}

fn cleanup_expired_sequence_tracking(
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_cleanup_ms: &mut u64,
) {
    let current_time = now_ms();
    if current_time.saturating_sub(*last_cleanup_ms) < SEQUENCE_MAP_CLEANUP_INTERVAL_MS {
        return;
    }
    *last_cleanup_ms = current_time;

    let before_size = seq_to_conn.len();
    let mut removed_count = 0;

    seq_to_conn.retain(|_seq, entry| {
        if entry.is_expired(current_time) {
            removed_count += 1;
            false
        } else {
            true
        }
    });

    seq_order.retain(|seq| seq_to_conn.contains_key(seq));

    if removed_count > 0 {
        let utilization = (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0;
        if utilization >= 75.0 {
            info!(
                "Cleaned up {} stale sequence mappings ({} → {}, {:.1}% capacity)",
                removed_count,
                before_size,
                seq_to_conn.len(),
                utilization
            );
        } else {
            debug!(
                "Cleaned up {} stale sequence mappings ({} → {}, {:.1}% capacity)",
                removed_count,
                before_size,
                seq_to_conn.len(),
                utilization
            );
        }
    }

    if seq_to_conn.len() > (MAX_SEQUENCE_TRACKING as f64 * 0.8) as usize {
        warn!(
            "Sequence tracking at {:.1}% capacity ({}/{}) - consider review",
            (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0,
            seq_to_conn.len(),
            MAX_SEQUENCE_TRACKING
        );
    }
}

pub(crate) fn calculate_quality_multiplier(conn: &SrtlaConnection) -> f64 {
    use crate::utils::now_ms;

    // Startup grace period: first 10 seconds after connection establishment
    // During this time, use simple scoring like original C version to go live fast
    // This prevents early NAKs from permanently degrading connections
    let connection_age_ms = now_ms().saturating_sub(conn.connection_established_ms());
    if connection_age_ms < 10000 {
        // During startup grace period, only apply light penalties to prevent permanent
        // degradation
        return if conn.total_nak_count() == 0 {
            1.2
        } else {
            0.95
        };
    }

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
    _last_idx: Option<usize>,
    enable_quality: bool,
    enable_explore: bool,
    classic: bool,
    now: Instant,
) -> Option<usize> {
    // Classic mode: simple algorithm matching original implementation
    if classic {
        let mut best_idx: Option<usize> = None;
        let mut best_score: i32 = -1;

        for (i, c) in conns.iter().enumerate() {
            if c.is_timed_out() {
                continue;
            }
            let score = c.get_score();
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        return best_idx;
    }

    // Exploration window: simple periodic exploration of second-best
    // Use elapsed time since program start for consistent periodic behavior
    let explore_now = enable_explore && (now.elapsed().as_millis() % 5000) < 300;
    // Score connections by base score; apply quality multiplier unless classic
    let mut best_idx: Option<usize> = None;
    let mut second_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut second_score: f64 = -1.0;
    for (i, c) in conns.iter().enumerate() {
        if c.is_timed_out() {
            continue;
        }
        let base = c.get_score() as f64;
        let score = if !enable_quality {
            base
        } else {
            let quality_mult = calculate_quality_multiplier(c);
            let final_score = (base * quality_mult).max(1.0);

            // Log quality issues and recoveries for debugging
            if quality_mult < 0.8 {
                debug!(
                    "{} quality degraded: {:.2} (NAKs: {}, last: {}ms ago, burst: {}) base: {} → \
                     final: {}",
                    c.label,
                    quality_mult,
                    c.total_nak_count(),
                    c.time_since_last_nak_ms().unwrap_or(0),
                    c.nak_burst_count(),
                    base as i32,
                    final_score as i32
                );
            } else if quality_mult < 1.0 && c.nak_burst_count() > 0 {
                debug!(
                    "{} quality recovering: {:.2} (burst: {})",
                    c.label,
                    quality_mult,
                    c.nak_burst_count()
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

    // Allow switching if better connection found (prevents getting stuck on
    // degraded connections)
    if explore_now {
        second_idx.or(best_idx)
    } else {
        best_idx
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
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
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
    seq_to_conn: &HashMap<u32, SequenceTrackingEntry>,
    seq_order: &VecDeque<u32>,
    last_selected_idx: Option<usize>,
    toggles: &DynamicToggles,
) {
    let total_connections = connections.len();
    let active_connections = connections.iter().filter(|c| !c.is_timed_out()).count();
    let timed_out_connections = total_connections - active_connections;

    info!("📊 Connection Status Report:");
    info!("  Total connections: {}", total_connections);
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

        info!(
            "    [{}] {} {} - Score: {} - Last recv: {} - Window: {} - In-flight: {}",
            i, status, conn.label, score_desc, last_recv, conn.window, conn.in_flight_packets
        );

        if conn.estimated_rtt_ms > 0.0 {
            info!(
                "        RTT: smooth={:.1}ms, fast={:.1}ms, jitter={:.1}ms, stable={} (last: \
                 {:.1}s ago)",
                conn.get_smooth_rtt_ms(),
                conn.get_fast_rtt_ms(),
                conn.get_rtt_jitter_ms(),
                conn.is_rtt_stable(),
                (now_ms().saturating_sub(conn.last_rtt_measurement_ms) as f64) / 1000.0
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
